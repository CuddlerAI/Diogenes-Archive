import copy
import json
import math
import shutil
import tempfile
import unittest
from pathlib import Path

from helpers import write_json
from gta_capture.errors import ContractError
from gta_capture.jsonio import load_json, sha256_file, write_json_atomic
from gta_capture.stage_group import (EXACT_M, compare, cross_worldline_gate, load_world,
                                     prepare_group, prepare_take)
from gta_capture.stage_take import (NATIVE_KEYS, StageScenario, build_take_scenario,
                                    check_native_keys, check_take_result, read_spec_header)

STAGE_X, STAGE_Y = -1574.1906, -1146.0808   # the SPEC frame
SPEC = """dm-gta-stage-spec/1
frame -1574.1906 -1146.0808 1.87 104.97
ticks 90
clip idle amb@world_human_stand_impatient@male@no_sign@base base loop
clip phone amb@world_human_stand_mobile@male@standing@call@base base loop
actor chat_a a_m_y_beach_01
beat chat_a idle idle 3 at -2.5 1.0 0
actor pacer a_m_y_hipster_01
prop pacer prop_npc_phone_02 28422
beat pacer idle phone 3 at 1.0 2.6 0
rule grab near chat_a 1.5 then chat_a:flee say 玩家逼近聊天的人，他逃开
"""


def group_config(root: Path) -> Path:
    (root / "stage-spec.txt").write_text(SPEC, encoding="utf-8")
    return write_json(root / "group.json", {
        "schema": "dm-gta-stage-group/1",
        "group_id": "beach-demo-test",
        "stage_spec": "stage-spec.txt",
        "duration_s": 4,
        "hold_s": 1.0,
        "seed": 7,
        "environment": {"location": [-1574.19, -1146.08, 2.87], "clock": "12:00", "weather": "EXTRASUNNY",
                        "source_epoch": "2026-09-23 00:00:00.000", "warmup_s": 20},
        "free_camera": {"x": 0, "y": -8, "height": 1.7, "heading": 0, "fov": 50},
        "takeover": {"actor": "pacer", "at_s": 1},
        "seventh": {"model": "a_m_y_beach_02", "x": 0, "y": -6, "heading": 0},
    })


def actor_row(aid, x, released=0, bone_dx=0.0):
    pos = [x, 1.0, 2.0]
    bones = [[x + bone_dx, 1.0, 2.0 + i * 0.1] for i in range(7)]
    return {"id": aid, "released": released, "beat": 0, "state": {"pos": pos, "h": 0.0, "phase": 0.5, "bones": bones}}


def world_rows(n=90, *, pacer_dx=0.0, chat_dx=0.0, from_tick=0, held=3, prop_dx=0.0):
    rows = []
    for k in range(n):
        dx = lambda d: d if k >= from_tick else 0.0
        rows.append({"schema": "dm-gta-world-telemetry/1", "tick": k, "game_ms": k * 33, "held": held, "skipped_before": 0,
                     "actors": [actor_row("chat_a", -2.0 + dx(chat_dx)), actor_row("pacer", 1.0 + dx(pacer_dx))],
                     "props": [{"id": "pacer:prop_npc_phone_02", "state": {"pos": [1.2 + dx(prop_dx), 1.0, 2.5], "h": 0}}],
                     "player": None})
    return rows


# C after its takeover and D are played in the game's third person, never leaving it
PLAYED = {"view_mode_restores": 0, "body_turns": 4}


