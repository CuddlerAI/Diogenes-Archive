import importlib.util
import math
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "dense_pair_join", Path(__file__).parents[1] / "Analyze-DensePairJoin.py")
join = importlib.util.module_from_spec(spec)
spec.loader.exec_module(join)

IDENTITY = {"session_id": "s", "run_id": "r", "generation": 1}
QPF = 1_000_000  # one QPC tick per microsecond
FRAME_US = 33_333
STEP_M = 0.120   # walking distance per simulation state


def yaw(degrees):
    half = math.radians(degrees) / 2.0
    return [0.0, math.sin(half), 0.0, math.cos(half)]


def state(index):
    """Simulation state `index`: actor root, and an eye that bobs relative to it."""
    root = [STEP_M * index, 0.0, 0.0]
    eye = [root[0] + 0.004 * math.sin(index), 1.7, 0.0]
    return root, eye


def dense_entry(number, index, first_us, last_us, root=None):
    actor, eye = state(index)
    actor = actor if root is None else root
    return {
        "schema": "dm-cp2077-dense-pair/v1", "generation": 1, "entry": number,
        "first_begin_qpc": first_us, "first_end_qpc": first_us + 20,
        "last_begin_qpc": last_us, "last_end_qpc": last_us + 20, "reads": 3,
        "player_position": actor, "player_rotation": yaw(0), "player_words": f"p{actor[0]:.6f}",
        "camera_position": eye, "camera_rotation": yaw(0.5 * index), "camera_words": f"c{index}",
    }


def build(publish_us, late_state):
    """Present n renders state n. `publish_us[k]` is when memory starts holding state k."""
    cameras, players, dense = [], [], [
        {"schema": "dm-cp2077-dense-pair-header/v1", "identity": IDENTITY, "qpc_frequency": QPF}]
    frames = len(late_state)
    for n in range(frames):
        _, eye = state(n)
        # The rendered camera carries float-rounding noise relative to the memory camera.
        rendered = [eye[0] + 0.0011, eye[1], eye[2] - 0.0004]
        present = (n + 1) * FRAME_US
        cameras.append({"identity": IDENTITY, "present_seq": n, "present_qpc": present,
                        "camera_position": rendered, "camera_rotation_quaternion": yaw(0.5 * n)})
        players.append({"present_seq": n, "present_qpc": present,
                        "player_position": state(late_state[n])[0]})
    for k, begin in enumerate(publish_us):
        end = publish_us[k + 1] - 1_000 if k + 1 < len(publish_us) else begin + FRAME_US
        dense.append(dense_entry(k + 1, k, begin, end))
    return cameras, players, dense


