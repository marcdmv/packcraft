//! The `.pcraft` container: content detection, routing, parallel coding and
//! extraction.
//!
//! Layout (all integers little-endian):
//! ```text
//! b"PKCRAFT1"
//! u64 index length, index (context-mixing stream of the entry and group table)
//! group payloads, back to back in index order
//! ```
//! Generic files are concatenated, sorted by extension then path, into one
//! "solid" stream that is cut into chunks coded in parallel. JPEG and WAV files
//! are coded one per group by their own route.

use crate::{audio, cm, jpeg};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const MAGIC: &[u8; 8] = b"PKCRAFT1";
const SOLID: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Route {
    Store = 0,
    Cm = 1,
    Jpeg = 2,
    Audio = 3,
    Brotli = 4,
}

impl Route {
    fn from_u8(v: u8) -> Result<Self, String> {
        Ok(match v {
            0 => Route::Store,
            1 => Route::Cm,
            2 => Route::Jpeg,
            3 => Route::Audio,
            4 => Route::Brotli,
            _ => return Err(format!("unknown route {v} in archive index")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Route::Store => "stored",
            Route::Cm => "context mixing",
            Route::Jpeg => "jpeg",
            Route::Audio => "audio",
            Route::Brotli => "brotli",
        }
    }
}

pub struct Options {
    pub threads: usize,
    pub table_bits: u32,
    pub chunk_bytes: usize,
    pub fast: bool,
    pub verify: bool,
}

impl Default for Options {
    /// The CLI defaults: up to 8 threads, 2^22 hash slots, 16 MB chunks, best mode.
    fn default() -> Self {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
        Options { threads, table_bits: 22, chunk_bytes: 16 << 20, fast: false, verify: false }
    }
}

struct Entry {
    path: String,
    /// 0 = regular file, 1 = symbolic link (its bytes are the link target).
    kind: u8,
    size: u64,
    crc: u32,
    group: u32,
    offset: u64,
}

struct Group {
    route: Route,
    raw_len: u64,
    payload: Vec<u8>,
}

/// Per-route totals for the summary line.
#[derive(Default, Clone)]
pub struct RouteStats {
    pub files: usize,
    pub raw: u64,
    pub packed: u64,
}

pub struct Report {
    pub stats: Vec<(Route, RouteStats)>,
    pub fallbacks: Vec<String>,
    pub skipped: Vec<String>,
    pub archive_bytes: u64,
    pub input_bytes: u64,
    pub seconds: f64,
}

/// Runs `f` over `0..n` on `threads` workers and returns results in order.
fn parallel_map<T: Send, F: Fn(usize) -> T + Sync>(n: usize, threads: usize, f: F) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads.max(1).min(n.max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = f(i);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|o| o.expect("every job ran")).collect()
}

/// Order-0 entropy of a sample, in bits per byte.
fn sample_entropy(data: &[u8]) -> f64 {
    let sample = &data[..data.len().min(1 << 20)];
    if sample.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in sample {
        counts[b as usize] += 1;
    }
    let n = sample.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| {
        let p = c as f64 / n;
        -p * p.log2()
    }).sum()
}

/// Collects regular files and symlinks under each input, with archive paths
/// rooted at the input's own name. Symlinks are stored, not followed; other
/// special files (sockets, devices) are skipped and reported.
pub fn collect(inputs: &[PathBuf], skipped: &mut Vec<String>) -> Result<Vec<(String, PathBuf, bool)>, String> {
    let mut files = Vec::new();
    for input in inputs {
        let meta = std::fs::symlink_metadata(input).map_err(|e| format!("{}: {e}", input.display()))?;
        let base = input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{}: cannot archive a path without a name", input.display()))?;
        if meta.is_file() || meta.file_type().is_symlink() {
            files.push((base, input.clone(), meta.file_type().is_symlink()));
        } else if meta.is_dir() {
            walk(input, &base, &mut files, skipped)?;
        } else {
            skipped.push(input.display().to_string());
        }
    }
    files.sort();
    Ok(files)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf, bool)>, skipped: &mut Vec<String>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for item in rd {
        let item = item.map_err(|e| format!("{}: {e}", dir.display()))?;
        let ft = item.file_type().map_err(|e| e.to_string())?;
        let name = item.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        if ft.is_dir() {
            walk(&item.path(), &rel, out, skipped)?;
        } else if ft.is_file() || ft.is_symlink() {
            out.push((rel, item.path(), ft.is_symlink()));
        } else {
            skipped.push(item.path().display().to_string());
        }
    }
    Ok(())
}