def write_take(group: Path, kind: str, rows, events, *, cal_text: str, measured: bool, extra=None, plan=None):
    take = group / "takes" / kind
    src = take / "source"
    src.mkdir(parents=True, exist_ok=True)
    (take / "evidence").mkdir(parents=True, exist_ok=True)
    with open(src / "master-world-telemetry.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    with open(src / "rule-events.jsonl", "w", encoding="utf-8") as f:
        for e in events:
            f.write(json.dumps(e, ensure_ascii=False) + "\n")
    with open(src / "input-camera-log.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps({"schema": "dm-gta-input-camera-log/1", "tick": r["tick"]}) + "\n")
    (src / "stage-calibration.txt").write_text(cal_text, encoding="utf-8")
    (src / "stage-plan.txt").write_text(plan or PLAN, encoding="utf-8")
    result = {"schema": "dm-gta-stage-take/1", "take_kind": kind,
              "clips": [{"index": 1, "sha256": "ab" * 32, "duration_s": 3.93}],
              "clips_duration_s": 3.93, "game_span_s": 4.0, "hold_s": 1.0,
              "ticks_sampled": len(rows), "ticks_skipped": 90 - len(rows),
              "frames_not_recording": 0, "calibration_measured": measured,
              "stage_calibration_path": str(src / "stage-calibration.txt"),
              "stage_calibration_sha256": sha256_file(src / "stage-calibration.txt"),
              "stage_plan_sha256": sha256_file(src / "stage-plan.txt"),
              "world_telemetry_sha256": sha256_file(src / "master-world-telemetry.jsonl"),
              "rule_events_sha256": sha256_file(src / "rule-events.jsonl"),
              "input_log_sha256": sha256_file(src / "input-camera-log.jsonl"),
              "scene_failures": 0, "runtime_releases": 0, "entries_checked": 4, "entries_not_running": 0,
              "entries_not_playing": 0, "plan_ground_misses": 0, "cams_read_late": 0,
              "takeover": {"actor": "pacer", "tick": 30, "ok": True, **PLAYED} if kind == "C" else None,
              "seventh": {"model": "a_m_y_beach_02", "ok": True, **PLAYED} if kind == "D" else None}
    result.update(extra or {})
    write_json_atomic(take / "evidence" / "stage-take-result.json", {"ok": True, "take": result})
    return result


def event(tick, rule, affected, say="说明"):
    return {"schema": "dm-gta-rule-events/1", "tick": tick, "t_s": tick / 30, "rule": rule, "when": "near",
            "subject": affected[0], "value": 1.0, "affected": [{"actor": a, "action": "flee"} for a in affected], "say": say}


CAL = "dm-gta-stage-calibration/1\nclip idle n 2 duration 0.1 zoff 1 speed 0\nend\n"
# chat_a blends into a new beat at tick 40; the pacer snaps into one at tick 60
PLAN = """dm-gta-stage-plan/1
actor chat_a beats 2
  idle idle start 0 ticks 40 phase0 0 cycle 2 path -1 speed 0 origin 0 0 0 heading 0 ground 0 entry_pop 0 blend 4.000000
  pair give_a start 40 ticks 50 phase0 0 cycle 50 path -1 speed 0 origin 0 0 0 heading 0 ground 0 entry_pop 0.1 blend 4.000000
actor pacer beats 2
  idle phone start 0 ticks 60 phase0 0 cycle 2 path -1 speed 0 origin 0 0 0 heading 0 ground 0 entry_pop 0 blend 0.000000
  idle phone start 60 ticks 30 phase0 0 cycle 2 path -1 speed 0 origin 0 0 0 heading 0 ground 0 entry_pop 0 blend 0.000000
"""


class StageScenarioTests(unittest.TestCase):
    def test_spec_header_reads_ticks_actors_rules_and_props(self):
        h = read_spec_header(SPEC)
        self.assertEqual(h.ticks, 90)
        self.assertEqual(h.actors, ("chat_a", "pacer"))
        self.assertEqual(h.rules, ("grab",))
        self.assertEqual(h.props, (("pacer", "prop_npc_phone_02"),))
        with self.assertRaises(ContractError):
            read_spec_header("dm-gta-stage-spec/2\nticks 90\n")

    def test_group_prepares_ab_then_c_and_d_replay_its_calibration(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            ab = StageScenario.load(group / "takes" / "AB" / "config" / "scenario.json")
            self.assertEqual(ab.take_kind, "AB")
            self.assertIsNone(ab.calibration_path)
            text = ab.path.read_text(encoding="utf-8")
            for key in NATIVE_KEYS:
                self.assertLessEqual(text.count(f'"{key}"'), 1, key)
            with self.assertRaises(ContractError):   # C before AB was recorded
                prepare_take(group, "C")
            write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=True)
            c = StageScenario.load(prepare_take(group, "C"))
            d = StageScenario.load(prepare_take(group, "D"))
            self.assertEqual(c.raw["take"]["takeover_actor"], "pacer")
            self.assertEqual(sha256_file(c.calibration_path), sha256_file(group / "takes" / "AB" / "source" / "stage-calibration.txt"))
            self.assertEqual(d.raw["take"]["seventh_model"], "a_m_y_beach_02")
            with self.assertRaises(ContractError):
                prepare_take(group, "C")   # never twice

    def test_contract_refusals(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            path = group / "takes" / "AB" / "config" / "scenario.json"
            good = load_json(path)

            def refused(mutate):
                bad = copy.deepcopy(good)
                mutate(bad)
                write_json_atomic(path, bad)
                with self.assertRaises(ContractError):
                    StageScenario.load(path)
                write_json_atomic(path, good)

            refused(lambda s: s["stage"].update(stage_spec_sha256="0" * 64))
            refused(lambda s: s["stage"].update(stage_spec_path="../stage-spec.txt"))
            refused(lambda s: s.update(radio="on"))
            refused(lambda s: s["replay"].update(duration_s=3.5))   # shorter than hold + ticks
            refused(lambda s: s["take"].update(take_kind="C", takeover_actor="pacer", takeover_at_s=1))   # C without calibration
            # a nested key named like one the native side reads would be read instead
            refused(lambda s: s.update(notes={"weather": "RAIN"}))
            StageScenario.load(path)

    def test_native_key_collision_is_refused_before_anything_else(self):
        with self.assertRaises(ContractError):
            check_native_keys('{"clock": "12:00", "x": {"clock": "13:00"}}')
        check_native_keys('{"clock": "12:00", "stage_calibration_path": "a"}')

    def test_c_takeover_second_rules(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=True)
            path = prepare_take(group, "C")
            good = load_json(path)
            for bad_value in (56, 1.5, -1, 3, "1"):   # 3 s lies after this 90-tick stage ends
                bad = copy.deepcopy(good)
                bad["take"]["takeover_at_s"] = bad_value
                write_json_atomic(path, bad)
                with self.assertRaises(ContractError, msg=str(bad_value)):
                    StageScenario.load(path)


class GateTests(unittest.TestCase):
    def _group(self, root, *, c_rows, c_events, d_rows, d_events, a_rows=None):
        group = prepare_group(group_config(root), root / "group")
        write_take(group, "AB", a_rows or world_rows(), [], cal_text=CAL, measured=True)
        prepare_take(group, "C")
        prepare_take(group, "D")
        write_take(group, "C", c_rows, c_events, cal_text=CAL, measured=False)
        write_take(group, "D", d_rows, d_events, cal_text=CAL, measured=False)
        return group

    def test_identical_worldlines_are_exact(self):
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp), c_rows=world_rows(), c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=world_rows(), d_events=[])
            report = cross_worldline_gate(group)
            self.assertTrue(report["exact"])
            self.assertTrue(report["same_plan"] and report["same_calibration"])
            self.assertTrue((group / "evidence" / "cross-worldline.json").is_file())

    def test_the_taken_over_and_the_released_may_differ_after_their_tick(self):
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp),
                                c_rows=world_rows(pacer_dx=3.0, prop_dx=3.0, from_tick=30),
                                c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=world_rows(chat_dx=2.0, from_tick=50),
                                d_events=[event(50, "grab", ["chat_a"], "玩家逼近聊天的人，他逃开")])
            report = cross_worldline_gate(group)
            self.assertTrue(report["exact"], report)
            self.assertEqual(report["affected"]["D"], {"chat_a": 50})
            self.assertEqual(report["causal_record"]["D"][0]["say"], "玩家逼近聊天的人，他逃开")

    def test_an_untouched_divergence_is_caught_and_graded(self):
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp), c_rows=world_rows(), c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=world_rows(chat_dx=0.001, from_tick=10), d_events=[])
            report = cross_worldline_gate(group)
            self.assertFalse(report["exact"])
            self.assertTrue(report["within_tolerance"])
            self.assertEqual(report["a_d_untouched"]["chat_a"]["first_over_tick"], 10)
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp), c_rows=world_rows(chat_dx=0.05, from_tick=5),
                                c_events=[event(30, "takeover", ["pacer"])], d_rows=world_rows(), d_events=[])
            report = cross_worldline_gate(group)
            self.assertFalse(report["within_tolerance"])
            self.assertLess(report["a_c_before_takeover"]["chat_a"]["pos_m"], 0.06)

    def test_a_pose_difference_just_after_a_blend_is_reported_not_failed(self):
        def rows_with(bone_dx_at, pos_dx_at=None):
            rows = world_rows()
            for r in rows:
                for a in r["actors"]:
                    if a["id"] == "chat_a" and r["tick"] in bone_dx_at:
                        a["state"]["bones"][3] = [9.0, 9.0, 9.0]   # a hand elsewhere mid-blend
                    if pos_dx_at and a["id"] == "chat_a" and r["tick"] in pos_dx_at:
                        a["state"]["pos"][0] += 0.01
            return rows
        with tempfile.TemporaryDirectory() as tmp:   # inside chat_a's blend window (40..59): allowed
            group = self._group(Path(tmp), c_rows=world_rows(), c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=rows_with({45}), d_events=[])
            report = cross_worldline_gate(group)
            self.assertTrue(report["exact"], report["a_d_untouched"])
            self.assertGreater(report["a_d_untouched"]["chat_a"]["bone_in_blend_m"], 1.0)
        with tempfile.TemporaryDirectory() as tmp:   # the same after the window: caught
            group = self._group(Path(tmp), c_rows=world_rows(), c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=rows_with({70}), d_events=[])
            self.assertFalse(cross_worldline_gate(group)["within_tolerance"])
        with tempfile.TemporaryDirectory() as tmp:   # a position is held exact even mid-blend
            group = self._group(Path(tmp), c_rows=world_rows(), c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=rows_with(set(), pos_dx_at={45}), d_events=[])
            self.assertFalse(cross_worldline_gate(group)["exact"])
        with tempfile.TemporaryDirectory() as tmp:   # a snap has no window: the pacer at tick 65 is caught
            rows = world_rows()
            for r in rows:
                for a in r["actors"]:
                    if a["id"] == "pacer" and r["tick"] == 65:
                        a["state"]["bones"][3] = [9.0, 9.0, 9.0]
            group = self._group(Path(tmp), c_rows=world_rows(), c_events=[event(30, "takeover", ["pacer"])],
                                d_rows=rows, d_events=[])
            self.assertFalse(cross_worldline_gate(group)["within_tolerance"])

    def test_props_compare_only_where_their_pose_was_up_two_frames(self):
        a = {k: {"held": 3, "actors": {}, "props": {"pacer:p": {"state": {"pos": [0, 0, 0]}}}} for k in range(3)}
        b = {k: {"held": 1 if k == 1 else 3, "actors": {},
                 "props": {"pacer:p": {"state": {"pos": [0.1 if k == 1 else 0, 0, 0]}}}} for k in range(3)}
        out = compare(a, b)
        self.assertEqual(out["pacer:p"]["ticks"], 2)
        self.assertLessEqual(out["pacer:p"]["pos_m"], EXACT_M)

    def test_a_group_may_bring_its_calibration(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = group_config(root)
            (root / "rehearsed.txt").write_text(CAL, encoding="utf-8")
            value = json.loads(Path(config).read_text(encoding="utf-8"))
            Path(config).write_text(json.dumps({**value, "stage_calibration": "rehearsed.txt"}), encoding="utf-8")
            group = prepare_group(config, root / "group")
            made = json.loads((group / "group.json").read_text(encoding="utf-8"))
            self.assertEqual(made["stage_calibration"], "stage-calibration.txt")
            self.assertEqual(made["stage_calibration_sha256"], sha256_file(root / "rehearsed.txt"))
            scenario = StageScenario.load(group / "takes" / "AB" / "config" / "scenario.json")
            self.assertEqual(sha256_file(scenario.calibration_path), sha256_file(root / "rehearsed.txt"))
            # AB replays it: a result that measured its own is refused, and the other way round without one
            replayed = write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=False)
            self.assertEqual(check_take_result(replayed, scenario), [])
            self.assertTrue(check_take_result({**replayed, "calibration_measured": True}, scenario))
            # C takes AB's, which is the one the group brought
            c = StageScenario.load(prepare_take(group, "C"))
            self.assertEqual(sha256_file(c.calibration_path), sha256_file(root / "rehearsed.txt"))
            # changed after the group named it: no take is prepared from it
            (group / "stage-calibration.txt").write_text(CAL + "\n", encoding="utf-8")
            shutil.rmtree(group / "takes" / "AB")
            with self.assertRaisesRegex(ContractError, "changed after"):
                prepare_take(group, "AB")
        for bad in ("", "../cal.txt", "dir/cal.txt", 3):
            with tempfile.TemporaryDirectory() as tmp, self.assertRaises(ContractError):
                root = Path(tmp)
                config = group_config(root)
                value = json.loads(Path(config).read_text(encoding="utf-8"))
                Path(config).write_text(json.dumps({**value, "stage_calibration": bad}), encoding="utf-8")
                prepare_group(config, root / "group")

    def test_take_result_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            scenario = StageScenario.load(group / "takes" / "AB" / "config" / "scenario.json")
            good = write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=True)
            self.assertEqual(check_take_result(good, scenario), [])
            self.assertTrue(check_take_result({**good, "clips_duration_s": 2.0}, scenario))
            self.assertTrue(check_take_result({**good, "calibration_measured": False}, scenario))
            self.assertTrue(check_take_result({**good, "frames_not_recording": 3}, scenario))
            # the cast off its plan fails the take, and so does a result that does not say
            for key in ("scene_failures", "runtime_releases", "entries_not_running", "entries_not_playing",
                        "plan_ground_misses", "cams_read_late"):
                self.assertTrue(check_take_result({**good, key: 1}, scenario), key)
                self.assertTrue(check_take_result({k: v for k, v in good.items() if k != key}, scenario), key)

    def test_c_and_d_hold_the_third_person(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=True)
            for kind, part in (("C", "takeover"), ("D", "seventh")):
                scenario = StageScenario.load(prepare_take(group, kind))
                good = write_take(group, kind, world_rows(), [], cal_text=CAL, measured=False)
                self.assertNotIn("third person", " ".join(check_take_result(good, scenario)))
                for bad in ({"view_mode_restores": 2}, {"view_mode_restores": None}, {"body_turns": None}):
                    result = {**good, part: {**good[part], **bad}}
                    self.assertTrue(check_take_result(result, scenario), (kind, bad))


