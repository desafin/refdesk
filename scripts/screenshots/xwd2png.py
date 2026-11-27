#!/usr/bin/env python3
"""Converts an XWD dump (from `xwd -root`) to PNG, optionally cropped and quantized.

    xwd2png.py in.xwd out.png [--crop X,Y,W,H] [--colors N]
"""

import argparse
import struct
import sys

from PIL import Image


def read_xwd(path):
    with open(path, "rb") as f:
        return read_xwd_bytes(f.read())


def read_xwd_bytes(data):
    fields = struct.unpack(">25I", data[:100])
    header_size, version, pixmap_format, depth, width, height = fields[:6]
    byte_order, bitmap_unit, bitmap_bit_order, bitmap_pad, bits_per_pixel, bytes_per_line = fields[7:13]
    ncolors = fields[19]
    if version != 7 or pixmap_format != 2 or bits_per_pixel not in (24, 32):
        sys.exit("xwd2png: unsupported XWD (version %d, format %d, %d bpp)"
                 % (version, pixmap_format, bits_per_pixel))
    offset = header_size + ncolors * 12
    raw = data[offset:offset + bytes_per_line * height]
    if bits_per_pixel == 32:
        mode = "BGRX" if byte_order == 0 else "XRGB"
    else:
        mode = "BGR" if byte_order == 0 else "RGB"
    return Image.frombuffer("RGB", (width, height), raw, "raw", mode, bytes_per_line, 1)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("src")
    p.add_argument("dst")
    p.add_argument("--crop")
    p.add_argument("--colors", type=int, default=0)
    a = p.parse_args()
    im = read_xwd(a.src)
    if a.crop:
        x, y, w, h = (int(v) for v in a.crop.split(","))
        im = im.crop((x, y, x + w, y + h))
    if a.colors:
        im = im.quantize(colors=a.colors, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
    im.save(a.dst, optimize=True)


if __name__ == "__main__":
    main()