fn extension(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn encode_generic(data: &[u8], opts: &Options) -> (Route, Vec<u8>) {
    let (route, out) = if opts.fast {
        let mut out = Vec::new();
        let params = brotli::enc::BrotliEncoderParams { quality: 9, lgwin: 24, ..Default::default() };
        brotli::BrotliCompress(&mut &data[..], &mut out, &params).expect("in-memory brotli cannot fail");
        (Route::Brotli, out)
    } else {
        (Route::Cm, cm::compress(data, opts.table_bits))
    };
    if out.len() >= data.len() {
        (Route::Store, data.to_vec())
    } else {
        (route, out)
    }
}

fn decode_group(route: Route, payload: &[u8], raw_len: u64) -> Result<Vec<u8>, String> {
    let out = match route {
        Route::Store => payload.to_vec(),
        Route::Cm => cm::decompress(payload)?,
        Route::Jpeg => jpeg::decompress(payload)?,
        Route::Audio => audio::decompress(payload)?,
        Route::Brotli => {
            let mut out = Vec::new();
            brotli::BrotliDecompress(&mut &payload[..], &mut out).map_err(|e| format!("brotli: {e}"))?;
            out
        }
    };
    if out.len() as u64 != raw_len {
        return Err(format!("{} group decoded to {} bytes, expected {raw_len}", route.name(), out.len()));
    }
    Ok(out)
}

pub fn create(archive: &Path, inputs: &[PathBuf], opts: &Options) -> Result<Report, String> {
    let t0 = Instant::now();
    let mut skipped = Vec::new();
    let listing = collect(inputs, &mut skipped)?;
    let contents: Vec<Vec<u8>> = parallel_map(listing.len(), opts.threads, |i| {
        let (_, p, link) = &listing[i];
        if *link {
            std::fs::read_link(p).map(|t| t.to_string_lossy().replace('\\', "/").into_bytes())
        } else {
            std::fs::read(p)
        }
    })
    .into_iter()
    .zip(&listing)
    .map(|(r, (_, p, _))| r.map_err(|e| format!("{}: {e}", p.display())))
    .collect::<Result<_, _>>()?;
    let input_bytes: u64 = contents.iter().map(|c| c.len() as u64).sum();

    // Phase 1: specialised routes, one file per job.
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Generic,
        Jpeg,
        Audio,
        Incompressible,
    }
    let kinds: Vec<Kind> = contents
        .iter()
        .zip(&listing)
        .map(|(c, (_, _, link))| {
            if *link {
                Kind::Generic
            } else if jpeg::looks_like_jpeg(c) {
                Kind::Jpeg
            } else if c.starts_with(b"RIFF") && c.get(8..12) == Some(b"WAVE") {
                Kind::Audio
            } else if c.len() > 4096 && sample_entropy(c) > 7.95 {
                Kind::Incompressible
            } else {
                Kind::Generic
            }
        })
        .collect();
    let special: Vec<usize> = (0..contents.len()).filter(|&i| matches!(kinds[i], Kind::Jpeg | Kind::Audio)).collect();
    let special_out: Vec<Option<Vec<u8>>> = parallel_map(special.len(), opts.threads, |j| {
        let i = special[j];
        let data = &contents[i];
        let out = match kinds[i] {
            Kind::Jpeg => jpeg::compress(data),
            _ => audio::compress(data, opts.table_bits).filter(|o| o.len() < data.len()),
        }?;
        if kinds[i] == Kind::Audio && opts.verify && audio::decompress(&out).ok().as_deref() != Some(&data[..]) {
            return None;
        }
        Some(out)
    });

    let mut entries: Vec<Entry> = listing
        .iter()
        .zip(&contents)
        .map(|((path, _, link), c)| Entry { path: path.clone(), kind: *link as u8, size: c.len() as u64, crc: crc32fast::hash(c), group: SOLID, offset: 0 })
        .collect();
    let mut groups: Vec<Group> = Vec::new();
    let mut fallbacks = Vec::new();
    let mut generic: Vec<usize> = Vec::new();
    for (i, kind) in kinds.iter().enumerate() {
        match kind {
            Kind::Generic => generic.push(i),
            Kind::Incompressible => {
                entries[i].group = groups.len() as u32;
                groups.push(Group { route: Route::Store, raw_len: contents[i].len() as u64, payload: contents[i].clone() });
            }
            Kind::Jpeg | Kind::Audio => {}
        }
    }
    for (j, out) in special_out.into_iter().enumerate() {
        let i = special[j];
        match out {
            Some(payload) => {
                entries[i].group = groups.len() as u32;
                let route = if kinds[i] == Kind::Jpeg { Route::Jpeg } else { Route::Audio };
                groups.push(Group { route, raw_len: contents[i].len() as u64, payload });
            }
            None => {
                fallbacks.push(entries[i].path.clone());
                generic.push(i);
            }
        }
    }

    // Phase 2: the solid stream, similar files next to each other.
    generic.sort_by(|&a, &b| (extension(&entries[a].path), &entries[a].path).cmp(&(extension(&entries[b].path), &entries[b].path)));
    let mut solid = Vec::new();
    for &i in &generic {
        entries[i].group = SOLID;
        entries[i].offset = solid.len() as u64;
        solid.extend_from_slice(&contents[i]);
    }
    drop(contents);
    let chunk = opts.chunk_bytes.max(solid.len().div_ceil(opts.threads.max(1))).max(1 << 20);
    let pieces: Vec<&[u8]> = solid.chunks(chunk).collect();
    let coded: Vec<(Route, Vec<u8>)> = parallel_map(pieces.len(), opts.threads, |k| encode_generic(pieces[k], opts));
    if opts.verify {
        let bad = parallel_map(coded.len(), opts.threads, |k| {
            decode_group(coded[k].0, &coded[k].1, pieces[k].len() as u64).ok().as_deref() != Some(pieces[k])
        });
        if bad.iter().any(|&b| b) {
            return Err("verification failed: a generic chunk did not decode to its input; archive not written".into());
        }
    }
    let solid_groups_start = groups.len() as u32;
    for (k, (route, payload)) in coded.into_iter().enumerate() {
        groups.push(Group { route, raw_len: pieces[k].len() as u64, payload });
    }

    // Index.
    let mut idx = Vec::new();
    idx.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in &entries {
        let p = e.path.as_bytes();
        idx.extend_from_slice(&(p.len() as u16).to_le_bytes());
        idx.extend_from_slice(p);
        idx.push(e.kind);
        idx.extend_from_slice(&e.size.to_le_bytes());
        idx.extend_from_slice(&e.crc.to_le_bytes());
        idx.extend_from_slice(&e.group.to_le_bytes());
        idx.extend_from_slice(&e.offset.to_le_bytes());
    }
    idx.extend_from_slice(&(groups.len() as u32).to_le_bytes());
    idx.extend_from_slice(&solid_groups_start.to_le_bytes());
    for g in &groups {
        idx.push(g.route as u8);
        idx.extend_from_slice(&g.raw_len.to_le_bytes());
        idx.extend_from_slice(&(g.payload.len() as u64).to_le_bytes());
    }
    let idx = cm::compress(&idx, 16);

    let tmp = archive.with_extension("pcraft.partial");
    {
        use std::io::Write;
        let f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut w = std::io::BufWriter::new(f);
        let mut write = |b: &[u8]| w.write_all(b).map_err(|e| format!("{}: {e}", tmp.display()));
        write(MAGIC)?;
        write(&(idx.len() as u64).to_le_bytes())?;
        write(&idx)?;
        for g in &groups {
            write(&g.payload)?;
        }
        w.flush().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, archive).map_err(|e| format!("{}: {e}", archive.display()))?;

    // Summary per route. Solid files are attributed to their chunk's route by share.
    let mut stats: Vec<(Route, RouteStats)> = Vec::new();
    let mut add = |route: Route, files: usize, raw: u64, packed: u64| {
        if let Some(s) = stats.iter_mut().find(|(r, _)| *r == route) {
            s.1.files += files;
            s.1.raw += raw;
            s.1.packed += packed;
        } else {
            stats.push((route, RouteStats { files, raw, packed }));
        }
    };
    for g in &groups[..solid_groups_start as usize] {
        add(g.route, 1, g.raw_len, g.payload.len() as u64);
    }
    for (k, g) in groups[solid_groups_start as usize..].iter().enumerate() {
        add(g.route, if k == 0 { generic.len() } else { 0 }, g.raw_len, g.payload.len() as u64);
    }
    let archive_bytes = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
    Ok(Report { stats, fallbacks, skipped, archive_bytes, input_bytes, seconds: t0.elapsed().as_secs_f64() })
}

