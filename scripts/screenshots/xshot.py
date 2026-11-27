#!/usr/bin/env python3
"""Screen grabs of the virtual display for scripts/screenshots.sh.

    xshot.py stable [--timeout S] [--settle N] [--min S]
        Waits until N grabs in a row (0.4 s apart) are identical, at least S seconds after the
        start. Exits 0 when the screen is still, 1 on timeout (the caller goes on regardless).
    xshot.py save OUT.png [--crop X,Y,W,H] [--colors N]
        Grabs the screen and writes a PNG. Crop is in physical pixels.

Uses `xwd -root` on $DISPLAY.
"""

import argparse
import hashlib
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from xwd2png import read_xwd_bytes  # noqa: E402

from PIL import Image, ImageChops  # noqa: E402


def grab_bytes():
    return subprocess.run(["xwd", "-root", "-silent"], check=True, capture_output=True).stdout


def cmd_stable(a):
    start = time.monotonic()
    last, last_raw, same = None, None, 0
    while True:
        raw = grab_bytes()
        # Hash the pixels only: the padding bytes of the dump differ between grabs.
        digest = hashlib.sha1(read_xwd_bytes(raw).tobytes()).digest()
        same = same + 1 if digest == last else 1
        elapsed = time.monotonic() - start
        if same >= a.settle and elapsed >= a.min:
            return 0
        if elapsed > a.timeout:
            box = ImageChops.difference(read_xwd_bytes(raw), read_xwd_bytes(last_raw)).getbbox()
            print("xshot: screen still changing after %.0f s (last change in %s)"
                  % (a.timeout, box), file=sys.stderr)
            return 1
        last, last_raw = digest, raw
        time.sleep(0.4)


def cmd_save(a):
    im = read_xwd_bytes(grab_bytes())
    if a.crop:
        x, y, w, h = (int(v) for v in a.crop.split(","))
        im = im.crop((x, y, x + w, y + h))
    if a.colors:
        im = im.quantize(colors=a.colors, method=Image.Quantize.MEDIANCUT,
                         dither=Image.Dither.NONE)
    im.save(a.out, optimize=True)
    return 0


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("stable")
    s.add_argument("--timeout", type=float, default=30)
    s.add_argument("--settle", type=int, default=3)
    s.add_argument("--min", type=float, default=0)
    v = sub.add_parser("save")
    v.add_argument("out")
    v.add_argument("--crop")
    v.add_argument("--colors", type=int, default=0)
    a = p.parse_args()
    sys.exit(cmd_stable(a) if a.cmd == "stable" else cmd_save(a))


if __name__ == "__main__":
    main()
