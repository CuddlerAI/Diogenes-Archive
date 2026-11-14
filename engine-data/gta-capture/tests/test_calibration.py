import json
import math
import random
import tempfile
import unittest
from pathlib import Path

import helpers  # noqa: F401  (puts the module root on sys.path)

from gta_capture.calibration import (FILMBACK_AGREEMENT_MM, MIN_FOCAL_GROUPS,
                                     MIN_SAMPLES_PER_FOCAL,
                                     PROJECTION_SCHEMA, build_projection_evidence,
                                     camera_basis, load_samples, project,
                                     solve_projection)
from gta_capture.errors import ContractError
from gta_capture.tape import _euler_zxy_deg, _quaternion_zxy


WIDTH, HEIGHT = 1920, 1080
TRUE_FILMBACK_MM = 36.0
FOCALS = (18, 24, 35, 50)


def _pose(rng):
    euler = [rng.uniform(-12.0, 12.0), 0.0, rng.uniform(-180.0, 180.0)]
    return ([rng.uniform(-40.0, 40.0), rng.uniform(-40.0, 40.0), rng.uniform(2.0, 8.0)],
            _quaternion_zxy(euler))


def _synthetic_samples(seed=7, *, filmback_by_focal=None, pixel_noise_px=0.0,
                       per_focal=12):
    """Render correspondences through a known pinhole and hand back the pixels.

    Generating from the same projection the solver inverts is the point: the test
    asserts the solver recovers the constant it was never told, and that injected
    noise shows up in the reported residual rather than being absorbed.
    """
    rng = random.Random(seed)
    samples = []
    for focal in FOCALS:
        filmback = (filmback_by_focal or {}).get(focal, TRUE_FILMBACK_MM)
        fx = fy = WIDTH * focal / filmback
        produced = 0
        while produced < per_focal:
            position, quaternion = _pose(rng)
            right, forward, up = camera_basis(quaternion)
            depth = rng.uniform(6.0, 60.0)
            # Aim for a point that lands somewhere across the whole frame.
            target_u = rng.uniform(0.06 * WIDTH, 0.94 * WIDTH)
            target_v = rng.uniform(0.06 * HEIGHT, 0.94 * HEIGHT)
            x_cam = (target_u - WIDTH / 2.0) * depth / fx
            y_cam = (HEIGHT / 2.0 - target_v) * depth / fy
            world = [position[axis] + right[axis] * x_cam + up[axis] * y_cam
                     + forward[axis] * depth for axis in range(3)]
            u, v, _ = project(world, position=position, quaternion=quaternion,
                              fx=fx, fy=fy, cx=WIDTH / 2.0, cy=HEIGHT / 2.0)
            if pixel_noise_px:
                u += rng.gauss(0.0, pixel_noise_px)
                v += rng.gauss(0.0, pixel_noise_px)
            if not (0.0 <= u <= WIDTH and 0.0 <= v <= HEIGHT):
                continue
            samples.append({"focal_mm": focal, "world": world, "pixel": [u, v],
                            "camera": {"position": position, "quaternion": quaternion}})
            produced += 1
    return samples


def _samples_document(samples):
    return {
        "schema": "dm-gta-calibration-samples/1",
        "game_build": "1.0.3889.0",
        "graphics_api": "DX11",
        "captured_at": "2026-09-08T00:00:00Z",
        "method": "surveyed world markers photographed at four focal lengths",
        "render": {"width": WIDTH, "height": HEIGHT},
        "samples": samples,
    }


class CameraBasisTests(unittest.TestCase):
    def test_identity_quaternion_is_gta_right_forward_up(self):
        right, forward, up = camera_basis([0.0, 0.0, 0.0, 1.0])
        self.assertEqual(right, (1.0, 0.0, 0.0))
        self.assertEqual(forward, (0.0, 1.0, 0.0))
        self.assertEqual(up, (0.0, 0.0, 1.0))

    def test_basis_agrees_with_the_tape_euler_reads(self):
        """tape.py inverts three components of this matrix; they must be one matrix."""
        rng = random.Random(3)
        for _ in range(64):
            euler = [rng.uniform(-80.0, 80.0), 0.0, rng.uniform(-180.0, 180.0)]
            quaternion = _quaternion_zxy(euler)
            right, forward, up = camera_basis(quaternion)
            pitch, roll, yaw = _euler_zxy_deg(quaternion)
            self.assertAlmostEqual(math.degrees(math.asin(forward[2])), pitch, places=9)
            self.assertAlmostEqual(
                math.degrees(math.atan2(-right[2], up[2])), roll, places=9)
            self.assertAlmostEqual(
                math.degrees(math.atan2(-forward[0], forward[1])), yaw, places=9)

    def test_basis_is_orthonormal(self):
        rng = random.Random(11)
        for _ in range(32):
            _, quaternion = _pose(rng)
            vectors = camera_basis(quaternion)
            for first in range(3):
                self.assertAlmostEqual(
                    math.sqrt(sum(component ** 2 for component in vectors[first])),
                    1.0, places=12)
                for second in range(first + 1, 3):
                    self.assertAlmostEqual(
                        sum(a * b for a, b in zip(vectors[first], vectors[second])),
                        0.0, places=12)


