# depth-lab · 离线核心

[77 号方案](../../docs/77-depth-ground-truth-hook-experiment-plan.md) 的**与平台无关**的那一半。

## 为什么先做这一半

整个 depth-lab 分两部分：

| 部分 | 平台 | 状态 |
|---|---|---|
| probe：注入 DLL、D3D hook、readback、水印写入 | **Windows only** | 独立 `dm-depth-probe` crate；实验路径已完成真机验证 |
| **离线核心：线性化、变体派生、水印编解码、逐帧配对** | **任意平台** | **本 crate** |

这样切分不是洁癖。本 crate 覆盖的恰好是[风险登记](../../docs/77-depth-ground-truth-hook-experiment-plan.md)里最贵的三条：

- **R-4 配对静默错位** —— CFR pacer 的 `duplicate` / `source_superseded` 导致
  「Present 次数 ≠ 输出帧数」，处理不当会在游戏帧率波动处静默错位，
  而错位后的 GT-vs-MDE 差异会被误读成「MDE 不准」。这是整个实验最隐蔽的伪结论通道。
- **R-7 fp16 装不下 1cm** —— 68 号规格「float16」与「`trunc(depth×100)/100` 约 1cm 精度」
  两条要求的交集边界。这个数要写进给需求方的澄清里，必须可复算。
- **R-23 Variant S 再生确定性** —— 体量旋钮 1 的前提：不常驻存 Variant S、按配方再生，
  要求同一配方两次生成逐位一致。

**三条全是纯计算，可以在碰 Windows 之前就用测试锁死。** Windows 侧于是只剩
「把像素搬出来」一件事，失败时归因也就清楚了。

## 模块

| 模块 | 内容 | 对应文档 |
|---|---|---|
| `half` | fp16 ↔ f32 转换、fp16 网格间隔分析、1cm 分辨率界限求解 | §5.5.6 |
| `depth` | `d = a + b/z` 线性化、Variant S / R 派生、格式损失统计 | §5.5 / §9.3E |
| `calibration` | `metric_scale` 的可辨识性、锚点标定（路②）、引擎惯例（路①）与来源强制记账 | §5.5.2 |
| `map_bounds` | cell 网格索引 → 68 号 §6.1 的四个角点；落域检查 | [82 号 §6.1](../../docs/82-depth-map-bounds-public-data-survey.md) |
| `watermark` | `present_seq` 帧序水印的编解码与绕回展开 | §5.6 |
| `pairing` | 逐帧配对表与七条不可违反的不变式 | §5.7 |
| `deliver` | 用已验证配对表生成 `depth.zip` / `action_camera.json`，并执行投影、编码与帧序门禁 | 68 号 §6.3 / §6.5 |
| `ets2` | ETS2：SCS 遥测流按 QPC 对齐配对表 → 位姿旁车（`--features ets2`） | [103 号](../../docs/103-ets2-pr405-delivery-runbook.md) |

## 几个刻意的设计选择

**`half` 自己写而不引依赖。** 只需要转换与间隔分析两件事，都是几十行位操作，
且必须能逐位断言——「16 m」这个数要写进对外澄清，不能建立在一个没读过源码的 crate 上。

**`watermark` 不碰任何图形 API。** 编码侧产出矩形列表（probe 在 Windows 侧交给
`ID3D11DeviceContext1::ClearView`），解码侧吃每块的灰度均值。因此可以在任意平台
把 16 位序号空间**全跑一遍**验证位序，而不需要一张显卡。

**`calibration` 里 `metric_scale` 不是裸 `f64`。** 这个量**无法由渲染观测单独定出**——
`d = A + B/z` 里的 `z` 是游戏单位，投影矩阵给的 `A`、`B` 也是游戏单位，
渲染管线全程不知道米。能解的只有复合量 `B·metric_scale`
（测试 `b_and_metric_scale_are_not_separately_identifiable` 把这条钉死）。
所以要单独报出 `metric_scale`，必须外接一个米制锚点或引擎惯例值，
而**每个交付出去的比例值都必须能指回它的来源**：`MetricScaleCalibration`
强制带来源与证据路径，路①（惯例常数）单用时还必须有一次锚点交叉验证，
否则 profile 校验直接拒绝。这与 DSV 选择「没有证据不许上线」是同一条纪律。

**`pairing` 算法刻意保持笨。** 按 `present_seq` 精确查表，查不到就是 `None`。
代码里**根本不存在**「就近取一个」的分支——不变式 2（不插值、不邻帧顶替）与
不变式 6（superseded 不得配给邻近帧）是靠「那条路径不存在」保证的，
而不是靠一个可以被调参放宽的容差。

## 正式交付

正式交付必须同时提供 probe 采集目录和逐帧配对表：

```bash
cargo run -p depth-lab --features exr-out -- deliver \
  --capture <depth-capture-dir> \
  --pairs <frame-pairs.jsonl> \
  --plan <clip-plan.json> \
  --out <delivery-root> \
  --clip-id <clip-id> \
  --video <video.mp4> \
  --pose <pose.jsonl>
```

