import json
from pathlib import Path
import tempfile
import unittest

import helpers  # noqa: F401  (puts the module root on sys.path)

from gta_capture.errors import ContractError
from gta_capture.trajectory import (CAM4_MODE_RULES, TRAJECTORY_FAMILIES,
                                    build_trajectory_verdict, granted_rules,
                                    verdict_for_run)


VERIFIABLE_TRAJECTORY_RULES = set(TRAJECTORY_FAMILIES) - {"covisibility"}


#: Every rule engine_qc emits for a passing four-lane mc clip, with the counts a
#: real report carries (per-lane rules repeat).  Taken from an actual
#: ``engine_qc check-clip --mode mc --json`` run over a placed trajgen clip.
BASE_PASS = {
    "dur.frames": 4, "frame.continuity": 4,
    "sync.frames": 1, "sync.frame_align": 1,
    "init.orientation": 1, "init.distance": 1,
    "covis.reading": 1, "covis.ratio": 1, "covis.ratio_strict": 1,
    "decouple.pairs": 1, "decouple.all4": 1,
    "phys.speed_p95": 4, "phys.speed_low": 4, "phys.speed_max": 3,
    "phys.accel_p95": 4, "phys.jerk_p95": 4, "phys.angspeed_p95": 4,
    "phys.angspeed_max": 4, "phys.angaccel_p95": 4, "phys.angjerk_p95": 4,
    "phys.speed_cap4": 1,
    "plan.pink_coverage": 4, "plan.pink_honesty": 24,
    "cam4.mode": 1,
}
TIMEWARP_ONLY = ("cam4.switch_count", "cam4.t_sw", "cam4.band_dwell")
TOPDOWN_ONLY = ("cam4.topdown_pitch", "cam4.topdown_alt", "cam4.topdown_route")


def _report(mode="topdown", *, fail=(), downgrade=()):
    """Build an engine_qc-shaped report for one declared fourth-lane form."""
    passing = dict(BASE_PASS)
    active = TOPDOWN_ONLY if mode == "topdown" else TIMEWARP_ONLY
    skipped = TIMEWARP_ONLY if mode == "topdown" else TOPDOWN_ONLY
    for rule in active:
        passing[rule] = 1
    # cam4's inactive form and cam4's roll exemption legitimately SKIP.
    results = [{"rule": rule, "status": "SKIP"} for rule in skipped]
    results.append({"rule": "phys.roll", "status": "SKIP"})
    for rule, count in passing.items():
        status = "FAIL" if rule in fail else ("SKIP" if rule in downgrade else "PASS")
        results.extend({"rule": rule, "status": status} for _ in range(count))
    verdict = "FAIL" if fail else "PASS"
    return {"verdict": verdict, "results": results}


class TrajectoryVerdictTests(unittest.TestCase):
    def test_clean_topdown_report_grants_only_verified_trajectory_families(self):
        verdict = build_trajectory_verdict(_report("topdown"), cam4_mode="topdown",
                                           qc_exit_code=0)
        self.assertEqual(set(verdict["granted_rules"]), VERIFIABLE_TRAJECTORY_RULES)
        self.assertEqual(granted_rules(verdict), VERIFIABLE_TRAJECTORY_RULES)
        self.assertEqual(set(verdict["planning_passed_rules"]), set(TRAJECTORY_FAMILIES))

    def test_clean_timewarp_report_grants_only_verified_trajectory_families(self):
        verdict = build_trajectory_verdict(_report("timewarp"), cam4_mode="timewarp",
                                           qc_exit_code=0)
        self.assertEqual(set(verdict["granted_rules"]), VERIFIABLE_TRAJECTORY_RULES)

    def test_topdown_results_cannot_satisfy_a_declared_timewarp_tape(self):
        """The tape declares the form; the report may not pick one after the fact."""
        verdict = build_trajectory_verdict(_report("topdown"), cam4_mode="timewarp",
                                           qc_exit_code=0)
        self.assertNotIn("cam4_reparameterization", verdict["granted_rules"])
        detail = verdict["rules"]["cam4_reparameterization"]
        self.assertEqual(detail["unsatisfied_required_rules"], sorted(TIMEWARP_ONLY))
        # The other families are independent and must still be granted.
        self.assertIn("motion_limits", verdict["granted_rules"])
        self.assertIn("covisibility", verdict["planning_passed_rules"])

    def test_one_failing_rule_withdraws_only_its_own_family(self):
        verdict = build_trajectory_verdict(_report("topdown", fail=("covis.ratio",)),
                                           cam4_mode="topdown", qc_exit_code=1)
        self.assertNotIn("covisibility", verdict["granted_rules"])
        # qc_exit_code != 0 is itself disqualifying for every family.
        self.assertEqual(verdict["granted_rules"], [])

    def test_a_failure_outside_the_required_list_still_withdraws_the_family(self):
        """``phys.speed_low`` is advisory, but a FAIL anywhere in phys is not ignorable."""
        verdict = build_trajectory_verdict(_report("topdown", fail=("phys.speed_low",)),
                                           cam4_mode="topdown", qc_exit_code=0)
        self.assertNotIn("motion_limits", verdict["granted_rules"])
        self.assertEqual(verdict["rules"]["motion_limits"]["failed_rules"],
                         ["phys.speed_low"])
        self.assertIn("trajectory_pr405", verdict["granted_rules"])
        self.assertIn("covisibility", verdict["planning_passed_rules"])

    def test_skipped_required_rule_is_not_a_pass(self):
        verdict = build_trajectory_verdict(
            _report("topdown", downgrade=("plan.pink_honesty",)),
            cam4_mode="topdown", qc_exit_code=0)
        self.assertNotIn("six_translation_honesty", verdict["granted_rules"])

    def test_missing_sidecar_family_grants_nothing(self):
        """No covis/plan sidecars means those rules never ran — absence is not proof."""
        report = _report("topdown")
        report["results"] = [row for row in report["results"]
                             if not row["rule"].startswith(("covis.", "decouple."))]
        verdict = build_trajectory_verdict(report, cam4_mode="topdown", qc_exit_code=0)
        self.assertNotIn("covisibility", verdict["granted_rules"])
        self.assertEqual(verdict["rules"]["covisibility"]["family_size"], 0)

    def test_unknown_cam4_mode_is_rejected(self):
        with self.assertRaises(ContractError):
            build_trajectory_verdict(_report(), cam4_mode="freeform", qc_exit_code=0)

    def test_malformed_report_is_rejected(self):
        with self.assertRaises(ContractError):
            build_trajectory_verdict({"verdict": "PASS"}, cam4_mode="topdown",
                                     qc_exit_code=0)
        with self.assertRaises(ContractError):
            build_trajectory_verdict({"results": [{"rule": 1, "status": "PASS"}]},
                                     cam4_mode="topdown", qc_exit_code=0)