class SolverTests(unittest.TestCase):
    def test_solver_recovers_the_filmback_it_was_never_told(self):
        solution = solve_projection(_synthetic_samples(), width=WIDTH, height=HEIGHT)
        self.assertAlmostEqual(solution["filmback_mm"], TRUE_FILMBACK_MM, places=6)
        self.assertLess(solution["reprojection"]["error_px"], 1e-6)
        self.assertEqual(solution["reprojection"]["metric"], "rms_euclidean_px")
        self.assertEqual(solution["reprojection"]["sample_count"], 12 * len(FOCALS))
        for item in solution["per_focal"]:
            self.assertAlmostEqual(item["fx"], WIDTH * item["focal_mm"] / TRUE_FILMBACK_MM,
                                   places=6)
            self.assertAlmostEqual(item["aspect_ratio"], 1.0, places=9)

    def test_measurement_noise_lands_in_the_reported_residual(self):
        solution = solve_projection(_synthetic_samples(seed=9, pixel_noise_px=1.5),
                                    width=WIDTH, height=HEIGHT)
        self.assertGreater(solution["reprojection"]["error_px"], 0.5)
        self.assertLess(solution["reprojection"]["error_px"], 4.0)
        # The fit still recovers the filmback; noise degrades precision, not truth.
        self.assertAlmostEqual(solution["filmback_mm"], TRUE_FILMBACK_MM, delta=0.2)

    def test_two_focal_groups_solve_without_claiming_complete_mc_evidence(self):
        samples = [row for row in _synthetic_samples() if row["focal_mm"] in (18, 35)]
        solution = solve_projection(samples, width=WIDTH, height=HEIGHT)
        self.assertEqual({item["focal_mm"] for item in solution["per_focal"]}, {18, 35})
        self.assertAlmostEqual(solution["filmback_mm"], TRUE_FILMBACK_MM, places=6)
        self.assertLess(solution["reprojection"]["error_px"], 1e-6)
        with self.assertRaisesRegex(ContractError, "all four focal groups"):
            build_projection_evidence(_samples_document(samples), calibration_id="c1",
                                      filmback_id="f1", metric_scale=1.0)

    def test_inconsistent_filmback_across_focals_is_rejected(self):
        """A wrong focal->FOV constant reprojects perfectly at its own focal length."""
        skewed = _synthetic_samples(
            seed=13, filmback_by_focal={50: TRUE_FILMBACK_MM + 4.0})
        with self.assertRaises(ContractError) as caught:
            solve_projection(skewed, width=WIDTH, height=HEIGHT)
        self.assertIn("filmback", str(caught.exception))
        self.assertIn(f"limit {FILMBACK_AGREEMENT_MM}", str(caught.exception))

    def test_too_few_samples_per_focal_is_rejected(self):
        samples = _synthetic_samples(per_focal=MIN_SAMPLES_PER_FOCAL - 1)
        with self.assertRaises(ContractError) as caught:
            solve_projection(samples, width=WIDTH, height=HEIGHT)
        self.assertIn("samples", str(caught.exception))

    def test_samples_clustered_in_the_frame_centre_are_rejected(self):
        rng = random.Random(5)
        samples = []
        for focal in FOCALS:
            fx = fy = WIDTH * focal / TRUE_FILMBACK_MM
            for _ in range(MIN_SAMPLES_PER_FOCAL + 2):
                position, quaternion = _pose(rng)
                right, forward, up = camera_basis(quaternion)
                depth = rng.uniform(10.0, 30.0)
                x_cam = rng.uniform(-20.0, 20.0) * depth / fx
                y_cam = rng.uniform(-20.0, 20.0) * depth / fy
                world = [position[axis] + right[axis] * x_cam + up[axis] * y_cam
                         + forward[axis] * depth for axis in range(3)]
                u, v, _ = project(world, position=position, quaternion=quaternion,
                                  fx=fx, fy=fy, cx=WIDTH / 2.0, cy=HEIGHT / 2.0)
                samples.append({"focal_mm": focal, "world": world, "pixel": [u, v],
                                "camera": {"position": position,
                                           "quaternion": quaternion}})
        with self.assertRaises(ContractError) as caught:
            solve_projection(samples, width=WIDTH, height=HEIGHT)
        self.assertIn("of the frame", str(caught.exception))

    def test_a_single_focal_length_cannot_vouch_for_the_filmback(self):
        """One focal group makes the cross-focal check vacuous, so it is refused.

        The samples below are perfect — zero noise, full-frame spread — and built
        on a *wrong* filmback.  At one focal length that error is invisible: the fit
        absorbs it into fx and reprojects to ~0 px.  Emitting evidence here would
        report filmback_spread_mm = 0.0, i.e. "perfect agreement" with nothing.
        """
        wrong = {focal: TRUE_FILMBACK_MM + 6.0 for focal in FOCALS}
        only = [sample for sample in _synthetic_samples(filmback_by_focal=wrong)
                if sample["focal_mm"] == FOCALS[0]]
        self.assertGreaterEqual(len(only), MIN_SAMPLES_PER_FOCAL)
        with self.assertRaises(ContractError) as caught:
            solve_projection(only, width=WIDTH, height=HEIGHT)
        self.assertIn("focal length", str(caught.exception))
        self.assertIn(f"at least {MIN_FOCAL_GROUPS}", str(caught.exception))

    def test_point_behind_the_camera_is_rejected(self):
        with self.assertRaises(ContractError):
            project([0.0, -5.0, 0.0], position=[0.0, 0.0, 0.0],
                    quaternion=[0.0, 0.0, 0.0, 1.0], fx=960.0, fy=960.0,
                    cx=960.0, cy=540.0)

    def test_pixel_outside_the_frame_is_rejected(self):
        samples = _synthetic_samples()
        samples[0]["pixel"] = [WIDTH + 10.0, 10.0]
        with self.assertRaises(ContractError) as caught:
            solve_projection(samples, width=WIDTH, height=HEIGHT)
        self.assertIn("outside the frame", str(caught.exception))


