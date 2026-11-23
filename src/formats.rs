//! One front door for every archive format PackCraft reads or writes.
//!
//! - Create: `.pcraft`, `.zip` (Deflate) and `.7z` (LZMA2, solid), each
//!   optionally split into parts (`.001`, `.002`, ... for pcraft and 7z; the
//!   zip specification's own `.z01`, `.z02`, ... `.zip` for zip).
//! - Open, test and extract: the same, plus RAR (see `rar.rs`), from any part
//!   of a multi-part set.
//!
//! Every entry path is checked with `archive::safe_join` before anything is
//! written, so no archive can write outside the destination folder, and links
//! are created only after every file and only when they stay inside it.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::archive::{self, check_link_target, make_symlink, safe_join};
use crate::volumes::{self, MultiReader, Scheme};
use crate::zipspan;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Pcraft,
    Zip,
    SevenZ,
    Rar,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Pcraft => "PackCraft",
            Format::Zip => "ZIP",
            Format::SevenZ => "7z",
            Format::Rar => "RAR",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Pcraft => "pcraft",
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::Rar => "rar",
        }
    }

    /// The formats PackCraft can write. RAR is read-only: the format is
    /// proprietary and only its owner's tools may create it.
    pub const WRITABLE: [Format; 3] = [Format::Pcraft, Format::Zip, Format::SevenZ];

    /// The format an output path asks for, from its extension.
    pub fn for_output(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        Format::WRITABLE.into_iter().find(|f| f.extension() == ext)
    }
}

/// Errors that mean "ask for a password and try again" start with this.
pub const NEED_PASSWORD: &str = "password needed";
pub const WRONG_PASSWORD: &str = "wrong password";

pub fn needs_password(err: &str) -> bool {
    err.starts_with(NEED_PASSWORD) || err.starts_with(WRONG_PASSWORD)
}

/// True for any file PackCraft can open: by extension, including every kind
/// of part name (`.001`, `.part2.rar`, `.r00`, `.z01`).
pub fn is_archive(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let ext = name.rsplit('.').next().unwrap_or("");
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    matches!(ext, "pcraft" | "zip" | "7z" | "rar")
        || (ext.len() >= 3 && digits(ext))
        || (ext.len() == 3 && matches!(ext.as_bytes()[0], b'r'..=b'z') && digits(&ext[1..]))
}

/// Finds the parts of the set `path` belongs to and the format inside.
pub fn detect(path: &Path) -> Result<(Format, volumes::VolumeSet), String> {
    let set = volumes::discover(path).map_err(|e| e.to_string())?;
    let mut head = [0u8; 8];
    let n = File::open(&set.parts[0]).and_then(|mut f| f.read(&mut head)).map_err(|e| format!("{}: {e}", set.parts[0].display()))?;
    let h = &head[..n];
    let format = if h.starts_with(b"PKCRAFT1") {
        Format::Pcraft
    } else if h.starts_with(b"7z\xBC\xAF\x27\x1C") {
        Format::SevenZ
    } else if h.starts_with(b"Rar!\x1a\x07") {
        Format::Rar
    } else if h.starts_with(b"PK\x03\x04") || h.starts_with(b"PK\x05\x06") || h.starts_with(b"PK\x07\x08") || h.starts_with(b"PK00") {
        Format::Zip
    } else {
        return Err(format!("{}: not an archive PackCraft can read", path.display()));
    };
    Ok((format, set))
}

/// One entry, whatever the format.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: String,
    pub size: u64,
    pub packed: Option<u64>,
    pub method: String,
    pub is_dir: bool,
    pub encrypted: bool,
}

/// Archive names use `/`; some Windows tools write `\`. Directory entries end
/// in `/`, which is trimmed.
fn clean_name(raw: &str) -> String {
    raw.replace('\\', "/").trim_end_matches('/').to_string()
}

