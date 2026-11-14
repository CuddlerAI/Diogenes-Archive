import json
import math
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from gta_capture import stage_camera_paths, stage_tape
from gta_capture.errors import ContractError
from gta_capture.jsonio import load_json
from gta_capture.stage_camera_paths import (CAMERA_MARGIN_M, CLEARANCE_M, HEADROOM, LIMITS, ROOMS, SIX_WAY_MIN_M,
                                            measure_moves, motion_p95, set_obstacles, six_ways, stage_local, stage_world,
                                            visible_share)
from gta_capture.stage_delivery import path_input_per_frame
from gta_capture.stage_tape import build_take_tape
from gta_capture.tape import _look_at_euler, _quaternion_zxy

FRAME = (-1574.1906, -1146.0808, 1.87, 104.97)
SPEC = f"""dm-gta-stage-spec/1
frame {FRAME[0]} {FRAME[1]} {FRAME[2]} {FRAME[3]}
ticks 1200
"""
TICKS = 1200   # 40 s: long enough that all six ways are owed
ROOT_Z = 2.55
SAND_Z = ROOT_Z - 0.99


def _on_stage(xs: float, ys: float, z: float = ROOT_Z) -> list[float]:
    x, y = stage_world(FRAME, xs, ys)
    return [x, y, z]


def _cast(k: int) -> list[list[float]]:
    """Six people round the stage: two standing, four walking loops of 2-4 m."""
    t = k / 30.0
    out = [_on_stage(-2.8, 1.4), _on_stage(3.4, 0.2)]
    for i, (r, cx, cy, w) in enumerate(((2.0, -1.0, 2.0, 0.5), (3.0, 1.0, 1.0, -0.35), (4.0, 0.0, 1.6, 0.3),
                                        (2.5, 0.5, 3.0, 0.45))):
        a = w * t + i
        out.append(_on_stage(cx + r * math.cos(a), cy + r * math.sin(a)))
    return out


def _played(k: int) -> list[float]:
    t = k / 30.0
    return _on_stage(-3.0 + 6.0 * (0.5 - 0.5 * math.cos(t / 6.0)), -0.5 + math.sin(t / 5.0))


def _facing(k: int) -> float:
    """The played person's GTA heading: the way it walks, turning round at either end."""
    a, b = _played(max(k - 1, 0)), _played(k + 1)
    return math.degrees(math.atan2(-(b[0] - a[0]), b[1] - a[1]))


def write_take(take: Path, kind: str, *, takeover: int = 300, crowd=None, played=None, log_controlled=True) -> None:
    played = played or _played

    def facing(k):
        a, b = played(max(k - 1, 0)), played(k + 1)
        return math.degrees(math.atan2(-(b[0] - a[0]), b[1] - a[1]))

    src = take / "source"
    src.mkdir(parents=True, exist_ok=True)
    with open(src / "master-world-telemetry.jsonl", "w", encoding="utf-8") as f:
        for k in range(TICKS):
            cast = crowd(k) if crowd else _cast(k)
            actors = [{"id": f"a{i}", "released": 0, "beat": 0,
                       "state": {"pos": p, "h": 0.0, "bones": [[p[0], p[1], p[2] + 0.1 * b] for b in range(7)]}}
                      for i, p in enumerate(cast)]
            player = {"pos": played(k), "h": facing(k)} if kind in ("C", "D") else None
            f.write(json.dumps({"schema": "dm-gta-world-telemetry/1", "tick": k, "actors": actors, "props": [],
                                "player": player}) + "\n")
    with open(src / "input-camera-log.jsonl", "w", encoding="utf-8") as f:
        for k in range(TICKS):
            p = played(k)
            row = {"schema": "dm-gta-input-camera-log/1", "tick": k, "game_ms": k * 33, "frames": 2, "keyCode": [],
                   "mouse_dx": 0.0, "mouse_dy": 0.0, "events": [],
                   "game_cam": {"pos": [p[0], p[1] - 4.0, p[2] + 1.5], "rot": [-10.0, 0.0, 0.0], "fov": 50}}
            if kind == "AB":
                row["free_cam"] = {"pos": _on_stage(0.0, -8.0, 3.3), "rot": [-5.0, 0.0, 0.0], "fov": 50}
            elif log_controlled and (kind == "D" or k >= takeover):
                row["controlled"] = {"id": "seventh" if kind == "D" else "a2",
                                     "head": [p[0], p[1], p[2] + 0.65], "pos": p, "h": facing(k)}
            f.write(json.dumps(row) + "\n")


