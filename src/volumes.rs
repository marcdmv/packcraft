//! Multi-part archives: finding every part of a set from any one of them,
//! reading the parts as one stream, and splitting a finished archive.
//!
//! Naming schemes recognised:
//! - `name.ext.001`, `.002`, ...  byte-split (7-Zip style), any format
//! - `name.part1.rar`, `.part2.rar` ...  RAR 3 and later
//! - `name.rar`, `name.r00`, `.r01` ... `.r99`, `.s00` ...  old-style RAR
//! - `name.z01`, `.z02` ... `name.zip`  spanned ZIP (PKWARE split archive)

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How the parts of a set relate to each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// One file.
    Single,
    /// Byte-split: concatenating the parts gives the archive.
    Numbered,
    /// RAR volumes: each part is a RAR archive of its own.
    Rar,
    /// Spanned ZIP: offsets in the central directory are per part.
    ZipSpanned,
}

pub struct VolumeSet {
    pub scheme: Scheme,
    /// In reading order.
    pub parts: Vec<PathBuf>,
}

impl VolumeSet {
    pub fn total_len(&self) -> io::Result<u64> {
        self.parts.iter().map(|p| std::fs::metadata(p).map(|m| m.len())).sum()
    }
}

fn sibling(p: &Path, name: &str) -> PathBuf {
    p.with_file_name(name)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Collects `make(i)` for i = start, start+1, ... while the file exists.
fn run_of(start: usize, make: impl Fn(usize) -> Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut i = start;
    while let Some(p) = make(i) {
        if !p.is_file() {
            break;
        }
        out.push(p);
        i += 1;
    }
    out
}

/// Old-style RAR continuation suffix for volume `i` (0-based after the .rar):
/// r00..r99, s00..s99, ... z99.
fn old_rar_ext(i: usize) -> Option<String> {
    let letter = b'r' + (i / 100) as u8;
    (letter <= b'z').then(|| format!("{}{:02}", letter as char, i % 100))
}

/// Splits `name.ext` into (`name`, `ext`) on the last dot, lowercase ext.
fn stem_ext(name: &str) -> Option<(&str, String)> {
    let dot = name.rfind('.')?;
    Some((&name[..dot], name[dot + 1..].to_ascii_lowercase()))
}

/// Finds the whole set that `path` belongs to. A plain file is a set of one.
pub fn discover(path: &Path) -> io::Result<VolumeSet> {
    if !path.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{}: not found", path.display())));
    }
    let name = file_name(path);
    let single = || VolumeSet { scheme: Scheme::Single, parts: vec![path.to_path_buf()] };
    let Some((stem, ext)) = stem_ext(&name) else { return Ok(single()) };

    // name.ext.001: numbered byte-split.
    if ext.len() >= 3 && ext.bytes().all(|b| b.is_ascii_digit()) {
        let width = ext.len();
        let parts = run_of(1, |i| Some(sibling(path, &format!("{stem}.{i:0width$}"))));
        if parts.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{}: the first part ({stem}.{:0width$}) is missing", path.display(), 1)));
        }
        return Ok(VolumeSet { scheme: Scheme::Numbered, parts });
    }

    // name.partN.rar
    if ext == "rar"
        && let Some((base, part)) = stem_ext(stem)
        && let Some(num) = part.strip_prefix("part")
        && !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
    {
        let width = num.len();
        // Keep the original case of the "part" and "rar" spellings.
        let part_word = &stem[base.len() + 1..base.len() + 5];
        let rar_word = &name[stem.len() + 1..];
        let parts = run_of(1, |i| Some(sibling(path, &format!("{base}.{part_word}{i:0width$}.{rar_word}"))));
        if !parts.is_empty() {
            return Ok(VolumeSet { scheme: Scheme::Rar, parts });
        }
    }

    // name.rar + name.r00.. (or any one of the .rNN given)
    let is_old_cont = ext.len() == 3 && (b'r'..=b'z').contains(&ext.as_bytes()[0]) && ext[1..].bytes().all(|b| b.is_ascii_digit());
    if ext == "rar" || is_old_cont {
        let upper = name[stem.len() + 1..].starts_with(|c: char| c.is_ascii_uppercase());
        let rar = if upper { "RAR" } else { "rar" };
        let cont = |s: String| if upper { s.to_ascii_uppercase() } else { s };
        let head = sibling(path, &format!("{stem}.{rar}"));
        if head.is_file() {
            let mut parts = vec![head];
            parts.extend(run_of(0, |i| old_rar_ext(i).map(|e| sibling(path, &format!("{stem}.{}", cont(e))))));
            let scheme = Scheme::Rar;
            return Ok(VolumeSet { scheme, parts });
        }
    }

    // name.z01.. + name.zip (or any .zNN given)
    let is_zip_cont = ext.len() >= 3 && ext.starts_with('z') && ext[1..].bytes().all(|b| b.is_ascii_digit());
    if ext == "zip" || is_zip_cont {
        let zip_word = if name[stem.len() + 1..].starts_with('Z') { ("Z", "ZIP") } else { ("z", "zip") };
        let head = sibling(path, &format!("{stem}.{}01", zip_word.0));
        let last = sibling(path, &format!("{stem}.{}", zip_word.1));
        if head.is_file() && last.is_file() {
            let mut parts = run_of(1, |i| Some(sibling(path, &format!("{stem}.{}{i:02}", zip_word.0))));
            parts.push(last);
            return Ok(VolumeSet { scheme: Scheme::ZipSpanned, parts });
        }
    }
    Ok(single())
}

