#!/usr/bin/env python3
"""ingest_remap.py 单元测试（不联网）。

两组夹具：
1. 真实 Halo attested API 布局样本（`INGEST_REMAP_SAMPLE` 环境变量指向 dump 目录；
   缺省用预取到 scratchpad 的路径，不存在则跳过该组）+ 仓库真实 catalog 注册表
   （含 git 历史回捞：样本声明的 device/game/catalog 版本已被 HEAD bump 掉）。
2. 纯合成的最小 v2（无音频）session，覆盖 game 闸、断裂检测、sha 对账、红线等
   错误路径——不依赖任何外部数据。

跑法：`python3 -m pytest tools/mv-pipeline/test_ingest_remap.py`
  或  `python3 -m unittest tools/mv-pipeline/test_ingest_remap.py`（在仓库根）。
"""

from __future__ import annotations

import copy
import hashlib
import json
import os
import shutil
import sys
import tempfile
import unittest
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import ingest_remap as ir

REPO_ROOT = Path(__file__).resolve().parents[2]
CATALOGS_DIR = REPO_ROOT / "DataMiner" / "config" / "catalogs"
DEFAULT_SAMPLE = (
    "/private/tmp/claude-501/-Users-oratis-Projects-Claude-Diogenes--claude-worktrees-"
    "distracted-mcnulty-774a6c/8c57ccb2-8c67-4ffd-b671-c2b0ae5a262d/scratchpad/"
    "halo_session_sample"
)
SAMPLE_DIR = Path(os.environ.get("INGEST_REMAP_SAMPLE", DEFAULT_SAMPLE))

HALO_GAME = "halo-infinite-steam-release-test"


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ---------------------------------------------------------------------------
# 真实 Halo 样本
# ---------------------------------------------------------------------------


