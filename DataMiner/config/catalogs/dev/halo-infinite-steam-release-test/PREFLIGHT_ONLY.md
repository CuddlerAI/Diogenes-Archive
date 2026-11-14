# 预检构建可用 · D6 未定稿（正式 S0 采集前必须回填实测值）

**状态更新 2026-08-04**：本 bundle 已登记于 shipped-games.json 完成显式登记（2026-08-23 起随 dev-catalog 内嵌，逐游戏 feature 退役），
并由 All Games DEV 安装包组合启用。默认/production
构建仍不包含它；预检产出全部 `non_trainable: true`。

## 仍未定稿的部分（H-1 真机回填）

- 本机已确认 Steam App 1240440 / build 22709428，但同名 `HaloInfinite.exe` 有 launcher、
  multiplayer 与 campaign 三份不同映像；`window_class` / `executable_sha256` 继续留 `null`，
  等 H-1 从真实目标窗口判定应钉的运行映像
- `view_scale`：`counts_per_360` / `vertical_scale_ratio` / `ads_states` 仍为空；回填前 converter
  不产出角度列
- `in_game_mouse_sensitivity`：`null`，开测前固化并回填
- mapping：研究侧按官方资料还原，H-1 逐键核对后才算定稿
- 合规：C-1 授权已落地；认证与 trainable 晋升仍走独立治理流程

## 剩余流程

1. H-1 preflight 真机执行
2. D6 定稿：回填实测值、升版并重算内容 hash
3. D7 另起会话完成真机 smoke 与故障矩阵

执行手册：[h1-preflight-execution-kit](../../../../../model_research/research/halo/h1-preflight-execution-kit.md)、
[s0-ignition-guide](../../../../../model_research/research/halo/s0-ignition-guide.md)。
