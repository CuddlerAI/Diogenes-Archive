import json
import math
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from gta_capture import stage_delivery
from gta_capture.errors import ContractError
from gta_capture.jsonio import sha256_file, write_json_atomic
from gta_capture.stage_delivery import KEY_MAPPING, check_take, frame_times, input_per_frame, key_mapping, standing_turn_zh, take_records

N, HOLD = 12, 3   # tape frames and the hold before stage tick 0; delivered: frame f = tick f
M = N - HOLD
ROLES = {"AB": {"1": "fixed", "2": "free_path", "3": "free_path", "4": "free_camera"},
         "C": {"1": "fixed", "2": "free_path", "3": "chase", "4": "first_person"},
         "D": {"1": "fixed", "2": "free_path", "3": "chase", "4": "first_person"}}
CLICK = "按鼠标左键：攻击/开火"
NAME = "2026_09_22_银行门口_001"


def _jsonl(path: Path, rows) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")


PLAN = """dm-gta-stage-plan/1
frame 0 0 0 0
ticks 9
actor chat_a beats 2
  idle chat_idle start 0 ticks 6 phase0 0 cycle 30 path -1 speed 0 origin 1 2 0 heading 90 ground 0 entry_pop 0 blend 0
  once wave start 6 ticks 30 phase0 0 cycle 30 path -1 speed 0 origin 1 2 0 heading 90 ground 0 entry_pop 0 blend 0.25
actor pacer beats 1
  walk walk_fast start 0 ticks 9 phase0 0 cycle 30 path 0 speed 1.4 origin 0 0 0 heading 0 ground 0 entry_pop 0 blend 0
actor jogger beats 1
  walk jog start 0 ticks 9 phase0 0 cycle 20 path 1 speed 2.5 origin 0 0 0 heading 0 ground 0 entry_pop 0 blend 0
"""
PHONE = "pacer:p_amb_phone_01"


def world_row(kind: str, k: int) -> dict:
    """The world as a take logs it a stage tick (dm-gta-world-telemetry/1): chat_a turns to its second beat at tick 6;
    in C the person takes pacer over at tick 2 and the jogger, blocked, is off its script ticks 5-7; D's seventh
    person is the player from tick 0."""
    def state(x, y, h, phase=None):
        st = {"pos": [x, y, 0.5], "h": h, "bones": [[x, y, 1.0 + b / 10] for b in range(7)]}
        return st if phase is None else {**st, "phase": phase}
    pacer = state(0.0, 0.5 * k, 0.0, (k % 30) / 30)
    released = kind == "C" and 5 <= k <= 7
    row = {"schema": "dm-gta-world-telemetry/1", "tick": k, "game_ms": 150 * k, "held": 1, "skipped_before": int(k == 5),
           "actors": [{"id": "chat_a", "released": 0, "beat": 0 if k < 6 else 1, "state": state(1.0, 2.0, 90.0, k / 30)},
                      {"id": "pacer", "released": 0, "beat": 0, "state": pacer},
                      {"id": "jogger", "released": int(released), "beat": 0, "state": state(5.0, -1.0 + k, 180.0, k / 20)}],
           "props": [{"id": PHONE, "state": {"pos": [0.2, 0.5 * k, 1.4], "h": 10.0}}],
           # the pier's wheel, turning 1 degree a tick about GTA's x axis (its axle)
           "map": [{"id": "wheel", "state": {"pos": [-90.0, 20.0, 15.0],
                                             "quat": [math.sin(math.radians(k) / 2), 0.0, 0.0, math.cos(math.radians(k) / 2)]}}],
           "env": {"wind_speed": 3.0, "wind_dir": [1.0, 0.0, 0.0]},
           "player": None}
    if kind == "C" and k >= 2:
        row["player"] = pacer
    if kind == "D":
        row["player"] = state(-3.0 + 0.1 * k, 4.0, 270.0)
    return row


def make_take(root: Path, kind: str, *, wall: bool = True, events=None) -> Path:
    take = root / "takes" / kind
    write_json_atomic(take / "config" / "camera-tape.json", {
        "n_frames": N, "source": {"hold_frames": HOLD, "roles": ROLES[kind]},
        "lanes": {str(i): {"resolution": {"width": 1920, "height": 1080}, "focal_mm": f,
                           "samples": [{"position": [10.0 * i + k, 2.0, 3.0]} for k in range(N)]}
                  for i, f in zip(range(1, 5), (35, 24, 50, 18))}})
    write_json_atomic(take / "config" / "camera-focus.json", {
        "schema": "dm-gta-stage-camera-focus/1", "n_frames": N,
        "lanes": {lane: {"kind": "player" if kind == "D" else "centroid",
                          "frames": [[1.0, 2.0 + f, 0.5] if kind == "D" else [5.0, 1.0 + f, 0.0]
                                     for f in range(N)]}
                  for lane, role in ROLES[kind].items() if role == "free_path"}})
    for lane in range(1, 5):
        rows = [{"output_frame": f, "source_t_ns": (f * 1_000_000_000 + 15) // 30,
                 "final_camera": {"position": [10.0 * lane + f, 2.0, 3.0], "quaternion": [0.0, 0.0, 0.0, 1.0],
                                  "intrinsics": {"fx": 900.0, "fy": 900.0, "cx": 960.0, "cy": 540.0}},
                 "player_state": {"position": [1.0, 2.0 + f, 0.5], "quaternion": [0.0, 0.0, 0.0, 1.0]}}
                for f in range(N)]
        path = take / "evidence" / f"frame-events-lane{lane}.jsonl"
        _jsonl(path, rows)
        write_json_atomic(take / "evidence" / f"native-lane-{lane}-result.json", {
            "ok": True, "pixel_check": {"pointer_frames": 0},
            "lane": {"frame_pattern": "frame_%06d.png", "frame_count": N, "frame_events_sha256": sha256_file(path)}})
        (take / "capture").mkdir(parents=True, exist_ok=True)
        (take / "capture" / f"video_{lane}.mp4").write_bytes(f"video {kind} {lane}".encode())
    if events is None:   # C: three clicks in a row, then E
        events = {1: [CLICK], 2: [CLICK], 3: [CLICK], 6: ["按 E：互动"]} if kind == "C" else {}
    logged = [k for k in range(M) if k != 4]   # the game skipped tick 4
    ticks = [{"schema": "dm-gta-input-camera-log/1", "tick": k, "game_ms": 150 * k,
              "frames": 50 if i == 0 else 9 * (k - logged[i - 1]),   # 60 fps; tick 0 counts the hold too
              "keyCode": [87] if k >= 2 else [], "mouse_dx": 3.0 if k == 1 else 0.25, "mouse_dy": -0.5,
              "events": events.get(k, [])}
             for i, k in enumerate(logged)]
    if wall:
        for t in ticks:
            t["wall"] = f"2026-09-23 15:00:00.{34 * t['tick']:03d}"
    _jsonl(take / "source" / "input-camera-log.jsonl", ticks)
    _jsonl(take / "source" / "master-world-telemetry.jsonl", [world_row(kind, k) for k in logged])
    (take / "source" / "stage-plan.txt").write_text(PLAN, encoding="utf-8")
    _jsonl(take / "source" / "rule-events.jsonl", {
        "AB": [],
        "C": [{"tick": 2, "rule": "takeover", "when": "take", "subject": "pacer", "say": "接管 pacer"},
              {"tick": 5, "rule": "block_jogger", "when": "near", "subject": "jogger", "say": "玩家挡住慢跑者的路，慢跑者停下"}],
        "D": [{"tick": 0, "rule": "seventh", "when": "take", "subject": "seventh", "say": "第 7 人登场"}]}[kind])
    write_json_atomic(take / "evidence" / "stage-take-result.json", {"ok": True, "take": {
        "schema": "dm-gta-stage-take/1", "take_kind": kind,
        "takeover": {"actor": "pacer", "tick": 2, "ok": True, "view_mode_restores": 0, "body_turns": 4} if kind == "C" else None,
        "seventh": {"model": "a_m_y_beach_02", "ok": True, "view_mode_restores": 0, "body_turns": 4} if kind == "D" else None}})
    return take


def make_group(root: Path, **takes) -> Path:
    for kind in ("AB", "C", "D"):
        make_take(root, kind, **takes.get(kind, {}))
    (root / "stage.txt").write_text("frame 0 0 0 0\n", encoding="utf-8")
    write_json_atomic(root / "group.json", {"group_id": "g1", "stage_spec": "stage.txt", "delivery_name": NAME,
                                            "environment": {"record_dpi": 1.5}})
    return root


def _log(ticks, **fields):
    return [{"schema": "dm-gta-input-camera-log/1", "tick": k, **{key: v(k) for key, v in fields.items()}}
            for k in ticks]


