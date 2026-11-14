# spec-export — dm-1 → 动作世界模型 spec-v1 离线导出转换器

把一个 dm-1 ready 会话目录投影为数据侧《动作世界模型 · 录制客户端采集需求与
数据格式 spec (v1, 2026-07-24)》要求的会话目录。定位与设计裁决见
[docs/17-action-world-model-spec-alignment.md](../../docs/17-action-world-model-spec-alignment.md)：
RAW 产物保持超集不动，所有 spec 兼容差异收敛到这个可重跑的离线投影层。

## 用法

```sh
cargo run -p spec-export -- convert \
  --session <dm-1 ready 会话目录> \
  --output <spec-v1 输出目录（必须为空或不存在）>
```

输出目录内容：

| 文件 | 来源 |
|---|---|
| `actions.jsonl` | 各段 `events.jsonl.zst` 解压后按 `(qpc, seq)` 全局排序，逐事件翻译 |
| `frames.jsonl` | 各段 `frames.jsonl.zst` 拼接投影（`frame_idx` = `session_frame`） |
| `heartbeat.jsonl` | 会话根 `heartbeat.jsonl`（RAW 存 `qpc`）投影；旧会话没有则跳过并警告 |
| `meta.json` | 从 `session.json` 组装 spec 字段 |
| `manifest.json` | 计数账，`counts` 用实际写出的行数 |

段布局同时支持目录式（当前客户端实际产物 `segments/00000/events.jsonl.zst`）
与平铺式（docs/02、docs/08 的 `segments/00000.events.jsonl.zst` 命名）。

## v0 范围（克制声明）

- **视频不处理**：不拼接分段 MP4、不转码。“分段 vs 单文件”和 CFR
  `duplicate` 帧判读是对齐会阻塞项（docs/17 §4），在拿到数据侧书面结论前，
  任何视频侧处理都可能返工，v0 明确不做。
- 不复刻数据侧八项验收探针。出厂只做 smoke 预检：三个 JSONL 行数与
  `manifest.counts` 对账、各流 `t_mono_ns` 单调不减、`frames.jsonl` 的
  `frame_idx` 严格递增；任一失败即非零退出码，不产出“看起来能交”的废品。
- 不重算段 hash、不校验 MP4：那是采集端 Finalizer 与 data-demo-converter
  的职责，本工具假定输入是 ready 会话。

## QPC → 纳秒换算（单实现纪律）

`t_mono_ns = qpc × 1e9 / qpc_frequency`，`i128` 中间值，不假定
`qpc_frequency` 为 10MHz（10MHz 下恰为精确 ×100）；`qpc_frequency` 从
`run.json` 读取，多 run 会话要求同一频率。实现只有一份：`src/qpc.rs`，
帧/动作/心跳三条流共用（docs/17 §5 裁决四：禁止第二个换算实现）。

## 扩展字段口径（spec 超集，接收端可忽略）

- `actions.jsonl`：`vk`（原始虚拟键码，保证键名可逆）、`control: true`
  （F8/F10 等控制热键，训练导出应排除）、`absolute: true`（绝对坐标设备）、
  `horizontal: true`（横向滚轮）。
- `frames.jsonl`：`duplicate`（CFR 补帧事实，阻塞项 ① 判读依据）、
  `source_t_mono_ns`（画面实际采集时刻 `source_qpc` 的换算值）。
- `heartbeat.jsonl`：`collector` 直通 RAW `state`
  （`recording`/`paused`/`degraded`/`stopping`，超集取值口径为对齐会
  阻塞项 ②）。
- `meta.json`：`recorded_keys`（契约键声明，取自 `mapping.json` bindings 的
  物理描述符：`keyboard:scan17` / `mouse:button1` / `mouse:delta_x`；
  mapping 不可得时留空数组并警告）、`spec_schema_version`（本次导出依据的
  数据侧 spec 版本）、`source_contract: "dm-1"`、
  `target_fps_num` / `target_fps_den`（采集帧率有理数真值）。
- `manifest.json`：`params.fps_num` / `params.fps_den`（同上）。spec 的
  `params.fps` 是整数近似——`60000/1001` 四舍五入成 `60`，单看它无法回溯
  出 NTSC 帧率，也就分不清 59.94 与 60.00 的会话。有理数真值只在这两个
  扩展字段里（docs/50 P-5c）。

键名表：字母/数字用字符本身（`W`），常用键用固定名（`SPACE`、`LSHIFT`、
`F8`…），表外 VK 统一 `VK_0xNN`，全会话一致（见 `src/vk.rs`）。

## Schema 追踪

数据侧字段单源是其仓库的 `action_world_model/schema.py`，会无通知演进
（docs/17 §5 裁决五）。后续必须落实三件套：

1. pin 数据侧 schema 版本并 diff 报警；
2. 用数据侧 `synth.py` 实际产物做黄金样例跑契约测试；
3. 导出物回写 `spec_schema_version`（已做，当前锚点 `v1-20260724`）。

v0 的样例基准暂时取 spec 文档中由 `synth.py` 生成后原样摘录的 demo 样例
（见 `tests/e2e.rs`）；拿到对方仓库访问权后应替换为真实 `synth.py` 产物。

## 测试

```sh
cargo test -p spec-export
```

平台无关（不依赖 windows crate），macOS 可直接跑；workspace 其他 crate
在非 Windows 平台编不过属已知情况，请用 `-p spec-export` 定向。
