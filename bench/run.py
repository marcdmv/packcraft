#!/usr/bin/env python3
"""Benchmark PackCraft against the common archivers on corpus/files.

For every tool: archive size, wall-clock compress and decompress time, and a
byte-for-byte comparison of the extracted tree with the input (a tool that does
not round-trip is reported as FAILED, never scored). Writes bench/results.md.

Usage: python3 -I bench/run.py [--only name,name] [--categories]
"""
import filecmp
import pathlib
import shutil
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
CORPUS = ROOT / "corpus"
OUT = ROOT / "bench" / "out"
PC = str(ROOT / "target" / "release" / "packcraft")

# name: (compress command, decompress command); {a} archive, {d} extract dir, {src} input folder name.
TOOLS = {
    "7-Zip 17.05 -mx9 (LZMA2, solid)": ("7z a -t7z -mx=9 -bd -bb0 {a} {src} >/dev/null", "7z x -y -bd -o{d} {a} >/dev/null"),
    "7-Zip 17.05 PPMd -mx9": ("7z a -t7z -m0=PPMd -mx=9 -bd -bb0 {a} {src} >/dev/null", "7z x -y -bd -o{d} {a} >/dev/null"),
    "RAR 7.12 -m5 -s -md1g": ("rar a -m5 -s -md1g -r -idq {a} {src}", "rar x -idq {a} {d}/"),
    "zstd 1.5.7 -19 --long": ("tar -cf - {src} | zstd -19 --long=27 -T0 -q -o {a}", "mkdir -p {d} && zstd -d --long=27 -q -c {a} | tar -xf - -C {d}"),
    "zstd 1.5.7 --ultra -22": ("tar -cf - {src} | zstd --ultra -22 --long=27 -T0 -q -o {a}", "mkdir -p {d} && zstd -d --long=27 -q -c {a} | tar -xf - -C {d}"),
    "xz 5.8 -9e": ("tar -cf - {src} | xz -9e -T0 > {a}", "mkdir -p {d} && xz -d -c {a} | tar -xf - -C {d}"),
    "PackCraft --fast": (PC + " c {a} {src} --fast >/dev/null", PC + " x {a} {d} >/dev/null"),
    "PackCraft (default)": (PC + " c {a} {src} >/dev/null", PC + " x {a} {d} >/dev/null"),
}
EXT = {"7-Zip": ".7z", "RAR": ".rar", "zstd": ".tar.zst", "xz": ".tar.xz", "PackCraft": ".pcraft"}


def same_tree(a: pathlib.Path, b: pathlib.Path) -> bool:
    cmp = filecmp.dircmp(a, b)
    if cmp.left_only or cmp.right_only or cmp.funny_files:
        return False
    _, mismatch, errors = filecmp.cmpfiles(a, b, cmp.common_files, shallow=False)
    if mismatch or errors:
        return False
    return all(same_tree(a / s, b / s) for s in cmp.common_dirs)


def tree_bytes(p: pathlib.Path) -> int:
    return sum(f.stat().st_size for f in p.rglob("*") if f.is_file())


def run_tool(name: str, src_parent: pathlib.Path, src: str) -> dict:
    comp, decomp = TOOLS[name]
    ext = next(v for k, v in EXT.items() if name.startswith(k))
    slug = "".join(c if c.isalnum() else "_" for c in name)[:40]
    arc = OUT / f"{slug}-{src}{ext}"
    dest = OUT / f"{slug}-{src}-x"
    for p in (arc, dest):
        if p.is_dir():
            shutil.rmtree(p)
        elif p.exists():
            p.unlink()
    t = time.perf_counter()
    subprocess.run(comp.format(a=arc, d=dest, src=src), shell=True, check=True, cwd=src_parent)
    ct = time.perf_counter() - t
    t = time.perf_counter()
    subprocess.run(decomp.format(a=arc, d=dest, src=src), shell=True, check=True, cwd=src_parent)
    dt = time.perf_counter() - t
    ok = same_tree(src_parent / src, dest / src)
    size = arc.stat().st_size
    shutil.rmtree(dest)
    arc.unlink()
    return {"name": name, "size": size, "ct": ct, "dt": dt, "ok": ok}


def main() -> None:
    only = None
    if "--only" in sys.argv:
        only = sys.argv[sys.argv.index("--only") + 1].split(",")
    OUT.mkdir(parents=True, exist_ok=True)
    files = CORPUS / "files"
    raw = tree_bytes(files)
    nfiles = sum(1 for f in files.rglob("*") if f.is_file())
    rows = []
    for name in TOOLS:
        if only and not any(o.lower() in name.lower() for o in only):
            continue
        r = run_tool(name, CORPUS, "files")
        print(f"{name:<34} {r['size']/1e6:9.2f} MB  {100*r['size']/raw:6.2f}%  c {r['ct']:6.1f}s  d {r['dt']:6.1f}s  {'ok' if r['ok'] else 'FAILED'}", flush=True)
        rows.append(r)

    cats = []
    if "--categories" in sys.argv:
        for cat in sorted(p.name for p in files.iterdir() if p.is_dir()):
            craw = tree_bytes(files / cat)
            line = {"cat": cat, "raw": craw}
            for name in ("7-Zip 17.05 -mx9 (LZMA2, solid)", "RAR 7.12 -m5 -s -md1g", "PackCraft (default)"):
                r = run_tool(name, files, cat)
                line[name] = r["size"] if r["ok"] else None
            cats.append(line)
            print(cat, line, flush=True)

    best_other = min((r["size"] for r in rows if r["ok"] and not r["name"].startswith("PackCraft")), default=None)
    md = [f"# Benchmark: corpus/files ({nfiles} files, {raw/1e6:.1f} MB)", "",
          "Apple M1 Pro, 8 cores. Wall-clock times, each tool at its own default thread count.", "",
          "| Tool | Archive | Ratio | vs best other | Compress | Decompress | Round trip |",
          "|---|---:|---:|---:|---:|---:|---|"]
    for r in sorted(rows, key=lambda r: r["size"]):
        vs = f"{100*(r['size']/best_other-1):+.1f}%" if best_other else ""
        md.append(f"| {r['name']} | {r['size']/1e6:.2f} MB | {100*r['size']/raw:.2f}% | {vs} | {r['ct']:.1f} s | {r['dt']:.1f} s | {'ok' if r['ok'] else '**FAILED**'} |")
    if cats:
        md += ["", "## By folder (MB)", "", "| Folder | Raw | 7-Zip -mx9 | RAR -m5 | PackCraft | PackCraft vs 7-Zip |", "|---|---:|---:|---:|---:|---:|"]
        for c in cats:
            z, rr, p = c["7-Zip 17.05 -mx9 (LZMA2, solid)"], c["RAR 7.12 -m5 -s -md1g"], c["PackCraft (default)"]
            f = lambda v: f"{v/1e6:.2f}" if v else "FAILED"
            vs = f"{100*(p/z-1):+.1f}%" if p and z else ""
            md.append(f"| {c['cat']} | {c['raw']/1e6:.2f} | {f(z)} | {f(rr)} | {f(p)} | {vs} |")
    (ROOT / "bench" / "results.md").write_text("\n".join(md) + "\n")
    print("wrote bench/results.md")


if __name__ == "__main__":
    main()