`present_seq` 是 probe 的 Present 时钟，只用于找到源 depth；`session_frame` 是 RGB 视频
时钟，决定 `depth_<session_frame>` 和 `action_camera.json.frame`。时间采样导致的 Present
奇偶相位变化本身不构成失败，但配对表缺行、偶数视频帧缺 depth、重复引用、继承帧、
相机模式混合或 DSV 不一致都会在创建交付目录之前失败。

线性化使用的 `(A,B,metric_scale)` 及来源同样只读 `depth-index.jsonl` 的逐帧记录。
`deliver` 不接受 `--near` / `--far` / `--metric-scale` 覆盖，防止生成结构正确但米制错误的 EXR。

跨 recorder segment 的输入可以直接把 `segments/` 目录交给 `pair` / `clip-plan`；工具会按
路径排序读取 `frames.jsonl.zst`。`video-concat --segments <segments/> --plan <clip-plan.json>`
用纯 Rust MP4 reader/writer 搬运选中 H.264 sample、重建连续时间轴，不解码或重编码。
`deliver --plan` 同步重编号配对与 pose；内建视频门校验单轨、1920×1080、30 fps、帧数、
连续 sample 时间轴和无 B-frame 重排，不依赖 FFmpeg/ffprobe。

最终 `action_camera` 位姿行由已验证的相机、玩家、输入与配对索引组装；严格 depth
交付使用 `assemble-action`，30 FPS 全帧动作交付使用 `assemble-action-30`：

```bash
cargo run -p depth-lab -- assemble-action \
  --depth <depth-index.jsonl> \
  --camera <camera-pose-index.jsonl> \
  --player <player-pose-index.jsonl> \
  --actions <actions.jsonl.zst> \
  --pairs <frame-pairs.jsonl> \
  --wallclock <frame-wallclock.jsonl> \
  --mouse <mouse-normalization.json> \
  --qpc-frequency <hz> \
  --out <final-pose.jsonl>
```

Fallout 4 的 30 FPS final-pose 先从会话快照、通过的 `dm-input-calibration-2` 和 X/Y 等比例
的 `Fallout4.ini` 生成可追溯动作上下文，再允许组装；标定、game identity、轴比例或 r0000
起点任一不符都会拒绝产出：

```bash
cargo run -p depth-lab --features client-orchestration -- prepare-fallout4-action-context \
  --frames <segments/> --session <session.json> \
  --calibration <input-calibration.json> --axis-ini <Fallout4.ini> \
  --wallclock-out <frame-wallclock.jsonl> --mouse-out <mouse-normalization.json>
```

组装器要求所有输入共享同一 run identity，并拒绝缺帧、重复帧、QPC 越界、非法四元数
或缺失鼠标标定；它不会用最后一个有效值填补位姿。

## ETS2 交付桥（`--features ets2`）

ETS2 的 `player_*` 与 `camera_*` 来自官方 SCS Telemetry SDK，而不是 GPU hook。
两条流分属两个进程，唯一的公共键是 QPC，所以要先把遥测对齐到配对表：

```bash
cargo run -p depth-lab --features ets2 -- ets2-pose \
  --telemetry <dm-ets2-telemetry.jsonl> \
  --pairs <frame-pairs.jsonl> \
  --config <ets2-pose-config.json> \
  --out <pose.jsonl> \
  --out-index <pose.index.jsonl> \
  --report <ets2-pose-report.json>
```

`--out` 是全速率（每个视频帧一行，喂 `deliver --pose30`），`--out-index` 按
`present_seq` 去重（喂 `deliver --pose`）。**两份不是冗余**：CFR pacer 的 duplicate
帧合法地共用一个 Present，而 15 fps 那一侧的读取器在重复键上直接报错。

对齐器**不插值、不外推、不让邻近样本替暂停帧补位**；超出 `max_skew_us`（默认半个
30 fps 帧）的视频帧进报告的 `rejected` 列表，带原因与实测 skew。报告里的
`skew_p50/p95/max` 必须随交付带出——它是这条链相对 Fallout 4 多出来的不确定度。

地图边界（68 号 §6.1 的四个角点）由解包出的扇区文件名算：

```bash
cargo run -p depth-lab --features ets2 -- ets2-map-bounds \
  --sectors <每行一个 sec*.base 文件名的 txt> --out <map-bounds.json>
```

完整流程与采集前自检见 [`tools/pr405-ets2/README.md`](../pr405-ets2/README.md)
与 [103 号](../../docs/103-ets2-pr405-delivery-runbook.md)。

## 跑测试

```bash
cargo test -p depth-lab
```

macOS / Linux / Windows 均可，不需要显卡、不需要 Windows SDK。

这条跑的是**默认特性**。EXR 交付层（`materialize` / `deliver`）与 ETS2 交付桥的测试
挂在非默认特性上，默认特性下压根不编译，所以改这两层时还要跑：

```bash
cargo test -p depth-lab --features exr-out,ets2
```

两条都是 CI 里的门禁。**只跑一条不算验过**——默认特性曾经因为一个调用点没跟着
函数定义一起加 `#[cfg(feature = "exr-out")]` 而在 main 上编不过，而当时 CI 的两条
命令都恒定带特性，谁也没看见。

## 尚未包含

- ETS2 SDK telemetry 到逐帧 `pose.jsonl` 的 Rust 原生桥接；
- 各游戏运行时 profile 的最终接线与真机验收。
