#!/usr/bin/env python3
"""reclass.py game_filter 逻辑的离线单测——不碰网络、不需要 rclone。

覆盖：lsf 输出解析→session 分组、meta 选取与 catalog.game.id 提取、白名单生成、
对账数字计算（含越界对象）、WAIVER json 生成（game_filter/refreshed_at 字段）、
以及 mock 掉 rclone 后的 execute 全链路（WAIVER 落盘内容）。

直接跑：python3 tools/r2-attested-reclass/tests/test_reclass.py
"""

import contextlib
import io
import importlib.util
import json
import os
import sys
import tempfile
import types
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_SPEC = importlib.util.spec_from_file_location(
    "reclass", os.path.join(_HERE, "..", "reclass.py"))
reclass = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(reclass)

HALO = "halo-infinite-steam-release-test"
FO4 = "fallout4-steam-release-test"

S_HALO1 = "11111111-aaaa-4aaa-8aaa-000000000001"
S_HALO2 = "11111111-aaaa-4aaa-8aaa-000000000002"
S_FO4 = "22222222-bbbb-4bbb-8bbb-000000000001"
S_NOCAT = "33333333-cccc-4ccc-8ccc-000000000001"  # meta 里没有 catalog 块
S_NOMETA = "44444444-dddd-4ddd-8ddd-000000000001"  # 整个 session 没有 meta 文件
S_OOS = "99999999-eeee-4eee-8eee-000000000001"  # 只存在于 dst 的历史越界 session


def clip(sid, n, files):
    """生成一个 clip 目录下的 (相对路径, size) 列表。"""
    base = f"{sid}/runs/r0001/clips/c{n:04d}"
    return [(f"{base}/{name}", size) for name, size in files]


def six(sid, n, sha):
    return clip(sid, n, [
        (f"video-{sha}.mp4", 1000 + n),
        (f"audio-{sha}.pcm.zst", 200 + n),
        (f"frames-{sha}.jsonl.zst", 30 + n),
        (f"events-{sha}.jsonl.zst", 20 + n),
        (f"meta-{sha}.json", 10 + n),
        (f"manifest-{sha}.json", 5 + n),
    ])


SRC_ENTRIES = (
    six(S_HALO1, 1, "aa01") + six(S_HALO1, 2, "aa02")
    + six(S_HALO2, 1, "ab01")
    + six(S_FO4, 1, "bb01")
    + six(S_NOCAT, 1, "cc01")
    + clip(S_NOMETA, 1, [("video-dd01.mp4", 500)])
)

SESSION_GAMES = {S_HALO1: HALO, S_HALO2: HALO, S_FO4: FO4, S_NOCAT: None, S_NOMETA: None}


def lsf_text(entries):
    return "".join(f"{p};{s}\n" for p, s in entries)


class TestPureHelpers(unittest.TestCase):
    def test_parse_lsf_roundtrip(self):
        parsed = reclass.parse_lsf(lsf_text(SRC_ENTRIES) + "\n")  # 尾部空行也要能吃
        self.assertEqual(parsed, SRC_ENTRIES)

    def test_group_sessions(self):
        groups = reclass.group_sessions(SRC_ENTRIES)
        self.assertEqual(
            set(groups), {S_HALO1, S_HALO2, S_FO4, S_NOCAT, S_NOMETA})
        self.assertEqual(len(groups[S_HALO1]), 12)  # 两个 clip × 六件套
        self.assertEqual(len(groups[S_NOMETA]), 1)

    def test_pick_meta_skips_manifest(self):
        groups = reclass.group_sessions(SRC_ENTRIES)
        picked = reclass.pick_meta(groups[S_HALO1])
        self.assertTrue(picked.rsplit("/", 1)[-1].startswith("meta-"))
        self.assertEqual(picked, f"{S_HALO1}/runs/r0001/clips/c0001/meta-aa01.json")
        self.assertIsNone(reclass.pick_meta(groups[S_NOMETA]))

    def test_extract_game_id(self):
        self.assertEqual(
            reclass.extract_game_id({"catalog": {"game": {"id": HALO}}}), HALO)
        for meta in ({}, {"catalog": None}, {"catalog": {}},
                     {"catalog": {"game": "halo"}}, {"catalog": {"game": {"id": 3}}}):
            self.assertIsNone(reclass.extract_game_id(meta), meta)


