#!/usr/bin/env python3
"""Refuse content that must not appear in this public repository.

Scans every tracked file plus the staged versions of changed files for e-mail
addresses, absolute home-directory paths, IP addresses and secret patterns,
plus the terms in an optional LOCAL denylist (one case-insensitive regex per
line, never committed): .git/info/public-denylist or $PACKCRAFT_DENYLIST.
With --author it also requires a noreply commit e-mail.

Exit 1 with every finding listed; there is no inline override.

Usage: python3 -I scripts/check-public.py [--author] [--staged]
"""
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
ALLOWED_EMAILS = re.compile(r"@users\.noreply\.github\.com$|^noreply@anthropic\.com$|@example\.(com|org)$", re.I)
PATTERNS = {
    "e-mail address": re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"),
    "home directory path": re.compile(r"/Users/[A-Za-z0-9._-]+|/home/[A-Za-z0-9._-]+|[A-Za-z]:\\\\?Users\\\\?[A-Za-z0-9._-]+", re.I),
    "IPv4 address": re.compile(r"(?<![\d.])(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)(?![\d.])"),
    "private key": re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    "GitHub token": re.compile(r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}|\bgithub_pat_[A-Za-z0-9_]{30,}"),
    "AWS key": re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    "Google API key": re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b"),
    "Slack token": re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    "API secret key": re.compile(r"\bsk-(ant-)?[A-Za-z0-9_-]{20,}"),
    "service-account JSON": re.compile(r'"private_key_id"\s*:'),
}
# Loopback and unspecified addresses are harmless.
IP_OK = re.compile(r"^(127\.0\.0\.1|0\.0\.0\.0)$")


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def local_denylist() -> list[re.Pattern]:
    path = os.environ.get("PACKCRAFT_DENYLIST") or str(ROOT / ".git" / "info" / "public-denylist")
    p = pathlib.Path(path)
    if not p.is_file():
        return []
    return [re.compile(line.strip(), re.I) for line in p.read_text().splitlines() if line.strip() and not line.startswith("#")]


def scan(name: str, text: str, deny: list[re.Pattern]) -> list[str]:
    found = []
    for lineno, line in enumerate(text.splitlines(), 1):
        for label, rx in PATTERNS.items():
            for m in rx.finditer(line):
                hit = m.group(0)
                if label == "e-mail address" and ALLOWED_EMAILS.search(hit):
                    continue
                if label == "IPv4 address" and IP_OK.match(hit):
                    continue
                found.append(f"{name}:{lineno}: {label}: {hit}")
        for rx in deny:
            if rx.search(line):
                found.append(f"{name}:{lineno}: local denylist term /{rx.pattern}/")
    return found


def main() -> None:
    deny = local_denylist()
    findings = []
    # --staged (pre-commit): read what is about to be committed, i.e. the index.
    from_index = "--staged" in sys.argv
    names = {n for n in git("ls-files", "-z").split("\0") if n}
    for name in sorted(names):
        if from_index:
            try:
                data = subprocess.run(["git", "show", f":{name}"], cwd=ROOT, capture_output=True, check=True).stdout
            except subprocess.CalledProcessError:
                continue  # staged for deletion
        else:
            p = ROOT / name
            if not p.is_file():
                continue
            data = p.read_bytes()
        if b"\0" in data[:8192]:
            continue  # binary
        findings += scan(name, data.decode("utf-8", "replace"), deny)
    if "--author" in sys.argv:
        email = git("config", "user.email").strip()
        if not ALLOWED_EMAILS.search(email):
            findings.append(f"commit author e-mail {email!r} is not a noreply address: "
                            "git config user.email <id>+<login>@users.noreply.github.com")
    if findings:
        print("check-public: refusing, this repository is public:", file=sys.stderr)
        for f in findings:
            print("  " + f, file=sys.stderr)
        sys.exit(1)
    print(f"check-public: ok ({len(names)} files, {len(deny)} local denylist terms)")


if __name__ == "__main__":
    main()
