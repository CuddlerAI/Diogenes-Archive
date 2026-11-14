# CP2077 本地证据与诊断记录

2026-09-18 起 CP2077 的两件套已移入正式目录
`../catalogs/dev/cyberpunk-2077-steam/`（见该目录 README），本目录不再有单独的诊断清单。
这里保留 `local-depth-evidence.json`：正式 profile 的 `evidence_path` 与 `evidence_sha256`
都指向它，挪动或改写会改变 profile 的字节，进而让四档性能认证失效。

以下为诊断构建期间的计时与相机诊断记录，保留作历史参考；其中「仅诊断构建加载本清单」
「运行时仍要求 `performance_validation_only`」等说法已不再成立。

## Present 定点计时短测

descriptor 优化新包的慢调用为 v5（Present 仍 v6）：慢 copy 的 `descriptor_copy` 包含
prepare/wait/apply/residual 和最慢获取的 shard/phase/group，`completed=false` 表示未完整
应用；缺少 detail 的提前返回保留为未知，不补造。`draw_redundancy_check` 补上 selected
Draw 计时之前的整体判重开销，但不是纯等锁时间，也没有所有短判重的累计时长。
hot-path 新增 `descriptor_update_same_gpuva/reindexed`，看同 GPUVA 原位更新的真实命中
数量；仍更新 descriptor 代次/size，不代表跳过采集。分项/索引正确不能替代真机收益验证。
已确认 Ctrl+Alt+F10 的 Alt/Ctrl 会触发本机 CP2077 武器/闪避输入；现有短测最大尖峰早于
该输入，本次不更改热键或事后裁尾。下轮测量的封窗/收尾方式需提前明确，勿将此快捷键
视作对游戏无影响的操作。

2026-09-18 的可选计时输出为 `perf-frames.jsonl` v6，记录原始 Present、入口准入/
等锁、环提交和本帧退出；同 epoch/generation/swapchain 可跨线程关联。v5 仅约 4.5%
同线程覆盖的缺口已改为独占原子槽；不能用脚本虚补旧录制。使用现有“本地短测（约 30 秒）”勾选框，
开启 Depth 后从客户端启动并绑定游戏，再用主界面开始录制；该入口自动开启 A3。
先站定约 10 秒，再步行/转身约 20–30 秒，Ctrl+Alt+F10 结束，等待 Flushed。
这是完整链路的计时接线/复现短测，不是上一轮 watermark-only 档的同条件 A/B，
也不登记四档认证结果。未复现长停顿不能宣布修复；需要时再安排受控复现。

录后运行 `python tools/pr735-cp2077/Summarize-PresentTiming.py <perf-frames.jsonl>
--flushed-ack <本轮 probe-control/acks/0004-flushed.json>`；核对本轮 DLL hash、v6 schema、
`own_exit_records`、分解覆盖率和具体缺失原因，再看慢帧。线程变化本身不应再导致缺失。
脚本自动读同目录 `perf-slow-hooks.jsonl`，检查 dropped 数及时间对齐误差；≥2 ms 的
descriptor-copy/Execute/selected draw 只作为可能重叠线索；Execute 包括整体、准备、
原始调用和完成（存在嵌套），不能相加或推断 GPU 成本。
Execute 锁外页查询版仍输出帧 v6，慢调用升级为 v4（脚本仍兼容旧 v1/v2/v3）。检查 prepare 事件中的 `execute_prepare` /
`prepare_times_ms`，并核对 hot-path 文件的 `execute_prepare_detail_*`：尤其是真实读取与
复用数量、外层锁、descriptor 分片锁、内存读取和 residual。优化后慢调用可能少于阈值，
不能因为事件消失就认定没有成本；全提交聚合仍保留。descriptor 时间是 validation 子集，
不能重复相加。新 DLL 必须独立核验 hash、Depth/Camera/Player/Flushed，再谈帧时收益。
v3 起同时核对 `memory_query_calls/rejected`、`query_cache_hits/inserts/full_reads/ineligible_reads`
（后四项均为 `query_cache_` 前缀）。仅已有 observer 资源引用与嵌套 Map 的范围可在
同次 Execute 内复用成功查询；无所有权、未命中或缓存满仍正常查询，不少采数据。
优先看全提交实际查询数、命中率及 ineligible 占比，再看查询总墙钟与慢帧。若全部
ineligible，就说明此窄优化未覆盖真实来源，不能宣布优化奏效，也不能擅自扩大所有权范围。
包含下述对象相机旁路读取的新 DLL 不是旧固定包的单变量 A/B，须标明配置差异。
v4 另查 `query_prefetch_*`：锁外查询只是预备页属性，实际读取仍锁内重新验证身份/代次。
真实查询次数为 `query_prefetch_calls + memory_query_calls`，真实查询时间为
`query_prefetch_query_ns + memory_query_ns`；不能只看后者下降就报总查询消失。
预查询可以未被消费；其调用计数不能硬塞进 read_attempts。`query_prefetch_ns` 包含
`query_prefetch_query_ns`，不可重复相加。Draw 的等待/持锁计时和对应≥2ms慢事件已覆盖
此常规短测路径；先看等锁时间及尾部，再看 contended / (immediate + contended)。
prepare 为锁内收集、锁外查询、锁内复核/读取两段持锁，不跨 Execute 缓存、不改采样覆盖。
本轮相位调查还提示玩家 actor-root 实时 RPM 可能领先渲染 CBV 一个 Present，且有例外；
结构上的 Present/QPC 配对通过不能证明引擎状态同帧。未修正或放宽规则，当前仍仅用于
non-trainable 工程诊断，不得把性能短测通过解释为玩家同帧或正式交付通过。
同包新增 `cp2077-player-sample-timing.jsonl`，记录原有玩家 sampler 的实际起止时间与
原返回值，不额外读取、不更换帧号。录后核对正式索引和旁路 identity/姿态一致及丢失数；
采样调用时间不是引擎状态所属帧，具体字段见下方诊断契约。
输出保留重叠/缺证据的“无法拆分”区间，不视为零开销，不生成质量/认证裁决。
先完成这一轮计时接线短测再决定是否复现水印档，不直接重跑四档。

## 投影族与对象相机旁路诊断

后续新构建可在相同 feature + debug assertions 下额外输出
`cp2077-projection-families.jsonl`、`cp2077-memory-camera.jsonl` 与独立 summary，
均位于 Camera 索引目录。常规 holstered/dedup 路径不变，诊断数据不参与准入或交付。
投影族聚合保留代表/去重前计数及未知覆盖；内存读取只用已知 exact-build 玩家 components
RTTI 链与 RPM，基础 FOV 不是已验证的最终渲染 FOV。
字段、坐标、±1 Present 对照及短测安排见
[`CAMERA-COMPARISON-DIAGNOSTICS.md`](../../tools/pr735-cp2077/CAMERA-COMPARISON-DIAGNOSTICS.md)。
先核验新 DLL 实际加载 hash 和新文件 header；旧 Execute prepare 固定包不包含此实验。
本轮只加观测，是否简化留待用户看数据后决定，不能拿单族观测自动放宽规则。