class InputTests(unittest.TestCase):
    def test_frame_f_is_tick_f_skipped_ticks_keep_keys_and_mouse_is_clamped(self):
        rows = [{"schema": "dm-gta-input-camera-log/1", "tick": 0, "keyCode": [87], "mouse_dx": 3.0, "mouse_dy": 0.1},
                {"schema": "dm-gta-input-camera-log/1", "tick": 2, "keyCode": [65], "mouse_dx": 0.2, "mouse_dy": 0.0}]
        out = input_per_frame(rows, 4)
        self.assertEqual(out[0], {"keyCode": [87], "mouse_dx": 1.0, "mouse_dy": 0.1})     # clamped to [-1, 1]
        self.assertEqual(out[1], {"keyCode": [87], "mouse_dx": 0.0, "mouse_dy": 0.0})     # tick 1 skipped
        self.assertEqual(out[2]["keyCode"], [65])
        self.assertEqual(out[3], {"keyCode": [65], "mouse_dx": 0.0, "mouse_dy": 0.0})     # past the last tick

    def test_a_repeated_tick_is_refused(self):
        row = {"schema": "dm-gta-input-camera-log/1", "tick": 0, "keyCode": []}
        with self.assertRaises(ContractError):
            input_per_frame([row, row], 3)

    def test_who_played_and_a_program_that_stopped_is_refused(self):
        from gta_capture.stage_delivery import input_source
        rows = lambda *src: [{"tick": k, **({"src": s} if s else {})} for k, s in enumerate(src)]
        self.assertEqual(input_source(rows("program", "program")), "program")
        self.assertEqual(input_source(rows("person", "person")), "person")
        self.assertEqual(input_source(rows(None, None)), "person")   # logged before the field existed
        with self.assertRaises(ContractError) as caught:   # the program let go at tick 2: the rest is unplayed
            input_source(rows("program", "program", "stopped", "stopped"))
        self.assertIn("tick 2", str(caught.exception))
        with self.assertRaises(ContractError):
            input_source(rows("program", "person"))


class FpsTests(unittest.TestCase):
    def test_the_rate_is_the_frames_over_the_last_second_of_game_time(self):
        rows = _log(range(31), game_ms=lambda k: 100 * k, frames=lambda k: 99 if k == 0 else (6 if k <= 15 else 3))
        fps = stage_delivery.game_fps_per_frame(rows, 31)
        self.assertEqual(fps[0], 60.0)    # the first second takes the first whole second (ticks 1-10)
        self.assertEqual(fps[10], 60.0)
        self.assertEqual(fps[20], 45.0)   # ticks 11-15 at 60 fps, 16-20 at 30
        self.assertEqual(fps[30], 30.0)

    def test_a_take_under_the_30_fps_cap_reads_a_whole_30(self):
        # one frame a tick near its middle, the game timer stepping 33 or 34 whole milliseconds
        rows = _log(range(90), game_ms=lambda k: (k * 1000) // 30 + 16, frames=lambda k: 1)
        fps = stage_delivery.game_fps_per_frame(rows, 90)
        self.assertEqual(set(fps), {30})
        self.assertTrue(all(type(v) is int for v in fps))

    def test_a_log_shorter_than_a_second_is_refused(self):
        rows = _log(range(2), game_ms=lambda k: 500 * k, frames=lambda k: 30)
        with self.assertRaises(ContractError):
            stage_delivery.game_fps_per_frame(rows, 5)


class TimeTests(unittest.TestCase):
    def test_the_wall_clock_is_each_frames_time_and_a_skipped_tick_lies_between_its_neighbours(self):
        walls = {1: "2026-09-23 15:00:00.040", 2: "2026-09-23 15:00:00.075", 4: "2026-09-23 15:00:00.140"}
        times, how = frame_times(_log(walls, game_ms=lambda k: 33 * k, wall=walls.get), 6)
        self.assertEqual(how, "wall_clock_per_tick")
        self.assertEqual(times, ["2026-09-23 15:00:00.007",    # before the first logged tick: 1/30 s a tick back
                                 "2026-09-23 15:00:00.040", "2026-09-23 15:00:00.075",
                                 "2026-09-23 15:00:00.108",    # tick 3 skipped: halfway, to the millisecond
                                 "2026-09-23 15:00:00.140",
                                 "2026-09-23 15:00:00.173"])   # after the last: 1/30 s a tick on

    def test_an_old_log_takes_the_anchor_plus_the_game_clock_since_tick_zero(self):
        rows = _log([1, 2, 4], game_ms=lambda k: 1000 + 40 * k)   # no wall clock; ticks 0 and 3 skipped
        times, how = frame_times(rows, 5, anchor="2026-09-23 15:00:59.990")
        self.assertEqual(how, "anchor_plus_game_clock")
        self.assertEqual(times, ["2026-09-23 15:00:59.990",    # tick 0 is the anchor
                                 "2026-09-23 15:01:00.023",    # tick 0 lay 1/30 s before tick 1 on the game clock
                                 "2026-09-23 15:01:00.063", "2026-09-23 15:01:00.103", "2026-09-23 15:01:00.143"])

    def test_no_time_is_made_up(self):
        old = _log([0, 1], game_ms=lambda k: 33 * k)
        with self.assertRaises(ContractError):          # no wall clock and no anchor: no fallback to an epoch
            frame_times(old, 2)
        new = _log([0, 1], game_ms=lambda k: 33 * k, wall=lambda k: f"2026-09-23 15:00:00.0{k}0")
        with self.assertRaises(ContractError):          # an anchor would overrule the logged clock
            frame_times(new, 2, anchor="2026-09-23 15:00:00.000")
        with self.assertRaises(ContractError):          # a wall clock on some ticks only
            frame_times(old[:1] + new[1:], 2)
        back = _log([0, 1], game_ms=lambda k: 33 * k, wall=lambda k: f"2026-09-23 15:00:00.{50 - 20 * k:03d}")
        with self.assertRaises(ContractError):          # the system clock went back
            frame_times(back, 2)
        for anchor in ("2026-09-23 15:00:00", "2026-09-31 15:00:00.000", "2026-09-23T15:00:00.000"):
            with self.assertRaises(ContractError, msg=anchor):
                frame_times(old, 2, anchor=anchor)


class EventTests(unittest.TestCase):
    def test_the_same_interaction_at_most_a_second_apart_is_one_run(self):
        frames = [928, 1046, 1055, 1062, 1074, 1104, 1135]   # 1104 is 30 frames after 1074, 1135 is 31 after 1104
        events = sorted([{"frame": f, "event": CLICK} for f in frames] + [{"frame": 1050, "event": "近身攻击"}],
                        key=lambda e: e["frame"])
        runs = stage_delivery.event_runs(events)
        self.assertEqual([(r["text"], r["first_frame"], r["last_frame"], r["count"]) for r in runs],
                         [(CLICK, 928, 928, 1), (CLICK, 1046, 1104, 5), ("近身攻击", 1050, 1050, 1),
                          (CLICK, 1135, 1135, 1)])

    def test_interactions_are_the_logged_events_on_their_ticks(self):
        rows = _log([0, 2, 9], events=lambda k: {2: [CLICK, "按 Shift：跑"], 9: [CLICK]}.get(k, []))
        self.assertEqual(stage_delivery.interactions(rows, 9),   # tick 9 lies past the delivered frames
                         [{"frame": 3, "event": CLICK}, {"frame": 3, "event": "按 Shift：跑"}])   # tick 2 = frame 3
        with self.assertRaises(ContractError):
            stage_delivery.interactions(_log([0], events=lambda k: [1]), 1)


def _unity_q(x_deg, y_deg, z_deg):
    """Unity's Quaternion.Euler(x, y, z) = qy * qx * qz, as [x, y, z, w]."""
    def mul(a, b):
        return [a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
                a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
                a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
                a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2]]
    hx, hy, hz = (math.radians(v) / 2 for v in (x_deg, y_deg, z_deg))
    return mul(mul([0, math.sin(hy), 0, math.cos(hy)], [math.sin(hx), 0, 0, math.cos(hx)]),
               [0, 0, math.sin(hz), math.cos(hz)])


