//! RAR 2.9 to 4.x reader ("RAR 3" format): block headers, and the decoder
//! for its LZSS + Huffman compression with VM filters, and PPMd var.H.
//!
//! The decoder is a Rust port of libarchive's
//! `archive_read_support_format_rar.c`, used under its BSD 2-clause licence:
//!
//! Copyright (c) 2003-2007 Tim Kientzle
//! Copyright (c) 2011 Andres Mejia
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
//! Beyond libarchive: solid archives (the decoder state carries over between
//! entries) and filters inside PPMd blocks. Like libarchive, the RAR VM is
//! not emulated: the standard filters (delta, x86 E8/E8E9, RGB, audio) are
//! recognised by the fingerprint of their bytecode and run natively; any
//! other program is refused with a clear error.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use super::huff::{Huff, Peek};
use super::ppmd::{ByteIn, PPMd7, RarRc};
use super::{Codec, Entry, Link, Seg, SIG4, clean_name, read_exact_at, unix_time};

const HEAD_MAIN: u8 = 0x73;
const HEAD_FILE: u8 = 0x74;
const HEAD_NEWSUB: u8 = 0x7a;
const HEAD_ENDARC: u8 = 0x7b;

const MHD_VOLUME: u16 = 0x0001;
const MHD_PASSWORD: u16 = 0x0080;
const HD_ADD_SIZE: u16 = 0x8000;

const FHD_SPLIT_BEFORE: u16 = 0x0001;
const FHD_SPLIT_AFTER: u16 = 0x0002;
const FHD_PASSWORD: u16 = 0x0004;
const FHD_SOLID: u16 = 0x0010;
const FHD_LARGE: u16 = 0x0100;
const FHD_UNICODE: u16 = 0x0200;

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// RAR 4 headers store the low 16 bits of a CRC32.
fn crc16(parts: &[&[u8]]) -> u16 {
    let mut h = crc32fast::Hasher::new();
    for p in parts {
        h.update(p);
    }
    (h.finalize() & 0xffff) as u16
}