/// An open zip: a spanned set is first joined into one temporary zip.
fn open_zip(set: &volumes::VolumeSet) -> Result<(zip::ZipArchive<Box<dyn ReadSeek>>, Option<zipspan::TempFile>), String> {
    let (reader, tmp): (Box<dyn ReadSeek>, _) = if set.scheme == Scheme::ZipSpanned {
        let tmp = zipspan::join(&set.parts)?;
        (Box::new(io::BufReader::new(File::open(&tmp.0).map_err(|e| e.to_string())?)), Some(tmp))
    } else {
        (Box::new(io::BufReader::new(MultiReader::open(&set.parts).map_err(|e| e.to_string())?)), None)
    };
    let z = zip::ZipArchive::new(reader).map_err(|e| format!("zip: {e}"))?;
    Ok((z, tmp))
}

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

fn open_7z(set: &volumes::VolumeSet, password: Option<&str>) -> Result<sevenz_rust2::ArchiveReader<io::BufReader<MultiReader>>, String> {
    let r = io::BufReader::new(MultiReader::open(&set.parts).map_err(|e| e.to_string())?);
    let pw = password.map(sevenz_rust2::Password::from).unwrap_or_else(sevenz_rust2::Password::empty);
    sevenz_rust2::ArchiveReader::new(r, pw).map_err(|e| sevenz_error(e, password.is_some()))
}

fn sevenz_error(e: sevenz_rust2::Error, had_password: bool) -> String {
    match e {
        sevenz_rust2::Error::PasswordRequired => NEED_PASSWORD.to_string(),
        sevenz_rust2::Error::MaybeBadPassword(_) if had_password => WRONG_PASSWORD.to_string(),
        sevenz_rust2::Error::MaybeBadPassword(_) => NEED_PASSWORD.to_string(),
        e => format!("7z: {e}"),
    }
}

pub fn list(path: &Path, password: Option<&str>) -> Result<Vec<Item>, String> {
    let (format, set) = detect(path)?;
    match format {
        Format::Pcraft => Ok(archive::list(path)?
            .into_iter()
            .map(|i| Item { path: i.path, size: i.size, packed: None, method: i.route.name().to_string(), is_dir: false, encrypted: false })
            .collect()),
        Format::Zip => {
            let (mut z, _tmp) = open_zip(&set)?;
            let mut out = Vec::with_capacity(z.len());
            for i in 0..z.len() {
                let f = z.by_index_raw(i).map_err(|e| format!("zip: {e}"))?;
                out.push(Item {
                    path: clean_name(&f.name().map_err(|e| format!("zip: {e}"))?),
                    size: f.size(),
                    packed: Some(f.compressed_size()),
                    method: format!("{}", f.compression()),
                    is_dir: f.is_dir(),
                    encrypted: f.encrypted(),
                });
            }
            Ok(out)
        }
        Format::SevenZ => {
            let r = open_7z(&set, password)?;
            let a = r.archive();
            Ok(a.files
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.is_anti_item)
                .map(|(i, e)| Item {
                    path: clean_name(&e.name),
                    size: e.size,
                    packed: None,
                    method: if e.has_stream { "LZMA2/7z".to_string() } else { String::new() },
                    is_dir: e.is_directory,
                    encrypted: a.is_file_encrypted(i),
                })
                .collect())
        }
        Format::Rar => crate::rar::list(&set.parts, password),
    }
}

/// Decodes every entry and checks it. With `dest`, also writes the entries
/// `keep` accepts. Returns the count and bytes written (or tested).
pub fn extract(path: &Path, dest: Option<&Path>, threads: usize, password: Option<&str>, keep: &dyn Fn(&str) -> bool) -> Result<(usize, u64), String> {
    let (format, set) = detect(path)?;
    match format {
        Format::Pcraft => archive::extract_filtered(path, dest, threads, keep),
        Format::Zip => extract_zip(&set, dest, password, keep),
        Format::SevenZ => extract_7z(&set, dest, threads, password, keep),
        Format::Rar => crate::rar::extract(&set.parts, dest, password, keep),
    }
}

/// Collects what an extraction must do after the file writes.
#[derive(Default)]
pub(crate) struct Sink {
    links: Vec<(PathBuf, String)>,
    pub(crate) count: usize,
    pub(crate) bytes: u64,
}

impl Sink {
    /// Where an entry goes, or None when testing or when `keep` refuses it.
    pub(crate) fn target(dest: Option<&Path>, name: &str, keep: &dyn Fn(&str) -> bool) -> Result<Option<PathBuf>, String> {
        let Some(d) = dest else { return Ok(None) };
        let out = safe_join(d, name)?;
        Ok(keep(name).then_some(out))
    }