@unittest.skipUnless(SAMPLE_DIR.is_dir(), f"Halo 样本 dump 不存在: {SAMPLE_DIR}")
@unittest.skipUnless(CATALOGS_DIR.is_dir(), f"仓库 catalogs 目录不存在: {CATALOGS_DIR}")
class RealHaloSampleTest(unittest.TestCase):
    plan: dict = None

    @classmethod
    def setUpClass(cls):
        cls.listing = json.loads((SAMPLE_DIR / "_listing.json").read_text())
        cls.dumps = ir.load_dump(SAMPLE_DIR)
        cls.plan = ir.build_plan(cls.dumps, CATALOGS_DIR, {HALO_GAME})

    def _session(self):
        self.assertEqual(len(self.plan["sessions"]), 1)
        return self.plan["sessions"][0]

    def _synth(self, session, rel):
        matches = [i for i in session["synthesized"] if i["rel"] == rel]
        self.assertEqual(len(matches), 1, f"合成件 {rel} 应恰有一份")
        return matches[0]

    # -- 任务点 1：映射条目数 == 清单媒体对象数（manifest 被 sha256.json 取代，不下载）
    def test_mapping_count_equals_media_object_count(self):
        media = [
            o for o in self.listing
            if not Path(o["key"]).name.startswith("manifest-")
        ]
        s = self._session()
        self.assertEqual(len(s["mappings"]), len(media))
        # 每个媒体对象的 key 都被映射且不重复
        mapped_keys = {m["key"] for m in s["mappings"]}
        self.assertEqual(mapped_keys, {o["key"] for o in media})
        # 本样本无 runs/ 层 sidecar
        self.assertEqual(s["sidecars"], [])

    # -- 任务点 2：segment 连续性 + run 枚举
    def test_run_and_segment_continuity(self):
        s = self._session()
        run_ids = [r["run_id"] for r in s["runs"]]
        self.assertEqual(run_ids, ["r0000", "r0001", "r0002", "r0003"])
        expected_start = 0
        for r in s["runs"]:
            self.assertEqual(r["session_frame_start"], expected_start)
            self.assertEqual(
                r["session_frame_end"], r["session_frame_start"] + r["frame_count"]
            )
            expected_start = r["session_frame_end"]
        # rel 路径按 5 位零填充段号，且每段五件套 + 合成 sha256.json
        rels = {m["rel"] for m in s["mappings"]}
        self.assertIn("runs/r0000/segments/00000/video.mp4", rels)
        self.assertIn("runs/r0003/segments/00065/meta.json", rels)
        self.assertIn(ir.AUDIO_ARTIFACT_NAME, {Path(r).name for r in rels})

    # -- 任务点 3：sha256.json 内容与 manifest 登记一致
    def test_sha256_json_matches_manifest_registrations(self):
        s = self._session()
        for (api_run, clip_id), clip in self.dumps[s["api_session"]].clips.items():
            seg = f"runs/{clip.meta['run_id']}/segments/{clip.meta['segment_index']:05d}"
            item = self._synth(s, f"{seg}/sha256.json")
            self.assertEqual(json.loads(item["content"]), clip.manifest)

    # -- 任务点 4：session.json 通过 lib.rs 提炼的字段检查表
    def test_session_json_passes_validator_field_checklist(self):
        s = self._session()
        session = json.loads(self._synth(s, "session.json")["content"])
        meta0 = next(iter(self.dumps[s["api_session"]].clips.values())).meta
        recording = json.loads(self._synth(s, "recording-profile.json")["content"])
        mapping = json.loads(self._synth(s, "mapping.json")["content"])

        # validate_session_header L2069/L2121：音频段 ⇒ dm-local-dev-session-3
        self.assertEqual(session["schema_version"], "dm-local-dev-session-3")
        # 治理位（L2082-L2101 + 红线）
        self.assertIs(session["non_trainable"], True)
        self.assertIs(session["production_certified"], False)
        self.assertNotIn("consent_id", session)
        # 安全路径组件（L2098）
        self.assertRegex(session["session_id"], r"^[A-Za-z0-9_-]+$")
        self.assertEqual(session["session_id"], meta0["session_id"])
        # contract dm-1（ensure_contract L3899）
        self.assertEqual(
            session["contract"],
            {"version": "dm-1", "pointer_clamp": 200, "pointer_mu": 8,
             "pointer_ref": 200, "pointer_max_bin": 10},
        )
        # catalog 块必须与 segment meta 完全相等（L2262）
        self.assertEqual(session["catalog"], meta0["catalog"])
        # game/mapping 交叉（L2140-L2148）
        self.assertEqual(session["game"]["id"], mapping["game"]["id"])
        self.assertEqual(session["mapping"]["id"], mapping["mapping_id"])
        self.assertEqual(session["mapping"]["version"], mapping["version"])
        self.assertEqual(
            session["mapping"]["recording_profile_id"], mapping["recording_profile_id"]
        )
        self.assertEqual(
            session["mapping"]["content_hash"], meta0["catalog"]["mapping"]["content_hash"]
        )
        # capture_policy ↔ recording profile（validate_capture_policy L3951）
        cp = session["capture_policy"]
        self.assertEqual(cp["source"], "game_window_only")
        self.assertEqual(cp["width"], recording["width"])
        self.assertEqual(cp["height"], recording["height"])
        self.assertEqual(cp["fps_num"], recording["fps_num"])
        self.assertEqual(cp["fps_den"], recording["fps_den"])
        self.assertEqual(cp["gop_frames"], recording["gop_frames"])
        # non_trainable 几何：AtLeast(960,540) + scale=true（profile.rs L252/L271）
        self.assertIs(cp["scale"], True)
        self.assertEqual(cp["minimum_source_width"], 960)
        self.assertEqual(cp["minimum_source_height"], 540)
        # v3 会话 audio 策略 == recording profile 的 audio 块（L3971-L3978 + L2121）
        self.assertEqual(cp["audio"], recording["audio"])

    def test_run_json_aggregation_equations(self):
        s = self._session()
        clips = self.dumps[s["api_session"]].clips
        by_run = {}
        for clip in clips.values():
            by_run.setdefault(clip.meta["run_id"], []).append(clip.meta)
        for r in s["runs"]:
            run = json.loads(self._synth(s, f"runs/{r['run_id']}/run.json")["content"])
            segs = sorted(by_run[r["run_id"]], key=lambda m: m["segment_index"])
            # validate_run L3641 + validate_ready_session 的账目（L1948-L2026）
            self.assertEqual(run["schemaVersion"], "dm-local-dev-recording-2")
            self.assertEqual(run["state"], "non_trainable_complete")
            self.assertIs(run["nonTrainable"], True)
            self.assertIs(run["productionCertified"], False)
            self.assertEqual(run["sessionId"], segs[0]["session_id"])
            self.assertEqual(run["runId"], r["run_id"])
            self.assertEqual(run["mappingId"], segs[0]["mapping"]["id"])
            self.assertEqual(run["mappingContentHash"], segs[0]["mapping"]["content_hash"])
            self.assertEqual(run["segmentCount"], len(segs))
            self.assertEqual(run["frameCount"], sum(m["n_frames"] for m in segs))
            self.assertEqual(run["sessionFrameStart"], segs[0]["session_frame_start"])
            self.assertEqual(
                run["sessionFrameEnd"], run["sessionFrameStart"] + run["frameCount"]
            )
            self.assertEqual(
                run["inputEventCount"], sum(m["events"]["lines"] for m in segs)
            )
            self.assertEqual(run["inputQueueOverflows"], 0)
            # 音频段在场 ⇒ qpcFrequency 必须与段音频一致（L2297）
            self.assertEqual(run["qpcFrequency"], segs[0]["audio"]["qpc_frequency"])
            # 不可知字段必须省略而非臆造
            self.assertNotIn("anchorQpc", run)
            self.assertNotIn("anchorAtUtc", run)
            self.assertNotIn("stopReason", run)

    def test_catalog_snapshots_hash_reconciled_and_git_recovered(self):
        s = self._session()
        meta0 = next(iter(self.dumps[s["api_session"]].clips.values())).meta
        cat = meta0["catalog"]
        declared = {
            "catalog.json": cat["catalog_content_hash"],
            "device-profile.json": cat["device"]["content_hash"],
            "recording-profile.json": cat["recording"]["content_hash"],
            "game-profile.json": cat["game"]["content_hash"],
            "mapping.json": cat["mapping"]["content_hash"],
        }
        prov = self.plan["provenance_sessions"][0]
        by_name = {c["name"]: c for c in prov["catalog_snapshots"]}
        self.assertEqual(set(by_name), set(declared))
        for name, want in declared.items():
            item = self._synth(s, name)
            self.assertEqual("sha256:" + _sha(item["content"]), want)
            self.assertEqual(by_name[name]["sha256"], want)
        # HEAD 已 bump 到 2026-08-04.1/v3：这三份必须是 git 历史回捞的
        for name in ("catalog.json", "device-profile.json", "game-profile.json"):
            self.assertTrue(
                by_name[name]["resolved_from"].startswith("git:"),
                f"{name} 应从 git 历史回捞，实得 {by_name[name]['resolved_from']}",
            )
        # 这两份 HEAD 未动，应直接来自工作区
        for name in ("mapping.json", "recording-profile.json"):
            self.assertFalse(by_name[name]["resolved_from"].startswith("git:"))

    def test_red_lines_absent_from_all_synthesized_output(self):
        s = self._session()
        for item in s["synthesized"]:
            text = item["content"].decode("utf-8")
            self.assertNotIn("consented", text, item["rel"])
            self.assertNotIn('"non_trainable": false', text, item["rel"])
            self.assertNotIn('"nonTrainable": false', text, item["rel"])
            self.assertNotIn('"consent_id"', text, item["rel"])

    def test_provenance_records_unknowns_and_writer_constants(self):
        prov = self.plan["provenance_sessions"][0]
        fields = {u["field"] for u in prov["synthesized_unknown"]}
        self.assertIn("anchorQpc/anchorAtUtc", fields)
        self.assertIn("stopReason", fields)
        self.assertIn("inputQueueOverflows", fields)
        self.assertEqual(prov["writer_constants"][0]["field"], "todo")

    def test_write_plan_materializes_raw_tree(self):
        with tempfile.TemporaryDirectory() as td:
            out = Path(td) / "plan"
            ir.write_plan(self.plan, out, {HALO_GAME})
            doc = json.loads((out / "remap-plan.json").read_text())
            self.assertEqual(doc["schema"], "ingest-remap-plan-1")
            sid = doc["sessions"][0]["session_id"]
            base = out / "synthesized" / sid
            for rel in (
                "session.json", "catalog.json", "device-profile.json",
                "game-profile.json", "mapping.json", "recording-profile.json",
                "runs/r0000/run.json", "runs/r0000/segments/00000/sha256.json",
                "runs/r0003/segments/00065/sha256.json",
            ):
                self.assertTrue((base / rel).is_file(), rel)
            self.assertTrue((out / "PROVENANCE.json").is_file())
            # emit-manifest --dry-run 全链路（不联网）
            manifest_path = Path(td) / "mv-manifest.json"
            rc = ir.main([
                "emit-manifest", "--plan", str(out), "--out", str(manifest_path),
                "--split", "train", "--source-bucket", "src", "--staging-bucket", "stage",
                "--staging-prefix", "mv-staging/test", "--dry-run",
            ])
            self.assertEqual(rc, 0)
            manifest = json.loads(manifest_path.read_text())
            session = manifest["sessions"][0]
            self.assertEqual(session["session_id"], sid)
            self.assertIs(session["expect_fail"], False)
            n_map = len(self.plan["sessions"][0]["mappings"])
            n_syn = len(self.plan["sessions"][0]["synthesized"])
            self.assertEqual(len(session["gets"]), n_map + n_syn)
            rels = [g["rel"] for g in session["gets"]]
            self.assertEqual(len(rels), len(set(rels)), "gets[].rel 必须互异")
            staged = [g for g in session["gets"] if g["rel"] == "session.json"]
            self.assertIn("mv-staging/test", staged[0]["url"])

    # -- 任务点 5（真样本侧）：allowlist 不含 halo 时整个 session 被拒
    def test_game_filter_rejects_real_sample_when_not_allowlisted(self):
        plan = ir.build_plan(self.dumps, CATALOGS_DIR, {"call-of-duty-steam-release-test"})
        self.assertEqual(plan["sessions"], [])
        self.assertEqual(len(plan["rejected"]["sessions"]), 1)
        rej = plan["rejected"]["sessions"][0]
        self.assertEqual(rej["reason"], "game_allowlist")
        self.assertIn(HALO_GAME, rej["games_rejected"])


