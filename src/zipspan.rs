//! Spanned ZIP archives (`name.z01`, `name.z02`, ... `name.zip`), written from
//! the PKWARE APPNOTE: in a split archive every central-directory record names
//! the part ("disk") its file starts on and an offset inside that part, and the
//! first part starts with the 0x08074b50 marker.
//!
//! - `join` rebuilds an ordinary single-file zip from the parts (offsets made
//!   global, disk numbers zeroed), which any zip reader then opens.
//! - `split` turns an ordinary zip into parts of at most `size` bytes. A local
//!   header, a central-directory record or the end records never straddle two
//!   parts (a part may end a little early instead); file data may.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::volumes::MultiReader;

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const EOCD: u32 = 0x0605_4b50;
const EOCD64: u32 = 0x0606_4b50;
const LOCATOR64: u32 = 0x0706_4b50;
const SPLIT_MARK: u32 = 0x0807_4b50;
const SPLIT_MARK_ALT: u32 = 0x3030_4b50; // "PK00": a split archive that ended up on one part

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn bad(what: &str) -> String {
    format!("zip central directory is damaged ({what})")
}

/// One central-directory record with its zip64 values resolved.
struct CdEntry {
    fixed: [u8; 46],
    name: Vec<u8>,
    /// Extra fields without the zip64 (0x0001) block.
    extra: Vec<u8>,
    comment: Vec<u8>,
    usize_: u64,
    csize: u64,
    offset: u64,
    disk: u32,
}