pub(super) fn parse(parts: &[PathBuf]) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut open_split: Option<usize> = None;
    for (pi, path) in parts.iter().enumerate() {
        let name = path.display();
        let io = |e: io::Error| format!("{name}: {e}");
        let mut f = File::open(path).map_err(io)?;
        let mut sig = [0u8; 7];
        if !read_exact_at(&mut f, &mut sig)? || sig != SIG4 {
            return Err(format!("{name}: not a RAR 4 volume"));
        }
        let mut next_volume: Option<bool> = None;
        let mut is_volume = false;
        loop {
            let at = f.stream_position().map_err(io)?;
            let mut h = [0u8; 7];
            if !read_exact_at(&mut f, &mut h)? {
                break;
            }
            let (crc, kind, flags, size) = (le16(&h), h[2], le16(&h[3..]), le16(&h[5..]) as u64);
            if size < 7 {
                return Err(format!("{name}: bad RAR header size"));
            }
            let mut body = vec![0u8; size as usize - 7];
            if !body.is_empty() && !read_exact_at(&mut f, &mut body)? {
                return Err(format!("{name}: RAR header cut short"));
            }
            let damaged = || format!("{name}: RAR header checksum mismatch (archive is damaged)");
            match kind {
                HEAD_MAIN => {
                    if flags & MHD_PASSWORD != 0 {
                        return Err(super::ENCRYPTED.into());
                    }
                    if crc16(&[&h[2..], &body]) != crc {
                        return Err(damaged());
                    }
                    is_volume = flags & MHD_VOLUME != 0;
                }
                HEAD_FILE | HEAD_NEWSUB => {
                    if crc16(&[&h[2..], &body]) != crc {
                        return Err(damaged());
                    }
                    if body.len() < 25 {
                        return Err(format!("{name}: RAR file header too short"));
                    }
                    let mut pack = le32(&body[0..]) as u64;
                    let mut unp = le32(&body[4..]) as u64;
                    let mut p = 25;
                    if flags & FHD_LARGE != 0 {
                        let hi = body.get(p..p + 8).ok_or("RAR file header too short")?;
                        pack |= (le32(hi) as u64) << 32;
                        unp |= (le32(&hi[4..]) as u64) << 32;
                        p += 8;
                    }
                    let data_at = at + size;
                    if kind == HEAD_FILE {
                        let e = file_header(&body, p, flags, unp, &mut f, data_at, pack, pi).map_err(|x| format!("{name}: {x}"))?;
                        if flags & FHD_SPLIT_BEFORE != 0 {
                            let prev = open_split.and_then(|i| entries.get_mut(i)).filter(|x| x.name == e.name).ok_or_else(|| {
                                format!("{name}: {} continues from a previous volume that is missing; open the first volume", e.name)
                            })?;
                            prev.segs.extend(e.segs);
                            // The last part carries the checksum of the whole file.
                            prev.crc = e.crc;
                        } else {
                            entries.push(e);
                        }
                        open_split = (flags & FHD_SPLIT_AFTER != 0).then(|| entries.len() - 1);
                    }
                    f.seek(SeekFrom::Start(data_at + pack)).map_err(io)?;
                }
                _ => {
                    // Comments, recovery records, signatures, the end marker:
                    // their CRC covers the header and any data after it.
                    let mut hasher = crc32fast::Hasher::new();
                    hasher.update(&h[2..]);
                    hasher.update(&body);
                    if flags & HD_ADD_SIZE != 0 {
                        if body.len() < 4 {
                            return Err(format!("{name}: bad RAR header size"));
                        }
                        let mut left = le32(&body) as u64;
                        let mut buf = vec![0u8; 64 << 10];
                        while left > 0 {
                            let n = left.min(buf.len() as u64) as usize;
                            if !read_exact_at(&mut f, &mut buf[..n])? {
                                return Err(format!("{name}: RAR block cut short"));
                            }
                            hasher.update(&buf[..n]);
                            left -= n as u64;
                        }
                    }
                    if (hasher.finalize() & 0xffff) as u16 != crc {
                        return Err(damaged());
                    }
                    if kind == HEAD_ENDARC {
                        next_volume = Some(flags & 1 != 0);
                        break;
                    }
                }
            }
        }
        // Archives made before the end marker existed say only that they
        // are a volume; an entry left open then means another part follows.
        let more = next_volume.unwrap_or(is_volume && open_split.is_some());
        if !more && pi + 1 < parts.len() {
            break;
        }
        if more && pi + 1 == parts.len() {
            return Err(format!("{name}: the next volume of this RAR set is missing"));
        }
    }
    if let Some(i) = open_split {
        return Err(format!("{}: continues in a volume that is missing", entries[i].name));
    }
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn file_header(body: &[u8], p: usize, flags: u16, unp: u64, f: &mut File, data_at: u64, pack: u64, part: usize) -> Result<Entry, String> {
    let host = body[8];
    let crc = le32(&body[9..]);
    let ftime = le32(&body[13..]);
    let unp_ver = body[17];
    let method = body[18];
    let name_size = le16(&body[19..]) as usize;
    let attr = le32(&body[21..]);
    let raw = body.get(p..p + name_size).ok_or("RAR file name runs past its header")?;
    let name = clean_name(&decode_name(raw, flags & FHD_UNICODE != 0));
    let dict_bits = (flags >> 5) & 7;
    let unix = (3..=5).contains(&host);
    let is_dir = dict_bits == 7 || if unix { attr & 0o170000 == 0o040000 } else { attr & 0x10 != 0 };
    let is_link = unix && attr & 0o170000 == 0o120000;
    let solid = flags & FHD_SOLID != 0;
    let encrypted = flags & FHD_PASSWORD != 0;
    let mut e = Entry {
        name,
        size: if is_dir { 0 } else { unp },
        is_dir,
        encrypted,
        solid,
        codec: Codec::Stored,
        method: String::new(),
        crc: (!is_dir).then_some(crc),
        blake: None,
        mtime: dos_time(ftime),
        link: None,
        segs: Vec::new(),
    };
    if is_dir {
        return Ok(e);
    }
    if is_link && flags & (FHD_SPLIT_BEFORE | FHD_SPLIT_AFTER | FHD_PASSWORD) == 0 {
        // The link target is stored as the entry's data.
        if pack > 64 << 10 {
            return Err(format!("{}: symbolic link target too long", e.name));
        }
        let mut t = vec![0u8; pack as usize];
        f.seek(SeekFrom::Start(data_at)).map_err(|x| x.to_string())?;
        if !t.is_empty() && !read_exact_at(f, &mut t)? {
            return Err(format!("{}: symbolic link cut short", e.name));
        }
        e.link = Some(Link::Symlink(String::from_utf8_lossy(&t).into_owned()));
        e.size = 0;
        e.crc = None;
        e.method = "RAR4 link".into();
        return Ok(e);
    }
    e.segs.push(Seg { part, offset: data_at, len: pack });
    let solid_tag = if solid { " solid" } else { "" };
    if method == 0x30 {
        e.method = "RAR4 stored".into();
    } else if !(0x31..=0x35).contains(&method) {
        e.method = format!("RAR4 method {method:#x}");
        e.codec = Codec::Unsupported(format!("unknown RAR compression method {method:#x}"));
    } else if unp_ver < 29 {
        e.method = format!("RAR{} -m{}", if unp_ver < 20 { "1.5" } else { "2" }, method - 0x30);
        e.codec = Codec::Unsupported(format!("RAR 1.5/2.x compression (unpack version {unp_ver}) is not supported; only RAR 2.9 and later"));
    } else {
        e.method = format!("RAR4 -m{}{solid_tag}", method - 0x30);
        e.codec = Codec::V4 { dict: 0x10000u64 << dict_bits };
    }
    Ok(e)
}

/// File names: plain bytes, UTF-8, or RAR's compressed UTF-16 (an ASCII
/// name, a NUL, then the Unicode name coded against it).
fn decode_name(b: &[u8], unicode: bool) -> String {
    let Some(z) = b.iter().position(|&c| c == 0).filter(|_| unicode) else {
        return match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            // Old archives use the creator's OEM code page; Latin-1 keeps
            // every byte visible rather than guessing.
            Err(_) => b.iter().map(|&c| c as char).collect(),
        };
    };
    let end = b.len();
    let mut off = z + 1;
    let high = if off < end {
        off += 1;
        b[off - 1]
    } else {
        0
    };
    let mut out: Vec<u16> = Vec::with_capacity(end);
    let (mut flagbits, mut flagbyte) = (0u32, 0u8);
    while off < end && out.len() < end {
        if flagbits == 0 {
            flagbyte = b[off];
            off += 1;
            flagbits = 8;
        }
        flagbits -= 2;
        match (flagbyte >> flagbits) & 3 {
            0 => {
                if off < end {
                    out.push(b[off] as u16);
                    off += 1;
                }
            }
            1 => {
                if off < end {
                    out.push((high as u16) << 8 | b[off] as u16);
                    off += 1;
                }
            }
            2 => {
                if off + 1 >= end {
                    off = end;
                } else {
                    out.push((b[off + 1] as u16) << 8 | b[off] as u16);
                    off += 2;
                }
            }
            _ => {
                if off >= end {
                    continue;
                }
                let mut len = b[off];
                off += 1;
                let (extra, hi) = if len & 0x80 != 0 {
                    if off >= end {
                        continue;
                    }
                    off += 1;
                    (b[off - 1], high)
                } else {
                    (0, 0)
                };
                len = (len & 0x7f) + 2;
                while len > 0 && out.len() < end {
                    let c = b.get(out.len()).copied().unwrap_or(0).wrapping_add(extra);
                    out.push((hi as u16) << 8 | c as u16);
                    len -= 1;
                }
            }
        }
    }
    String::from_utf16_lossy(&out)
}

