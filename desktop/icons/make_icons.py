"""Draws the app icon (crop marks around a frame, in the app palette) and writes
the PNG and ICO files Tauri needs. Pure Python: run `python3 make_icons.py`."""
import struct, zlib, os

MIDNIGHT, NAVY, BLUE, CREAM = (0x01, 0x07, 0x36), (0x0D, 0x1C, 0x42), (0x22, 0x39, 0x6F), (0xFC, 0xF1, 0xD0)

def inside_rounded(x, y, x0, y0, x1, y1, r):
    if not (x0 <= x <= x1 and y0 <= y <= y1):
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r

def shade(x, y):
    """Colour at (x, y) in a 0..1 unit square, or None for transparent."""
    if not inside_rounded(x, y, 0.02, 0.02, 0.98, 0.98, 0.2):
        return None
    # Crop marks: an L at each corner of the frame.
    m0, m1, arm, w = 0.17, 0.83, 0.2, 0.065
    for cx, sx in ((m0, 1), (m1, -1)):
        for cy, sy in ((m0, 1), (m1, -1)):
            dx, dy = (x - cx) * sx, (y - cy) * sy
            if (0 <= dx <= arm and 0 <= dy <= w) or (0 <= dy <= arm and 0 <= dx <= w):
                return CREAM
    # The stabilized frame, with a small cream "subject" in the middle.
    if inside_rounded(x, y, 0.29, 0.33, 0.71, 0.67, 0.05):
        if (x - 0.5) ** 2 + (y - 0.5) ** 2 <= 0.07 ** 2:
            return CREAM
        return BLUE
    return MIDNIGHT

def render(size, ss=4):
    rows = []
    for py in range(size):
        row = bytearray([0])  # PNG filter: none
        for px in range(size):
            r = g = b = a = 0
            for sy in range(ss):
                for sx in range(ss):
                    c = shade((px + (sx + 0.5) / ss) / size, (py + (sy + 0.5) / ss) / size)
                    if c:
                        r, g, b, a = r + c[0], g + c[1], b + c[2], a + 1
            n = ss * ss
            row += bytes([r // a, g // a, b // a, 255 * a // n]) if a else bytes(4)
        rows.append(bytes(row))
    return b"".join(rows)

def png(size):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(render(size), 9)) + chunk(b"IEND", b"")

def ico(images):
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for size, data in images:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(d for _, d in images)

here = os.path.dirname(os.path.abspath(__file__))
pngs = {size: png(size) for size in (16, 24, 32, 48, 64, 128, 256)}
for name, size in (("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256), ("icon.png", 256)):
    open(os.path.join(here, name), "wb").write(pngs[size])
open(os.path.join(here, "icon.ico"), "wb").write(ico([(s, pngs[s]) for s in (16, 24, 32, 48, 64, 256)]))
print("wrote icons to", here)
