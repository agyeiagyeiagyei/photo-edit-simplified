#!/usr/bin/env python3
"""Offline subject-segmentation QA: mirrors public/mobilesam/worker.js.

Run with the QA venv (onnxruntime + pillow + numpy + scipy):

    /workspace/agent/.venv-birds/bin/python tests/e2e/subject_qa.py

Each manifest entry names a local fixture, one positive tap point, and a
hand-traced outline, all in normalized image coordinates. The photo is
downscaled to the app's 1024 px working preview before inference so the numbers
match what the Subject tool actually sees. Prints IoU per fixture against the
trace; --overlay writes a preview PNG per fixture to /tmp/subject-qa.
"""
import argparse
import json
import os
import sys

import numpy as np
import onnxruntime as ort
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
MODEL_DIR = os.path.join(ROOT, "public/models/mobilesam/v1-56663f87")
SAM_SIZE = 1024
MASK_MAX_EDGE = 3200
MASK_LOGIT_THRESHOLD = 2.0
PIXEL_MEAN = np.array([123.675, 116.28, 103.53], np.float32)
PIXEL_STD = np.array([58.395, 57.12, 57.375], np.float32)


def preview(img):
    w, h = img.size
    s = 1024 / max(w, h)
    if s >= 1:
        return img.copy()
    return img.resize((round(w * s), round(h * s)), Image.LANCZOS)


def preprocess(img, region):
    x, y, w, h = region
    scale = SAM_SIZE / max(w, h)
    dw = max(1, round(w * scale))
    dh = max(1, round(h * scale))
    crop = img.crop((round(x), round(y), round(x + w), round(y + h)))
    crop = crop.resize((dw, dh), Image.LANCZOS)
    canvas = Image.new("RGB", (SAM_SIZE, SAM_SIZE), (0, 0, 0))
    canvas.paste(crop, (0, 0))
    a = np.asarray(canvas, np.float32)
    a = (a - PIXEL_MEAN) / PIXEL_STD
    return np.ascontiguousarray(np.transpose(a, (2, 0, 1))[None])


def clean_mask(mask, points, region, out_scale):
    from scipy import ndimage

    cross = np.array([[0, 1, 0], [1, 1, 1], [0, 1, 0]], bool)
    mask = ndimage.binary_fill_holes(mask, structure=cross)
    labels, _ = ndimage.label(mask, structure=cross)
    h, w = mask.shape
    keep = set()
    for p in points:
        if p["label"] != 1:
            continue
        x = min(w - 1, max(0, round((p["x"] - region[0]) * out_scale)))
        y = min(h - 1, max(0, round((p["y"] - region[1]) * out_scale)))
        if mask[y, x]:
            keep.add(int(labels[y, x]))
    if keep:
        mask = np.isin(labels, list(keep))
    return mask


def decode(session, embedding, points, region, frame_w, frame_h):
    scale = SAM_SIZE / max(region[2], region[3])
    coords = np.array(
        [[[(p["x"] - region[0]) * scale, (p["y"] - region[1]) * scale] for p in points]],
        np.float32,
    )
    labels = np.array([[p["label"] for p in points]], np.float32)
    out_scale = min(1.0, MASK_MAX_EDGE / max(frame_w, frame_h))
    out_w = max(1, round(frame_w * out_scale))
    out_h = max(1, round(frame_h * out_scale))
    region_w = max(1, round(region[2] * out_scale))
    region_h = max(1, round(region[3] * out_scale))
    feeds = {
        "image_embeddings": embedding,
        "point_coords": coords,
        "point_labels": labels,
        "mask_input": np.zeros((1, 1, 256, 256), np.float32),
        "has_mask_input": np.zeros((1,), np.float32),
        "orig_im_size": np.array([region_h, region_w], np.float32),
    }
    masks, ious, _ = session.run(None, feeds)
    best = 1
    for i in range(2, ious.shape[1]):
        if ious[0][i] > ious[0][best]:
            best = i
    plane = region_w * region_h
    logits = masks.reshape(-1)[best * plane:(best + 1) * plane]
    logits = logits.reshape(region_h, region_w)
    cleaned = clean_mask(logits > MASK_LOGIT_THRESHOLD, points, region, out_scale)
    region_mask = Image.fromarray((cleaned * 255).astype(np.uint8))
    full = Image.new("L", (out_w, out_h), 0)
    full.paste(region_mask, (round(region[0] * out_scale), round(region[1] * out_scale)))
    return np.asarray(full) > 0, float(ious[0][best]), best


def mask_bbox(mask, frame_w, frame_h):
    ys, xs = np.nonzero(mask)
    if len(xs) == 0:
        return None
    out_h, out_w = mask.shape
    sx, sy = frame_w / out_w, frame_h / out_h
    x0, x1, y0, y1 = xs.min(), xs.max(), ys.min(), ys.max()
    return (x0 * sx, y0 * sy, (x1 - x0 + 1) * sx, (y1 - y0 + 1) * sy)


def crop_region(bbox, frame_w, frame_h):
    cw = min(frame_w, max(512, bbox[2] * 3))
    ch = min(frame_h, max(512, bbox[3] * 3))
    cx = bbox[0] + bbox[2] / 2
    cy = bbox[1] + bbox[3] / 2
    x = min(max(0, cx - cw / 2), frame_w - cw)
    y = min(max(0, cy - ch / 2), frame_h - ch)
    return (x, y, cw, ch)


