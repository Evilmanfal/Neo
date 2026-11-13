"""Generates assets/neo.ico (and preview PNGs with --preview DIR).

Neo's mark: a violet gradient tile with a white "N" whose right stroke rises
into an arrow, for sending. Pure Python, no dependencies.
Run: python assets/make_icon.py [--preview DIR]
"""
import os
import struct
import sys
import zlib

TOP = (0x9A, 0x8C, 0xFF)  # tile gradient, top-left
BOTTOM = (0x58, 0x45, 0xE8)  # bottom-right
GLYPH = (0xFF, 0xFF, 0xFF)
SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def rounded_square(x, y, r=0.24, m=0.03):
    lo, hi = m, 1 - m
    if not (lo <= x <= hi and lo <= y <= hi):
        return False
    cx = min(max(x, lo + r), hi - r)
    cy = min(max(y, lo + r), hi - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_poly(x, y, pts):
    inside = False
    j = len(pts) - 1
    for i in range(len(pts)):
        xi, yi = pts[i]
        xj, yj = pts[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            inside = not inside
        j = i
    return inside


def glyph_shapes(size):
    """The N: two stems and a diagonal; the right stem ends in an arrowhead.
    Small sizes get thicker strokes so the mark stays legible; at 20 px and
    below there's no room for the arrow, so it's a pixel-aligned plain N."""
    if size <= 20:
        u = 1 / 16  # laid out on a 16-unit grid
        top, bot, w = 4 * u, 12 * u, 3 * u
        lx, rx = 3 * u, 10 * u
        return [
            [(lx, top), (lx + w, top), (lx + w, bot), (lx, bot)],
            [(rx, top), (rx + w, top), (rx + w, bot), (rx, bot)],
            [(lx, top), (lx + w, top), (rx + w, bot), (rx, bot)],
        ]
    w = 0.12 if size >= 40 else 0.15 if size >= 24 else 0.18
    top, bot = 0.32, 0.75
    # Shifted left a little: the arrowhead makes the right side heavier.
    lx = 0.245 if size >= 24 else 0.20  # left stem's left edge
    rx = lx + 0.47 - w  # right stem's left edge
    left = [(lx, top), (lx + w, top), (lx + w, bot), (lx, bot)]
    right = [(rx, top), (rx + w, top), (rx + w, bot), (rx, bot)]
    diag = [(lx, top), (lx + w, top), (rx + w, bot), (rx, bot)]
    # Arrowhead on the right stem, pointing up.
    cx = rx + w / 2
    head_w = w * 1.2 if size >= 24 else w
    head = [(cx, top - 0.12), (cx + head_w, top + 0.05), (cx - head_w, top + 0.05)]
    return [left, right, diag, head]


def render(size):
    """Returns rows of (r, g, b, a): colour 0..255, alpha 0..1, supersampled."""
    ss = 4 if size >= 64 else 8
    shapes = glyph_shapes(size)
    rows = []
    for py in range(size):
        row = []
        for px in range(size):
            tile = glyph = 0
            for sy in range(ss):
                for sx in range(ss):
                    x = (px + (sx + 0.5) / ss) / size
                    y = (py + (sy + 0.5) / ss) / size
                    if rounded_square(x, y):
                        tile += 1
                        if any(in_poly(x, y, s) for s in shapes):
                            glyph += 1
            n = ss * ss
            a, g = tile / n, (glyph / tile if tile else 0)
            # Diagonal gradient, plus a soft light from the top.
            t = ((px + 0.5) / size + (py + 0.5) / size) / 2
            base = [TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3)]
            lift = max(0.0, 0.10 * (1 - (py + 0.5) / size * 2))
            base = [c + (255 - c) * lift for c in base]
            rgb = [base[i] * (1 - g) + GLYPH[i] * g for i in range(3)]
            row.append((rgb[0], rgb[1], rgb[2], a))
        rows.append(row)
    return rows


def bmp_entry(size):
    rows = render(size)
    pixels = bytearray()
    for row in reversed(rows):  # BMP rows are bottom-up
        for r, g, b, a in row:
            pixels += bytes((int(b), int(g), int(r), int(round(a * 255))))
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(pixels), 0, 0, 0, 0)
    mask_row = ((size + 31) // 32) * 4
    return header + bytes(pixels) + bytes(mask_row * size)


def png(path, rows, bg):
    """Writes the rows over a solid `bg`, for previewing."""
    h, w = len(rows), len(rows[0])
    raw = bytearray()
    for row in rows:
        raw.append(0)
        for r, g, b, a in row:
            raw += bytes(int(c * a + bg[i] * (1 - a)) for i, c in enumerate((r, g, b)))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    out = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(out)


def preview(d):
    """One sheet: 256 px plus 48/32/24/16 px blown up 4x, on dark and light."""
    big = render(256)
    smalls = [(s, render(s)) for s in (48, 32, 24, 16)]
    for name, bg in (("dark", (0x20, 0x20, 0x24)), ("light", (0xF3, 0xF3, 0xF3))):
        clear = (bg[0], bg[1], bg[2], 0.0)
        width = 4 * 96 + 3 * 8
        sheet = [list(r) + [clear] * (width - 256) for r in big]
        x = 0
        tiles = []
        for s, rows in smalls:
            k = 96 // s  # blown up to 96 px, nearest neighbour
            tiles.append([[p for p in row for _ in range(k)] for row in rows for _ in range(k)])
        # Lay the small ones out in a strip under the big one.
        strip_h = max(len(t) for t in tiles)
        strip = [[clear] * width for _ in range(strip_h + 16)]
        for t in tiles:
            for yy, row in enumerate(t):
                for xx, p in enumerate(row):
                    if x + xx < len(strip[0]):
                        strip[16 + yy][x + xx] = p
            x += len(t[0]) + 8
        png(os.path.join(d, f"neo-icon-{name}.png"), sheet + strip, bg)
    print(f"previews in {d}")


def main():
    if "--preview" in sys.argv:
        preview(sys.argv[sys.argv.index("--preview") + 1])
        return
    entries = [bmp_entry(s) for s in SIZES]
    out = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    for s, data in zip(SIZES, entries):
        out += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for data in entries:
        out += data
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "neo.ico")
    with open(path, "wb") as f:
        f.write(out)
    print(f"wrote {path} ({len(out)} bytes)")


if __name__ == "__main__":
    main()
