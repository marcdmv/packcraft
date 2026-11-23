# PackCraft: rules for anyone (human or agent) working in this repo

## This repository is PUBLIC

Everything committed here, including commit messages, author identities, issue
and PR text, and benchmark logs, is visible to the world and stays in git
history even after a later commit deletes it. Before you write anything, assume
a stranger will read it.

**Never commit or write:**

- Personal details: real email addresses (commits use the GitHub
  `@users.noreply.github.com` address), phone numbers, home or employer names,
  local usernames or absolute paths such as `/Users/<name>/` or `/home/<name>/`.
- Anything about a contributor's employer, clients, colleagues or internal
  projects. PackCraft is a stand-alone open-source project.
- Hosting and infrastructure details: hostnames, IP addresses, SSH ports,
  cloud or CDN provider names tied to a contributor's own machines.
- Secrets of any kind: API keys, tokens, passwords, private keys, cookies,
  `.env` files, service-account JSON. Not even expired or "test" ones.
- Test data that is not openly licensed, or photos of identifiable people.
  Benchmark files are downloaded from `corpus/manifest.tsv` (URL, sha256,
  licence) and are never committed.

If something sensitive was committed, do not just delete it in a new commit:
stop, tell the maintainer, and rewrite history before it is pushed (or rotate
the secret if it was already pushed).

## The guard (mechanical, not optional)

`scripts/check-public.py` scans every tracked and staged file for e-mail
addresses, home-directory paths, IP addresses and secret patterns, and refuses
a commit whose author e-mail is not a noreply address. It runs:

- as the pre-commit hook: run `git config core.hooksPath .githooks` once per clone;
- in CI on every push and pull request.

Personal terms (your own name, employer, hostnames) belong in a **local,
untracked** denylist, one case-insensitive regex per line, at
`.git/info/public-denylist` (or the path in `$PACKCRAFT_DENYLIST`). Never put
that list in the repository: it would publish exactly what it protects.
Never bypass the hook with `--no-verify`; fix the content instead.

## Code rules

- **Pure Rust, no exceptions**, dependencies included: no C/C++ code, no `-sys`
  crates, no `cc`/`cmake`/`bindgen` build steps. `scripts/check-deps.py` enforces
  this and the licence allowlist in CI.
- Dependency licences: MIT, Apache-2.0, BSD-2/3-Clause, Zlib, 0BSD, ISC,
  Unlicense, Unicode-3.0, CC0. GPL, LGPL and AGPL code is banned, and so is
  reading it: algorithms are implemented clean-room from specs and papers.
- Every change keeps `cargo test --release` green on Linux, macOS and Windows.
- A benchmark claim must be reproducible with `corpus/fetch.py`,
  `corpus/derive_audio.py` and `bench/run.py`, and name the tool versions.