class DensePairJoinTests(unittest.TestCase):
    def test_micro_degree_angles_keep_precision_and_sign_is_ignored(self):
        self.assertAlmostEqual(join.angle_deg(yaw(10.0), yaw(10.00001)), 1e-5, delta=1e-9)
        negated = [-v for v in yaw(37.0)]
        self.assertAlmostEqual(join.angle_deg(yaw(37.0), negated), 0.0, delta=1e-12)

    def test_stall_then_double_tick_is_joined_where_the_per_present_read_is_not(self):
        frames = 12
        # Normal phase: state k is published one frame before Present k renders it, so the read
        # at Present n already sees state n+1 (simulation leads render by one frame).
        publish = [k * FRAME_US - FRAME_US // 2 for k in range(frames + 1)]
        late = [n + 1 for n in range(frames)]
        # Stall: state 6 is not published until after Present 5, so that read repeats state 5.
        # Then two states land inside one render interval: state 6 lives for only 4 ms and no
        # per-Present read ever sees it, although Present 6 renders it.
        publish[6] = 6 * FRAME_US + 2_000
        publish[7] = 6 * FRAME_US + 6_000
        late[5] = 5
        self.assertNotIn(6, late)
        cameras, players, dense = build(publish, late)
        result = join.analyze(cameras, players, dense)

        self.assertEqual(result["presents"], frames)
        self.assertEqual(result["matched_presents"], frames)
        self.assertEqual(result["unmatched_presents"], 0)
        self.assertEqual(result["torn_pair_presents"], 0)
        self.assertLess(result["match_residual_mm"]["max"], 1.5)
        errors = result["moving_anchor_increment_error_mm"]
        # The per-Present read jumps a whole step around the stall; the join does not.
        self.assertGreater(errors["formal_late_sample"]["max"], 100.0)
        for rule in join.RULES:  # no torn pair here, so every rule agrees
            self.assertLess(errors["content_joined_by_rule"][rule]["max"], 10.0)
            self.assertAlmostEqual(
                errors["content_joined_by_rule"][rule]["rms"],
                errors["intrinsic_floor_same_entries_by_rule"][rule]["rms"], delta=1e-6)

    def test_state_never_sampled_stays_unmatched_and_future_entries_are_ineligible(self):
        frames = 6
        publish = [k * FRAME_US - FRAME_US // 2 for k in range(frames + 1)]
        cameras, players, dense = build(publish, [n + 1 for n in range(frames)])
        dense = [row for row in dense if row.get("camera_words") != "c3"]
        # An entry first seen only AFTER Present 4 cannot serve Present 4 in an online join.
        for row in dense:
            if row.get("camera_words") == "c4":
                row["first_begin_qpc"] = 5 * FRAME_US + 10
                row["last_end_qpc"] = 5 * FRAME_US + 5_000
        result = join.analyze(cameras, players, dense)
        self.assertEqual([u["present_seq"] for u in result["unmatched"]], [3, 4])
        self.assertGreater(result["unmatched"][0]["nearest_mm"], 100.0)
        # Removing state 3 leaves a hole of one whole step between its neighbours' entries.
        self.assertGreater(result["hole_between_consecutive_entries_ms"]["max"], 30.0)

    def test_root_first_update_order_is_measured_and_only_the_earliest_rule_survives_it(self):
        frames = 10
        publish = [k * FRAME_US - FRAME_US // 2 for k in range(frames + 1)]
        cameras, players, dense = build(publish, [n + 1 for n in range(frames)])
        # The engine writes the actor root 600 us before the camera, so for that long memory
        # holds (root k+1, camera k): the pair a Present-time read caught at P1490. A sampler
        # only sometimes lands inside that window; here it does on every even step.
        header, entries = dense[0], dense[1:]
        rebuilt = [header]
        for k, entry in enumerate(entries):
            rebuilt.append(entry)
            if k % 2 == 0 and k + 1 < len(entries):
                torn_begin = entry["last_begin_qpc"] + 100
                rebuilt.append(dense_entry(
                    100 + k, k, torn_begin, torn_begin + 600, root=state(k + 1)[0]))
        result = join.analyze(cameras, players, rebuilt)

        spans = result["update_order"]["entry_observed_span_ms_by_how_it_was_created"]
        # The torn pair is the short-lived kind. That lifetime asymmetry, not the alternation
        # of root-only and camera-only changes, is what reveals the write order.
        self.assertEqual(spans["root_only_change"]["count"], 5)
        self.assertAlmostEqual(spans["root_only_change"]["max"], 0.62, delta=1e-9)
        self.assertGreater(spans["camera_only_change"]["p50"], 25.0)
        self.assertGreater(spans["both_changed"]["count"], 0)
        self.assertEqual([t["present_seq"] for t in result["torn"]], [0, 2, 4, 6, 8])
        # Those five Presents have two candidates whose actor roots are one whole step apart.
        spread = result["actor_root_spread_among_multiple_candidates_mm"]
        self.assertEqual(spread["count"], 5)
        self.assertAlmostEqual(spread["max"], 1000.0 * STEP_M, delta=1e-6)
        errors = result["moving_anchor_increment_error_mm"]["content_joined_by_rule"]
        floor = result["moving_anchor_increment_error_mm"]["intrinsic_floor_same_entries_by_rule"]
        # The earliest entry of a camera content is the pair published with that camera.
        self.assertLess(errors["earliest"]["max"], 10.0)
        self.assertAlmostEqual(errors["earliest"]["rms"], floor["earliest"]["rms"], delta=1e-6)
        self.assertLess(errors["most_reads"]["max"], 10.0)
        # Taking the torn pair on even frames alternates between a whole step and none.
        self.assertGreater(errors["latest"]["p50"], 100.0)

    def test_entries_from_the_production_sampler_are_read_like_the_diagnostic_ones(self):
        frames = 4
        publish = [k * FRAME_US - FRAME_US // 2 for k in range(frames + 1)]
        cameras, players, dense = build(publish, [n + 1 for n in range(frames)])
        for row in dense:
            row["schema"] = row["schema"].replace("/v1", "/v2")
        self.assertEqual(join.analyze(cameras, players, dense)["matched_presents"], frames)
        dense[0]["schema"] = "dm-cp2077-dense-pair-header/v3"
        with self.assertRaises(SystemExit):
            join.analyze(cameras, players, dense)

    def test_foreign_identity_is_refused(self):
        cameras, players, dense = build([0, FRAME_US], [1])
        cameras[0]["identity"] = {**IDENTITY, "generation": 2}
        with self.assertRaises(SystemExit):
            join.analyze(cameras, players, dense)


if __name__ == "__main__":
    unittest.main()
