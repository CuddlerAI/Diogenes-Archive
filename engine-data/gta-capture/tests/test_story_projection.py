"""Synthetic fake-pipe contract tests; no game calls or calibration evidence."""

import copy
from contextlib import redirect_stderr, redirect_stdout
import io
import json
import math
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch
import zlib

import helpers  # noqa: F401

from gta_capture import cli
from gta_capture.errors import BackendError, ContractError
from gta_capture.jsonio import sha256_file
from gta_capture.protocol import DEFAULT_PIPE
from gta_capture.story_projection import (
    OBSERVATION_SCHEMA, REQUEST_SCHEMA, capture_sample, validate_request,
)


def _request():
    return {
        "schema": REQUEST_SCHEMA,
        "camera_position": [0.0, 0.0, 2.0],
        "camera_euler_deg": [0.0, 0.0, 0.0],
        "point_world": [0.0, 10.0, 2.0],
        "fov_deg": 60.0,
        "nominal_focal_mm": 35,
        "marker_mode": "world_cross",
    }


def _synthetic_png(width=1, height=1):
    """A complete, deliberately synthetic RGBA PNG with valid chunk CRCs."""
    def chunk(kind, body):
        return (struct.pack(">I", len(body)) + kind + body
                + struct.pack(">I", zlib.crc32(kind + body) & 0xffffffff))

    pixels = b"".join(b"\0" + b"\xff\0\xff\xff" * width for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b""))


class _FakePipe:
    """Simulate native response fields solely inside TemporaryDirectory."""
    def __init__(self, *, mutate=None, hello=None, fail=None, png_bytes=None):
        self.mutate = mutate
        self.hello = ({"story_mode": True, "online_session": False}
                      if hello is None else hello)
        self.fail = fail
        self.png_bytes = _synthetic_png() if png_bytes is None else png_bytes
        self.calls = []
        self.opened = []
        self.closed = False

    def factory(self, pipe):
        self.opened.append(pipe)
        return self

    def request(self, command, payload=None):
        self.calls.append((command, copy.deepcopy(payload)))
        if command == "hello":
            return copy.deepcopy(self.hello)
        if command != "story_projection_sample":
            raise AssertionError(f"unexpected fake-pipe command: {command}")
        if self.fail is not None:
            raise self.fail
        output = Path(payload["output_path"])
        output.write_bytes(self.png_bytes)
        result = {
            "schema": OBSERVATION_SCHEMA, "mock": False,
            "approval_status": "draft", "editor_equivalence_verified": False,
            "delivery_eligible": False, "output_path": str(output),
            "output_bytes": output.stat().st_size, "output_sha256": sha256_file(output),
            "width": 1, "height": 1, "request_sha256": payload["request_sha256"],
        }
        if self.mutate:
            self.mutate(payload, result)
        return result

    def close(self):
        self.closed = True


class StoryProjectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="story-projection-unit-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.request_path = self.root / "original-request.json"
        self.request_path.write_text(json.dumps(_request()), encoding="utf-8")
        self.attempt = 0

    def _output(self):
        self.attempt += 1
        return self.root / f"synthetic-attempt-{self.attempt}"

    def _capture(self, fake, output=None):
        return capture_sample(self.request_path, output or self._output(),
                              pipe="unit-test-pipe-never-opened", client_factory=fake.factory)

    def test_request_exact_fields_and_legal_boundaries(self):
        for focal in (18, 24, 35, 50):
            for fov in (5, 120):
                for marker in ("world_cross", "none"):
                    request = _request()
                    request.update(nominal_focal_mm=focal, fov_deg=fov, marker_mode=marker,
                                   camera_position=[-20000, 20000, 0],
                                   point_world=[20000, -20000, 0],
                                   camera_euler_deg=[-360, 360, 0])
                    with self.subTest(focal=focal, fov=fov, marker=marker):
                        self.assertEqual(validate_request(request), request)

    def test_missing_extra_and_unknown_request_fields_are_rejected(self):
        invalid = [None, [], {**_request(), "approved": True},
                   {**_request(), "schema": "dm-gta-story-projection-request/unknown"},
                   {**_request(), "marker_mode": "screen_overlay"}]
        for key in _request():
            request = _request()
            del request[key]
            invalid.append(request)
        for document in invalid:
            with self.subTest(document=document), self.assertRaises(ContractError):
                validate_request(document)

    def test_request_numbers_reject_bool_nonfinite_and_overflow(self):
        invalid_numbers = (True, None, "1", math.nan, math.inf, -math.inf, 10 ** 1000)
        for field in ("camera_position", "camera_euler_deg", "point_world",
                      "fov_deg", "nominal_focal_mm"):
            for value in invalid_numbers:
                request = _request()
                request[field] = [value, 0, 0] if isinstance(request[field], list) else value
                with self.subTest(field=field, value=value), self.assertRaises(ContractError):
                    validate_request(request)

    def test_request_vectors_and_numeric_ranges_are_enforced(self):
        cases = [(field, value)
                 for field in ("camera_position", "camera_euler_deg", "point_world")
                 for value in ([], [0, 0], [0, 0, 0, 0], (0, 0, 0))]
        cases += [("camera_position", [20001, 0, 0]),
                  ("point_world", [0, -20001, 0]),
                  ("camera_euler_deg", [0, 0, 361]),
                  ("fov_deg", 4.999), ("fov_deg", 120.001),
                  ("nominal_focal_mm", 0), ("nominal_focal_mm", 36)]
        for field, value in cases:
            request = _request()
            request[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ContractError):
                validate_request(request)

    def test_capture_preserves_request_and_sends_exact_bound_payload(self):
        fake = _FakePipe()
        original = self.request_path.read_bytes()
        output = self._output()
        result = self._capture(fake, output)
        self.assertEqual(fake.opened, ["unit-test-pipe-never-opened"])
        self.assertEqual([command for command, _ in fake.calls], ["hello", "story_projection_sample"])
        self.assertEqual(fake.calls[1][1], {
            "request_path": str(output / "request.json"),
            "request_sha256": sha256_file(output / "request.json"),
            "output_path": str(output / "point.png"),
        })
        self.assertTrue(fake.closed)
        self.assertEqual(self.request_path.read_bytes(), original)
        self.assertEqual(json.loads((output / "request.json").read_text("utf-8")), _request())
        self.assertEqual(json.loads((output / "observation.json").read_text("utf-8")), result)
        self.assertEqual(result["output_sha256"], sha256_file(output / "point.png"))
        self.assertEqual((result["width"], result["height"]), (1, 1))
        self.assertEqual(result["approval_status"], "draft")
        self.assertIs(result["editor_equivalence_verified"], False)

    def test_existing_output_is_not_overwritten_or_sent_to_native(self):
        for populated in (False, True):
            with self.subTest(populated=populated):
                output = self._output()
                output.mkdir()
                sentinel = output / "keep.txt"
                if populated:
                    sentinel.write_bytes(b"previous attempt")
                fake = _FakePipe()
                with self.assertRaises(FileExistsError):
                    self._capture(fake, output)
                self.assertEqual(fake.opened, [])
                if populated:
                    self.assertEqual(sentinel.read_bytes(), b"previous attempt")

    def test_invalid_request_is_rejected_before_output_or_pipe(self):
        self.request_path.write_text(json.dumps({**_request(), "fov_deg": True}), encoding="utf-8")
        fake, output = _FakePipe(), self._output()
        with self.assertRaises(ContractError):
            self._capture(fake, output)
        self.assertEqual(fake.opened, [])
        self.assertFalse(output.exists())

    def test_offline_story_flags_are_required_and_client_is_closed(self):
        for hello in ({}, {"story_mode": False, "online_session": False},
                      {"story_mode": True, "online_session": True},
                      {"story_mode": True, "online_session": 0},
                      {"story_mode": 1, "online_session": False}):
            fake, output = _FakePipe(hello=hello), self._output()
            with self.subTest(hello=hello), self.assertRaisesRegex(ContractError, "offline Story"):
                self._capture(fake, output)
            self.assertTrue(fake.closed)
            self.assertEqual(len(fake.calls), 1)
            self.assertFalse((output / "observation.json").exists())

    def test_native_failure_closes_client_and_does_not_publish_observation(self):
        fake, output = _FakePipe(fail=BackendError("synthetic native failure")), self._output()
        with self.assertRaisesRegex(BackendError, "synthetic native failure"):
            self._capture(fake, output)
        self.assertTrue(fake.closed)
        self.assertFalse((output / "observation.json").exists())

    def test_observation_cannot_claim_mock_approval_or_editor_equivalence(self):
        changes = [("schema", "wrong"), ("mock", True), ("mock", 0),
                   ("approval_status", "approved"), ("approval_status", None),
                   ("editor_equivalence_verified", True), ("editor_equivalence_verified", 0),
                   ("delivery_eligible", True), ("delivery_eligible", None)]
        for key, value in changes:
            fake = _FakePipe(mutate=lambda payload, result: result.update({key: value}))
            output = self._output()
            with self.subTest(key=key, value=value), self.assertRaisesRegex(ContractError, "unapproved diagnostic"):
                self._capture(fake, output)
            self.assertTrue(fake.closed)
            self.assertFalse((output / "observation.json").exists())

    def test_png_path_size_hash_and_dimensions_are_bound(self):
        changes = [("output_path", str(self.root / "other.png")),
                   ("output_bytes", 0), ("output_sha256", "a" * 64),
                   ("width", 2), ("height", 2), ("width", True), ("height", True),
                   ("width", 1.0), ("height", math.inf), ("height", 0)]
        for key, value in changes:
            fake = _FakePipe(mutate=lambda payload, result: result.update({key: value}))
            output = self._output()
            with self.subTest(key=key, value=value), self.assertRaises(ContractError):
                self._capture(fake, output)
            self.assertFalse((output / "observation.json").exists())

    def test_png_must_exist_and_have_png_signature_and_ihdr(self):
        for payload in (b"not PNG", b"X" + _synthetic_png()[1:],
                        _synthetic_png()[:12] + b"NOPE" + _synthetic_png()[16:]):
            with self.subTest(payload=payload[:16]), self.assertRaisesRegex(ContractError, "native PNG"):
                self._capture(_FakePipe(png_bytes=payload))
        fake = _FakePipe(mutate=lambda payload, result: Path(payload["output_path"]).unlink())
        with self.assertRaisesRegex(ContractError, "PNG missing"):
            self._capture(fake)

    def test_request_hash_and_original_bytes_cannot_change(self):
        fake = _FakePipe(mutate=lambda payload, result: result.update(request_sha256="0" * 64))
        with self.assertRaisesRegex(ContractError, "request changed"):
            self._capture(fake)
        fake = _FakePipe(mutate=lambda payload, result: self.request_path.write_bytes(b"changed"))
        with self.assertRaisesRegex(ContractError, "request changed"):
            self._capture(fake)

    def test_snapshot_rewrite_cannot_be_rebound_to_new_response_hash(self):
        def rewrite(payload, result):
            snapshot = Path(payload["request_path"])
            document = json.loads(snapshot.read_text("utf-8"))
            document["point_world"][0] = 99.0
            snapshot.write_text(json.dumps(document), encoding="utf-8")
            result["request_sha256"] = sha256_file(snapshot)

        output = self._output()
        with self.assertRaisesRegex(ContractError, "request changed"):
            self._capture(_FakePipe(mutate=rewrite), output)
        self.assertFalse((output / "observation.json").exists())

    def test_cli_wires_paths_pipe_and_draft_output_through_fake_client(self):
        for selected_pipe in (None, "unit-custom-pipe"):
            fake, output, stdout = _FakePipe(), self._output(), io.StringIO()

            def capture(request, out, *, pipe):
                return capture_sample(request, out, pipe=pipe, client_factory=fake.factory)

            argv = ["sample-story-projection", "--request", str(self.request_path),
                    "--out-dir", str(output)]
            if selected_pipe:
                argv.extend(["--pipe", selected_pipe])
            with patch.object(cli, "capture_sample", side_effect=capture) as bridge, redirect_stdout(stdout):
                self.assertEqual(cli.main(argv), 0)
            bridge.assert_called_once_with(self.request_path, output, pipe=selected_pipe or DEFAULT_PIPE)
            self.assertEqual(fake.opened, [selected_pipe or DEFAULT_PIPE])
            response = json.loads(stdout.getvalue())
            self.assertEqual(response["approval_status"], "draft")
            self.assertIs(response["editor_equivalence_verified"], False)
            self.assertEqual(response["observation"], str(output / "observation.json"))

    def test_cli_returns_failure_on_contract_error(self):
        stderr = io.StringIO()
        with (patch.object(cli, "capture_sample", side_effect=ContractError("synthetic contract failure")),
              redirect_stderr(stderr), redirect_stdout(io.StringIO())):
            code = cli.main(["sample-story-projection", "--request", str(self.request_path),
                             "--out-dir", str(self._output())])
        self.assertEqual(code, 1)
        self.assertIn("ContractError: synthetic contract failure", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
