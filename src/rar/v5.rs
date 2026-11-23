//! RAR 5 reader: block headers and the RAR 5 LZ + Huffman decoder with its
//! delta, x86 and ARM filters.
//!
//! The decoder is a Rust port of libarchive's
//! `archive_read_support_format_rar5.c`, used under its BSD 2-clause licence:
//!
//! Copyright (c) 2018 Grzegorz Antoniak (http://antoniak.org)
//! All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions
//! are met:
//! 1. Redistributions of source code must retain the above copyright
//!    notice, this list of conditions and the following disclaimer.
//! 2. Redistributions in binary form must reproduce the above copyright
//!    notice, this list of conditions and the following disclaimer in the
//!    documentation and/or other materials provided with the distribution.
//!
//! THIS SOFTWARE IS PROVIDED BY THE AUTHOR(S) ``AS IS'' AND ANY EXPRESS OR
//! IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//! OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//! IN NO EVENT SHALL THE AUTHOR(S) BE LIABLE FOR ANY DIRECT, INDIRECT,
//! INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//! NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//! DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//! THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//! (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//! THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//!
//! The header layout follows RARLAB's published "RAR 5.0 archive format"
//! description.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use super::huff::{Huff, Peek};
use super::{Codec, Entry, Link, Seg, SIG5, clean_name, filetime, read_exact_at, unix_time};

// ---------------------------------------------------------------- headers

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn vint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for i in 0..10 {
            let b = *self.b.get(self.p).ok_or("RAR header cut short")?;
            self.p += 1;
            if i < 10 {
                v |= ((b & 0x7f) as u64).checked_shl(7 * i).unwrap_or(0);
            }
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("bad number in RAR header".into())
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.p.checked_add(n).filter(|&e| e <= self.b.len()).ok_or("RAR header cut short")?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
}

/// Reads one vint straight from the file; returns it and its raw bytes.
fn file_vint(f: &mut File, raw: &mut Vec<u8>) -> Result<Option<u64>, String> {
    let mut v = 0u64;
    for i in 0..10 {
        let mut b = [0u8; 1];
        if !read_exact_at(f, &mut b)? {
            return if i == 0 { Ok(None) } else { Err("RAR header cut short".into()) };
        }
        raw.push(b[0]);
        v |= ((b[0] & 0x7f) as u64).checked_shl(7 * i).unwrap_or(0);
        if b[0] & 0x80 == 0 {
            return Ok(Some(v));
        }
    }
    Err("bad number in RAR header".into())
}

const HEAD_MAIN: u64 = 1;
const HEAD_FILE: u64 = 2;
const HEAD_SERVICE: u64 = 3;
const HEAD_CRYPT: u64 = 4;
const HEAD_END: u64 = 5;

const HFL_EXTRA: u64 = 0x1;
const HFL_DATA: u64 = 0x2;
const HFL_SPLIT_BEFORE: u64 = 0x8;