class EulerTests(unittest.TestCase):
    def test_angles_are_unity_euler_angles(self):
        # a delivered fixed-camera frame: 8.86 deg down, 58.3 deg to the left
        e = stage_delivery._unity_euler_deg([0.067476, -0.48553, 0.037623, 0.870799])
        self.assertAlmostEqual(e[0], 8.8617, places=3)
        self.assertAlmostEqual(e[1], 301.7148, places=3)
        self.assertAlmostEqual(e[2], 0.0, places=3)   # the 6-decimal quaternion leaves 0.0001
        self.assertEqual(stage_delivery._unity_euler_deg(_unity_q(10, 0, 0)), [10.0, 0.0, 0.0])   # down is positive
        self.assertEqual(stage_delivery._unity_euler_deg(_unity_q(20, 30, 10)), [20.0, 30.0, 10.0])
        self.assertEqual(stage_delivery._unity_euler_deg(_unity_q(-20, -30, -10)), [340.0, 330.0, 350.0])
        self.assertEqual(stage_delivery._unity_euler_deg([0.0, 0.0, 0.0, 1.0]), [0.0, 0.0, 0.0])


class RecordTests(unittest.TestCase):
    def test_frame_zero_is_stage_tick_zero_and_the_hold_is_left_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = take_records(make_take(Path(tmp), "AB"), "AB", target_gta=[0.0, 0.0, 0.0])
            lanes = take["lanes"]
            self.assertEqual([r["frame"] for r in lanes[1]], list(range(1, M + 1)))   # frame 1 = stage tick 0
            self.assertEqual(lanes[1][0]["camera_position"], [13.0, 3.0, 2.0])   # tape frame HOLD: GTA (13, 2, 3)
            self.assertEqual(lanes[1][0]["time"], "2026-09-23 15:00:00.000")     # tick 0's wall clock
            self.assertEqual(lanes[1][4]["time"], "2026-09-23 15:00:00.136")     # tick 4 skipped: between 3 and 5
            self.assertEqual(take["skipped_ticks"], [4])
            self.assertEqual(take["time_semantics"], "wall_clock_per_tick")
            self.assertEqual(take["focus"], {2: "centroid", 3: "centroid"})

    def test_the_camera_stands_where_the_tape_put_it_not_where_the_game_rounded_it(self):
        # 2026-10-13: the game reads its camera back in single precision (0.12 mm steps 1,500 m from the
        # origin); the third difference of that rounding put B's jerk p95 over the customer's 20
        with tempfile.TemporaryDirectory() as tmp:
            take = make_take(Path(tmp), "AB")
            events = take / "evidence" / "frame-events-lane4.jsonl"
            rows = [json.loads(line) for line in events.read_text(encoding="utf-8").splitlines()]
            for r in rows:   # the export stood 0.12 mm off the tape, as float32 rounds it
                r["final_camera"]["position"][0] += 0.00012
            _jsonl(events, rows)
            result = take / "evidence" / "native-lane-4-result.json"
            value = json.loads(result.read_text(encoding="utf-8"))
            value["lane"]["frame_events_sha256"] = sha256_file(events)
            write_json_atomic(result, value)
            lanes = take_records(take, "AB", target_gta=[0.0, 0.0, 0.0])["lanes"]
            self.assertEqual(lanes[4][0]["camera_position"], [43.0, 3.0, 2.0])   # tape frame HOLD: GTA (43, 2, 3)
            for r in rows:   # a lane rendered a centimetre or more off its tape is still refused
                r["final_camera"]["position"][0] += 0.02
            _jsonl(events, rows)
            value["lane"]["frame_events_sha256"] = sha256_file(events)
            write_json_atomic(result, value)
            with self.assertRaisesRegex(ContractError, "export the lane again"):
                take_records(take, "AB", target_gta=[0.0, 0.0, 0.0])

    def test_ab_gives_b_its_input_but_no_person_and_the_free_paths_follow_their_focus(self):
        with tempfile.TemporaryDirectory() as tmp:
            lanes = take_records(make_take(Path(tmp), "AB"), "AB", target_gta=[0.0, 0.0, 0.0])["lanes"]
            none = {"mouseX": 0.5, "mouseY": 0.5, "mouse_dx": 0.0, "mouse_dy": 0.0, "keyCode": []}
            self.assertTrue(all({k: r[k] for k in stage_delivery.INPUT_KEYS} == none for r in lanes[1]))   # the fixed camera: none
            self.assertNotIn("player_position", lanes[1][2])
            # a free path carries keys and mouse worked out from its path: the fixture's 9 frames
            # of sideways motion are shorter than a kept press, and it does not turn
            self.assertEqual({k: lanes[2][2][k] for k in stage_delivery.INPUT_KEYS},
                             {"mouseX": 0.5, "mouseY": 0.5, "mouse_dx": 0.0, "mouse_dy": 0.0, "keyCode": []})
            self.assertNotIn("player_position", lanes[2][2])
            b = lanes[4][2]
            self.assertNotIn("player_position", b)                        # B plays nobody
            self.assertEqual(b["camera_follow_offset"], [0.0, 0.0, 0.0])
            self.assertEqual(b["keyCode"], [87])                          # frame 2 = tick 2
            self.assertEqual({r["fps"] for lane in lanes.values() for r in lane}, {60.0})
            self.assertEqual(lanes[4][0]["mouse_dx"], 0.25)               # tick 0
            self.assertEqual(lanes[4][1]["mouse_dx"], 1.0)                # tick 1, clamped
            self.assertEqual(lanes[4][4]["mouse_dx"], 0.0)                # tick 4 skipped
            # GTA [x, y, z] -> docs/69 [x, z, y]; the fixed camera at (15, 2, 3) aims at the origin
            self.assertEqual(lanes[1][2]["camera_follow_offset"], [15.0, 3.0, 2.0])
            # free path lane 2 at tape frame 5: camera (25, 2, 3) minus that frame's focus (5, 6, 0)
            self.assertEqual(lanes[2][2]["camera_follow_offset"], [20.0, 3.0, -4.0])
            self.assertEqual([r["time"] for r in lanes[2]], [r["time"] for r in lanes[4]])

    def test_d_free_path_offsets_use_the_seventh_person_after_the_hold_without_person_fields(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = take_records(make_take(Path(tmp), "D"), "D", target_gta=[0.0, 0.0, 0.0])
            self.assertEqual(stage_delivery.number("D", 2), 10)
            self.assertEqual(take["roles"][2], "free_path")
            self.assertEqual(take["focus"], {2: "player"})
            lane = take["lanes"][2]
            self.assertEqual(len(lane), M)
            self.assertEqual(lane[0]["camera_follow_offset"], [22.0, 2.5, -3.0])
            for f, row in enumerate(lane):
                # Focus is tape frame HOLD + f in GTA [x,y,z], exported in docs/69 [x,z,y].
                self.assertEqual([row["camera_position"][i] - row["camera_follow_offset"][i] for i in range(3)],
                                 [1.0, 0.5, 2.0 + HOLD + f])
                self.assertEqual(row["frame"], f + 1)
                self.assertEqual(row["time"], take["lanes"][3][f]["time"])
                self.assertFalse(any(key.startswith("player_") for key in row))
            # Looking at the seventh person does not turn this lane into the input-driven chase camera.
            self.assertEqual(lane[1]["mouse_dx"], 0.0)
            self.assertEqual(take["lanes"][3][1]["mouse_dx"], 1.0)

    def test_c_puts_the_played_person_and_the_input_on_both_person_lanes(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = take_records(make_take(Path(tmp), "C"), "C", target_gta=[0.0, 0.0, 0.0])
            lanes = take["lanes"]
            chase, first = lanes[3][3], lanes[4][3]                        # frame 3 = tape frame 6
            self.assertEqual(chase["player_position"], [1.0, 0.5, 8.0])
            self.assertEqual(chase["camera_follow_offset"], [35.0, 2.5, -6.0])   # camera (36, 3, 2) minus player (1, 0.5, 8)
            self.assertEqual(chase["player_speed"], [0.0, 0.0, 30.0])
            for key in stage_delivery.PLAYER_KEYS:
                self.assertEqual(first[key], chase[key], key)
            # the input is not the same (the customer, 2026-10-16): the third person's is the one recorded, the first
            # person's its head camera's own turns and the way the person goes off it (it neither turns nor walks here)
            self.assertEqual((chase["mouse_dx"], chase["keyCode"]), (0.25, [87]))
            self.assertEqual((first["mouse_dx"], first["mouse_dy"], first["keyCode"]), (0.0, 0.0, []))
            # the takeover is at tick 2: before it the person lanes record the actor and the camera,
            # and carry no key or mouse field at all (the customer, 2026-09-30)
            self.assertEqual(take["person_input_from_frame"], 3)
            for lane in (3, 4):
                for f in (0, 1):
                    self.assertEqual(set(lanes[lane][f]), set(stage_delivery.CAMERA_KEYS + stage_delivery.PLAYER_KEYS))
                    self.assertEqual(lanes[lane][f]["player_position"], [1.0, 0.5, 5.0 + f])
                self.assertIn("keyCode", lanes[lane][2])                    # from the takeover's own frame on
            self.assertEqual(lanes[3][2]["keyCode"], [87])
            self.assertIn("keyCode", lanes[1][0])                          # the fixed camera and the free path: all take long
            self.assertIn("keyCode", lanes[2][0])
            d = take_records(make_take(Path(tmp), "D"), "D", target_gta=[0.0, 0.0, 0.0])
            self.assertEqual(d["person_input_from_frame"], 1)              # D's person is played from tick 0
            self.assertIn("keyCode", d["lanes"][3][0])
            ab = take_records(make_take(Path(tmp), "AB"), "AB", target_gta=[0.0, 0.0, 0.0])
            self.assertIsNone(ab["person_input_from_frame"])
            self.assertIn("keyCode", ab["lanes"][4][0])                    # B's free camera: all take long
            self.assertEqual(first["camera_follow_offset"], [45.0, 2.5, -6.0])  # its own camera, the same person
            self.assertNotIn("player_position", lanes[2][3])
            self.assertEqual(lanes[2][3]["keyCode"], [])                  # the free path's own, not the person's W
            self.assertEqual(take["interactions"][:3], [{"frame": k + 1, "event": CLICK} for k in (1, 2, 3)])

    def test_the_fixed_camera_is_offset_from_the_point_its_tape_aims_at(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = make_take(Path(tmp), "AB")
            tape = json.loads((take / "config" / "camera-tape.json").read_text(encoding="utf-8"))
            tape["source"]["fixed"] = {"position": [10.0, 2.0, 3.0], "aim": [1.0, 2.0, 0.5]}
            write_json_atomic(take / "config" / "camera-tape.json", tape)
            lanes = take_records(take, "AB", target_gta=[0.0, 0.0, 0.0])["lanes"]
            # tape frame 5: camera GTA (15, 2, 3) minus the aim (1, 2, 0.5), in docs/69 [x, z, y]
            self.assertEqual(lanes[1][2]["camera_follow_offset"], [14.0, 2.5, 0.0])

    def test_a_lane_exported_before_its_tape_was_built_again_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = make_take(Path(tmp), "C")
            tape = json.loads((take / "config" / "camera-tape.json").read_text(encoding="utf-8"))
            for sample in tape["lanes"]["1"]["samples"]:   # the fixed camera moved, lane 1 was not exported again
                sample["position"][0] -= 3.0
            write_json_atomic(take / "config" / "camera-tape.json", tape)
            with self.assertRaises(ContractError) as caught:
                take_records(take, "C", target_gta=[0.0, 0.0, 0.0])
            self.assertIn("lane 1", str(caught.exception))
            self.assertIn("export the lane again", str(caught.exception))

    def test_what_does_not_match_is_refused_not_guessed(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = make_take(Path(tmp), "C")
            with open(take / "evidence" / "frame-events-lane2.jsonl", "a", encoding="utf-8") as f:
                f.write("\n")                                          # the events differ from the lane result
            with self.assertRaises(ContractError):
                take_records(take, "C", target_gta=[0.0, 0.0, 0.0])
        with tempfile.TemporaryDirectory() as tmp:
            take = make_take(Path(tmp), "AB")
            (take / "config" / "camera-focus.json").unlink()             # a free path with nothing to look at
            with self.assertRaises(ContractError):
                take_records(take, "AB", target_gta=[0.0, 0.0, 0.0])
        with tempfile.TemporaryDirectory() as tmp:
            take = make_take(Path(tmp), "AB")
            tape = json.loads((take / "config" / "camera-tape.json").read_text(encoding="utf-8"))
            tape["source"]["roles"]["2"] = "orbit"                         # the roles before free paths
            write_json_atomic(take / "config" / "camera-tape.json", tape)
            with self.assertRaises(ContractError):
                take_records(take, "AB", target_gta=[0.0, 0.0, 0.0])


class CheckTests(unittest.TestCase):
    CD = {1: "fixed", 2: "free_path", 3: "chase", 4: "first_person"}
    AB = {1: "fixed", 2: "free_path", 3: "free_path", 4: "free_camera"}

    def _lanes(self):
        base = {"fps": 60.0, "camera_position": [0, 0, 0], "camera_rotation_oula": [0, 0, 0],
                "camera_rotation_quaternion": [0, 0, 0, 1], "camera_follow_offset": [0, 0, 0],
                "camera_intrinsics": {"fx": 1.0, "fy": 1.0, "cx": 0, "cy": 0}, "camera_speed": [0, 0, 0],
                "metric_scale": 1.0}
        extra = {k: 0 for k in stage_delivery.PLAYER_KEYS + stage_delivery.INPUT_KEYS}
        keys = {k: 0 for k in stage_delivery.INPUT_KEYS}   # a fixed camera's and a free path's input, nobody played
        return {lane: [dict(base, frame=f + 1, time=f"t{f:02d}", **(extra if lane >= 3 else keys))
                       for f in range(3)] for lane in range(1, 5)}

    def test_before_the_person_is_played_the_person_lanes_carry_no_input(self):
        with self.assertRaises(ContractError):   # keys and mouse filled in before the takeover (tick 2)
            check_take(self._lanes(), roles=self.CD, n=3, played_from=2)
        lanes = self._lanes()
        for lane in (3, 4):
            for r in lanes[lane][:2]:
                for k in stage_delivery.INPUT_KEYS:
                    del r[k]
        check_take(lanes, roles=self.CD, n=3, played_from=2)
        del lanes[3][2]["keyCode"]               # from the takeover's frame on they are all there
        del lanes[4][2]["keyCode"]
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.CD, n=3, played_from=2)
        lanes = self._lanes()
        del lanes[1][0]["keyCode"]               # the other lanes carry theirs all take long
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.CD, n=3, played_from=2)

    def test_missing_fields_unequal_focal_and_unsynced_lanes_are_refused(self):
        check_take(self._lanes(), roles=self.CD, n=3)
        for lane in (3, 2, 1):   # a person lane, a free path and the fixed camera all carry keys
            lanes = self._lanes()
            del lanes[lane][1]["keyCode"]
            with self.assertRaises(ContractError):
                check_take(lanes, roles=self.CD, n=3)
        lanes = self._lanes()
        lanes[1][0]["player_position"] = 0   # a fixed camera plays nobody
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.CD, n=3)
        lanes = self._lanes()
        lanes[2][0]["camera_intrinsics"] = {"fx": 1.0, "fy": 2.0, "cx": 0, "cy": 0}
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.CD, n=3)
        lanes = self._lanes()
        lanes[4][2]["time"] = "t99"
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.CD, n=3)

    def test_both_person_lanes_must_agree_and_b_carries_no_person(self):
        lanes = self._lanes()
        lanes[4][2]["player_position"] = 1
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.CD, n=3)
        lanes = self._lanes()
        for r in lanes[3]:   # AB's lane 3 is a free path: its worked-out input, nobody played
            for k in stage_delivery.PLAYER_KEYS:
                del r[k]
        for r in lanes[4]:
            for k in stage_delivery.PLAYER_KEYS:
                del r[k]
        check_take(lanes, roles=self.AB, n=3)   # B: its input, nobody played
        lanes[4][0]["player_position"] = 0
        with self.assertRaises(ContractError):
            check_take(lanes, roles=self.AB, n=3)


