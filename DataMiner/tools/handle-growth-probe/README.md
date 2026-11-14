# handle-growth-probe

定位 Capture 24 里"采集进程句柄每帧涨一个"的诊断工具。**不需要 Fallout4、不需要 depth
探针、不需要操作者碰游戏**：它自己开一个 1920×1080 窗口自渲染，然后用生产同一条
WGC → BGRA 环 → NV12 → NVIDIA MF 硬编链路去采自己这个窗口。

主程序只采自己创建的窗口，不采桌面、不采显示器、不写录制 session/run/manifest，
输出为诊断 JSON；`profile-preflight` 辅助入口另会生成合成 MP4 用于硬件检查，所有输出
均为本地测试材料，不可训练。和 `tools/issue750-wgc-probe` 一样是独立
workspace，不进客户端构建、不进 release surface、不进 `cargo check --workspace`。

## 为什么这么设计

Capture 24 只记了句柄**总数**，记不了类型，而且当时的进程已经退出，句柄表再也拿不回来。
这个工具把缺的两件事一次补齐：

1. **哪一段泄漏**——三个累进阶段各跑一段，看哪一段的斜率先冲到帧率：
   - `wgc`：帧到达 → `CopyResource` 进 BGRA 环 → D3D11 event query → 回收。
   - `nv12`：再加 `BgraToNv12`（和 `record_inner` 一样先转进 canonical 槽再
     `CopyResource` 到输出槽）。
   - `encode`：再加 `HardwareH264Encoder::write_texture` 与 tracked-sample
     release / encoded 两个 drain。
2. **漏的是什么对象**——每个采样点除了 `GetProcessHandleCount`（就是 PowerShell
   资源采样器记的那个数），还做一次按内核对象类型的直方图。阶段结束时给出
   `settle → end` 的类型差值，直接指名是 Event / File / Section / Thread / ...

只查 `ObjectTypeInformation`，**不查** `ObjectNameInformation`：后者在带挂起 I/O 的
同步管道句柄上可能永久阻塞，而且对象名会把本地路径写进证据文件。

## 跑法

Windows MSVC 构建现在会和客户端一样导出 `NvOptimusEnablement=1`，用于双显卡机器的 NVIDIA MFT 启动选择。旧的无导出实验与新构建必须分别留存二进制哈希；比较句柄曲线时同时核对 `encoder_finished.effective_encoder`，不能只凭适配器名称判断实际编码器。此工具按到达帧处理，不执行常规录制的 CFR/分段验收。

先编译（不需要游戏、不需要显示器空着，几分钟）：

```bash
cargo check --locked --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml --all-targets
```

代码检查和现有量具单测可独立执行，不会启动合成窗口、游戏或编码测试：

```bash
cargo fmt --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml --all-targets
```

这些命令需要单独运行，父 workspace 的检查不包含本工具。默认构建目录 `target/` 和建议
使用的本地输出目录 `out/` 已忽略；实际诊断 JSON、MP4、设备清单和二进制不应提交。

再跑（默认三阶段各 60 秒，加上启动开销总共约 4 分钟，跑完自动退出）：

```bash
cargo run --release --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml -- --output <一个全新的目录>
```

`--output` 必须指向**新的空目录**，工具会拒绝写进已有内容的目录，历史证据不会被覆盖。

只检查 Fallout4 的视频编码参数与一帧 MP4 输出，可以运行 `cargo run --release --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml --bin profile-preflight -- <一个不存在的新目录>`。该辅助入口调用现有 `probe_hardware_path`，不抓游戏、不修改 profile；它通过也不代表游戏分段或 5 分钟验收通过。`SHA256SUMS.txt` 保留原始交接压缩包的校验记录，不是当前修改后源码的清单。

期间会有一个 1920×1080 的无边框窗口出现在左上角。创建时不设置 WS_VISIBLE，并设置
`WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`，之后用 `SW_SHOWNA` 显示。被别的
窗口挡住也不影响采集；不要最小化它。

可选参数：

| 参数 | 默认 | 说明 |
|---|---|---|
| `--stages wgc,nv12,encode` | 全部 | 只跑其中几段 |
| `--stage-seconds 60` | 60 | 每段时长 |
| `--settle-seconds 8` | 8 | 每段前多少秒不计入斜率（避开启动那一跳） |
| `--sample-seconds 3` | 3 | 采样间隔 |
| `--fps 30` | 30 | 源窗口 Present 节奏，同时作为 WGC `minimum_update_interval` |
| `--no-census` | 关 | 只记总数不做类型直方图；枚举实际耗时见 `scan_micros` |