/// The parts of a byte-split set, read as one seekable stream.
pub struct MultiReader {
    files: Vec<File>,
    starts: Vec<u64>,
    total: u64,
    pos: u64,
}

impl MultiReader {
    pub fn open(parts: &[PathBuf]) -> io::Result<Self> {
        let mut files = Vec::with_capacity(parts.len());
        let mut starts = Vec::with_capacity(parts.len());
        let mut total = 0u64;
        for p in parts {
            let f = File::open(p).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
            starts.push(total);
            total += f.metadata()?.len();
            files.push(f);
        }
        Ok(Self { files, starts, total, pos: 0 })
    }

    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

impl Read for MultiReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.total || buf.is_empty() {
            return Ok(0);
        }
        let i = self.starts.partition_point(|&s| s <= self.pos) - 1;
        let end = self.starts.get(i + 1).copied().unwrap_or(self.total);
        let want = buf.len().min((end - self.pos) as usize);
        let f = &mut self.files[i];
        f.seek(SeekFrom::Start(self.pos - self.starts[i]))?;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a part is shorter than when it was opened"));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MultiReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.total.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

/// Reads a whole set into memory (byte-split or single).
pub fn read_all(path: &Path) -> Result<Vec<u8>, String> {
    let set = discover(path).map_err(|e| e.to_string())?;
    let mut r = MultiReader::open(&set.parts).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(r.len() as usize);
    r.read_to_end(&mut out).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(out)
}

/// The name of part `i` (1-based) of a byte-split set for `archive`.
pub fn numbered_part(archive: &Path, i: usize) -> PathBuf {
    sibling(archive, &format!("{}.{i:03}", file_name(archive)))
}

/// Parses a volume size such as `100m`, `4092M`, `1.5g`, `700000k` or a byte count.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mul) = match t.trim_end_matches('b').chars().last() {
        Some('k') => (&t[..t.trim_end_matches('b').len() - 1], 1u64 << 10),
        Some('m') => (&t[..t.trim_end_matches('b').len() - 1], 1 << 20),
        Some('g') => (&t[..t.trim_end_matches('b').len() - 1], 1 << 30),
        _ => (t.as_str(), 1),
    };
    let v: f64 = num.trim().parse().map_err(|_| format!("bad volume size {s:?} (try 100m, 700m, 4g)"))?;
    let bytes = (v * mul as f64) as u64;
    if bytes < 64 << 10 {
        return Err(format!("volume size {s:?} is too small (64 KB minimum)"));
    }
    Ok(bytes)
}

