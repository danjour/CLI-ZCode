#!/usr/bin/env python3
"""
Generate half-block truecolor ANSI art assets (FMT1 format) from the anime-eye reference.

Outputs:
  assets/eye-art-96.txt       96x36 cells (96x72 pixels, top/bottom half-blocks)
  assets/eye-art-side.txt     26x12 cells (26x24 pixels), eye-only crop

Pipeline: crop -> LANCZOS resize -> saturation/contrast boost -> black crush ->
procedural heart-pupil overlay (auto-detected from blue pixels) -> yellow iris
ring boost -> median-cut quantization (no dithering) -> FMT1 emission.
"""

import os
import sys
from collections import deque

from PIL import Image, ImageEnhance

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SRC = r"C:\Users\eduar\Downloads\1266509a1200a036a5f823e474b58c3d.jpg"

HEART_BLUE_DARK = (24, 96, 190)
HEART_BLUE = (52, 150, 238)
HEART_BLUE_LIGHT = (140, 214, 255)
HEART_WHITE = (255, 255, 255)
GOLD = (255, 196, 46)


def crop_box(im, box_frac):
    w, h = im.size
    l, t, r, b = box_frac
    return im.crop((round(l * w), round(t * h), round(r * w), round(b * h)))


def crop_box_px(im, box_px):
    return im.crop(box_px)


def adjust(im, sat=1.0, con=1.0, bright=1.0, crush=0):
    if bright != 1.0:
        im = ImageEnhance.Brightness(im).enhance(bright)
    if sat != 1.0:
        im = ImageEnhance.Color(im).enhance(sat)
    if con != 1.0:
        im = ImageEnhance.Contrast(im).enhance(con)
    if crush > 0:
        lut = [max(0, round((v - crush) * 255 / (255 - crush))) for v in range(256)]
        im = im.point(lut * 3)
    return im


def hexify(rgb):
    return "#%02X%02X%02X" % rgb


def detect_pupil(px, w, h, win=(0.20, 0.80, 0.22, 0.78)):
    """Largest connected component of blue-dominant pixels, centroid forced
    into a central window so blue hair/lash strands at the edges can't win."""
    mask = [
        [False] * w for _ in range(h)
    ]
    for y in range(h):
        for x in range(w):
            r, g, b = px[x, y][:3]
            if b > 120 and (b - r) > 50 and (b - g) > 20:
                mask[y][x] = True

    seen = [[False] * w for _ in range(h)]
    best = None  # (n, cx, cy)
    for y0 in range(h):
        for x0 in range(w):
            if not mask[y0][x0] or seen[y0][x0]:
                continue
            q = deque([(x0, y0)])
            seen[y0][x0] = True
            sx = sy = n = 0
            while q:
                x, y = q.popleft()
                sx += x
                sy += y
                n += 1
                for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
                    nx, ny = x + dx, y + dy
                    if 0 <= nx < w and 0 <= ny < h and mask[ny][nx] and not seen[ny][nx]:
                        seen[ny][nx] = True
                        q.append((nx, ny))
            cx, cy = sx / n, sy / n
            wx0, wx1, wy0, wy1 = win
            if not (wx0 * w <= cx <= wx1 * w and wy0 * h <= cy <= wy1 * h):
                continue
            if best is None or n > best[0]:
                best = (n, cx, cy)
    if best is None:
        return None
    n, cx, cy = best
    rad = max(1.5, (n / 3.14159) ** 0.5)
    return cx, cy, rad, n


def heart_sdf(px_x, px_y, cx, cy, width):
    """>0 outside heart, <=0 inside. Classic implicit heart, y-axis up."""
    nx = (px_x - cx) / (width / 2.0)
    ny = -(px_y - cy) / (width / 2.0)
    ny += 0.18
    a = nx * nx + ny * ny - 1.0
    return a * a * a - nx * nx * ny * ny * ny


