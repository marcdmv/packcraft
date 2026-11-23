#!/usr/bin/env python3
"""Fetch the benchmark set listed in manifest.tsv into corpus/files/.

Every row names its original source, licence and sha256. A row whose sha256 is
"-" is pinned on its first download (the manifest is rewritten); after that a
mismatch stops the run. Paths ending in "!extract" are tar.gz archives that
are unpacked (safely, data filter) into their folder.

Usage: python3 -I corpus/fetch.py
"""
import hashlib
import io
import pathlib
import random
import sys
import tarfile
import time
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
MANIFEST = HERE / "manifest.tsv"
FILES = HERE / "files"
UA = {"User-Agent": "packcraft-benchmark-corpus/0.1"}


def download(url: str) -> bytes:
    for attempt in range(4):
        try:
            req = urllib.request.Request(url, headers=UA)
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.read()
        except Exception as e:  # noqa: BLE001 - report and back off, then fail loudly
            if attempt == 3:
                raise SystemExit(f"download failed after retries: {url}: {e}")
            wait = 15 * 2**attempt
            print(f"  retry in {wait}s ({e})", file=sys.stderr)
            time.sleep(wait)
    raise AssertionError("unreachable")


def main() -> None:
    lines = MANIFEST.read_text().splitlines()
    out_lines, changed, fetched = [], False, 0
    for line in lines:
        if line.startswith("#") or not line.strip():
            out_lines.append(line)
            continue
        path, url, sha, licence, source = line.split("\t")
        extract = path.endswith("!extract")
        target = FILES / path.removesuffix("!extract")
        cache = HERE / "cache" / (target.name if extract else "")
        have = cache if extract else target
        data = have.read_bytes() if have.is_file() else None
        if data is None:
            time.sleep(random.uniform(1.0, 3.0))
            print(f"fetch {path}")
            data = download(url)
            fetched += 1
            have.parent.mkdir(parents=True, exist_ok=True)
            have.write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        if sha == "-":
            sha, changed = digest, True
        elif sha != digest:
            raise SystemExit(f"sha256 mismatch for {path}: manifest {sha}, got {digest}")
        if extract:
            dest = target.parent / target.name.removesuffix(".tar.gz")
            if not dest.exists():
                with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as t:
                    t.extractall(target.parent, filter="data")
        out_lines.append("\t".join([path, url, sha, licence, source]))
    if changed:
        MANIFEST.write_text("\n".join(out_lines) + "\n")
    print(f"done: {fetched} downloaded, manifest {'pinned' if changed else 'unchanged'}")


if __name__ == "__main__":
    main()
