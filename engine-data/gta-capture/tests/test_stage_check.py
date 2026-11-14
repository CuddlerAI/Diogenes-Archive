import argparse
import io
import json
import math
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

from gta_capture import cli, stage_tape
from gta_capture.errors import ContractError
from gta_capture.stage_camera_paths import ROOT_ABOVE_SAND_M, stage_world
from gta_capture.stage_check import (BONE_CLEARANCE_M, CHECK_SCHEMA, PLAN_WORLD_SCHEMA, ROOT_CLEARANCE_M,
                                     against_recorded, check_clearance, check_fixed, check_script, check_set,
                                     check_stage, check_stage_script, dump_plan_world, read_plan_world)

FRAME = (-1574.1906, -1146.0808, 1.87, 104.97)
TICKS = 1200   # 40 s: long enough that a free camera owes all six ways
SAND = 1.55


def person(xs: float, ys: float, *, hand: tuple[float, float] | None = None) -> dict:
    """Somebody standing at stage (xs, ys): root, spine, head, two hands, two feet."""
    x, y = stage_world(FRAME, xs, ys)
    bones = [[x, y, SAND + 0.95], [x, y, SAND + 1.25], [x, y, SAND + 1.6], [x - 0.25, y, SAND + 1.0],
             [x + 0.25, y, SAND + 1.0], [x - 0.12, y, SAND + 0.08], [x + 0.12, y, SAND + 0.08]]
    if hand:
        bones[4] = [*stage_world(FRAME, *hand), SAND + 1.0]
    return {"pos": [x, y, SAND + ROOT_ABOVE_SAND_M], "h": 0.0, "phase": 0.0, "bones": bones}


def cast(k: int) -> dict[str, dict]:
    """Six people round the stage: two standing, four walking loops of 2-4 m."""
    t = k / 30.0
    out = {"a0": person(-2.8, 1.4), "a1": person(3.4, 0.2)}
    for i, (r, cx, cy, w) in enumerate(((1.0, -0.5, 3.6, 0.5), (0.8, 0.8, -1.2, -0.35), (0.7, -3.2, -1.0, 0.3),
                                        (0.8, 2.6, 3.4, 0.45))):
        a = w * t + i
        out[f"a{i + 2}"] = person(cx + r * math.cos(a), cy + r * math.sin(a))
    return out


def plan_world(at=cast, *, ticks: int = TICKS, sets=(), beats=None, absent=()) -> list[str]:
    ids = sorted(at(0))
    header = {"frame": list(FRAME), "ticks": ticks, "ground_z": SAND, "sink": 0.0, "sets": list(sets), "notes": [],
              "actors": [{"id": i, "beats": (beats or {}).get(i, [
                  {"kind": "idle", "clip": "stand", "start": 0, "ticks": ticks, "entry_pop": 0.0, "blend_in": 4.0}])}
                         for i in ids]}
    lines = [json.dumps({"schema": PLAN_WORLD_SCHEMA, "header": header})]
    for k in range(ticks):
        lines.append(json.dumps({"schema": PLAN_WORLD_SCHEMA, "tick": k, "actors": [
            {"id": i, "beat": 0, "walking": 0, "state": s} for i, s in at(k).items() if (i, k) not in absent]}))
    return lines


def with_(extra: dict):
    return lambda k: {**cast(k), **{i: at(k) for i, at in extra.items()}}


