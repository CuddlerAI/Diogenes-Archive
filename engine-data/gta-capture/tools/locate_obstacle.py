"""Where a thing of the map stands, from the same point of it picked on two or more delivered frames.

    python tools/locate_obstacle.py <stage-spec.txt> <action_camera_9.json>:150:1205,478 <action_camera_1.json>:150:330,550

Each view is an action_camera JSON of a delivered package, a row of it (0 = the first frame) and the pixel (x, y)
of the point -- a flagpole's foot, a leg of a tower -- on that frame of the matching video. Every view is a ray from
that frame's camera; the point is where the rays meet best. The stage spec gives the stage's frame.

Prints the point in stage metres (x, y, and its height over the stage's sand) and in GTA world metres, and how far
each ray passes from it: a few centimetres means the picks are the same point, half a metre and more that they are
not. A thing the free cameras must keep clear of goes into stage_camera_paths.MAP_OBSTACLES as world x, y, a radius
(half its width, plus whatever it spreads: a canopy, a roof) and its top over the sand. Frames of the fixed cameras
are the easiest to pick on (views from 60 degrees apart and more meet best); the stage spec's own set pieces need
no picking, they are obstacles already.
"""
import json
import math
import sys
from pathlib import Path


def rotate(q, v):
    """v by the unit quaternion q = [x, y, z, w]."""
    x, y, z, w = q
    tx, ty, tz = 2 * (y * v[2] - z * v[1]), 2 * (z * v[0] - x * v[2]), 2 * (x * v[1] - y * v[0])
    return [v[0] + w * tx + (y * tz - z * ty), v[1] + w * ty + (z * tx - x * tz), v[2] + w * tz + (x * ty - y * tx)]


def ray(path: str, row: int, u: float, v: float):
    """The camera of that frame and the way to the pixel, in the delivery's basis (x right, y up, z ahead)."""
    r = json.loads(Path(path).read_text(encoding="utf-8"))[row]
    q, k = r["camera_rotation_quaternion"], r["camera_intrinsics"]
    right, up, ahead = (rotate(q, a) for a in ((1, 0, 0), (0, 1, 0), (0, 0, 1)))
    d = [ahead[i] + (u - k["cx"]) / k["fx"] * right[i] - (v - k["cy"]) / k["fy"] * up[i] for i in range(3)]
    n = math.sqrt(sum(c * c for c in d))
    return r["camera_position"], [c / n for c in d]


def meet(rays):
    """The point nearest to all rays (least squares), and each ray's distance from it."""
    a = [[0.0] * 3 for _ in range(3)]
    b = [0.0] * 3
    for p, d in rays:
        for i in range(3):
            for j in range(3):
                m = (1.0 if i == j else 0.0) - d[i] * d[j]
                a[i][j] += m
                b[i] += m * p[j]
    det = lambda m: (m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
                     + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]))
    whole = det(a)
    if abs(whole) < 1e-9:
        raise SystemExit("the rays run side by side: pick the point on views from further apart")
    x = []
    for c in range(3):
        m = [row[:] for row in a]
        for i in range(3):
            m[i][c] = b[i]
        x.append(det(m) / whole)
    misses = []
    for p, d in rays:
        w = [x[i] - p[i] for i in range(3)]
        t = sum(w[i] * d[i] for i in range(3))
        misses.append(math.sqrt(max(0.0, sum(c * c for c in w) - t * t)))
    return x, misses


def main(argv) -> int:
    if len(argv) < 3:
        print(__doc__)
        return 2
    frame = None
    for line in Path(argv[0]).read_text(encoding="utf-8").splitlines():
        words = line.split("#", 1)[0].split()
        if words and words[0] == "frame" and len(words) == 5:
            frame = [float(w) for w in words[1:]]
    if frame is None:
        raise SystemExit("the stage spec has no frame line")
    rays = []
    for view in argv[1:]:
        path, row, pixel = view.rsplit(":", 2)
        u, v = (float(c) for c in pixel.split(","))
        rays.append(ray(path, int(row), u, v))
    x, misses = meet(rays)
    wx, wz, wy = x                             # the delivery's [x, z, y] back to GTA's x, y, z
    fx, fy, ground, heading = frame
    h = math.radians(heading)
    sx, sy = math.cos(h) * (wx - fx) + math.sin(h) * (wy - fy), -math.sin(h) * (wx - fx) + math.cos(h) * (wy - fy)
    print(f"stage ({sx:.2f}, {sy:.2f}), {wz - ground:+.2f} m over the stage's ground line; world ({wx:.2f}, {wy:.2f}, {wz:.2f})")
    print("rays pass " + ", ".join(f"{m:.2f}" for m in misses) + " m from it")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
