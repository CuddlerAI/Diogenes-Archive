# ETS2 · PR 405 采集与交付工具

面向**采集机上的操作**。工程原理见
[docs/103](../../docs/103-ets2-pr405-delivery-runbook.md)；采集员的操作手册见
[docs/79](../../docs/79-annotator-depth-capture-manual.md)。

与 [`tools/pr405-fallout4`](../pr405-fallout4/README.md) 同形，但 ETS2 这条链有一处
根本差别，值得先说清楚：

| | Fallout 4 | ETS2 |
|---|---|---|
| `player_*` | 精确构建内存偏移（钉 EXE SHA-256） | **官方 Telemetry SDK（MIT）** |
| `camera_*` 位姿 | 同上，probe 内 Present 回调采样 | 精确 draw 的 GPU view 矩阵；SDK 三级复合作独立交叉验证 |
| `camera_intrinsics` | GPU 常量缓冲里的投影矩阵 | 同一个精确 draw、同一逻辑缓冲区的 GPU projection 矩阵 |
| depth | D3D11 DSV hook | 同左；与 view/projection 共享精确 pass 身份 |
| `metric_scale` | 0.0142875（引擎惯例 + 锚点交叉验证） | **1.0**（Prism3D 世界单位就是米） |

也就是说：ETS2 的 `player_*` 世界位姿与速度走官方通道；逐 Present 的相机 view、
projection 与 depth 走 D3D11 probe，并由同一个 pass/draw 身份闭环。SDK 的
`world.placement ∘ cabin ∘ head` 与解包/控制台 FOV 保留为独立交叉验证，不再冒充
逐帧 GPU 观测值。正式采集仍硬性要求
**一次只用一个相机视角、全程不改 FOV**，见 §4。

---

## 1. 一次完整流程

若 Steam UI 在创建进程前报 `AppError_33`，不要重复调用 `steam://run/227300`，也不要使用
`-noonline` / `-no_steam_subsys`。本机 2026-08-11 实测：前者会在 online subsystem
未初始化时触发非连续异常，后者会脱离 Steam profile 路径。使用精确 EXE helper；它先核对
build 23966373 的 SHA-256，再只在子进程环境中设置 `SteamAppId/SteamGameId=227300`，以
DX11 方式启动到 profile 菜单，不改 Steam 安装或云存档：

```powershell
$session = & tools\pr405-ets2\Start-Ets2ResearchSession.ps1
$session | Format-List
```

helper 只证明进程与顶层窗口成功创建。操作员仍须选择 **Continue Career** 并确认已进入驾驶场景；
主菜单、profile 桌面或加载画面都不能开始 depth 认证。

```powershell
# ① 装遥测插件（换游戏版本或换构建才需要重跑）
tools\pr405-ets2\Install-Ets2TelemetryPlugin.ps1

# ② 取地图边界（解包一次就够，除非游戏更新）
tools\pr405-ets2\Export-Ets2MapBounds.ps1 `
  -SectorDir C:\ets2-extract\map\europe `
  -Out tools\pr405-ets2\evidence\ets2-map-bounds.json

# ③ 填 ets2-pose-config.json（见 §3），然后自检
tools\pr405-ets2\Test-Ets2Readiness.ps1 `
  -PerfCertificationManifestPath C:\path\to\performance-certifications.json `
  -ProbeRuntimeManifestPath C:\path\to\dm-depth-probe-runtime.json `
  -PerfCertificationKeyPath C:\path\to\generation-key.json `
  -PerfHardwarePath C:\path\to\hardware-audit.json

# ④ 只有 exit 0 才开采。采集本身走客户端，不在本目录。
#    这里仅核对 authoring manifest 的适用性；正式 smoke 还必须让客户端在线验签 R2 current.json。

# ⑤ 采完转交付
tools\pr405-ets2\Invoke-Ets2Delivery.ps1 `
  -CaptureDir  <depth 采集目录> `
  -TelemetryPath <dm-ets2-telemetry.jsonl> `
  -FramesPath  <frames.jsonl(.zst)> `
  -WatermarkSamplesPath <watermark-samples.jsonl> `
  -VideoPath   <video.mp4> `
  -ClipId      ets2-vehicle-3min-0001 `
  -OutRoot     D:\deliveries