struct Parsed {
    entries: Vec<Entry>,
    groups: Vec<(Route, u64, std::ops::Range<usize>)>,
    solid_start: usize,
}

fn parse(data: &[u8]) -> Result<Parsed, String> {
    let bad = |what: &str| format!("archive index is damaged ({what})");
    if data.len() < 16 || &data[..8] != MAGIC {
        return Err("not a PackCraft archive".into());
    }
    let idx_len = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;
    let idx_end = 16usize.checked_add(idx_len).filter(|&e| e <= data.len()).ok_or_else(|| bad("length"))?;
    let idx = cm::decompress(&data[16..idx_end])?;
    let mut r = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let s = idx.get(r..r + n).ok_or_else(|| bad("truncated"))?;
        r += n;
        Ok(s)
    };
    let n = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let mut entries = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let pl = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
        let path = String::from_utf8(take(pl)?.to_vec()).map_err(|_| bad("path encoding"))?;
        let kind = take(1)?[0];
        if kind > 1 {
            return Err(bad("entry kind"));
        }
        let size = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let crc = u32::from_le_bytes(take(4)?.try_into().unwrap());
        let group = u32::from_le_bytes(take(4)?.try_into().unwrap());
        let offset = u64::from_le_bytes(take(8)?.try_into().unwrap());
        entries.push(Entry { path, kind, size, crc, group, offset });
    }
    let ng = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let solid_start = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let mut groups = Vec::with_capacity(ng.min(1 << 20));
    let mut pos = idx_end;
    for _ in 0..ng {
        let route = Route::from_u8(take(1)?[0])?;
        let raw = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let len = u64::from_le_bytes(take(8)?.try_into().unwrap()) as usize;
        let end = pos.checked_add(len).filter(|&e| e <= data.len()).ok_or_else(|| bad("group length"))?;
        groups.push((route, raw, pos..end));
        pos = end;
    }
    if solid_start > groups.len() {
        return Err(bad("solid start"));
    }
    Ok(Parsed { entries, groups, solid_start })
}

