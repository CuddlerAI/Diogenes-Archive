"""The sun from the shadows in a delivered group, for group.json's environment.sun (stage_multicam.sun_lighting).

The game has no native that reads the sun, and the delivery doc v1.1 wants the main light's direction and intensity in
every multicam_sync frame. A take sets the group's clock and stops it, so the sun stands still all take and one
direction holds for every frame: measured here from people standing still in the fixed lanes, each giving the line from
the top of the head to the tip of its shadow on the sand.

    python tools/sun_from_shadows.py <package dir> <group dir> <samples.json>

samples.json: [["<number>:<frame>:<id>", [head-top x, y], [shadow-tip x, y]], ...] -- pixels read off the frame
(ffmpeg -vf "select=eq(n\\,<frame>)"), <number> a fixed lane (1, 5 or 9), <id> an actor id or "player" (C's taken
actor after the takeover, D's seventh), standing still: walking or running, the head's shadow is not told from the
arms' and legs'. Prints each sample and the mean as group.json wants it (GTA axes: x east, y north, z up; the way the
light goes, from the sun down), with how far the samples lie off it.

beach-demo-113 (2026-10-18), ten people in lanes 1 and 9 from 5 s to 115 s: the sun 56.3 degrees up, 162.8 from north
toward east, every sample within 5.2 degrees of the mean (median 3.7); 109's shadows (recorded the day before) lie on
113's to the pixel. Hold a measured sun to its hour, weather and place (sun_lighting refuses another).
"""
import json
import math
import sys
from pathlib import Path

TAKE = {1: "AB", 5: "C", 9: "D"}


def _rotate(q, v):
    x, y, z, w = q
    ix, iy = w * v[0] + y * v[2] - z * v[1], w * v[1] + z * v[0] - x * v[2]
    iz, iw = w * v[2] + x * v[1] - y * v[0], -x * v[0] - y * v[1] - z * v[2]
    return [ix * w - iw * x - iy * z + iz * y, iy * w - iw * y - iz * x + ix * z, iz * w - iw * z - ix * y + iy * x]


def camera_ray(record, u, v):
    """the delivered camera (y up, left-handed, looking along its local +z) and the unit ray through pixel (u, v)"""
    q, i = record["camera_rotation_quaternion"], record["camera_intrinsics"]
    R, U, F = (_rotate(q, a) for a in ([1, 0, 0], [0, 1, 0], [0, 0, 1]))
    x, y = (u - i["cx"]) / i["fx"], -(v - i["cy"]) / i["fy"]
    d = [F[j] + x * R[j] + y * U[j] for j in range(3)]
    n = math.sqrt(sum(c * c for c in d))
    return record["camera_position"], [c / n for c in d]


def height_over(C, d, px, pz):
    """the height where the ray passes nearest the vertical line at (px, pz)"""
    t = ((px - C[0]) * d[0] + (pz - C[2]) * d[2]) / (d[0] ** 2 + d[2] ** 2)
    return C[1] + t * d[1]


def on_level(C, d, h):
    t = (h - C[1]) / d[1]
    return [C[j] + t * d[j] for j in range(3)]


def main(argv) -> int:
    package, group, samples = Path(argv[0]), Path(argv[1]), json.loads(Path(argv[2]).read_text(encoding="utf-8"))
    cams, worlds, dirs = {}, {}, []
    for key, top_px, tip_px in samples:
        number, frame, who = key.split(":")
        number, frame = int(number), int(frame)
        if number not in cams:
            cams[number] = json.loads((package / f"action_camera_{number}.json").read_text(encoding="utf-8"))
            worlds[number] = {json.loads(line)["tick"]: json.loads(line) for line in
                              (group / "takes" / TAKE[number] / "source" / "master-world-telemetry.jsonl").open(encoding="utf-8")}
        row = worlds[number][frame]   # video frame f is stage tick f
        bones = row["player"]["bones"] if who == "player" else next(a["state"]["bones"] for a in row["actors"] if a["id"] == who)
        hx, hz = bones[2][0], bones[2][1]                    # the head bone; delivered x, z = GTA x, y
        sand = min(bones[5][2], bones[6][2]) - 0.07          # the foot bones stand 7 cm over the sand
        record = cams[number][frame]
        top = height_over(*camera_ray(record, *top_px), hx, hz)
        tip = on_level(*camera_ray(record, *tip_px), sand)
        d = [tip[0] - hx, tip[1] - top, tip[2] - hz]
        n = math.sqrt(sum(c * c for c in d))
        d = [c / n for c in d]
        dirs.append(d)
        print(f"{key:16s} head top {top - sand:.2f} m up: sun {math.degrees(math.asin(-d[1])):.1f} deg up, "
              f"{math.degrees(math.atan2(-d[0], -d[2])) % 360:.1f} from north")
    m = [sum(d[i] for d in dirs) / len(dirs) for i in range(3)]
    n = math.sqrt(sum(c * c for c in m))
    m = [c / n for c in m]
    off = sorted(math.degrees(math.acos(max(-1.0, min(1.0, sum(a * b for a, b in zip(d, m)))))) for d in dirs)
    print(f"mean: sun {math.degrees(math.asin(-m[1])):.1f} deg up, {math.degrees(math.atan2(-m[0], -m[2])) % 360:.1f} from "
          f"north; samples off it median {off[len(off) // 2]:.1f}, max {off[-1]:.1f} deg (n={len(dirs)})")
    print(json.dumps({"light": [round(m[0], 4), round(m[2], 4), round(m[1], 4)], "intensity": 1.0}))   # GTA axes
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
