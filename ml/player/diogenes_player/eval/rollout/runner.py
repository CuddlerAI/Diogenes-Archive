"""W4 rollout runner CLI: 组装 capture/policy/injector/GSI/协议, 产出 kd.csv + telemetry.csv。

macOS/CI (结构验证):
    python -m diogenes_player.eval.rollout.runner --onnx policy.onnx \
        --gru-hidden 96 --capture fake --inject fake --hz 60 \
        --protocol deathmatch --episodes 2 --duration 5 --out runs/rollout_fake

★ 红线（research/07 §4.1）: 闭环注入只在 CS2 -insecure / 无反作弊探针环境；**永不对 COD**。
sendinput 后端每次注入前守卫前台进程，命中 COD 名单即拒绝。

★ C-1 授权游戏（Halo Infinite, research/27 §1）: 注入必须带齐三要素
    --auth-consent-id <协议ID> --auth-account <账号> --auth-allowlist <白名单,逗号分隔>
缺任一守卫 fail-closed 拒注入。H-1 第 5 项注入实测（无模型, RandomPolicy）:
    python -m diogenes_player.eval.rollout.runner --capture fake --inject sendinput \
        --mapping <halo-infinite mapping.json> --hz 30 --no-gsi \
        --episodes 1 --duration 600 \
        --auth-consent-id C1-2026-08-01-PENDING --auth-account <账号> --auth-allowlist <账号>

CS2 游戏机 (W4-real, 操作手册):
    1. CS2 cfg 目录放入 configs/gamestate_integration_diogenes.cfg
    2. 启动项加 -insecure, 窗口化; 本地服: map de_dust2 + bot_add
    3. --capture dxcam --inject sendinput --mapping <cs2 mapping.json> --region L,T,R,B
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from ...action_registry import default_cod_vocab
from .gsi import GsiServer, GsiState
from .loop import FakeCapture, FakeInjector, OnnxPolicy, RandomPolicy, RolloutLoop
from .protocol import median_kd, run_episodes, to_csv
from .telemetry import Telemetry


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--onnx", default=None, help="W3 导出的单步 ONNX; 缺省用 RandomPolicy")
    ap.add_argument("--mapping", default=None, help="sendinput 逆映射用 mapping.json")
    ap.add_argument("--gru-layers", type=int, default=1)
    ap.add_argument("--gru-hidden", type=int, default=512)
    ap.add_argument("--capture", choices=["fake", "dxcam"], default="fake")
    ap.add_argument("--inject", choices=["fake", "sendinput"], default="fake")
    ap.add_argument("--region", default=None, help="dxcam L,T,R,B")
    ap.add_argument("--height", type=int, default=144)
    ap.add_argument("--width", type=int, default=256)
    ap.add_argument("--hz", type=float, default=30.0)
    ap.add_argument("--protocol", choices=["aim_botz", "deathmatch"], default="deathmatch")
    ap.add_argument("--episodes", type=int, default=3)
    ap.add_argument("--duration", type=float, default=1800.0, help="秒/episode")
    ap.add_argument("--gsi-port", type=int, default=3000)
    ap.add_argument("--no-gsi", action="store_true", help="无 GSI (纯延迟测试)")
    ap.add_argument("--auth-consent-id", default=None,
                    help="C-1 授权协议 ID（授权游戏注入必需, research/27 §1）")
    ap.add_argument("--auth-account", default=None, help="当前登录的白名单账号 ID")
    ap.add_argument("--auth-allowlist", default=None, help="法务备案账号白名单, 逗号分隔")
    ap.add_argument("--out", default="runs/rollout")
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    if args.capture == "dxcam":
        from .windows import DxcamCapture

        region = tuple(int(x) for x in args.region.split(","))
        capture = DxcamCapture(region=region, out_height=args.height,
                               out_width=args.width)
    else:
        capture = FakeCapture(args.height, args.width)

    if args.inject == "sendinput":
        from .windows import MappingResolver, SelfplayAuthorization, SendInputInjector

        if not args.mapping:
            ap.error("--inject sendinput 需要 --mapping <mapping.json>")
        auth = None
        if args.auth_consent_id or args.auth_account or args.auth_allowlist:
            auth = SelfplayAuthorization(
                consent_id=args.auth_consent_id or "",
                account_id=args.auth_account or "",
                allowlist=frozenset(a for a in (args.auth_allowlist or "").split(",") if a))
        injector = SendInputInjector(MappingResolver(args.mapping), authorization=auth)
    else:
        injector = FakeInjector()

    if args.onnx:
        policy = OnnxPolicy(args.onnx, default_cod_vocab(),
                            gru_layers=args.gru_layers, gru_hidden=args.gru_hidden)
    else:
        policy = RandomPolicy()

    gsi = GsiState()
    server = None
    if not args.no_gsi:
        server = GsiServer(gsi, port=args.gsi_port)
        server.start()
        print(f"GSI listening on 127.0.0.1:{server.port}")

    loop = RolloutLoop(capture, policy, injector, hz=args.hz, telemetry=Telemetry())
    try:
        results = run_episodes(loop, gsi, protocol=args.protocol,
                               episodes=args.episodes, duration_s=args.duration)
    finally:
        if server:
            server.stop()
        if hasattr(injector, "release_all"):
            injector.release_all()

    to_csv(results, out / "kd.csv")
    loop.telemetry.to_csv(out / "telemetry.csv")
    report = {"episodes": [vars(r) for r in results],
              "median_kd": median_kd(results),
              "latency": loop.telemetry.summary()}
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report["latency"], indent=2))
    print("median K/D:", report["median_kd"])


if __name__ == "__main__":
    main()
