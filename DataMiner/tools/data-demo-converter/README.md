# DataMiner RAW → data_demo 转换工具

这个 crate 同时提供共享 Rust assembler library 和独立的 `dataminer-data-demo` 离线 CLI。Tauri 客户端静态
链接 library，但不会启动 CLI 子进程；客户端入口由 SQLite journal 选择已确认的 RAW ready segment，CLI 则由
调用方显式选择 session 目录并完整校验 RAW/run 闭合性。两种入口都不会启动窗口采集、编码器或第二套帧投影。

## DataModel 契约版本

- RAW 采集契约继续使用 `contract.version = "dm-1"`；这与派生 DataModel 的 envelope 版本是两个独立版本轴。
- 新转换固定写出 `SessionMetadata.schema_version = "dm-2"` 与 `ChunkMeta.schema_version = "dm-2"`。
  `dm-2` 新增顶层 `catalog` 键，用于承载 RAW catalog provenance；旧 RAW 没有该事实时键仍必须存在且值为
  `null`，不能通过省略键掩盖未知来源。
- 独立 verifier 继续读取严格的 legacy `dm-1` DataModel：其 metadata/chunk 必须缺少顶层 `catalog`。
  `dm-1` 携带该键、`dm-2` 缺少该键、session/chunk 版本不一致以及未知版本都会被拒绝。
- finalize/recovery 只在逐文件与当前 RAW canonical 输出一致时，把 legacy `dm-1` metadata 原子升级为
  `dm-2`；任何其它字段或媒体字节漂移仍然失败，不会被迁移吞掉。

客户端调用 `advance_incremental` 时只发布完整 4 秒 chunk；`finalize_incremental` 在 session Completed 后
校验最终 run.json/ref 集合，补唯一尾块和完整 metadata。两者都支持从 RAW 重放、校验已有 chunk 和清理自有
staging；FramePair 通过 `run_frame_ranges` 保证不跨 pause/resume run。

录制期 work 目录使用原子 `.incremental-state.json` 保存已验证 refs、投影检查点和不足 4 秒 tail 的 RAW
sample 位置。正常 `advance_incremental` 只验证新增 segment 并读取新增帧/tail；cursor 缺失、损坏或与 chunk
数量不闭合时才全量重放恢复。最终化会对当前完整 RAW 再做一次闭环校验，并在发布 metadata 完成标志前移除
cursor，因此内部恢复状态不会进入 ready dataset。

## 当前支持范围

- 输入支持既有 `dm-local-dev-session-1/2/3`，以及签名下发目录产生的中性
  `dm-session-ingest-2` ready session；不再维护 game/profile/mapping 的独立来源白名单。
  两代都会按各自 schema 严格校验训练字段的在场性，中性 v2 还会校验冻结 catalog
  snapshot，调用方仍需从 SQLite journal 选择已知 ready session，本工具不会仅凭散落目录倒推
  journal-ready 事实。
- 接受冻结 capture profile 中的原生分辨率、分数帧率和 4 秒 GOP（`round(fps * 4s)`）；不缩放、
  不重采样、不解码、不重新编码，也不调用 FFmpeg/ffprobe。MP4 H.264 sample 原样重封装，输出
  metadata 同时保留显示 fps 与精确 `fps_num`/`fps_den`。
- 转换前逐文件重算 `video.mp4`、`frames.jsonl.zst`、`events.jsonl.zst`、`meta.json`
  的 SHA-256，并对照 `sha256.json` 和 meta；同时校验 session/run/segment、profile、mapping
  id/version/content hash、实际 encoder 参数、JSONL 行数/QPC/帧数、MP4 sample 数和 non-trainable 边界。
