#!/usr/bin/env python3
"""Draws the ZP app icon (assets/icon.png, 1024x1024) with the standard library only.

A dark rounded square with a light "Z" and an accent prompt cursor. Shapes are filled
with a scanline rasterizer and 4x vertical/horizontal supersampling for anti-aliasing.
"""

import os
import struct
import zlib

SIZE = 1024
SS = 4  # supersampling factor per axis

BG_TOP = (0x23, 0x25, 0x2E)
BG_BOTTOM = (0x14, 0x15, 0x1A)
FG = (0xEE, 0xEF, 0xF3)
ACCENT = (0x6E, 0x8C, 0xFF)


def rounded_rect(x0, y0, x1, y1, r, steps=24):
    """A rounded rectangle as a polygon."""
    import math

    points = []
    corners = [
        (x1 - r, y0 + r, -90, 0),
        (x1 - r, y1 - r, 0, 90),
        (x0 + r, y1 - r, 90, 180),
        (x0 + r, y0 + r, 180, 270),
    ]
    for cx, cy, a0, a1 in corners:
        for i in range(steps + 1):
            a = math.radians(a0 + (a1 - a0) * i / steps)
            points.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return points


def coverage(polygon):
    """Per-pixel coverage (0..1) of a polygon, as rows of floats."""
    rows = [[0.0] * SIZE for _ in range(SIZE)]
    edges = list(zip(polygon, polygon[1:] + polygon[:1]))
    weight = 1.0 / (SS * SS)
    for sy in range(SIZE * SS):
        y = (sy + 0.5) / SS
        xs = []
        for (ax, ay), (bx, by) in edges:
            if (ay <= y < by) or (by <= y < ay):
                xs.append(ax + (y - ay) * (bx - ax) / (by - ay))
        xs.sort()
        row = rows[sy // SS]
        for i in range(0, len(xs) - 1, 2):
            start, end = xs[i], xs[i + 1]
            # Sub-samples along x: count those whose centers fall inside [start, end).
            s0 = max(0, int(start * SS + 0.5))
            s1 = min(SIZE * SS, int(end * SS + 0.5))
            for sx in range(s0, s1):
                row[sx // SS] += weight
    return rows


def blend(image, cov, color):
    for y in range(SIZE):
        crow = cov[y]
        irow = image[y]
        for x in range(SIZE):
            a = crow[x]
            if a <= 0:
                continue
            a = min(a, 1.0)
            r, g, b, alpha = irow[x]
            irow[x] = (
                r + (color[0] - r) * a,
                g + (color[1] - g) * a,
                b + (color[2] - b) * a,
                alpha + (1.0 - alpha) * a,
            )


def main():
    image = [[(0.0, 0.0, 0.0, 0.0)] * SIZE for _ in range(SIZE)]
    margin = 100
    background = coverage(rounded_rect(margin, margin, SIZE - margin, SIZE - margin, 185))
    for y in range(SIZE):
        t = (y - margin) / (SIZE - 2 * margin)
        t = min(max(t, 0.0), 1.0)
        color = tuple(BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t for i in range(3))
        row = image[y]
        for x in range(SIZE):
            a = min(background[y][x], 1.0)
            if a > 0:
                row[x] = (color[0], color[1], color[2], a)

    # The "Z": top bar, diagonal, bottom bar.
    left, right, top, bottom, bar = 270, 700, 300, 724, 92
    z = [
        (left, top), (right, top), (right, top + bar),
        (left + bar * 1.45, bottom - bar), (right, bottom - bar), (right, bottom),
        (left, bottom), (left, bottom - bar),
        (right - bar * 1.45, top + bar), (left, top + bar),
    ]
    blend(image, coverage(z), FG)
    # The prompt cursor.
    blend(image, coverage(rounded_rect(752, 632, 842, 724, 14)), ACCENT)

    raw = bytearray()
    for row in image:
        raw.append(0)
        for r, g, b, a in row:
            if a > 0:
                # Stored colors are premultiplied by the mask edge; unpremultiply.
                raw += bytes((int(min(r, 255)), int(min(g, 255)), int(min(b, 255)), int(round(a * 255))))
            else:
                raw += b"\0\0\0\0"

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    out = os.path.join(os.path.dirname(__file__), "..", "assets", "icon.png")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "wb") as f:
        f.write(png)
    print("wrote", os.path.normpath(out))


if __name__ == "__main__":
    main()
