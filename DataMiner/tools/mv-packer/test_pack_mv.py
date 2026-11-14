import hashlib
import json
import pathlib
import runpy
import sys
import tarfile
import tempfile
import types
import unittest
from unittest import mock

from audio_contract import AUDIO_FORMAT


HERE = pathlib.Path(__file__).resolve().parent


class _FakeTable:
    @staticmethod
    def from_pylist(rows, schema):
        return rows, schema


def fake_arrow_modules():
    arrow = types.ModuleType("pyarrow")
    arrow.__path__ = []
    arrow.Table = _FakeTable
    arrow.schema = lambda fields: fields
    arrow.string = lambda: "string"
    arrow.int64 = lambda: "int64"
    arrow.bool_ = lambda: "bool"
    arrow.null = lambda: "null"
    parquet = types.ModuleType("pyarrow.parquet")

    def write_table(_table, path, compression):
        assert compression == "snappy"
        pathlib.Path(path).write_bytes(b"fake parquet for packer integration test")

    parquet.write_table = write_table
    arrow.parquet = parquet
    return {"pyarrow": arrow, "pyarrow.parquet": parquet}


class PackMvIntegrationTests(unittest.TestCase):
    def test_audio_dataset_uses_v2_indexes_and_fourth_tar_member(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            converted = root / "converted"
            output = root / "packed"
            sid = "session-audio"
            session = converted / sid
            inner = session / "fixture" / sid
            chunk = inner / "c0000"
            chunk.mkdir(parents=True)
            (chunk / "p0.mp4").write_bytes(b"video")
            (chunk / "p0.jsonl").write_bytes(b"{}\n")
            flac = b"fLaC integration fixture"
            (chunk / "p0.flac").write_bytes(flac)
            chunk_audio = {
                "state": "ready",
                **AUDIO_FORMAT,
                "present": True,
                "file": "p0.flac",
                "codec": "flac",
                "frames": 192_000,
                "sha256": f"sha256:{hashlib.sha256(flac).hexdigest()}",
                "failure_codes": [],
            }
            (chunk / "meta.json").write_text(
                json.dumps({"audio": chunk_audio}), encoding="utf-8"
            )
            summary = {
                **AUDIO_FORMAT,
                "codec": "flac",
                "ready_chunks": 1,
                "degraded_chunks": 0,
                "absent_chunks": 0,
                "all_publishable": True,
            }
            metadata = {"audio": summary}
            (inner / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
            (inner / "framepairs.jsonl").write_text("", encoding="utf-8")
            (session / "metadata.jsonl").write_text(
                json.dumps(metadata) + "\n", encoding="utf-8"
            )
            (session / "_dataset_stats.json").write_text(
                json.dumps({"total_hours": 0.001}), encoding="utf-8"
            )

            argv = ["pack_mv.py", str(converted), str(output), "v-audio-test"]
            with mock.patch.dict(sys.modules, fake_arrow_modules()), mock.patch.object(
                sys, "argv", argv
            ):
                runpy.run_path(str(HERE / "pack_mv.py"), run_name="__main__")

            version = output / "v-audio-test"
            manifest = json.loads((version / "MANIFEST.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["schema"], "dm-mv-manifest-2")
            self.assertEqual(manifest["audio"]["ready_chunks"], 1)
            self.assertEqual(
                manifest["dataset_config"]["chunk_member_order"][-1], "p0.flac?"
            )
            # docs/148 A2: the DATASHEET ships, is registered, and carries the
            # aggregation semantics section.
            datasheet_path = version / "DATASHEET.md"
            datasheet_text = datasheet_path.read_text(encoding="utf-8")
            self.assertIn("## Action aggregation & downsampling", datasheet_text)
            self.assertIn("held at window end", datasheet_text)
            self.assertIn("dm1-v2", datasheet_text)
            self.assertIn("coverage ratio in [0,1]", datasheet_text)
            self.assertIn("p0.flac", datasheet_text)
            registered = manifest["index_files"]["DATASHEET.md"]
            self.assertEqual(
                registered["sha256"],
                hashlib.sha256(datasheet_path.read_bytes()).hexdigest(),
            )
            self.assertEqual(registered["bytes"], len(datasheet_path.read_bytes()))
            session_index = json.loads(
                (version / "index" / "sessions" / sid / "session-index.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(session_index["schema"], "dm-mv-session-index-2")
            sample = json.loads(
                (version / "index" / "sessions" / sid / "samples.jsonl")
                .read_text(encoding="utf-8")
                .strip()
            )
            self.assertTrue(sample["audio_present"])
            shard = version / "test" / sid / "shard-000.tar"
            with tarfile.open(shard) as archive:
                self.assertEqual(
                    archive.getnames(),
                    [
                        f"{sid}_c0000.p0.mp4",
                        f"{sid}_c0000.p0.jsonl",
                        f"{sid}_c0000.meta.json",
                        f"{sid}_c0000.p0.flac",
                    ],
                )


if __name__ == "__main__":
    unittest.main()