def overlay_pupil(px, w, h, params):
    """Paint a crisp blue pupil + white heart centered on the detected pupil,
    then deepen the yellow iris ring around it."""
    cx, cy = params["cx"], params["cy"]
    rad = max(params["radius"] * params.get("radius_gain", 1.1),
              params.get("min_radius", 5.0))
    rad = min(rad, params.get("max_radius", 99.0))
    heart_w = rad * params.get("heart_scale", 1.25)
    heart_cy = cy - rad * 0.12
    ring_out = rad * 1.65
    for y in range(h):
        for x in range(w):
            fx, fy = x + 0.5, y + 0.5
            dx, dy = fx - cx, fy - cy
            d = (dx * dx + dy * dy) ** 0.5
            if d <= rad:
                t = max(0.0, min(1.0, (dx + dy) / (2.0 * rad) + 0.5))
                if t < 0.30:
                    f = t / 0.30
                    col = tuple(
                        round(HEART_BLUE_LIGHT[i] * (1 - f) + HEART_BLUE[i] * f)
                        for i in range(3)
                    )
                else:
                    f = (t - 0.30) / 0.70
                    col = tuple(
                        round(HEART_BLUE[i] * (1 - f) + HEART_BLUE_DARK[i] * f)
                        for i in range(3)
                    )
                if heart_sdf(fx, fy, cx, heart_cy, heart_w) <= 0:
                    col = HEART_WHITE
                px[x, y] = col
            elif d <= ring_out:
                r, g, b = px[x, y][:3]
                # Pull yellowish pixels toward a solid gold iris ring.
                if r > 150 and g > 95 and b < 160 and r + g - 2 * b > 40:
                    f = 0.60
                    px[x, y] = tuple(
                        round(c * (1 - f) + GOLD[i] * f)
                        for c, i in zip((r, g, b), range(3))
                    )
    return rad


def quantize(im, ncolors):
    return im.quantize(
        colors=ncolors, method=Image.MEDIANCUT, dither=Image.Dither.NONE
    ).convert("RGB")


def emit(path, px, w, h):
    lines = ["FMT1 %dx%d" % (w, h // 2)]
    for cy in range(h // 2):
        toks = []
        for x in range(w):
            top = hexify(px[x, cy * 2][:3])
            bot = hexify(px[x, cy * 2 + 1][:3])
            toks.append("\u2580%s/%s" % (top, bot))
        lines.append(" ".join(toks))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines) + "\n")
    print("wrote %s (%dx%d cells)" % (path, w, h // 2))


def gen_main(im):
    im = crop_box(im, (0.000, 0.026, 1.000, 0.975))
    im = im.resize((96, 72), Image.LANCZOS)
    im = adjust(im, sat=1.30, con=1.15, bright=1.03, crush=34)
    px = im.load()
    det = detect_pupil(px, 96, 72, win=(0.20, 0.80, 0.22, 0.78))
    if det:
        cx, cy, rad, n = det
        print("main: pupil at (%.1f, %.1f) r=%.1f n=%d" % (cx, cy, rad, n))
        p = {"cx": cx, "cy": cy, "radius": rad, "min_radius": 9.0,
             "max_radius": 13.0, "heart_scale": 1.30}
    else:
        print("main: pupil NOT detected, using fixed position", file=sys.stderr)
        p = {"cx": 47.5, "cy": 41.0, "radius": 11.0, "heart_scale": 1.30}
    used = overlay_pupil(px, 96, 72, p)
    print("main: overlay pupil r=%.1f" % used)
    im = quantize(im, 24)
    emit(os.path.join(ROOT, "assets", "eye-art-96.txt"), im.load(), 96, 72)


def gen_side(im):
    im = crop_box_px(im, (205, 150, 800, 699))  # 595 x 549 ~ 1.083 target
    im = im.resize((26, 24), Image.LANCZOS)
    im = adjust(im, sat=1.45, con=1.18, bright=1.03, crush=28)
    px = im.load()
    # The crop is deterministic, so the pupil center is a known constant
    # (source pupil ~ (490, 432), r ~ 128 px -> grid (12.4, 12.3), r ~ 5.6).
    # Blue-component detection is unreliable here: the white heart highlight
    # fragments the pupil ring in only 26x24 px.
    p = {"cx": 12.4, "cy": 12.3, "radius": 5.6, "radius_gain": 1.0,
         "min_radius": 4.5, "max_radius": 7.0, "heart_scale": 1.30}
    used = overlay_pupil(px, 26, 24, p)
    print("side: fixed pupil (12.4, 12.3), overlay r=%.1f" % used)
    im = quantize(im, 16)
    emit(os.path.join(ROOT, "assets", "eye-art-side.txt"), im.load(), 26, 24)


def main():
    im = Image.open(SRC).convert("RGB")
    gen_main(im)
    gen_side(im)


if __name__ == "__main__":
    main()
