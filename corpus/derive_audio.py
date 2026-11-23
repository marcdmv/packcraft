#!/usr/bin/env python3
"""Build the audio part of the benchmark set from NASA public-domain recordings.

Source: NASA Image and Video Library, "Artemis II Live Launch Coverage"
(KSC-20260401-AU-LMM01-0001, microphones Azimuth 2 and Pad CS6), original
broadcast WAV, 32-bit float, mono, 192 kHz. LicenseRef-Public-Domain (NASA
media are not copyrighted in the US).

The originals are about 690 MB each, so this fetches one fixed byte range of
the sample data from each (pinned by sha256) and writes:
  audio/artemis2-azimuth2-float32-192k-mono.wav   the slice as published (float)
  audio/artemis2-stereo-pcm24-96k.wav             both mics as L/R, 24-bit PCM
  audio/artemis2-stereo-pcm16-48k.wav             both mics as L/R, 16-bit PCM
The PCM files are derived with ffmpeg run as a separate process (allowed for
generating fixtures; it is never linked or shipped).

Usage: python3 -I corpus/derive_audio.py
"""
import hashlib
import pathlib
import struct
import subprocess
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
OUT = HERE / "files" / "audio"
CACHE = HERE / "cache"
UA = {"User-Agent": "packcraft-benchmark-corpus/0.1"}
BASE = "https://images-assets.nasa.gov/audio/"
DATA_OFFSET = 5548  # first sample byte (data chunk at 5540 + 8-byte header)
SLICE_START = DATA_OFFSET + 768000 * 60  # one minute in, past the preroll silence
SLICE_LEN = 768000 * 52  # 52 s of 192 kHz float mono
SOURCES = {
    "azimuth2": ("KSC-20260401-AU-LMM01-0001-Artemis_II_Live_Launch_Coverage_Azimuth_2", "-"),
    "padcs6": ("KSC-20260401-AU-LMM01-0001-Artemis_II_Live_Launch_Coverage_Pad_CS6", "-"),
}
PINS = HERE / "audio-pins.tsv"


def float_wav(samples: bytes) -> bytes:
    fmt = struct.pack("<HHIIHH", 3, 1, 192000, 768000, 4, 32)
    body = b"WAVE" + b"fmt " + struct.pack("<I", 16) + fmt + b"data" + struct.pack("<I", len(samples)) + samples
    return b"RIFF" + struct.pack("<I", len(body)) + body


def fetch_slice(name: str, nid: str, pins: dict) -> bytes:
    cache = CACHE / f"{name}.slice"
    if cache.is_file():
        data = cache.read_bytes()
    else:
        url = f"{BASE}{nid}/{nid}~orig.wav"
        rng = f"bytes={SLICE_START}-{SLICE_START + SLICE_LEN - 1}"
        print(f"fetch {name} ({SLICE_LEN / 1e6:.0f} MB range)")
        req = urllib.request.Request(url, headers={**UA, "Range": rng})
        with urllib.request.urlopen(req, timeout=600) as r:
            data = r.read()
        if len(data) != SLICE_LEN:
            raise SystemExit(f"{name}: got {len(data)} bytes, expected {SLICE_LEN}")
        CACHE.mkdir(exist_ok=True)
        cache.write_bytes(data)
    digest = hashlib.sha256(data).hexdigest()
    if name in pins and pins[name] != digest:
        raise SystemExit(f"{name}: sha256 mismatch, pinned {pins[name]}, got {digest}")
    pins[name] = digest
    return data


def main() -> None:
    pins = dict(line.split("\t") for line in PINS.read_text().split("\n") if line) if PINS.exists() else {}
    OUT.mkdir(parents=True, exist_ok=True)
    tmp = {}
    for name, (nid, _) in SOURCES.items():
        tmp[name] = CACHE / f"{name}.wav"
        tmp[name].write_bytes(float_wav(fetch_slice(name, nid, pins)))
    PINS.write_text("".join(f"{k}\t{v}\n" for k, v in sorted(pins.items())))
    (OUT / "artemis2-azimuth2-float32-192k-mono.wav").write_bytes(tmp["azimuth2"].read_bytes())
    for codec, rate, out in [("pcm_s24le", 96000, "artemis2-stereo-pcm24-96k.wav"), ("pcm_s16le", 48000, "artemis2-stereo-pcm16-48k.wav")]:
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-i", str(tmp["azimuth2"]), "-i", str(tmp["padcs6"]),
             "-filter_complex", "[0:a][1:a]amerge=inputs=2[a]", "-map", "[a]",
             "-ar", str(rate), "-c:a", codec, "-bitexact", "-map_metadata", "-1", str(OUT / out)],
            check=True,
        )
    print("audio done:", ", ".join(p.name for p in sorted(OUT.glob("artemis2-*"))))


if __name__ == "__main__":
    main()