def _level(heading_deg: float, pitch_deg: float = 0.0) -> list[float]:
    """A GTA camera quaternion looking along a heading (a left turn adds) and pitch."""
    from gta_capture.tape import _quaternion_zxy
    return _quaternion_zxy([pitch_deg, 0.0, heading_deg])


class PathInputTests(unittest.TestCase):
    """Free paths: keys and mouse worked out from the camera path, in worldline B's keys."""

    @staticmethod
    def _walk(steps):
        """positions from per-frame steps (metres a frame), starting at the origin."""
        pos, out = [0.0, 0.0, 0.0], []
        for step in steps:
            out.append(list(pos))
            pos = [pos[i] + step[i] for i in range(3)]
        return out

    def test_the_way_the_camera_moves_is_the_key_in_its_own_level_frame(self):
        # facing -x (heading 90): moving -x is forward (W), +y is its right (D), -y left (A), +x back (S)
        q = [_level(90.0)] * 40
        for step, key in (((-0.02, 0, 0), 87), ((0.02, 0, 0), 83), ((0, 0.02, 0), 68), ((0, -0.02, 0), 65),
                          ((0, 0, 0.02), 69), ((0, 0, -0.02), 81)):
            out = stage_delivery.path_input_per_frame(self._walk([step] * 40), q)   # 0.6 m/s
            self.assertEqual({tuple(r["keyCode"]) for r in out}, {(key,)}, key)
        diagonal = stage_delivery.path_input_per_frame(self._walk([(-0.02, 0.02, 0.0)] * 40), q)
        self.assertEqual(diagonal[20]["keyCode"], [87, 68])   # forward and right: W+D, in W A S D E Q order

    def test_slow_drift_and_short_moves_press_nothing_and_short_let_gos_are_one_press(self):
        q = [_level(0.0)] * 60
        drift = stage_delivery.path_input_per_frame(self._walk([(0.0, 0.001, 0.0)] * 60), q)   # 0.03 m/s
        self.assertTrue(all(r["keyCode"] == [] for r in drift))
        blip = [(0, 0, 0)] * 25 + [(0, 0.03, 0)] * 6 + [(0, 0, 0)] * 29   # 0.2 s of moving
        self.assertTrue(all(r["keyCode"] == [] for r in stage_delivery.path_input_per_frame(self._walk(blip), q)))
        dip = [(0, 0.03, 0)] * 25 + [(0, 0, 0)] * 5 + [(0, 0.03, 0)] * 30   # a 0.17 s stop inside a push
        held = [r["keyCode"] for r in stage_delivery.path_input_per_frame(self._walk(dip), q)]
        self.assertEqual(held[1:-1], [[87]] * 58)

    def test_a_move_or_a_turn_too_slow_to_see_is_no_key_and_no_mouse(self):
        # the customer, 2026-09-30: number 6 carried keys where its camera did not visibly move
        q = [_level(0.0)] * 60
        creep = stage_delivery.path_input_per_frame(self._walk([(0.0, 0.005, 0.0)] * 60), q)   # 0.15 m/s: a still picture
        self.assertTrue(all(r["keyCode"] == [] for r in creep))
        walk = stage_delivery.path_input_per_frame(self._walk([(0.0, 0.013, 0.0)] * 60), q)    # 0.4 m/s: it shows
        self.assertEqual(walk[30]["keyCode"], [87])
        slow_turn = stage_delivery.path_input_per_frame([[0.0, 0.0, 0.0]] * 30, [_level(-0.005 * f) for f in range(30)])
        self.assertTrue(all(r["mouse_dx"] == 0.0 for r in slow_turn))                         # 0.15 deg/s: no mouse
        turn = stage_delivery.path_input_per_frame([[0.0, 0.0, 0.0]] * 30, [_level(-0.02 * f) for f in range(30)])
        self.assertAlmostEqual(turn[10]["mouse_dx"], 0.0025, places=5)                         # 0.6 deg/s: the mouse

    def test_the_turn_and_the_pitch_are_mouse_in_b_units(self):
        # turning right 1.6 degrees a frame (a GTA heading falls) and looking down 0.8 degrees a frame
        q = [_level(-1.6 * f, -0.8 * f) for f in range(30)]
        out = stage_delivery.path_input_per_frame([[0.0, 0.0, 0.0]] * 30, q)
        self.assertEqual((out[0]["mouse_dx"], out[0]["mouse_dy"]), (0.0, 0.0))   # the first frame has no turn before it
        self.assertAlmostEqual(out[10]["mouse_dx"], 0.2, places=5)                # right is positive, 8 degrees a unit
        self.assertAlmostEqual(out[10]["mouse_dy"], 0.1, places=5)                # down is positive
        self.assertTrue(all(r["keyCode"] == [] for r in out))
        across = stage_delivery.path_input_per_frame([[0.0, 0.0, 0.0]] * 2, [_level(179.0), _level(-179.0)])
        self.assertAlmostEqual(across[1]["mouse_dx"], -0.25, places=5)            # a 2 degree left turn over the seam