## 产物

- `handle-growth.jsonl`：逐条采样，schema `diogenes.handle-growth-probe/v2`。
- `summary.json`：三段的斜率、每帧句柄数和类型差值。

控制台也会直接打印每段结论，例如：

```
stage encode running for 60s ...
  handles 2104 -> 3663 (+1559) = 29.982/s, 1.001/frame
    Event                    +1559
```

## 怎么读结果

- **某一段的 `handles_per_processed_frame` ≈ 1.0**：该阶段观察到与处理帧数近似同比例的增长。
  结合 `type_delta` 及样本完整性缩小范围，不能仅凭速率认定根因。
- **`wgc` 段就 ≈ 1.0**：说明不写入 MF 编码器时也能出现增长；仍需区分渲染源、WGC、
  GPU 拷贝、诊断工具及其他运行时行为。
- **三段都 ≈ 0**：只说明当前合成条件未复现，不能排除共享组件在真实游戏环境下的问题。

这个工具不走 `record_inner`，不含 #922 的 CFR 封槽/空槽等待逻辑。但 Capture 24 没有句柄
类型或创建栈，不能仅因两次增长速率相似便认定为同一个问题，也不能由此为补丁的全部性能影响免责。
旧验收文档包含句柄要求，不代表已经证明故障早于 #922。

无论哪种结果都还不是根因证明，只是把范围从"整个采集进程"收敛到一段代码加一个对象类型。

## 已知边界

- 需要 NVIDIA 卡：设备走 `D3dContext::create_nvidia_hardware_only()`，和生产同一条。
- 句柄值是暴力遍历出来的（Win32 没有按进程枚举句柄的 API），句柄表特别稀疏时可能
  扫不全；这种情况 census 会把 `incomplete` 置真，直方图按下界看。
- 构建需要 Windows/MSVC。原始来包曾在 Mac 上借 Windows target 与签名 stub 做过
  check、Clippy；Windows 侧后续已使用真实依赖验证，见下文。两种检查不能混作同一证据，
  编译或单测通过也不等于 GPU 采集、硬编或真实游戏验证通过。

## Windows 侧补充（2026-09-08）

- 使用生产 workspace 锁文件中的版本生成独立 Cargo.lock，真实依赖下 check、Clippy 和
  3 项量具单测通过；不是 stub 编译。
- 补充主线程 COM MTA 初始化、无激活窗口创建与显式 DestroyWindow。
- 类型查询使用对齐缓冲区，校验 UTF-16 返回范围；500ms 扫描预算、失败计数及前后总数变化
  都记入 census。不完整枚举只能看下界；扫描不是原子快照，即使计数相同也可能有并发替换。
- `type_delta_complete` 表示两端未检出枚举不完整；`measurement_healthy` 要求有稳态样本、
  处理帧且无 ring/NV12 饥饿或 copy timeout。两者都不代表根因确认。
- 先做 8 秒只渲染的对照，阶段结束后记录 WGC stop、阶段资源 drop 和 3 秒后状态，最后记录
  渲染源/MF/设备关闭及 3 秒后的状态。建议每个阶段独立进程运行，避免跨阶段残留混杂。
- GPU copy timeout 不再继续编码或回收未完成表面；输出文件 create-new，历史结果不覆盖。
- v2 将原本只统计成功提交的 `frames_encoded` 改为 `frames_submitted`，新增实际抓取的
  `frames_output`，正常结束时要求两者相等。`encoder_finished` 记录实际编码器身份与最终
  生效配置。旧 v1 的 `frames_encoded` 不能当成已核对过的输出帧数。
- SHA256SUMS.txt 保留原始来包校验值；修改后的工具及生产依赖应在执行前另外冻结 hash。

只读查看 NV12→H.264 硬件 MFT 清单，可运行
`cargo run --release --manifest-path DataMiner/tools/handle-growth-probe/Cargo.toml --example list_hardware_encoders`。
它不激活编码器、不采集、不改系统配置；枚举成功不代表生产 writer 会选中对应项。