/// MS-DOS date and time. RAR stores the creator's local time without a
/// zone, so it is read as UTC.
fn dos_time(t: u32) -> Option<SystemTime> {
    let sec = 2 * (t & 0x1f) as i64;
    let min = ((t >> 5) & 0x3f) as i64;
    let hour = ((t >> 11) & 0x1f) as i64;
    let day = ((t >> 16) & 0x1f) as i64;
    let mon = ((t >> 21) & 0xf) as i64;
    let year = ((t >> 25) & 0x7f) as i64 + 1980;
    if !(1..=12).contains(&mon) || day == 0 {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's civil-date algorithm).
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mon + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    (secs >= 0).then(|| unix_time(secs as u64, 0))
}

// ---------------------------------------------------------------------------
// Decoder

/// RAR 3 dictionaries are at most 4 MB.
const WIN: usize = 0x40_0000;
const WIN_MASK: usize = WIN - 1;
const VM_SIZE: usize = 0x4_0000;
const WORK_SIZE: usize = 0x3_C000;

const MAIN_N: usize = 299;
const OFF_N: usize = 60;
const LOW_N: usize = 17;
const LEN_N: usize = 28;
const TABLE_N: usize = MAIN_N + OFF_N + LOW_N + LEN_N;

const LEN_BASE: [u32; 28] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224];
const LEN_BITS: [u32; 28] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5];
const OFF_BASE: [u32; 60] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576,
    32768, 49152, 65536, 98304, 131072, 196608, 262144, 327680, 393216, 458752, 524288, 589824, 655360, 720896, 786432, 851968, 917504, 983040,
    1048576, 1310720, 1572864, 1835008, 2097152, 2359296, 2621440, 2883584, 3145728, 3407872, 3670016, 3932160,
];
const OFF_BITS: [u32; 60] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
];
const SHORT_BASE: [u32; 8] = [0, 4, 8, 16, 32, 64, 128, 192];
const SHORT_BITS: [u32; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

const DAMAGED: &str = "bad compressed data (archive is damaged)";

/// MSB-first bit reader over one entry's packed bytes. Past the end it
/// feeds zeros and remembers that it did, so a truncated stream is an error
/// rather than garbage.
struct BitIn<'a> {
    src: &'a mut dyn Read,
    buf: Vec<u8>,
    bpos: usize,
    blen: usize,
    cache: u64,
    avail: u32,
    padded: u32,
    err: Option<String>,
}

impl<'a> BitIn<'a> {
    fn new(src: &'a mut dyn Read) -> Self {
        let mut b = BitIn { src, buf: vec![0; 64 << 10], bpos: 0, blen: 0, cache: 0, avail: 0, padded: 0, err: None };
        b.fill();
        b
    }

    fn next_raw(&mut self) -> u8 {
        if self.bpos == self.blen {
            while self.padded == 0 && self.err.is_none() {
                match self.src.read(&mut self.buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        self.blen = n;
                        self.bpos = 1;
                        return self.buf[0];
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => self.err = Some(e.to_string()),
                }
            }
            self.padded = self.padded.saturating_add(1);
            return 0;
        }
        self.bpos += 1;
        self.buf[self.bpos - 1]
    }

    fn fill(&mut self) {
        while self.avail <= 56 {
            let b = self.next_raw() as u64;
            self.cache |= b << (56 - self.avail);
            self.avail += 8;
        }
    }

    #[inline(always)]
    fn drop_bits(&mut self, n: u32) {
        self.cache <<= n;
        self.avail -= n;
        if self.avail < 32 {
            self.fill();
        }
    }

    #[inline(always)]
    fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = (self.cache >> (64 - n)) as u32;
        self.drop_bits(n);
        v
    }

    fn align(&mut self) {
        self.drop_bits(self.avail & 7);
    }

    fn check(&self) -> Result<(), String> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        if self.padded as u64 * 8 > self.avail as u64 {
            return Err("compressed data ends early (archive is damaged or cut short)".into());
        }
        Ok(())
    }
}

impl Peek for BitIn<'_> {
    #[inline(always)]
    fn peek16(&self) -> u32 {
        (self.cache >> 48) as u32
    }
    #[inline(always)]
    fn skip(&mut self, n: u32) {
        self.drop_bits(n)
    }
}

impl ByteIn for BitIn<'_> {
    fn byte(&mut self) -> u8 {
        self.bits(8) as u8
    }
}

