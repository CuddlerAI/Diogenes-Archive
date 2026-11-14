import hashlib
import json
import pathlib
import tempfile
import unittest

from audio_contract import AUDIO_FORMAT, chunk_members, validate_session_audio


def digest(raw: bytes) -> str:
    return f"sha256:{hashlib.sha256(raw).hexdigest()}"


class AudioContractTests(unittest.TestCase):
    def chunk(self, state: str = "ready") -> tuple[tempfile.TemporaryDirectory, pathlib.Path, dict]:
        temp = tempfile.TemporaryDirectory()
        path = pathlib.Path(temp.name)
        for name, raw in (("p0.mp4", b"video"), ("p0.jsonl", b"{}\n")):
            (path / name).write_bytes(raw)
        audio = {
            "state": state,
            **AUDIO_FORMAT,
            "present": state == "ready",
            "failure_codes": [] if state == "ready" else ["AUDIO_UNAVAILABLE"],
        }
        if state == "ready":
            flac = b"fLaC fixture"
            (path / "p0.flac").write_bytes(flac)
            audio.update({
                "file": "p0.flac",
                "codec": "flac",
                "frames": 192_000,
                "sha256": digest(flac),
            })
        meta = {"audio": audio}
        (path / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
        return temp, path, audio

    def test_ready_flac_is_the_optional_fourth_member(self):
        temp, path, audio = self.chunk()
        self.addCleanup(temp.cleanup)
        members, _, parsed = chunk_members(path, "sample_c0000", True)
        self.assertEqual(
            [name for name, _ in members],
            [
                "sample_c0000.p0.mp4",
                "sample_c0000.p0.jsonl",
                "sample_c0000.meta.json",
                "sample_c0000.p0.flac",
            ],
        )
        self.assertEqual(parsed, audio)

    def test_degraded_audio_keeps_three_members_and_failure_state(self):
        temp, path, audio = self.chunk("degraded")
        self.addCleanup(temp.cleanup)
        members, _, parsed = chunk_members(path, "sample_c0000", True)
        self.assertEqual(len(members), 3)
        self.assertEqual(parsed, audio)

    def test_stray_flac_is_rejected_for_legacy_chunk(self):
        temp, path, _ = self.chunk()
        self.addCleanup(temp.cleanup)
        meta = {}
        (path / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "members"):
            chunk_members(path, "sample_c0000", False)

    def test_session_summary_must_close_over_chunk_states(self):
        temp, _, ready = self.chunk()
        self.addCleanup(temp.cleanup)
        degraded_temp, _, degraded = self.chunk("degraded")
        self.addCleanup(degraded_temp.cleanup)
        summary = {
            **AUDIO_FORMAT,
            "codec": "flac",
            "ready_chunks": 1,
            "degraded_chunks": 1,
            "absent_chunks": 0,
            "all_publishable": False,
        }
        validate_session_audio(summary, [ready, degraded])
        summary["all_publishable"] = True
        with self.assertRaisesRegex(ValueError, "does not close"):
            validate_session_audio(summary, [ready, degraded])


if __name__ == "__main__":
    unittest.main()