class GateCoverageTests(unittest.TestCase):
    """Nothing absent, cut short or let go unexplained passes the gate."""

    def _group(self, root, *, a_rows=None, c_rows=None, c_events=None, d_rows=None, d_events=None):
        group = prepare_group(group_config(root), root / "group")
        write_take(group, "AB", a_rows or world_rows(), [], cal_text=CAL, measured=True)
        prepare_take(group, "C")
        prepare_take(group, "D")
        write_take(group, "C", c_rows or world_rows(), c_events if c_events is not None else [event(30, "takeover", ["pacer"])],
                   cal_text=CAL, measured=False)
        write_take(group, "D", d_rows or world_rows(), d_events or [], cal_text=CAL, measured=False)
        return group

    def test_a_telemetry_file_cut_short_after_sealing_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp))
            path = group / "takes" / "D" / "source" / "master-world-telemetry.jsonl"
            lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
            path.write_text("".join(lines[:10]), encoding="utf-8")
            report = cross_worldline_gate(group)
            self.assertFalse(report["exact"])
            self.assertTrue(any("sealed" in p for p in report["problems"]), report["problems"])
            self.assertTrue(any("ticks of world telemetry" in p for p in report["problems"]), report["problems"])

    def test_an_actor_missing_or_without_a_state_fails(self):
        rows = world_rows()
        for r in rows:
            r["actors"] = [a for a in r["actors"] if a["id"] != "chat_a"]
        with tempfile.TemporaryDirectory() as tmp:
            report = cross_worldline_gate(self._group(Path(tmp), d_rows=rows))
            self.assertFalse(report["exact"])
            self.assertEqual(report["a_d_untouched"]["chat_a"]["missing_ticks"], 90)
        rows = world_rows()
        for r in rows[20:]:
            for a in r["actors"]:
                if a["id"] == "chat_a":
                    a["state"] = None
        with tempfile.TemporaryDirectory() as tmp:
            report = cross_worldline_gate(self._group(Path(tmp), d_rows=rows))
            self.assertFalse(report["within_tolerance"])
            self.assertEqual(report["a_d_untouched"]["chat_a"]["missing_ticks"], 70)
        rows = world_rows()
        for r in rows:
            r["props"] = []
        with tempfile.TemporaryDirectory() as tmp:
            report = cross_worldline_gate(self._group(Path(tmp), c_rows=rows))
            self.assertFalse(report["exact"])
            self.assertGreater(report["a_c_before_takeover"]["pacer:prop_npc_phone_02"]["missing_ticks"], 0)

    def test_a_coordinate_that_is_not_a_number_fails(self):
        rows = world_rows()
        rows[40]["actors"][0]["state"]["pos"][0] = float("nan")
        with tempfile.TemporaryDirectory() as tmp:
            self.assertFalse(cross_worldline_gate(self._group(Path(tmp), d_rows=rows))["within_tolerance"])

    def test_a_release_with_nothing_on_the_causal_record_fails(self):
        def released_from(tick, actor):
            rows = world_rows()
            for r in rows[tick:]:
                for a in r["actors"]:
                    if a["id"] == actor:
                        a["released"] = 1
            return rows
        with tempfile.TemporaryDirectory() as tmp:   # the runtime let chat_a go in every take alike
            report = cross_worldline_gate(self._group(Path(tmp), a_rows=released_from(20, "chat_a"),
                                                      c_rows=released_from(20, "chat_a"), d_rows=released_from(20, "chat_a")))
            self.assertFalse(report["exact"])
            self.assertEqual(report["a_released"], ["chat_a"])
        with tempfile.TemporaryDirectory() as tmp:   # the takeover itself is on the record: fine
            rows = released_from(30, "pacer")
            report = cross_worldline_gate(self._group(Path(tmp), c_rows=rows))
            self.assertTrue(report["exact"], report["problems"])
        with tempfile.TemporaryDirectory() as tmp:   # released before its rule fired: not fine
            report = cross_worldline_gate(self._group(Path(tmp), d_rows=released_from(10, "chat_a"),
                                                      d_events=[event(50, "grab", ["chat_a"])]))
            self.assertFalse(report["exact"])

    def test_the_gate_command_passes_only_exact(self):
        from gta_capture import cli
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp), d_rows=world_rows(chat_dx=0.001, from_tick=10))
            self.assertEqual(cli.main(["stage-group-gate", "--group", str(group)]), 1)   # within tolerance, not exact
        with tempfile.TemporaryDirectory() as tmp:
            group = self._group(Path(tmp))
            self.assertEqual(cli.main(["stage-group-gate", "--group", str(group)]), 0)

    def test_plan_lines_are_read_by_position_whatever_a_clip_is_called(self):
        from gta_capture.stage_group import load_blend_windows
        plan = PLAN.replace("pair give_a start 40", "pair start start 40").replace("idle phone start 60", "idle blend start 60")
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "stage-plan.txt"
            path.write_text(plan, encoding="utf-8")
            windows = load_blend_windows(path)
            self.assertEqual(min(windows["chat_a"]), 40)
            self.assertEqual(windows["pacer"], set())   # a snap: no window
            path.write_text(PLAN.replace("blend 4.000000", "blend 2.000000"), encoding="utf-8")
            with self.assertRaises(ContractError):   # slower than the window covers
                load_blend_windows(path)


