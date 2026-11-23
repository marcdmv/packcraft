//! RAR extraction against libarchive's test archives (BSD 2-clause, see
//! tests/data/rar/README.md). Every archive stores CRC32 or BLAKE2sp
//! checksums, so `packcraft t` passing means every byte decoded correctly.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_packcraft")
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/rar").join(name)
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("packcraft-rar-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&std::ffi::OsStr]) -> (bool, String) {
    let o = Command::new(bin()).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    (o.status.success(), text)
}

fn test_ok(path: &Path, entries: usize) {
    let (ok, out) = run(&[std::ffi::OsStr::new("t"), path.as_os_str()]);
    assert!(ok, "{}: {out}", path.display());
    assert!(out.contains(&format!("ok: {entries} entries")), "{}: {out}", path.display());
    assert!(out.contains("all checksums match"), "{}: {out}", path.display());
}

#[test]
fn rar4_archives_decode() {
    for (name, entries) in [
        ("test_read_format_rar.rar", 3),
        ("test_read_format_rar_compress_normal.rar", 4),
        ("test_read_format_rar_multi_lzss_blocks.rar", 1),
        ("test_read_format_rar_filter.rar", 1),
        ("test_read_format_rar_noeof.rar", 1),
        ("test_read_format_rar_unicode.rar", 4),
        ("test_read_format_rar_windows.rar", 3),
        ("test_read_format_rar_subblock.rar", 1),
        ("test_read_format_rar_ppmd_lzss_conversion.rar", 1),
        ("test_read_format_rar_multivolume.part0001.rar", 5),
    ] {
        test_ok(&data(name), entries);
    }
}

#[test]
fn rar5_archives_decode() {
    for (name, entries) in [
        ("test_read_format_rar5_stored.rar", 1),
        ("test_read_format_rar5_compressed.rar", 1),
        ("test_read_format_rar5_solid.rar", 7),
        ("test_read_format_rar5_blake2.rar", 1),
        ("test_read_format_rar5_multiarchive_solid.part01.rar", 9),
    ] {
        test_ok(&data(name), entries);
    }
}

#[test]
fn opening_a_middle_volume_opens_the_set() {
    test_ok(&data("test_read_format_rar_multivolume.part0003.rar"), 5);
    test_ok(&data("test_read_format_rar5_multiarchive_solid.part02.rar"), 9);
}

#[test]
fn old_style_volume_names() {
    let d = scratch("oldstyle");
    for (i, ext) in ["rar", "r00", "r01", "r02"].iter().enumerate() {
        std::fs::copy(data(&format!("test_read_format_rar_multivolume.part000{}.rar", i + 1)), d.join(format!("set.{ext}"))).unwrap();
    }
    test_ok(&d.join("set.r01"), 5);
    let out = d.join("out");
    let (ok, text) = run(&[std::ffi::OsStr::new("x"), d.join("set.rar").as_os_str(), out.as_os_str()]);
    assert!(ok, "{text}");
    let big = std::fs::metadata(out.join("ppmd_lzss_conversion_test.txt")).unwrap();
    assert_eq!(big.len(), 241_647_978);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn missing_volume_is_a_clear_error() {
    let d = scratch("missing");
    std::fs::copy(data("test_read_format_rar_multivolume.part0001.rar"), d.join("set.part1.rar")).unwrap();
    std::fs::copy(data("test_read_format_rar_multivolume.part0002.rar"), d.join("set.part2.rar")).unwrap();
    let (ok, text) = run(&[std::ffi::OsStr::new("t"), d.join("set.part1.rar").as_os_str()]);
    assert!(!ok, "{text}");
    assert!(text.contains("missing"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn damaged_data_fails_loudly() {
    let d = scratch("damaged");
    let mut bytes = std::fs::read(data("test_read_format_rar_compress_normal.rar")).unwrap();
    // Flip a byte well inside the first file's packed data.
    let at = bytes.len() / 3;
    bytes[at] ^= 0x55;
    let p = d.join("bad.rar");
    std::fs::write(&p, bytes).unwrap();
    let (ok, text) = run(&[std::ffi::OsStr::new("t"), p.as_os_str()]);
    assert!(!ok, "a damaged archive passed: {text}");
    let _ = std::fs::remove_dir_all(&d);
}
