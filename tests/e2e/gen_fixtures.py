# Deterministic test fixtures: two 24MP JPEGs with fine detail (feather-like
# stripes, gradients, noise) plus a small PNG. Seeded so visual baselines are stable.
import sys, random
from PIL import Image, ImageDraw
out = sys.argv[1]
random.seed(1234)

def make(w, h, seed, hue):
    random.seed(seed)
    img = Image.new("RGB", (w, h))
    px = img.load()
    # Cheap but detailed: diagonal gradient + high-frequency stripes via row ops.
    import math
    for y in range(0, h):
        base_r = int(60 + 140 * y / h)
        row = []
        for x in range(0, w, 1):
            s = 0.5 + 0.5 * math.sin((x * 0.35) + (y * 0.02))   # fine stripes
            g = int(40 + 180 * x / w)
            r = int(base_r * (0.6 + 0.4 * s))
            b = int(hue * (0.5 + 0.5 * s))
            row.append((min(255, r), min(255, g), min(255, b)))
        for x, c in enumerate(row):
            px[x, y] = c
    d = ImageDraw.Draw(img)
    # Blocks of flat colour and sharp edges for crop/level checks.
    for i in range(12):
        x0 = random.randint(0, max(1, w - 600)); y0 = random.randint(0, max(1, h - 400))
        d.rectangle([x0, y0, min(w - 1, x0 + random.randint(200, 600)), min(h - 1, y0 + random.randint(150, 400))],
                    fill=(random.randint(0, 255), random.randint(0, 255), random.randint(0, 255)))
    d.ellipse([w * 0.35, h * 0.3, w * 0.65, h * 0.7], outline=(255, 255, 255), width=12)
    return img

# 6000x4000 is ~24MP (Sony a6400/a7 class). Row-by-row generation in pure
# Python is slow at full size; generate at 1500x1000 and upscale 4x with
# bicubic so the files are still 24MP with smooth detail.
def exif_for(dt, model):
    # Real-camera-shaped EXIF so capture-time sorting and EXIF carry-through get exercised.
    ex = Image.Exif()
    ex[0x010F] = "Sony"; ex[0x0110] = model; ex[0x0112] = 1; ex[0x0132] = dt
    # Pillow 9.x: the Exif sub-IFD must be assigned back as a plain dict to be serialised.
    ex[0x8769] = {0x9003: dt, 0x9004: dt, 0x829A: (1, 1600), 0x829D: (63, 10), 0x8827: 800, 0x920A: (600, 1)}
    return ex.tobytes()
a = make(1500, 1000, 1, 200).resize((6000, 4000), Image.BICUBIC)
a.save(f"{out}/bird-a-24mp.jpg", quality=92, subsampling=0, exif=exif_for("2026:05:01 06:12:00", "ILCE-6400"))
b = make(1500, 1000, 2, 90).resize((6000, 4000), Image.BICUBIC)
b.save(f"{out}/bird-b-24mp.jpg", quality=92, subsampling=0, exif=exif_for("2026:05:01 06:13:30", "ILCE-6400"))
c = make(400, 300, 3, 160).resize((800, 600), Image.BICUBIC)
c.save(f"{out}/small.png")
print("ok")
