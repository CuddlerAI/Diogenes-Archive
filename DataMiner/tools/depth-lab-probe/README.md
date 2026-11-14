# dm-depth-probe

Windows-only 的 D3D11 注入 probe。BioShock Infinite 的可执行文件位于 `Binaries/Win32`，
因此必须构建 32 位 DLL；默认的 x86_64 host 产物不能注入该游戏。

```powershell
rustup target add i686-pc-windows-msvc
cargo build -p dm-depth-probe --release --target i686-pc-windows-msvc
```

产物：

```text
target/i686-pc-windows-msvc/release/dm_depth_probe.dll
```

支持的深度资源/视图格式为：

- `R24G8_TYPELESS` / `D24_UNORM_S8_UINT`（32 bit/px）；
- `R32_TYPELESS` / `D32_FLOAT`（32 bit/px）；
- `R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT`（64 bit/px，低 32 位为 f32 深度）。

未知格式会明确拒绝，不会按相近布局猜测。ETS2 build 23966373 的主场景 DSV 实测为
第三种格式；缺少这一路径时会在资源选择阶段 fail-closed。
若引擎源资源是 fully typed `D32_FLOAT_S8X24_UINT`，probe 会先按同一 DXGI type group
拷入 `R32G8X24_TYPELESS` 私有快照，再用 `R32_FLOAT_X8X24_TYPELESS` SRV 在 GPU 上
降采样；WARP 测试会把这条结果与 8 byte/px CPU 参考逐位对拍。

## 测试

### 自动局部性能验证（Windows，仅显式 local-perf-lab + debug assertions）

`dm-depth-perf-lab` 是无窗口负载工具，复用生产 `QueryCache`、volatile reader、
查询排序函数、64 分片 `CbvDescriptorStore`。不用人操作，不启动/注入游戏、不安装 hook，
不创建 swapchain/WGC/NVENC，不改变准入或认证规则。只回答局部改动的成本/退化问题，
不是完整游戏模拟器或 CP2077 FPS 预测器。

```powershell
$env:DM_DEPTH_PROBE_RUNTIME_VERSION = 'depth-2026.09.12' # 本地未发布批次，须与实际 runtime policy 核对
$env:CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS = 'true'
cargo run --offline --locked -p dm-depth-probe --bin dm-depth-perf-lab --release `
  --target x86_64-pc-windows-msvc --features local-perf-lab -- `
  --name query-baseline-01 --threads 1,4,8 --heap-mib 192 --batches 128 --rounds 4
```

从 DataMiner 目录执行。输出固定在 `DataMiner/target/local-perf-lab/<name>.json`，
`create_new` 拒绝覆盖；失败/超预算报告 `completed=false` 并返回非零。默认只选 NVIDIA
物理适配器，无静默 WARP/iGPU fallback；显式 `--warp` 仅作软件设备逻辑验证。
普通 release 关闭 debug assertions 时，入口明确拒绝执行。默认关闭此 feature。
构建此 lab 可能覆盖 Cargo target 下的开发 DLL；**不能拿该 DLL 替换已封存的游戏测试包**。

工具分两组，均先预热、交替正反顺序运行所有 case，不挑有利轮次：

- 页查询：本程序拥有一个 1–256 MiB、持续 Map 的真实 D3D12 upload resource，初始化后
  不再写入，所有线程退出后才 Unmap。每批 1–32 个精确 256-byte 读取，源偏移可不按
  256 对齐；原范围/页保护校验不变。比较“查询在模拟共享锁内”“锁外原顺序”“锁外升序”，
  三者实际读取顺序完全相同，每批重新建缓存。读出的所有字节必须正确。
- descriptor：复用生产 store 的 plan/apply 算法，默认 8 descriptor/copy，
  `--descriptors-per-copy` 可取 1–64；各线程写独立目标，
  默认合成 handle 对比访问分散/同片、GPUVA 恒定/交替变化。校验每次返回、最终目标
  GPUVA/size、全部更新计数，并在 join 后核对所有 live 代次非零且唯一、反向计数与正向表一致。
  每 case 另保留最慢一次实际 copy 的 worker/序号和 total/wait/apply 联合计时，不能把
  不同分布的最大值拼成同一个事件；只存一条，不新增锁或线程周期查询。
  默认只测 CPU 记账路径。一个当前版本内部的负载对比不是“旧版/新版 A/B”；比较版本
  必须封存两个 exe/hash，使用相同参数独立多轮交替执行，保留所有结果。