# ---------------------------------------------------------------------------
# 合成夹具（v2 无音频）：错误路径全覆盖，不依赖外部数据
# ---------------------------------------------------------------------------


class SyntheticFixture:
    """构造一个自洽的最小 API dump + 与之 sha 对账的 catalog 注册表。"""

    GAME = "test-game"

    def __init__(self, root: Path, n_segments: int = 3, game_id: str | None = None):
        self.root = root
        self.game_id = game_id or self.GAME
        self.dump_dir = root / "dump"
        self.catalogs_dir = root / "catalogs"
        (self.catalogs_dir / "dev" / "testgame").mkdir(parents=True)
        self.dump_dir.mkdir(parents=True)
        self._build_catalog()
        self._build_dump(n_segments)

    def _write_bundle(self, name: str, obj: dict) -> str:
        data = (json.dumps(obj, indent=2) + "\n").encode()
        (self.catalogs_dir / "dev" / "testgame" / name).write_bytes(data)
        return "sha256:" + _sha(data)

    def _build_catalog(self):
        g = self.game_id
        device = {
            "schema_version": "dm-device-profile-2",
            "profile_id": f"{g}-device-v1", "version": "1", "non_trainable": True,
        }
        recording = {
            "schema_version": "dm-recording-profile-1",
            "profile_id": f"{g}-recording-v1", "version": "1", "game_id": g,
            "non_trainable": True,
            "capture_backend": "native_wgc_window_v1",
            "encoder_backend": "mf_nvidia_hardware_v1",
            "width": 1920, "height": 1080, "fps_num": 60, "fps_den": 1,
            "bitrate_bps": 16000000, "max_bitrate_bps": 24000000,
            "gop_frames": 240, "segment_seconds": 10, "test_segments": 30,
        }
        mapping = {
            "schema_version": "dm-mapping-1", "mapping_id": f"game.{g}.pc",
            "version": "2026-01-01.0", "game": {"id": g},
            "recording_profile_id": f"{g}-recording-v1", "bindings": [],
        }
        game = {
            "schema_version": "dm-game-profile-2", "game_id": g, "version": "1",
            "device_profile_id": f"{g}-device-v1",
            "recording_profile_id": f"{g}-recording-v1",
            "mapping_id": f"game.{g}.pc", "non_trainable": True,
        }
        h_device = self._write_bundle("device-profile.json", device)
        h_recording = self._write_bundle("recording-profile.json", recording)
        h_mapping = self._write_bundle("mapping.json", mapping)
        h_game = self._write_bundle("game-profile.json", game)
        catalog = {
            "schema_version": "dm-catalog-bundle-2",
            "catalog_id": f"dataminer-{g}", "catalog_version": "2026-01-01.0",
            "non_trainable": True,
            "device_profiles": [{"id": f"{g}-device-v1", "version": "1", "content_hash": h_device}],
            "recording_profiles": [{"id": f"{g}-recording-v1", "version": "1", "content_hash": h_recording}],
            "game_profiles": [{"id": g, "version": "1", "content_hash": h_game}],
            "mappings": [{"id": f"game.{g}.pc", "version": "2026-01-01.0", "content_hash": h_mapping}],
        }
        h_catalog = self._write_bundle("catalog.json", catalog)
        self.catalog_block = {
            "catalog_content_hash": h_catalog,
            "catalog_id": f"dataminer-{g}", "catalog_version": "2026-01-01.0",
            "device": {"content_hash": h_device, "id": f"{g}-device-v1", "version": "1"},
            "game": {"content_hash": h_game, "id": g, "version": "1"},
            "mapping": {"content_hash": h_mapping, "id": f"game.{g}.pc", "version": "2026-01-01.0"},
            "mode": "dev_non_trainable", "non_trainable": True, "provenance": "compiled",
            "recording": {"content_hash": h_recording, "id": f"{g}-recording-v1", "version": "1"},
        }

    def _meta(self, seg: int) -> dict:
        g = self.game_id
        return {
            "schema_version": "dm-local-dev-recording-1",
            "session_id": "dev-1785830000000-0001",
            "run_id": "r0000",
            "segment_index": seg,
            "session_frame_start": seg * 600,
            "n_frames": 600,
            "state": "non_trainable_ready",
            "non_trainable": True,
            "production_certified": False,
            "input_status": "raw_input_v1",
            "qpc_start": 1_000_000_000 + seg * 100_000_000,
            "qpc_end": 1_000_000_000 + (seg + 1) * 100_000_000,
            "fps_num": 60, "fps_den": 1, "width": 1920, "height": 1080,
            "mapping": {
                "content_hash": self.catalog_block["mapping"]["content_hash"],
                "id": f"game.{g}.pc", "version": "2026-01-01.0",
            },
            "catalog": self.catalog_block,
            "contract": {"version": "dm-1", "pointer_clamp": 200, "pointer_mu": 8,
                         "pointer_ref": 200, "pointer_max_bin": 10},
            "video": {"bytes": 1000 + seg, "path": "video.mp4",
                      "sha256": "sha256:" + ("a%063d" % seg)},
            "frames": {"lines": 600, "path": "frames.jsonl.zst",
                       "sha256": "sha256:" + ("b%063d" % seg)},
            "events": {"lines": 42, "path": "events.jsonl.zst",
                       "sha256": "sha256:" + ("c%063d" % seg)},
            "effective_encoder": {"transform_clsid": None,
                                  "friendly_name": "NVIDIA H.264 Encoder MFT",
                                  "hardware_url": "NVIDIA H.264 Encoder MFT"},
            "probe": {"codec": "H.264", "h264_profile": 100, "width": 1920,
                      "height": 1080, "fps_num": 60, "fps_den": 1,
                      "sample_count": 600, "first_sample_clean_point": True,
                      "pts_monotonic": True},
            "health": {"duplicate_frames": 0, "input_boundary_reorder_drops": 0,
                       "input_overflow": 0, "queue_drops": 0, "source_superseded": 0},
            "target": {"executable_sha256": "sha256:" + "d" * 64,
                       "game_id": g, "target_id": "target-" + "e" * 64},
        }

    def _build_dump(self, n_segments: int, mutate_meta=None, mutate_manifest=None):
        api_session = str(uuid.uuid4())
        api_run = str(uuid.uuid4())
        prefix = f"raw/v1/attested-trainable/test/sessions/{api_session}/"
        listing = []
        for seg in range(n_segments):
            clip = str(uuid.uuid4())
            meta = self._meta(seg)
            if mutate_meta:
                mutate_meta(seg, meta)
            meta_bytes = (json.dumps(meta, indent=2) + "\n").encode()
            meta_sha = _sha(meta_bytes)
            manifest = {
                "events.jsonl.zst": meta["events"]["sha256"],
                "frames.jsonl.zst": meta["frames"]["sha256"],
                "meta.json": "sha256:" + meta_sha,
                "video.mp4": meta["video"]["sha256"],
            }
            if mutate_manifest:
                mutate_manifest(seg, manifest)
            manifest_bytes = (json.dumps(manifest, indent=2) + "\n").encode()
            manifest_sha = _sha(manifest_bytes)
            clip_prefix = f"{prefix}runs/{api_run}/clips/{clip}/"
            objs = [
                (f"video-{meta['video']['sha256'][7:]}.mp4", meta["video"]["bytes"]),
                (f"frames-{meta['frames']['sha256'][7:]}.jsonl.zst", 500),
                (f"events-{meta['events']['sha256'][7:]}.jsonl.zst", 300),
                (f"meta-{meta_sha}.json", len(meta_bytes)),
                (f"manifest-{manifest_sha}.json", len(manifest_bytes)),
            ]
            for name, size in objs:
                listing.append({"key": clip_prefix + name, "size": size})
            local_base = f"runs__{api_run}__clips__{clip}__"
            (self.dump_dir / f"{local_base}meta-{meta_sha}.json").write_bytes(meta_bytes)
            (self.dump_dir / f"{local_base}manifest-{manifest_sha}.json").write_bytes(
                manifest_bytes
            )
        # 一个 runs/ 层 sidecar（docs/133 的 camera-pose-index 场景）
        listing.append(
            {"key": f"{prefix}runs/{api_run}/camera-pose-index.jsonl", "size": 77}
        )
        (self.dump_dir / "_listing.json").write_text(json.dumps(listing))
        self.api_session = api_session

    def rebuild_dump(self, n_segments: int, **kwargs):
        shutil.rmtree(self.dump_dir)
        self.dump_dir.mkdir()
        self._build_dump(n_segments, **kwargs)