/// Splits the finished file `src` into `dest_archive.001`, `.002`, ... of at
/// most `size` bytes each, then deletes `src`. Refuses to overwrite parts.
pub fn split_numbered(src: &Path, dest_archive: &Path, size: u64) -> Result<Vec<PathBuf>, String> {
    let total = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?.len();
    let count = total.div_ceil(size).max(1) as usize;
    let names: Vec<PathBuf> = (1..=count).map(|i| numbered_part(dest_archive, i)).collect();
    if let Some(p) = names.iter().find(|p| p.exists()) {
        return Err(format!("{} already exists", p.display()));
    }
    let mut input = File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    for p in &names {
        let mut out = File::create_new(p).map_err(|e| format!("{}: {e}", p.display()))?;
        io::copy(&mut (&mut input).take(size), &mut out).map_err(|e| format!("{}: {e}", p.display()))?;
        out.flush().map_err(|e| format!("{}: {e}", p.display()))?;
    }
    std::fs::remove_file(src).map_err(|e| format!("{}: {e}", src.display()))?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("packcraft-vol-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(d: &Path, names: &[&str]) {
        for n in names {
            std::fs::write(d.join(n), n.as_bytes()).unwrap();
        }
    }

    fn names(set: &VolumeSet) -> Vec<String> {
        set.parts.iter().map(|p| file_name(p)).collect()
    }

    #[test]
    fn finds_every_naming_scheme_from_any_part() {
        let d = dir("names");
        touch(&d, &["a.7z.001", "a.7z.002", "a.7z.003", "b.part1.rar", "b.part2.rar", "c.rar", "c.r00", "c.r01", "e.z01", "e.z02", "e.zip", "f.zip"]);
        let s = discover(&d.join("a.7z.002")).unwrap();
        assert_eq!((s.scheme, names(&s)), (Scheme::Numbered, vec!["a.7z.001".into(), "a.7z.002".into(), "a.7z.003".into()]));
        let s = discover(&d.join("b.part2.rar")).unwrap();
        assert_eq!((s.scheme, names(&s)), (Scheme::Rar, vec!["b.part1.rar".into(), "b.part2.rar".into()]));
        let s = discover(&d.join("c.r01")).unwrap();
        assert_eq!((s.scheme, names(&s)), (Scheme::Rar, vec!["c.rar".into(), "c.r00".into(), "c.r01".into()]));
        let s = discover(&d.join("e.zip")).unwrap();
        assert_eq!((s.scheme, names(&s)), (Scheme::ZipSpanned, vec!["e.z01".into(), "e.z02".into(), "e.zip".into()]));
        let s = discover(&d.join("f.zip")).unwrap();
        assert_eq!(s.scheme, Scheme::Single);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn old_rar_suffixes_roll_over_to_s() {
        assert_eq!(old_rar_ext(0).unwrap(), "r00");
        assert_eq!(old_rar_ext(99).unwrap(), "r99");
        assert_eq!(old_rar_ext(100).unwrap(), "s00");
        assert!(old_rar_ext(900).is_none());
    }

    #[test]
    fn split_then_read_back_is_identical() {
        let d = dir("split");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let src = d.join("tmp.bin");
        std::fs::write(&src, &data).unwrap();
        let parts = split_numbered(&src, &d.join("x.7z"), 100_000).unwrap();
        assert_eq!(parts.len(), 3);
        assert!(!src.exists());
        let mut r = MultiReader::open(&discover(&parts[1]).unwrap().parts).unwrap();
        let mut back = Vec::new();
        r.read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
        // Seeking across a boundary.
        r.seek(SeekFrom::Start(99_998)).unwrap();
        let mut four = [0u8; 4];
        r.read_exact(&mut four).unwrap();
        assert_eq!(&four, &data[99_998..100_002]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn volume_sizes() {
        assert_eq!(parse_size("100m").unwrap(), 100 << 20);
        assert_eq!(parse_size("4092MB").unwrap(), 4092 << 20);
        assert_eq!(parse_size("1.5g").unwrap(), 3 << 29);
        assert!(parse_size("1k").is_err());
        assert!(parse_size("lots").is_err());
    }
}
