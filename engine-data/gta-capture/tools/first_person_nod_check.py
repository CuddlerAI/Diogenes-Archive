"""first_person_nod_check.py <old video> <new video> <from s> <to s>: how far a first-person lane's picture slides up
and down over a stretch where the person stands still, two versions side by side -- frames every 0.5 s, each against
the one before (against the first one the scene changes and the match fails), summed, by phase correlation on the top
60% of the frame (sky, sea, far beach: little of it moves by itself); with each step's shift and match strength.
beach-demo-113 lane 12, 7-22 s (2026-10-18): the pitch following the game camera slid 177 px, held level 0.2 px.
Needs ffmpeg and numpy."""
import subprocess
import sys

import numpy as np

W, H, TOP = 960, 540, 324   # frames scaled to half 1080p; the top 60% of them compared


def frames(path, t0, t1):
    out = []
    t = t0
    while t <= t1 + 1e-6:
        raw = subprocess.run(["ffmpeg", "-v", "error", "-ss", f"{t:.2f}", "-i", path, "-frames:v", "1", "-vf", f"scale={W}:{H}",
                              "-f", "rawvideo", "-pix_fmt", "gray", "-"], capture_output=True, check=True).stdout
        out.append(np.frombuffer(raw, np.uint8).reshape(H, W).astype(np.float64))
        t += 0.5
    return out


def _peak(r, i, n):
    """sub-pixel offset of a peak at index i along an axis of n (a parabola through it and its two neighbours)"""
    a, b, c = r[(i - 1) % n], r[i], r[(i + 1) % n]
    d = a - 2 * b + c
    return 0.0 if d == 0 else 0.5 * (a - c) / d


def phase_shift(a, b, window):
    """(dx, dy, strength): how far b is shifted from a, by phase correlation (as OpenCV's phaseCorrelate)"""
    fa, fb = np.fft.fft2(a * window), np.fft.fft2(b * window)
    cross = np.conj(fa) * fb
    r = np.fft.ifft2(cross / (np.abs(cross) + 1e-12)).real
    y, x = np.unravel_index(np.argmax(r), r.shape)
    h, w = r.shape
    dy = y + _peak(r[:, x], y, h)
    dx = x + _peak(r[y, :], x, w)
    return (dx - w if dx > w / 2 else dx), (dy - h if dy > h / 2 else dy), float(r[y, x])


def main(argv) -> int:
    t0, t1 = float(argv[2]), float(argv[3])
    window = np.outer(np.hanning(TOP), np.hanning(W))
    for name, path in (("old", argv[0]), ("new", argv[1])):
        fs = frames(path, t0, t1)
        dys, per, total = [], [], 0.0
        for prev, f in zip(fs, fs[1:]):   # each 0.5 s against the one before, summed
            _, dy, strength = phase_shift(prev[:TOP], f[:TOP], window)
            total += dy * 2   # back to 1080p pixels
            dys.append(total)
            per.append(f"{dy * 2:+.0f}/{strength:.2f}")
        print(f"{name}: up/down shift vs {t0:g} s over {t0:g}-{t1:g} s: min {min(dys):+.1f} max {max(dys):+.1f} px "
              f"(range {max(dys) - min(dys):.1f} px at 1080p)")
        print("   per 0.5 s (shift px / match strength):", " ".join(per))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