/// Refuses absolute paths and `..`, so an archive cannot write outside `dest`.
pub(crate) fn safe_join(dest: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if rel.is_empty() || rel.contains('\\') || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("refusing unsafe path in archive: {rel:?}"));
    }
    Ok(dest.join(p))
}

pub struct ListItem {
    pub path: String,
    pub size: u64,
    pub route: Route,
}

pub fn list(archive: &Path) -> Result<Vec<ListItem>, String> {
    let data = crate::volumes::read_all(archive)?;
    let p = parse(&data)?;
    Ok(p.entries
        .iter()
        .map(|e| {
            let route = if e.group == SOLID {
                p.groups.get(p.solid_start).map(|g| g.0).unwrap_or(Route::Store)
            } else {
                p.groups.get(e.group as usize).map(|g| g.0).unwrap_or(Route::Store)
            };
            ListItem { path: e.path.clone(), size: e.size, route }
        })
        .collect())
}

/// Decodes everything and checks every CRC. With `dest`, also writes the files.
pub fn extract(archive: &Path, dest: Option<&Path>, threads: usize) -> Result<(usize, u64), String> {
    extract_filtered(archive, dest, threads, &|_| true)
}

/// Like `extract`, but writes only the entries whose archive path `keep`
/// accepts. Every entry is still decoded and CRC-checked (solid groups have
/// to be decoded whole anyway). Returns the count and bytes written.
pub fn extract_filtered(archive: &Path, dest: Option<&Path>, threads: usize, keep: &dyn Fn(&str) -> bool) -> Result<(usize, u64), String> {
    let data = crate::volumes::read_all(archive)?;
    let p = parse(&data)?;
    if let Some(d) = dest {
        for e in &p.entries {
            safe_join(d, &e.path)?;
        }
    }
    let decoded: Vec<Result<Vec<u8>, String>> = parallel_map(p.groups.len(), threads, |k| {
        let (route, raw, range) = &p.groups[k];
        decode_group(*route, &data[range.clone()], *raw)
    });
    let mut decoded: Vec<Vec<u8>> = decoded.into_iter().collect::<Result<_, _>>()?;
    let mut solid = Vec::new();
    for g in decoded.iter_mut().skip(p.solid_start) {
        solid.append(g);
    }
    let mut total = 0u64;
    let mut written = 0usize;
    let mut links: Vec<(PathBuf, String)> = Vec::new();
    for e in &p.entries {
        let bytes: &[u8] = if e.group == SOLID {
            let start = e.offset as usize;
            solid.get(start..start + e.size as usize).ok_or_else(|| format!("{}: outside the solid stream", e.path))?
        } else {
            let g = decoded.get(e.group as usize).ok_or_else(|| format!("{}: missing group", e.path))?;
            if g.len() as u64 != e.size {
                return Err(format!("{}: size mismatch", e.path));
            }
            g
        };
        if crc32fast::hash(bytes) != e.crc {
            return Err(format!("{}: CRC mismatch, archive is damaged", e.path));
        }
        if !keep(&e.path) {
            continue;
        }
        written += 1;
        if let (Some(d), 1) = (dest, e.kind) {
            let target = String::from_utf8(bytes.to_vec()).map_err(|_| format!("{}: link target is not UTF-8", e.path))?;
            check_link_target(&e.path, &target)?;
            links.push((safe_join(d, &e.path)?, target));
        } else if let Some(d) = dest {
            let out = safe_join(d, &e.path)?;
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            std::fs::write(&out, bytes).map_err(|e| format!("{}: {e}", out.display()))?;
        }
        total += e.size;
    }
    // Links are created after every file, so no file write can pass through one.
    for (at, target) in links {
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        make_symlink(&target, &at)?;
    }
    Ok((written, total))
}

