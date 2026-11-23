//! RAR extraction: RAR 5 (`v5.rs`) and RAR 2.9 to 4.x (`v4.rs`), plain or
//! solid, from single files or multi-volume sets (`name.partN.rar` and the
//! older `name.rar`, `name.r00`, `name.r01` ...). Read-only: PackCraft never
//! writes RAR.
//!
//! The decoders are Rust ports of libarchive's BSD-licensed readers (see the
//! notices in `v5.rs` and `v4.rs`), with headers checked against RARLAB's
//! published format description. No RARLAB unRAR source was used.
//! Encrypted RAR archives are refused with a clear error.

pub mod blake2sp;
mod huff;
mod ppmd;
mod v4;
mod v5;

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::archive::safe_join;
use crate::formats::{Item, Sink};

pub(crate) const SIG4: &[u8] = b"Rar!\x1a\x07\x00";
pub(crate) const SIG5: &[u8] = b"Rar!\x1a\x07\x01\x00";

const ENCRYPTED: &str = "this RAR archive is password-protected; PackCraft cannot open encrypted RAR archives yet";

/// Where an entry's packed bytes live: (volume index, offset, length).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seg {
    pub part: usize,
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum Link {
    Symlink(String),
    /// A hard link or file copy: the content of an earlier entry.
    Copy(String),
}

#[derive(Clone, Debug)]
pub(crate) enum Codec {
    Stored,
    /// RAR 5 compression; `dict` is the dictionary size in bytes.
    V5 { dict: u64 },
    /// RAR 2.9 to 4.x compression (LZSS with VM filters, or PPMd).
    V4 { dict: u64 },
    Unsupported(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub encrypted: bool,
    pub solid: bool,
    pub codec: Codec,
    pub method: String,
    pub crc: Option<u32>,
    pub blake: Option<[u8; 32]>,
    pub mtime: Option<SystemTime>,
    pub link: Option<Link>,
    pub segs: Vec<Seg>,
}

impl Entry {
    pub fn packed(&self) -> u64 {
        self.segs.iter().map(|s| s.len).sum()
    }
}

/// Reads an entry's packed bytes across volumes as one stream.
pub(crate) struct SegReader<'a> {
    parts: &'a [PathBuf],
    segs: Vec<Seg>,
    cur: Option<(File, u64)>,
    idx: usize,
}

impl<'a> SegReader<'a> {
    pub fn new(parts: &'a [PathBuf], segs: &[Seg]) -> Self {
        SegReader { parts, segs: segs.to_vec(), cur: None, idx: 0 }
    }
}

impl Read for SegReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cur.is_none() {
                let Some(s) = self.segs.get(self.idx) else { return Ok(0) };
                let path = &self.parts[s.part];
                let mut f = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
                f.seek(SeekFrom::Start(s.offset))?;
                self.cur = Some((f, s.len));
            }
            let (f, left) = self.cur.as_mut().unwrap();
            if *left == 0 {
                self.cur = None;
                self.idx += 1;
                continue;
            }
            let n = (*left).min(buf.len() as u64) as usize;
            let got = f.read(&mut buf[..n])?;
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a RAR volume is shorter than its headers say"));
            }
            *left -= got as u64;
            return Ok(got);
        }
    }
}