class TestWhitelist(unittest.TestCase):
    def setUp(self):
        self.groups = reclass.group_sessions(SRC_ENTRIES)
        self.whitelist, self.excluded, self.unresolved = reclass.build_whitelist(
            self.groups, SESSION_GAMES, [HALO])

    def test_whitelist_only_halo(self):
        self.assertEqual(len(self.whitelist), 18)  # 3 halo clip × 6
        for path, _ in self.whitelist:
            self.assertTrue(path.startswith((S_HALO1, S_HALO2)), path)

    def test_excluded_histogram(self):
        self.assertEqual(self.excluded[FO4], [1, 6, sum(s for _, s in six(S_FO4, 1, "bb01"))])
        # 读不出 game 的两类（无 catalog / 无 meta）合并在同一个桶里
        self.assertEqual(self.excluded["<catalog.game.id 读不出>"][0], 2)
        self.assertEqual(self.excluded["<catalog.game.id 读不出>"][1], 6 + 1)

    def test_unresolved_sessions(self):
        self.assertEqual(sorted(self.unresolved), [S_NOCAT, S_NOMETA])

    def test_reconciliation_totals(self):
        count, nbytes = reclass.totals(self.whitelist)
        self.assertEqual(count, 18)
        expected = sum(
            s for p, s in SRC_ENTRIES if p.startswith((S_HALO1, S_HALO2)))
        self.assertEqual(nbytes, expected)


class TestDiff(unittest.TestCase):
    def test_diff_against_whitelist(self):
        whitelist = six(S_HALO1, 1, "aa01")
        oos = clip(S_OOS, 1, [("video-ee01.mp4", 777)])
        # dst：白名单全在但有一个 size 不一致，另有一个越界对象
        dst = [(p, s if "audio" not in p else s + 1) for p, s in whitelist] + oos
        bad, out_of_scope = reclass.diff_against_whitelist(whitelist, dst)
        self.assertEqual(len(bad), 1)
        self.assertIn("audio", bad[0])
        self.assertEqual(out_of_scope, oos)

    def test_diff_missing_object(self):
        whitelist = six(S_HALO1, 1, "aa01")
        bad, out_of_scope = reclass.diff_against_whitelist(whitelist, whitelist[:-1])
        self.assertEqual(bad, [whitelist[-1][0]])
        self.assertEqual(out_of_scope, [])

    def test_diff_clean_pass(self):
        whitelist = six(S_HALO1, 1, "aa01")
        bad, out_of_scope = reclass.diff_against_whitelist(whitelist, list(whitelist))
        self.assertEqual((bad, out_of_scope), ([], []))


class TestWaiver(unittest.TestCase):
    def test_build_waiver_fields(self):
        results = [
            {"name": "halo-production-ingest", "game": HALO,
             "src": "s", "dst": "d", "game_filter": [HALO],
             "objects": 18, "bytes": 4321,
             "known_out_of_scope_in_lane": {"objects": 3, "bytes": 999},
             "restrictions": ["r1"]},
            {"name": "cod-manual-dev", "game": "call-of-duty-steam-release-test",
             "src": "s2", "dst": "d2", "objects": 5, "bytes": 500,
             "restrictions": ["r2"]},
        ]
        w = reclass.build_waiver(results, refreshed_at="2026-08-16",
                                 generated_at="2026-08-16T00:00:00+00:00")
        self.assertEqual(w["attested_at"], "2026-08-10")  # 具结时点不因刷新而漂移
        self.assertEqual(w["refreshed_at"], "2026-08-16")
        self.assertEqual(w["sources"][0]["game_filter"], [HALO])
        self.assertEqual(w["sources"][0]["objects"], 18)
        self.assertEqual(w["sources"][0]["known_out_of_scope_in_lane"],
                         {"objects": 3, "bytes": 999})
        self.assertNotIn("game_filter", w["sources"][1])
        json.dumps(w, ensure_ascii=False)  # 必须可序列化


