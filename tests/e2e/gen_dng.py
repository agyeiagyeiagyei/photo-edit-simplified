#!/usr/bin/env python3
"""Deterministic synthetic DNG (uncompressed 16-bit RGGB CFA) for e2e/CI.
Layout (DNG spec): IFD0 = 8-bit RGB thumbnail (NewSubFileType=1) + SubIFDs -> raw IFD (NewSubFileType=0),
plus JPEGInterchangeFormat/Length in IFD0 pointing at a real JPEG preview so the app's
"camera JPEG" fast path (largest_embedded_jpeg) is exercised as well.
Usage: gen_dng.py out.dng [width height]   (defaults 2600 2000; dims must be even)
"""
import os, struct, sys, io, math
from PIL import Image

out = sys.argv[1]; W = int(sys.argv[2]) if len(sys.argv) > 2 else 2600; H = int(sys.argv[3]) if len(sys.argv) > 3 else 2000
assert W % 2 == 0 and H % 2 == 0
os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
WHITE, BLACK = 16383, 512  # 14-bit sensor with a 512 pedestal

# --- scene: smooth gradient + fine stripes + a few flat patches, in linear light, as RGB 0..1
def scene_rgb(x, y):
    u, v = x / W, y / H
    s = 0.5 + 0.5 * math.sin(x * 0.35 + y * 0.02)          # fine stripes (demosaic stress)
    r = 0.15 + 0.7 * u
    g = 0.10 + 0.6 * v * (0.7 + 0.3 * s)
    b = 0.20 + 0.5 * (1 - u) * (0.6 + 0.4 * s)
    if 0.35 * W < x < 0.65 * W and 0.30 * H < y < 0.70 * H:  # flat grey patch
        r = g = b = 0.45
    if 0.05 * W < x < 0.20 * W and 0.05 * H < y < 0.25 * H:  # saturated red patch (clipping test)
        r, g, b = 0.95, 0.08, 0.06
    return min(r, 1.0), min(g, 1.0), min(b, 1.0)

# AsShotNeutral: camera neutral for daylight = (R gain^-1, 1, B gain^-1); use 0.5, 1, 0.7 (typical).
NEUTRAL = (0.5, 1.0, 0.7)
raw = bytearray(W * H * 2)
rows = []
for y in range(H):
    row = []
    for x in range(W):
        r, g, b = scene_rgb(x, y)
        # RGGB mosaic; apply inverse white balance so a neutral patch comes out neutral after WB.
        if y % 2 == 0: v = r * NEUTRAL[0] if x % 2 == 0 else g
        else:          v = g if x % 2 == 0 else b * NEUTRAL[2]
        code = int(BLACK + v * (WHITE - BLACK) + 0.5)
        row.append(min(WHITE, code))
    struct.pack_into('<%dH' % W, raw, y * W * 2, *row)
    if y % 4 == 0: rows.append([scene_rgb(x, y) for x in range(0, W, 4)])

# Preview/thumbnail from the same scene (what a camera JPEG would look like, gamma-encoded).
tw, th = W // 4, H // 4
thumb = Image.new('RGB', (tw, th))
px = thumb.load()
for j in range(th):
    for i in range(tw):
        r, g, b = rows[j][i]
        px[i, j] = (int(255 * r ** (1 / 2.2)), int(255 * g ** (1 / 2.2)), int(255 * b ** (1 / 2.2)))