- 对带目录的会话逐字节读取并重算全部 catalog snapshot 的 SHA-256，同时核对 catalog
  manifest、`session.json` 和每段 `meta.json` 中的条目 identity；任一文件或 identity 被篡改
  都会拒绝转换。世代由 session.json catalog 段的形状裁决（`recording` 单册 = docs/57 断代后，
  `capture`+`settings` 两册 = bundle-1 遗留代，混装即拒）：断代后走 catalog/device/recording/
  game/mapping 五份 snapshot；bundle-1 遗留代（issue #811 路线 A）走 catalog/device/capture/
  settings/game/mapping 六份，mapping 按 `dm-mapping-1` 读入并在内存规范化（哈希仍取原字节），
  且只对 local-debug 与 attested-trainable 两档开放——first-party-release 维持 v2+ 断代。
- `events.jsonl.zst` 与 `mapping.json` 会交给 `capture-core::FrameProjector` 重新投影，并与
  `frames.jsonl.zst` 逐字段对拍；转换器只重排已经验证的投影结果，不复制投影算法。
- 输出 `p0.jsonl` 保留每帧物理 `keys/events/pointer` 和冻结 mapping 已派生的 `actions`；
  `framepairs.jsonl` 的 `actionT` 同步携带 `actions`。独立 `verify` 要求动作名称合法、排序且去重，
  并从 p0 重新闭合 FramePair；旧的本地 non-trainable 输出缺少该加法字段时仍可读，但不会凭空补标签。
- 视频仅用纯 Rust MP4 reader/writer 搬运原始 H.264 sample。每个按 profile GOP 切出的原生
  4 秒输出边界必须同时是 MP4 sync sample 和实际 H.264 IDR；不满足时拒绝转换，不丢帧、不复制
  整段冒充成功。
- 非尾 chunk 固定为 profile GOP 帧数；尾 chunk 可不足一个 GOP，与
  `data_demo/make_dataset.py` 的 `g >= n_total` 尾块行为一致。
- 输出沿用 data_demo 核心目录/字段，并以 `dm-2` envelope 追加 RAW provenance、顶层 `catalog` 与合规字段；不宣称与旧 demo 的
  统计扩展逐字段相同。合规印章由 `ComplianceProfile` 决定（见下），并拒绝覆盖已经存在的目标目录。

## 合规档位（ComplianceProfile）

CLI 与 library 支持三个互斥档位，输入门与输出印章都由同一个档位派生，不存在混合状态：

| | `local-debug`（默认） | `first-party-release` | `attested-trainable` |
|---|---|---|---|
| RAW 输入要求 | legacy：session/run/segment `non_trainable=true`，segment `state=non_trainable_ready`，run `state=non_trainable_complete`，catalog `dev_non_trainable`；中性 v2：三个层级都不带训练字段，segment `state=ready`、run `state=complete`、catalog 不带 mode | 全链 `non_trainable=false`，segment `state=consented_ready` 且 `consent_id` 与 CLI 一致，run `state=consented_complete`，v2 catalog `first_party_release`，session `consent_id` 一致 | 具结车道的**如实** RAW（docs/141 §3 路线 B）：legacy 保持 `non_trainable=true`；中性 v2 不携带训练字段；`consent_id=null`，segment `state ∈ {ready, non_trainable_ready}`，run 按各自世代取 `complete/non_trainable_complete`；完整性校验（六份 snapshot 逐字节 SHA-256、sha256.json 对账、segment 连续性、schema 白名单）与另两档完全一致 |
| 输出印章 | `rights=local_debug_non_trainable`、`is_trainable=false`、`production_certified=false`、`consent_id=null` | `rights=first_party`、`first_party=true`、`is_trainable=true`、`production_certified=true`、`consent_id=CNS-*`、`annotation.source=human_recorded`、framepairs `consentId` 同步携带 | `rights=first_party_attested`（刻意区别于 `first_party`，下游据此分辨证据强度）、`first_party=true`、`is_trainable=true`、`production_certified=false`（恒 false，具结≠certified）、`consent_id=null`、`annotation.source=human_recorded`、`tags=["attested_trainable","first_party"]`、`attestation.waiver_ref` 写进 chunk meta / session metadata / `_dataset_stats` 三处；DATASHEET 追加具结车道三点披露（车道属性、`combat.aim` 坏标签须 mask、view_scale 未标定） |
| verify | `verify --dataset <dir>` | `verify --dataset <dir> --profile first-party-release --consent-id <CNS-*>`（强制操作者对照 consent 登记） | `verify --dataset <dir> --profile attested-trainable --waiver-ref <ref>`（强制操作者对照 WAIVER 登记） |

