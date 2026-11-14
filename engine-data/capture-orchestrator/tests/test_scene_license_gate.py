"""场景登记与上传许可闸（docs/70 R1 / docs/106）。

回归的是 warehouse 三批的实际错法：采集层硬编码许可 ⇒ 换场景不换标记 ⇒
Apache-2.0 的数据被标成 Asset Store EULA，且标记本身「合法」，只有与
scene_asset_id 交叉验证才看得出来。
"""

import argparse
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scenes"))

import registry  # noqa: E402

MODULE_PATH = ROOT / "capture-orchestrator" / "megacity_capture.py"
SPEC = importlib.util.spec_from_file_location("megacity_capture", MODULE_PATH)
MC = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MC)


def entry(name, **over):
    p = {
        "scene_asset_id": f"{name}@x",
        "license_id": "Apache-2.0",
        "license_marker": f"{name}-deliverable",
        "delivery": "deliverable",
        "upload_prefix": "",
        "group_slug": "abc",
        "game_process_name": "Game",
        "attribution": "",
    }
    p.update(over)
    return {"name": name, "tier": "A", "provenance": p}


class RegistryShapeTests(unittest.TestCase):
    def test_ships_manifest_parses_and_markers_are_unique(self):
        index = registry.marker_index()
        self.assertIn("Apache-2.0-deliverable-with-attribution", index)
        self.assertEqual(index["Apache-2.0-deliverable-with-attribution"]["name"],
                         "robotics-warehouse")
        # Megacity 线的既有标记必须仍在白名单里，否则这次改动会挡掉在跑的批
        self.assertEqual(index["UCL-internal-validation-only"]["name"], "megacity-metro")

    def test_warehouse_is_deliverable_and_carries_attribution(self):
        p = registry.resolve("robotics-warehouse")["provenance"]
        self.assertEqual(p["delivery"], "deliverable")
        self.assertEqual(p["upload_prefix"], "")
        self.assertIn("Apache License", p["attribution"])

    def test_internal_scenes_keep_isolation_prefix(self):
        for name in ("megacity-metro", "factory-training"):
            p = registry.resolve(name)["provenance"]
            self.assertEqual(p["delivery"], "internal", name)
            self.assertEqual(p["upload_prefix"], "internal-validation", name)

    def test_resolvable_by_scene_asset_id(self):
        self.assertEqual(registry.resolve("robotics-warehouse@apache-2.0")["name"],
                         "robotics-warehouse")

    def test_unverified_entry_refuses_to_resolve(self):
        # 清单里有 ai2thor，但没做来源核验 ⇒ 不给采集，而不是给个像样的默认值
        with self.assertRaisesRegex(registry.RegistryError, "未核验|provenance 块"):
            registry.resolve("ai2thor")

    def test_unknown_scene_refuses(self):
        with self.assertRaisesRegex(registry.RegistryError, "未登记"):
            registry.resolve("no-such-scene")

    def test_marker_disposition_must_match_delivery(self):
        bad = [entry("x", license_marker="x-internal-validation-only")]
        with self.assertRaisesRegex(registry.RegistryError, "自述处置"):
            registry.marker_index(bad)

    def test_internal_without_prefix_is_rejected(self):
        bad = [entry("x", delivery="internal",
                     license_marker="x-internal-validation-only", upload_prefix="")]
        with self.assertRaisesRegex(registry.RegistryError, "物理隔离"):
            registry.marker_index(bad)

    def test_duplicate_marker_is_rejected(self):
        dup = [entry("a", license_marker="same-deliverable"),
               entry("b", license_marker="same-deliverable")]
        with self.assertRaisesRegex(registry.RegistryError, "同时占用"):
            registry.marker_index(dup)

    def test_bad_group_slug_is_rejected(self):
        with self.assertRaisesRegex(registry.RegistryError, "组名文法"):
            registry.marker_index([entry("x", group_slug="Cang_Ku")])

    def test_capture_env_covers_every_identity_field(self):
        env = registry.capture_env(registry.resolve("robotics-warehouse"))
        self.assertEqual(env["DIOGENES_LICENSE"],
                         "Apache-2.0-deliverable-with-attribution")
        self.assertEqual(env["DIOGENES_SCENE_ASSET_ID"], "robotics-warehouse@apache-2.0")
        self.assertEqual(env["DIOGENES_GAME_NAME"], "RoboticsWarehouse")


