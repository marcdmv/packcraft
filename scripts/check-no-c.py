#!/usr/bin/env python3
"""Fail if a build compiled any C, C++ or Objective-C (pure Rust, no exceptions).

The `cc`/`cmake` crates leave object files and static libraries in the build
scripts' OUT_DIRs under target/[<triple>/]<profile>/build/. Run
this after `cargo build`; it scans every OUT_DIR under target/.

Usage: python3 -I scripts/check-no-c.py [target-dir]
"""
import pathlib
import sys

NATIVE = {".o", ".obj", ".a", ".lib"}
root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target")
# Both layouts: build/<crate>-<hash>/out (classic) and build/<crate>/<hash>/out (newer cargo).
outs = [d for d in root.glob("**/build/**/out") if d.is_dir()]
if not outs:
    sys.exit(f"check-no-c: no build output under {root}; run cargo build first")
found = sorted(str(f) for d in outs for f in d.rglob("*") if f.suffix.lower() in NATIVE)
if found:
    print("check-no-c: refusing, native code was compiled:", *found, sep="\n  ", file=sys.stderr)
    sys.exit(1)
print(f"check-no-c: ok, {len(outs)} build-script outputs, no native objects")