/// Bit reader over a filter's parameter bytes, with RAR VM numbers.
struct MemBits<'a> {
    b: &'a [u8],
    off: usize,
    bits: u64,
    avail: u32,
    eof: bool,
}

impl MemBits<'_> {
    fn bits(&mut self, n: u32) -> u32 {
        while self.avail < n && self.off < self.b.len() {
            self.bits = (self.bits << 8) | self.b[self.off] as u64;
            self.off += 1;
            self.avail += 8;
        }
        if n > self.avail {
            self.eof = true;
            return 0;
        }
        self.avail -= n;
        ((self.bits >> self.avail) & ((1u64 << n) - 1)) as u32
    }

    fn number(&mut self) -> u32 {
        match self.bits(2) {
            0 => self.bits(4),
            1 => {
                let v = self.bits(8);
                if v >= 16 { v } else { 0xFFFF_FF00 | v << 4 | self.bits(4) }
            }
            2 => self.bits(16),
            _ => self.bits(32),
        }
    }
}

struct Tables {
    main: Box<Huff>,
    off: Box<Huff>,
    low: Box<Huff>,
    len: Box<Huff>,
}

struct Prog {
    fingerprint: u64,
    old_len: u32,
    usage: u32,
}

struct Filter {
    start: u64,
    len: u32,
    fingerprint: u64,
    regs: [u32; 8],
}

const FP_DELTA: u64 = 0x1D_0E06_077D;
const FP_E8: u64 = 0x35_AD57_6887;
const FP_E8E9: u64 = 0x39_3CD7_E57E;
const FP_RGB: u64 = 0x95_1C2C_5DC8;
const FP_AUDIO: u64 = 0xD8_BC85_E701;

/// The RAR 3 decoder. One value decodes a whole archive; in a solid archive
/// the window, tables, PPMd model and filters carry over between entries.
pub(crate) struct Unpack4 {
    win: Vec<u8>,
    /// Absolute output position (bytes decoded so far).
    pos: u64,
    /// Bytes handed to the output so far.
    written: u64,
    /// Where the current entry starts.
    base: u64,
    lengths: [u8; TABLE_N],
    tables: Option<Tables>,
    old: [u32; 4],
    last_off: u32,
    last_len: u32,
    low_off: u32,
    low_reps: u32,
    ppm: Option<PPMd7<RarRc>>,
    ppm_block: bool,
    escape: u32,
    init_esc: u32,
    new_table: bool,
    eod: bool,
    progs: Vec<Prog>,
    filters: Vec<Filter>,
    last_filter: usize,
    vm: Vec<u8>,
}

impl Default for Unpack4 {
    fn default() -> Self {
        Unpack4 {
            win: Vec::new(),
            pos: 0,
            written: 0,
            base: 0,
            lengths: [0; TABLE_N],
            tables: None,
            old: [0; 4],
            last_off: 0,
            last_len: 0,
            low_off: 0,
            low_reps: 0,
            ppm: None,
            ppm_block: false,
            escape: 2,
            init_esc: 0,
            new_table: true,
            eod: false,
            progs: Vec::new(),
            filters: Vec::new(),
            last_filter: 0,
            vm: Vec::new(),
        }
    }
}

fn ppm_sym(m: &mut PPMd7<RarRc>, b: &mut BitIn) -> Result<u32, String> {
    match m.decode_symbol(b) {
        Ok(s) if s >= 0 => Ok(s as u32),
        Ok(_) => Err(DAMAGED.into()),
        Err(e) => Err(e.to_string()),
    }
}

/// A filter record: a flags byte, a length, then the parameter bytes.
fn read_filter_bytes(next: &mut dyn FnMut() -> Result<u8, String>) -> Result<(u8, Vec<u8>), String> {
    let flags = next()?;
    let mut len = (flags & 7) as usize + 1;
    if len == 7 {
        len = next()? as usize + 7;
    } else if len == 8 {
        len = (next()? as usize) << 8;
        len |= next()? as usize;
    }
    let code = (0..len).map(|_| next()).collect::<Result<Vec<u8>, String>>()?;
    Ok((flags, code))
}

impl Unpack4 {
    fn reset(&mut self) {
        if self.win.is_empty() {
            self.win = vec![0; WIN];
            self.vm = vec![0; VM_SIZE + 4];
        }
        self.lengths = [0; TABLE_N];
        self.tables = None;
        self.old = [0; 4];
        self.last_off = 0;
        self.last_len = 0;
        self.low_off = 0;
        self.low_reps = 0;
        self.ppm = None;
        self.ppm_block = false;
        self.escape = 2;
        self.new_table = true;
        self.eod = false;
        self.progs.clear();
        self.filters.clear();
        self.last_filter = 0;
        self.written = self.pos;
    }

