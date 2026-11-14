# ETS2 / ATS Telemetry SDK 插件

游戏加载的那个 DLL。逐帧把 `player_*` 写成 JSON Lines,配合
[`ets2-player-telemetry`](../ets2-player-telemetry/README.md)(拥有换算与契约)使用。
对应 [98 号 §5.7.1](../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md) 第 ①② 步。

## 分工

| crate | 拥有 |
|---|---|
| `ets2-player-telemetry` | 欧拉→四元数、轴变换、速度来源记账、契约 |
| **本 crate** | 与游戏进程的接口:注册通道、按帧攒数据、非阻塞落盘 |

**判断逻辑一行 Windows API 都没用。** 通道回调只调用引擎传进来的函数指针,
状态机是纯算术。唯一的例外是 `host_clock` 的两个只读调用
(`QueryPerformanceCounter` / `QueryPerformanceFrequency`),它必须是原始 QPC
才能与采集端**跨进程**对齐——`render_time` 会被引擎重置,`std::time::Instant`
的原点跨进程不可比,两者都 join 不了视频帧。除此之外本 crate 是平台无关的,
这就是它能在没有游戏、没有 Windows 的机器上被验证的原因。

## 构建

```bash
# Windows 上出 DLL
cargo build -p ets2-telemetry-plugin --release
# → target/release/ets2_telemetry_plugin.dll
```

macOS/Linux 上**编不出 DLL**(链接需要 MSVC 工具链),但可以做全部类型与 ABI 检查:

```bash
cargo clippy -p ets2-telemetry-plugin --target x86_64-pc-windows-msvc --all-targets -- -D warnings
cargo test -p ets2-telemetry-plugin
```

`ffi.rs` 里的布局断言是**编译期**的,上面那条 `--target` 命令一跑就等于把
头文件的每一条 `scs_check_size` 都验了一遍。32 位游戏用 `i686-pc-windows-msvc`。

## 安装

把 DLL 放进游戏的插件目录(不存在就自己建):

```
<Steam>/steamapps/common/Euro Truck Simulator 2/bin/win_x64/plugins/ets2_telemetry_plugin.dll
```

ATS 同理;32 位游戏放 `bin/win_x86/`。加载成功时游戏日志(`game.log.txt`)里会有一行
`dm-ets2: 已就绪 out=… roll_convention=… roll_status=…`。

## 配置(环境变量,游戏启动前设)

| 变量 | 取值 | 默认 |
|---|---|---|
| `DM_ETS2_TELEMETRY_OUT` | 输出 JSONL 路径 | 系统临时目录下 `dm-ets2-telemetry.jsonl` |
| `DM_ETS2_ROLL_CONVENTION` | `unvalidated` / `forward` / `local_z_backward` | `forward` |

装插件与设变量可以直接用 [`tools/pr405-ets2/Install-Ets2TelemetryPlugin.ps1`](../../tools/pr405-ets2/Install-Ets2TelemetryPlugin.ps1)。

ETS2 A46 实机标定通过 GPU 相对旋转与 SDK world/cabin/head 三级位姿交叉验证，确认
`forward`：统一正号在 24 个有效 roll 过渡中胜出 23 个，因此未配置时默认使用
`forward`。显式拼错或空白的取值仍会**退回 `unvalidated`**，而不是静默套用已标定值。

## 输出格式

每帧一行 `ets2-telemetry-frame/v3`,**无论那一帧有没有有效样本**。v3 新增
`host_timing` 和 `camera_cross_check`，用与 D3D11 probe 相同的 Windows QPC 时钟记录
SDK 帧区间，并保留 cabin/head 的默认位置与逐帧偏移：

```json
{"schema_version":"ets2-telemetry-frame/v3","frame_seq":1,"render_time_us":1000,
 "timer_epoch":0,"host_timing":{"status":"available","frame_start_qpc":50000,"frame_end_qpc":50250,"qpc_frequency_hz":10000000},"player":{...},
 "camera_cross_check":{"source":"scs_telemetry_sdk114","cabin_default_position":{"status":"available","position_m":[0,1.5,-0.5]},"head_default_position":{"status":"available","position_m":[0,0.7,-0.2]},"cabin_offset":{"status":"available","value":{"position_m":[0,0,0],"heading_pitch_roll_turns":[0,0,0]}},"head_offset":{"status":"available","value":{"position_m":[0,0,0],"heading_pitch_roll_turns":[0,0,0]}}},"skip_reason":null}
```

几个字段值得单独说:

- **`render_time_us`** 是引擎时钟，只保留作诊断；它会被 `timer_restart` 重置，
  不能直接与采集端的 `source_qpc` 跨进程比较。
- **`host_timing`** 是 SDK 帧的 QPC 起止区间；只有 discovery 的 `sample_qpc`
  落入该区间或满足明确的最近邻容差时，才能声称是同帧交叉验证。
- **`camera_cross_check`** 保存 truck 配置里的 `cabin.position` / `head.position` 默认位置，
  以及逐帧 `cabin.offset` / `head.offset`。它只用于核对 D3D11 hook 候选，不能替代
  最终 camera pose 交付。