/// A link target must be relative and must stay inside the extracted tree.
pub(crate) fn check_link_target(entry: &str, target: &str) -> Result<(), String> {
    let refuse = || Err(format!("refusing link {entry:?} -> {target:?}: it points outside the archive"));
    if target.is_empty() || target.starts_with('/') || target.contains('\\') || target.contains(':') {
        return refuse();
    }
    let mut depth: i64 = entry.matches('/').count() as i64;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return refuse();
                }
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn make_symlink(target: &str, at: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, at).map_err(|e| format!("{}: {e}", at.display()))
}

#[cfg(windows)]
pub(crate) fn make_symlink(target: &str, at: &Path) -> Result<(), String> {
    std::os::windows::fs::symlink_file(target.replace('/', "\\"), at)
        .map_err(|e| format!("{}: cannot create symlink ({e}); Windows needs Developer Mode or admin rights", at.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options { threads: 2, table_bits: 16, chunk_bytes: 1 << 20, fast: false, verify: true }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("packcraft-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn mixed_folder_round_trips_byte_for_byte() {
        let root = scratch("mixed");
        let src = root.join("proj");
        std::fs::create_dir_all(src.join("sub/deeper")).unwrap();
        std::fs::write(src.join("notes.txt"), "hello world\n".repeat(500)).unwrap();
        std::fs::write(src.join("sub/data.csv"), (0..2000).map(|i| format!("{i},{}\n", i * 3)).collect::<String>()).unwrap();
        std::fs::write(src.join("sub/deeper/empty.bin"), b"").unwrap();
        std::fs::write(src.join("sub/song.wav"), crate::audio::tests::synth_wav(2, 16, 20000)).unwrap();
        let mut x = 7u32;
        let noise: Vec<u8> = (0..100_000).map(|_| { x ^= x << 13; x ^= x >> 17; x ^= x << 5; x as u8 }).collect();
        std::fs::write(src.join("noise.bin"), &noise).unwrap();

        for fast in [false, true] {
            let arc = root.join(format!("out-{fast}.pcraft"));
            let rep = create(&arc, &[src.clone()], &Options { fast, ..opts() }).unwrap();
            assert!(rep.archive_bytes < rep.input_bytes);
            assert!(rep.stats.iter().any(|(r, _)| *r == Route::Audio), "wav took the audio route");
            let dest = root.join(format!("x-{fast}"));
            let (n, _) = extract(&arc, Some(&dest), 2).unwrap();
            assert_eq!(n, 5);
            for rel in ["notes.txt", "sub/data.csv", "sub/deeper/empty.bin", "sub/song.wav", "noise.bin"] {
                assert_eq!(std::fs::read(src.join(rel)).unwrap(), std::fs::read(dest.join("proj").join(rel)).unwrap(), "{rel}");
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupted_archive_fails_loudly() {
        let root = scratch("corrupt");
        let f = root.join("a.txt");
        std::fs::write(&f, "some text that compresses ".repeat(2000)).unwrap();
        let arc = root.join("a.pcraft");
        create(&arc, &[f], &opts()).unwrap();
        let mut bytes = std::fs::read(&arc).unwrap();
        let last = bytes.len() - 20;
        bytes[last] ^= 0x55;
        std::fs::write(&arc, &bytes).unwrap();
        assert!(extract(&arc, None, 1).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let d = Path::new("/tmp/out");
        assert!(safe_join(d, "../etc/passwd").is_err());
        assert!(safe_join(d, "/etc/passwd").is_err());
        assert!(safe_join(d, "a/../../b").is_err());
        assert!(safe_join(d, "a\\..\\b").is_err());
        assert!(safe_join(d, "a/b/c.txt").is_ok());
    }

    #[test]
    fn link_targets_must_stay_inside() {
        assert!(check_link_target("proj/HomebrewFormula", "pkg/brew").is_ok());
        assert!(check_link_target("proj/a/b", "../c").is_ok());
        assert!(check_link_target("proj/a", "../../etc/passwd").is_err());
        assert!(check_link_target("proj/a", "/etc/passwd").is_err());
        assert!(check_link_target("proj/a", "C:/Windows").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_round_trip_as_links() {
        let root = scratch("links");
        let src = root.join("p");
        std::fs::create_dir_all(src.join("pkg")).unwrap();
        std::fs::write(src.join("pkg/real.txt"), "content").unwrap();
        std::os::unix::fs::symlink("pkg/real.txt", src.join("alias")).unwrap();
        let arc = root.join("l.pcraft");
        create(&arc, &[src], &opts()).unwrap();
        let dest = root.join("x");
        extract(&arc, Some(&dest), 1).unwrap();
        let link = dest.join("p/alias");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("pkg/real.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