    pub fn unpack(&mut self, src: &mut dyn Read, size: u64, dict: u64, solid: bool, out: &mut dyn Write) -> Result<(), String> {
        if dict > WIN as u64 {
            return Err(format!("{} MB dictionary is larger than RAR 4 allows (archive is damaged)", dict >> 20));
        }
        if !solid || self.win.is_empty() {
            self.reset();
        } else if self.pos != self.written {
            return Err("solid stream out of step with the previous entry".into());
        }
        let mut b = BitIn::new(src);
        self.base = self.pos;
        let end = self.pos + size;
        self.eod = false;
        // A PPMd stream restarts its range coder at each entry.
        if self.ppm_block {
            self.new_table = true;
        }
        let mut block_end = false;
        loop {
            self.flush(self.pos.min(end), out)?;
            b.check()?;
            if self.written >= end {
                break;
            }
            if block_end || self.eod {
                return Err("compressed data ends before the file does (archive is damaged)".into());
            }
            if self.pos >= end {
                return Err("a RAR filter reaches past the end of its file (archive is damaged)".into());
            }
            if self.new_table {
                self.parse_codes(&mut b)?;
                continue;
            }
            let limit = end.min(self.written + WIN as u64 - 0x1000);
            if self.ppm_block {
                self.ppm_run(&mut b, limit)?;
            } else {
                let t = self.tables.take().ok_or("RAR data uses Huffman tables it never defined (archive is damaged)")?;
                let r = self.lz_run(&t, &mut b, limit);
                self.tables = Some(t);
                block_end = r?;
            }
        }
        if self.pos > end {
            return Err("decoded past the end of the file (archive is damaged)".into());
        }
        // In LZ mode an entry ends with symbol 256, whose next bit says
        // whether the following solid entry brings new tables.
        if !self.ppm_block && !block_end {
            if let Some(t) = &self.tables {
                if t.main.decode(&mut b) == 256 && b.bits(1) == 0 {
                    self.new_table = b.bits(1) == 1;
                }
            }
        }
        Ok(())
    }

    fn parse_codes(&mut self, b: &mut BitIn) -> Result<(), String> {
        b.align();
        if b.bits(1) == 1 {
            self.ppm_block = true;
            let f = b.bits(7);
            let mem = if f & 0x20 != 0 { (b.bits(8) + 1) << 20 } else { 0 };
            if f & 0x40 != 0 {
                self.escape = b.bits(8);
                self.init_esc = self.escape;
                if let Some(m) = self.ppm.as_mut() {
                    m.init_esc = self.escape;
                }
            } else {
                self.escape = 2;
            }
            if f & 0x20 != 0 {
                let mut order = (f & 0x1f) + 1;
                if order > 16 {
                    order = 16 + (order - 16) * 3;
                }
                if order == 1 {
                    return Err(DAMAGED.into());
                }
                let mut m = PPMd7::new_rar(order, mem)?;
                m.init_esc = self.init_esc;
                if !m.rc_mut().init(b) {
                    return Err(DAMAGED.into());
                }
                self.ppm = Some(m);
            } else {
                let m = self.ppm.as_mut().ok_or("PPMd block continues a model that was never started (archive is damaged)")?;
                if !m.rc_mut().init(b) {
                    return Err(DAMAGED.into());
                }
            }
        } else {
            self.ppm_block = false;
            if b.bits(1) == 0 {
                self.lengths = [0; TABLE_N];
            }
            let mut bl = [0u8; 20];
            let mut i = 0;
            while i < 20 {
                let l = b.bits(4) as u8;
                bl[i] = l;
                i += 1;
                if l == 15 {
                    let zeros = b.bits(4) as usize;
                    if zeros != 0 {
                        i -= 1;
                        for _ in 0..zeros + 2 {
                            if i >= 20 {
                                break;
                            }
                            bl[i] = 0;
                            i += 1;
                        }
                    }
                }
            }
            let pre = Huff::new(&bl);
            let mut i = 0;
            while i < TABLE_N {
                let v = pre.decode(b);
                if v < 16 {
                    self.lengths[i] = (self.lengths[i] + v as u8) & 15;
                    i += 1;
                } else if v < 18 {
                    if i == 0 {
                        return Err(DAMAGED.into());
                    }
                    let n = if v == 16 { b.bits(3) + 3 } else { b.bits(7) + 11 };
                    for _ in 0..n {
                        if i >= TABLE_N {
                            break;
                        }
                        self.lengths[i] = self.lengths[i - 1];
                        i += 1;
                    }
                } else {
                    let n = if v == 18 { b.bits(3) + 3 } else { b.bits(7) + 11 };
                    for _ in 0..n {
                        if i >= TABLE_N {
                            break;
                        }
                        self.lengths[i] = 0;
                        i += 1;
                    }
                }
            }
            let l = &self.lengths;
            self.tables = Some(Tables {
                main: Huff::new(&l[..MAIN_N]),
                off: Huff::new(&l[MAIN_N..MAIN_N + OFF_N]),
                low: Huff::new(&l[MAIN_N + OFF_N..MAIN_N + OFF_N + LOW_N]),
                len: Huff::new(&l[MAIN_N + OFF_N + LOW_N..]),
            });
        }
        self.new_table = false;
        b.check()
    }

