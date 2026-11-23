#!/usr/bin/env python3
"""Write PackCraft.icns: the crate mark from src/gui/icons.rs (window_icon),
drawn here with the standard library only, so the release needs no image tools.

Usage: python3 -I packaging/macos/make-icns.py OUT.icns
"""
import struct
import sys
import zlib

FILL = (0xE0, 0x7A, 0x2E)  # ACCENT in src/gui/main.rs
SS = 4  # supersampling per axis, for smooth edges


def coverage(fx: float, fy: float, n: float) -> tuple:
    """Same geometry as icons::window_icon: (inside, on_a_white_line)."""
    radius, lo, hi = n * 0.18, n * 0.06, n * 0.94
    dx = max(lo + radius - fx, fx - (hi - radius), 0.0)
    dy = max(lo + radius - fy, fy - (hi - radius), 0.0)
    if not (lo <= fx <= hi and lo <= fy <= hi and (dx * dx + dy * dy) ** 0.5 <= radius):
        return False, False
    for i in range(3):
        yc = n * (0.24 + 0.52 * (0.18 + 0.32 * i))
        indent = n * 0.06 * i
        if abs(fy - yc) <= n * 0.04 and n * 0.24 + indent <= fx <= n * 0.76 - indent:
            return True, True
    return True, False


def png(size: int) -> bytes:
    rows = bytearray()
    for y in range(size):
        rows.append(0)  # filter: none
        for x in range(size):
            a = r = g = b = 0
            for sy in range(SS):
                for sx in range(SS):
                    inside, line = coverage(x + (sx + 0.5) / SS, y + (sy + 0.5) / SS, float(size))
                    if inside:
                        a += 1
                        c = (255, 255, 255) if line else FILL
                        r, g, b = r + c[0], g + c[1], b + c[2]
            rows += bytes((r // a, g // a, b // a, 255 * a // (SS * SS))) if a else b"\0\0\0\0"

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(rows), 9)) + chunk(b"IEND", b"")


def main() -> None:
    # icns PNG entries: ic07 128, ic08 256, ic09 512, ic10 1024 (512@2x), ic11 32 (16@2x), ic12 64 (32@2x)
    entries = [(b"ic11", 32), (b"ic12", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)]
    body = b""
    cache: dict = {}
    for kind, size in entries:
        data = cache.setdefault(size, png(size))
        body += kind + struct.pack(">I", len(data) + 8) + data
    with open(sys.argv[1], "wb") as f:
        f.write(b"icns" + struct.pack(">I", len(body) + 8) + body)


if __name__ == "__main__":
    main()
