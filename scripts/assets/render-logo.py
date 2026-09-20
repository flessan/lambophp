#!/usr/bin/env python3
"""Render the application logo from the project's own icon.

The desktop application needs an `.ico` for the tray and the window, and the
welcome page needs a `.png` beside it. Both come from `web/favicon.svg`, which
is the icon this project ships - the same mark on the web page, in the taskbar
and in the notification area - so there is one design to keep in step instead of
two.

The previous implementation's `logo.ico` is *not* copied: it is that product's
branding, and this
application is Lambo PHP.

The SVG is drawn here rather than parsed, because the drawing is four shapes and
rasterising arbitrary SVG needs a renderer. The geometry below is checked
against the file before anything is written, so an icon that changes fails this
script instead of silently leaving a stale logo in the tree.

Run from anywhere:  python3 scripts/assets/render-logo.py
"""

from __future__ import annotations

import pathlib
import struct
import sys
import zlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
SVG = ROOT / "web" / "favicon.svg"
OUT_DIR = ROOT / "crates" / "lambo-core" / "assets"

# The shapes of `web/favicon.svg`, in its own 64x64 coordinate space.
VIEWBOX = 64.0
BACKDROP_RGB = (0x10, 0x15, 0x1D)
BORDER_RGB = (0x26, 0x30, 0x3F)
GRADIENT_FROM = (0xFF, 0xD1, 0x66)
GRADIENT_TO = (0xF5, 0xA3, 0x01)

# The mark: a crown-like chevron over a bar, both in the gradient.
MARK = [(14.0, 42.0), (17.0, 26.0), (25.0, 33.0), (32.0, 20.0), (39.0, 33.0), (47.0, 26.0), (50.0, 42.0)]
BAR = (14.0, 45.0, 36.0, 4.0, 2.0)

# What the SVG has to say for the drawing above to be the same picture.
EXPECTED = [
    'viewBox="0 0 64 64"',
    'rx="14" fill="#10151d"',
    'x="1" y="1" width="62" height="62" rx="13" fill="none" stroke="#26303f" stroke-width="2"',
    'd="M14 42 17 26 25 33 32 20 39 33 47 26 50 42Z" fill="url(#g)"',
    'x="14" y="45" width="36" height="4" rx="2" fill="url(#g)"',
    'stop offset="0" stop-color="#ffd166"',
    'stop offset="1" stop-color="#f5a301"',
]

# 16 and 32 are what Windows asks for in a title bar and the notification area;
# 256 is what a large-icon view asks for. 24 and 48 are the intermediate steps
# the shell picks when the display scale is not 100%.
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]
PNG_SIZE = 256
SUBSAMPLES = 4


def check_svg() -> None:
    """Refuse to draw a picture the icon no longer describes."""
    if not SVG.exists():
        sys.exit(f"{SVG} is missing: it is the source of the application logo")
    text = SVG.read_text(encoding="utf-8")
    for needle in EXPECTED:
        if needle not in text:
            sys.exit(
                f"{SVG} no longer contains {needle!r}. The drawing in this script is "
                "out of date; update it before rendering the logo."
            )