class TestExecuteEndToEnd(unittest.TestCase):
    """mock 掉 rclone/subprocess 后跑 cmd_execute 全链路，核对 WAIVER 落盘内容。"""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="reclass-test-")
        self._orig_cache = reclass.CACHE_DIR
        reclass.CACHE_DIR = self.tmp
        self._orig_rclone = reclass.rclone
        self._orig_run = reclass.subprocess.run
        self._orig_sources = reclass.SOURCES

        # 按 name 取而不是按下标——SOURCES 会随摄取车道增删（2026-09 加了 v2），
        # 位置索引会让这套测试在"加一条车道"时莫名其妙地错位。
        by_name = {s["name"]: dict(s) for s in reclass.SOURCES}
        self.halo_src = by_name["halo-production-ingest"]
        self.halo_v2_src = by_name["halo-production-ingest-v2"]
        self.cod_src = by_name["cod-manual-dev"]
        reclass.SOURCES = [self.halo_src, self.halo_v2_src, self.cod_src]
        self.bucket = "test-bucket"

        oos = clip(S_OOS, 1, [("video-ee01.mp4", 777)])
        cod_entries = [
            ("com.diogeneslab.dataminer/journal.sqlite3", 100),
            ("com.diogeneslab.dataminer/non-trainable-sessions/dev-cod-1/session.json", 5),
            ("com.diogeneslab.dataminer/non-trainable-sessions/dev-cod-1/runs/r0000/segments/00000/video.mp4", 300),
            ("com.diogeneslab.dataminer/non-trainable-sessions/dev-fo4-1/session.json", 6),
            ("com.diogeneslab.dataminer/non-trainable-sessions/dev-fo4-1/runs/r0000/segments/00000/video.mp4", 400),
        ]
        self.state = {
            self.halo_src["src"]: list(SRC_ENTRIES),
            self.halo_src["dst"]: list(oos),  # 车道内已有历史越界对象
            # raw/v2 车道现实里就是空的：Halo 08-12 停采，早于 08-29 的车道切换。
            # 源前缀为空 ≠ 过滤值写错，不该 die（见 filtered_state）。
            self.halo_v2_src["src"]: [],
            self.cod_src["src"]: cod_entries,
        }
        self.metas = {
            f"{S_HALO1}/runs/r0001/clips/c0001/meta-aa01.json": {"catalog": {"game": {"id": HALO}}},
            f"{S_HALO2}/runs/r0001/clips/c0001/meta-ab01.json": {"catalog": {"game": {"id": HALO}}},
            f"{S_FO4}/runs/r0001/clips/c0001/meta-bb01.json": {"catalog": {"game": {"id": FO4}}},
            f"{S_NOCAT}/runs/r0001/clips/c0001/meta-cc01.json": {"schema_version": "dm-1", "catalog": None},
        }
        self.cod_metas = {
            "com.diogeneslab.dataminer/non-trainable-sessions/dev-cod-1/session.json":
                {"catalog": {"game": {"id": "call-of-duty-steam-release-test"}}},
            "com.diogeneslab.dataminer/non-trainable-sessions/dev-fo4-1/session.json":
                {"catalog": {"game": {"id": FO4}}},
        }
        self.waiver_body = None
        reclass.rclone = self.fake_rclone
        reclass.subprocess.run = self.fake_run

    def tearDown(self):
        reclass.CACHE_DIR = self._orig_cache
        reclass.rclone = self._orig_rclone
        reclass.subprocess.run = self._orig_run
        reclass.SOURCES = self._orig_sources

    def _prefix_of(self, remote):
        assert remote.startswith(f"r2:{self.bucket}/")
        return remote[len(f"r2:{self.bucket}/"):]

    def fake_rclone(self, args, capture=True):
        cmd = args[0]
        if cmd == "lsf":
            return lsf_text(self.state.get(self._prefix_of(args[-1]), []))
        if cmd == "cat":
            full = self._prefix_of(args[1])
            if full.startswith(self.cod_src["src"] + "/"):
                return json.dumps(self.cod_metas[full[len(self.cod_src["src"]) + 1:]])
            rel = full[len(self.halo_src["src"]) + 1:]
            return json.dumps(self.metas[rel])
        if cmd == "size":
            prefix = self._prefix_of(args[1])
            if prefix in self.state:
                c, b = reclass.totals(self.state[prefix])
            else:  # cod 前缀：src/dst 恒一致
                c, b = 5, 500
            return json.dumps({"count": c, "bytes": b})
        if cmd == "copy":
            src, dst = self._prefix_of(args[1]), self._prefix_of(args[2])
            if "--files-from" in args:  # 只复制白名单
                with open(args[args.index("--files-from") + 1]) as f:
                    wanted = {line.strip() for line in f if line.strip()}
                src_map = dict(self.state[src])
                dst_map = dict(self.state.get(dst, []))
                for p in sorted(wanted):
                    dst_map[p] = src_map[p]
                self.state[dst] = sorted(dst_map.items())
            return ""
        raise AssertionError(f"unexpected rclone call: {args}")

    def fake_run(self, cmd, **kwargs):
        assert cmd[:2] == ["rclone", "rcat"], cmd
        self.waiver_body = kwargs["input"]
        return types.SimpleNamespace(returncode=0)

    def test_execute_writes_filtered_waiver(self):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            reclass.cmd_execute(self.bucket, reclass.SOURCES)

        # 1) 复制结果：dst = 白名单 ∪ 历史越界，FO4/无 game session 一个都没进来
        dst = dict(self.state[self.halo_src["dst"]])
        self.assertEqual(len(dst), 18 + 1)
        self.assertFalse(any(p.startswith((S_FO4, S_NOCAT, S_NOMETA)) for p in dst))

        # 2) WAIVER 内容：白名单口径读数 + game_filter + refreshed_at + 越界回填
        w = json.loads(self.waiver_body)
        self.assertEqual(w["attested_at"], "2026-08-10")
        self.assertEqual(
            w["refreshed_at"],
            __import__("datetime").datetime.now(
                __import__("datetime").timezone.utc).date().isoformat())
        halo = w["sources"][0]
        self.assertEqual(halo["game_filter"], [HALO])
        self.assertEqual(halo["objects"], 18)
        self.assertEqual(
            halo["bytes"],
            sum(s for p, s in SRC_ENTRIES if p.startswith((S_HALO1, S_HALO2))))
        self.assertEqual(halo["known_out_of_scope_in_lane"], {"objects": 1, "bytes": 777})
        halo_v2 = w["sources"][1]
        self.assertEqual(halo_v2["name"], "halo-production-ingest-v2")
        self.assertEqual(halo_v2["skipped"], "source_prefix_empty")
        self.assertEqual((halo_v2["objects"], halo_v2["bytes"]), (0, 0))
        cod = w["sources"][2]
        self.assertEqual(cod["game_filter"], ["call-of-duty-steam-release-test"])
        self.assertEqual(cod["objects"], 3)  # journal + dev-cod-1 两件；dev-fo4-1 被排除
        cod_dst = dict(self.state[self.cod_src["dst"]])
        self.assertIn("com.diogeneslab.dataminer/journal.sqlite3", cod_dst)
        self.assertFalse(any("dev-fo4-1" in p for p in cod_dst))

        # 3) 排除与越界都要在 stdout 报告里出现
        text = out.getvalue()
        self.assertIn(FO4, text)
        self.assertIn("读不出 catalog.game.id", text)
        self.assertIn(S_OOS, text)

        # 4) session→game cache 落盘且可复用（幂等重跑不再读 meta）
        cache_file = os.path.join(
            self.tmp, f"session-game-{self.bucket}-{self.halo_src['name']}.json")
        with open(cache_file) as f:
            cached = json.load(f)["sessions"]
        self.assertEqual(cached[S_HALO1], HALO)
        self.assertIsNone(cached[S_NOMETA])
        self.metas.clear()  # 再跑一遍：若 cache 不生效会 KeyError
        with contextlib.redirect_stdout(io.StringIO()):
            reclass.cmd_execute(self.bucket, reclass.SOURCES)

    def test_empty_source_prefix_is_skipped_not_fatal(self):
        """新挂一条车道必然先经历"还没有数据"这个状态，不能把工具拖死。"""
        st = reclass.filtered_state(self.bucket, self.halo_v2_src)
        self.assertEqual(st["whitelist"], [])

    def test_filter_drift_is_still_fatal(self):
        """前缀下**有** session 却一个都没过白名单 = 过滤值与 catalog.game.id 对不上，
        这正是这条闸要拦的，必须保持致命。"""
        drifted = dict(self.halo_src, name="typo", game_filter=["halo-infinite-typo"])
        with self.assertRaises(SystemExit):
            reclass.filtered_state(self.bucket, drifted)

    def test_execute_dies_when_whitelist_object_missing(self):
        # copy 后偷偷从 dst 拿掉一个白名单对象 → execute 必须 die 且不写 WAIVER
        real_fake = self.fake_rclone

        def sabotage(args, capture=True):
            out = real_fake(args, capture)
            if args[0] == "copy" and "--files-from" in args:
                dst = self.state[self.halo_src["dst"]]
                # 排序后首个对象必是白名单内（S_OOS 的 uuid 排在最后）
                self.state[self.halo_src["dst"]] = dst[1:]
            return out

        reclass.rclone = sabotage
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                reclass.cmd_execute(self.bucket, reclass.SOURCES)
        self.assertIsNone(self.waiver_body)