/// Read helpers shared by both header parsers.
pub(crate) fn read_exact_at(f: &mut File, buf: &mut [u8]) -> Result<bool, String> {
    let mut done = 0;
    while done < buf.len() {
        match f.read(&mut buf[done..]) {
            Ok(0) if done == 0 => return Ok(false),
            Ok(0) => return Err("RAR header cut short".into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(true)
}

fn parse(parts: &[PathBuf]) -> Result<Vec<Entry>, String> {
    let mut head = [0u8; 8];
    let mut f = File::open(&parts[0]).map_err(|e| format!("{}: {e}", parts[0].display()))?;
    let n = f.read(&mut head).map_err(|e| e.to_string())?;
    if head[..n].starts_with(SIG5) {
        v5::parse(parts)
    } else if head[..n].starts_with(SIG4) {
        v4::parse(parts)
    } else {
        Err(format!("{}: not a RAR archive (self-extracting RAR files are not supported)", parts[0].display()))
    }
}

pub fn list(parts: &[PathBuf], _password: Option<&str>) -> Result<Vec<Item>, String> {
    Ok(parse(parts)?
        .into_iter()
        .map(|e| Item {
            path: e.name.clone(),
            size: e.size,
            packed: Some(e.packed()),
            method: e.method.clone(),
            is_dir: e.is_dir,
            encrypted: e.encrypted,
        })
        .collect())
}

/// Wraps the output writer: counts bytes and computes the checksums RAR
/// stores, so every entry is verified as it is written.
struct Check<'a> {
    w: &'a mut dyn Write,
    crc: crc32fast::Hasher,
    blake: Option<blake2sp::Blake2sp>,
    n: u64,
}

impl Write for Check<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.crc.update(&buf[..n]);
        if let Some(b) = &mut self.blake {
            b.update(&buf[..n]);
        }
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Decoder state that carries over between the entries of a solid archive.
#[derive(Default)]
struct Decoders {
    v5: Option<v5::Unpack5>,
    v4: Option<v4::Unpack4>,
}

fn decode(parts: &[PathBuf], e: &Entry, dec: &mut Decoders, out: &mut dyn Write) -> Result<u64, String> {
    let mut chk = Check { w: out, crc: crc32fast::Hasher::new(), blake: e.blake.map(|_| blake2sp::Blake2sp::new()), n: 0 };
    let mut src = io::BufReader::with_capacity(1 << 20, SegReader::new(parts, &e.segs));
    match &e.codec {
        Codec::Stored => {
            let n = io::copy(&mut (&mut src).take(e.size), &mut chk).map_err(|x| format!("{}: {x}", e.name))?;
            if n != e.size {
                return Err(format!("{}: stored data is cut short", e.name));
            }
        }
        Codec::V5 { dict } => {
            let u = dec.v5.get_or_insert_with(v5::Unpack5::default);
            u.unpack(&mut src, e.size, *dict, e.solid, &mut chk).map_err(|x| format!("{}: {x}", e.name))?;
        }
        Codec::V4 { dict } => {
            let u = dec.v4.get_or_insert_with(v4::Unpack4::default);
            u.unpack(&mut src, e.size, *dict, e.solid, &mut chk).map_err(|x| format!("{}: {x}", e.name))?;
        }
        Codec::Unsupported(why) => return Err(format!("{}: {why}", e.name)),
    }
    chk.flush().map_err(|x| format!("{}: {x}", e.name))?;
    if chk.n != e.size {
        return Err(format!("{}: decoded {} bytes, expected {}", e.name, chk.n, e.size));
    }
    if let Some(want) = e.crc {
        let got = chk.crc.clone().finalize();
        if got != want {
            return Err(format!("{}: CRC32 mismatch (archive is damaged)", e.name));
        }
    }
    if let (Some(want), Some(b)) = (e.blake, chk.blake.take()) {
        if b.finish() != want {
            return Err(format!("{}: BLAKE2sp mismatch (archive is damaged)", e.name));
        }
    }
    Ok(chk.n)
}

pub fn extract(parts: &[PathBuf], dest: Option<&Path>, password: Option<&str>, keep: &dyn Fn(&str) -> bool) -> Result<(usize, u64), String> {
    let entries = parse(parts)?;
    if entries.iter().any(|e| e.encrypted) {
        let _ = password;
        return Err(ENCRYPTED.into());
    }
    let solid = entries.iter().any(|e| e.solid);
    let mut dec = Decoders::default();
    let mut sink = Sink::default();
    let mut copies = Vec::new();
    for e in &entries {
        let at = Sink::target(dest, &e.name, keep)?;
        if e.is_dir {
            if let Some(at) = &at {
                Sink::dir(at)?;
            }
            continue;
        }
        let wanted = at.is_some() || dest.is_none();
        // A solid archive's later entries depend on every earlier one, so
        // skipped entries are still decoded (into nowhere).
        let must_decode = e.packed() > 0 && (wanted || solid);
        match &e.link {
            Some(Link::Symlink(target)) => {
                if must_decode {
                    decode(parts, e, &mut dec, &mut io::sink())?;
                }
                if wanted {
                    sink.link(&e.name, at, target.clone())?;
                }
            }
            Some(Link::Copy(target)) => {
                if must_decode {
                    decode(parts, e, &mut dec, &mut io::sink())?;
                }
                if wanted {
                    copies.push((e.name.clone(), at, target.clone()));
                }
            }
            None if wanted => sink.write_with(at.as_deref(), e.mtime, &mut |w| decode(parts, e, &mut dec, w))?,
            None if must_decode => {
                decode(parts, e, &mut dec, &mut io::sink())?;
            }
            None => {}
        }
    }
    for (name, at, target) in copies {
        let target = target.replace('\\', "/");
        if let (Some(at), Some(d)) = (at, dest) {
            let from = safe_join(d, &target)?;
            if let Some(p) = at.parent() {
                Sink::dir(p)?;
            }
            std::fs::copy(&from, &at).map_err(|e| format!("{name}: copy of {target}: {e}"))?;
        }
        sink.count += 1;
    }
    sink.finish()
}

/// Windows FILETIME (100 ns ticks since 1601) to SystemTime.
pub(crate) fn filetime(t: u64) -> Option<SystemTime> {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let secs = t / 10_000_000;
    let nanos = (t % 10_000_000) * 100;
    secs.checked_sub(EPOCH_DIFF)
        .map(|s| SystemTime::UNIX_EPOCH + std::time::Duration::new(s, nanos as u32))
}

pub(crate) fn unix_time(secs: u64, nanos: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::new(secs, nanos.min(999_999_999))
}

/// Archive names use `/`; RAR from Windows hosts may use `\`.
pub(crate) fn clean_name(raw: &str) -> String {
    raw.replace('\\', "/").trim_end_matches('/').to_string()
}