    /// LZ symbols until `limit`; true when the end-of-file marker was read.
    fn lz_run(&mut self, t: &Tables, b: &mut BitIn, limit: u64) -> Result<bool, String> {
        while self.pos < limit {
            let sym = t.main.decode(b) as usize;
            if sym < 256 {
                self.win[self.pos as usize & WIN_MASK] = sym as u8;
                self.pos += 1;
                continue;
            }
            let (off, len);
            match sym {
                256 => {
                    if b.bits(1) == 0 {
                        self.new_table = b.bits(1) == 1;
                        return Ok(true);
                    }
                    self.new_table = true;
                    return Ok(false);
                }
                257 => {
                    let (flags, code) = read_filter_bytes(&mut || Ok(b.bits(8) as u8))?;
                    self.parse_filter(flags, &code)?;
                    continue;
                }
                258 => {
                    if self.last_len == 0 {
                        continue;
                    }
                    off = self.last_off;
                    len = self.last_len;
                }
                259..=262 => {
                    let idx = sym - 259;
                    off = self.old[idx];
                    let ls = t.len.decode(b) as usize;
                    if ls >= LEN_N {
                        return Err(DAMAGED.into());
                    }
                    len = LEN_BASE[ls] + 2 + b.bits(LEN_BITS[ls]);
                    for i in (1..=idx).rev() {
                        self.old[i] = self.old[i - 1];
                    }
                    self.old[0] = off;
                }
                263..=270 => {
                    let s = sym - 263;
                    off = SHORT_BASE[s] + 1 + b.bits(SHORT_BITS[s]);
                    len = 2;
                    self.old.copy_within(0..3, 1);
                    self.old[0] = off;
                }
                _ => {
                    let s = sym - 271;
                    if s >= LEN_N {
                        return Err(DAMAGED.into());
                    }
                    let mut l = LEN_BASE[s] + 3 + b.bits(LEN_BITS[s]);
                    let os = t.off.decode(b) as usize;
                    if os >= OFF_N {
                        return Err(DAMAGED.into());
                    }
                    let mut o = OFF_BASE[os] + 1;
                    let ob = OFF_BITS[os];
                    if ob > 0 {
                        if os > 9 {
                            if ob > 4 {
                                o += b.bits(ob - 4) << 4;
                            }
                            if self.low_reps > 0 {
                                self.low_reps -= 1;
                                o += self.low_off;
                            } else {
                                let lo = t.low.decode(b) as u32;
                                if lo == 16 {
                                    self.low_reps = 15;
                                    o += self.low_off;
                                } else {
                                    o += lo;
                                    self.low_off = lo;
                                }
                            }
                        } else {
                            o += b.bits(ob);
                        }
                    }
                    if o >= 0x40000 {
                        l += 1;
                    }
                    if o >= 0x2000 {
                        l += 1;
                    }
                    self.old.copy_within(0..3, 1);
                    self.old[0] = o;
                    off = o;
                    len = l;
                }
            }
            self.last_off = off;
            self.last_len = len;
            self.copy(off, len as usize)?;
        }
        Ok(false)
    }