class GroupPreparationTests(unittest.TestCase):
    def test_a_bad_takeover_is_refused_before_anybody_plays(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = group_config(root)
            value = load_json(config)
            value["takeover"]["at_s"] = 3   # after this 90-tick stage ends
            write_json_atomic(config, value)
            with self.assertRaises(ContractError):
                prepare_group(config, root / "group")
            self.assertFalse((root / "group" / "group.json").exists())

    def test_a_refused_take_leaves_nothing_prepared(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=True)
            settings = load_json(group / "group.json")
            settings["takeover"]["at_s"] = 3
            write_json_atomic(group / "group.json", settings)
            with self.assertRaises(ContractError):
                prepare_take(group, "C")
            self.assertFalse((group / "takes" / "C" / "config" / "scenario.json").exists())
            settings["takeover"]["at_s"] = 1
            write_json_atomic(group / "group.json", settings)
            StageScenario.load(prepare_take(group, "C"))   # fixed, it prepares

    def test_the_tape_takes_its_hold_from_the_recorded_take(self):
        from gta_capture import cli
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = prepare_group(group_config(root), root / "group")
            write_take(group, "AB", world_rows(), [], cal_text=CAL, measured=True)
            TakeTapeTests()._logs(group / "takes" / "AB", "AB")
            settings = load_json(group / "group.json")
            settings["hold_s"] = 2.0   # edited after the take was recorded with 1 s
            write_json_atomic(group / "group.json", settings)
            self.assertEqual(cli.main(["stage-take-tape", "--group", str(group), "--take", "AB"]), 0)
            tape = load_json(group / "takes" / "AB" / "config" / "camera-tape.json")
            self.assertEqual(tape["source"]["hold_frames"], 30)


class TakeTapeTests(unittest.TestCase):
    def _logs(self, take_dir: Path, kind: str, *, skip=(), takeover=30):
        src = take_dir / "source"
        src.mkdir(parents=True, exist_ok=True)
        with open(src / "input-camera-log.jsonl", "w", encoding="utf-8") as f:
            for k in range(90):
                if k in skip:
                    continue
                row = {"schema": "dm-gta-input-camera-log/1", "tick": k, "game_ms": k * 33, "frames": 5, "keyCode": [87],
                       "mouse_dx": 0.01, "mouse_dy": 0.0, "events": [],
                       "game_cam": {"pos": [-1580.0 + k * 0.01, -1150.0, 4.0], "rot": [-5.0, 0.0, 100.0 + k], "fov": 50}}
                if kind == "AB":
                    row["free_cam"] = {"pos": [-1580.0, -1150.0 + k * 0.02, 3.6], "rot": [-4.0, 0.0, 90.0 + 0.5 * k], "fov": 50}
                    row["controlled"] = None
                elif kind == "D" or k >= takeover:
                    row["controlled"] = {"id": "pacer", "head": [-1575.0 + k * 0.01, -1146.0, 3.55],
                                         "pos": [-1575.0 + k * 0.01, -1146.0, 2.9], "h": 10.0}
                else:
                    row["controlled"] = None
                f.write(json.dumps(row) + "\n")
        rows = world_rows()
        for r in rows:   # on the stage; the pacer faces 45 degrees, head up at 3.5 m
            for a in r["actors"]:
                for p in [a["state"]["pos"], *a["state"]["bones"]]:
                    p[0] += STAGE_X
                    p[1] += STAGE_Y
                if a["id"] == "pacer":
                    a["state"]["h"] = 45.0
                    a["state"]["bones"][2] = [1.0 + STAGE_X, 1.0 + STAGE_Y, 3.5]
        with open(src / "master-world-telemetry.jsonl", "w", encoding="utf-8") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")

    def test_ab_c_d_tapes_validate_with_their_focals(self):
        # AB one lens each; C and D's first person 35 mm since 2026-10-16 (the customer: 18 mm was too wide; the meeting:
        # 35-50 mm), their fixed camera 24 like A's: a stage take may repeat a lens, any other clip still may not
        from gta_capture.stage_tape import build_take_tape
        from gta_capture.tape import validate_tape
        with tempfile.TemporaryDirectory() as tmp:
            for kind in ("AB", "C", "D"):
                take = Path(tmp) / kind
                self._logs(take, kind, skip=(40,))
                tape = build_take_tape(take, spec_text=SPEC, take_kind=kind, hold_s=1.0, duration_s=4.0, takeover_actor="pacer")
                self.assertEqual(tape["n_frames"], 120)
                self.assertEqual(sorted(l["focal_mm"] for l in tape["lanes"].values()),
                                 [18, 24, 35, 50] if kind == "AB" else [24, 35, 35, 50])
                if kind != "AB":
                    self.assertEqual((tape["lanes"]["1"]["focal_mm"], tape["lanes"]["4"]["focal_mm"]), (24, 35))
                    self.assertEqual(tape["lanes"]["4"]["samples"][0]["intrinsics"]["fx"], 1920 * 35 / 36)
                    self.assertIs(validate_tape(tape), tape)   # a stage take's tape loads again
                    with self.assertRaisesRegex(ContractError, "exactly once"):   # any other clip still one lens each
                        validate_tape({**tape, "source": {**tape["source"], "kind": "trajgen"}})
                logged_role = "free_camera" if kind == "AB" else "first_person"
                self.assertEqual(tape["source"]["interpolated_ticks"][logged_role], 1)

    def test_each_take_has_its_own_still_wide_fixed_camera(self):
        from gta_capture.stage_tape import FIXED, build_take_tape
        from gta_capture.tape import _look_at_euler
        spots = {}
        with tempfile.TemporaryDirectory() as tmp:
            for kind in ("AB", "C", "D"):
                take = Path(tmp) / kind
                self._logs(take, kind)
                tape = build_take_tape(take, spec_text=SPEC, take_kind=kind, hold_s=1.0, duration_s=4.0, takeover_actor="pacer")
                self.assertEqual(tape["lanes"]["1"]["focal_mm"], 24)
                if kind != "AB":
                    self.assertEqual(tape["lanes"]["3"]["focal_mm"], 35)   # the chase
                fixed = tape["lanes"]["1"]["samples"]
                self.assertEqual({(tuple(s["position"]), tuple(s["euler_deg"])) for s in fixed},
                                 {(tuple(fixed[0]["position"]), tuple(fixed[0]["euler_deg"]))})   # still, all take long
                recorded = tape["source"]["fixed"]
                self.assertEqual([round(v, 6) for v in fixed[0]["position"]], recorded["position"])
                for got, want in zip(fixed[0]["euler_deg"], _look_at_euler(recorded["position"], recorded["aim"])):
                    self.assertAlmostEqual(got, want, places=4)   # it looks at the aim the tape records
                (_, _, at_h), (_, _, aim_h) = FIXED[kind]
                self.assertAlmostEqual(recorded["position"][2] - recorded["aim"][2], at_h - aim_h, places=5)
                spots[kind] = tuple(recorded["position"])
        self.assertEqual(len(set(spots.values())), 3)   # A, C and D stand in different places

    def test_b_lane_is_the_flown_camera_and_hold_frames_hold_tick_zero(self):
        from gta_capture.stage_tape import build_take_tape
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "AB"
            self._logs(take, "AB", skip=(40,))
            tape = build_take_tape(take, spec_text=SPEC, take_kind="AB", hold_s=1.0, duration_s=4.0)
            b = tape["lanes"]["4"]["samples"]
            self.assertEqual(b[0]["position"], b[29]["position"])            # the hold shows tick 0
            self.assertAlmostEqual(b[30 + 10]["position"][1], -1150.0 + 0.2, places=6)
            self.assertAlmostEqual(b[30 + 40]["position"][1], -1150.0 + 0.8, places=6)   # skipped tick 40, interpolated
            self.assertAlmostEqual(b[30 + 10]["euler_deg"][2], 95.0, places=6)

    def test_first_person_rides_the_actor_until_the_takeover(self):
        from gta_capture.stage_tape import EYE_AHEAD_M, FP_SMOOTH_TICKS, build_take_tape
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "C"
            self._logs(take, "C", takeover=30)
            tape = build_take_tape(take, spec_text=SPEC, take_kind="C", hold_s=1.0, duration_s=4.0, takeover_actor="pacer")
            fp = tape["lanes"]["4"]["samples"]
            before = fp[30 + 10]   # tick 10: nobody played, the pacer's head facing his way
            self.assertEqual(before["euler_deg"], [0.0, 0.0, 45.0])
            self.assertAlmostEqual(before["position"][2], 3.5, places=6)
            # from the actor's way (45) to the played body's (10) at the takeover (tick 30) -- not to the game camera's
            # (100 + tick): the first person goes with the body, with no lag (2026-10-16); only the smoothing either side
            reach = int(3 * FP_SMOOTH_TICKS)
            self.assertAlmostEqual(fp[30 + 30 - reach - 1]["euler_deg"][2], 45.0, places=6)
            self.assertAlmostEqual(fp[30 + 30 + reach]["euler_deg"][2], 10.0, places=6)
            after = fp[30 + 75]   # round by then, level, 20 cm out along that way -- past the head
            self.assertEqual(after["euler_deg"][0], 0.0)
            self.assertAlmostEqual(after["euler_deg"][2], 10.0, delta=0.2)
            self.assertAlmostEqual(after["position"][2], 3.55, places=6)

    def test_the_standing_head_swing_is_measured(self):
        """The user, 2026-10-17: no idle head sway in the first person, which rides the head. beach-demo-113's played
        person swayed 5-8 cm sideways every second or two standing (GTA's own idle); every tape measures it now."""
        from gta_capture.stage_tape import EYE_AHEAD_M, FP_STAND_SWAY_MAX_M, _forward, standing_sway
        root = [10.0, 20.0, 1.0]
        f, left = _forward(0.0, 30.0), _forward(0.0, 120.0)
        def lane(amp):
            out = []
            for k in range(240):   # standing, facing 30 degrees, the head swaying amp either side every 1.6 s
                s = amp * math.sin(2 * math.pi * k / 48)
                head = [root[0] + s * left[0], root[1] + s * left[1], root[2] + 0.63]
                out.append(([head[i] + EYE_AHEAD_M * f[i] for i in range(3)], [0.0, 0.0, 30.0]))
            return out
        self.assertGreater(standing_sway(lane(0.04), [root] * 240), 0.07)    # 113's idle: over the bar
        self.assertLess(standing_sway(lane(0.002), [root] * 240), FP_STAND_SWAY_MAX_M)   # a calm stance: under it
        walking = [[root[0] + 0.04 * k, root[1], root[2]] for k in range(240)]
        self.assertEqual(standing_sway(lane(0.04), walking), 0.0)   # only standing counts
        self.assertEqual(standing_sway(lane(0.04), [root] * 240, played_from=200), 0.0)   # and only once played

    def test_first_person_looks_where_the_body_faces(self):
        """The customer, 2026-10-16: the first person goes with the head, which in GTA goes with the body; the camera
        is the third person's and comes round the person's side or face. Wherever the camera is, the eye looks the
        body's way, level (the user, 2026-10-18: the camera's pitch -- the mouse's -- nodded the first person standing)."""
        from gta_capture.stage_tape import first_person_view
        for body in range(-180, 180, 5):
            self.assertEqual(first_person_view(float(body)), [0.0, 0.0, float(body)])
        self.assertEqual(first_person_view(190.0), [0.0, 0.0, -170.0])

    def test_the_first_person_eye_is_out_of_the_head_whichever_way_it_looks(self):
        from gta_capture.stage_tape import EYE_AHEAD_M, _eye, _forward
        head = [10.0, 20.0, 3.0]
        for yaw in range(-180, 180, 15):
            for pitch in (-30.0, 0.0, 30.0):
                eye = _eye(head, [pitch, 0.0, float(yaw)])
                out = [eye[i] - head[i] for i in range(3)]
                self.assertAlmostEqual(math.sqrt(sum(c * c for c in out)), EYE_AHEAD_M, places=9)
                f = _forward(pitch, float(yaw))
                self.assertAlmostEqual(sum(out[i] * f[i] for i in range(3)), EYE_AHEAD_M, places=9)   # straight ahead of it

    def test_the_first_person_view_turns_with_the_body(self):
        """The game turns the body round as the keys go down, 178 degrees in 0.63 s (beach-demo-003 C, second 87),
        while the camera -- the mouse's -- goes its own way: the view is the body's, turning as it turns, no lag (the
        customer, 2026-10-16: "开始转身的时候得对上不能有延迟")."""
        from gta_capture.stage_tape import FP_SMOOTH_TICKS, _smooth_views, first_person_view, first_person_views
        def body(k):
            t = min(1.0, max(0.0, (k - 150) / 19.0))
            return 20.0 + 178.0 * t * t * (3.0 - 2.0 * t)
        wrap = lambda a: (a + 180.0) % 360.0 - 180.0
        played = {k: (wrap(body(k)), [0.0, 0.0, 0.0]) for k in range(300) if k != 100}   # a skipped tick
        views = first_person_views(played)
        self.assertEqual(sorted(views), sorted(played))
        for k, view in views.items():
            self.assertEqual(view, first_person_view(wrap(body(k))))
            self.assertAlmostEqual(wrap(view[2] - body(k)), 0.0, places=9)
        ticks = sorted(views)
        lane = _smooth_views([([0.0, 0.0, 0.0], views[k]) for k in ticks], FP_SMOOTH_TICKS)
        yaws = {k: view[2] for k, (_, view) in zip(ticks, lane)}
        # half way round (tick 159.5), the view is where the body is: the smoothing is the same either side, no lag
        self.assertAlmostEqual(wrap((yaws[159] + yaws[160]) / 2 - (body(159) + body(160)) / 2), 0.0, delta=0.2)
        self.assertGreater(yaws[140], body(140) + 0.1)   # ... and it begins to turn as the body does, not after
        self.assertAlmostEqual(wrap(yaws[299] - body(299)), 0.0, places=6)

    def test_the_chase_lane_is_the_game_follow_camera_behind_the_person(self):
        """GTA's follow camera put together behind the person: behind the actor to be taken over from tick 0 at the
        resting pitch, after the takeover turned by the game camera -- the mouse (2026-10-16: round the person's side
        or face as a player turns it) -- whose own place is never used: one view all take long, no leap at the takeover."""
        from gta_capture.stage_tape import (CHASE_MAX_ACCEL, FOLLOW_BACK_M, FOLLOW_REST_PITCH, FOLLOW_RIGHT_M, FOLLOW_UP_M,
                                            _forward, build_take_tape)
        pacer = [1.0 + STAGE_X, 1.0 + STAGE_Y, 2.0]
        with tempfile.TemporaryDirectory() as tmp:
            tapes = []
            for leap in (0.0, 1.75):
                take = Path(tmp) / f"C{leap}"
                self._logs(take, "C", takeover=30)
                path = take / "source" / "input-camera-log.jsonl"
                rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
                for r in rows:   # the taken body walks on from where the pacer stood, the view turning 1 degree a tick
                    k = r["tick"]
                    r["game_cam"]["rot"][2] = 45.0 + max(0, k - 30)
                    if r["controlled"]:
                        x = pacer[0] + (k - 30) * 0.01
                        r["controlled"].update(pos=[x, pacer[1], 2.0], head=[x, pacer[1], 2.65], h=45.0 + k - 30)
                    if k >= 31:   # the game camera leaps, or not
                        r["game_cam"]["pos"][0] += leap
                path.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
                tapes.append(build_take_tape(take, spec_text=SPEC, take_kind="C", hold_s=1.0, duration_s=4.0,
                                             takeover_actor="pacer"))
            self.assertEqual(tapes[0]["lanes"]["3"], tapes[1]["lanes"]["3"])
            report = tapes[0]["source"]["chase"]
            self.assertEqual((report["pulled_in_ticks"], report["crowded_ticks"]), (0, 0))
            chase = tapes[0]["lanes"]["3"]["samples"]
            steps = [math.dist(chase[f + 1]["position"], chase[f]["position"]) for f in range(len(chase) - 1)]
            self.assertLess(max(steps), 0.2)

            def behind(root, pitch, yaw):
                f, h = _forward(pitch, yaw), math.radians(yaw)
                return [root[0] + FOLLOW_RIGHT_M * math.cos(h) - FOLLOW_BACK_M * f[0],
                        root[1] + FOLLOW_RIGHT_M * math.sin(h) - FOLLOW_BACK_M * f[1],
                        root[2] + FOLLOW_UP_M - FOLLOW_BACK_M * f[2]]
            # behind the actor at the resting pitch, then after the played view, a swing behind it: turning 30 deg/s,
            # the swing brakes in time to stop on it -- (30^2 / 2 a) degrees behind as it sets the tick's rate, a
            # tick's turn less once it has moved
            lag = 30.0 ** 2 / (2.0 * CHASE_MAX_ACCEL) - 30.0 / 30
            # (tick 2, where the chase's smoothing does not yet reach past the takeover at 30)
            for k, root, pitch, yaw, behind_by in ((0, pacer, FOLLOW_REST_PITCH, 45.0, 0.0), (2, pacer, FOLLOW_REST_PITCH, 45.0, 0.0),
                                                   (70, [pacer[0] + 0.4, pacer[1], 2.0], -5.0, 85.0, lag)):   # the camera's own pitch
                s = chase[30 + k]
                self.assertAlmostEqual(s["euler_deg"][0], pitch, places=2)
                self.assertAlmostEqual(yaw - s["euler_deg"][2], behind_by, delta=0.3)
                for got, want in zip(s["position"], behind(root, pitch, s["euler_deg"][2])):
                    self.assertAlmostEqual(got, want, places=3)

    def test_the_chase_pulls_in_short_of_things_and_keeps_clear_of_people(self):
        from gta_capture.stage_camera_paths import BODY_RADIUS_M, ROOT_ABOVE_SAND_M
        from gta_capture.stage_tape import (FOLLOW_BACK_M, FOLLOW_CLEAR_M, FOLLOW_PERSON_M, FOLLOW_PITCH, FOLLOW_SAND_M,
                                            chase_lane)
        n = 120
        roots = [[0.0, 0.0, 1.0] for _ in range(n)]   # standing, facing +y: the camera stands toward -y
        views, played = [[-8.0, 0.0, 0.0]] * n, [True] * n
        nobody = [[] for _ in range(n)]
        poses, report = chase_lane(roots, views, played, nobody, [])
        self.assertEqual(report["pulled_in_ticks"], 0)
        # a pole on the boom: the camera stays short of it all take
        poses, report = chase_lane(roots, views, played, nobody, [(0.0, -4.0, 0.3, 3.0)])
        self.assertEqual(report["pulled_in_ticks"], n)
        for pos, _ in poses:
            self.assertGreaterEqual(math.hypot(pos[0], pos[1] + 4.0), 0.3 + FOLLOW_CLEAR_M - 1e-6)
            self.assertLess(-pos[1], 3.5)
        # somebody walking across where the camera stands: pulled in ahead of them, back out after
        walker = [[[-5.0 + 10.0 * k / (n - 1), -4.3, 1.0]] for k in range(n)]
        poses, report = chase_lane(roots, views, played, walker, [])
        self.assertGreater(report["pulled_in_ticks"], 0)
        self.assertAlmostEqual(-poses[0][0][1], FOLLOW_BACK_M * math.cos(math.radians(8.0)), places=3)
        for (pos, _), (q,) in zip(poses, walker):
            clear = math.hypot(pos[0] - q[0], pos[1] - q[1]) >= BODY_RADIUS_M + FOLLOW_PERSON_M - 1e-6
            self.assertTrue(clear or pos[2] >= q[2] + 0.85 + FOLLOW_PERSON_M)
        # looking far up, the pitch is held and the camera stays over the sand
        poses, _ = chase_lane(roots, [[60.0, 0.0, 0.0]] * n, played, nobody, [])
        for pos, rot in poses:
            self.assertAlmostEqual(rot[0], FOLLOW_PITCH[1], places=9)
            self.assertGreaterEqual(pos[2], 1.0 - ROOT_ABOVE_SAND_M + FOLLOW_SAND_M - 1e-9)


if __name__ == "__main__":
    unittest.main()
