#!/usr/bin/env python3
"""Generate the minimal PNG icon set used by the desktop scaffold."""

import math
import struct
import zlib
from pathlib import Path

BACKGROUND = (15, 17, 32, 255)
ACCENT = (99, 91, 255, 255)
RING = (240, 242, 255, 255)


def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )


def render(size: int) -> bytes:
    center = (size - 1) / 2
    pad = size * 0.09
    corner = size * 0.2
    half = size / 2 - pad
    outer = size * 0.32
    inner = size * 0.2

    raw = bytearray()
    for y in range(size):
        raw.append(0)
        for x in range(size):
            dx = max(abs(x - center) - half + corner, 0)
            dy = max(abs(y - center) - half + corner, 0)
            inside = math.hypot(dx, dy) <= corner
            distance = math.hypot(x - center, y - center)

            if not inside:
                color = BACKGROUND
            elif inner <= distance <= outer:
                color = RING
            else:
                color = ACCENT

            raw.extend(color)

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    icons = Path(__file__).resolve().parent.parent / "apps" / "desktop" / "src-tauri" / "icons"
    icons.mkdir(parents=True, exist_ok=True)

    for name, size in {
        "32x32.png": 32,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
    }.items():
        (icons / name).write_bytes(render(size))
        print(f"wrote {icons / name}")


if __name__ == "__main__":
    main()
