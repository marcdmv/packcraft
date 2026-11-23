#!/usr/bin/env python3
"""Enforce the dependency rules: pure Rust and an allowlist of licences.

Fails when any crate in the resolved graph
  - has a licence expression that cannot be satisfied by ALLOWED,
  - declares `links` (binds a native library), or
  - has a build dependency on a C toolchain crate (cc, cmake, bindgen, pkg-config).
OS bindings (libc, winapi) are allowed: they call the operating system, they
compile no C.

Usage: python3 -I scripts/check-deps.py
"""
import json
import re
import subprocess
import sys

ALLOWED = {"MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "Zlib", "0BSD", "ISC",
           "Unlicense", "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0", "BSL-1.0"}
NATIVE_BUILD = {"cc", "cmake", "bindgen", "pkg-config", "vcpkg", "autotools"}


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


def main() -> None:
    meta = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"],
                                     capture_output=True, text=True, check=True).stdout)
    root = {p["id"] for p in meta["packages"] if p["source"] is None}
    problems = []
    for p in meta["packages"]:
        if p["id"] in root:
            continue
        name = f"{p['name']} {p['version']}"
        if not p.get("license") or not satisfiable(p["license"]):
            problems.append(f"{name}: licence {p.get('license')!r} is not on the allowlist")
        if p.get("links"):
            problems.append(f"{name}: links native library {p['links']!r}")
        for d in p["dependencies"]:
            if d["kind"] == "build" and d["name"] in NATIVE_BUILD:
                problems.append(f"{name}: builds native code via {d['name']}")
    if problems:
        print("check-deps: refusing:", *problems, sep="\n  ", file=sys.stderr)
        sys.exit(1)
    print(f"check-deps: ok ({len(meta['packages']) - len(root)} crates, all pure Rust and allowlisted)")


if __name__ == "__main__":
    main()