jpeg_buf = io.BytesIO(); thumb.save(jpeg_buf, 'JPEG', quality=90); jpeg = jpeg_buf.getvalue()
thumb_rgb = thumb.resize((256, 256 * th // tw)).tobytes(); TW, TH = 256, 256 * th // tw

# --- TIFF writer -------------------------------------------------------------------------
# entries: (tag, type, count, value_bytes). types: 1 BYTE,2 ASCII,3 SHORT,4 LONG,5 RATIONAL,10 SRATIONAL
BYTE, ASCII, SHORT, LONG, RATIONAL, SRATIONAL, UNDEF = 1, 2, 3, 4, 5, 10, 7
def ent(tag, typ, vals):
    if typ == ASCII:  data = vals.encode() + b'\0'; cnt = len(data)
    elif typ == BYTE or typ == UNDEF: data = bytes(vals); cnt = len(data)
    elif typ == SHORT: data = struct.pack('<%dH' % len(vals), *vals); cnt = len(vals)
    elif typ == LONG:  data = struct.pack('<%dI' % len(vals), *vals); cnt = len(vals)
    elif typ == RATIONAL: data = b''.join(struct.pack('<II', n, d) for n, d in vals); cnt = len(vals)
    elif typ == SRATIONAL: data = b''.join(struct.pack('<ii', n, d) for n, d in vals); cnt = len(vals)
    return (tag, typ, cnt, data)

blobs = bytearray()          # out-of-line data area, appended after all IFDs
def blob(data):
    off = len(blobs); blobs.extend(data)
    if len(blobs) % 2: blobs.extend(b'\0')
    return off

# Layout plan: header(8) | IFD0 | rawIFD | blobs. Compute IFD sizes first.
def ifd_size(entries): return 2 + 12 * len(entries) + 4
def write_ifd(entries, ifd_off, next_ifd, base_blob_off):
    """Serialise IFD at ifd_off; out-of-line values are placed into `blobs` (offsets relative to base_blob_off)."""
    b = bytearray(struct.pack('<H', len(entries)))
    for tag, typ, cnt, data in sorted(entries, key=lambda e: e[0]):
        if len(data) <= 4:
            b += struct.pack('<HHI', tag, typ, cnt) + data.ljust(4, b'\0')
        else:
            b += struct.pack('<HHII', tag, typ, cnt, base_blob_off + blob(data))
    b += struct.pack('<I', next_ifd)
    return bytes(b)

# Placeholders for data offsets: we put image data in blobs too.
raw_blob = blob(bytes(raw)); thumb_blob = blob(thumb_rgb); jpeg_blob = blob(jpeg)
ifd0_entries_fn = lambda sub_off, base: [
    ent(254, LONG, [1]), ent(256, LONG, [TW]), ent(257, LONG, [TH]), ent(258, SHORT, [8, 8, 8]), ent(259, SHORT, [1]),
    ent(262, SHORT, [2]), ent(271, ASCII, 'PES'), ent(272, ASCII, 'Synthetic DNG'), ent(273, LONG, [base + thumb_blob]),
    ent(274, SHORT, [1]), ent(277, SHORT, [3]), ent(278, LONG, [TH]), ent(279, LONG, [len(thumb_rgb)]), ent(284, SHORT, [1]),
    ent(305, ASCII, 'pes gen_dng.py'), ent(330, LONG, [sub_off]),
    ent(0x0201, LONG, [base + jpeg_blob]), ent(0x0202, LONG, [len(jpeg)]),            # JPEG preview for the app's camera-JPEG path
    ent(50706, BYTE, [1, 4, 0, 0]), ent(50707, BYTE, [1, 1, 0, 0]), ent(50708, ASCII, 'PES Synthetic DNG'),
    ent(50721, SRATIONAL, [(9000, 10000), (-2000, 10000), (-1000, 10000), (-3000, 10000), (11000, 10000), (2000, 10000), (0, 10000), (1000, 10000), (6000, 10000)]),  # ColorMatrix1 (XYZ->cam), plausible
    ent(50778, SHORT, [21]),                                                              # CalibrationIlluminant1 = D65
    ent(50728, RATIONAL, [(int(NEUTRAL[0] * 10000), 10000), (10000, 10000), (int(NEUTRAL[2] * 10000), 10000)]),  # AsShotNeutral
]
raw_entries_fn = lambda base: [
    ent(254, LONG, [0]), ent(256, LONG, [W]), ent(257, LONG, [H]), ent(258, SHORT, [16]), ent(259, SHORT, [1]),
    ent(262, SHORT, [32803]), ent(273, LONG, [base + raw_blob]), ent(274, SHORT, [1]), ent(277, SHORT, [1]), ent(278, LONG, [H]),
    ent(279, LONG, [len(raw)]), ent(284, SHORT, [1]),
    ent(33421, SHORT, [2, 2]), ent(33422, BYTE, [0, 1, 1, 2]),                          # CFARepeatPatternDim, CFAPattern RGGB
    ent(50710, BYTE, [0, 1, 2]), ent(50711, SHORT, [1]),                                 # CFAPlaneColor, CFALayout
    ent(50714, LONG, [BLACK]), ent(50717, LONG, [WHITE]),                                # BlackLevel, WhiteLevel
    ent(50719, LONG, [0, 0]), ent(50720, LONG, [W, H]),                                  # DefaultCropOrigin/Size
    ent(50829, LONG, [0, 0, H, W]),                                                      # ActiveArea (top,left,bottom,right)
]
# Two-pass: sizes don't depend on offsets (counts fixed), so compute layout then serialise.
n0 = len(ifd0_entries_fn(0, 0)); n1 = len(raw_entries_fn(0))
ifd0_off = 8; raw_off = ifd0_off + ifd_size(ifd0_entries_fn(0, 0)); base = raw_off + ifd_size(raw_entries_fn(0))
ifd0 = write_ifd(ifd0_entries_fn(raw_off, base), ifd0_off, 0, base)
rawifd = write_ifd(raw_entries_fn(base), raw_off, 0, base)
assert len(ifd0) == ifd_size(ifd0_entries_fn(0, 0)) and len(rawifd) == ifd_size(raw_entries_fn(0))
with open(out, 'wb') as f:
    f.write(b'II*\0' + struct.pack('<I', ifd0_off)); f.write(ifd0); f.write(rawifd); f.write(bytes(blobs))
print(f'wrote {out}: {W}x{H} RGGB 16-bit, {len(raw)/1e6:.1f} MB raw, jpeg preview {tw}x{th} {len(jpeg)/1e3:.0f} KB, thumb {TW}x{TH}')