    fn ppm_run(&mut self, b: &mut BitIn, limit: u64) -> Result<(), String> {
        while self.pos < limit {
            let m = self.ppm.as_mut().ok_or(DAMAGED)?;
            let sym = ppm_sym(m, b)?;
            if sym != self.escape {
                self.win[self.pos as usize & WIN_MASK] = sym as u8;
                self.pos += 1;
                continue;
            }
            match ppm_sym(m, b)? {
                0 => {
                    self.new_table = true;
                    return Ok(());
                }
                2 => {
                    self.eod = true;
                    return Ok(());
                }
                3 => {
                    let (flags, code) = read_filter_bytes(&mut || ppm_sym(m, b).map(|s| s as u8))?;
                    self.parse_filter(flags, &code)?;
                }
                4 => {
                    let mut off = 0u32;
                    for i in (0..3).rev() {
                        off |= ppm_sym(m, b)? << (i * 8);
                    }
                    let len = ppm_sym(m, b)?;
                    self.copy(off + 2, len as usize + 32)?;
                }
                5 => {
                    let len = ppm_sym(m, b)?;
                    self.copy(1, len as usize + 4)?;
                }
                _ => {
                    self.win[self.pos as usize & WIN_MASK] = sym as u8;
                    self.pos += 1;
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    fn copy(&mut self, dist: u32, len: usize) -> Result<(), String> {
        let dist = dist as usize;
        if dist >= WIN {
            return Err(DAMAGED.into());
        }
        let mut dst = self.pos as usize & WIN_MASK;
        let mut src = dst.wrapping_sub(dist) & WIN_MASK;
        if dist >= len && src + len <= WIN && dst + len <= WIN {
            self.win.copy_within(src..src + len, dst);
        } else {
            for _ in 0..len {
                self.win[dst] = self.win[src];
                dst = (dst + 1) & WIN_MASK;
                src = (src + 1) & WIN_MASK;
            }
        }
        self.pos += len as u64;
        Ok(())
    }

    fn parse_filter(&mut self, flags: u8, code: &[u8]) -> Result<(), String> {
        let bad = || "bad RAR filter record (archive is damaged)".to_string();
        let mut r = MemBits { b: code, off: 0, bits: 0, avail: 0, eof: false };
        let num = if flags & 0x80 != 0 {
            let mut n = r.number() as usize;
            if n == 0 {
                self.filters.clear();
                self.progs.clear();
            } else {
                n -= 1;
            }
            if n > self.progs.len() {
                return Err(bad());
            }
            self.last_filter = n;
            n
        } else {
            self.last_filter
        };
        let existing = num < self.progs.len();
        if existing {
            self.progs[num].usage += 1;
        }
        let mut start = r.number() as u64 + self.pos;
        if flags & 0x40 != 0 {
            start += 258;
        }
        let len = if flags & 0x20 != 0 {
            r.number()
        } else if existing {
            self.progs[num].old_len
        } else {
            0
        };
        if len as usize > VM_SIZE {
            return Err(bad());
        }
        let mut regs = [0u32; 8];
        regs[3] = WORK_SIZE as u32;
        regs[4] = len;
        regs[5] = if existing { self.progs[num].usage } else { 0 };
        regs[7] = VM_SIZE as u32;
        if flags & 0x10 != 0 {
            let mask = r.bits(7);
            for (i, reg) in regs.iter_mut().enumerate().take(7) {
                if mask & (1 << i) != 0 {
                    *reg = r.number();
                }
            }
        }
        if !existing {
            let plen = r.number() as usize;
            if plen == 0 || plen > 0x10000 {
                return Err(bad());
            }
            let bytecode: Vec<u8> = (0..plen).map(|_| r.bits(8) as u8).collect();
            let xor = bytecode[1..].iter().fold(0u8, |a, &x| a ^ x);
            if xor != bytecode[0] {
                return Err(bad());
            }
            let fingerprint = crc32fast::hash(&bytecode) as u64 | (plen as u64) << 32;
            self.progs.push(Prog { fingerprint, old_len: 0, usage: 0 });
        }
        self.progs[num].old_len = len;
        if flags & 0x08 != 0 {
            // Global data for the VM program; the native filters ignore it.
            let n = r.number();
            if n > 0x2000 - 0x40 {
                return Err(bad());
            }
            for _ in 0..n {
                r.bits(8);
            }
        }
        if r.eof {
            return Err(bad());
        }
        self.filters.push(Filter { start, len, fingerprint: self.progs[num].fingerprint, regs });
        Ok(())
    }

    /// Writes decoded bytes up to `upto`, running each filter once its
    /// whole block has been decoded.
    fn flush(&mut self, upto: u64, out: &mut dyn Write) -> Result<(), String> {
        loop {
            let Some(f) = self.filters.first() else {
                if upto > self.written {
                    self.write_window(self.written, upto, out)?;
                    self.written = upto;
                }
                return Ok(());
            };
            let (start, end) = (f.start, f.start + f.len as u64);
            if self.written > start {
                return Err("RAR filters overlap (archive is damaged)".into());
            }
            if self.written < start {
                let to = upto.min(start);
                if to > self.written {
                    self.write_window(self.written, to, out)?;
                    self.written = to;
                }
                if self.written < start {
                    return Ok(());
                }
            }
            if upto < end {
                return Ok(());
            }
            self.run_filters(out)?;
        }
    }

    fn write_window(&self, from: u64, to: u64, out: &mut dyn Write) -> Result<(), String> {
        if to - from > WIN as u64 {
            return Err("RAR data outran the dictionary window (archive is damaged)".into());
        }
        let a = from as usize & WIN_MASK;
        let n = (to - from) as usize;
        let first = n.min(WIN - a);
        out.write_all(&self.win[a..a + first]).map_err(|e| e.to_string())?;
        if first < n {
            out.write_all(&self.win[..n - first]).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn run_filters(&mut self, out: &mut dyn Write) -> Result<(), String> {
        let f = self.filters.remove(0);
        let len = f.len as usize;
        let a = f.start as usize & WIN_MASK;
        let first = len.min(WIN - a);
        self.vm[..first].copy_from_slice(&self.win[a..a + first]);
        self.vm[first..len].copy_from_slice(&self.win[..len - first]);
        let file_pos = f.start.saturating_sub(self.base);
        let (mut addr, mut olen) = self.exec(&f, file_pos)?;
        // Filters stacked on the same block run on the previous output.
        while self.filters.first().is_some_and(|n| n.start == f.start && n.len as usize == olen) {
            let n = self.filters.remove(0);
            self.vm.copy_within(addr..addr + olen, 0);
            (addr, olen) = self.exec(&n, file_pos)?;
        }
        if olen != len {
            return Err("RAR filter changed its block size (archive is damaged)".into());
        }
        out.write_all(&self.vm[addr..addr + olen]).map_err(|e| e.to_string())?;
        self.written = f.start + len as u64;
        if self.filters.first().is_some_and(|n| n.start < self.written) {
            return Err("RAR filters overlap (archive is damaged)".into());
        }
        Ok(())
    }

    /// Runs one standard filter on `vm[..len]`; returns where its output is.
    fn exec(&mut self, f: &Filter, file_pos: u64) -> Result<(usize, usize), String> {
        let bad = || "bad RAR filter parameters (archive is damaged)".to_string();
        let r = &f.regs;
        let len = r[4] as usize;
        if len != f.len as usize {
            return Err(bad());
        }
        let vm = &mut self.vm;
        match f.fingerprint {
            FP_DELTA => {
                let ch = r[0] as usize;
                if len > WORK_SIZE / 2 {
                    return Err(bad());
                }
                let mut s = 0;
                for i in 0..ch {
                    let mut last = 0u8;
                    let mut idx = i;
                    while idx < len {
                        if s >= len {
                            return Err(bad());
                        }
                        last = last.wrapping_sub(vm[s]);
                        vm[len + idx] = last;
                        s += 1;
                        idx += ch;
                    }
                }
                Ok((len, len))
            }
            FP_E8 | FP_E8E9 => {
                const FILE_SIZE: u32 = 0x100_0000;
                let e9 = f.fingerprint == FP_E8E9;
                if len > WORK_SIZE || len <= 4 {
                    return Err(bad());
                }
                let mut i = 0;
                while i + 5 <= len {
                    if vm[i] == 0xE8 || (e9 && vm[i] == 0xE9) {
                        let cur = (file_pos as u32).wrapping_add(i as u32 + 1);
                        let addr = le32(&vm[i + 1..]) as i32;
                        let new = if addr < 0 {
                            (cur >= (addr as u32).wrapping_neg()).then(|| (addr as u32).wrapping_add(FILE_SIZE))
                        } else {
                            ((addr as u32) < FILE_SIZE).then(|| (addr as u32).wrapping_sub(cur))
                        };
                        if let Some(v) = new {
                            vm[i + 1..i + 5].copy_from_slice(&v.to_le_bytes());
                        }
                        i += 4;
                    }
                    i += 1;
                }
                Ok((0, len))
            }
            FP_RGB => {
                let stride = r[0] as usize;
                let byte_off = r[1] as usize;
                if len > WORK_SIZE / 2 || stride > len || len < 3 || byte_off > 2 {
                    return Err(bad());
                }
                let d = len;
                let mut s = 0;
                for i in 0..3 {
                    let mut byte = 0u8;
                    let mut prev = (d + i) as isize - stride as isize;
                    let mut j = i;
                    while j < len {
                        if s >= len {
                            return Err(bad());
                        }
                        if prev >= d as isize {
                            let p = prev as usize;
                            let (p0, p3, bi) = (vm[p] as i32, vm[p + 3] as i32, byte as i32);
                            let d1 = (p3 - p0).abs();
                            let d2 = (bi - p0).abs();
                            let d3 = (p3 - p0 + bi - p0).abs();
                            if d1 > d2 || d1 > d3 {
                                byte = if d2 <= d3 { p3 as u8 } else { p0 as u8 };
                            }
                        }
                        byte = byte.wrapping_sub(vm[s]);
                        vm[d + j] = byte;
                        s += 1;
                        prev += 3;
                        j += 3;
                    }
                }
                let mut i = byte_off;
                while i + 2 < len {
                    vm[d + i] = vm[d + i].wrapping_add(vm[d + i + 1]);
                    vm[d + i + 2] = vm[d + i + 2].wrapping_add(vm[d + i + 1]);
                    i += 3;
                }
                Ok((d, len))
            }
            FP_AUDIO => {
                let ch = r[0] as usize;
                if len > WORK_SIZE / 2 {
                    return Err(bad());
                }
                let mut s = 0;
                for i in 0..ch {
                    let mut w = [0i32; 3];
                    let mut dl = [0i32; 3];
                    let mut err = [0i32; 7];
                    let mut last_delta = 0i8;
                    let mut last_byte = 0u8;
                    let mut count = 0u32;
                    let mut j = i;
                    while j < len {
                        if s >= len {
                            return Err(bad());
                        }
                        let delta = vm[s] as i8 as i32;
                        s += 1;
                        dl[2] = dl[1];
                        dl[1] = (last_delta as i32 - dl[0]) as i16 as i32;
                        dl[0] = last_delta as i32;
                        let pred = ((8 * last_byte as i32 + w[0] * dl[0] + w[1] * dl[1] + w[2] * dl[2]) >> 3) & 0xFF;
                        let byte = ((pred - delta) & 0xFF) as u8;
                        let pe = delta << 3;
                        err[0] += pe.abs();
                        err[1] += (pe - dl[0]).abs();
                        err[2] += (pe + dl[0]).abs();
                        err[3] += (pe - dl[1]).abs();
                        err[4] += (pe + dl[1]).abs();
                        err[5] += (pe - dl[2]).abs();
                        err[6] += (pe + dl[2]).abs();
                        last_delta = byte.wrapping_sub(last_byte) as i8;
                        vm[len + j] = byte;
                        last_byte = byte;
                        if count & 0x1F == 0 {
                            let mut idx = 0;
                            for k in 1..7 {
                                if err[k] < err[idx] {
                                    idx = k;
                                }
                            }
                            err = [0; 7];
                            match idx {
                                1 if w[0] >= -16 => w[0] -= 1,
                                2 if w[0] < 16 => w[0] += 1,
                                3 if w[1] >= -16 => w[1] -= 1,
                                4 if w[1] < 16 => w[1] += 1,
                                5 if w[2] >= -16 => w[2] -= 1,
                                6 if w[2] < 16 => w[2] += 1,
                                _ => {}
                            }
                        }
                        count = count.wrapping_add(1);
                        j += ch;
                    }
                }
                Ok((len, len))
            }
            _ => Err("this RAR archive uses a custom RAR VM filter, which PackCraft does not run".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_time_epoch_and_leap_day() {
        // 1980-01-01 00:00:00
        let t = (1 << 21) | (1 << 16);
        assert_eq!(dos_time(t), Some(unix_time(315_532_800, 0)));
        // 2024-02-29 13:45:30
        let t = (44 << 25) | (2 << 21) | (29 << 16) | (13 << 11) | (45 << 5) | 15;
        assert_eq!(dos_time(t), Some(unix_time(1_709_214_330, 0)));
        assert_eq!(dos_time(0), None);
    }

    #[test]
    fn unicode_name_round_trip() {
        // ASCII "ab", NUL, high byte 0x04, flags 01 01 00 00 (two
        // high-byte chars) -> U+0430 U+0431.
        let raw = [b'a', b'b', 0, 0x04, 0b0101_0000, 0x30, 0x31];
        assert_eq!(decode_name(&raw, true), "\u{430}\u{431}");
        assert_eq!(decode_name(b"dir\\file.txt", false), "dir\\file.txt");
    }

    #[test]
    fn rarvm_numbers() {
        // 00 0101 -> 5; 10 + 16 bits 0x1234
        let bytes = [0b0001_0110, 0b0001_0010, 0b0011_0100, 0];
        let mut r = MemBits { b: &bytes, off: 0, bits: 0, avail: 0, eof: false };
        assert_eq!(r.number(), 5);
        assert_eq!(r.number(), 0x1234);
        assert!(!r.eof);
    }
}
