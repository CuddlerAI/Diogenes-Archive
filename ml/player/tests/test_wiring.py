"""接线待用：真实 converter 布局的深度无关发现、身份来自 meta、preflight、strict-bins。"""
import json
import shutil

import pytest

from diogenes_player.action_registry import default_cod_vocab
from diogenes_player.dataset import WindowDataset
from diogenes_player.ingest import discover_sessions, ingest
from diogenes_player.physical_vocab import default_cod_physical_vocab
from diogenes_player.preflight import preflight
from diogenes_player.synth import synth_session

pytestmark = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="需要 ffmpeg")


@pytest.fixture(scope="module")
def real_layout(tmp_path_factory):
    """模拟真实 converter 布局: ready/<session>/<game_slug>/<session>/cNNNN。"""
    root = tmp_path_factory.mktemp("real")
    ready = root / "non-trainable-data-model" / "ready"
    sess = "cod-1784900000000-0000"
    # synth 写 <out>/<game>/<session>/cNNNN；把 out 设成 ready/<session> 即得三层嵌套
    synth_session(ready / sess, game="call-of-duty-steam-pc", session=sess,
                  chunks=3, w=128, h=72, n_runs=2, seed=21)
    return ready


def test_discovery_depth_agnostic(real_layout):
    """指向 ready/ 根（chunk 在其下 3 层）也能发现 session。"""
    sessions = discover_sessions(real_layout)
    assert len(sessions) == 1
    assert (sessions[0] / "metadata.json").exists()


def test_ingest_real_layout_and_identity(real_layout, tmp_path):
    out = tmp_path / "plane"
    stats = ingest(real_layout, out, width=128, height=72, gop=30,
                   scheme="dm1-v2", strict_bins=True,
                   physical_vocab=default_cod_physical_vocab())
    assert stats["failed"] == []
    assert stats["sessions"] == 1
    assert stats["chunks"] == 3
    # 身份来自 meta 的 game 字段（非路径位置）
    assert "call-of-duty-steam-pc" in stats["games"] or list(stats["games"])
    assert stats["non_trainable_chunks"] == 3  # synth 恒 non_trainable
    assert stats["trainable_chunks"] == 0
    # 索引可被 dataset 消费
    ds = WindowDataset(out, t=24, vocab=default_cod_vocab(),
                       allow_untrainable=True, seed=1)
    assert len(ds) > 0


def test_preflight_passes_on_synth(real_layout):
    report = preflight(real_layout, scheme="dm1-v2",
                       physical_vocab=default_cod_physical_vocab())
    assert report["blocking"] == [], report["blocking"]
    assert report["sessions"] == 1
    assert report["chunks"] == 3
    assert report["non_trainable_chunks"] == 3
    assert report["fps"] == {"30": 3}  # synth 默认 30fps,单值不阻断
    assert report["action_coverage"]["coverage_pct"] > 0
    assert not report["sessions_without_run_ranges"]  # synth 带 run ranges


def test_preflight_blocks_mixed_fps(tmp_path):
    """两个 session 各 30/60fps → 混合语料必须阻断（docs/50 P-4）。"""
    ready = tmp_path / "ready"
    synth_session(ready / "s30", game="g", session="s30", chunks=1,
                  w=128, h=72, n_runs=1, seed=7)
    synth_session(ready / "s60", game="g", session="s60", chunks=1,
                  w=128, h=72, n_runs=1, seed=8)
    meta_f = next((ready / "s60").rglob("c0000/meta.json"))
    meta = json.loads(meta_f.read_text())
    meta["video"]["fps"] = 60
    meta_f.write_text(json.dumps(meta))

    report = preflight(ready, scheme="dm1-v2",
                       physical_vocab=default_cod_physical_vocab())
    assert report["fps"] == {"30": 1, "60": 1}
    assert any("混合 fps" in b for b in report["blocking"])

    # --expect-fps 钉死后,不符者阻断、符合者放行
    report = preflight(ready, scheme="dm1-v2", expect_fps=30,
                       physical_vocab=default_cod_physical_vocab())
    assert any("fps 不符" in b for b in report["blocking"])
    report = preflight(ready / "s30", scheme="dm1-v2", expect_fps=30,
                       physical_vocab=default_cod_physical_vocab())
    assert not any("fps" in b for b in report["blocking"]), report["blocking"]


def test_preflight_missing_fps_is_schema_error(tmp_path):
    ready = tmp_path / "ready"
    synth_session(ready / "s1", game="g", session="s1", chunks=1,
                  w=128, h=72, n_runs=1, seed=9)
    meta_f = next(ready.rglob("c0000/meta.json"))
    meta = json.loads(meta_f.read_text())
    del meta["video"]["fps"]
    meta_f.write_text(json.dumps(meta))
    report = preflight(ready, scheme="dm1-v2",
                       physical_vocab=default_cod_physical_vocab())
    assert any("缺 video.fps" in e for e in report["schema_errors"])
    assert any("schema 错误" in b for b in report["blocking"])


def test_preflight_detects_wrong_scheme(real_layout):
    """synth 是 dm1-v2；用 legacy 自检必然报契约漂移。"""
    report = preflight(real_layout, scheme="legacy",
                       physical_vocab=default_cod_physical_vocab())
    assert any("requires scheme='dm1-v2'" in b for b in report["blocking"])
    assert report["bin_drift"]
    assert any("漂移" in b for b in report["blocking"])


def test_preflight_empty_src_blocks(tmp_path):
    report = preflight(tmp_path, physical_vocab=default_cod_physical_vocab())
    assert report["blocking"]
    assert report["sessions"] == 0


def test_strict_bins_catches_drift(real_layout, tmp_path):
    """人为篡改一个 chunk 的 pointerBins → strict-bins ingest 拒收该 chunk。"""
    # 找一个 p0.jsonl，改一行 bins
    jl = next(real_layout.rglob("c0000/p0.jsonl"))
    lines = jl.read_text().splitlines()
    row = json.loads(lines[0])
    row["pointerBins"] = [row["pointerBins"][0] + 3, row["pointerBins"][1]]  # 错值
    lines[0] = json.dumps(row)
    jl.write_text("\n".join(lines) + "\n")
    stats = ingest(real_layout, tmp_path / "plane2", width=128, height=72,
                   gop=30, scheme="dm1-v2", strict_bins=True,
                   physical_vocab=default_cod_physical_vocab())
    assert any("漂移" in f["error"] for f in stats["failed"])


def test_preflight_distinguishes_ntsc_from_integer_fps(tmp_path):
    """59.94(60000/1001)与 60.00 取整后同为 60——门必须按有理数真值区分（docs/50 P-8）。"""
    ready = tmp_path / "ready"
    synth_session(ready / "s60", game="g", session="s60", chunks=1,
                  w=128, h=72, n_runs=1, seed=11)
    synth_session(ready / "sntsc", game="g", session="sntsc", chunks=1,
                  w=128, h=72, n_runs=1, seed=12)
    for sess, video in (("s60", {"fps": 60}),
                        ("sntsc", {"fps": 60, "fps_num": 60000, "fps_den": 1001})):
        meta_f = next((ready / sess).rglob("c0000/meta.json"))
        meta = json.loads(meta_f.read_text())
        meta["video"].update(video)
        meta_f.write_text(json.dumps(meta))
    report = preflight(ready, scheme="dm1-v2",
                       physical_vocab=default_cod_physical_vocab())
    assert set(report["fps"]) == {"60", f"{60000 / 1001:g}"}
    assert any("混合 fps" in b for b in report["blocking"])