`--descriptor-handles native` 切换到自有真实 D3D12 CBV heap：从设备查询 handle increment，
使用真实 upload GPUVA 建立两个只读源 bank，实际执行 `CopyDescriptorsSimple`。
源 heap 非 shader-visible，目标 shader-visible，各 worker 写不相交的连续范围。
所有 heap/resource 持有到 worker join；handle 只交给 API，绝不解引用。native 的两组布局
是紧密排列（`hot_shard=true`）/worker 起点间隔至少 4096 handle 整数单位；这是测试分组，
不是对物理内存页的声明，也不保证紧密布局恰好只有一个 shard。报告实际 source/destination
shard 数、设备 increment 和 worker stride。native 要求 balanced layout。
驱动调用单独计时，store total/wait/apply 不含驱动，case wall 包含两者。
检查设备未移除；不做 shader/readback，因此 CPU bookkeeping 校验不等于 GPU descriptor 内容验收。

仅 lab 构型可使用以下对照，均不替换普通构建的锁/分片策略：

- `--descriptor-lock std|eventual|fair|rw`：默认 std；其他为 parking_lot 锁。lab wrapper 给
  所有策略补充 panic 后拒绝访问的语义。rw 只让 source/get/range 等只读访问共享，目标写独占。
  wrapper 的枚举/poison 开销也在实验中，不能把绝对值当成普通 DLL 的原样时延。
- `--descriptor-layout balanced|source_hot`：合成地址默认 balanced，source_hot 的热点 case
  把源放在同片、各 worker 目标分开。两者为固定地址序列，不是完整引擎工作负载。
- 编译 feature `local-perf-lab-page-hash`：对完整 handle 页编号 XOR 折叠，仍按 4096 分组。
  `local-perf-lab-fine-hash` 则实验性按 256 handle 整数单位分组、再折叠。
  `local-perf-lab-pad-shards` 仅在 lab + debug assertions 下把 shard cell 按 64 字节对齐。
  报告记录实际 shard policy、cell 大小/对齐。这些候选可能改善热点却让其他负载退化，
  没有自动准入/自动选择赢家，也不因实验胜出直接改变生产策略。
- `local-perf-lab-inline-plan`：仅把最多 8 项的两个排序索引和源快照临时数组放进
  SmallVec 内联缓冲，超过 8 项正常溢出到堆；4096 项原上限不变。报告 inline capacity，
  不改变分组顺序、源先于目标读取、代次分配或任何锁规则。普通构型仍用 Vec。
- `local-perf-lab-source-cache`：实验性线程本地元数据缓存，最多 8 个精确源序列，
  每序列最多 64 项/4 个源 shard；不缓存相机字节。普通 CBV write/invalidate/release 在
  同一 shard mutex 内更新 epoch；任何 copy destination 永久禁用该 shard 的缓存。
  owner 身份隔离 store，版本溢出禁用、写中/poison/版本变化退回原读锁路径。
  `--no-descriptor-source-cache` 保留完全相同的 cell 布局及 epoch 写，仅禁用复用，
  用于同一 exe 对照；这不等于无此 feature 的生产开销基线。运行前用独立 fixture
  校验更新、释放、重复目标、跨实例、1000 次跨线程有序改写、版本耗尽和缓存后的 poison。
  poison 校验会打印一条明确标注 intentional 的预期 panic，子线程被 join，只有校验
  全通过才继续，并记录 `source_cache_preflight=true`；不能忽略其他 panic 或非零退出。

`--descriptor-refresh-every N`（默认 0 关闭，1–10000）：合成负载每 N 次 copy 前重写
当前源序列的同 GPUVA/size 描述符，产生新代次；各线程源 handle 独立，分片可能共享。
源重写单独计时，copy latency 不含它，case wall 包含它，更新总数及重写样本数严格对账。
N=1 时缓存命中必须为 0。仅用于合成句柄、不能与 owner trace 同用，避免未观测的写锁
混入持锁者关联。它不是游戏真实命中率的预测；收益与退化都要保留。