- **`timer_epoch`**:引擎会重置计时器,重置时该值 +1。**只有同一 epoch 内的时间戳可比**;
  跨 epoch 按单调递增去 join 会错配整段。
- **`skip_reason`**:空档是**显式**的,不是消失的。`paused` / `no_world_placement` /
  `no_speed_source` / `conversion_rejected` / `no_frame_start`,每一条都是可判读的事实。

`frame_seq` 连续且从 1 起,所以「这一帧没数据」(有 skip 记录)与「这一帧丢了」(缺号)
可以分开——队列满时会丢,丢了多少在关机日志里报。

## 三条设计纪律

1. **暂停期绝不产出 player 载荷。** 主菜单里通道停更而上一帧的值还在内存里,
   照写就是把菜单时段伪装成行驶时段——那种帧事后无法从数值上分辨。
   暂停还会**清空通道缓存**,恢复后必须重新上报才算有值。
2. **游戏线程永不阻塞。** 回调里只做一次 `try_send`,队列满就丢并计数,**绝不重试**
   ——重试就是把阻塞换个写法。卡顿会改变玩家操作,污染的是数据分布不只是时间戳。
3. **panic 绝不跨 FFI 边界**(那是 UB)。所有回调套 `catch_unwind`。

## 首次上机检查单

A43/A45/A46 已在迁移前的本地实现上完成真机验证；本次移植到最新上游后，仍应按顺序
做一次集成冒烟。以后每次升级游戏、SDK 或 D3D11 probe 也按这份清单回归：

1. **加载成功**:游戏日志出现 `dm-ets2: 已就绪`。没有就是 DLL 位置/位数不对。
2. **导出符号在**:CI 已用字节扫描验过 `scs_telemetry_init`/`scs_telemetry_shutdown`;
   本地构建的话可用 `dumpbin /exports`。
3. **有帧在写**:开一局跑十秒,JSONL 行数应约等于 帧率 × 秒数。
4. **暂停确实产出 `paused`**:进主菜单十秒,该段应全是 `skip_reason="paused"`。
   若那段仍有 `player`,说明 `paused` 事件没收到——这是最要命的一条,先停下来查。
5. **`host_qpc` 单调递增**,且与采集端 `frame-pairs.jsonl` 的 `source_qpc`
   落在同一区间。这一步决定 68 §6.5 的对齐硬闸能不能过——
   用 `depth-lab ets2-pose` 跑一遍,看报告里的 `skew_p95_us` 是否远小于 16667。
6. **位姿方向对**:把车停在一段已知朝向的直路上,核对 `player.rotation_quaternion_xyzw`
   ——朝北应接近 `(0,0,0,1)`,朝东应接近 `(0,0.707,0,0.707)`。
7. **roll 符号标定**:见 `ets2-player-telemetry` README「roll 符号标定」,约 10 分钟。
8. **丢帧率**:关机日志里 `dropped_queue_full` 应为 0。不为 0 就调大 `sink::QUEUE_CAPACITY`
   或换更快的盘,**不要**改成阻塞写。

## 验证边界

- **当前上游移植分支尚未重新做真机冒烟。** A43/A46 已证明迁移前的本地实现能被游戏
  加载，并能用 QPC 将 SDK world/cabin/head 与 GPU 相对旋转交叉验证；这份历史证据不能
  自动替代当前分支的回归。注册序列和回调线程模型仍按官方文档防御性处理，`host_qpc`
  与截帧的实测偏差继续以 `ets2-pose` 报告里的 skew 统计为准。
- **`cabin.offset` / `head.offset` 的复合次序未标定。** 见
  `ets2_player_telemetry::OffsetComposition`:标定前非零偏移一律拒绝,
  所以采集时必须 `g_cam_physics 0` 且不做头部环视。
- **32 位构建的导出符号名未验证。** x86 上 `__stdcall` 会被修饰成 `_scs_telemetry_init@8`,
  而游戏按未修饰名查找。Rust 的 `#[unsafe(no_mangle)] extern "system"` 在
  `i686-pc-windows-msvc` 上到底出不出修饰名,本轮**没有实际链接验证过**
  (本机没有 MSVC,CI 只构建 x64)。若 32 位加载不上,先用 `dumpbin /exports` 看名字,
  必要时加 `.def` 文件或 `#[export_name]`。**x64 无此问题**(x64 只有一种调用约定,不修饰),
  而现代 ETS2/ATS 都是 x64,所以这条优先级低。
- **CI 的导出校验是字节扫描,只跑 x64。** 它能抓「符号根本不在」,
  抓不到「名字被修饰了」——因为修饰名里也含未修饰名这个子串。
- `truck.speed` 与 `truck.local.velocity.linear` 在打滑/倒车时的实际差异未实测。
- ~~与 `camera_*`(D3D11 hook)的合流没做~~ —— 已改走三级复合
  (`ets2_player_telemetry::compose_interior_camera`),`camera_*` 不再需要 D3D11 hook;
  代价是内参改用钉死的 FOV(95 号 §3.3 路线 D)。见 `tools/pr405-ets2/README.md`。