class LicenseGateTests(unittest.TestCase):
    def clip(self, root, **prov):
        d = Path(root) / "clip-000001"
        (d / "capture" / "clip").mkdir(parents=True)
        if prov:
            (d / "capture" / "clip" / "provenance.json").write_text(
                json.dumps(prov), encoding="utf-8")
        return d

    def test_warehouse_marker_passes(self):
        with tempfile.TemporaryDirectory() as root:
            d = self.clip(root, license="Apache-2.0-deliverable-with-attribution",
                          scene_asset_id="robotics-warehouse@apache-2.0")
            e, why = MC.license_gate(d)
            self.assertEqual(why, "")
            self.assertEqual(e["name"], "robotics-warehouse")

    def test_ucl_marker_still_passes(self):
        with tempfile.TemporaryDirectory() as root:
            d = self.clip(root, license="UCL-internal-validation-only",
                          scene_asset_id="megacity-metro@6000.1.0f1")
            e, why = MC.license_gate(d)
            self.assertEqual(why, "")
            self.assertEqual(e["name"], "megacity-metro")

    def test_unknown_marker_refused(self):
        with tempfile.TemporaryDirectory() as root:
            d = self.clip(root, license="MIT-whatever", scene_asset_id="x")
            e, why = MC.license_gate(d)
            self.assertIsNone(e)
            self.assertIn("白名单", why)

    def test_missing_marker_refused(self):
        with tempfile.TemporaryDirectory() as root:
            d = self.clip(root, scene_asset_id="robotics-warehouse@apache-2.0")
            self.assertIsNone(MC.license_gate(d)[0])

    def test_missing_provenance_refused(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertIsNone(MC.license_gate(self.clip(root))[0])

    def test_the_actual_warehouse_v3_provenance_is_refused(self):
        """v1/v2/v3 落盘的那种 provenance：标记本身合法，但与场景对不上。"""
        with tempfile.TemporaryDirectory() as root:
            d = self.clip(root, license="AssetStoreEULA-internal-validation-only",
                          scene_asset_id="robotics-warehouse@apache-2.0")
            e, why = MC.license_gate(d)
            self.assertIsNone(e)
            self.assertIn("自相矛盾", why)


class VoidMarkerTests(unittest.TestCase):
    """作废是人工判定，必须**先于**许可闸生效——作废的理由常常正是
    「provenance 自洽但整体是错的」，闸按定义看不出来。"""

    def batch(self, root, marker="Apache-2.0-deliverable-with-attribution",
              scene="robotics-warehouse@apache-2.0"):
        d = Path(root) / "clip-023000"
        (d / "capture" / "clip").mkdir(parents=True)
        (d / "capture" / "clip" / "provenance.json").write_text(
            json.dumps({"license": marker, "scene_asset_id": scene}), encoding="utf-8")
        return d

    def args(self):
        return argparse.Namespace(upload=True, upload_group=None, scene_entry=None)

    def test_void_blocks_an_otherwise_valid_clip(self):
        with tempfile.TemporaryDirectory() as root:
            d = self.batch(root)
            self.assertEqual(MC.license_gate(d)[1], "")          # 闸本身放行
            MC.set_marker(d, MC.VOID_MARKER, "docs/106 §4 裁决")
            self.assertFalse(MC.step_upload(d, 23000, self.args()))

    def test_cmd_void_stamps_every_clip_and_writes_note(self):
        with tempfile.TemporaryDirectory() as root:
            for n in (1, 2, 3):
                (Path(root) / ("clip-%06d" % n)).mkdir()
            rc = MC.cmd_void(argparse.Namespace(runs_root=root, reason="身份指错场景"))
            self.assertEqual(rc, 0)
            for n in (1, 2, 3):
                self.assertTrue(MC.has_marker(Path(root) / ("clip-%06d" % n), "VOID"))
            self.assertIn("不得上传", (Path(root) / "VOID.md").read_text())

    def test_cmd_void_refuses_empty_or_missing_root(self):
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual(
                MC.cmd_void(argparse.Namespace(runs_root=root, reason="x")), 2)
            self.assertEqual(
                MC.cmd_void(argparse.Namespace(runs_root=root + "/nope", reason="x")), 2)

    def test_void_data_is_left_alone(self):
        """作废纪律是「不修数据、留档」——marker 之外一个字节都不该动。"""
        with tempfile.TemporaryDirectory() as root:
            d = self.batch(root)
            prov = d / "capture" / "clip" / "provenance.json"
            before = prov.read_bytes()
            MC.cmd_void(argparse.Namespace(runs_root=root, reason="x"))
            self.assertEqual(prov.read_bytes(), before)


class UploadDestTests(unittest.TestCase):
    def args(self):
        return argparse.Namespace(upload_group=None)

    def test_deliverable_scene_skips_internal_prefix(self):
        dest = MC.upload_dest(Path("/x"), 23000, self.args(),
                              registry.resolve("robotics-warehouse"))
        self.assertNotIn("internal-validation", dest)
        self.assertIn("/2MultiCamera/RoboticsWarehouse/", dest)
        self.assertTrue(dest.endswith("_cangku_00"), dest)

    def test_internal_scene_keeps_isolation_prefix(self):
        dest = MC.upload_dest(Path("/x"), 2000, self.args(),
                              registry.resolve("megacity-metro"))
        self.assertIn("internal-validation/2MultiCamera/MegacityMetro/", dest)

    def test_no_entry_falls_back_to_legacy_layout(self):
        """没给 --scene-id 时必须与改动前逐字一致，否则会打断在跑的 Megacity 批。"""
        with tempfile.TemporaryDirectory() as root:
            d = Path(root)
            (d / "capture" / "clip").mkdir(parents=True)
            (d / "capture" / "clip" / "systeminfo_1.json").write_text(
                json.dumps({"gameProcessName": "MegacityMetro"}), encoding="utf-8")
            dest = MC.upload_dest(d, 2000, self.args())
            self.assertIn("internal-validation/2MultiCamera/MegacityMetro/", dest)
            self.assertTrue(dest.endswith("_megacity_00"), dest)

    def test_group_matches_delivery_grammar(self):
        from importlib import import_module
        sys.path.insert(0, str(ROOT / "delivery"))
        spec = import_module("engine_delivery.spec")
        dest = MC.upload_dest(Path("/x"), 23001, self.args(),
                              registry.resolve("robotics-warehouse"))
        self.assertIsNotNone(spec.MC_GROUP_RE.match(dest.rsplit("/", 1)[1]))


if __name__ == "__main__":
    unittest.main()