class SyntheticTest(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)

    def tearDown(self):
        self._td.cleanup()

    def _plan(self, fx: SyntheticFixture, allowlist=None):
        dumps = ir.load_dump(fx.dump_dir)
        return ir.build_plan(
            dumps, fx.catalogs_dir, allowlist or {fx.game_id}, git_fallback=False
        )

    def test_happy_path_v2_without_audio(self):
        fx = SyntheticFixture(self.root)
        plan = self._plan(fx)
        s = plan["sessions"][0]
        self.assertEqual(s["schema_generation"], "v2")
        session = json.loads(
            [i for i in s["synthesized"] if i["rel"] == "session.json"][0]["content"]
        )
        self.assertEqual(session["schema_version"], "dm-local-dev-session-2")
        self.assertIs(session["capture_policy"]["audio"], False)  # 无音频代（L3973）
        run = json.loads(
            [i for i in s["synthesized"] if i["rel"] == "runs/r0000/run.json"][0]["content"]
        )
        self.assertEqual(run["schemaVersion"], "dm-local-dev-recording-1")
        self.assertNotIn("qpcFrequency", run)  # 无音频 ⇒ 无本地证据，省略
        # sidecar 归类：不进 RAW 段，落 _sidecar/ 前缀
        self.assertEqual(len(s["sidecars"]), 1)
        self.assertTrue(s["sidecars"][0]["rel"].startswith("_sidecar/runs/"))
        self.assertTrue(s["sidecars"][0]["rel"].endswith("camera-pose-index.jsonl"))
        # 3 段 × 4 对象（无 audio）
        self.assertEqual(len(s["mappings"]), 12)

    # -- 任务点 5：game 闸拒非白名单 session
    def test_game_filter_rejects_foreign_game(self):
        fx = SyntheticFixture(self.root, game_id="fallout4-steam-release-test")
        plan = self._plan(fx, allowlist={HALO_GAME})
        self.assertEqual(plan["sessions"], [])
        rej = plan["rejected"]["sessions"]
        self.assertEqual(len(rej), 1)
        self.assertEqual(rej[0]["reason"], "game_allowlist")
        self.assertIn("fallout4-steam-release-test", rej[0]["games_rejected"])

    def test_segment_gap_is_a_hard_error_listing_the_hole(self):
        fx = SyntheticFixture(self.root, n_segments=3)
        # 删掉 segment 1 的 meta/manifest 与 listing 条目 → 断裂
        listing = json.loads((fx.dump_dir / "_listing.json").read_text())
        drop_keys = set()
        for f in fx.dump_dir.iterdir():
            if f.name == "_listing.json":
                continue
            if f.name.startswith("runs__") and "meta-" in f.name:
                if json.loads(f.read_bytes())["segment_index"] == 1:
                    clip = f.name.split("__")[3]
                    drop_keys.add(clip)
        listing = [o for o in listing if not any(c in o["key"] for c in drop_keys)]
        for f in list(fx.dump_dir.iterdir()):
            if any(c in f.name for c in drop_keys):
                f.unlink()
        (fx.dump_dir / "_listing.json").write_text(json.dumps(listing))
        with self.assertRaises(ir.PlanError) as ctx:
            self._plan(fx)
        msg = str(ctx.exception)
        self.assertIn("segment_index 断裂", msg)
        self.assertIn("[1]", msg)  # 缺口点名

    def test_manifest_vs_filename_sha_mismatch_is_fatal(self):
        fx = SyntheticFixture(self.root)

        def tamper(seg, manifest):
            if seg == 0:
                manifest["video.mp4"] = "sha256:" + "f" * 64

        fx.rebuild_dump(3, mutate_manifest=tamper)
        with self.assertRaises(ir.PlanError) as ctx:
            self._plan(fx)
        self.assertIn("manifest 登记", str(ctx.exception))

    def test_lane_contamination_non_trainable_false_is_fatal(self):
        fx = SyntheticFixture(self.root)

        def taint(seg, meta):
            if seg == 2:
                meta["non_trainable"] = False

        fx.rebuild_dump(3, mutate_meta=taint)
        with self.assertRaises(ir.PlanError) as ctx:
            self._plan(fx)
        self.assertIn("治理位越界", str(ctx.exception))

    def test_consented_state_is_fatal(self):
        fx = SyntheticFixture(self.root)

        def taint(seg, meta):
            meta["state"] = "consented_ready"

        fx.rebuild_dump(1, mutate_meta=taint)
        with self.assertRaises(ir.PlanError):
            self._plan(fx)

    def test_non_enumerable_run_ids_are_fatal(self):
        fx = SyntheticFixture(self.root)

        def shift(seg, meta):
            meta["run_id"] = "r0001"  # 没有 r0000 → converter 枚举必拒

        fx.rebuild_dump(2, mutate_meta=shift)
        with self.assertRaises(ir.PlanError) as ctx:
            self._plan(fx)
        self.assertIn("枚举", str(ctx.exception))

    def test_frame_discontinuity_across_segments_is_fatal(self):
        fx = SyntheticFixture(self.root)

        def hole(seg, meta):
            if seg == 2:
                meta["session_frame_start"] += 7

        fx.rebuild_dump(3, mutate_meta=hole)
        with self.assertRaises(ir.PlanError) as ctx:
            self._plan(fx)
        self.assertIn("帧区间断裂", str(ctx.exception))

    def test_catalog_hash_mismatch_refuses_to_synthesize(self):
        fx = SyntheticFixture(self.root)
        # 篡改注册表里的 device profile → 与 meta 声明 sha 对不上，且禁 git 回捞
        p = fx.catalogs_dir / "dev" / "testgame" / "device-profile.json"
        obj = json.loads(p.read_text())
        obj["version"] = "999"
        p.write_text(json.dumps(obj, indent=2) + "\n")
        with self.assertRaises(ir.PlanError) as ctx:
            self._plan(fx)
        self.assertIn("绝不合成", str(ctx.exception))

    def test_build_mv_manifest_shapes_gets_for_download_as_rename(self):
        fx = SyntheticFixture(self.root)
        plan = self._plan(fx)
        out = self.root / "plan_out"
        ir.write_plan(plan, out, {fx.game_id})
        plan_doc = json.loads((out / "remap-plan.json").read_text())
        manifest = ir.build_mv_manifest(
            plan_doc,
            split="val",
            sign_source_get=lambda key: f"https://signed/{key}",
            sign_staged_get=lambda sid, rel: f"https://staged/{sid}/{rel}",
            puts_by_session={"dev-1785830000000-0001": {"logs/x.log": "https://put"}},
            global_puts={"logs/final-summary.json": "https://put-final"},
        )
        s = manifest["sessions"][0]
        self.assertEqual(s["split"], "val")
        self.assertEqual(s["puts"], {"logs/x.log": "https://put"})
        self.assertEqual(manifest["global_puts"], {"logs/final-summary.json": "https://put-final"})
        by_rel = {g["rel"]: g for g in s["gets"]}
        # 源对象：URL 指向 API key，rel 是 RAW 相对路径 → 下载即重命名
        video = by_rel["runs/r0000/segments/00000/video.mp4"]
        self.assertIn("/clips/", video["url"])
        self.assertIn("video-", video["url"])
        # 合成件：URL 指向 staging
        self.assertTrue(by_rel["session.json"]["url"].startswith("https://staged/"))
        self.assertTrue(by_rel["runs/r0000/run.json"]["url"].startswith("https://staged/"))
        # sidecar 不落段目录
        sidecar_rels = [r for r in by_rel if r.startswith("_sidecar/")]
        self.assertEqual(len(sidecar_rels), 1)
        # size 逐条与 plan 一致（driver 靠它做完整性校验）
        for m in plan_doc["sessions"][0]["mappings"]:
            self.assertEqual(by_rel[m["rel"]]["size"], m["size"])


if __name__ == "__main__":
    unittest.main()