pub(super) fn parse(parts: &[PathBuf]) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    // An entry whose data continues in the next volume.
    let mut open_split: Option<usize> = None;
    for (pi, path) in parts.iter().enumerate() {
        let name = path.display();
        let mut f = File::open(path).map_err(|e| format!("{name}: {e}"))?;
        let mut sig = [0u8; 8];
        if !read_exact_at(&mut f, &mut sig)? || sig != SIG5 {
            return Err(format!("{name}: not a RAR 5 volume"));
        }
        let mut more_volumes = false;
        loop {
            let mut crc = [0u8; 4];
            if !read_exact_at(&mut f, &mut crc)? {
                break;
            }
            let mut raw = Vec::new();
            let size = file_vint(&mut f, &mut raw)?.ok_or("RAR header cut short")?;
            if size == 0 || size > 2 << 20 {
                return Err(format!("{name}: bad RAR header size"));
            }
            let mut body = vec![0u8; size as usize];
            if !read_exact_at(&mut f, &mut body)? {
                return Err(format!("{name}: RAR header cut short"));
            }
            let mut h = crc32fast::Hasher::new();
            h.update(&raw);
            h.update(&body);
            if h.finalize() != u32::from_le_bytes(crc) {
                return Err(format!("{name}: RAR header checksum mismatch (archive is damaged)"));
            }
            let mut c = Cursor { b: &body, p: 0 };
            let kind = c.vint()?;
            let flags = c.vint()?;
            let extra_size = if flags & HFL_EXTRA != 0 { c.vint()? } else { 0 };
            let data_size = if flags & HFL_DATA != 0 { c.vint()? } else { 0 };
            let extra = body.len().checked_sub(extra_size as usize).filter(|&s| s >= c.p).ok_or("bad RAR extra area")?;
            let data_at = f.stream_position().map_err(|e| e.to_string())?;
            match kind {
                HEAD_MAIN | HEAD_SERVICE => {}
                HEAD_CRYPT => return Err(super::ENCRYPTED.into()),
                HEAD_END => {
                    let end_flags = c.vint()?;
                    more_volumes = end_flags & 1 != 0;
                    break;
                }
                HEAD_FILE => {
                    let mut e = file_header(&mut c, &body[extra..])?;
                    let seg = Seg { part: pi, offset: data_at, len: data_size };
                    if flags & HFL_SPLIT_BEFORE != 0 {
                        let prev = open_split.and_then(|i| entries.get_mut(i)).filter(|p| p.name == e.name).ok_or_else(|| {
                            format!("{name}: {} continues from a previous volume that is missing; open the first volume", e.name)
                        })?;
                        prev.segs.push(seg);
                        // The last part carries the checksum of the whole file.
                        prev.crc = e.crc;
                        prev.blake = e.blake;
                    } else {
                        e.segs.push(seg);
                        entries.push(e);
                    }
                    open_split = (flags & 0x10 != 0).then(|| entries.len() - 1);
                }
                _ => {}
            }
            f.seek(SeekFrom::Start(data_at + data_size)).map_err(|e| e.to_string())?;
        }
        if !more_volumes && pi + 1 < parts.len() {
            break;
        }
        if more_volumes && pi + 1 == parts.len() {
            return Err(format!("{name}: the next volume of this RAR set is missing"));
        }
    }
    if let Some(i) = open_split {
        return Err(format!("{}: continues in a volume that is missing", entries[i].name));
    }
    Ok(entries)
}