`--profile first-party-release` 必须显式给出 `--consent-id`；`--profile attested-trainable` 必须显式
给出 `--waiver-ref`（如 `r2://diogenes-model-training/raw/v1/attested-trainable/WAIVER.json@2026-08-10`），
且**禁止** `--consent-id`——具结车道的 consent 由 WAIVER 承载，不由采集时点 consent id 承载。三档 RAW
`production_certified` 均要求为 false（RAW 层不做认证宣称，认证是转换产物的印章）。attested-trainable
不改写 RAW 头、不合成 consent 形状：它的意义就是如实接受 non-trainable RAW、由 WAIVER 引用承载资格。
录制客户端的增量路径（`advance_incremental`/`finalize_incremental`）固定为 local-debug 档，发布转换
只走离线 CLI。

这只是客户端内的本地格式桥接与配套契约验收工具，不是完整 Forge：离线
规范化、选片、数据集版本、云端发布、GCS/Hugging Face 同步均未实现；`first-party-release`
档要求的 consented RAW（`consented_ready`/`consented_complete`/catalog `first_party_release`）
需要 capture-lab 侧 `--consent-id` 支持落地（见 docs/12 §3.2 条目 3，后续 PR）。

## Library API

- `advance_incremental(session_dir, work_output, refs)`：只消费显式 `ReadySegmentRef`，发布当前可闭合的完整块。
- `finalize_incremental(session_dir, work_output, refs)`：要求 refs 与最终 run.json 完全闭合，发布尾块与完整索引。
- `verify_dataset(dataset)`：独立读回验证最终 dataset。

客户端默认把派生数据放在 RAW 树之外：

```text
%LOCALAPPDATA%/com.diogeneslab.dataminer/non-trainable-data-model/
├── work/<session_id>/
└── ready/<session_id>/
```

## CLI

在 `DataMiner` 目录运行：

```powershell
cargo run -p data-demo-converter --bin dataminer-data-demo -- `
  convert `
  --session "$env:LOCALAPPDATA\com.diogeneslab.dataminer\non-trainable-sessions\<session_id>" `
  --output "D:\temp\dataminer-data-demo"
```

输出是一个新的单 session data_demo dataset root：

```text
<output>/
├── metadata.jsonl
├── _dataset_stats.json
├── DATASHEET.md
└── <game_slug>/<session_id>/
    ├── metadata.json
    ├── framepairs.jsonl
    └── cNNNN/{p0.mp4,p0.jsonl,meta.json}
```

独立重放校验：

```powershell
cargo run -p data-demo-converter --bin dataminer-data-demo -- `
  verify --dataset "D:\temp\dataminer-data-demo"
```

查看源 MP4 的 sample/IDR 分布：

```powershell
cargo run -p data-demo-converter --bin dataminer-data-demo -- `
  inspect-mp4 --path "<ready-segment>\video.mp4"
```

仓库现有 `data_demo/` 没有单独的 validator；本工具的 `verify` 是独立读回校验。
`tests/golden/data-demo-summary.json` 锁定 metadata/chunk/stats 顶层字段、代表性 frame/framepair 及其
`actions`、4 秒 chunk、连续 `gframe` 和跨 RAW segment provenance；测试还逐 sample 对拍源/输出 H.264 payload，
确认只重封装、不改码流。

## 测试

```powershell
cargo test -p data-demo-converter
cargo clippy -p data-demo-converter --all-targets -- -D warnings
```