定点归因可另编译 `local-perf-lab-lock-trace`，再显式添加 `--descriptor-owner-trace`
（最多 2000 copy/线程、8项/copy、2轮、3种线程数；需相应设置 `--rounds 2`）。
若同编 source-cache，必须另设 `--no-descriptor-source-cache`；不能同时启用 source refresh。
这是一组**有扰动的观测**，不是性能 A/B：仅给自有 store 的 copy source/destination 锁
记录 attempt/acquired/unlock 前后时刻及持锁线程周期，额外周期查询本身的前后时刻也保留。
每线程至多 32000 条、最多 16 线程，线程本地缓冲在 start gate 前预分配，记录在解锁后
追加；最终值校验移至全部 worker join 后，避免把未观测的校验读锁混进时间窗口。
按完整 handle 分组计算精确应有访问数，丢失/少记录/时钟顺序错误即失败。
报告最长获取窗口、同片已观测持锁区间并集、所有重叠持锁者数量；最多采样其中重叠时间
最大的32条事件，汇总计算包含全部记录。内存中完整事件不作为全量 JSON 输出。
这些时间只是已观测持锁的下界：没被并集覆盖的时间不能直接叫“锁空闲”或“线程未调度”，
也可能在计时边界被抢占。cycles 仍不换算毫秒，不能据此证明历史游戏慢调用的因果关系。
没有选此 feature 时，普通工具/游戏构型没有这些额外 TLS/周期探针。

默认 1/4/8 线程，每个线程使用相同均匀抽取的 workload 子集，只轮换访问顺序。
最多 16 线程、256 批/线程/轮、8 轮、10000 descriptor copy/线程/case；
默认 90 秒、最大 180 秒的合作式预算每次操作前检查（不是对驱动调用的强行超时）。
不改进程优先级/CPU 亲和性、不模拟输入、不开调试层；测量时不要同时编译或跑游戏。
分线程延迟有均值/P50/P95/P99/max，查询数/耗时、读取命中/字节、锁争用次数/等待，
另有每 case 总 wall time。并发累计时间不能当墙钟占比；这些都不是帧率或 1% low。
模拟锁省略 tracker 校验及 draw 竞争，不能用它解释游戏约 30% 的竞争率。
本程序未启动旁路 JSONL worker，其新增计量/RPM 成本也不在测试范围内。

可用 `--workload path.json` 输入只读的既有源偏移序列（文件至多 4 MiB）：

```json
{"schema":"dm-local-query-workload/v1","scope":"representative offsets, not full game replay","source_sha256":null,"batches":[[49152,32768,16384],[4096,8192]]}
```

每项是资源相对 byte offset；每批至多 32 项，至多 4096 批。保留原地址顺序和未对齐
源偏移，不缩放/排序输入。若有多个源资源不能混成一个 owner；此版本只接受由调用者
验证为单资源的序列。source_sha256 是调用者提供的来源声明，不是工具自行验证的 RAW
签名。代表回执序列不包含所有 draw/请求/VM 布局，不能冒充完整 Execute trace。

最终游戏同帧、复杂场景调度、GPU/驱动/WGC 干扰以及四档认证仍需真实游戏验证。
日常方向筛选可先自动跑该工具，只有候选局部效果稳定后才请求联合游戏短测。

单次查询成本另用 `--mapping-sweep --heap-mib 192 --mapping-repeats 96`（仍需 `--name`）。
该模式不接受 workload/threads/batches/rounds/descriptor 参数，不跑其他 case。
保留三个各 1–256 MiB 的自有分配：D3D12 upload Map、VirtualAlloc 普通 WB 和 WC 对照，
总分配上限 768 MiB；不改动游戏或驱动拥有的页保护。全部初始化后，单线程在三类分配的
八个位置交错查询，预热两轮，再循环轮换测试顺序；每 24 轮每项占据每个顺序位置一次，
下一组反向。重复数 24–240 且须是 24 的倍数。报告单独 schema `dm-local-mapping-sweep/v1`，
保留每条真实回执、查询耗时、线程周期差及 CPU 端点编号，不按慢/快删样本。
周期差含计时调用开销且不是纳秒，不换算成等待时间；CPU 端点相同也不能排除抢占/迁移。
此模式无并发/渲染/锁竞争，适合隔离区域大小与内存类型相关性，不证明游戏历史慢调用根因。

