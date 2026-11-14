# Issue #750 · D3D11 双进程共享纹理实验

这个目录是一次性硬件取证工具，不属于 recorder / probe 生产路径，也不加入
`DataMiner/Cargo.toml` 工作区。它只回答 docs/125 §9.1 的 V6：无 keyed mutex 的
`MISC_SHARED | MISC_SHARED_NTHANDLE` 纹理环，在读端存活和被杀两种状态下，写端是否出现
等待或长尾。`SHARED_NTHANDLE` 只选择 NT handle 类型，`SHARED` 才声明资源可共享；本机
RTX 4060 驱动对只设前者的描述在槽 0 返回 `E_INVALIDARG`。

固定契约：

- 两个独立进程，各自创建 D3D11 device；
- 96 槽 × 192×8 BGRA；槽位为 `ordinal % 96`；
- 每槽用 `IDXGIResource1::CreateSharedHandle` 创建带名字的 NT handle，读端用
  `ID3D11Device1::OpenSharedResourceByName` 打开；
- 不创建、不查询、不调用 `IDXGIKeyedMutex`；
- 写端用命名共享内存发布单调 ordinal，读端逐像素核对 ordinal/slot/content；
- 写端分别记录 `CopySubresourceRegion` 调用和 `CopySubresourceRegion + Flush` 的 QPC 开销；
- 活态阶段结束后，父进程强杀读端并 `wait()` 确认退出，再继续记录死态阶段。

Microsoft 的 D3D11 共享资源文档要求资源在一个 device 上更新后调用 `Flush`，而 `Flush`
文档明确它异步返回。因此实验保留两列：纯 copy 调用成本，以及包含合规提交成本的总调用成本。
这不是 GPU 完成时间，也不能代替 118 号 A/B FPS 门。

## 运行

```powershell
cd DataMiner/tools/issue750-shared-texture
cargo run --release -- adapters
cargo run --release -- run --output evidence/same-adapter
```

默认 writer/reader 都选 adapter 0，240 Hz，先预热 2 秒，再记录读端存活 10 秒和被杀后
10 秒。读端默认落后写端 4 槽（240 Hz 下约 16.7 ms），模拟实际链路里 ROI copy →
同 context Present → DWM/WGC observation 之后 recorder 才取槽的先后关系。`--reader-lag-slots 0`
是更激进的立即读取压力项；它能暴露 CPU ordinal 发布早于 GPU 完成的窗口，不能冒充实际 WGC
时序。完整参数：

```text
run [--writer-adapter N] [--reader-adapter N] [--hz 240]
    [--warmup-seconds 2] [--alive-seconds 10] [--dead-seconds 10]
    [--reader-lag-slots 4]
    [--output PATH]
```

跨 adapter 机器上再跑：

```powershell
cargo run --release -- run --writer-adapter 0 --reader-adapter 1 --output evidence/cross-adapter
```

## 输出与判读

- `writer-samples.csv`：每次写入的原始 QPC ticks 与微秒数；必须随证据保留，不能只抄平均值。
- `summary.json`：alive/dead 各自的 p50/p95/p99/max、adapter LUID、driver、读端核对计数。

先检查硬失败：`reader_mismatches_before_kill == 0`、`reader_open_failures == 0`、
`reader_kill_succeeded == true`。再比较 alive/dead 的 p95/p99/max；只有原始分布没有出现
与读端状态相关的长尾，才能把本机组合记为 A-2 放行证据。工具不内置任意的微秒阈值，最终
还必须把本结果与 118 号 A/B 的 2% / 3% FPS 门合看。