fn file_header(c: &mut Cursor, extra: &[u8]) -> Result<Entry, String> {
    let file_flags = c.vint()?;
    let size = c.vint()?;
    let _attr = c.vint()?;
    let mtime = if file_flags & 0x2 != 0 { Some(unix_time(c.u32()? as u64, 0)) } else { None };
    let crc = if file_flags & 0x4 != 0 { Some(c.u32()?) } else { None };
    let info = c.vint()?;
    let _host = c.vint()?;
    let name_len = c.vint()? as usize;
    let raw_name = c.bytes(name_len)?;
    let mut name = clean_name(&String::from_utf8_lossy(raw_name));
    let is_dir = file_flags & 0x1 != 0;
    if file_flags & 0x8 != 0 && !is_dir {
        return Err(format!("{name}: files of unknown size are not supported"));
    }
    let version = info & 0x3f;
    let solid = info & 0x40 != 0;
    let method = (info >> 7) & 7;
    let (dict, codec_ok) = match version {
        0 => (0x20000u64 << ((info >> 10) & 15), Ok(())),
        1 => {
            let n = (info >> 10) & 31;
            let base = 0x20000u64.checked_shl(n as u32).unwrap_or(u64::MAX);
            let d = base.saturating_add(base / 32 * ((info >> 15) & 31));
            if info & 0x100000 != 0 { (d, Ok(())) } else { (d, Err("RAR 7 large-dictionary compression is not supported yet".to_string())) }
        }
        v => (0, Err(format!("unknown RAR 5 compression version {v}"))),
    };
    let mut e = Entry {
        name: String::new(),
        size: if is_dir { 0 } else { size },
        is_dir,
        encrypted: false,
        solid,
        codec: Codec::Stored,
        method: String::new(),
        crc,
        blake: None,
        mtime,
        link: None,
        segs: Vec::new(),
    };
    // Extra records.
    let mut x = Cursor { b: extra, p: 0 };
    while x.p < extra.len() {
        let rec_size = x.vint()? as usize;
        let rec = x.bytes(rec_size)?;
        let mut r = Cursor { b: rec, p: 0 };
        match r.vint()? {
            0x01 => e.encrypted = true,
            0x02 => {
                if r.vint()? == 0 {
                    e.blake = Some(r.bytes(32)?.try_into().unwrap());
                }
            }
            0x03 => {
                let fl = r.vint()?;
                let unix = fl & 1 != 0;
                let mut times = [None; 3];
                for (i, bit) in [0x2u64, 0x4, 0x8].into_iter().enumerate() {
                    if fl & bit != 0 {
                        times[i] = Some(if unix { r.u32()? as u64 } else { r.u64()? });
                    }
                }
                let mut ns = 0u32;
                if unix && fl & 0x10 != 0 && fl & 0x2 != 0 {
                    ns = r.u32()?;
                }
                if let Some(t) = times[0] {
                    e.mtime = if unix { Some(unix_time(t, ns)) } else { filetime(t) };
                }
            }
            0x04 => {
                let _fl = r.vint()?;
                let v = r.vint()?;
                name = format!("{name};{v}");
            }
            0x05 => {
                let kind = r.vint()?;
                let _fl = r.vint()?;
                let len = r.vint()? as usize;
                let target = String::from_utf8_lossy(r.bytes(len)?).into_owned();
                e.link = match kind {
                    1..=3 => Some(Link::Symlink(target)),
                    4 | 5 => Some(Link::Copy(target)),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    e.name = name;
    e.codec = if is_dir || method == 0 {
        Codec::Stored
    } else {
        match codec_ok {
            Ok(()) if dict > (4u64 << 30) || dict > usize::MAX as u64 / 2 => Codec::Unsupported(format!("dictionary of {} MB is too large", dict >> 20)),
            Ok(()) => Codec::V5 { dict },
            Err(why) => Codec::Unsupported(why),
        }
    };
    e.method = if is_dir {
        String::new()
    } else if method == 0 {
        "RAR5 stored".into()
    } else {
        format!("RAR5 -m{method}{}", if solid { " solid" } else { "" })
    };
    Ok(e)
}

// ---------------------------------------------------------------- decoder

const HUFF_BC: usize = 20;
const HUFF_NC: usize = 306;
const HUFF_DC: usize = 64;
const HUFF_LDC: usize = 16;
const HUFF_RC: usize = 44;
const HUFF_TABLE_SIZE: usize = HUFF_NC + HUFF_DC + HUFF_RC + HUFF_LDC;

/// Bit reader over one compressed block. The buffer carries zero padding so
/// a symbol that starts inside the block can always be read in full.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

const PAD: usize = 64;

impl Peek for Bits<'_> {
    #[inline(always)]
    fn peek16(&self) -> u32 {
        Bits::peek16(self)
    }
    #[inline(always)]
    fn skip(&mut self, n: u32) {
        Bits::skip(self, n)
    }
}

impl Bits<'_> {
    #[inline(always)]
    fn peek16(&self) -> u32 {
        let i = self.pos >> 3;
        let v = (self.b[i] as u32) << 16 | (self.b[i + 1] as u32) << 8 | self.b[i + 2] as u32;
        (v >> (8 - (self.pos & 7))) & 0xffff
    }
    #[inline(always)]
    fn peek32(&self) -> u32 {
        let i = self.pos >> 3;
        let s = (self.pos & 7) as u32;
        let v = u32::from_be_bytes([self.b[i], self.b[i + 1], self.b[i + 2], self.b[i + 3]]);
        (v << s) | ((self.b[i + 4] as u32) >> (8 - s))
    }
    #[inline(always)]
    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }
    /// n in 1..=16
    #[inline(always)]
    fn bits(&mut self, n: u32) -> u32 {
        let v = self.peek16() >> (16 - n);
        self.skip(n);
        v
    }
}

struct Tables {
    ld: Box<Huff>,
    dd: Box<Huff>,
    ldd: Box<Huff>,
    rd: Box<Huff>,
}

#[derive(Clone, Copy, PartialEq)]
enum FilterKind {
    Delta(usize),
    E8,
    E8E9,
    Arm,
}

#[derive(Clone, Copy)]
struct Filter {
    kind: FilterKind,
    /// Absolute position in the decoded stream.
    start: u64,
    len: usize,
}

/// The RAR 5 decoder. One value decodes a whole archive; in a solid archive
/// the window, tables and distance history carry over between entries.
#[derive(Default)]
pub(crate) struct Unpack5 {
    win: Vec<u8>,
    mask: u64,
    /// Bytes decoded so far (absolute, never wraps).
    wp: u64,
    /// Bytes handed to the output so far.
    flushed: u64,
    /// Where the current entry starts in the decoded stream.
    file_base: u64,
    filters: VecDeque<Filter>,
    last_filter_end: u64,
    tables: Option<Tables>,
    dist_cache: [u64; 4],
    last_len: usize,
    filtered: Vec<u8>,
    block: Vec<u8>,
}

impl Unpack5 {
    fn reset(&mut self, dict: u64) {
        let want = dict.next_power_of_two().max(8 << 20) as usize;
        if self.win.len() != want {
            self.win = vec![0u8; want];
        }
        self.mask = want as u64 - 1;
        self.wp = 0;
        self.flushed = 0;
        self.tables = None;
        self.dist_cache = [0; 4];
        self.last_len = 0;
    }

    pub fn unpack(&mut self, src: &mut dyn Read, size: u64, dict: u64, solid: bool, out: &mut dyn Write) -> Result<(), String> {
        if !solid || self.win.is_empty() {
            self.reset(dict);
        } else if dict.next_power_of_two().max(8 << 20) > self.win.len() as u64 {
            return Err("solid entry needs a larger dictionary than the entries before it".into());
        }
        self.file_base = self.wp;
        self.filters.clear();
        self.last_filter_end = 0;
        let end = self.wp + size;
        let mut last_block = size == 0;
        while !last_block && self.wp < end {
            // Block header: flags, checksum, 1-3 size bytes.
            let mut hdr = [0u8; 2];
            src.read_exact(&mut hdr).map_err(|_| "compressed data cut short")?;
            let flags = hdr[0];
            let nbytes = ((flags >> 3) & 7) as usize + 1;
            if nbytes > 3 {
                return Err("unsupported RAR 5 block header".into());
            }
            let mut sz = [0u8; 4];
            src.read_exact(&mut sz[..nbytes]).map_err(|_| "compressed data cut short")?;
            let bsize = u32::from_le_bytes(sz) as usize;
            let ck = 0x5A ^ flags ^ sz[0] ^ sz[1] ^ sz[2];
            if ck != hdr[1] {
                return Err("block checksum mismatch (archive is damaged)".into());
            }
            last_block = flags & 0x40 != 0;
            let table_present = flags & 0x80 != 0;
            let bit_size = (flags & 7) as usize + 1;
            let mut block = std::mem::take(&mut self.block);
            block.clear();
            block.resize(bsize + PAD, 0);
            src.read_exact(&mut block[..bsize]).map_err(|_| "compressed data cut short")?;
            let r = self.block_decode(&block, bsize, bit_size, table_present, end, out);
            self.block = block;
            r?;
        }
        self.flush(end, out)?;
        if self.wp != end || self.flushed != end {
            return Err("compressed data does not match the stored size (archive is damaged)".into());
        }
        Ok(())
    }

    fn block_decode(&mut self, block: &[u8], bsize: usize, bit_size: usize, table_present: bool, end: u64, out: &mut dyn Write) -> Result<(), String> {
        let mut b = Bits { b: block, pos: 0 };
        if table_present {
            self.tables = Some(parse_tables(&mut b, bsize)?);
        }
        let Some(t) = self.tables.take() else { return Err("compressed block without Huffman tables".into()) };
        let r = self.symbols(&mut b, &t, bsize, bit_size, end, out);
        self.tables = Some(t);
        r
    }

    fn symbols(&mut self, b: &mut Bits, t: &Tables, bsize: usize, bit_size: usize, end: u64, out: &mut dyn Write) -> Result<(), String> {
        // The block ends `bit_size` bits into its last byte.
        let end_bit = if bsize == 0 { 0 } else { (bsize - 1) * 8 + bit_size };
        let half = self.win.len() as u64 / 2;
        let mask = self.mask;
        while b.pos < end_bit {
            if self.wp - self.flushed > half {
                self.flush(self.wp.min(end), out)?;
                if self.wp - self.flushed + 0x10000 > self.win.len() as u64 {
                    return Err("filter range larger than the dictionary (archive is damaged)".into());
                }
            }
            let num = t.ld.decode(b) as usize;
            if num < 256 {
                self.win[(self.wp & mask) as usize] = num as u8;
                self.wp += 1;
            } else if num >= 262 {
                let mut len = code_length(b, num - 262);
                let slot = t.dd.decode(b) as u64;
                let mut dist: u64 = 1;
                if slot < 4 {
                    dist += slot;
                } else {
                    let dbits = (slot / 2 - 1) as u32;
                    dist += (2 | (slot & 1)) << dbits;
                    if dbits >= 4 {
                        if dbits > 4 {
                            let add = (b.peek32() >> (36 - dbits)) as u64;
                            b.skip(dbits - 4);
                            dist += add << 4;
                        }
                        dist += t.ldd.decode(b) as u64;
                    } else {
                        dist += b.bits(dbits) as u64;
                    }
                }
                if dist > 0x100 {
                    len += 1;
                    if dist > 0x2000 {
                        len += 1;
                        if dist > 0x40000 {
                            len += 1;
                        }
                    }
                }
                self.dist_cache = [dist, self.dist_cache[0], self.dist_cache[1], self.dist_cache[2]];
                self.last_len = len;
                self.copy(len, dist);
            } else if num == 256 {
                self.parse_filter(b)?;
            } else if num == 257 {
                if self.last_len != 0 {
                    self.copy(self.last_len, self.dist_cache[0]);
                }
            } else {
                let idx = num - 258;
                let dist = self.dist_cache[idx];
                for i in (1..=idx).rev() {
                    self.dist_cache[i] = self.dist_cache[i - 1];
                }
                self.dist_cache[0] = dist;
                let slot = t.rd.decode(b) as usize;
                let len = code_length(b, slot);
                self.last_len = len;
                self.copy(len, dist);
            }
        }
        Ok(())
    }

    #[inline(always)]
    fn copy(&mut self, len: usize, dist: u64) {
        let n = self.win.len();
        let w = (self.wp & self.mask) as usize;
        let r = (self.wp.wrapping_sub(dist) & self.mask) as usize;
        if w + len <= n && r + len <= n && (r + len <= w || w + len <= r) {
            self.win.copy_within(r..r + len, w);
        } else {
            let m = self.mask as usize;
            for i in 0..len {
                self.win[(w + i) & m] = self.win[(r + i) & m];
            }
        }
        self.wp += len as u64;
    }

    fn parse_filter(&mut self, b: &mut Bits) -> Result<(), String> {
        let start = filter_number(b);
        let len = filter_number(b) as usize;
        let kind = b.bits(3);
        let start = self.wp + start as u64;
        if !(4..=0x400000).contains(&len) || kind > 3 || start < self.last_filter_end {
            return Err("invalid filter (archive is damaged)".into());
        }
        let kind = match kind {
            0 => FilterKind::Delta(b.bits(5) as usize + 1),
            1 => FilterKind::E8,
            2 => FilterKind::E8E9,
            _ => FilterKind::Arm,
        };
        self.last_filter_end = start + len as u64;
        self.filters.push_back(Filter { kind, start, len });
        Ok(())
    }

    /// Hands decoded bytes up to `target` to the output, running each filter
    /// once its whole range is decoded.
    fn flush(&mut self, target: u64, out: &mut dyn Write) -> Result<(), String> {
        while self.flushed < target {
            match self.filters.front().copied() {
                Some(f) if f.start <= self.flushed => {
                    let f_end = f.start + f.len as u64;
                    if f.start < self.flushed {
                        return Err("overlapping filters (archive is damaged)".into());
                    }
                    if self.wp < f_end {
                        // Not fully decoded yet: wait for more data.
                        return Ok(());
                    }
                    if f_end > target {
                        return Err("filter reaches past the end of the entry (archive is damaged)".into());
                    }
                    self.run_filter(f);
                    out.write_all(&self.filtered[..f.len]).map_err(|e| e.to_string())?;
                    self.flushed += f.len as u64;
                    self.filters.pop_front();
                }
                next => {
                    let upto = next.map_or(target, |f| f.start.min(target));
                    self.write_window(self.flushed, upto, out)?;
                    self.flushed = upto;
                }
            }
        }
        Ok(())
    }

    fn write_window(&self, from: u64, to: u64, out: &mut dyn Write) -> Result<(), String> {
        let n = self.win.len() as u64;
        let mut p = from;
        while p < to {
            let i = (p & self.mask) as usize;
            let take = (to - p).min(n - i as u64) as usize;
            out.write_all(&self.win[i..i + take]).map_err(|e| e.to_string())?;
            p += take as u64;
        }
        Ok(())
    }

    fn run_filter(&mut self, f: Filter) {
        let m = self.mask;
        let src = |i: usize| self.win[((f.start + i as u64) & m) as usize];
        let mut o = std::mem::take(&mut self.filtered);
        o.clear();
        o.resize(f.len, 0);
        // Offsets inside the current entry, as the encoder saw them.
        let rel = f.start - self.file_base;
        match f.kind {
            FilterKind::Delta(ch) => {
                let mut s = 0;
                for c in 0..ch {
                    let mut prev = 0u8;
                    let mut d = c;
                    while d < f.len {
                        prev = prev.wrapping_sub(src(s));
                        o[d] = prev;
                        s += 1;
                        d += ch;
                    }
                }
            }
            FilterKind::E8 | FilterKind::E8E9 => {
                const FILE_SIZE: u32 = 0x1000000;
                for (i, v) in o.iter_mut().enumerate() {
                    *v = src(i);
                }
                let e9 = f.kind == FilterKind::E8E9;
                let mut i = 0usize;
                while i + 4 < f.len {
                    let byte = src(i);
                    i += 1;
                    if byte == 0xE8 || (e9 && byte == 0xE9) {
                        let offset = ((i as u64 + rel) % FILE_SIZE as u64) as u32;
                        let addr = u32::from_le_bytes([src(i), src(i + 1), src(i + 2), src(i + 3)]);
                        if addr & 0x8000_0000 != 0 {
                            if addr.wrapping_add(offset) & 0x8000_0000 == 0 {
                                o[i..i + 4].copy_from_slice(&addr.wrapping_add(FILE_SIZE).to_le_bytes());
                            }
                        } else if addr.wrapping_sub(FILE_SIZE) & 0x8000_0000 != 0 {
                            o[i..i + 4].copy_from_slice(&addr.wrapping_sub(offset).to_le_bytes());
                        }
                        i += 4;
                    }
                }
            }
            FilterKind::Arm => {
                for (i, v) in o.iter_mut().enumerate() {
                    *v = src(i);
                }
                let mut i = 0usize;
                while i + 3 < f.len {
                    if src(i + 3) == 0xEB {
                        let mut off = u32::from_le_bytes([src(i), src(i + 1), src(i + 2), src(i + 3)]) & 0x00ff_ffff;
                        off = off.wrapping_sub(((i as u64 + rel) / 4) as u32);
                        o[i..i + 4].copy_from_slice(&((off & 0x00ff_ffff) | 0xeb00_0000).to_le_bytes());
                    }
                    i += 4;
                }
            }
        }
        self.filtered = o;
    }
}

fn code_length(b: &mut Bits, code: usize) -> usize {
    if code < 8 {
        2 + code
    } else {
        let lbits = (code / 4 - 1) as u32;
        2 + ((4 | (code & 3)) << lbits) + b.bits(lbits) as usize
    }
}

fn filter_number(b: &mut Bits) -> u32 {
    let n = b.bits(2) + 1;
    let mut v = 0u32;
    for i in 0..n {
        v |= b.bits(8) << (i * 8);
    }
    v
}

fn parse_tables(b: &mut Bits, bsize: usize) -> Result<Tables, String> {
    let mut bl = [0u8; HUFF_BC];
    let mut w = 0;
    while w < HUFF_BC {
        if b.pos >> 3 >= bsize {
            return Err("Huffman tables cut short".into());
        }
        let v = b.bits(4) as u8;
        if v == 15 {
            let n = b.bits(4) as usize;
            if n == 0 {
                bl[w] = 15;
                w += 1;
            } else {
                for _ in 0..n + 2 {
                    if w < HUFF_BC {
                        bl[w] = 0;
                        w += 1;
                    }
                }
            }
        } else {
            bl[w] = v;
            w += 1;
        }
    }
    let bd = Huff::new(&bl);
    let mut table = [0u8; HUFF_TABLE_SIZE];
    let mut i = 0;
    while i < HUFF_TABLE_SIZE {
        if b.pos >> 3 >= bsize {
            return Err("Huffman tables cut short".into());
        }
        let num = bd.decode(b);
        if num < 16 {
            table[i] = num as u8;
            i += 1;
        } else if num < 18 {
            let n = if num == 16 { b.bits(3) + 3 } else { b.bits(7) + 11 };
            if i == 0 {
                return Err("bad Huffman tables".into());
            }
            for _ in 0..n {
                if i < HUFF_TABLE_SIZE {
                    table[i] = table[i - 1];
                    i += 1;
                }
            }
        } else {
            let n = if num == 18 { b.bits(3) + 3 } else { b.bits(7) + 11 };
            for _ in 0..n {
                if i < HUFF_TABLE_SIZE {
                    table[i] = 0;
                    i += 1;
                }
            }
        }
    }
    let (ld, rest) = table.split_at(HUFF_NC);
    let (dd, rest) = rest.split_at(HUFF_DC);
    let (ldd, rd) = rest.split_at(HUFF_LDC);
    Ok(Tables { ld: Huff::new(ld), dd: Huff::new(dd), ldd: Huff::new(ldd), rd: Huff::new(rd) })
}
