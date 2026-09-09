#!/usr/bin/env python3
"""Render the FMT1 half-block art files back to PNG previews (8x nearest)."""

import os
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SCALE = 8


def parse(path):
    with open(path, encoding="utf-8") as f:
        lines = [ln.rstrip("\n") for ln in f if ln.strip()]
    header = lines[0].split()
    assert header[0] == "FMT1", "bad header in %s" % path
    cols, rows = map(int, header[1].split("x"))
    assert len(lines) - 1 == rows, "%s: expected %d rows, got %d" % (
        path, rows, len(lines) - 1)
    grid = []
    for ln in lines[1:]:
        toks = ln.split(" ")
        assert len(toks) == cols, "%s: expected %d tokens, got %d" % (
            path, cols, len(toks))
        row = []
        for t in toks:
            assert t[0] == "\u2580", "bad token %r" % t
            fg, bg = t[1:].split("/")
            row.append((fg, bg))
        grid.append(row)
    return cols, rows, grid


def render(path, out):
    cols, rows, grid = parse(path)
    im = Image.new("RGB", (cols, rows * 2))
    px = im.load()
    for cy in range(rows):
        for x in range(cols):
            fg, bg = grid[cy][x]
            px[x, cy * 2] = tuple(int(fg[i:i + 2], 16) for i in (1, 3, 5))
            px[x, cy * 2 + 1] = tuple(int(bg[i:i + 2], 16) for i in (1, 3, 5))
    big = im.resize((cols * SCALE, rows * 2 * SCALE), Image.NEAREST)
    big.save(out)
    print("%s -> %s (%dx%d px, scaled %dx)" % (path, out, cols, rows * 2, SCALE))


def main():
    render(os.path.join(ROOT, "assets", "eye-art-96.txt"),
           os.path.join(HERE, "preview-96.png"))
    render(os.path.join(ROOT, "assets", "eye-art-side.txt"),
           os.path.join(HERE, "preview-side.png"))


if __name__ == "__main__":
    main()