class TestManualLayout(unittest.TestCase):
    """manual 布局（journal 镜像）：session 键在 non-trainable-sessions/ 之后，
    证据文件是 <sid>/session.json，设备级文件恒放行。"""

    ENTRIES = [
        ("com.diogeneslab.dataminer/journal.sqlite3", 100),
        ("com.diogeneslab.dataminer/journal.sqlite3-wal", 10),
        ("com.diogeneslab.dataminer/non-trainable-sessions/dev-1/session.json", 5),
        ("com.diogeneslab.dataminer/non-trainable-sessions/dev-1/runs/r0000/segments/00000/video.mp4", 999),
        ("com.diogeneslab.dataminer/non-trainable-sessions/dev-2/session.json", 6),
        ("com.diogeneslab.dataminer/non-trainable-sessions/dev-2/runs/r0000/segments/00000/video.mp4", 888),
    ]

    def test_group_manual(self):
        g = reclass.group_sessions(self.ENTRIES, "manual")
        self.assertEqual(set(g), {reclass.DEVICE_LEVEL, "dev-1", "dev-2"})
        self.assertEqual(len(g[reclass.DEVICE_LEVEL]), 2)

    def test_pick_meta_manual(self):
        g = reclass.group_sessions(self.ENTRIES, "manual")
        self.assertTrue(reclass.pick_meta(g["dev-1"], "manual").endswith("dev-1/session.json"))
        self.assertIsNone(reclass.pick_meta(g[reclass.DEVICE_LEVEL], "manual"))

    def test_whitelist_manual_filters_fo4_keeps_device_level(self):
        g = reclass.group_sessions(self.ENTRIES, "manual")
        games = {"dev-1": "call-of-duty-steam-release-test", "dev-2": "fallout4-steam-release-test"}
        wl, excluded, unresolved = reclass.build_whitelist(g, games, ["call-of-duty-steam-release-test"])
        paths = {p for p, _ in wl}
        self.assertIn("com.diogeneslab.dataminer/journal.sqlite3", paths)
        self.assertIn("com.diogeneslab.dataminer/non-trainable-sessions/dev-1/session.json", paths)
        self.assertNotIn("com.diogeneslab.dataminer/non-trainable-sessions/dev-2/session.json", paths)
        self.assertEqual(excluded["fallout4-steam-release-test"][0], 1)
        self.assertEqual(unresolved, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
