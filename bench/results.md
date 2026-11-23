# Benchmark: corpus/files (633 files, 229.7 MB)

Apple M1 Pro, 8 cores. Wall-clock times, each tool at its own default thread count.

| Tool | Archive | Ratio | vs best other | Compress | Decompress | Round trip |
|---|---:|---:|---:|---:|---:|---|
| PackCraft (default) | 129.24 MB | 56.27% | -6.8% | 21.2 s | 21.4 s | ok |
| PackCraft --fast | 137.39 MB | 59.82% | -0.9% | 13.7 s | 1.9 s | ok |
| 7-Zip 17.05 -mx9 (LZMA2, solid) | 138.67 MB | 60.37% | +0.0% | 47.7 s | 3.8 s | ok |
| xz 5.8 -9e | 139.01 MB | 60.52% | +0.2% | 103.0 s | 3.2 s | ok |
| 7-Zip 17.05 PPMd -mx9 | 144.98 MB | 63.12% | +4.6% | 61.0 s | 70.1 s | ok |
| zstd 1.5.7 -19 --long | 147.86 MB | 64.37% | +6.6% | 44.1 s | 0.5 s | ok |
| zstd 1.5.7 --ultra -22 | 148.29 MB | 64.56% | +6.9% | 86.7 s | 0.5 s | ok |
| RAR 7.12 -m5 -s -md1g | 148.67 MB | 64.73% | +7.2% | 9.8 s | 1.0 s | ok |

## By folder (MB)

| Folder | Raw | 7-Zip -mx9 | RAR -m5 | PackCraft | PackCraft vs 7-Zip |
|---|---:|---:|---:|---:|---:|
| 3d | 87.77 | 75.26 | 76.21 | 74.25 | -1.3% |
| audio | 79.87 | 29.40 | 36.69 | 25.77 | -12.3% |
| code | 3.03 | 0.42 | 0.48 | 0.37 | -10.7% |
| data | 24.47 | 5.19 | 6.37 | 4.69 | -9.7% |
| fonts | 3.35 | 1.43 | 1.59 | 1.49 | +4.4% |
| photos | 19.71 | 19.14 | 19.28 | 15.44 | -19.3% |
| text | 11.49 | 7.85 | 8.01 | 7.07 | -9.9% |
