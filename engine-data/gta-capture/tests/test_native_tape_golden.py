"""camera-tape.bin cross-language golden contract, Python side.

The C++ half (native/src/native_tape_selftest.cpp, run by CTest) decodes the
same committed fixture. Together they pin the binary layout on both sides; a
writer or reader drift fails CI instead of a capture rig.
"""
import tempfile
import unittest
from pathlib import Path

from helpers import MODULE_ROOT  # noqa: F401 - importing helpers installs the package root
from gta_capture.jsonio import load_json, sha256_file
from gta_capture.native_tape import inspect_native_tape, write_native_tape

FIXTURES = Path(__file__).resolve().parent / "fixtures"
GOLDEN_JSON = FIXTURES / "golden-camera-tape.json"
GOLDEN_BIN = FIXTURES / "golden-camera-tape.bin"


class NativeTapeGoldenTests(unittest.TestCase):
    def test_writer_reproduces_committed_golden_bytes(self):
        tape = load_json(GOLDEN_JSON)
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "camera-tape.bin"
            write_native_tape(out, tape, json_path=GOLDEN_JSON)
            self.assertEqual(out.read_bytes(), GOLDEN_BIN.read_bytes(),
                             "write_native_tape output drifted from the sealed "
                             "golden fixture; the C++ reader contract is pinned "
                             "to those exact bytes")

    def test_golden_envelope_matches_fixture_json(self):
        envelope = inspect_native_tape(GOLDEN_BIN)
        self.assertEqual(envelope["json_sha256"], sha256_file(GOLDEN_JSON))
        self.assertEqual(envelope["lane_count"], 4)
        self.assertEqual(envelope["frame_count"], 3)
        self.assertEqual(envelope["fps_num"], 30)
        self.assertEqual(envelope["fps_den"], 1)
        self.assertEqual(envelope["byte_length"], GOLDEN_BIN.stat().st_size)


if __name__ == "__main__":
    unittest.main()