class EvidenceDocumentTests(unittest.TestCase):
    def test_evidence_matches_the_shape_preflight_re_reads(self):
        document = _samples_document(_synthetic_samples())
        evidence = build_projection_evidence(
            document, calibration_id="gta-legacy-3889-v1",
            filmback_id="gta-poc-36mm-horizontal", metric_scale=1.0)
        self.assertEqual(evidence["schema"], PROJECTION_SCHEMA)
        self.assertEqual(evidence["approval_status"], "draft")
        self.assertEqual(evidence["reprojection"]["metric"], "rms_euclidean_px")
        self.assertEqual(evidence["render"],
                         {"width": WIDTH, "height": HEIGHT, "graphics_api": "DX11"})
        self.assertEqual(evidence["game_build"], "1.0.3889.0")
        self.assertEqual(evidence["metric_scale"], 1.0)

    def test_metric_scale_is_carried_not_fitted(self):
        document = _samples_document(_synthetic_samples())
        evidence = build_projection_evidence(
            document, calibration_id="c1", filmback_id="f1", metric_scale=0.97)
        self.assertEqual(evidence["metric_scale"], 0.97)
        self.assertIn("not fitted", evidence["metric_scale_source"])
        for bad in (0.0, -1.0, float("nan"), "1.0"):
            with self.assertRaises(ContractError):
                build_projection_evidence(document, calibration_id="c1",
                                          filmback_id="f1", metric_scale=bad)

    def test_an_unreviewed_measurement_is_draft_unless_explicitly_approved(self):
        """Forgetting the flag must not stamp a fresh solve as approved."""
        document = _samples_document(_synthetic_samples())
        evidence = build_projection_evidence(
            document, calibration_id="c1", filmback_id="f1", metric_scale=1.0)
        self.assertEqual(evidence["approval_status"], "draft")
        with self.assertRaises(ContractError):
            build_projection_evidence(document, calibration_id="c1", filmback_id="f1",
                                      metric_scale=1.0, approval_status="signed-off")

    def test_approved_status_requires_explicit_selection(self):
        evidence = build_projection_evidence(
            _samples_document(_synthetic_samples()), calibration_id="c1",
            filmback_id="f1", metric_scale=1.0, approval_status="approved")
        self.assertEqual(evidence["approval_status"], "approved")

    def test_cli_default_does_not_approve_a_measurement(self):
        from gta_capture.cli import build_parser
        args = build_parser().parse_args([
            "solve-calibration", "--samples", "samples.json", "--out", "projection.json",
            "--calibration-id", "c1", "--filmback-id", "f1", "--metric-scale", "1.0"])
        self.assertEqual(args.approval_status, "draft")

    def test_missing_focal_group_cannot_emit_projection_evidence(self):
        for missing in FOCALS:
            samples = [row for row in _synthetic_samples() if row["focal_mm"] != missing]
            for approval_status in ("draft", "approved"):
                with self.subTest(missing=missing, approval_status=approval_status):
                    with self.assertRaisesRegex(ContractError, "all four focal groups"):
                        build_projection_evidence(_samples_document(samples), calibration_id="c1",
                            filmback_id="f1", metric_scale=1.0, approval_status=approval_status)

    def test_samples_file_requires_provenance(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "samples.json"
            document = _samples_document(_synthetic_samples())
            path.write_text(json.dumps(document), encoding="utf-8")
            self.assertEqual(load_samples(path)["game_build"], "1.0.3889.0")
            for key in ("game_build", "graphics_api", "captured_at", "method"):
                broken = dict(document)
                broken.pop(key)
                path.write_text(json.dumps(broken), encoding="utf-8")
                with self.assertRaises(ContractError):
                    load_samples(path)
            broken = dict(document)
            broken["schema"] = "dm-gta-calibration-samples/9"
            path.write_text(json.dumps(broken), encoding="utf-8")
            with self.assertRaises(ContractError):
                load_samples(path)


if __name__ == "__main__":
    unittest.main()
