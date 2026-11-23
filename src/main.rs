use packcraft::archive::Options;
use packcraft::formats::{self, CreateOptions, Format};
use packcraft::volumes;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage:
  packcraft c <archive> <path>... [options]   create; the format follows the extension:
                                              .pcraft (best ratio), .zip or .7z
  packcraft x <archive> <dest-dir> [options]  extract (.pcraft, .zip, .7z, .rar and their parts)
  packcraft t <archive> [options]             decode everything and check every checksum
  packcraft l <archive> [options]             list

options:
  -v SIZE       split into parts of SIZE (e.g. 100m, 700m, 4092m, 1.5g)
  -p PASSWORD   zip/7z: encrypt with AES-256 (7z also hides file names); extract: decrypt.
                PACKCRAFT_PASSWORD in the environment does the same without showing it in ps.
  -l LEVEL      zip/7z compression level 0-9 (default 9 for zip, 7 for 7z)
  -t THREADS
  pcraft only:  --fast  --verify  --mem BITS  --chunk MB

Any part of a multi-part set opens the whole set: name.7z.001, name.part3.rar,
name.r00 or name.z01 all work.";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("packcraft: {e}");
            ExitCode::FAILURE
        }
    }
}

fn mb(b: u64) -> String {
    format!("{:.2} MB", b as f64 / 1e6)
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let mut opts = Options { threads, table_bits: 22, chunk_bytes: 16 << 20, fast: false, verify: false };
    let mut volume = None;
    let mut level = None;
    let mut password = std::env::var("PACKCRAFT_PASSWORD").ok().filter(|p| !p.is_empty());
    let mut pos = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        let num = |name: &str, v: String| v.parse::<u64>().map_err(|_| format!("{name} needs a number"));
        match a.as_str() {
            "-t" => threads = num("-t", value("-t")?)?.max(1) as usize,
            "-v" => volume = Some(volumes::parse_size(&value("-v")?)?),
            "-p" => password = Some(value("-p")?),
            "-l" => level = Some(num("-l", value("-l")?)?.min(9) as u32),
            "--fast" => opts.fast = true,
            "--verify" => opts.verify = true,
            "--mem" => opts.table_bits = num("--mem", value("--mem")?)?.clamp(16, 26) as u32,
            "--chunk" => opts.chunk_bytes = (num("--chunk", value("--chunk")?)? as usize) << 20,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => pos.push(a),
        }
    }
    opts.threads = threads;
    let pw = password.as_deref();
    let cmd = pos.first().cloned().ok_or(USAGE)?;
    match (cmd.as_str(), pos.len()) {
        ("c", n) if n >= 3 => {
            let out = PathBuf::from(&pos[1]);
            let format = Format::for_output(&out).ok_or_else(|| {
                if out.extension().is_some_and(|e| e.eq_ignore_ascii_case("rar")) {
                    "PackCraft cannot create RAR archives (the format is proprietary); use .zip or .7z".to_string()
                } else {
                    "the archive name must end in .pcraft, .zip or .7z".to_string()
                }
            })?;
            let inputs: Vec<PathBuf> = pos[2..].iter().map(PathBuf::from).collect();
            let level = level.unwrap_or(if format == Format::Zip { 9 } else { 7 });
            let r = formats::create(&out, &inputs, &CreateOptions { format, pcraft: opts, level, volume, password })?;
            if let Some(p) = &r.pcraft {
                for (route, s) in &p.stats {
                    let pct = if s.raw > 0 { 100.0 * s.packed as f64 / s.raw as f64 } else { 0.0 };
                    println!("  {:<15} {:>6} files  {:>12} -> {:>12}  ({pct:.1}%)", route.name(), s.files, mb(s.raw), mb(s.packed));
                }
                for f in &p.fallbacks {
                    println!("  note: {f} used the generic route");
                }
            }
            for f in &r.skipped {
                println!("  note: skipped {f} (not a regular file or symlink)");
            }
            if r.parts.len() > 1 {
                println!("  {} parts: {} ... {}", r.parts.len(), r.parts[0].display(), r.parts[r.parts.len() - 1].display());
            }
            println!("{} {} files: {} -> {} ({:.2}%) in {:.1}s", format.name(), r.files, mb(r.input_bytes), mb(r.archive_bytes),
                100.0 * r.archive_bytes as f64 / r.input_bytes.max(1) as f64, r.seconds);
            Ok(())
        }
        ("x", 3) => {
            let t = std::time::Instant::now();
            let (n, b) = formats::extract(&PathBuf::from(&pos[1]), Some(&PathBuf::from(&pos[2])), threads, pw, &|_| true)?;
            println!("extracted {n} entries, {} in {:.1}s, all checksums match", mb(b), t.elapsed().as_secs_f64());
            Ok(())
        }
        ("t", 2) => {
            let t = std::time::Instant::now();
            let (n, b) = formats::extract(&PathBuf::from(&pos[1]), None, threads, pw, &|_| true)?;
            println!("ok: {n} entries, {} decoded in {:.1}s, all checksums match", mb(b), t.elapsed().as_secs_f64());
            Ok(())
        }
        ("l", 2) => {
            for i in formats::list(&PathBuf::from(&pos[1]), pw)? {
                let packed = i.packed.map(|p| p.to_string()).unwrap_or_default();
                let flag = if i.encrypted { "*" } else { " " };
                println!("{:>12} {:>12} {flag} {:<15} {}{}", i.size, packed, i.method, i.path, if i.is_dir { "/" } else { "" });
            }
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}
