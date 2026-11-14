"""W4: GSI 记分 / rollout 环 / 协议 / ONNX policy 后端 (全部 fake 后端, 跨平台)。"""
import json
import urllib.request

import numpy as np
import pytest

from diogenes_player import contract
from diogenes_player.eval.rollout.gsi import GsiServer, GsiState
from diogenes_player.eval.rollout.loop import (
    FakeCapture, FakeInjector, RandomPolicy, RolloutLoop)
from diogenes_player.eval.rollout.protocol import median_kd, run_episodes
from diogenes_player.eval.rollout.telemetry import Telemetry


def _payload(kills, deaths, map_name="de_dust2"):
    return {"map": {"phase": "live", "name": map_name},
            "round": {"phase": "live"},
            "player": {"match_stats": {"kills": kills, "deaths": deaths}}}


def test_gsi_kill_death_diff():
    s = GsiState()
    s.update(_payload(0, 0), now=1.0)
    s.update(_payload(2, 1), now=2.0)
    s.update(_payload(3, 1), now=3.0)
    assert s.counts_since(0.0) == (3, 1)
    assert s.counts_since(2.5) == (1, 0)


def test_gsi_map_change_resets_baseline():
    s = GsiState()
    s.update(_payload(5, 3), now=1.0)
    s.update(_payload(0, 0, map_name="de_mirage"), now=2.0)  # 换图归零
    s.update(_payload(1, 0, map_name="de_mirage"), now=3.0)
    assert s.counts_since(0.0) == (1, 0)  # 不产生负事件, 新图从 0 记


def test_gsi_http_server_roundtrip():
    s = GsiState()
    server = GsiServer(s, port=0)  # 随机端口
    server.start()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{server.port}", method="POST",
            data=json.dumps(_payload(1, 0)).encode(),
            headers={"Content-Type": "application/json"})
        assert urllib.request.urlopen(req, timeout=5).status == 200
        assert s.snapshot.kills == 1
    finally:
        server.stop()


def test_rollout_loop_ticks_and_telemetry():
    inj = FakeInjector()
    loop = RolloutLoop(FakeCapture(), RandomPolicy(), inj, hz=500,
                       telemetry=Telemetry())
    tel = loop.run(max_ticks=50)
    assert len(tel.ticks) == 50
    assert len(inj.calls) == 50
    summ = tel.summary()
    assert summ["n"] == 50
    assert summ["total_ms"]["p50"] >= 0
    starts = [t.t_start for t in tel.ticks]
    assert all(b > a for a, b in zip(starts, starts[1:]))  # 单调


def test_protocol_episodes_kd(monkeypatch):
    gsi = GsiState()
    gsi.update(_payload(0, 0), now=0.0)  # 打基线: 首个 payload 视为换图, 只 rebase 不计事件
    loop = RolloutLoop(FakeCapture(), RandomPolicy(), FakeInjector(), hz=500)
    ep_data = [(3, 1), (2, 2)]

    def feed(ep):
        import time
        base = gsi.snapshot
        k, d = ep_data[ep]
        gsi.update(_payload(base.kills + k, base.deaths + d),
                   now=time.monotonic() + 0.05)

    # 每个 episode 开始后灌入该局的 GSI 增量 (事件时间戳落在窗口内)
    results = run_episodes(loop, gsi, protocol="deathmatch", episodes=2,
                           duration_s=0.1,
                           on_episode_start=lambda ep: feed(ep))
    assert (results[0].kills, results[0].deaths) == (3, 1)
    assert (results[1].kills, results[1].deaths) == (2, 2)
    assert results[0].kd == 3.0
    assert median_kd(results) == 2.0


def test_onnx_policy_backend(tmp_path):
    torch = pytest.importorskip("torch")
    pytest.importorskip("onnxruntime")
    from diogenes_player.action_registry import default_cod_vocab
    from diogenes_player.deploy.export import export_onnx
    from diogenes_player.eval.rollout.loop import OnnxPolicy
    from diogenes_player.models.policy import PlayerPolicy, PolicyConfig

    vocab = default_cod_vocab()
    torch.manual_seed(0)
    p = PlayerPolicy(PolicyConfig(encoder="tiny_cnn", d_model=64, gru_hidden=96,
                                  n_actions=vocab.n_actions,
                                  freeze_encoder=False)).eval()
    onnx_path = str(tmp_path / "p.onnx")
    export_onnx(p, onnx_path, height=72, width=128)

    backend = OnnxPolicy(onnx_path, vocab, gru_layers=1, gru_hidden=96)
    frame = np.random.default_rng(0).integers(0, 255, (72, 128, 3), dtype=np.uint8)
    actions, dx, dy = backend.step(frame)
    assert actions <= set(vocab.actions)          # 只吐词表内语义
    assert -contract.REF <= dx <= contract.REF
    assert -contract.REF <= dy <= contract.REF