```

首次适配新 ETS2 build 时，先在不可交付目录扫描 GPU camera 常量。游戏进入驾驶场景后：

```powershell
# A1：静止、只转视角；随后可用 SetPhase 扫下一个 draw 窗口或记录另一动作 phase。
tools\pr405-ets2\Invoke-Ets2CameraDiscovery.ps1 -Action Start `
  -OutputDir D:\dm-evidence\ets2-camera-discovery -PhaseId A1 -DrawWindowStart 0

tools\pr405-ets2\Invoke-Ets2CameraDiscovery.ps1 -Action SetPhase `
  -PhaseId A2 -DrawWindowStart 128

# SetPhase 只有在 A1 固定报告封口，且 A2 mutable 报告已处理至少一个 draw 后才会 exit 0。

# 最终 Flush 并生成 camera-candidates-A2.json；超时会失败，不会把 checkpoint 当封口报告。
tools\pr405-ets2\Invoke-Ets2CameraDiscovery.ps1 -Action Stop
```

`Start` 钉住 ETS2 build 23966373 的 EXE hash，并只接受实测的
`R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT` 64-bit DSV。输出是 discovery 证据，不能交付；
必须据各动作 phase 的候选报告生成并复核 production binding 后再开正式 run。

每个 discovery phase 在命中首个采样 draw 时会对当时已绑定的 VS constant buffer 做
**一次且仅一次**冷启动快照。这补齐“buffer 在 probe 启动前已经上传并绑定、之后不再触发
Map/Update/VSSet 回调”的盲区。D3D11.1 大缓冲会额外截获采样窗口内的
`VSSetConstantBuffers1`，只回读着色器实际可见的绑定范围；报告里的 `buffer_bytes` / `offset`
因此描述稳定的逻辑范围，而不是每帧轮转的 2 MiB backing resource 绝对地址。
日志中的 `cold-start VS snapshot seeded ...; failed=...` 必须随候选报告复核。若所有 phase
仍然零候选，不得手填 binding，应保留报告并检查 DSV/窗口/着色器阶段是否匹配。

本构建已认证的 profile 与 binding 分别是 `ets2-pr405.lane30.json` 和
`ets2-camera-binding.json`；来源选择与四阶段报告哈希见
`evidence/ets2-dsv-projection-20260810.json`。真实驾驶舱中的正式采集闭环见
`evidence/ets2-production-validation-20260810.json`：8/8 depth rows/files、1124 行同 run
相机位姿、0 diagnostics、0 writer errors，8 帧都命中投影 draw 所在的精确 DSV pass。

正式采集必须保持 `pass_identity_trace=false`。该开关只用于候选排障；打开时会刻意保留
完整 DSV 候选环并回到 `unbind_from_end` 选择，不再使用 production 的精确投影 pass。
它不能用于 profile 认证或正式采集。

配置里的 64-bit DSV 枚举必须使用 serde 契约拼写
`r32_g8_x24_typeless` / `d32_float_s8_x24_uint`。`SetPhase` 在 Windows 上通过带备份路径的
`File.Replace` 原子切换配置；缺少备份路径会被 Windows 拒绝，phase 也不会真正切换。

## 2. 每个脚本做什么

| 脚本 | 做什么 | 失败的含义 |
|---|---|---|
| `Start-Ets2ResearchSession.ps1` | 精确哈希门禁后绕过 Steam `AppError_33` 启动 DX11 菜单 | build 漂移、已有实例或窗口未创建 |
| `Install-Ets2TelemetryPlugin.ps1` | 放 DLL、设 `DM_ETS2_*` 环境变量 | 路径/位数不对 |
| `Invoke-Ets2CameraDiscovery.ps1` | DSV/投影常量 discovery、phase 热切与封口 | build/格式/候选链不闭环 |
| `Test-Ets2Readiness.ps1` | 采集前 fail-closed 自检 | **exit 2 就不要开采** |
| `Export-Ets2MapBounds.ps1` | 扇区文件名 → 四角边界 | 解包目录不对 |
| `Invoke-Ets2Delivery.ps1` | pair → ets2-pose → deliver | 见下 |

`Invoke-Ets2Delivery.ps1` 的三步各自可能失败，含义不同：

- **pair 失败** → 配对覆盖率不到 98%，多半是游戏帧率没锁 30（#501 的教训）；
- **ets2-pose 失败** → 遥测与视频对不齐；看 `ets2-pose-report.json` 的 `rejected`
  逐条原因，它会指出是掉帧（`skew_exceeded`）、暂停（同上）还是超窗（`outside_telemetry_window`）；
- **deliver 失败** → 68 号的交付门，缺件会列在 `delivery-manifest.json` 的 `blocking` 里。

## 3. 相机基线怎么取

`ets2-pose-config.json` 里那三格（`default_head_position_m` / `horizontal_fov_degrees` /
`evidence_path`）决定整批数据的相机位置与内参，**取错了看不出来**。两条路：

**路 A（推荐）· 解包驾驶舱相机定义。** `scs_extractor` 解出车辆包，读该车型
interior 相机的 `fov` 与驾驶员头位。0806 那批 Scania S 2016 走的就是这条，取到 71°，
且由 near/far 推出的投影 a/b 与 depth-index 逐位一致。`fov_source` 填
`unpacked_game_definition`，`evidence_path` 指向 `evidence/` 下的转录文件。

**路 B · 控制台钉死 FOV。** `r_multimon_mode 4` + `horizontal_fov_override <角度>`，
得到确定角度值。`fov_source` 填 `console_override`，并把设定过程记进 `evidence/`。
这条路下 §4 的「全程不改 FOV」尤其要守住。

两条路都算完之后，用 95 号 §2.3 的双路交叉验证核一次：`fx = (W/2)/tan(hFOV/2)`
与从投影矩阵反算的 `fx` 相对误差应当 <1%。

## 4. 开采前口头三问（脚本查不到，必须人确认）

1. **游戏帧率锁在 30 了吗？** 驱动锁或游戏内锁都行，但要在游戏内 FPS 显示上看到
   稳定 30 上下（不是 59、不是 61）。没锁的后果见 #501：对齐率 47.8%，61 GiB 全废。
2. **这一段只用一个相机视角吗？** 车内视角选定后，**全程不切视角、不做头部环视
   （鼠标右键拖动看四周）、不进照片模式、不开地图/菜单**。
   偏移一旦非零，转换会整段拒绝——因为复合次序尚未实机标定（见 §5）。
3. **这一段全程在驾驶吗？** 暂停、主菜单、下车步行都会在遥测里变成 skip 记录，
   对应的视频帧没有位姿，按 68 号 §6.5「缺一条全部打回」。

## 5. 两条尚未标定的约定

两者都**没有默认值**，标定前一律填 `unvalidated`，它会原样进交付元数据，
使结论出来后既有数据仍可重算。

| 约定 | 现状 | 怎么标定 |
|---|---|---|
| `roll_convention` | 官方文档对 roll 轴方向有两种读法，差一个正负号，**平地上永远看不出来** | 见 [`crates/ets2-player-telemetry/README.md`](../../crates/ets2-player-telemetry/README.md) 的「roll 符号标定」，约 10 分钟 |
| `offset_composition` | `cabin.offset` / `head.offset` 是「相对默认位姿的增量」，左乘右乘两种读法都说得通 | 见 [`crates/ets2-player-telemetry/src/camera.rs`](../../crates/ets2-player-telemetry/src/camera.rs) 的 `OffsetComposition` 文档 |

`unvalidated` 下 `offset_composition` 会**拒绝任何非零偏移**，所以 §4 第 2 问是硬要求，
不是建议。

## 6. 不包含

- 游戏文件、`scs_extractor`、SCS SDK 压缩包
- 任何原始 RGB / depth / 遥测数据
- 密钥、token、凭据
- 采集机的本地绝对路径（脚本参数里给，不写进仓库）