class StageCheckTest(unittest.TestCase):
    def test_a_script_that_keeps_its_distances_passes(self):
        report = check_stage_script(read_plan_world(plan_world()), seed=7)
        self.assertEqual(report["problems"], [])
        self.assertTrue(report["ok"])
        self.assertEqual((report["schema"], report["scope"]), (CHECK_SCHEMA, "baseline"))
        self.assertEqual([c["id"] for c in report["checks"]],
                         ["script", "clearance", "set", "stage", "fixed", "free", "people", "shared"])
        self.assertTrue(any("played person" in line for line in report["not_judged"]))   # it says what it left out
        free = report["checks"][5]
        self.assertEqual(sorted(free["paths"]), ["2", "3"])

    def test_two_who_walk_through_each_other_are_named(self):
        # a2 crosses the spot a0 stands on, 12 s into the take
        world = read_plan_world(plan_world(with_({"a2": lambda k: person(-2.8 + (k - 360) / 30.0, 1.4)})))
        result = check_clearance(world)
        self.assertFalse(result["ok"])
        self.assertRegex(result["problems"][0], r"a0 and a2 come within 0\.00 m of each other at 12\.0 s")
        self.assertEqual(result["closest"][0]["pair"], ["a0", "a2"])

    def test_a_hand_in_somebody_is_found_where_the_bodies_keep_apart(self):
        apart = ROOT_CLEARANCE_M + 0.1
        world = read_plan_world(plan_world(with_({"a2": lambda k: person(-2.8 + apart, 1.4, hand=(-2.8 + 0.05, 1.4))})))
        result = check_clearance(world)
        self.assertRegex(result["problems"][0], "a0 and a2: a hand, foot or head")
        self.assertLess(result["closest"][0]["bone_m"], BONE_CLEARANCE_M)
        self.assertTrue(check_clearance(read_plan_world(plan_world(
            with_({"a2": lambda k: person(-2.8 + apart, 1.4)}))))["ok"])

    def test_a_thing_takes_less_room_than_a_vehicle(self):
        def piece(models, xs):
            x, y = stage_world(FRAME, xs, 1.4)
            return {"id": "piece", "pos": [x, y, SAND], "h": 0.0, "models": models}
        near, far = -2.8 - 2.0, -2.8 - 1.0   # from a0, who stands at (-2.8, 1.4), away from everybody else
        self.assertTrue(check_set(read_plan_world(plan_world(sets=[piece(["prop_beach_bag_01a"], near)])))["ok"])
        self.assertFalse(check_set(read_plan_world(plan_world(sets=[piece(["prop_beach_bag_01a"], far)])))["ok"])
        vehicle = check_set(read_plan_world(plan_world(sets=[piece(["lguard"], near)])))
        self.assertRegex(vehicle["problems"][0], r"a0 passes 2\.00 m from the middle of piece")
        # one model of several a vehicle: the piece may turn out to be it
        self.assertFalse(check_set(read_plan_world(plan_world(sets=[piece(["prop_beach_bag_01a", "bifta"], near)])))["ok"])

    def test_the_cast_keeps_to_ten_metres(self):
        self.assertTrue(check_stage(read_plan_world(plan_world()))["ok"])
        wide = check_stage(read_plan_world(plan_world(with_({"a2": lambda k: person(-5.0 + k / 100.0, 3.0)}))))
        self.assertRegex(wide["problems"][0], r"the cast covers 12\.0 x")

    def test_an_actor_with_nothing_to_do_is_a_hole_in_the_script(self):
        holes = {("a3", k) for k in range(100, 200)}
        result = check_script(read_plan_world(plan_world(absent=holes)))
        self.assertEqual(result["problems"], ["a3 has nothing to do on ticks 100-199"])

    def test_the_largest_change_of_pose_is_reported(self):
        beats = {"a1": [{"kind": "idle", "clip": "stand", "start": 0, "ticks": 600, "entry_pop": 0.0, "blend_in": 4.0},
                        {"kind": "once", "clip": "wave", "start": 600, "ticks": 600, "entry_pop": 0.42, "blend_in": 4.0}]}
        result = check_script(read_plan_world(plan_world(beats=beats)))
        self.assertEqual(result["largest_pose_step"],
                         {"metres_over_the_bones": 0.42, "actor": "a1", "into": "wave", "tick": 600})

    # stage_tape.FIXED is per group (each group's cameras are found on its own takes): this test pins the tripod layout
    # of 2026-10-16 instead of following whichever group's cameras are checked in
    @mock.patch.dict(stage_tape.FIXED, {"AB": ((-3.5, -6.5, 1.3), (0.0, 2.0, 0.9)),
                                        "C": ((2.5, -12.0, 1.6), (3.0, -5.0, 0.9)),
                                        "D": ((-6.0, -14.5, 1.4), (2.0, 1.0, 0.9))})
    def test_a_fixed_camera_is_a_tripod_that_holds_three_people_and_names_whom_it_loses(self):
        # a5 off stage right, out of A's view (from the front left), in C's and D's (from in front): the rest are still
        # 3 and more -- it passes (the meeting of 10-16: 3 people and more, not the whole cast) and says whom it lost
        result = check_fixed(read_plan_world(plan_world(with_({"a5": lambda k: person(8.0, -2.0)}))))
        self.assertEqual(result["problems"], [])
        self.assertEqual({t: c["cut_off_s"] for t, c in result["cameras"].items()}, {"AB": {"a5": 40.0}, "C": {}, "D": {}})
        self.assertEqual((result["cameras"]["AB"]["everybody_whole"], result["cameras"]["AB"]["3_whole"]), (0.0, 1.0))
        self.assertEqual((result["cameras"]["C"]["height_m"], result["cameras"]["C"]["looking_down_deg"]), (1.6, 5.7))
        # two people: no camera holds three
        two = check_fixed(read_plan_world(plan_world(lambda k: {i: s for i, s in cast(k).items() if i in ("a0", "a1")})))
        self.assertEqual(two["problems"], [f"take {t}'s fixed camera holds 3 people whole 0% of the take (under 95%)"
                                           for t in ("AB", "C", "D")])
        # 003's C: up a pole, looking down on the people
        with mock.patch.dict(stage_tape.FIXED, {"C": ((5.5, -4.0, 3.5), (2.0, 0.0, 0.9))}):
            high = check_fixed(read_plan_world(plan_world()))
        self.assertEqual(high["problems"], ["take C's fixed camera stands 3.5 m up (a tripod: 1.6 m at most)",
                                            "take C's fixed camera looks down 26 degrees (10 at most)"])

    def test_a_recorded_take_holds_the_plan_to_a_millimetre(self):
        world = read_plan_world(plan_world())

        def recorded(shift_m=0.0, released=()):
            lines = []
            for k in range(0, TICKS, 2):   # the game skipped every other tick
                actors = []
                for i, s in cast(k).items():
                    moved = dict(s, pos=[s["pos"][0] + (shift_m if i == "a4" else 0.0), *s["pos"][1:]])
                    actors.append({"id": i, "released": int(i in released), "beat": 0, "state": moved})
                lines.append(json.dumps({"schema": "dm-gta-world-telemetry/1", "tick": k, "actors": actors}))
            return lines
        same = against_recorded(world, recorded())
        self.assertTrue(same["ok"])
        self.assertEqual(same["ticks_compared"]["a4"], TICKS // 2)
        off = against_recorded(world, recorded(shift_m=0.005))
        self.assertRegex(off["problems"][0], r"a4: the take has it 5\.0 mm from the plan")
        # whoever a rule released is somewhere else, as it should be -- and then nobody logged a4 as the plan's
        released = against_recorded(world, recorded(shift_m=2.0, released=("a4",)))
        self.assertEqual(released["problems"], ["a4: the take never logged it"])

    def test_what_a_plan_world_may_not_be(self):
        good = plan_world(ticks=30)
        for lines, why in ((good[1:], "a header"), (good[:-1], "every tick"), (good + [good[-1]], None),
                           ([good[0].replace(PLAN_WORLD_SCHEMA, "dm-gta-world-telemetry/1")] + good[1:], "schema")):
            if why is None:   # the last tick twice is that tick, once
                self.assertEqual(len(read_plan_world(lines).ticks), 30)
                continue
            with self.subTest(why=why), self.assertRaisesRegex(ContractError, why):
                read_plan_world(lines)
        short = json.loads(good[1])
        short["actors"][0]["state"]["bones"].pop()
        with self.assertRaisesRegex(ContractError, "body points"):
            read_plan_world([good[0], json.dumps(short)] + good[2:])

    def test_the_dump_tool_must_be_built(self):
        with tempfile.TemporaryDirectory() as tmp, self.assertRaisesRegex(ContractError, "gta_stage_plan_dump"):
            dump_plan_world(Path(tmp) / "gta_stage_plan_dump", "spec.txt", "calibration.txt")

    def test_the_command_writes_its_report_and_fails_a_script_that_does_not_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            spec, cal, out = Path(tmp) / "spec.txt", Path(tmp) / "cal.txt", Path(tmp) / "report.json"
            spec.write_text("spec", encoding="utf-8")
            cal.write_text("cal", encoding="utf-8")
            args = argparse.Namespace(dump_tool="tool", spec=str(spec), calibration=str(cal), ground_z=None, seed=7,
                                      recorded=None, out=str(out))
            for lines, code in ((plan_world(), 0),
                                (plan_world(with_({"a2": lambda k: person(-2.8, 1.4 + 0.3)})), 1)):
                with self.subTest(code=code), mock.patch.object(cli, "dump_plan_world",
                                                               return_value=read_plan_world(lines)):
                    printed = io.StringIO()
                    with redirect_stdout(printed):
                        self.assertEqual(cli._cmd_stage_spec_check(args), code)
                    shown, report = json.loads(printed.getvalue()), json.loads(out.read_text(encoding="utf-8"))
                    self.assertEqual(shown["ok"], code == 0)
                    self.assertEqual(shown["checks"]["clearance"], code == 0)
                    self.assertEqual(report["problems"], shown["problems"])
                    self.assertEqual(len(report["spec"]["sha256"]), 64)


if __name__ == "__main__":
    unittest.main()