def test_cod_process_guard():
    """COD 进程名必须命中红线；CS2/探针进程不命中（research/07 §4.1）。"""
    from diogenes_player.eval.rollout.windows import is_forbidden_process
    for name in ("cod.exe", "cod23.exe", "ModernWarfare.exe", "Warzone.exe"):
        assert is_forbidden_process(name), name
    for name in ("cs2.exe", "SimpleFPS.exe", "vizdoom.exe", ""):
        assert not is_forbidden_process(name), name


def test_halo_injection_asymmetry():
    """两款 Halo 的非对称（C-1 授权后的新语义，research/27 §1）：

    MCC 走「可证明反作弊未加载」通道（EAC-off 启动项）——无需授权上下文直接放行；
    Infinite 自 2026-08-01 起从硬拒绝改为 **C-1 授权通道**——不在硬拒绝表，但无
    SelfplayAuthorization 时注入仍被拒（fail-closed）。
    """
    from diogenes_player.eval.rollout.windows import (
        injection_block_reason, is_forbidden_process, requires_authorization)
    for name in ("HaloInfinite.exe", "haloinfinite.exe"):
        assert not is_forbidden_process(name), f"{name} 不再硬拒绝（C-1 授权通道）"
        assert requires_authorization(name), f"{name} 应要求授权上下文"
        assert injection_block_reason(name, None) is not None, \
            f"{name} 无授权上下文时必须 fail-closed 拒绝"
    for name in ("MCC-Win64-Shipping.exe", "mcclauncher.exe"):
        assert not is_forbidden_process(name), f"{name} 应放行（有 EAC-off 启动项，是 P3 闭环环境）"
        assert not requires_authorization(name), f"{name} 无需授权上下文"
        assert injection_block_reason(name, None) is None, f"{name} 应直接放行"


def test_selfplay_authorization_gate():
    """C-1 授权三要素缺一即拒：consent_id、account_id、白名单命中（research/27 §1）。"""
    from diogenes_player.eval.rollout.windows import (
        SelfplayAuthorization, injection_block_reason)
    ok = SelfplayAuthorization(consent_id="C1-2026-08-01",
                               account_id="rig-01",
                               allowlist=frozenset({"rig-01", "rig-02"}))
    assert ok.problem() is None
    assert injection_block_reason("HaloInfinite.exe", ok) is None

    for bad in (
        SelfplayAuthorization("", "rig-01", frozenset({"rig-01"})),          # 无协议编号
        SelfplayAuthorization("C1-2026-08-01", "", frozenset({"rig-01"})),   # 无账号
        SelfplayAuthorization("C1-2026-08-01", "rig-99", frozenset({"rig-01"})),  # 不在白名单
        SelfplayAuthorization("C1-2026-08-01", "rig-01", frozenset()),       # 空白名单
    ):
        assert bad.problem() is not None
        assert injection_block_reason("HaloInfinite.exe", bad) is not None

    # 授权上下文不放宽其他红线：COD 仍硬拒绝，空进程名仍 fail-closed
    assert injection_block_reason("cod.exe", ok) is not None
    assert injection_block_reason("", ok) is not None


def test_mapping_resolver_semantic_to_physical():
    from pathlib import Path

    from diogenes_player.eval.rollout.windows import MappingResolver
    repo = Path(__file__).resolve().parents[3]
    mp = repo / "DataMiner/config/catalogs/dev/cod-steam-release-test/mapping.json"
    if not mp.exists():
        pytest.skip("COD mapping 不在")
    r = MappingResolver(mp)
    scans, btns = r.resolve({"move.forward", "combat.primary_attack", "camera.yaw"})
    assert (17, False) in scans            # W scan code
    assert 1 in btns                        # 左键
    # camera.* 不产生物理键（由 dx/dy 驱动）
    assert r.resolve({"camera.yaw"}) == (set(), set())


def test_windows_backends_refuse_on_non_windows():
    import sys
    if sys.platform == "win32":
        pytest.skip("Windows 上跳过反向测试")
    from diogenes_player.eval.rollout.windows import MappingResolver, SendInputInjector
    from pathlib import Path
    repo = Path(__file__).resolve().parents[3]
    mp = repo / "DataMiner/config/catalogs/dev/cod-steam-release-test/mapping.json"
    resolver = MappingResolver(mp) if mp.exists() else None
    with pytest.raises(RuntimeError, match="仅在 Windows"):
        SendInputInjector(resolver)
