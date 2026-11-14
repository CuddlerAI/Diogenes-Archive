"""tools/r2-game-index/build_index.py 的离线单元测试(docs/149 §2)."""
import importlib.util
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "build_index",
    Path(__file__).resolve().parents[2] / "tools" / "r2-game-index" / "build_index.py")
bi = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(bi)

SHA = "0" * 64


def _key(sid, clip):
    return f"sessions/{sid}/runs/r0/clips/{clip}/meta-{SHA}.json"


def test_read_meta_receives_full_path_with_prefix():
    """回归: 清单行带上级前缀(prod 车道实况)时, read_meta 必须收到整行路径。"""
    key = f"dataminer-api-production/sessions/s1/runs/r0/clips/c1/meta-{SHA}.json"
    seen = []

    def read(rel):
        seen.append(rel)
        return {"target": {"game_id": "halo"}}

    idx = bi.build("lane", [key], read, thorough=False)
    assert seen == [key]
    assert idx["by_game"]["halo"]["sessions"] == 1


def _meta(gid, n_frames=600, fps=60):
    m = {"n_frames": n_frames, "fps_num": fps, "fps_den": 1}
    if gid is not None:
        m["target"] = {"game_id": gid}
    return m


def test_groups_by_game_and_flags_missing_target():
    listing = [_key("s1", "c1"), _key("s1", "c2"), _key("s2", "c1"), _key("s3", "c1")]
    metas = {_key("s1", "c1"): _meta("halo"), _key("s1", "c2"): _meta("halo"),
             _key("s2", "c1"): _meta("cod"), _key("s3", "c1"): _meta(None)}
    idx = bi.build("lane", listing, lambda k: metas[k], thorough=False)
    assert idx["by_game"]["halo"]["sessions"] == 1
    assert idx["by_game"]["cod"]["clips"] == 1
    assert idx["by_game"][bi.MISSING]["sessions"] == 1  # 缺 target 不猜路径
    assert idx["sessions"]["s1"]["clips"] == 2
    assert idx["mixed_sessions"] == []


def test_thorough_detects_intra_session_mixing_and_hours():
    listing = [_key("s1", "c1"), _key("s1", "c2")]
    metas = {_key("s1", "c1"): _meta("halo"), _key("s1", "c2"): _meta("wuchang")}
    idx = bi.build("lane", listing, lambda k: metas[k], thorough=True)
    assert idx["mixed_sessions"] == ["s1"]
    assert idx["sessions"]["s1"]["hours"] == round(2 * 600 / 60 / 3600, 4)


def test_sampled_reads_one_meta_per_session():
    calls = []
    listing = [_key("s1", f"c{i}") for i in range(5)]

    def read(k):
        calls.append(k)
        return _meta("halo")

    idx = bi.build("lane", listing, read, thorough=False)
    assert len(calls) == 1                       # 抽样模式只读首个
    assert idx["sessions"]["s1"]["metas_read"] == 1
    assert idx["mode"] == "sampled_first_meta"


def test_unreadable_meta_counted_not_fatal():
    listing = [_key("s1", "c1")]
    idx = bi.build("lane", listing, lambda k: None, thorough=False)
    assert idx["sessions"]["s1"]["game_id"] == bi.MISSING
    assert idx["sessions"]["s1"]["metas_unreadable"] == 1
