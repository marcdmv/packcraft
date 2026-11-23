# PackCraft

A lossless archiver for creative work (photos, audio, 3D scenes, fonts, text,
code and data), written in pure Rust, with no C code anywhere in the
dependency tree. It runs on Linux, macOS and Windows.

PackCraft looks at each file's content, not its name. Then it sends the file down the route that compresses it best:

| Route | For | How |
|---|---|---|
| JPEG | `.jpg` photos and textures | Lossless recompression with Lepton (`lepton_jpeg`, Apache-2.0); restored bit for bit |
| Audio | 16/24-bit PCM WAV, mono or stereo | Mid/side + adaptive LMS prediction + adaptive binary residual coding |
| Context mixing | Everything else: text, code, CSV, glTF, binaries | Orders 0-6, word, column, sparse and match models, two mixers, SSE |
| Store | Already-compressed data (entropy > 7.95 bits/byte) | Kept as is, no time wasted |
| `--fast` | Everything generic | Brotli q9: decompresses about 10x faster |

## Formats

| Format | Create | Open, test, extract | Split into parts |
|---|---|---|---|
| `.pcraft` | yes (best ratio, the routes above) | yes | `name.pcraft.001`, `.002`, ... |
| `.zip` | yes (Deflate, optional AES-256) | yes (Deflate, Deflate64, bzip2, LZMA, XZ, PPMd, stored; AES and ZipCrypto) | `name.z01`, `.z02`, ... `name.zip` (the zip spec's split format); `name.zip.001` sets open too |
| `.7z` | yes (LZMA2 solid, optional AES-256 with hidden file names) | yes | `name.7z.001`, `.002`, ... |
| `.rar` | no (proprietary format) | yes: RAR 2.9 to 4.x and RAR 5, solid, stored, symlinks (not encrypted RAR yet) | `name.part1.rar`, ... and the old `name.rar` + `.r00`, `.r01`, ... |

Open any part of a set and PackCraft finds the rest. The zip and 7z engines are
the pure-Rust `zip` and `sevenz-rust2` crates; files they write open in 7-Zip,
WinRAR, Info-ZIP `unzip`, bsdtar and Windows Explorer (single-part zip).

Every file carries a CRC32. Extraction refuses paths that escape the target folder
(and symlinks pointing outside it). Archives are written atomically.

## Results

Test set: 633 openly licensed files, 229.7 MB (`corpus/manifest.tsv`). Machine: Apple M1 Pro, 8 cores.
All tools passed the byte-for-byte round-trip check.

| Tool | Archive | Ratio | vs best other | Compress | Decompress |
|---|---:|---:|---:|---:|---:|
| **PackCraft (default)** | 129.24 MB | 56.27% | **-6.8%** | 21.2 s | 21.4 s |
| **PackCraft --fast** | 137.39 MB | 59.82% | -0.9% | 13.7 s | 1.9 s |
| 7-Zip 17.05 -mx9 (LZMA2) | 138.67 MB | 60.37% | best other | 47.7 s | 3.8 s |
| xz 5.8 -9e | 139.01 MB | 60.52% | +0.2% | 103.0 s | 3.2 s |
| 7-Zip 17.05 PPMd -mx9 | 144.98 MB | 63.12% | +4.6% | 61.0 s | 70.1 s |
| zstd 1.5.7 -19 --long | 147.86 MB | 64.37% | +6.6% | 44.1 s | 0.5 s |
| zstd 1.5.7 --ultra -22 | 148.29 MB | 64.56% | +6.9% | 86.7 s | 0.5 s |
| RAR 7.12 -m5 -s -md1g | 148.67 MB | 64.73% | +7.2% | 9.8 s | 1.0 s |

By folder, PackCraft vs 7-Zip -mx9: photos -19.3%, audio -12.3%, code -10.7%,
text -9.9%, data -9.7%, 3D -1.3%, fonts +4.4%. Full table: [`bench/results.md`](bench/results.md).

Known gaps, in priority order:
- **PNG and other deflate data is stored as is.** A clean-room deflate
  re-creation route is the biggest open win (about 44 MB of the 3D folder).
- **32-bit float WAV** still goes through the generic route.
- **woff2 fonts** are already brotli-compressed.
- **Default-mode decompression is symmetric** (as slow as compression). Use
  `--fast` when extraction speed matters.
- **The 7-Zip baseline is 17.05.** Current 7-Zip releases should be measured too.

## Download

Prebuilt binaries for every release are on the
[Releases page](https://github.com/marcdmv/packcraft/releases):

| System | Archive |
|---|---|
| Windows x64 / ARM64 | `packcraft-<ver>-windows-x64.zip`, `-windows-arm64.zip` |
| macOS Apple Silicon / Intel | `packcraft-<ver>-macos-arm64.tar.gz`, `-macos-x64.tar.gz` (includes `PackCraft.app`) |
| Linux x64 / ARM64 (glibc 2.35+) | `packcraft-<ver>-linux-x64.tar.gz`, `-linux-arm64.tar.gz` |

Each archive holds the command-line tool `packcraft` and the desktop app
`packcraft-gui`, with a `.sha256` file next to it. The builds are not yet
signed with a publisher certificate, so the first launch needs one extra step:

- macOS: open `PackCraft.app`, click Done on the "could not verify" message,
  then System Settings, Privacy & Security, Open Anyway. Or, in Terminal:
  `xattr -dr com.apple.quarantine PackCraft.app`.
- Windows: on the SmartScreen message choose More info, Run anyway.

## Desktop app

`packcraft-gui` is a classic two-pane archiver window: a menu bar, a toolbar
(Add, Extract To, Test, View, Info), an address bar, and a sortable file list
that browses folders and the inside of archives (`.pcraft`, `.zip`, `.7z`,
`.rar` and their parts) the same way. The Add dialog picks the format, the
compression level, a split size (CD, FAT32, DVD presets or your own) and an
optional password; it remembers your last choice.
Double-click a folder or archive to open it; Backspace goes up. Drop files on
the window to archive them, or drop an archive to open it. Extract takes the
whole archive or only the selected entries.

**Finder right-click (macOS).** Options > Install Finder actions (or
`packcraft-gui --install-finder-actions`) adds four Quick Actions to the
Finder context menu: *PackCraft - Compress* (one click, your saved format and
settings, never overwrites), *Compress with settings* (opens the Add dialog),
*Extract here* and *Extract to folder* (several archives at once, each set
once however many of its parts you selected). The window closes when the job
succeeds and stays open with the error when it does not. RAR is extract-only:
the format is proprietary, so compressing makes zip, 7z or pcraft.

It is built with egui (pure Rust, OpenGL). On Linux it talks to X11 or to
XWayland, loading the system libraries at run time. Only the Inter font is
bundled, so file names in scripts Inter does not cover (CJK, for example)
show as boxes for now. Archives are read-only in the app: to change one,
create a new one.

```sh
cargo run --release --features gui --bin packcraft-gui -- [folder-or-archive]
```

## Use

```sh
cargo build --release                                   # add --features gui for the desktop app
./target/release/packcraft c art.pcraft my-project/      # create (-t threads, --fast, --verify, --mem bits, --chunk MB)
./target/release/packcraft c art.zip my-project/ -v 700m # zip in 700 MB parts: art.z01, art.z02, ..., art.zip
./target/release/packcraft c art.7z my-project/ -p pass  # 7z, AES-256, file names hidden (-l 0-9 sets the level)
./target/release/packcraft l art.pcraft                  # list
./target/release/packcraft t art.pcraft                  # test: decode everything, check every CRC
./target/release/packcraft x photos.part1.rar out/       # extract: any format, any part of a set
```

The format follows the archive's extension. `PACKCRAFT_PASSWORD` in the
environment works like `-p` without showing the password in the process list.

The format is experimental (`PKCRAFT1`) and may change before 1.0.

## Reproduce the benchmark

```sh
python3 -I corpus/fetch.py          # downloads the files listed in corpus/manifest.tsv, checks sha256
python3 -I corpus/derive_audio.py   # NASA public-domain audio slices (needs ffmpeg)
cargo build --release
python3 -I bench/run.py --categories
```

Test files are never committed. Each row of the manifest names its source
URL, sha256 and licence (CC0, CC-BY-4.0, OFL, public domain, MIT).

## Contributing

Read [`CLAUDE.md`](CLAUDE.md) first. The repository is public, so a pre-commit
guard refuses personal data and secrets (`git config core.hooksPath .githooks`).
Code is pure Rust, and dependencies must use permissive licences. GPL and
LGPL code is never read: algorithms are written clean-room from specs and
papers.

## Licence

MIT OR Apache-2.0, at your option. Third-party notices are in [`NOTICE`](NOTICE).
