# capture-orchestrator · Megacity MC 批量采集编排器

docs/78 §5 的实现：把「trajgen 计划 → Unity 采集（McClipCaptureTest）→ engine_qc 终审
→（可选）R2 演练上传」编排成目录 marker 状态机，断点续跑天然成立。

```
PLANNED → CAPTURED → QC_PASS | QC_FAIL-<rule> → UPLOADED(--upload 显式开启)
```

- 每条 clip 一个目录 `clip-<seed>/`：`plan/`（trajgen 12 文件 json 面）、
  `capture/clip/`（Unity 产出的完整 clip）、`.state-*` marker、`unity-*.log`、
  `qc-report.json`。
- **看门狗**：Unity 单次 30min 超时 kill，重试 ≤2；连续 5 个种子同一 QC 规则失败
  → 停批（规则性缺陷不是坏运气）。
- **QC 失败纪律**：不修数据；`capture/clip` 改名 `clip.rejected-<rule>` 留档，
  seed+1 重录。
- **批级作废**：`void` 子命令给整批落 `.state-VOID` + `VOID.md`，`step_upload`
  见到即拒传，**优先于许可闸**。用在「数据本身没毛病，但因外部原因不得出仓」——
  典型是 provenance 身份指错场景且三字段自洽（闸按定义看不出来，见 docs/106）。
  同样不修数据，撤销就删 marker。
- **配额账本**：`ledger.json` 是 trajgen `QuotaTracker` 的计数快照（同一实现，
  不重写），跨批持久化——画幅/运镜类目缺口驱动后续计划的抽样权重。
- **许可闸**：upload 前拿 clip 自己写的 `provenance.license` 在
  [`../scenes/manifest.json`](../scenes/manifest.json) 的登记里反查处置——标记必须
  在白名单里，`scene_asset_id` 必须与标记同属一条登记，给了 `--scene-id` 时还要与
  本批声明的场景一致；任一条不满足即拒传。前缀由登记的处置决定：
  `*-internal-validation-only` 只进 `internal-validation/`（物理隔离），
  `*-deliverable*` 才进交付面根。
  **闸挡不住的一类**：provenance 三个字段一起指错场景时（身份自洽但整体是错的），
  闸看不出来——所以身份要靠 `--scene-id` 从登记注入，别靠调用方记得导环境变量
  （代价见 docs/106 的 warehouse-v1/v2）。

## 用法

```bash
cd engine-data
python3 capture-orchestrator/megacity_capture.py batch \
  --count 5 --seed-base 2000 \
  --scene-id megacity-metro \
  --runs-root ~/Documents/diogenes-capture-runs/batch-001 \
  --world-offset 20,160,0
# 单条调试 / 断点续跑 / 只重算报表
python3 capture-orchestrator/megacity_capture.py single --seed 2000 --runs-root <dir>
python3 capture-orchestrator/megacity_capture.py resume --runs-root <dir>
python3 capture-orchestrator/megacity_capture.py report --runs-root <dir>
# 整批作废（不删数据，只是从此拒传）
python3 capture-orchestrator/megacity_capture.py void --runs-root <dir> --reason "..."
# 目检取证包（联表 + 待人填的判定表；作废批次拒绝出包）
python3 capture-orchestrator/visual_audit.py --runs-root <dir> [--count 5]
# 车辆导演模式（docs/78 §10）：--mode director，各子命令通用。
# 计划走 `python -m trajgen direct-clip` 子进程，--ledger 跨条传同一账本
# ⇒ 画幅逐步配平 60/10/10/20（进程内每次新建 QuotaTracker 会恒出 16:9）；
# 采集恒等摆位 + DIOGENES_FFMPEG/CODEC/PRESET；QC 附 --init-exempt-lanes 4。
python capture-orchestrator/megacity_capture.py batch --mode director \
  --count 5 --seed-base 13 --runs-root <dir> \
  --scene-file capture-orchestrator/megacity-scene.json
```

> ⚠️ `report` 不是只读的：它按**当前命令行参数**重写 `batch-report.*`，会把
> `world_offset` 换成默认值、并丢掉逐条用时（marker 里没存这两样）。
> 要验证 CLI 就另开一个空 runs-root，别拿真实批次当靶子。

前置：
- **`--scene-id`**：`scenes/manifest.json` 里已核验的条目名
  （`python3 scenes/registry.py` 看当前可用集）。给了它，采集身份与许可
  （`scene_asset_id` / `license` / `gameProcessName` / 署名）全部由登记注入，
  环境里的同名变量被覆盖并记日志。不给则沿用历史行为（环境变量自带 +
  `internal-validation/` 前缀），只对已在跑的 Megacity 批保持兼容，新场景别这么用。
- Unity 6000.1.0f1（`DIOGENES_UNITY_BIN` 可覆盖）+ Megacity Metro 工程
  （`DIOGENES_UNITY_PROJECT`，默认 `~/Documents/megacity-metro`），工程内需嵌入
  `enginecapture` SDK 与 `Samples~/Megacity` 的测试脚本（见
  `../enginecapture/BRINGUP-MEGACITY.md`）。
- 世界偏移/朝向：默认由 Unity 侧自动吸附（最近路网段高度 + cam4 主导视轴对齐
  走廊方位）；`--world-offset` 只需给 XZ 邻域。

产出：`batch-report.md` / `batch-report.json`（成功率、逐条状态与 QC 首个失败规则、
配额账本快照）。

## 目检取证（`visual_audit.py`）

**engine_qc 全绿不构成可交付结论**——它是数值口径，测不到穿模、贴脸遮挡、空画面、
卡墙/闪屏/漂移、画面质感。这条产线已有两批 125 PASS / 0 FAIL 的数据在目检被打回。

`visual_audit.py` 出「联表 + 待人填的判定表」：每条 clip 每路一张 3×3 联表
（9 个时间点 `t_i = i×时长/8`，tile 宽 520px），抽样按画幅去重优先（窄画幅是结构性
弱项，全抽 16:9 等于没查）。AUDIT.md 里另有一段**数值线索**——拿 `mean_pixel_share`
与 `active_frac` 排「先看哪一路」，**不是判据**，结论以联表为准，且由人写。
