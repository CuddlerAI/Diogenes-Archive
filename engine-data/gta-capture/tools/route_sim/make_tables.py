"""cast.csv and bones.csv for route_sim, from AB's world telemetry (the cast plays the same script in C and D).

    python make_tables.py <AB master-world-telemetry.jsonl> <stage spec> <out dir>

Before AB is recorded -- or away from the capture machine -- the plan's own telemetry does as well:
gta_stage_plan_dump <stage spec> <calibration> > plan-world.jsonl (native/src/stage_plan_dump.cpp; its
positions are 003's recorded AB to 0.2 mm, its bones to a hand's width) is read the same way, header and all.

cast.csv   one row per tick (3600), stage axes, metres and degrees:
           chat_a x y, chat_b x y, tourist x y, jogger x y, sitter x y, pacer x y heading   (13 numbers)
bones.csv  one row per tick: for each of chat_a chat_b tourist jogger sitter, 7 bones x (dx dy z) in the order
           root spine3 head hand_l hand_r foot_l foot_r; dx dy are stage-axis offsets from the person's root,
           z is the height over the sand.
bones_pacer.csv  the same 7 bones for the pacer: in D he is one more person the seventh sees (route_sim KIND=D).
A tick the telemetry skipped keeps the one before. The stage frame is the spec's `frame` line."""
import json, math, statistics, sys
from pathlib import Path

IDS = ["chat_a", "chat_b", "tourist", "jogger", "sitter"]
PLAYED = "pacer"   # the actor C takes over: the cast row's last three numbers
TICKS = 3600
ROOT_ABOVE_SAND_M = 0.99   # a standing root bone: the same sand height stage_tape and lanecheck use


def stage_frame(spec_text: str):
    for line in spec_text.splitlines():
        words = line.split("#", 1)[0].split()
        if words and words[0] == "frame" and len(words) == 5:
            return tuple(float(w) for w in words[1:])   # x, y, ground z, heading
    raise SystemExit("the stage spec has no frame line")


def build(world_lines, frame):
    ox, oy, _, heading = frame
    c, s = math.cos(math.radians(heading)), math.sin(math.radians(heading))
    def to_stage(x, y): return c * (x - ox) + s * (y - oy), -s * (x - ox) + c * (y - oy)   # R(-heading)
    ticks, roots = {}, []
    for line in world_lines:
        r = json.loads(line)
        if "actors" not in r:   # the plan dump's header line
            continue
        row = {a["id"]: a["state"] for a in r["actors"] if a.get("state")}
        if all(i in row for i in IDS + [PLAYED]):
            ticks[int(r["tick"])] = row
            roots += [row[i]["pos"][2] for i in IDS if i in ("chat_a", "chat_b", "sitter")]
    if not ticks:
        raise SystemExit("no tick of the telemetry has the whole cast")
    sand = statistics.median(roots) - ROOT_ABOVE_SAND_M
    cast, bones, pacer, last = [], [], [], ticks[min(ticks)]
    for k in range(TICKS):
        row = ticks.get(k, last)
        last = row
        vals = []
        for i in IDS:
            vals += to_stage(*row[i]["pos"][:2])
        px, py = to_stage(*row[PLAYED]["pos"][:2])
        vals += [px, py, (row[PLAYED]["h"] - heading) % 360.0]
        cast.append(" ".join(f"{v:.4f}" for v in vals))
        def offsets(s_):
            b = []
            for bone in s_["bones"]:
                dx, dy = to_stage(bone[0] - s_["pos"][0] + ox, bone[1] - s_["pos"][1] + oy)
                b += [dx, dy, bone[2] - sand]
            return b
        bones.append(" ".join(f"{v:.3f}" for i in IDS for v in offsets(row[i])))
        pacer.append(" ".join(f"{v:.3f}" for v in offsets(row[PLAYED])))
    return cast, bones, pacer, sand


def main(argv):
    if len(argv) != 4:
        raise SystemExit(__doc__)
    world, spec, out = Path(argv[1]), Path(argv[2]), Path(argv[3])
    cast, bones, pacer, sand = build(world.read_text(encoding="utf-8").splitlines(), stage_frame(spec.read_text(encoding="utf-8")))
    out.mkdir(parents=True, exist_ok=True)
    (out / "cast.csv").write_text("\n".join(cast) + "\n", encoding="utf-8")
    (out / "bones.csv").write_text("\n".join(bones) + "\n", encoding="utf-8")
    (out / "bones_pacer.csv").write_text("\n".join(pacer) + "\n", encoding="utf-8")
    print(f"{out}: cast.csv, bones.csv and bones_pacer.csv, {TICKS} ticks, sand at {sand:.3f} m")


if __name__ == "__main__":
    main(sys.argv)