另有尚未完成运行验证的 `--page-query-compare`（同样使用 `--heap-mib`、
`--mapping-repeats`，与 mapping/hot-path 模式互斥）。它只在自有分配上比较当前单页的
`QueryWorkingSetEx` 元数据与原始 `VirtualQuery` validator；跨页、无效/未知属性等回退原检查。
先用私有 fixture 检查 guard/noaccess/decommit/保护切换，不改 D3D12 映射保护。
实验结果不能直接授权字节读取，字节验真仍依据原 validator 和保留的所有权。
**当前仅编译通过，安全预检和性能对照尚未执行，未接入生产 reader。**

2026-09-18 阶段结论、负收益实验、位姿反例与数据交接入口见
[CP2077 交接说明](../pr735-cp2077/HANDOFF-20260918.md)。这不是性能或四档认证通过声明。

性能统计的单测与普通 probe/writer 测试使用各自的计数器和环，均编译同一份
`src/perf/state.rs` 实现；测试内的 worker 仍共享该测试的统计状态。这样可直接并行跑
`cargo test -p dm-depth-probe --lib`，不会把别的测试的 EXR 耗时混进性能断言。

## B0 DSV 诊断

在启动游戏进程前设置：

```powershell
$env:DM_DEPTH_PROBE_DSV_AUDIT = '1'
$env:DM_DEPTH_PROBE_OUTPUT_DIR = 'D:\dm-depth-e2e'
$env:DM_DEPTH_PROBE_MAX_FRAMES = '900' # optional deterministic Lane-30 run
```

`DM_DEPTH_PROBE_MAX_FRAMES` is optional. A positive value stops new readbacks after that many
accepted frames while the Present hook continues draining in-flight work. For a Lane-30 60-second
acceptance run, use `900`, wait for the index to reach 900 rows, and then exit the game.

注入后 probe 会从真实 swapchain 取得游戏的 immediate context，在该 context 的 vtable
上安装 `OMSetRenderTargets` 与 `ClearDepthStencilView` hook，并把去重后的 DSV 纹理描述
写入 `%TEMP%\dm-depth-probe.log`。不设置此变量时只运行 P1a 的 Present/水印路径，
不会承担 DSV 枚举开销。

设置 `DM_DEPTH_PROBE_DSV_AUDIT` 也会启用 P1c/P1d：主场景 DSV 每次解绑时写入私有
GPU 环形快照，在 `Present` 前把选中的快照拷进四槽 STAGING 池，并把就绪帧异步落到
`DM_DEPTH_PROBE_OUTPUT_DIR` 下的 `raw/*.exr` 与 `depth-index.jsonl`。输出目录未设置时
默认为 `%TEMP%\dm-depth-probe-output`。

真机时序诊断可设置 `DM_DEPTH_PROBE_UNBIND_FROM_END=1..12`，选择读取帧尾倒数第 N 次
主 DSV 解绑快照；设为 `0` 时连续帧轮询 1..12。probe 每 300 次 Present 会把该帧实际
解绑次数与当前 N 写入日志。BioShock Infinite 真机每帧稳定解绑 12 次：N=1..3 是帧末
清空，N=12 是早期局部 pass，N=4/5 是完整主场景，因此默认使用 N=4。

当前 writer 写 `depth-index/v6`：每行 `pass` 会持久记录实际 `unbind_ordinal_from_end`、
`unbind_total`、该 pass 的 `draw_calls`，并用 `dsv::rank` 对账 operator 选择。实际选择
不是 top 只告警并留证据；首二名 margin 过低或观察到任何 MSAA 时交付门 fail-closed。
投影配置仍是 run 级常量，写作 `proj_scope=assumed_constant_for_run`，在逐帧常量缓冲的
`observed_this_frame` 接通前不得冒充米制可交付数据。

BioShock Infinite 真机 B0 结果：主场景 DSV 为 1920×1080、`SampleDesc.Count=1`；
另观察到 2048×2048、`SampleDesc.Count=1` 的深度资源。因此 P1c 可以沿用
`CopyResource → STAGING → Map` 路线，无需 `Texture2DMS` shader 解析。
