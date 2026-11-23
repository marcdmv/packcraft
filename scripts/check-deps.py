#!/usr/bin/env python3
"""Enforce the dependency rules for every shipped target, all features on.

Fails when any crate actually built for a target
  - has a licence expression that cannot be satisfied by ALLOWED, or
  - declares `links` to a native library that is not an OS runtime (OS_LINKS).
Whether C is really compiled is decided by scripts/check-no-c.py on the build
output, because a `cc` build dependency is often only used behind a feature
that is off. OS bindings (libc, winapi, libobjc) call the operating system and
compile no C.

Usage: python3 -I scripts/check-deps.py
"""
import json
import re
import subprocess
import sys

ALLOWED = {"MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "Zlib", "0BSD", "ISC",
           "Unlicense", "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0", "BSL-1.0",
           "MIT-0",
           # bzip2's own BSD-style licence (no copyleft), used by the pure-Rust
           # libbz2-rs-sys port that reads and writes bzip2 inside ZIP and 7z.
           "bzip2-1.0.6"}
# Crates whose `links` names an operating-system library that every install of
# that OS ships (loaded, never compiled), allowed like libc or winapi.
OS_LINKS = {"objc-sys"}  # the macOS Objective-C runtime (libobjc)


def satisfiable(expr: str) -> bool:
    """Tiny SPDX evaluator: OR (or '/') needs one side, AND needs both, parentheses group."""
    tokens = re.findall(r"\(|\)|[A-Za-z0-9.+-]+|/", expr.replace(" WITH LLVM-exception", ""))
    pos = 0

    def parse_or() -> bool:
        nonlocal pos
        ok = parse_and()
        while pos < len(tokens) and tokens[pos] in ("OR", "/"):
            pos += 1
            ok = parse_and() or ok
        return ok

    def parse_and() -> bool:
        nonlocal pos
        ok = parse_atom()
        while pos < len(tokens) and tokens[pos] == "AND":
            pos += 1
            ok = parse_atom() and ok
        return ok

    def parse_atom() -> bool:
        nonlocal pos
        tok = tokens[pos]
        pos += 1
        if tok == "(":
            ok = parse_or()
            pos += 1  # ')'
            return ok
        return tok in ALLOWED

    return bool(tokens) and parse_or()


TARGETS = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-apple-darwin",
           "aarch64-apple-darwin", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]


def check_target(target: str, problems: dict) -> int:
    """Walk the crates actually built for `target` with all features on."""
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features", "--filter-platform", target],
        capture_output=True, text=True, check=True).stdout)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    seen, stack = set(), [root]
    while stack:
        nid = stack.pop()
        if nid in seen:
            continue
        seen.add(nid)
        p = pkgs[nid]
        name = f"{p['name']} {p['version']}"
        if nid != root:
            if not p.get("license") or not satisfiable(p["license"]):
                problems.setdefault(f"{name}: licence {p.get('license')!r} is not on the allowlist", set()).add(target)
            if p.get("links") and p["name"] not in OS_LINKS:
                problems.setdefault(f"{name}: links native library {p['links']!r}", set()).add(target)
        stack.extend(d["pkg"] for d in nodes[nid]["deps"])
    return len(seen) - 1


def main() -> None:
    problems: dict = {}
    counts = {t: check_target(t, problems) for t in TARGETS}
    if problems:
        print("check-deps: refusing:", file=sys.stderr)
        for msg, targets in sorted(problems.items()):
            print(f"  {msg} [{', '.join(sorted(targets))}]", file=sys.stderr)
        sys.exit(1)
    print("check-deps: ok, all pure Rust and allowlisted: " + ", ".join(f"{t} {n}" for t, n in counts.items()))


if __name__ == "__main__":
    main()