impl CdEntry {
    /// Serialises with `offset` and `disk`, adding a zip64 block only for the
    /// values that do not fit.
    fn encode(&self, offset: u64, disk: u32) -> Vec<u8> {
        let mut fixed = self.fixed;
        let mut z64 = Vec::new();
        let mut put32 = |at: usize, v: u64, z: &mut Vec<u8>| {
            if v >= 0xFFFF_FFFF {
                fixed[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                z.extend_from_slice(&v.to_le_bytes());
            } else {
                fixed[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
            }
        };
        // APPNOTE order: uncompressed, compressed, offset, disk.
        put32(24, self.usize_, &mut z64);
        put32(20, self.csize, &mut z64);
        put32(42, offset, &mut z64);
        if disk >= 0xFFFF {
            fixed[34..36].copy_from_slice(&0xFFFFu16.to_le_bytes());
            z64.extend_from_slice(&disk.to_le_bytes());
        } else {
            fixed[34..36].copy_from_slice(&(disk as u16).to_le_bytes());
        }
        let mut extra = Vec::new();
        if !z64.is_empty() {
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&(z64.len() as u16).to_le_bytes());
            extra.extend_from_slice(&z64);
        }
        extra.extend_from_slice(&self.extra);
        fixed[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
        let mut out = fixed.to_vec();
        out.extend_from_slice(&self.name);
        out.extend_from_slice(&extra);
        out.extend_from_slice(&self.comment);
        out
    }
}

fn parse_cd(cd: &[u8], count: u64) -> Result<Vec<CdEntry>, String> {
    let mut out = Vec::with_capacity(count.min(1 << 20) as usize);
    let mut p = 0usize;
    for _ in 0..count {
        let fixed: [u8; 46] = cd.get(p..p + 46).ok_or_else(|| bad("truncated record"))?.try_into().unwrap();
        if u32_at(&fixed, 0) != CENTRAL {
            return Err(bad("record signature"));
        }
        let (nl, xl, cl) = (u16_at(&fixed, 28) as usize, u16_at(&fixed, 30) as usize, u16_at(&fixed, 32) as usize);
        let body = cd.get(p + 46..p + 46 + nl + xl + cl).ok_or_else(|| bad("truncated name or extra"))?;
        let (name, rest) = body.split_at(nl);
        let (raw_extra, comment) = rest.split_at(xl);
        let mut e = CdEntry {
            fixed,
            name: name.to_vec(),
            extra: Vec::new(),
            comment: comment.to_vec(),
            usize_: u32_at(&fixed, 24) as u64,
            csize: u32_at(&fixed, 20) as u64,
            offset: u32_at(&fixed, 42) as u64,
            disk: u16_at(&fixed, 34) as u32,
        };
        let mut q = 0usize;
        while q + 4 <= raw_extra.len() {
            let (id, len) = (u16_at(raw_extra, q), u16_at(raw_extra, q + 2) as usize);
            let data = raw_extra.get(q + 4..q + 4 + len).ok_or_else(|| bad("extra field length"))?;
            if id == 1 {
                let mut z = 0usize;
                let mut take64 = |cur: u64| -> Result<u64, String> {
                    if cur != 0xFFFF_FFFF {
                        return Ok(cur);
                    }
                    let v = data.get(z..z + 8).ok_or_else(|| bad("zip64 field"))?;
                    z += 8;
                    Ok(u64_at(v, 0))
                };
                e.usize_ = take64(e.usize_)?;
                e.csize = take64(e.csize)?;
                e.offset = take64(e.offset)?;
                if e.disk == 0xFFFF {
                    e.disk = u32_at(data.get(z..z + 4).ok_or_else(|| bad("zip64 disk"))?, 0);
                }
            } else {
                e.extra.extend_from_slice(&raw_extra[q..q + 4 + len]);
            }
            q += 4 + len;
        }
        out.push(e);
        p += 46 + nl + xl + cl;
    }
    Ok(out)
}

/// End-of-archive facts, zip64 resolved.
struct End {
    entries: u64,
    cd_size: u64,
    cd_offset: u64,
    cd_disk: u32,
}

/// Reads the end records from the tail of `last` (the final part's bytes),
/// fetching a zip64 end record through `fetch(disk, offset, len)` if present.
fn parse_end(last: &[u8], fetch: &mut dyn FnMut(u32, u64, usize) -> io::Result<Vec<u8>>) -> Result<End, String> {
    let min = last.len().saturating_sub(22 + 0xFFFF);
    let at = (min..=last.len().saturating_sub(22))
        .rev()
        .find(|&i| u32_at(last, i) == EOCD && i + 22 + u16_at(last, i + 20) as usize == last.len())
        .or_else(|| (min..=last.len().saturating_sub(22)).rev().find(|&i| u32_at(last, i) == EOCD))
        .ok_or_else(|| "not a zip archive (no end of central directory)".to_string())?;
    let e = &last[at..];
    let mut end = End { entries: u16_at(e, 10) as u64, cd_size: u32_at(e, 12) as u64, cd_offset: u32_at(e, 16) as u64, cd_disk: u16_at(e, 6) as u32 };
    if at >= 20 && u32_at(last, at - 20) == LOCATOR64 {
        let loc = &last[at - 20..at];
        let z = fetch(u32_at(loc, 4), u64_at(loc, 8), 56).map_err(|err| bad(&format!("zip64 end record: {err}")))?;
        if u32_at(&z, 0) != EOCD64 {
            return Err(bad("zip64 end signature"));
        }
        end = End { entries: u64_at(&z, 32), cd_size: u64_at(&z, 40), cd_offset: u64_at(&z, 48), cd_disk: u32_at(&z, 20) };
    }
    Ok(end)
}

fn end_records(entries: u64, entries_here: u64, cd_size: u64, cd_offset: u64, cd_disk: u32, this_disk: u32, z64_offset: u64) -> Vec<u8> {
    let need64 = entries >= 0xFFFF || entries_here >= 0xFFFF || cd_size >= 0xFFFF_FFFF || cd_offset >= 0xFFFF_FFFF || cd_disk >= 0xFFFF || this_disk >= 0xFFFF;
    let mut out = Vec::new();
    if need64 {
        out.extend_from_slice(&EOCD64.to_le_bytes());
        out.extend_from_slice(&44u64.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&45u16.to_le_bytes());
        out.extend_from_slice(&this_disk.to_le_bytes());
        out.extend_from_slice(&cd_disk.to_le_bytes());
        out.extend_from_slice(&entries_here.to_le_bytes());
        out.extend_from_slice(&entries.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&LOCATOR64.to_le_bytes());
        out.extend_from_slice(&this_disk.to_le_bytes());
        out.extend_from_slice(&z64_offset.to_le_bytes());
        out.extend_from_slice(&(this_disk + 1).to_le_bytes());
    }
    let c16 = |v: u64| if v >= 0xFFFF { 0xFFFFu16 } else { v as u16 };
    let c32 = |v: u64| if v >= 0xFFFF_FFFF { 0xFFFF_FFFFu32 } else { v as u32 };
    out.extend_from_slice(&EOCD.to_le_bytes());
    out.extend_from_slice(&c16(this_disk as u64).to_le_bytes());
    out.extend_from_slice(&c16(cd_disk as u64).to_le_bytes());
    out.extend_from_slice(&c16(entries_here).to_le_bytes());
    out.extend_from_slice(&c16(entries).to_le_bytes());
    out.extend_from_slice(&c32(cd_size).to_le_bytes());
    out.extend_from_slice(&c32(cd_offset).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A temporary file deleted on drop.
pub struct TempFile(pub PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn temp_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("packcraft-{tag}-{}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

/// Rebuilds one ordinary zip from the parts of a spanned set.
pub fn join(parts: &[PathBuf]) -> Result<TempFile, String> {
    let io = |e: io::Error| e.to_string();
    let mut starts = Vec::with_capacity(parts.len());
    let mut total = 0u64;
    for p in parts {
        starts.push(total);
        total += std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?.len();
    }
    let mut r = MultiReader::open(parts).map_err(io)?;
    let mut head = [0u8; 4];
    r.read_exact(&mut head).map_err(io)?;
    let mark = u32::from_le_bytes(head);
    let strip: u64 = if mark == SPLIT_MARK || mark == SPLIT_MARK_ALT { 4 } else { 0 };

    let last_len = total - starts[parts.len() - 1];
    let tail = last_len.min(22 + 0xFFFF + 20);
    let mut last = vec![0u8; tail as usize];
    r.seek(SeekFrom::Start(total - tail)).map_err(io)?;
    r.read_exact(&mut last).map_err(io)?;
    let global = |disk: u32, off: u64| -> Result<u64, String> {
        let s = *starts.get(disk as usize).ok_or_else(|| bad(&format!("disk {} of {} parts", disk + 1, parts.len())))?;
        Ok(s + off)
    };
    let mut fetch = |disk: u32, off: u64, len: usize| -> io::Result<Vec<u8>> {
        let g = global(disk, off).map_err(io::Error::other)?;
        let mut b = vec![0u8; len];
        r.seek(SeekFrom::Start(g))?;
        r.read_exact(&mut b)?;
        Ok(b)
    };
    let end = parse_end(&last, &mut fetch)?;
    let cd_start = global(end.cd_disk, end.cd_offset)?;
    let mut cd = vec![0u8; end.cd_size as usize];
    r.seek(SeekFrom::Start(cd_start)).map_err(io)?;
    r.read_exact(&mut cd).map_err(io)?;
    let entries = parse_cd(&cd, end.entries)?;

    let out_path = temp_path("joined");
    let tmp = TempFile(out_path.clone());
    let mut out = io::BufWriter::new(File::create_new(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?);
    r.seek(SeekFrom::Start(strip)).map_err(io)?;
    io::copy(&mut (&mut r).take(cd_start - strip), &mut out).map_err(io)?;
    let mut new_cd = Vec::with_capacity(cd.len() + 64);
    for e in &entries {
        let g = global(e.disk, e.offset)?;
        new_cd.extend_from_slice(&e.encode(g - strip, 0));
    }
    let new_cd_offset = cd_start - strip;
    out.write_all(&new_cd).map_err(io)?;
    let n = entries.len() as u64;
    out.write_all(&end_records(n, n, new_cd.len() as u64, new_cd_offset, 0, 0, new_cd_offset + new_cd.len() as u64)).map_err(io)?;
    out.flush().map_err(io)?;
    Ok(tmp)
}

/// Writes parts `stem.z01`, `stem.z02`, ... and finally `stem.zip`.
struct SpanWriter {
    dest: PathBuf,
    size: u64,
    disk: u32,
    len: u64,
    file: Option<io::BufWriter<File>>,
    parts: Vec<PathBuf>,
}

impl SpanWriter {
    fn part_name(&self, disk: u32) -> PathBuf {
        let stem = self.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.dest.with_file_name(format!("{stem}.z{:02}", disk + 1))
    }

    fn open_next(&mut self) -> Result<(), String> {
        if let Some(mut f) = self.file.take() {
            f.flush().map_err(|e| e.to_string())?;
            self.disk += 1;
        }
        let p = self.part_name(self.disk);
        let f = File::create_new(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        self.parts.push(p);
        self.file = Some(io::BufWriter::new(f));
        self.len = 0;
        Ok(())
    }

    /// Starts a new part unless `n` more bytes fit in the current one.
    fn keep_together(&mut self, n: u64) -> Result<(), String> {
        if self.len > 0 && self.len + n > self.size {
            self.open_next()?;
        }
        Ok(())
    }

    fn write(&mut self, mut b: &[u8]) -> Result<(), String> {
        while !b.is_empty() {
            if self.len >= self.size {
                self.open_next()?;
            }
            let room = (self.size - self.len).min(b.len() as u64) as usize;
            self.file.as_mut().unwrap().write_all(&b[..room]).map_err(|e| e.to_string())?;
            self.len += room as u64;
            b = &b[room..];
        }
        Ok(())
    }

    /// Flushes and renames the final part to `stem.zip`.
    fn finish(mut self) -> Result<Vec<PathBuf>, String> {
        if let Some(mut f) = self.file.take() {
            f.flush().map_err(|e| e.to_string())?;
        }
        let last = self.parts.pop().unwrap();
        if self.dest.exists() {
            return Err(format!("{} already exists", self.dest.display()));
        }
        std::fs::rename(&last, &self.dest).map_err(|e| format!("{}: {e}", self.dest.display()))?;
        self.parts.push(self.dest.clone());
        Ok(self.parts)
    }
}

/// Splits the ordinary zip `src` into a spanned set ending in `dest` (a `.zip`
/// path), each part at most `size` bytes. Deletes `src` on success.
pub fn split(src: &Path, dest: &Path, size: u64) -> Result<Vec<PathBuf>, String> {
    const MIN_PART: u64 = 64 << 10;
    if size < MIN_PART {
        return Err("volume size is too small (64 KB minimum)".into());
    }
    let io = |e: io::Error| e.to_string();
    let mut f = File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let total = f.metadata().map_err(io)?.len();
    let tail = total.min(22 + 0xFFFF + 20);
    let mut last = vec![0u8; tail as usize];
    f.seek(SeekFrom::Start(total - tail)).map_err(io)?;
    f.read_exact(&mut last).map_err(io)?;
    let mut fetch = |_d: u32, off: u64, len: usize| -> io::Result<Vec<u8>> {
        let mut b = vec![0u8; len];
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(&mut b)?;
        Ok(b)
    };
    let end = parse_end(&last, &mut fetch)?;
    let mut cd = vec![0u8; end.cd_size as usize];
    f.seek(SeekFrom::Start(end.cd_offset)).map_err(io)?;
    f.read_exact(&mut cd).map_err(io)?;
    let entries = parse_cd(&cd, end.entries)?;
    if entries.iter().any(|e| e.disk != 0) {
        return Err("the zip to split is already multi-part".into());
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| entries[i].offset);

    let mut w = SpanWriter { dest: dest.to_path_buf(), size, disk: 0, len: 0, file: None, parts: Vec::new() };
    let result = (|| -> Result<Vec<PathBuf>, String> {
        w.open_next()?;
        w.write(&SPLIT_MARK.to_le_bytes())?;
        let mut placed = vec![(0u32, 0u64); entries.len()];
        let mut buf = vec![0u8; 1 << 20];
        for (k, &i) in order.iter().enumerate() {
            let start = entries[i].offset;
            let stop = order.get(k + 1).map(|&j| entries[j].offset).unwrap_or(end.cd_offset);
            let mut lh = [0u8; 30];
            f.seek(SeekFrom::Start(start)).map_err(io)?;
            f.read_exact(&mut lh).map_err(io)?;
            if u32_at(&lh, 0) != LOCAL {
                return Err(format!("{}: local header missing", String::from_utf8_lossy(&entries[i].name)));
            }
            let header_len = 30 + u16_at(&lh, 26) as u64 + u16_at(&lh, 28) as u64;
            if header_len > size {
                return Err("volume size is smaller than one file header".into());
            }
            w.keep_together(header_len)?;
            placed[i] = (w.disk, w.len);
            // Header and data, copied as is (sizes and CRCs are unchanged).
            f.seek(SeekFrom::Start(start)).map_err(io)?;
            let mut left = stop - start;
            let mut first = true;
            while left > 0 {
                let n = left.min(buf.len() as u64) as usize;
                f.read_exact(&mut buf[..n]).map_err(io)?;
                if first {
                    // The header must land whole; it fits (checked above).
                    let h = header_len.min(n as u64) as usize;
                    w.write(&buf[..h])?;
                    w.write(&buf[h..n])?;
                    first = false;
                } else {
                    w.write(&buf[..n])?;
                }
                left -= n as u64;
            }
        }
        let mut cd_disk = None;
        let mut cd_offset = 0;
        let mut cd_size = 0u64;
        let mut here = 0u64;
        for (i, e) in entries.iter().enumerate() {
            let rec = e.encode(placed[i].1, placed[i].0);
            let before = w.disk;
            w.keep_together(rec.len() as u64)?;
            if w.disk != before {
                here = 0;
            }
            if cd_disk.is_none() {
                cd_disk = Some(w.disk);
                cd_offset = w.len;
            }
            w.write(&rec)?;
            cd_size += rec.len() as u64;
            here += 1;
        }
        let n = entries.len() as u64;
        let cd_disk = cd_disk.unwrap_or(w.disk);
        let probe = end_records(n, here, cd_size, cd_offset, cd_disk, w.disk, 0);
        let before = w.disk;
        w.keep_together(probe.len() as u64)?;
        if w.disk != before {
            here = 0;
        }
        let records = end_records(n, here, cd_size, cd_offset, cd_disk, w.disk, w.len);
        w.write(&records)?;
        Ok(Vec::new())
    })();
    match result {
        Ok(_) => {
            let parts = w.finish()?;
            std::fs::remove_file(src).map_err(|e| format!("{}: {e}", src.display()))?;
            Ok(parts)
        }
        Err(e) => {
            drop(w.file.take());
            for p in &w.parts {
                let _ = std::fs::remove_file(p);
            }
            Err(e)
        }
    }
}