class StandingTurnTests(unittest.TestCase):
    """The README says how C's and D's standing person was turned: the group's "turn" (2026-10-13)."""

    def test_the_mapping_names_the_turn_the_group_used(self):
        for turn, said in (("game", "0.5–1 秒转完"), (None, "0.5–1 秒转完"), ("steps", "每步最多 30°"),
                           ("glide 50 100", "最快每秒 50°"), ("glide", "最快每秒 45°")):
            with self.subTest(turn=turn):
                notes = " ".join(key_mapping(turn)["C/D"]["notes"])
                self.assertIn(said, notes)
                self.assertNotIn("{turn}", notes)
                self.assertIn(said.split("，")[0][:4], standing_turn_zh(turn)[0] + standing_turn_zh(turn)[1])
        self.assertIn("{turn}", " ".join(KEY_MAPPING["C/D"]["notes"]))   # the template itself is left alone
        self.assertIn("p95 每秒 90°", standing_turn_zh("glide 50 100")[1])


class SettingsTests(unittest.TestCase):
    def test_the_folder_name_and_the_screen_scaling_come_from_the_group(self):
        ok = {"delivery_name": NAME, "environment": {"record_dpi": 1.5}}
        self.assertEqual(stage_delivery.delivery_settings(ok), (NAME, {"AB": 1.5, "C": 1.5, "D": 1.5}))
        by_take = {**ok, "environment": {"record_dpi": {"AB": 1.5, "C": 1, "D": 1.0}}}   # takes recorded on two machines
        self.assertEqual(stage_delivery.delivery_settings(by_take), (NAME, {"AB": 1.5, "C": 1.0, "D": 1.0}))
        for dpi in ({"AB": 1.5, "C": 1.0}, {"AB": 1.5, "C": 1.0, "D": 1.0, "B": 1.0}, {"AB": 1.5, "C": 0.5, "D": 1.0}):
            with self.assertRaises(ContractError, msg=repr(dpi)):
                stage_delivery.delivery_settings({**ok, "environment": {"record_dpi": dpi}})
        for name in (None, "银行门口_001", "2026_09_22_银行门口_1", "2026_02_30_银行门口_001", "2026_09_22_a/b_001",
                     "2026_09_22__001"):
            with self.assertRaises(ContractError, msg=name):
                stage_delivery.delivery_settings({**ok, "delivery_name": name})
        for dpi in (None, True, "1.5", 0.9, 4.5, float("nan")):
            with self.assertRaises(ContractError, msg=repr(dpi)):
                stage_delivery.delivery_settings({**ok, "environment": {"record_dpi": dpi}})

    def test_the_readme_gives_the_screen_scaling_by_take_when_they_differ(self):
        takes = {"AB": {"numbers": [1, 2, 3, 4]}, "C": {"numbers": [5, 6, 7, 8]}, "D": {"numbers": [9, 10, 11, 12]}}
        self.assertEqual(stage_delivery.dpi_text(1.5, takes), "1.5 = 150%")
        self.assertEqual(stage_delivery.dpi_text({"AB": 1.5, "C": 1.5, "D": 1.5}, takes), "1.5 = 150%")
        self.assertEqual(stage_delivery.dpi_text({"AB": 1.5, "C": 1.0, "D": 1.0}, takes),
                         "编号 5–12（C、D）1 = 100%；编号 1–4（AB）1.5 = 150%：这几遍是在两台机器上录的")

    def test_time_anchors_are_one_per_take(self):
        anchors = stage_delivery.parse_time_anchors(["AB=2026-09-23 15:00:00.000", " C = 2026-09-23 15:10:00.500"])
        self.assertEqual(anchors, {"AB": "2026-09-23 15:00:00.000", "C": "2026-09-23 15:10:00.500"})
        for values in (["AB=2026-09-23 15:00:00.000", "AB=2026-09-23 15:00:01.000"], ["B=2026-09-23 15:00:00.000"],
                       ["C 2026-09-23 15:00:00.000"], ["D=2026-09-23 15:00"]):
            with self.assertRaises(ContractError, msg=values):
                stage_delivery.parse_time_anchors(values)


