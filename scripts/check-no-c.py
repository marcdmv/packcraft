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


def rustc_import_lib(f: pathlib.Path) -> bool:
    """rustc writes <name>.dll.lib next to a Rust dylib or proc-macro <name>.dll on
    Windows (newer cargo keeps both in the unit's out dir). That is an import
    library for Rust code, not compiled C: allowed only with its sibling .dll."""
    n = f.name.lower()
    return n.endswith(".dll.lib") and f.with_name(f.name[:-4]).is_file()


found = sorted(str(f) for d in outs for f in d.rglob("*")
               if f.suffix.lower() in NATIVE and not rustc_import_lib(f))
if found:
    print("check-no-c: refusing, native code was compiled:", *found, sep="\n  ", file=sys.stderr)
    sys.exit(1)
print(f"check-no-c: ok, {len(outs)} build-script outputs, no native objects")
