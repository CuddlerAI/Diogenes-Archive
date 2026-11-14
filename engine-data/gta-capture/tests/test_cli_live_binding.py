import unittest

from gta_capture.cli import _require_live_record_binding
from gta_capture.errors import GtaCaptureError


class LiveRecordBindingTests(unittest.TestCase):
    def test_accepts_same_generation_bound_replay(self):
        digest = "a" * 64
        _require_live_record_binding({
            "controller_generation": 2,
            "record_binding_ready": True,
            "master_replay_sha256": digest,
        }, digest)

    def test_rejects_binding_lost_after_editor_relaunch(self):
        with self.assertRaisesRegex(GtaCaptureError, "lost the master binding"):
            _require_live_record_binding({
                "controller_generation": 2,
                "record_binding_ready": False,
                "master_replay_sha256": None,
            }, "a" * 64)

    def test_rejects_a_different_master(self):
        with self.assertRaisesRegex(GtaCaptureError, "differs"):
            _require_live_record_binding({
                "controller_generation": 2,
                "record_binding_ready": True,
                "master_replay_sha256": "b" * 64,
            }, "a" * 64)

    def test_rejects_missing_generation(self):
        with self.assertRaisesRegex(GtaCaptureError, "controller generation"):
            _require_live_record_binding({
                "record_binding_ready": True,
                "master_replay_sha256": "a" * 64,
            }, "a" * 64)


if __name__ == "__main__":
    unittest.main()
