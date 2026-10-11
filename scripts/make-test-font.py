"""Write a tiny synthetic TrueType font: the seed of the `font` fuzz target.

"Keel Test" / "Regular", 1000 units per em, three glyphs (an empty .notdef, a square for
every printable ASCII character, an empty space) mapped by a format 6 cmap. No outlines
are taken from any real font. Run from anywhere: it writes fuzz/corpus/font/keel-test.ttf.
"""

from pathlib import Path
import struct

OUT = Path(__file__).parents[1] / "fuzz" / "corpus" / "font" / "keel-test.ttf"

# The square: one contour, four on-curve points (100,0) (100,700) (500,700) (500,0).
square = struct.pack(">hhhhh", 1, 100, 0, 500, 700)  # contours, bbox
square += struct.pack(">HH", 3, 0)  # end point of the contour, no instructions
square += bytes([1, 1, 1, 1])  # flags: on curve, 16-bit coordinates
square += struct.pack(">hhhh", 100, 0, 400, 0)  # x deltas
square += struct.pack(">hhhh", 0, 700, 0, -700)  # y deltas
square += b"\0" * (-len(square) % 4)

tables = {
    b"head": struct.pack(
        ">IIIIHHqqhhhhHHhhh",
        0x00010000, 0x00010000, 0, 0x5F0F3CF5, 0, 1000, 0, 0,
        0, 0, 500, 700, 0, 8, 2, 0, 0,
    ),
    b"hhea": struct.pack(
        ">IhhhHhhhhhhhhhhhH",
        0x00010000, 800, -200, 0, 600, 0, 0, 500, 1, 0, 0, 0, 0, 0, 0, 0, 3,
    ),
    b"maxp": struct.pack(">IHHHHHHHHHHHHHH", 0x00010000, 3, 4, 1, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0),
    b"hmtx": struct.pack(">HhHhHh", 600, 0, 600, 100, 300, 0),
    # Short loca (offsets / 2): .notdef empty, the square, space empty.
    b"loca": struct.pack(">HHHH", 0, 0, len(square) // 2, len(square) // 2),
    b"glyf": square,
}
glyph_ids = [2] + [1] * 94  # U+0020 space, U+0021..U+007E the square
cmap6 = struct.pack(">HHHHH", 6, 10 + 2 * len(glyph_ids), 0, 0x20, len(glyph_ids))
cmap6 += struct.pack(f">{len(glyph_ids)}H", *glyph_ids)
tables[b"cmap"] = struct.pack(">HHHHI", 0, 1, 3, 1, 12) + cmap6
names = [(1, "Keel Test"), (2, "Regular")]
records, strings = b"", b""
for name_id, text in names:
    data = text.encode("utf-16-be")
    records += struct.pack(">HHHHHH", 3, 1, 0x409, name_id, len(data), len(strings))
    strings += data
tables[b"name"] = struct.pack(">HHH", 0, len(names), 6 + len(records)) + records + strings


def checksum(data: bytes) -> int:
    data += b"\0" * (-len(data) % 4)
    return sum(struct.unpack(f">{len(data) // 4}I", data)) & 0xFFFFFFFF


tags = sorted(tables)
offset = 12 + 16 * len(tags)
directory, body = b"", b""
for tag in tags:
    data = tables[tag]
    directory += struct.pack(">4sIII", tag, checksum(data), offset + len(body), len(data))
    body += data + b"\0" * (-len(data) % 4)
search = 16 * 2 ** (len(tags).bit_length() - 1)
header = struct.pack(">IHHHH", 0x00010000, len(tags), search, len(tags).bit_length() - 1,
                     16 * len(tags) - search)
OUT.parent.mkdir(parents=True, exist_ok=True)
OUT.write_bytes(header + directory + body)
print(OUT, OUT.stat().st_size, "bytes")