def people_per_tick(kind: str):
    return [_cast(k) + ([_played(k)] if kind in ("C", "D") else []) for k in range(TICKS)]


def _poses(tape, lane):
    samples = tape["lanes"][lane]["samples"][tape["source"]["hold_frames"]:]
    return [(s["position"], s["euler_deg"]) for s in samples]


def _keys(tape, lane):
    samples = tape["lanes"][lane]["samples"][tape["source"]["hold_frames"]:]
    return path_input_per_frame([s["position"] for s in samples], [s["quaternion"] for s in samples])


class FreePathTests(unittest.TestCase):
    def test_worldline_a_free_paths_travel_six_ways_keep_clear_and_differ(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "AB"
            write_take(take, "AB")
            tape = build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=11)
            self.assertEqual(tape["source"]["roles"], {"1": "fixed", "2": "free_path", "3": "free_path", "4": "free_camera"})
            people = people_per_tick("AB")
            focus = load_json(take / "config" / "camera-focus.json")
            paths = {}
            for lane in ("2", "3"):
                poses = _poses(tape, lane)
                paths[lane] = [p for p, _ in poses]
                ways = six_ways(poses)
                self.assertTrue(all(v >= SIX_WAY_MIN_M for v in ways.values()), ways)
                p95 = motion_p95(poses)
                for key, limit in LIMITS.items():
                    self.assertLessEqual(p95[key], HEADROOM * limit, key)
                nearest = min(math.dist(p, q) for (p, _), who in zip(poses, people) for q in who)
                self.assertGreaterEqual(nearest, CLEARANCE_M)
                report = tape["source"]["free_paths"][lane]
                self.assertEqual(report["focus"], "centroid")
                self.assertTrue(1 <= len(report["segments"]) <= 10)
                kinds = {s["kind"] for s in report["segments"]}
                self.assertTrue({"orbit_left", "orbit_right"} <= kinds, kinds)
                # worldline A keeps to the front and the sides of the stage, round the cast's middle
                for (p, _), c in zip(poses, focus["lanes"][lane]["frames"][30:]):
                    cx, cy = stage_local(FRAME, c[0], c[1])
                    x, y = stage_local(FRAME, p[0], p[1])
                    self.assertLessEqual(abs(math.degrees(math.atan2(x - cx, cy - y))), ROOMS["centroid_wide"].az[1] + 1e-6)
            self.assertNotEqual(paths["2"], paths["3"])   # two cameras, two paths
            self.assertEqual(focus["n_frames"], tape["n_frames"])
            self.assertEqual(set(focus["lanes"]), {"2", "3"})
            self.assertEqual(len(focus["lanes"]["2"]["frames"]), tape["n_frames"])

    def test_a_free_path_moves_all_take_at_a_speed_its_keys_show(self):
        """The customer, 2026-10-07: keys with no motion to see, an E key with no rise to see. A free camera moves
        all take long, so its keys are down nearly all of it; an E or a Q is held over a rise or sink of 1 m and more."""
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "AB"
            write_take(take, "AB")
            tape = build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=11)
            for lane in ("2", "3"):
                poses, keys = _poses(tape, lane), _keys(tape, lane)
                self.assertGreater(sum(1 for r in keys if r["keyCode"]) / len(keys), 0.9)
                still = sum(1 for i in range(len(poses) - 1) if math.dist(poses[i + 1][0], poses[i][0]) * 30 < 0.05)
                self.assertLess(still / len(poses), 0.03)
                ways = set()
                for code in (69, 81):   # E, Q
                    held = [code in r["keyCode"] for r in keys] + [False]
                    start = None
                    for i, down in enumerate(held):
                        if down and start is None:
                            start = i
                        elif not down and start is not None:
                            self.assertGreaterEqual(abs(poses[i - 1][0][2] - poses[start][0][2]), 1.0)
                            ways.add(code)
                            start = None
                self.assertEqual(ways, {69, 81})   # up and down, both

    def test_the_same_take_and_seed_build_the_same_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "AB"
            write_take(take, "AB")
            one = build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=5)
            two = build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=5)
            other = build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=6)
            self.assertEqual(one["lanes"]["2"]["samples"], two["lanes"]["2"]["samples"])
            self.assertNotEqual(one["lanes"]["2"]["samples"], other["lanes"]["2"]["samples"])

    def test_c_watches_the_event_and_d_goes_round_the_seventh_person(self):
        """The meeting of 2026-10-16: a free camera watches the event where there is one (C: the tourist stopped) and
        goes round the played person where there is none (D: the seventh person only looks on) -- not the cast's
        middle in every case. C's event is the played person and whoever is answering him; nobody answers here, so
        it is him alone, smoothed."""
        self.assertEqual(stage_tape.FREE_PATHS[("C", "2")], ("event", "centroid_tele"))
        self.assertEqual(stage_tape.FREE_PATHS[("D", "2")], ("player", "centroid_tele"))
        with tempfile.TemporaryDirectory() as tmp:
            for kind in ("C", "D"):
                take = Path(tmp) / kind
                write_take(take, kind)
                tape = build_take_tape(take, spec_text=SPEC, take_kind=kind, hold_s=1.0, duration_s=41.0, seed=3,
                                       takeover_actor="a2")
                report = tape["source"]["free_paths"]["2"]
                self.assertEqual(report["focus"], "event" if kind == "C" else "player")
                poses = _poses(tape, "2")
                focus = load_json(take / "config" / "camera-focus.json")["lanes"]["2"]["frames"][30:]
                self.assertGreater(visible_share(poses, [[c[0], c[1], c[2] + 0.3] for c in focus], 50), 0.8)
                nearest = min(math.dist(p, q) for (p, _), who in zip(poses, people_per_tick(kind)) for q in who)
                self.assertGreaterEqual(nearest, CLEARANCE_M)
                if kind == "C":   # in front of the event and at its sides (D's room goes round its placement anchor)
                    for (p, _), c in zip(poses, focus):
                        cx, cy = stage_local(FRAME, c[0], c[1])
                        x, y = stage_local(FRAME, p[0], p[1])
                        self.assertLessEqual(abs(math.degrees(math.atan2(x - cx, cy - y))), ROOMS["centroid_tele"].az[1] + 1e-6)

    # D2's focus kind since 2026-10-16 (beach-demo-006's D2 was taped with it too, then the cast's middle on 10-15).
    def test_a_player_focus_follows_the_seventh_person_outside_the_centroid_clamp(self):
        def seventh(k):
            t = k / 30.0
            return _on_stage(5.0 + 0.7 * math.sin(t / 6.0), -2.0 + 0.5 * math.cos(t / 5.0))

        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "D"
            write_take(take, "D", played=seventh)
            tape = build_take_tape(take, spec_text=SPEC, take_kind="D", hold_s=1.0, duration_s=41.0, seed=3)
            focus = load_json(take / "config" / "camera-focus.json")["lanes"]["2"]
            self.assertEqual(focus["kind"], "player")
            self.assertEqual(tape["source"]["free_paths"]["2"]["focus"], "player")
            self.assertEqual(tape["lanes"]["2"]["focal_mm"], 50)
            self.assertEqual(focus["frames"][:30], [[round(v, 6) for v in seventh(0)]] * 30)
            for k, point in enumerate(focus["frames"][30:]):
                self.assertEqual(point, [round(v, 6) for v in seventh(k)])
                x, y = stage_local(FRAME, *point[:2])
                self.assertGreater(math.hypot(x, y - 1.6), stage_tape.CENTROID_REACH_M)
            poses = _poses(tape, "2")
            # This is the seventh person's own target, never the union with the cast or stage.
            targets = [[*seventh(k)[:2], seventh(k)[2] + 0.3] for k in range(TICKS)]
            self.assertGreaterEqual(visible_share(poses, targets, 50), 0.8)
            nearest = min(math.dist(p, q) for k, (p, _) in enumerate(poses) for q in _cast(k) + [seventh(k)])
            self.assertGreaterEqual(nearest, CLEARANCE_M)
            self.assertTrue(all(v >= SIX_WAY_MIN_M for v in six_ways(poses).values()))
            for key, value in motion_p95(poses).items():
                self.assertLessEqual(value, HEADROOM * LIMITS[key], key)

    def test_d_focus_change_preserves_every_other_camera_sample(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "D"
            write_take(take, "D")
            old = build_take_tape(take, spec_text=SPEC, take_kind="D", hold_s=1.0, duration_s=41.0, seed=3)
            with mock.patch.dict(stage_tape.FREE_PATHS, {("D", "2"): ("centroid", "centroid_tele")}):
                new = build_take_tape(take, spec_text=SPEC, take_kind="D", hold_s=1.0, duration_s=41.0, seed=3)
            for lane in ("1", "3", "4"):
                self.assertEqual(old["lanes"][lane], new["lanes"][lane])
            self.assertNotEqual(old["lanes"]["2"]["samples"], new["lanes"]["2"]["samples"])

    def test_d_without_a_logged_controlled_person_cannot_fall_back_to_the_cast(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "D"
            write_take(take, "D", log_controlled=False)
            with self.assertRaisesRegex(ContractError, "no controlled seventh person"):
                build_take_tape(take, spec_text=SPEC, take_kind="D", hold_s=1.0, duration_s=41.0, seed=3)

    def test_a_set_piece_is_kept_clear_of(self):
        """A car the spec sets in the middle of where the cameras go: no path comes nearer than its size and the
        margin, and nothing passes over it lower than its top and the margin."""
        car = "set car lguard 0.0 -7.0 90\n"
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "AB"
            write_take(take, "AB")
            tape = build_take_tape(take, spec_text=SPEC + car, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=11)
            [(x, y, r, top)] = set_obstacles(SPEC + car, FRAME)
            for lane in ("2", "3"):
                for p, _ in _poses(tape, lane):
                    self.assertTrue(math.hypot(p[0] - x, p[1] - y) >= r + CAMERA_MARGIN_M
                                    or p[2] >= SAND_Z + top + CAMERA_MARGIN_M - 1e-6)

    def test_set_pieces_by_their_models(self):
        spec = ("set lifeguard lguard -7.5 13.0 90\n"
                "set parasol prop_beach_parasol_03|prop_beach_parasol_01 -3.0 10.5 0\n"
                "set towel prop_beach_towel_02|prop_beach_towel_01 -3.4 9.6 15\n"
                "set cooler prop_coolbox_01|prop_cooler_01 4.2 8.5 70  # a comment\n")
        pieces = set_obstacles(spec, FRAME)
        self.assertEqual([p[2:] for p in pieces], [(2.6, 2.6), (1.2, 2.6), (0.4, 0.6)])   # the towel lies flat
        self.assertAlmostEqual(math.dist(pieces[0][:2], stage_world(FRAME, -7.5, 13.0)), 0.0)

    def test_a_take_with_no_room_for_a_camera_fails_loudly(self):
        def crowd(k):   # people standing everywhere a camera could go
            return [_on_stage(x, y) for x in range(-16, 17, 2) for y in range(-16, 17, 2)]
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "AB"
            write_take(take, "AB", crowd=crowd)
            with mock.patch.object(stage_camera_paths, "MAX_TRIES", 5), self.assertRaises(ContractError) as caught:
                build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=41.0, seed=1)
            self.assertIn("from a person", str(caught.exception))


def _orbit(degrees: float, *, rise: float = 0.0, seconds: float = 10.0, radius: float = 6.0):
    """A camera going round a still focus at the origin, starting due south of it, looking at it."""
    n = int(seconds * 30)
    focus = [[0.0, 0.0, 1.0]] * n
    look = [[0.0, 0.0, 1.4]] * n
    poses = []
    for k in range(n):
        a = math.radians(-90.0 + degrees * k / (n - 1))
        pos = [radius * math.cos(a), radius * math.sin(a), 3.0 + rise * k / (n - 1)]
        pitch, _, yaw = _look_at_euler(pos, look[k])
        poses.append((pos, [pitch, 0.0, yaw]))
    return poses, focus, look, n


class MeasureTests(unittest.TestCase):
    def _moves(self, poses, focus, look, n):
        [seg] = measure_moves(poses, focus, look, [(0, n)])
        return {m["move"]: m for m in seg["moves"]}

    def test_going_clockwise_seen_from_above_is_an_orbit_to_the_left_named_by_its_angle(self):
        moves = self._moves(*_orbit(-120.0))
        self.assertEqual(moves["orbit_mid"]["label"], "环绕-中幅环绕：90°至180°")
        self.assertEqual(moves["orbit_mid"]["detail"], "向左")
        self.assertAlmostEqual(moves["orbit_mid"]["amount"], 120.0, delta=0.5)
        self.assertEqual(moves["orbit_mid"]["unit"], "°")
        self.assertNotIn("left", moves)   # going round is the orbit, not a translation besides
        self.assertEqual(self._moves(*_orbit(75.0))["orbit_small"]["detail"], "向右")
        for degrees, kind in ((200.0, "orbit_large"), (400.0, "orbit_round"), (800.0, "orbit_rounds")):
            self.assertIn(kind, self._moves(*_orbit(degrees, seconds=30.0)))

    def test_coming_nearer_while_going_round_is_the_orbits_pull_in(self):
        poses, focus, look, n = _orbit(150.0)
        nearer = [([p[0] * (1 - 0.3 * k / n), p[1] * (1 - 0.3 * k / n), p[2]], r) for k, (p, r) in enumerate(poses)]
        moves = self._moves(nearer, focus, look, n)
        self.assertEqual(moves["orbit_nearer"]["label"], "环绕-环绕拉近")   # 1.8 m in, round the focus
        self.assertNotIn("forward", moves)

    def test_a_rise_while_going_round_is_a_spiral_and_a_straight_rise_a_rise_and_a_tilt(self):
        moves = self._moves(*_orbit(90.0, rise=1.6))
        self.assertEqual(set(moves), {"spiral_up"})
        self.assertEqual(moves["spiral_up"]["label"], "螺旋升降-螺旋上升")
        self.assertAlmostEqual(moves["spiral_up"]["amount"], 1.6, delta=0.01)
        self.assertIn("spiral_down", self._moves(*_orbit(90.0, rise=-1.6)))
        moves = self._moves(*_orbit(0.0, rise=2.0, seconds=4.0))
        self.assertEqual(set(moves), {"up", "tilt_down"})   # looking at the focus from higher up: the view tilts down
        self.assertEqual(moves["tilt_down"]["label"], "摇动-基础上摇／下摇／左摇／右摇")
        self.assertEqual(moves["tilt_down"]["detail"], "下摇")

    def test_a_pan_off_the_cast_and_back_is_a_plain_pan(self):
        """A cast-centroid pan does not establish the person-specific follow or pan-return categories."""
        poses, focus, look, n = _orbit(0.0, seconds=6.0)   # standing still, looking at the focus ...
        off = [min(k, n - k) for k in range(n)]           # ... panning left off it and back
        panned = [(p, [r[0], 0.0, r[2] + min(90.0, o * 0.5)]) for (p, r), o in zip(poses, off)]
        moves = self._moves(panned, focus, look, n)
        self.assertEqual(set(moves), {"pan_left"})
        self.assertEqual((moves["pan_left"]["label"], moves["pan_left"]["detail"]), ("摇动-基础上摇／下摇／左摇／右摇", "左摇"))

    def test_small_moves_are_not_named(self):
        poses, focus, look, n = _orbit(8.0, rise=0.5)   # under a metre each way, under 10 degrees of tilt
        self.assertEqual(self._moves(poses, focus, look, n), {})
        self.assertEqual(set(self._moves(*_orbit(20.0))), {"right"})   # 2 m round: a move to the right, no orbit yet

    def test_an_orbit_to_the_left_is_the_a_key_on_the_tapes_quaternions(self):
        poses, *_ = _orbit(-120.0)
        keys = path_input_per_frame([p for p, _ in poses], [_quaternion_zxy(r) for _, r in poses])
        held = sum(1 for r in keys if 65 in r["keyCode"])   # A: going left
        self.assertGreater(held / len(keys), 0.9)


if __name__ == "__main__":
    unittest.main()
