# halo-intake-qc — Halo 生产摄取的分布 QC

对 `raw/v1/non-trainable/dataminer-api-production/sessions/` 最近 N 天的 clip 抽样，
把 [116 号需求书](../../DataMiner/docs/116-wm-gameplay-capture-vendor-requirements/README.md)
§8.3 的分布硬阈值套在**自采数据**上机器验证，并加一条
[docs/111](../../DataMiner/docs/111-r2-corpus-player-model-usability-audit.md) §5 的
toggle-zoom 签名探测（`combat.aim` 帧占比 < 0.2 × `combat.primary_attack` 即报警——
0.01 的比值就是 92/111 号审计抓到的坏标签形态）。

**为什么需要它**：服务端 quality gate 只验工程健康位（帧对齐/上传完整性），不验分布。
`combat.aim` 废标签、28% 纯 W、18.9% 空帧都是带着绿灯入的库（research/44 §3.3）。
对外供应商按 116 §8.3 批验，自采数据没有理由豁免同一条线。

## 判据住在库里，`qc.py` 只是 IO 外壳

判据、阈值、两档口径全部实现在 **[`ml/diogenes_wm/halo_qc.py`](../../ml/diogenes_wm/halo_qc.py)**
（纯函数、无 IO、测试在 `ml/tests/test_halo_qc.py`）。`qc.py` 负责列对象、抽样、
下载、解压、打印；训练侧的 `halo_adapter.py` 用**同一个库**选片。这样"过闸"在
运维巡检与训练装配两边是同一个定义——research/50 §4.1 之前的问题正是判据只存在于
一次性脚本里，训练侧无法调用，于是没人能自动选出可用的那部分语料。

阈值口径、两档差别、`combat.aim` 的抽样敏感性、报告 schema，全部见该模块的 docstring。

## 用法

凭据配置同 [tools/r2-attested-reclass/README](../r2-attested-reclass/README.md)。

```bash
./qc.py --days 3                    # 最近 3 天，抽样 ≤240 clip
./qc.py --listing recent.txt        # 复用 rclone lsf --format tp 清单
./qc.py --days 3 --report qc.json   # 额外落一份逐 session 的 JSON 报告（供适配器选片）
./qc.py --days 3 --tier floor       # 换「行为地板」档判定
```

退出码 0=全过，1=有失败（可挂 cron / CI）。会话 <600s 的判定对时间窗首尾的
截断 session 会误报，人工豁免即可。

## 检查项

116 §8.3 全套：无输入帧 ≤15% · 指针非零 ≥30% · 纯 W <15% · 键覆盖中位 ≥8 ·
duplicate ≤2% · 动作熵 ≥0.75 · 六冻结通道整批各 ≥1% 且每会话齐全；
外加 toggle-zoom 签名（aim/attack 比值）。

`--tier` 选判定档（**默认 `strict` = 上面全 10 项，与本脚本的历史行为一致**）：

| 判据 | `strict` | `floor`（research/50「行为地板」） |
|---|---|---|
| 无输入帧 / 指针非零 / 纯 W / 键覆盖 / duplicate / `combat.primary_attack` | ✅ | ✅ 同阈值 |
| 动作熵 ≥0.75 | ✅ | ❌ 不参与判定 |
| `combat.aim` ≥1% · toggle-zoom 签名 | ✅ | ❌ 不参与判定 |
| 通道每会话齐全 | 冻结**六**通道 | **五**通道（摘掉 `combat.aim`） |

两档**阈值完全相同**，只差哪些判据参与总判定；关掉的判据仍照常计算并写进
`--report` 的 JSON（`enabled: false`），所以一份报告可以事后换档重判而不必重跑。

### 两条容易读错的边界

- **`--report` 是逐 session 判定，屏幕上那张表是批级判定。** 批级把所有 clip 的
  计数加在一起，好 session 会稀释坏 session——**批级过闸 ≠ 每个 session 都能用**。
  选片一律以 `--report` 的逐 session 结果为准。
- **过闸小时数随抽样规模漂。** research/50 §2.4 的 floor 45/122 session、8.960 h 是
  365 clip 抽样跑的；同一定义在全量 8,663 clip 上重跑是 57/122、11.48 h（若第十项
  仍按六通道判则 43/122、9.13 h）。引用数字时必须同时说清是哪份样本。

本脚本**不按 `meta.target.game_id` 过滤**，车道里混着 cod/w2/wuchang 串场语料
（research/50 §6 R1–R3），需要过滤请在调用侧做——`halo_qc` 只看 frames 行，看不到 meta。

## 告警与定时（`watch.py`）

`qc.py` 只把结果打到 stdout——没人跑就没人看。[research/50](../../model_research/research/50-r2-corpus-training-readiness-audit.md)
§5.1 实测的代价：prod 摄取从 2026-08-12 16:10:06Z 起**停摆 44.8 小时无人发现**，
而熵与纯 W 两项超线也从未惊动过任何人。`watch.py` 补的就是这一层：

```bash
./watch.py                 # 完整巡检（停摆 + 分布），有告警推飞书 Diogenes 群
./watch.py --stall-only    # 只查停摆，秒级，不下载 clip
./watch.py --dry-run       # 只打印将要发出的告警正文
./watch.py --listing tp.txt --stall-only   # 用离线 `lsf --format tp` 清单回放/取证
```

