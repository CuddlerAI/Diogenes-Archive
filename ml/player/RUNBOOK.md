# 采集数据接线 Runbook（DataMiner → 训练线）

面向操作员：拿到 DataMiner 客户端产出的 DataModel 数据后，如何验证并喂进训练线。
当前所有真实数据都是 **non-trainable**（grant/consent + certified catalog 落地前），
只能练管线、产出的权重即弃。

## 0. 数据在哪

DataMiner 客户端在标注员本机产出：

```
%LOCALAPPDATA%/com.diogeneslab.dataminer/non-trainable-data-model/ready/<session_id>/
    <game_slug>/<session_id>/
        cNNNN/{p0.mp4, p0.jsonl, meta.json}   ← 4s/120 帧 chunk
        metadata.json                          ← provenance.run_frame_ranges
        framepairs.jsonl
    metadata.jsonl  _dataset_stats.json  DATASHEET.md
```

把一个或多个 `ready/<session_id>/` 目录（或整个 `ready/`）同步到训练机任意路径即可。
下面的命令对**任意嵌套层级**都能工作（深度无关发现）。

## 1. 就绪自检（先跑这个，不转码、秒级）

```bash
python -m diogenes_player.preflight --src <ready-dir> --scheme dm1-v2 \
    --mapping <catalog-dir>/mapping.json \
    --game-profile <catalog-dir>/game-profile.json \
    --physical-vocab-name <game-or-corpus> --physical-vocab-version 1
```

它报告并把阻断问题作为非零退出码（可进 CI）：

- **发现**：sessions / chunks / frames，每个 game 的 session 数
- **治理门**：trainable vs non_trainable（真实数据现在恒为 non_trainable，属预期）
- **schema**：meta 必需字段、`p0.jsonl` 行数 == `meta.video.frames`、actions 列在
- **fps 一致性**：每 chunk 必须声明 `video.fps`；混合帧率=阻断（window/pointer/framepair
  全是帧量纲，DataMiner docs/50 P-4）。`--expect-fps 30` 可钉死期望帧率
- **μ-law**：抽样重算 bins vs 存储；不一致=契约漂移（DataMiner 换了公式），红字阻断
- **run_frame_ranges**：缺失会列出（采样不跨 run 依赖它；缺了整 session 当单一 range）
- **动作覆盖**：COD 26 语义里哪些采到过（support>0）、哪些从未出现——采集缺口提示
- **物理输入契约**：mapping id/version/content hash 必须与给定 catalog 完全一致；逐行统计
  未知 VK/鼠标按钮，任一未知值即阻断。预检报告只做低敏聚合计数，不把未知输入吞成全零。

`scheme` 默认 `dm1-v2`（DataMiner 现行公式）。若自检报大量 μ-law 漂移，先确认 DataMiner
的 `foveated_bin` 公式没变（见 `capture-core/src/projection.rs`），不要盲目改 scheme。

**pointerRawDx/Dy（dm-1 #137）**：帧行现含未截断的真实鼠标 Δ（`pointerDx/Dy` 是 clamp
±200 的）。ingest 把 `pointerRawDx/Dy` 存入 `raw_dx/raw_dy` 列，评估 camera R² 用它——
从而如实暴露快速甩枪（>200px）被 ±21 bin 截断的误差；bins 仍从 clamped 派生（契约一致）。
旧数据（无该字段）自动回退 clamped，无需特殊处理。

## 2. 物化训练平面（转码 + 索引）

```bash
python -m diogenes_player.ingest --src <ready-dir> --out <plane-dir> \
    --scheme dm1-v2 --strict-bins \
    --mapping <catalog-dir>/mapping.json \
    --game-profile <catalog-dir>/game-profile.json \
    --physical-vocab-name <game-or-corpus> --physical-vocab-version 1
```

- 产出低分辨率代理 mp4（默认 256×144、GOP=1s）+ `actions.parquet` + `index.parquet`
  + `run_ranges.json` + `_ingest_stats.json` + `physical_input_vocab.json`
- 转码前先全量审计物理输入和 mapping 身份；未知输入/哈希漂移时不创建训练平面。
- 已冻结词表可改用 `--physical-vocab <physical_input_vocab.json>` 重放；只有 data_demo
  存量路径可显式传 `--legacy-physical-vocab`，该模式会拒绝 dm-1/per-game mapping 数据。
- `--strict-bins`：真实 dm1-v2 数据应满足重算==存储，不满足即拒收该 chunk 并留痕
- 身份（game/session）从 meta 字段读，plane 目录用其 slug；重跑覆盖，幂等
- 分辨率消融：`--width 320 --height 180` 或更高；平面是可丢弃缓存，随时重物化

## 3. 冒烟训练 / 评估 / 导出（验证全链路，权重即弃）

```bash
# 训练（语义动作头；--allow-untrainable 因为现在数据全 non-trainable）
python -m diogenes_player.train.bc --plane <plane-dir> --out <run-dir> \
    --steps 300 --batch 8 --t 96 --burn-in 16 --allow-untrainable

# checkpoint 内嵌 physical_input_vocab + canonical hash；eval/export 会重新校验，
# 与 plane hash 不同或旧 checkpoint 没有该字段都会 fail-closed。

# 离线评估 + 回归门（首次不带 --gate 生成 baseline，之后带 --gate baseline.json）
python -m diogenes_player.eval.offline --plane <plane-dir> --ckpt <run-dir>/ckpt.pt \
    --n 64 --allow-untrainable --save baseline.json

# 导出单步 ONNX + ORT 数值对账
python -m diogenes_player.deploy.export --ckpt <run-dir>/ckpt.pt \
    --out <run-dir>/policy.onnx --height 144 --width 256 --check --bench
```

## 4. 何时数据变 trainable

需 DataMiner 侧打通：客户端凭据（Credential Manager/DPAPI）→ uploader → Forge →
真实 annotator/device/consent grant + certified catalog。届时 chunk meta 的
`is_trainable` 翻 true，`WindowDataset` 无需 `allow_untrainable` 即可消费，产出的权重
方可发布。在此之前一切按 non-trainable 对待。

## 5. 红线

- **评估闭环永不对 COD**：Ricochet 内核反作弊 + TOS 禁自动化。`SendInputInjector`
  已内建前台进程守卫，命中 COD 进程名即拒绝注入。闭环只在 CS2 `-insecure` / 无反作弊
  探针环境（research/07 §4.1）。
- 不引入 Pearce CS BC 代码与 DIAMOND csgo 权重（license 红线）。