    pub(crate) fn dir(at: &Path) -> Result<(), String> {
        std::fs::create_dir_all(at).map_err(|e| format!("{}: {e}", at.display()))
    }

    /// Streams `r` into `at` (or into nowhere when `at` is None).
    pub(crate) fn file(&mut self, at: Option<&Path>, r: &mut dyn Read, mtime: Option<SystemTime>) -> Result<(), String> {
        let n = match at {
            Some(at) => {
                if let Some(p) = at.parent() {
                    Self::dir(p)?;
                }
                let mut f = File::create(at).map_err(|e| format!("{}: {e}", at.display()))?;
                let n = io::copy(r, &mut f).map_err(|e| format!("{}: {e}", at.display()))?;
                if let Some(t) = mtime {
                    let _ = f.set_modified(t);
                }
                n
            }
            None => io::copy(r, &mut io::sink()).map_err(|e| e.to_string())?,
        };
        self.count += 1;
        self.bytes += n;
        Ok(())
    }

    /// Like `file`, for decoders that push their output: `fill` writes the
    /// entry into the writer it is given and returns how many bytes it wrote.
    pub(crate) fn write_with(&mut self, at: Option<&Path>, mtime: Option<SystemTime>, fill: &mut dyn FnMut(&mut dyn Write) -> Result<u64, String>) -> Result<(), String> {
        let n = match at {
            Some(at) => {
                if let Some(p) = at.parent() {
                    Self::dir(p)?;
                }
                let f = File::create(at).map_err(|e| format!("{}: {e}", at.display()))?;
                let mut w = io::BufWriter::with_capacity(1 << 20, f);
                let n = fill(&mut w)?;
                let f = w.into_inner().map_err(|e| format!("{}: {e}", at.display()))?;
                if let Some(t) = mtime {
                    let _ = f.set_modified(t);
                }
                n
            }
            None => fill(&mut io::sink())?,
        };
        self.count += 1;
        self.bytes += n;
        Ok(())
    }

    pub(crate) fn link(&mut self, name: &str, at: Option<PathBuf>, target: String) -> Result<(), String> {
        check_link_target(name, &target)?;
        if let Some(at) = at {
            self.links.push((at, target));
        }
        self.count += 1;
        Ok(())
    }