class DeliverTests(unittest.TestCase):
    def _deliver(self, root: Path, out: Path, **kw):
        with mock.patch.object(stage_delivery, "_video_frames", return_value=M):
            return stage_delivery.deliver_group(root, out, **kw)

    def test_the_group_is_one_folder_of_forty_eight_numbered_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            manifest = self._deliver(root, out)
            folder = out / NAME
            self.assertEqual(len(stage_delivery.DELIVERY_FILES), 48)   # video, systeminfo, action_camera, multicam_sync
            self.assertEqual(sorted(p.name for p in folder.iterdir()), sorted(stage_delivery.DELIVERY_FILES))
            self.assertEqual(sorted(p.name for p in out.iterdir()),
                             sorted([NAME, f"{NAME}.README.md", f"{NAME}.manifest.json"]))   # beside it, not in it
            # AB lanes 1-3 = A, AB lane 4 = B, C = 5-8, D = 9-12
            for i, source in ((1, "AB 1"), (4, "AB 4"), (5, "C 1"), (8, "C 4"), (9, "D 1"), (12, "D 4")):
                self.assertEqual((folder / f"video_{i}.mp4").read_bytes(), f"video {source}".encode())
            b = json.loads((folder / "action_camera_4.json").read_text(encoding="utf-8"))
            chase = json.loads((folder / "action_camera_7.json").read_text(encoding="utf-8"))
            self.assertEqual(len(b), M)
            self.assertIn("keyCode", b[0])
            self.assertNotIn("player_position", b[0])
            self.assertIn("player_position", chase[0])
            info = json.loads((folder / "systeminfo_9.json").read_text(encoding="utf-8"))
            self.assertEqual((info["gameProcessName"], info["recordDpi"]), ("GTA5.exe", 1.5))

            self.assertEqual(json.loads((out / f"{NAME}.manifest.json").read_text(encoding="utf-8")), manifest)
            entry = manifest["files"]["action_camera_7.json"]
            keys = ("number", "worldline", "take", "lane", "camera", "camera_zh", "focal_mm")
            self.assertEqual({k: entry[k] for k in keys},
                             {"number": 7, "worldline": "C", "take": "C", "lane": 3, "camera": "chase",
                              "camera_zh": "第三人称追背", "focal_mm": 50})
            self.assertEqual((entry["sha256"], entry["bytes"]), (sha256_file(folder / "action_camera_7.json"),
                                                                 (folder / "action_camera_7.json").stat().st_size))
            self.assertEqual(manifest["files"]["video_2.mp4"]["camera_zh"], "自由运镜（看共视焦点）")
            self.assertEqual(manifest["files"]["video_6.mp4"]["camera_zh"], "自由运镜（看共视焦点）")
            self.assertEqual(manifest["files"]["video_10.mp4"]["camera_zh"], "自由运镜（围绕受控第7人）")
            self.assertEqual(manifest["files"]["action_camera_10.json"]["focus"], "player")
            self.assertEqual(manifest["files"]["video_4.mp4"]["camera_zh"], "真实输入自由相机")
            self.assertEqual(manifest["person_numbers"], [7, 8, 11, 12])
            self.assertEqual(manifest["input_numbers"], [4, 7, 8, 11, 12])
            self.assertEqual({k: t["time_semantics"] for k, t in manifest["takes"].items()},
                             dict.fromkeys(("AB", "C", "D"), "wall_clock_per_tick"))
            self.assertEqual(manifest["takes"]["C"]["hold_frames_cut"], HOLD)

    def test_a_take_logged_from_tick_1_shows_tick_0_as_tick_1s_world(self):
        # every take so far logs from tick 1 (the recorder skips tick 0; beach-demo-003 and 102 alike): tick 0 is the
        # first logged tick's world, at the stage clock's 0 -- not a refusal
        from gta_capture.stage_multicam import take_frames
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "takes" / "AB"
            _jsonl(take / "source" / "master-world-telemetry.jsonl", [world_row("AB", k) for k in range(1, 6)])
            (take / "source" / "stage-plan.txt").write_text(PLAN, encoding="utf-8")
            frames, info = take_frames(take, kind="AB", ids=["chat_a", "pacer"], n=6, played=None, takeover_tick=None)
            self.assertEqual(info["skipped_ticks"], [0])
            self.assertEqual([f["sample_time"] for f in frames], [round(k / 30, 6) for k in range(6)])
            self.assertEqual(info["game_tick_ms"], [150, 150])   # the game's own ticks, for the note
            self.assertEqual([(e["position"], e["rotation"]) for e in frames[0]["entities"]],
                             [(e["position"], e["rotation"]) for e in frames[1]["entities"]])   # tick 1's world

    def test_floats_the_export_bobs_on_its_own_are_left_out_on_request(self):
        # 2026-10-18: a replay bobs the sea's floats on its own waves, so what a take logged of them is not what the
        # videos show -- a group that tracked them is delivered without them, and the note says why
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            self._deliver(root, out, untrack_floats=["wheel"])
            s = json.loads((out / NAME / "multicam_sync_1.json").read_text(encoding="utf-8"))
            self.assertEqual(s["tracked_entity_ids"], ["chat_a", "pacer", "jogger", "seventh", PHONE])
            self.assertTrue(all("wheel" not in [e["id"] for e in f["entities"]] for f in s["frames"]))
            self.assertNotIn("wheel：逐帧记录", s["capture_note"])
            self.assertIn("随浪起伏，导出时由游戏按自己的海浪模拟", s["capture_note"])
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            with self.assertRaises(ContractError):
                self._deliver(root, out, untrack_floats=["float_9"])   # nothing of that id was tracked

    def test_the_sun_lights_every_frame_where_the_group_measured_it(self):
        # the delivery doc v1.1: a main light's direction and intensity in every frame. The game reads no sun: the
        # group carries one measured from shadows (2026-10-18), good only for its hour, weather and place
        sun = {"light": [-0.1638, 0.5295, -0.8324], "intensity": 1.0, "clock": "12:00", "weather": "EXTRASUNNY",
               "near": [10.0, 20.0], "measured": "10 个站着的人的影子"}
        env = {"record_dpi": 1.5, "location": [10.0, 20.0, 1.3], "clock": "12:00", "weather": "EXTRASUNNY", "sun": sun}

        def group(tmp, **env_over):
            root = make_group(Path(tmp) / "group")
            g = json.loads((root / "group.json").read_text(encoding="utf-8"))
            g["environment"] = {**env, **env_over}
            write_json_atomic(root / "group.json", g)
            return root

        with tempfile.TemporaryDirectory() as tmp:
            root, out = group(tmp), Path(tmp) / "out"
            self._deliver(root, out)
            for i in range(1, 13):
                s = json.loads((out / NAME / f"multicam_sync_{i}.json").read_text(encoding="utf-8"))
                # GTA [x, y, z] -> [x, z, y]: down and to the north-north-west, the sun high in the south-south-east
                self.assertEqual({json.dumps(f["lighting"]) for f in s["frames"]},
                                 {json.dumps({"sun_direction": [-0.1638, -0.8324, 0.5295], "intensity": 1.0})})
                self.assertIn("太阳高度 56.3°、方位从正北顺时针 162.8°（南偏东 17°）", s["capture_note"])
                self.assertIn("10 个站着的人的影子", s["capture_note"])
                self.assertNotIn("本版未记录太阳", s["capture_note"])
        for wrong in ({"clock": "18:00"}, {"weather": "CLOUDS"}, {"location": [900.0, 20.0, 1.3]},
                      {"sun": {**sun, "light": [0.0, 0.0, 1.0]}}, {"sun": {**sun, "light": [0.5, 0.0, -0.5]}},
                      {"sun": {**sun, "intensity": 0}}):
            with self.subTest(wrong=wrong), tempfile.TemporaryDirectory() as tmp:
                root, out = group(tmp, **wrong), Path(tmp) / "out"
                with self.assertRaises(ContractError):
                    self._deliver(root, out)
                self.assertFalse(out.exists() and any(out.iterdir()))   # stopped before anything was copied

    def test_each_lane_carries_the_world_under_it_frame_by_frame(self):
        # the delivery doc v1.1 (2026-10-16): multicam_sync_i.json, from what the take logged -- not from a frame rate
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            manifest = self._deliver(root, out)
            sync = {i: json.loads((out / NAME / f"multicam_sync_{i}.json").read_text(encoding="utf-8")) for i in range(1, 13)}
            ids = ["chat_a", "pacer", "jogger", "seventh", PHONE, "wheel"]
            for i, s in sync.items():
                self.assertEqual((s["schema_version"], s["capture_group_id"], s["camera_id"], s["video_file"], s["fps"],
                                  s["time_unit"], s["tracked_entity_ids"]),
                                 ("1.0", NAME, f"video_{i}", f"video_{i}.mp4", 30, "second", ids))
                self.assertEqual([(f["video_frame"], f["logical_frame"]) for f in s["frames"]], [(k, k) for k in range(M)])
                self.assertEqual([[e["id"] for e in f["entities"]] for f in s["frames"]], [ids] * M)   # every id every frame
                self.assertTrue(all(f["lighting"] is None for f in s["frames"]))
                entry = manifest["files"][f"multicam_sync_{i}.json"]
                self.assertEqual((entry["number"], entry["sha256"]), (i, sha256_file(out / NAME / f"multicam_sync_{i}.json")))
            # a take is one world: its four lanes differ only in the camera's name
            for a, b in ((1, 4), (5, 8), (9, 12)):
                self.assertEqual(sync[a]["frames"], sync[b]["frames"])
            # the stage clock, tick k at k/30 s, the same in every take (2026-10-19: the game's own clock differed
            # 1-3 ms between takes and failed the customer's cross-take check); the game's 150 ms ticks only in the note
            for s in sync.values():
                self.assertEqual([f["sample_time"] for f in s["frames"]], [round(k / 30, 6) for k in range(M)])
            self.assertIn("第 k 拍 = k/30 秒", sync[1]["capture_note"])
            self.assertIn("每拍在 150–150 毫秒之间抖动", sync[1]["capture_note"])
            self.assertIn("跳过了 1 拍", sync[1]["capture_note"])
            self.assertIn("wheel：逐帧记录位置和完整旋转", sync[1]["capture_note"])
            self.assertIn("风速约 3.0 m/s", sync[1]["capture_note"])
            # the map's own thing turned whole: GTA [x, y, z, w] -> [-x, -z, -y, w]; 3 ticks = 3 degrees about the axle
            wheel = sync[1]["frames"][3]["entities"][5]
            h = math.radians(3) / 2
            self.assertEqual((wheel["position"], wheel["rotation"]), ([-90.0, 15.0, 20.0], [-round(math.sin(h), 9), 0.0, 0.0, round(math.cos(h), 9)]))
            self.assertIn("第 4 拍", sync[1]["capture_note"])
            seventh = {i: [f["entities"][3] for f in s["frames"]] for i, s in sync.items()}
            for i in range(1, 9):   # the seventh person is only in D's world
                self.assertTrue(all(e == {"id": "seventh", "exists": False, "position": None, "rotation": None, "animation": None}
                                    for e in seventh[i]))
            self.assertEqual(seventh[9][2]["position"], [-2.8, 0.5, 4.0])   # GTA [x, y, z] -> [x, z, y]
            self.assertTrue(all(e["exists"] and e["animation"] is None for e in seventh[9]))   # played: the game's own walk
            # chat_a: GTA heading 90 about the up axis; its second beat from tick 6, time counted from the beat's start
            chat = [f["entities"][0] for f in sync[1]["frames"]]
            h = math.radians(45.0)
            self.assertEqual(chat[0]["rotation"], [0.0, -round(math.sin(h), 9), 0.0, round(math.cos(h), 9)])
            self.assertEqual([c["animation"]["id"] for c in chat], ["chat_a:chat_idle"] * 6 + ["chat_a:wave"] * 3)
            self.assertEqual((chat[7]["animation"]["beat"], chat[7]["animation"]["elapsed_time"]), (1, round(1 / 30, 4)))
            self.assertEqual(chat[3]["animation"]["phase"], 0.1)
            # C: pacer is played from tick 2 and the blocked jogger is off its script ticks 5-7; A's world has neither
            pacer = [f["entities"][1]["animation"] for f in sync[5]["frames"]]
            self.assertEqual([p and p["id"] for p in pacer], ["pacer:walk_fast"] * 2 + [None] * 7)
            jog = [f["entities"][2]["animation"]["id"] for f in sync[5]["frames"]]
            self.assertEqual(jog, ["jogger:jog"] * 5 + ["rule_reaction"] * 3 + ["jogger:jog"])
            self.assertEqual([f["entities"][2]["animation"]["id"] for f in sync[1]["frames"]], ["jogger:jog"] * M)
            self.assertEqual(sync[5]["takeover"], [{
                "logical_frame": 2, "phase": "before_update", "video_frame": 2, "entity_ids": ["pacer"],
                "comparison_note": sync[5]["takeover"][0]["comparison_note"]}])
            self.assertIn("pacer", sync[5]["takeover"][0]["comparison_note"])
            self.assertTrue(sync[1]["takeover"] is None and sync[9]["takeover"] is None)
            self.assertEqual(manifest["takes"]["C"]["multicam_sync"]["takeover_tick"], 2)

    def test_the_free_lanes_moves_are_counted_for_the_customer(self):
        """The customer, 2026-10-07: count the kinds of camera move and how often. Each free lane's segments as its
        tape measured them, the segments carrying each kind, all its kinds (the metadata's 包含运镜轨迹)."""
        orbit = {"move": "orbit_mid", "label": "环绕-中幅环绕：90°至180°", "amount": 97.2, "unit": "°", "detail": "向左"}
        spiral = {"move": "spiral_down", "label": "螺旋升降-螺旋下降", "amount": 1.72, "unit": "m", "detail": ""}
        pan = {"move": "pan_right", "label": "摇动-基础上摇／下摇／左摇／右摇", "amount": 24.5, "unit": "°", "detail": "右摇"}
        report = {"moves": [{"from_s": 0.0, "to_s": 15.5, "moves": [orbit]},
                            {"from_s": 15.5, "to_s": 30.0, "moves": [spiral, pan]},
                            {"from_s": 30.0, "to_s": 40.0, "moves": [orbit, pan]}],
                  "six_ways_m": {"forward": 14.9, "back": 15.2, "right": 67.7, "left": 62.2, "up": 6.0, "down": 8.5}}
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            tape_path = root / "takes" / "C" / "config" / "camera-tape.json"
            tape = json.loads(tape_path.read_text(encoding="utf-8"))
            tape["source"]["free_paths"] = {"2": report}
            write_json_atomic(tape_path, tape)
            manifest = self._deliver(root, out)
            moves = manifest["camera_moves"]
            self.assertEqual(set(moves), {"6"})   # AB's tape here measured nothing: no entry rather than an empty one
            self.assertEqual(moves["6"]["counts"], {"摇动-基础上摇／下摇／左摇／右摇": 2, "环绕-中幅环绕：90°至180°": 2,
                                                    "螺旋升降-螺旋下降": 1})
            self.assertEqual(moves["6"]["labels"], ["摇动-基础上摇／下摇／左摇／右摇", "环绕-中幅环绕：90°至180°",
                                                    "螺旋升降-螺旋下降"])   # the tree's order
            readme = (out / f"{NAME}.README.md").read_text(encoding="utf-8")
            self.assertIn("## 自由运镜的运镜", readme)
            self.assertIn("rev 637 §3.4", readme)
            self.assertIn("| 环绕-中幅环绕：90°至180° | 2 |", readme)
            self.assertIn("| 2 | 15.5–30.0 |  | 螺旋升降-螺旋下降 1.72 m；摇动-基础上摇／下摇／左摇／右摇 24.5°（右摇） |", readme)
            self.assertIn("| 3 | 30.0–40.0 |  | 环绕-中幅环绕：90°至180° 97.2°（向左）；摇动-基础上摇／下摇／左摇／右摇 24.5°（右摇） |",
                          readme)   # a tape measured before the view was: none in its column
            self.assertIn("都看共视焦点（人群的中点）", readme)
            self.assertIn("看人群中点的路不记跟随", readme)
            self.assertIn("记「左摇后自然回镜」「右摇后自然回镜」", readme)
            self.assertIn("包含运镜轨迹（元信息 W 列）：摇动-基础上摇／下摇／左摇／右摇、环绕-中幅环绕：90°至180°、螺旋升降-螺旋下降",
                          readme)

    def test_the_readme_distinguishes_the_seventh_person_from_the_centroid_without_inventing_moves(self):
        report = {"moves": [{"from_s": 0.0, "to_s": 10.0, "moves": [
            {"move": "orbit_mid", "label": "环绕-中幅环绕：90°至180°", "amount": 97.2, "unit": "°", "detail": "向左"}]}]}
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            for kind in ("AB", "C", "D"):
                path = root / "takes" / kind / "config" / "camera-tape.json"
                tape = json.loads(path.read_text(encoding="utf-8"))
                tape["source"]["free_paths"] = {lane: report for lane, role in ROLES[kind].items() if role == "free_path"}
                write_json_atomic(path, tape)
            manifest = self._deliver(root, out)
            readme = (out / f"{NAME}.README.md").read_text(encoding="utf-8")
            self.assertIn("编号 2、3、6 都看共视焦点（人群的中点）", readme)
            self.assertIn("编号 10 围绕受控第7人（这条世界线没有改变原有事件），焦点取该人物逐帧记录的位置（缺拍处插值）", readme)
            self.assertNotIn("都看共视焦点（人群的中点），不跟着哪一个人", readme)
            self.assertIn("看人群中点的路不记跟随", readme)
            self.assertIn("类型见各路说明：共视焦点、事件焦点或受控第7人，逐帧记录", readme)
            self.assertEqual(set(manifest["camera_moves"]), {"2", "3", "6", "10"})
            for moves in manifest["camera_moves"].values():
                self.assertEqual(moves["counts"], {"环绕-中幅环绕：90°至180°": 1})

    def test_the_first_person_turning_round_before_the_body_is_listed(self):
        """The first-person view is the game camera's; standing, the take turns the body and the camera follows it.
        The README says so and lists the frames it looks more than FIRST_PERSON_AHEAD_DEG round from the body (should
        there be any); it says the walking's own lag too."""
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            path = root / "takes" / "C" / "evidence" / "frame-events-lane4.jsonl"
            rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
            turned = [0.0, 0.0, math.sin(math.radians(75.0)), math.cos(math.radians(75.0))]   # heading 150, the body 0
            for r in rows:
                if HOLD + 4 <= r["output_frame"] <= HOLD + 6:   # stage ticks 4-6, after C's control starts (tick 2)
                    r["final_camera"]["quaternion"] = turned
            _jsonl(path, rows)
            result_path = root / "takes" / "C" / "evidence" / "native-lane-4-result.json"
            result = json.loads(result_path.read_text(encoding="utf-8"))
            result["lane"]["frame_events_sha256"] = sha256_file(path)
            write_json_atomic(result_path, result)
            manifest = self._deliver(root, out)
            self.assertEqual(manifest["first_person_ahead_frames"], {"8": [[5, 7]], "12": []})
            readme = (out / f"{NAME}.README.md").read_text(encoding="utf-8")
            self.assertIn("所以朝身体的朝向（需方 2026-10-16：第一人称跟着头走）", readme)
            self.assertIn("是 GTA 第三人称的跟随镜头，跟着录制时的鼠标转", readme)
            self.assertIn("第一人称路（编号见各路说明）的键鼠不是这份真实记录，而是按头部相机的真实变化换算的", readme)
            self.assertIn("编号 8：第 5–7 帧（0.1–0.2 秒），合计 0.1 秒；编号 12：没有", readme)
            self.assertIn("走满速时约 1 秒、约 1 米", readme)

    def test_the_readme_says_what_each_worldline_did_second_by_second(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            manifest = self._deliver(root, out)
            c, d = manifest["worldlines"]["C"], manifest["worldlines"]["D"]
            self.assertEqual(c["numbers"], [5, 6, 7, 8])
            self.assertEqual(c["controlled"], {"actor": "pacer", "model": None, "from_frame": 3, "from_s": 0.067})
            self.assertEqual((c["event_changed"], c["triggered_rules"], c["attack_interactions"]),
                             (True, ["block_jogger"], 3))   # the takeover itself is no rule firing
            self.assertEqual(d["controlled"], {"actor": "seventh", "model": "a_m_y_beach_02", "from_frame": 1,
                                               "from_s": 0.0})
            self.assertEqual((d["event_changed"], d["triggered_rules"]), (False, []))   # nor is the seventh's entry
            readme = (out / f"{NAME}.README.md").read_text(encoding="utf-8")
            self.assertIn("| 4 | B | AB | 4 | 真实输入自由相机 | 18 mm |", readme)
            self.assertIn("| 6 | C | C | 2 | 自由运镜（看共视焦点） | 24 mm |", readme)
            self.assertIn("JSON 的第 1 帧（视频的第 1 帧）就是舞台第 0 秒", readme)
            self.assertIn(f"第 0.0–0.1 秒 {CLICK}（连续 3 次）", readme)
            self.assertIn("第 0.2 秒 按 E：互动", readme)
            self.assertIn("第 0.2 秒 玩家挡住慢跑者的路，慢跑者停下（规则 block_jogger）", readme)
            self.assertIn("原事件是否被改变：**是**（触发了因果规则 block_jogger；玩家有 3 次攻击类操作）", readme)
            self.assertIn("原事件是否被改变：**否**", readme)
            self.assertIn("演员 `pacer`，第 0.067 秒（第 3 帧）起由真人键鼠接管", readme)
            self.assertIn("新增的第 7 人（模型 `a_m_y_beach_02`），第 0 秒（第 1 帧）起", readme)
            self.assertIn("B 与 A 出自同一遍录制", readme)
            self.assertIn("1.5 = 150%", readme)
            self.assertNotIn("静止段", readme)
            self.assertNotIn("接管 pacer", readme)   # the control start is said once, not as a rule

    def test_the_key_mapping_and_the_frame_cap_reach_the_readme_and_the_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            for kind in ("AB", "C", "D"):   # recorded under the 30 fps cap
                path = root / "takes" / kind / "evidence" / "stage-take-result.json"
                result = json.loads(path.read_text(encoding="utf-8"))
                result["take"].update(frame_cap={"fps": 30, "frames": 3700, "late_frames": 2, "worst_late_ms": 12.5},
                                      aircraft_removed=1 if kind == "C" else 0)
                write_json_atomic(path, result)
            manifest = self._deliver(root, out)
            self.assertEqual((manifest["takes"]["C"]["aircraft_removed"], manifest["takes"]["D"]["frame_cap"]["fps"]), (1, 30))
            self.assertEqual(manifest["takes"]["C"]["keys_used"], [87])
            self.assertEqual(sorted(manifest["key_mapping"]), ["B", "C/D", "自由运镜"])
            self.assertEqual(manifest["path_input_numbers"], [2, 3, 6, 10])
            self.assertEqual(manifest["still_input_numbers"], [1, 5, 9])
            readme = (out / f"{NAME}.README.md").read_text(encoding="utf-8")
            self.assertIn("### 世界线 B（编号 4：真实输入自由相机）", readme)
            self.assertIn("### 世界线 C/D（编号 7、8、11、12：受控人物", readme)
            self.assertIn("| E | 69 | 上升，1.0 m/s |", readme)
            self.assertIn("### 自由运镜（世界线 A、C、D 的编号 2、3、6、10：由运镜路线换算，非实时操作）", readme)
            self.assertIn("编号 2、3、6、10（自由运镜）也带键鼠字段，但不是实时操作的记录", readme)
            self.assertIn("编号 1、5、9（固定机位）也带键鼠字段：相机全程不动，每帧都是无按键", readme)
            self.assertIn("编号 7–8（C）从第 3 帧（接管那一帧）起才有键鼠字段", readme)
            self.assertNotIn("编号 11–12（D）从第", readme)   # D's person is played from the first frame
            self.assertEqual((manifest["takes"]["C"]["person_input_from_frame"], manifest["takes"]["D"]["person_input_from_frame"],
                              manifest["takes"]["AB"]["person_input_from_frame"]), (3, 1, None))
            c7 = json.loads((out / NAME / "action_camera_7.json").read_text(encoding="utf-8"))
            self.assertEqual(["keyCode" in r for r in c7[:4]], [False, False, True, True])
            self.assertIn("本组实际按过的键：C 用到 W；D 用到 W。", readme)
            self.assertIn("录制时游戏锁在每秒 30 帧", readme)
            b = json.loads((out / NAME / "action_camera_4.json").read_text(encoding="utf-8"))
            self.assertTrue(all(type(r["fps"]) is int for r in b))   # whole frames a second, as the sample writes it

    def test_an_attack_changes_the_events_even_when_no_rule_fired(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = make_group(Path(tmp) / "group", D={"events": {5: ["近身攻击"]}})
            d = self._deliver(root, Path(tmp) / "out")["worldlines"]["D"]
            self.assertEqual((d["event_changed"], d["triggered_rules"], d["attack_interactions"]), (True, [], 1))

    def test_an_old_log_needs_a_time_anchor_and_the_readme_says_so(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group", C={"wall": False}), Path(tmp) / "out"
            with self.assertRaises(ContractError):
                self._deliver(root, out)
            self.assertFalse((out / NAME).exists())   # nothing goes under the delivery's name until it is whole
            manifest = self._deliver(root, out, time_anchors={"C": "2026-09-23 15:10:00.000"})
            self.assertEqual((manifest["takes"]["C"]["time_semantics"], manifest["takes"]["C"]["time_anchor"]),
                             ("anchor_plus_game_clock", "2026-09-23 15:10:00.000"))
            first_person = json.loads((out / NAME / "action_camera_8.json").read_text(encoding="utf-8"))
            self.assertEqual([r["time"] for r in first_person[:3]],
                             ["2026-09-23 15:10:00.000", "2026-09-23 15:10:00.150", "2026-09-23 15:10:00.300"])
            readme = (out / f"{NAME}.README.md").read_text(encoding="utf-8")
            self.assertIn("编号 5–8（C）的 `time` 是给定的舞台第 0 拍系统时间加游戏时钟推算的", readme)

    def test_a_delivery_is_written_once_and_whole(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, out = make_group(Path(tmp) / "group"), Path(tmp) / "out"
            with mock.patch.object(stage_delivery, "_video_frames", return_value=N):   # a video that kept its hold
                with self.assertRaises(ContractError):
                    stage_delivery.deliver_group(root, out)
            self.assertFalse((out / NAME).exists())
            self._deliver(root, out)
            with self.assertRaises(ContractError):   # the name is taken
                self._deliver(root, out)
            settings = json.loads((root / "group.json").read_text(encoding="utf-8"))
            del settings["delivery_name"]
            write_json_atomic(root / "group.json", settings)
            with self.assertRaises(ContractError):
                self._deliver(root, Path(tmp) / "elsewhere")


if __name__ == "__main__":
    unittest.main()