def inside_rounded_rect(x: float, y: float, left: float, top: float, width: float, height: float, radius: float) -> bool:
    """Whether a point is inside a rounded rectangle."""
    right = left + width
    bottom = top + height
    if x < left or x > right or y < top or y > bottom:
        return False
    # Only the corners need the distance test; the straight runs are inside the
    # bounds already.
    cx = min(max(x, left + radius), right - radius)
    cy = min(max(y, top + radius), bottom - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius * radius


def inside_mark(x: float, y: float) -> bool:
    """Whether a point is inside the mark, by crossing count."""
    inside = False
    count = len(MARK)
    for index in range(count):
        x1, y1 = MARK[index]
        x2, y2 = MARK[(index + 1) % count]
        if (y1 > y) != (y2 > y):
            crossing = x1 + (y - y1) * (x2 - x1) / (y2 - y1)
            if x < crossing:
                inside = not inside
    return inside


def gradient(x: float, y: float) -> tuple[int, int, int]:
    """The mark's colour: a diagonal gradient across the icon's own square."""
    t = (x / VIEWBOX + y / VIEWBOX) / 2.0
    t = min(max(t, 0.0), 1.0)
    return tuple(round(a + (b - a) * t) for a, b in zip(GRADIENT_FROM, GRADIENT_TO))


def sample(x: float, y: float) -> tuple[int, int, int, int]:
    """One sub-pixel of the icon, in the SVG's own coordinates.

    The shapes are tested outermost first: the backdrop's rounded square, then
    the two marks on top of it, then the border the square is outlined with.
    """
    if not inside_rounded_rect(x, y, 0.0, 0.0, 64.0, 64.0, 14.0):
        return (0, 0, 0, 0)
    if inside_mark(x, y) or inside_rounded_rect(x, y, BAR[0], BAR[1], BAR[2], BAR[3], BAR[4]):
        red, green, blue = gradient(x, y)
    elif inside_rounded_rect(x, y, 2.0, 2.0, 60.0, 60.0, 12.0):
        red, green, blue = BACKDROP_RGB
    else:
        red, green, blue = BORDER_RGB
    return (red, green, blue, 255)


def render(size: int) -> list[list[tuple[int, int, int, int]]]:
    """The icon at one size, top row first, supersampled down."""
    scale = VIEWBOX / size
    step = scale / SUBSAMPLES
    offset = step / 2.0
    rows: list[list[tuple[int, int, int, int]]] = []
    for row in range(size):
        pixels = []
        for column in range(size):
            red = green = blue = alpha = 0
            for sub_y in range(SUBSAMPLES):
                y = (row * scale) + offset + sub_y * step
                for sub_x in range(SUBSAMPLES):
                    x = (column * scale) + offset + sub_x * step
                    sample_red, sample_green, sample_blue, sample_alpha = sample(x, y)
                    # Premultiplied, so a half-covered edge pixel does not drag
                    # the transparent black of the corner into the colour.
                    red += sample_red * sample_alpha
                    green += sample_green * sample_alpha
                    blue += sample_blue * sample_alpha
                    alpha += sample_alpha
            count = SUBSAMPLES * SUBSAMPLES
            if alpha == 0:
                pixels.append((0, 0, 0, 0))
            else:
                pixels.append(
                    (
                        round(red / alpha),
                        round(green / alpha),
                        round(blue / alpha),
                        round(alpha / count),
                    )
                )
        rows.append(pixels)
    return rows


def ico_bytes(images: list[tuple[int, list[list[tuple[int, int, int, int]]]]]) -> bytes:
    """A multi-size icon: one `BITMAPINFOHEADER` bitmap per size, 32 bits deep."""
    entries = []
    blobs = []
    offset = 6 + 16 * len(images)
    for size, rows in images:
        header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
        body = bytearray()
        for row in reversed(rows):
            for red, green, blue, alpha in row:
                body += bytes((blue, green, red, alpha))
        # The 1-bit mask is still read by some shell code paths; a pixel is
        # transparent exactly when its alpha is zero.
        mask_stride = ((size + 31) // 32) * 4
        for row in reversed(rows):
            bits = bytearray(mask_stride)
            for column, (_, _, _, alpha) in enumerate(row):
                if alpha == 0:
                    bits[column // 8] |= 0x80 >> (column % 8)
            body += bits
        blob = header + bytes(body)
        entries.append((size, len(blob), offset))
        blobs.append(blob)
        offset += len(blob)

    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    for size, length, position in entries:
        out += struct.pack(
            "<BBBBHHII", 0 if size >= 256 else size, 0 if size >= 256 else size, 0, 0, 1, 32, length, position
        )
    for blob in blobs:
        out += blob
    return bytes(out)


def png_bytes(size: int, rows: list[list[tuple[int, int, int, int]]]) -> bytes:
    """A PNG, so the welcome page can show the same mark."""
    raw = bytearray()
    for row in rows:
        raw.append(0)  # filter: none
        for red, green, blue, alpha in row:
            raw += bytes((red, green, blue, alpha))

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    check_svg()
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    images = [(size, render(size)) for size in ICO_SIZES]
    icon = OUT_DIR / "logo.ico"
    icon.write_bytes(ico_bytes(images))
    print(f"{icon.relative_to(ROOT)}  {icon.stat().st_size} bytes  sizes {', '.join(str(s) for s in ICO_SIZES)}")

    logo = OUT_DIR / "logo.png"
    logo.write_bytes(png_bytes(PNG_SIZE, render(PNG_SIZE)))
    print(f"{logo.relative_to(ROOT)}  {logo.stat().st_size} bytes  {PNG_SIZE}x{PNG_SIZE}")


if __name__ == "__main__":
    main()
