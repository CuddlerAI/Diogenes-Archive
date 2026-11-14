# diogenes-player · Player Model 训练管线

设计文档：[model_research/research/04-player-model-training-framework.md](../../model_research/research/04-player-model-training-framework.md)

## 当前状态：W1–W4 代码全部落地（Windows 实机验证项见下）

| 模块 | 内容 | 状态 |
|---|---|---|
| `contract.py` | dm-1 帧行/μ-law 翻译；32 键常量仅保留 data_demo legacy 兼容 | ✅ W1 黄金测试对 data_demo 全量锁定 |
| `physical_vocab.py` | 从 per-game mapping + game-profile 冻结 VK/鼠标键词表、原始文件哈希和未知输入拒绝策略 | ✅ T6：plane/checkpoint 同 hash，未知输入 fail-closed |
| `video.py` | 窗口解码三后端：torchcodec(训练机) → PyAV(开发机) → ffmpeg 管道(兜底) | ✅ W1 |
| `ingest.py` | dm-1 DATASET 平面 → 训练代理平面（低分辨率 mp4 GOP=1s + actions.parquet + 治理索引） | ✅ W1 |
| `dataset.py` + `data_torch.py` | 随机时间窗采样 + torch IterableDataset；默认拒绝 `is_trainable=false` | ✅ W1/W2（CS1: 事件加权、跨 chunk 拼接）|
| `bench.py` | dataloader 吞吐基准 | ✅ W1：4 workers 5,065 帧/s（macOS/pyav）|
| `models/` | 可插拔 encoder + GRU 双头 policy；BCE(pos_weight)+CE 损失 | ✅ W2（dinov3_s 留 CS1 + Gate-0 探针）|
| `train/bc.py` | BC 循环（burn-in BPTT、VPT 超参起点、JSONL 日志） | ✅ W2（多卡 accelerate 排 CS1）|
| `eval/offline.py` | per-key F1 / mouse R² / 回归门 CLI | ✅ W2 |
| `deploy/export.py` | 单步 ONNX（h_in/h_out 显式 I/O）+ ORT 对账 + 微基准 | ✅ W3：parity 1.9e-6，ORT CPU 0.48ms/步（TRT/int8 → Windows 机）|
| `eval/rollout/` | GSI 记分、协议状态机、可插拔 capture/policy/inject 环、遥测 | ✅ W4（fake 后端全测试；dxcam/SendInput 待实机验证）|

## R0：按数据现实重锚（research/07）

DataMiner 落地后数据现实变了，代码随之改造（设计见
[research/07-data-grounded-training-plan.md](../../model_research/research/07-data-grounded-training-plan.md)）：

- **动作头改语义动作**（`action_registry.py`）：BC 主目标是 mapping 投影的语义动作
  （COD 26 个可训练语义），camera.yaw/pitch 独立分箱头，keys 降为可选 aux。部署时经
  `MappingResolver` 逆映射回物理键注入。
- **物理辅助词表动态冻结**（`physical_vocab.py`）：从 `game-profile.input_contract` 取键盘
  VK/鼠标按钮，绑定 `mapping.json` 与 `game-profile.json` 原始 SHA-256。ingest 在任何转码前
  全量审计，未知输入、mapping id/version/hash 漂移都会拒绝；plane 与 checkpoint 保存同一
  `physical_input_vocab.json`/hash。当前 uint64 编码最多 64 个物理输入，超限拒绝而非截断。
- **μ-law 双公式**（`contract.py`）：`dm1-v2`（DataMiner 现行教科书式，存储真值）与
  `legacy`（data_demo 存量）并存；ingest 从 raw dx/dy 用指定 scheme 重算，杜绝两代刻度混入。
- **run 边界约束**（`dataset.py`）：采样窗口不跨 `run_frame_range`（失焦/暂停切断）。
- **COD 闭环红线写进代码**（`eval/rollout/windows.py`）：注入器每次 apply 前守卫前台进程，
  命中 COD 名单即拒绝——COD 有内核 Ricochet + TOS 禁自动化，闭环评估只在 CS2/无反作弊探针环境。
- **synth.py**：macOS 开发机上合成 DataMiner converter 格式数据（Windows-only 采集的替身），
  全链路可离线冒烟。

R0 冒烟（synth COD 数据，macOS）：ingest→train→eval→export→rollout 全通；ONNX parity
3.4e-7、ORT p99 0.93ms、rollout 60Hz total p50 1.78ms。

## 接线待用（R0b）：消费真实 DataMiner 产出

采集数据将至，管线已对准真实 converter 输出（[RUNBOOK.md](RUNBOOK.md) 是操作手册）：

- **深度无关发现**（`ingest.py`）：真实布局是 `ready/<session>/<game_slug>/<session>/cNNNN`
  （chunk 在 3 层之下）；ingest/preflight 用 `rglob` 发现 `cNNNN/meta.json`，指向任意层级都行。
- **身份来自 meta**（不靠路径位置）：game/session 从 chunk `meta.json` 字段读，plane 目录用其 slug。
- **`preflight`（新）**：秒级就绪自检（不转码）——发现/治理门/schema/μ-law 漂移/run_ranges/
  动作覆盖，阻断问题作非零退出码。**采到 sample 数据先跑这个**。
- **`--strict-bins`**：真实 dm1-v2 数据断言重算 bins==存储，捕获 DataMiner 公式再漂移。
- **治理计数**：ingest/preflight 报 trainable vs non_trainable，操作员一眼看到门状态。

真实嵌套布局全链路冒烟（synth 模拟）：preflight→ingest(strict)→train→export 全绿，
parity 3.5e-7；57 测试通过。

## Windows 实机待办（W3-real / W4-real）

TRT fp16/int8 引擎 + IOBinding、dxcam 采集校准、SendInput 注入验证、CS2 `-insecure`
本地服 + `configs/gamestate_integration_diogenes.cfg`、30min p99 <25ms 上线门——
checklist 见 `deploy/export.py` 与 `eval/rollout/windows.py` 模块注释。

## 快速开始

```bash
cd ml/player
uv venv --python 3.11 && uv pip install -e ".[dev]"

# 测试（契约黄金测试 + 端到端管线冒烟）
uv run pytest -q

# 从 data_demo 物化训练平面（三方数据，仅练管线）
uv run python -m diogenes_player.ingest \
  --src ../../data_demo/dataset --out /tmp/train_plane \
  --legacy-physical-vocab --scheme legacy

# 吞吐基准
uv run python -m diogenes_player.bench \
  --plane /tmp/train_plane --n 64 --t 96 --workers 4 --allow-untrainable
```

训练机（Linux/GPU）另装 `uv pip install -e ".[train]"` 启用 torchcodec 后端。

## 治理红线

- `is_trainable=false` 数据默认被 `WindowDataset` 拒绝；管线验证须显式 `allow_untrainable=True` 且产出权重不得发布。
- 伪标签（未来 IDM 扩标）必须带 `annotation.source:"idm"` 溯源标记。
- 禁止引入 Pearce CS BC 仓库代码与 DIAMOND csgo 权重（license 红线，见设计文档 §1）。