class GrantedRulesTrustTests(unittest.TestCase):
    def test_a_handwritten_verdict_grants_nothing(self):
        for forged in (
            {"granted_rules": sorted(TRAJECTORY_FAMILIES)},
            {"schema": "something-else", "granted_rules": ["covisibility"],
             "qc_exit_code": 0},
            None,
            "covisibility",
        ):
            self.assertEqual(granted_rules(forged), set())

    def test_non_zero_qc_exit_grants_nothing_even_when_listed(self):
        verdict = build_trajectory_verdict(_report("topdown"), cam4_mode="topdown",
                                           qc_exit_code=0)
        verdict["qc_exit_code"] = 2
        self.assertEqual(granted_rules(verdict), set())

    def test_names_outside_this_module_are_never_granted(self):
        verdict = build_trajectory_verdict(_report("topdown"), cam4_mode="topdown",
                                           qc_exit_code=0)
        verdict["granted_rules"] = verdict["granted_rules"] + [
            "commercial_rights", "human_review", "projection_calibration"]
        self.assertEqual(granted_rules(verdict), VERIFIABLE_TRAJECTORY_RULES)

    def test_old_verdict_cannot_grant_unverified_geometry(self):
        old_verdict = {"schema": "dm-gta-trajectory-verdict/1", "qc_exit_code": 0,
                       "granted_rules": ["covisibility"],
                       "measured_in_gta": True}
        self.assertEqual(granted_rules(old_verdict), set())


class GateIntegrationTests(unittest.TestCase):
    def test_planar_proxy_and_self_declared_measured_geometry_never_grant_covisibility(self):
        for source, claimed_measured in (("synthetic_planar_proxy", False),
                                         ("gta_native", True)):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                (root / "capture").mkdir()
                (root / "capture" / "covis.json").write_text(json.dumps({
                    "geometry_source": source, "measured_in_gta": claimed_measured,
                }), encoding="utf-8")
                tape = {"duration_s": 60.0, "source": {
                    "kind": "trajgen_clip_placed", "cam4_mode": "topdown"}}
                verdict = verdict_for_run(
                    root, tape, stage=lambda _: None,
                    invoke=lambda *args, **kwargs: (0, _report(), ""))
                self.assertNotIn("covisibility", verdict["granted_rules"])
                self.assertIn("covisibility", verdict["planning_passed_rules"])
                detail = verdict["rules"]["covisibility"]
                self.assertFalse(detail["measured_geometry_verified"])
                self.assertEqual(detail["source_provenance"]["declared_geometry_source"], source)
                self.assertEqual(len(detail["source_provenance"]["sidecar_sha256"]), 64)

    def test_every_family_name_is_a_real_gate_rule(self):
        from gta_capture.gate import REQUIRED_RULES
        self.assertLessEqual(set(TRAJECTORY_FAMILIES), REQUIRED_RULES)

    def test_cam4_mode_rules_cover_both_documented_forms(self):
        self.assertEqual(set(CAM4_MODE_RULES), {"timewarp", "topdown"})


if __name__ == "__main__":
    unittest.main()