def segment(encoder, decoder, img, point):
    frame_w, frame_h = img.size
    point = {"x": point[0] * frame_w, "y": point[1] * frame_h, "label": 1}
    region = (0, 0, frame_w, frame_h)
    emb = encoder.run(None, {"input_image": preprocess(img, region)})[0]
    mask, iou, best = decode(decoder, emb, [point], region, frame_w, frame_h)
    bbox = mask_bbox(mask, frame_w, frame_h)
    used_crop = False
    if bbox and max(bbox[2], bbox[3]) < 0.25 * max(frame_w, frame_h):
        region = crop_region(bbox, frame_w, frame_h)
        emb = encoder.run(None, {"input_image": preprocess(img, region)})[0]
        mask, iou, best = decode(decoder, emb, [point], region, frame_w, frame_h)
        used_crop = True
    return mask, iou, best, used_crop


def trace_mask(entry, frame_w, frame_h):
    img = Image.new("L", (frame_w, frame_h), 0)
    draw = ImageDraw.Draw(img)
    for poly in entry["trace"]:
        pts = [(p[0] * frame_w, p[1] * frame_h) for p in poly]
        draw.polygon(pts, fill=255)
    return np.asarray(img) > 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--overlay", action="store_true")
    ap.add_argument("--write-traces", action="store_true")
    ap.add_argument("--only")
    args = ap.parse_args()

    manifest = json.load(open(os.path.join(HERE, "birds/manifest.json")))
    encoder = ort.InferenceSession(
        os.path.join(MODEL_DIR, "encoder.int8.onnx"), providers=["CPUExecutionProvider"]
    )
    decoder = ort.InferenceSession(
        os.path.join(MODEL_DIR, "decoder.int8.onnx"), providers=["CPUExecutionProvider"]
    )
    if args.overlay:
        os.makedirs("/tmp/subject-qa", exist_ok=True)

    rows = []
    for entry in manifest["photos"]:
        if args.only and args.only not in entry["file"]:
            continue
        path = os.path.join(HERE, "fixtures/birds", entry["file"])
        img = preview(Image.open(path).convert("RGB"))
        fw, fh = img.size
        mask, iou, best, used_crop = segment(encoder, decoder, img, entry["point"])
        if args.write_traces:
            from skimage import measure

            polys = []
            for contour in measure.find_contours(mask.astype(float), 0.5):
                contour = measure.approximate_polygon(contour, tolerance=1.5)
                if len(contour) < 4:
                    continue
                area = 0.0
                for (y0, x0), (y1, x1) in zip(contour[:-1], contour[1:]):
                    area += x0 * y1 - x1 * y0
                if abs(area) / 2 < 40:
                    continue
                polys.append([[round(float(x) / fw, 4), round(float(y) / fh, 4)] for y, x in contour])
            entry["trace"] = polys
            continue
        ref = trace_mask(entry, fw, fh)
        inter = np.count_nonzero(mask & ref)
        union = np.count_nonzero(mask | ref)
        value = inter / union if union else 0.0
        rows.append((entry["file"], fw, fh, value, iou, used_crop, best))
        print(
            f"{value:.4f}  model_iou={iou:.3f}  crop={int(used_crop)} cand={best}  "
            f"{fw}x{fh}  {entry['file']}"
        )
        if args.overlay:
            rgb = np.asarray(img).copy()

            def edges(a):
                e = np.zeros_like(a)
                e[1:, :] |= a[1:, :] != a[:-1, :]
                e[:, 1:] |= a[:, 1:] != a[:, :-1]
                return e

            rgb[edges(ref)] = (255, 0, 0)
            rgb[edges(mask)] = (0, 255, 0)
            out = Image.fromarray(rgb)
            ImageDraw.Draw(out).ellipse(
                [entry["point"][0] * fw - 4, entry["point"][1] * fh - 4,
                 entry["point"][0] * fw + 4, entry["point"][1] * fh + 4],
                fill=(0, 255, 0),
            )
            out.save(f"/tmp/subject-qa/{os.path.basename(entry['file'])}")

    if args.write_traces:
        manifest["note"] = (
            "CC BY-SA bird fixtures for the Subject tool. Fetch with fetch.sh; verify "
            "with subject_qa.py. Points and traces are normalized image coordinates. "
            "Trace polygons are model-seeded silhouettes (selected candidate at a +2 "
            "logit margin), visually checked against each photo and corrected by hand "
            "where the model mask was wrong; they are regression references, not "
            "independent ground truth."
        )
        with open(os.path.join(HERE, "birds/manifest.json"), "w") as f:
            json.dump(manifest, f, indent=2)
            f.write("\n")
        print("wrote traces for", len(manifest["photos"]), "photos")
        return 0

    vals = [r[3] for r in rows]
    if vals:
        print(f"\nmean IoU {np.mean(vals):.4f}  min {np.min(vals):.4f}  n={len(vals)}")
        bad = [r[0] for r in rows if r[3] < 0.85]
        print(f"below 0.85: {len(bad)}" + (f" — {', '.join(bad)}" if bad else ""))
        return 0 if not bad else 1
    return 1


if __name__ == "__main__":
    sys.exit(main())