两类判据：

| 判据 | 阈值 | 依据 |
|---|---|---|
| **采集停摆** | 最后一次 clip 入库距今 > `--stall-hours`（默认 24h）→ warn；> `--critical-stall-hours`（默认 72h）→ critical | 采集是人工开会话的，夜间与单日空窗属正常；24h 无新增才是真异常。50 号那次实际空窗 44.8h，24h 阈值能在半天内抓到 |
| **分布超线** | `qc.py` 退出码非 0（116 §8.3 九项 + toggle-zoom 任一不过） | 判据逻辑不在 `watch.py` 里重复实现，只读 `qc.py` 的退出码与报告行 |

设计边界：`watch.py` **不含任何判据逻辑**，只做 freshness 探测 + 编排 + 通知；
`qc.py`（含后续抽库到 `ml/diogenes_wm/halo_qc.py` 的重构）保持
`0=全过 / 1=有失败 / 2=致命` 的退出码契约即可，`watch.py` 无需跟着改。
另有两条 fail-closed 细节：`qc.py` 非零退出但一条判据行都没打出来，判为「QC 自身
失效」而非「数据不合格」（缺 rclone/zstd、清单为空、未捕获异常都是这一类）；
freshness 探测自己抛异常时也会发通知——**监控静默失效正是 50 号那 44.8 小时的根因**。

飞书通知是**可选依赖**：凭据取 `PANGMAO_APP_ID`/`PANGMAO_APP_SECRET` 环境变量，
其次 `~/.claude/skills/feishu-notify/credentials.json`；两者都没有就降级为 stdout
打印，退出码不变（0=无告警，1=有告警，2=巡检自身失败）——告警不会因为通道不通被吞掉。

**定时**走 [`.github/workflows/halo-intake-watch.yml`](../../.github/workflows/halo-intake-watch.yml)：
每 3 小时 `--stall-only`，每天 02:30 UTC 跑完整巡检。上线前需人工把 GCP Secret
Manager（project `hakkolite`）的 `r2-training-*` 四件套镜像成仓库 secret
`R2_TRAINING_{ACCOUNT_ID,ACCESS_KEY_ID,SECRET_ACCESS_KEY,BUCKET}`（本仓库还没接
GCP Workload Identity Federation，Actions 里没有 gcloud 身份可用）；
`PANGMAO_APP_*` 与 `github-to-feishu.yml` 共用、已存在。secret 缺失时这条 workflow
**会红**，是刻意的——静默 no-op 的监控等于没有。

本机 launchd 是等价替代（在 CI 拿不到 R2 凭据时用，凭据按上面 r2-attested-reclass
的写法导出后再起）：

```xml
<!-- ~/Library/LaunchAgents/ai.diogenes.halo-intake-watch.plist，每 3h 一次 -->
<key>ProgramArguments</key>
<array><string>/bin/zsh</string><string>-lc</string>
  <string>tools/halo-intake-qc/watch.py --stall-only --state-file ~/.cache/halo-watch.json</string>
</array>
<key>StartInterval</key><integer>10800</integer>
```

`--state-file` 会做重复告警抑制（同一告警 `--repeat-hours`，默认 12h 内只通知一次）。
GitHub Actions 的 runner 没有持久盘，状态由 workflow 用 `actions/cache` 跨 run 带
（`halo-intake-watch.yml` 的 Restore/Save alert state 两步；save 必须 `if: always()`，
因为有告警的那一轮 job 是红的）。

> **`ts` 记的是「上次真的发出去」，不是「上次跑过」。** 早先的实现每轮都无条件重写它，
> 3h 一轮的巡检让 `now - ts` 恒为 3h < 12h，于是首次通知之后**永远静默**——一条持续
> 几十小时的停摆只叫一次，而「安静」和「已恢复」在群里长得一模一样。通知失败也不写，
> 免得通道故障白占一个抑制窗。

### ⚠️ 跑这个脚本必须在 UTC 下取时间戳

`rclone lsf --format t` 打的是**运行机器本地时区的墙钟，且不带偏移量**（实测 v1.74.4：
同一个文件 `TZ=UTC` 打 `2026-08-23 06:42:48`、`TZ=Asia/Shanghai` 打 `2026-08-23 14:42:48`）。
解析侧对无偏移量的时间戳一律按 UTC 解释，所以 `rclone_lsf()` 把子进程 `TZ` 钉成 `UTC`——
**这是判据的一部分，不是环境洁癖**：不钉的话停摆时长会整体偏移一个时区差，在 UTC+8 上
一次 25h 的真实停摆读成 17h，低于 24h 阈值，**告警不会触发**。

2026-08-23 实测过这个偏差：同一份数据，GitHub Actions（TZ=UTC）算 34.2h、本机
（CST+0800）算 27.2h，差值恰是 8h。用 `--listing` 喂离线清单时同理，生成那份清单也得
`TZ=UTC rclone …`（或 `--time-format RFC3339` 带上偏移量，需 rclone ≥1.66）。

回归测试在 `tests/`，跑在 `halo-intake-qc` 这条 CI 上：

```bash
python -m pytest tools/halo-intake-qc/tests -q
```

注意：`view_scale` 标定缺失**不在**本脚本检查范围——那是 catalog/H-1 层面的闸
（`clip_trainability` 的 `view_scale_unverified`），不是分布问题。
