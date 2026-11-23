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

## Use

```sh
cargo build --release
./target/release/packcraft c art.pcraft my-project/      # create (-t threads, --fast, --verify, --mem bits, --chunk MB)
./target/release/packcraft l art.pcraft                  # list
./target/release/packcraft t art.pcraft                  # test: decode everything, check every CRC
./target/release/packcraft x art.pcraft out/             # extract
```

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
