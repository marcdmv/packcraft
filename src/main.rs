use packcraft::archive::{self, Options};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage:
  packcraft c <archive.pcraft> <path>... [-t threads] [--fast] [--verify] [--mem bits] [--chunk MB]
  packcraft x <archive.pcraft> <dest-dir> [-t threads]
  packcraft t <archive.pcraft> [-t threads]      decode everything and check every CRC
  packcraft l <archive.pcraft>";

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
    let mut pos = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut num = |name: &str| -> Result<u64, String> {
            it.next().and_then(|v| v.parse().ok()).ok_or(format!("{name} needs a number"))
        };
        match a.as_str() {
            "-t" => threads = num("-t")?.max(1) as usize,
            "--fast" => opts.fast = true,
            "--verify" => opts.verify = true,
            "--mem" => opts.table_bits = num("--mem")?.clamp(16, 26) as u32,
            "--chunk" => opts.chunk_bytes = (num("--chunk")? as usize) << 20,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => pos.push(a),
        }
    }
    opts.threads = threads;
    let cmd = pos.first().cloned().ok_or(USAGE)?;
    match (cmd.as_str(), pos.len()) {
        ("c", n) if n >= 3 => {
            let inputs: Vec<PathBuf> = pos[2..].iter().map(PathBuf::from).collect();
            let r = archive::create(&PathBuf::from(&pos[1]), &inputs, &opts)?;
            for (route, s) in &r.stats {
                let pct = if s.raw > 0 { 100.0 * s.packed as f64 / s.raw as f64 } else { 0.0 };
                println!("  {:<15} {:>6} files  {:>12} -> {:>12}  ({pct:.1}%)", route.name(), s.files, mb(s.raw), mb(s.packed));
            }
            for f in &r.fallbacks {
                println!("  note: {f} used the generic route");
            }
            for f in &r.skipped {
                println!("  note: skipped {f} (not a regular file or symlink)");
            }
            println!("{} -> {} ({:.2}%) in {:.1}s", mb(r.input_bytes), mb(r.archive_bytes),
                100.0 * r.archive_bytes as f64 / r.input_bytes.max(1) as f64, r.seconds);
            Ok(())
        }
        ("x", 3) => {
            let t = std::time::Instant::now();
            let (n, b) = archive::extract(&PathBuf::from(&pos[1]), Some(&PathBuf::from(&pos[2])), threads)?;
            println!("extracted {n} files, {} in {:.1}s, all CRCs match", mb(b), t.elapsed().as_secs_f64());
            Ok(())
        }
        ("t", 2) => {
            let t = std::time::Instant::now();
            let (n, b) = archive::extract(&PathBuf::from(&pos[1]), None, threads)?;
            println!("ok: {n} files, {} decoded in {:.1}s, all CRCs match", mb(b), t.elapsed().as_secs_f64());
            Ok(())
        }
        ("l", 2) => {
            for i in archive::list(&PathBuf::from(&pos[1]))? {
                println!("{:>12}  {:<15} {}", i.size, i.route.name(), i.path);
            }
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}