    /// Links last, so no file write can pass through one.
    pub(crate) fn finish(self) -> Result<(usize, u64), String> {
        for (at, target) in &self.links {
            if let Some(p) = at.parent() {
                Self::dir(p)?;
            }
            make_symlink(target, at)?;
        }
        Ok((self.count, self.bytes))
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// public-domain `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Zip times carry no zone; they are read and written as UTC.
fn zip_time_to_system(t: zip::DateTime) -> Option<SystemTime> {
    let days = days_from_civil(t.year() as i64, t.month() as i64, t.day() as i64);
    let secs = days * 86_400 + t.hour() as i64 * 3600 + t.minute() as i64 * 60 + t.second() as i64;
    (secs >= 0).then(|| UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn system_to_zip_time(t: SystemTime) -> Option<zip::DateTime> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    zip::DateTime::from_date_and_time(y as u16, m as u8, d as u8, (s / 3600) as u8, (s / 60 % 60) as u8, (s % 60) as u8).ok()
}

fn extract_zip(set: &volumes::VolumeSet, dest: Option<&Path>, password: Option<&str>, keep: &dyn Fn(&str) -> bool) -> Result<(usize, u64), String> {
    let (mut z, _tmp) = open_zip(set)?;
    let mut sink = Sink::default();
    // Every path is checked before the first write.
    let mut names = Vec::with_capacity(z.len());
    for i in 0..z.len() {
        let f = z.by_index_raw(i).map_err(|e| format!("zip: {e}"))?;
        let name = clean_name(&f.name().map_err(|e| format!("zip: {e}"))?);
        if let Some(d) = dest {
            safe_join(d, &name)?;
        }
        names.push(name);
    }
    for (i, name) in names.iter().enumerate() {
        let encrypted = z.by_index_raw(i).map_err(|e| format!("zip: {e}"))?.encrypted();
        let mut f = match (encrypted, password) {
            (true, None) => return Err(format!("{NEED_PASSWORD}: {name} is encrypted")),
            (true, Some(pw)) => z.by_index_decrypt(i, pw.as_bytes()).map_err(|e| match e {
                zip::result::ZipError::InvalidPassword => WRONG_PASSWORD.to_string(),
                e => format!("{name}: {e}"),
            })?,
            (false, _) => z.by_index(i).map_err(|e| format!("{name}: {e}"))?,
        };
        let at = Sink::target(dest, name, keep)?;
        if f.is_dir() {
            if let Some(at) = at {
                Sink::dir(&at)?;
            }
        } else if f.is_symlink() {
            let mut target = String::new();
            f.read_to_string(&mut target).map_err(|e| format!("{name}: link target: {e}"))?;
            sink.link(name, at, target)?;
        } else {
            let mtime = f.last_modified().and_then(zip_time_to_system);
            let mode = f.unix_mode();
            sink.file(at.as_deref(), &mut f, mtime).map_err(|e| format!("{name}: {e}"))?;
            #[cfg(unix)]
            if let (Some(at), Some(mode)) = (at.as_deref(), mode) {
                use std::os::unix::fs::PermissionsExt;
                if mode & 0o111 != 0 {
                    let _ = std::fs::set_permissions(at, std::fs::Permissions::from_mode(mode & 0o755));
                }
            }
            #[cfg(not(unix))]
            let _ = mode;
        }
    }
    sink.finish()
}

/// 7-Zip's convention for a Unix entry: bit 15 of the attributes is set and
/// the high 16 bits hold `st_mode`.
const UNIX_EXT: u32 = 0x8000;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

fn extract_7z(set: &volumes::VolumeSet, dest: Option<&Path>, threads: usize, password: Option<&str>, keep: &dyn Fn(&str) -> bool) -> Result<(usize, u64), String> {
    let mut r = open_7z(set, password)?;
    r.set_thread_count(threads.clamp(1, 64) as u32);
    if let Some(d) = dest {
        for e in &r.archive().files {
            safe_join(d, &clean_name(&e.name))?;
        }
    }
    let mut sink = Sink::default();
    let mut failure: Option<String> = None;
    let had_pw = password.is_some();
    let res = r.for_each_entries(|e, reader| {
        if e.is_anti_item {
            return Ok(true);
        }
        let name = clean_name(&e.name);
        let step = (|| -> Result<(), String> {
            let at = Sink::target(dest, &name, keep)?;
            if e.is_directory {
                if let Some(at) = at {
                    Sink::dir(&at)?;
                }
                return Ok(());
            }
            let attrs = e.windows_attributes;
            if e.has_windows_attributes && attrs & UNIX_EXT != 0 && (attrs >> 16) & S_IFMT == S_IFLNK {
                let mut target = String::new();
                reader.read_to_string(&mut target).map_err(|e| format!("{name}: link target: {e}"))?;
                return sink.link(&name, at, target);
            }
            let mtime = e.has_last_modified_date.then(|| SystemTime::from(e.last_modified_date));
            sink.file(at.as_deref(), reader, mtime).map_err(|e| format!("{name}: {e}"))?;
            #[cfg(unix)]
            if let Some(at) = at.as_deref() {
                let mode = attrs >> 16;
                if e.has_windows_attributes && attrs & UNIX_EXT != 0 && mode & 0o111 != 0 {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(at, std::fs::Permissions::from_mode(mode & 0o755));
                }
            }
            Ok(())
        })();
        match step {
            Ok(()) => Ok(true),
            Err(msg) => {
                failure = Some(msg);
                Ok(false)
            }
        }
    });
    if let Some(msg) = failure {
        return Err(msg);
    }
    res.map_err(|e| sevenz_error(e, had_pw))?;
    sink.finish()
}

pub struct CreateOptions {
    pub format: Format,
    /// Used by the pcraft format; `threads` is shared by all formats.
    pub pcraft: archive::Options,
    /// 0..=9 for zip (Deflate) and 7z (LZMA2).
    pub level: u32,
    /// Split into parts of at most this many bytes.
    pub volume: Option<u64>,
    /// zip: AES-256; 7z: AES-256 with encrypted file names. pcraft has none.
    pub password: Option<String>,
}

pub struct CreateReport {
    pub files: usize,
    pub input_bytes: u64,
    pub archive_bytes: u64,
    pub parts: Vec<PathBuf>,
    pub skipped: Vec<String>,
    pub seconds: f64,
    /// The pcraft per-route report, for pcraft archives.
    pub pcraft: Option<archive::Report>,
}

/// Creates `out` (or, with a volume size, its parts). Refuses to overwrite.
pub fn create(out: &Path, inputs: &[PathBuf], opts: &CreateOptions) -> Result<CreateReport, String> {
    let t0 = Instant::now();
    if opts.format == Format::Rar {
        return Err("PackCraft cannot create RAR archives: the format is proprietary. Choose ZIP or 7z.".into());
    }
    if opts.password.is_some() && opts.format == Format::Pcraft {
        return Err("PackCraft archives have no encryption yet: choose ZIP or 7z for a password".into());
    }
    if out.exists() {
        return Err(format!("{} already exists", out.display()));
    }
    let first_part = match opts.format {
        Format::Zip => out.with_extension("z01"),
        _ => volumes::numbered_part(out, 1),
    };
    if opts.volume.is_some() && first_part.exists() {
        return Err(format!("{} already exists", first_part.display()));
    }
    let work = match opts.volume {
        Some(_) => out.with_file_name(format!("{}.partial", out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
        None => out.to_path_buf(),
    };
    let built = (|| -> Result<(usize, u64, Vec<String>, Option<archive::Report>), String> {
        match opts.format {
            Format::Pcraft => {
                let r = archive::create(&work, inputs, &opts.pcraft)?;
                let files = r.stats.iter().map(|(_, s)| s.files).sum();
                Ok((files, r.input_bytes, r.skipped.clone(), Some(r)))
            }
            Format::Zip => create_zip(&work, inputs, opts).map(|(n, b, s)| (n, b, s, None)),
            Format::SevenZ => create_7z(&work, inputs, opts).map(|(n, b, s)| (n, b, s, None)),
            Format::Rar => unreachable!(),
        }
    })();
    let (files, input_bytes, skipped, pcraft) = match built {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&work);
            return Err(e);
        }
    };
    let parts = match opts.volume {
        None => vec![out.to_path_buf()],
        Some(size) => {
            let r = match opts.format {
                Format::Zip => zipspan::split(&work, out, size),
                _ => volumes::split_numbered(&work, out, size),
            };
            if r.is_err() {
                let _ = std::fs::remove_file(&work);
            }
            r?
        }
    };
    let archive_bytes = parts.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    Ok(CreateReport { files, input_bytes, archive_bytes, parts, skipped, seconds: t0.elapsed().as_secs_f64(), pcraft })
}

fn create_zip(out: &Path, inputs: &[PathBuf], opts: &CreateOptions) -> Result<(usize, u64, Vec<String>), String> {
    use zip::write::SimpleFileOptions;
    let mut skipped = Vec::new();
    let listing = archive::collect(inputs, &mut skipped)?;
    let file = File::create_new(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut w = zip::ZipWriter::new(io::BufWriter::new(file));
    let mut total = 0u64;
    let base = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(opts.level.min(9) as i64));
    for (name, path, is_link) in &listing {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut fo = base.large_file(meta.len() >= 0xFFFF_FFFF);
        if let Some(t) = meta.modified().ok().and_then(system_to_zip_time) {
            fo = fo.last_modified_time(t);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fo = fo.unix_permissions(meta.permissions().mode() & 0o777);
        }
        if let Some(pw) = opts.password.as_deref() {
            fo = fo.with_aes_encryption(zip::AesMode::Aes256, pw);
        }
        let zerr = |e: zip::result::ZipError| format!("{name}: {e}");
        if *is_link {
            let target = std::fs::read_link(path).map_err(|e| format!("{}: {e}", path.display()))?;
            w.add_symlink(name, target.to_string_lossy().replace('\\', "/"), fo).map_err(zerr)?;
        } else {
            w.start_file(name, fo).map_err(zerr)?;
            let mut src = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            total += io::copy(&mut src, &mut w).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    let mut inner = w.finish().map_err(|e| format!("zip: {e}"))?;
    inner.flush().map_err(|e| e.to_string())?;
    Ok((listing.len(), total, skipped))
}

fn create_7z(out: &Path, inputs: &[PathBuf], opts: &CreateOptions) -> Result<(usize, u64, Vec<String>), String> {
    use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, NtTime, SourceReader};
    let mut skipped = Vec::new();
    let listing = archive::collect(inputs, &mut skipped)?;
    let file = File::create_new(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let serr = |e: sevenz_rust2::Error| format!("7z: {e}");
    let mut w = ArchiveWriter::new(io::BufWriter::new(file)).map_err(serr)?;
    let threads = opts.pcraft.threads.clamp(1, 64) as u32;
    let lzma: sevenz_rust2::EncoderConfiguration = if threads > 1 {
        // Chunks of 64 MB (at least the dictionary) keep most of the solid gain.
        Lzma2Options::from_level_mt(opts.level.min(9), threads, 64 << 20).into()
    } else {
        Lzma2Options::from_level(opts.level.min(9)).into()
    };
    let mut methods = Vec::new();
    if let Some(pw) = opts.password.as_deref() {
        methods.push(AesEncoderOptions::new(pw.into()).into());
        w.set_encrypt_header(true);
    }
    methods.push(lzma);
    w.set_content_methods(methods);

    let mut solid_entries = Vec::new();
    let mut solid_readers: Vec<SourceReader<Box<dyn Read>>> = Vec::new();
    let mut total = 0u64;
    for (name, path, is_link) in &listing {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut e = ArchiveEntry::new_file(name);
        if let Some(t) = meta.modified().ok().and_then(|t| NtTime::try_from(t).ok()) {
            e.has_last_modified_date = true;
            e.last_modified_date = t;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if *is_link { S_IFLNK | 0o777 } else { 0o100000 | (meta.permissions().mode() & 0o777) };
            e.has_windows_attributes = true;
            e.windows_attributes = UNIX_EXT | (mode << 16) | 0x20;
        }
        let reader: Box<dyn Read> = if *is_link {
            let target = std::fs::read_link(path).map_err(|e| format!("{}: {e}", path.display()))?;
            #[cfg(not(unix))]
            {
                e.has_windows_attributes = true;
                e.windows_attributes = UNIX_EXT | ((S_IFLNK | 0o777) << 16) | 0x20;
            }
            Box::new(io::Cursor::new(target.to_string_lossy().replace('\\', "/").into_bytes()))
        } else {
            if meta.len() == 0 {
                e.has_stream = false;
                w.push_archive_entry::<File>(e, None).map_err(serr)?;
                continue;
            }
            total += meta.len();
            Box::new(File::open(path).map_err(|e| format!("{}: {e}", path.display()))?)
        };
        solid_entries.push(e);
        solid_readers.push(SourceReader::new(reader));
    }
    if !solid_entries.is_empty() {
        w.push_archive_entries(solid_entries, solid_readers).map_err(serr)?;
    }
    let mut inner = w.finish().map_err(|e| format!("7z: {e}"))?;
    inner.flush().map_err(|e| e.to_string())?;
    Ok((listing.len(), total, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("packcraft-fmt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample(root: &Path) -> PathBuf {
        let src = root.join("proj");
        std::fs::create_dir_all(src.join("sub/deeper")).unwrap();
        std::fs::write(src.join("notes.txt"), "hello world\n".repeat(5000)).unwrap();
        std::fs::write(src.join("empty.txt"), b"").unwrap();
        let mut noise = Vec::with_capacity(300_000);
        let mut x: u32 = 1;
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            noise.push(x as u8);
        }
        std::fs::write(src.join("sub/deeper/noise.bin"), &noise).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../notes.txt", src.join("sub/link.txt")).unwrap();
        src
    }

    fn opts(format: Format, volume: Option<u64>, password: Option<&str>) -> CreateOptions {
        CreateOptions {
            format,
            pcraft: archive::Options { threads: 2, table_bits: 16, chunk_bytes: 1 << 20, fast: true, verify: false },
            level: 6,
            volume,
            password: password.map(str::to_string),
        }
    }

    fn same_tree(a: &Path, b: &Path) {
        for rel in ["notes.txt", "empty.txt", "sub/deeper/noise.bin"] {
            assert_eq!(std::fs::read(a.join(rel)).unwrap(), std::fs::read(b.join(rel)).unwrap(), "{rel}");
        }
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(b.join("sub/link.txt")).unwrap(), Path::new("../notes.txt"));
    }

    fn round_trip(tag: &str, format: Format, volume: Option<u64>, password: Option<&str>) {
        let root = scratch(tag);
        let src = sample(&root);
        let out = root.join(format!("out.{}", format.extension()));
        let rep = create(&out, std::slice::from_ref(&src), &opts(format, volume, password)).unwrap();
        if volume.is_some() {
            assert!(rep.parts.len() > 1, "{tag}: expected several parts, got {:?}", rep.parts);
            assert!(!root.join(format!("out.{}.partial", format.extension())).exists());
        }
        // Open from the first part and from the last part.
        for open in [rep.parts[0].clone(), rep.parts[rep.parts.len() - 1].clone()] {
            let items = list(&open, password).unwrap();
            assert!(items.iter().any(|i| i.path == "proj/sub/deeper/noise.bin" && i.size == 300_000), "{tag}: {items:?}");
            extract(&open, None, 2, password, &|_| true).unwrap();
        }
        let dest = root.join("x");
        let (n, _) = extract(&rep.parts[0], Some(&dest), 2, password, &|_| true).unwrap();
        assert!(n >= 3);
        same_tree(&src, &dest.join("proj"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn zip_round_trips() {
        round_trip("zip", Format::Zip, None, None);
    }

    #[test]
    fn zip_spanned_parts_round_trip() {
        round_trip("zipspan", Format::Zip, Some(100 << 10), None);
    }

    #[test]
    fn zip_with_password_round_trips_and_asks() {
        round_trip("zippw", Format::Zip, None, Some("s3cret"));
        let root = scratch("zippw2");
        let src = sample(&root);
        let out = root.join("a.zip");
        create(&out, &[src], &opts(Format::Zip, None, Some("s3cret"))).unwrap();
        assert!(needs_password(&extract(&out, None, 1, None, &|_| true).unwrap_err()));
        assert!(needs_password(&extract(&out, None, 1, Some("nope"), &|_| true).unwrap_err()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sevenz_round_trips() {
        round_trip("7z", Format::SevenZ, None, None);
    }

    #[test]
    fn sevenz_numbered_parts_round_trip() {
        round_trip("7zparts", Format::SevenZ, Some(64 << 10), None);
    }

    #[test]
    fn sevenz_with_password_round_trips_and_asks() {
        round_trip("7zpw", Format::SevenZ, None, Some("s3cret"));
        let root = scratch("7zpw2");
        let src = sample(&root);
        let out = root.join("a.7z");
        create(&out, &[src], &opts(Format::SevenZ, None, Some("s3cret"))).unwrap();
        assert!(needs_password(&list(&out, None).unwrap_err()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pcraft_parts_round_trip() {
        round_trip("pcparts", Format::Pcraft, Some(64 << 10), None);
    }

    #[test]
    fn refuses_to_overwrite_and_to_write_rar() {
        let root = scratch("refuse");
        let src = sample(&root);
        let out = root.join("a.zip");
        std::fs::write(&out, b"keep me").unwrap();
        assert!(create(&out, std::slice::from_ref(&src), &opts(Format::Zip, None, None)).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"keep me");
        assert!(create(&root.join("b.rar"), &[src], &opts(Format::Rar, None, None)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn zip_slip_is_refused() {
        let root = scratch("slip");
        let evil = root.join("evil.zip");
        let mut w = zip::ZipWriter::new(File::create(&evil).unwrap());
        w.start_file("../escaped.txt", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
        let dest = root.join("x");
        assert!(extract(&evil, Some(&dest), 1, None, &|_| true).is_err());
        assert!(!root.join("escaped.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dates_convert_both_ways() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(zip_time_to_system(system_to_zip_time(t).unwrap()).unwrap(), t);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }
}
