# 研究侧起草的多游戏 dev catalog（待 DataMiner 验证）

## 目录分四层：可采集、预检入口、起草物、研究快照

本目录下的 bundle 不是同一类东西，混用会直接造成操作事故
（[docs/54](../../../docs/54-control-plane-reconciliation-and-drift.md) 记录了一次实例）：

| 层 | 含义 | 判别方法 |
|---|---|---|
| **内置清单** | 编入客户端（登记于 `config/catalogs/shipped-games.json`，client build.rs 派生 `include_bytes!`）；`admitted` 可由生产 admission 放行，`onboarding` 在普通客户端保持不可选、只允许 `local-catalog-only` D7 口味试采 | 权威判别是 shipped-games.json 的条目与 admission；逐游戏 Cargo feature 已于 2026-08-23 退役 |
| **预检入口（已编入）** | 操作员可选，但 D6/D7 尚未完成，只能产出 non-trainable 本机预检数据 | 目录下有 `PREFLIGHT_ONLY.md`，写明待回填身份、mapping 或标定事实 |
| **起草物（未编入）** | D6/H-1 未完成，客户端里不存在，**不可采集** | 目录下**有** `NOT_COMPILED.md`，写明缺什么、编入前置 |
| **研究快照** | 故意不编入 Rust，只作调研留存与 `test_new_game_catalogs.py` 的并集词表输入 | 目录下**有** `SUPERSEDED.md`，写明取代者与理由 |

> 历史教训（docs/64 §2.1）：旧判别法只有两层（「无 `SUPERSEDED.md` = 可采集」），会把
> 三个未编入的起草物误读成可采集。sidecar 只是速查；**「能不能采」的事实源永远是
> Rust 侧登记**（feature + `include_bytes!` + validator）。

当前三个研究快照：`bioshock-infinite-steam-release-test/`、
`wolfenstein2-steam-release-test/`、`halo-mcc-steam-release-test/`。
每个的退役理由见各自的 `SUPERSEDED.md`。

当前预检入口：`halo-infinite-steam-release-test/`、`halo-reach-mcc-s0/`、
`elden-ring-nightreign-steam-release-test/`、`cyberpunk-2077-steam/`。四者已由独立 preflight
feature 编入 All Games DEV 客户端，但 D6/D7 尚未定稿；各自待回填事实见
`PREFLIGHT_ONLY.md`。Cyberpunk 当前只声明 RGB/on-foot，绝不暗示 D3D12 Depth 已支持。
当前没有仍带 `NOT_COMPILED.md` 的未编入目录。

> **为什么用 sidecar 文件而不是 `catalog.json` 里的字段**：`CatalogManifest` 带
> `#[serde(deny_unknown_fields)]`，往 `catalog.json` 里加 `superseded_by` 会让整个 bundle
> **无法加载**（已用 `catalog-validator` 实测：`unknown field 'superseded_by'`）。五文件是
> 字节钉死的，标注只能走 sidecar；加载器按固定文件名读取，多余文件被忽略（同样已实测）。

## DataMiner 本机实证 authoring

`age-of-empires-4-steam/` 是独立于下方研究快照的新 D6 bundle：钉住 Steam build
`24231237` 的 executable exact hash、真实窗口类与 1920×1080 borderless/SDR 设置，
并把 mapping 限定在已由本机教程、配置和官方当前快捷键资料核对的 RTS 基础任务子集。
它保持 `dev_non_trainable`；详见
[D6 本机证据](../../../docs/40-d6-age-of-empires-4-profile-evidence.md)。D7 WGC/Raw Input/
encoder smoke 与两类治理批准完成前，不得放入 published fleet manifest。

`euro-truck-simulator-2-steam/` 是 Steam build `23966373` 的本机 D6 bundle：钉住
`eurotrucks2.exe` exact hash、`prism3d` 窗口类与 1920×1080/30 fps borderless SDR
设置，mapping 仅覆盖单人模式驾驶室视角正常行驶。它保持 `dev_non_trainable`；详见
[D6 本机证据](../../../docs/81-d6-euro-truck-simulator-2-profile-evidence.md)。D7、深度标定、
本地离线配对和治理批准完成前，不得放入 published fleet manifest。

`assassins-creed-iii-remastered-steam/` 是 Steam build `11324511` 的本机 D6 bundle：钉住
`ACIII.exe` exact hash、两次一致的 `ScimitarEngineWindowClass`，以及第二轮实测
1920×1080 SDR borderless 客户区。mapping 仅覆盖随该 build 分发且与运行后本地配置逐键一致的
单人徒步核心控制；未注释、未持久化的控制不外推。详见
[D6 本机证据](../../../docs/155-d6-assassins-creed-iii-remastered-onboarding-evidence.md)。该条目在
`shipped-games.json` 中保持 `admission=onboarding`；D7、生产 admission 与独立 Depth
profile/binding 门完成前，不得声明 RGB/Depth Ready，也不得放入 published fleet manifest。

以下 dev catalog 由 [model_research/research/10-multi-game-collection-expansion.md](../../../../model_research/research/10-multi-game-collection-expansion.md)
的采集扩展调研起草；其中 6 个目录根据本机 Steam app manifest 与已落盘游戏主程序补充。
全部保持 `non_trainable: true`。v2 断代后 manifest 不再声明 `catalog_mode`，mode 由投递渠道决定
（见 docs/57 §3）：

| 目录 | 游戏 | Steam | taxonomy | 归属 |
|---|---|---|---|---|
| `halo-mcc-steam-release-test/` | Halo: MCC | 976730 | first_person_shooter | 微软一方 |
| `halo-infinite-steam-release-test/` | Halo Infinite | 1240440 | first_person_shooter | 微软一方 |
| `wolfenstein2-steam-release-test/` | Wolfenstein II | 612880 | first_person_shooter | 微软（ZeniMax） |
| `wolfenstein2-steam/` | Wolfenstein II（D6 exact identity authoring；D7 前不可 capture-approved） | 612880 | first_person_shooter | 微软（ZeniMax） |
| `wuchang-fallen-feathers-steam/` | 明末：渊虚之羽 | 2277560 | action_role_playing | 505 Games |
| `bioshock-infinite-steam-release-test/` | BioShock Infinite | 8870 | first_person_shooter | 第三方 Take-Two |
| `elden-ring-nightreign-steam-release-test/` | Elden Ring Nightreign | 2622380 | **action_rpg / third_person** | 第三方 万代/FromSoft |
| `a-plague-tale-innocence-steam-release-test/` | A Plague Tale: Innocence | 752590 | action_adventure / third_person | 第三方 Focus |
| `total-war-warhammer-iii-steam-release-test/` | Total War: WARHAMMER III | 1142710 | real_time_strategy | 第三方 SEGA |
| `hades-ii-steam-release-test/` | Hades II | 1145350 | action_roguelike / isometric | 第三方 Supergiant；本机身份已验证 |
| `fallout4-steam-release-test/` | Fallout 4 | 377160 | action_role_playing / first_or_third_person | 微软（ZeniMax）；本机 EXE、设置与窗口类已验证 |
| `cyberpunk-2077-steam/` | Cyberpunk 2077 | 1091500 | action_role_playing / first_person | 第三方 CDPR；本机 RGB/on-foot 预检，窗口类与 D7 待补 |

每套 5 文件（v2 断代把 capture/settings 两册并入 `recording-profile.json`，docs/57 §3.1），
schema 严格对照 `cod-steam-release-test/`；`catalog.json` 的 `content_hash`
已与同级文件精确字节 SHA-256 **自洽**（`ml/player/tests/test_new_game_catalogs.py` 与
`ml/player/tests/test_local_steam_catalogs.py` 校验）。
语义词表对齐调研 §6.1，`action_registry.build_vocab([...])` 可直接消费产出并集词表。

## BioShock Infinite 的两种测试用途

`bioshock-infinite-steam-e2e/` 是 BioShock Infinite 的权威条目（登记于
shipped-games.json，随 dev-catalog 内嵌），**钉了本机实测的精确 EXE SHA-256**，
用于重复真机发现与绑定验收。

`bioshock-infinite-steam-release-test/` 继续保留为研究快照；All Games DEV 复用权威的
`bioshock-infinite-steam-e2e/`，因此下拉框不会
出现两个 BioShock。

窗口身份已由两条独立路径互证，无需再测：`BioShockInfinite.exe` /
窗口类 `LaunchUnrealUWindowsClient`（PR #157 本机实测；PR #212 另一次三启动复测一致）。
已回填进本目录的 release-test game profile（v2），使其不再因 `window_class: null` 而
无法通过 `validate_game`。

## ⚠ 起草物，非认证——DataMiner 侧待办

这些是**研究侧的提案快照**，接入采集前需 DataMiner owner 逐项核实
（BioShock 的第 1、3 条已由上一节结清）：

1. **可执行名 / 窗口身份**：`executable_names` 是最佳猜测（如 `MCC-Win64-Shipping.exe`、
   `NewColossus_x64vk.exe`、`nightreign.exe`），需在真实采集机上按进程名/窗口类核对；
   `executable_sha256` 留 `null`（release 测试口味不钉，同 COD）。Hades II 已于
   2026-07-28 完成例外核实：Steam build 23661331 实际启动 `Ship/Hades2.exe`
   （file version 138517，Authenticode 有效），窗口类 `The Forge`，物理客户区与游戏设置
   均为 2560×1440；其 game profile 已钉精确 EXE SHA-256，且不再允许缺省窗口类。
2. **默认键位**：`bindings` 是各游戏**默认预设**的合理还原（依调研的控制方案 + 标准 PC 默认），
   需对照游戏内设置界面截图逐键复核；scan_code 用 PS/2 Set-1 make 码，与 game-profile 的
   VK 一致性已由测试锁定。操作员自定义键位须发新 mapping 版本（同 COD 约定）。
3. **Rust 编入**：15 个非 COD 目录已由逐游戏 feature 在客户端 Rust 侧显式登记
   （`include_bytes!` + 精确 hash/交叉引用校验）；All Games DEV 只组合这些 feature，新增目录仍必须显式登记，
   不会从磁盘自动发现。
4. **游戏特定采集注意**（详见调研 §5）：
   - **Wolfenstein II**：Vulkan-only → capture 用无边框窗口，早测采集路径；纳粹意象内容门。
   - **Elden Ring Nightreign**：EAC-off（社区 toggler）→ 强制离线采集；**第三人称、画面含 Avatar**；
     `camera.yaw/pitch` 是环视/锁定-相对，**语义 ≠ 瞄准**（训练侧按 taxonomy 分条件，见调研 §6.1）。
   - **Halo Infinite**：限离线战役 / bot Academy 采集（多人内核 EAC）。
   - **合规**：微软三连（Halo/Wolfenstein）走既有合作；第三方（BioShock/Take-Two、
     Elden Ring/万代）授权已按完成处理（owner 裁决 2026-08-02，法务跟进剩余文书），
     不再阻塞采集与训练——但 `non_trainable` 位的晋升仍走治理审批，不因授权自动置位
     （docs/64 后记 P-6）。

## ⚠️ halo-mcc 是六合一映射，S0 不得使用

`halo-mcc-steam-release-test/` 的 mapping 是 MCC **六款游戏的最小公倍数**，对任何单作品采集
都至少有一处错误：

- `move.sprint`：CE/H2/H3/ODST **根本没有冲刺**；Reach 的冲刺是**护甲能力之一**（非独立键）
- `LShift` 的语义逐作不同：H3 = 使用装备｜Reach = 使用护甲能力（**还随 loadout 变**）｜H4 = 冲刺
- MCC 的键位、V-Sync、FOV、画质**全部按作品分页**，不是一套配置

**S0 采集请用单作品条目 `halo-reach-mcc-s0/`**（依据与配额修订见
[research/halo/s0-mcc-primary-revision.md](../../../../model_research/research/halo/s0-mcc-primary-revision.md)）。
`halo-mcc` 保留仅供跨作品泛化研究参考。

注：`halo-reach-mcc-s0` 与其它研究侧草案一样，`window_class`/`executable_sha256` 为占位 `null`，
（历史：从前 `make_release_catalog.py` 会据此**拒绝**把它晋升为 first_party_release，#226 的就绪闸门。那条车道与那个脚本都已随 docs/57 §3.3 退役。）
必须先由 H-1 在真机上取到真实窗口类与 exe 哈希。

## 2026-08-20 七款本地 RGB 首版

`avatar-frontiers-of-pandora-pc/`、`dying-light-pc/`、`far-cry-5-pc/`、
`forza-horizon-5-pc/`、`gta5-legacy-pc/`、`indiana-jones-great-circle-pc/` 与
`titanfall-2-pc/` 是 All Games DEV 的本地五文件 bundle。它们统一为
1920×1080/30 fps、process-loopback 音频、任意窗口模式、无 depth。GTA V Legacy 已在
2026-09-02 钉住 Steam build `24129523` 的 `GTA5.exe` 精确 SHA-256，但窗口类仍未完成两次
独立启动复核；其余六款仍按 owner 决定不约束 EXE SHA-256 与窗口类。七款都保持
`dev_non_trainable`，等待有游戏机器上的 D6/D7 真机验收。单独钉住 GTA 主 EXE 不构成
窗口身份、D7 或 Depth Ready 证据；脱敏安装快照见
[exact-install evidence](../../../docs/evidence/gta5-legacy-exact-install-20260902.json)。

首版只覆盖各游戏已经确认的核心玩法子集；Ikran/SID、buggy、Far Cry 载具、Forza
UI/自由镜头、GTA 载具/飞行/潜水器/降落伞/手机/Online、Indiana 船只和 Titanfall
Titan 控制均明确排除。完整证据等级、逐游戏 mapping 范围、上下文冲突策略与试采门槛
见 [docs/143](../../../docs/143-seven-local-games-rgb-v1-scope.md)。

## 2026-08-20 三款本地 RGB 首版

`prey-2017-pc/`、`red-dead-redemption-2-pc/` 与 `starfield-pc/` 是同批追加到
All Games DEV 的本地五文件 bundle。统一为 1920×1080/30 fps、process-loopback
音频、任意窗口模式、无 depth，并明确不约束 EXE SHA-256 与窗口类。

mapping 均从 `D:\workspace\新游戏导入` 的游戏内截图收敛：Prey 只取主战役徒步，
RDR2 只取单人徒步且因无法证明默认键位而要求真机先复核，Starfield 只取徒步探索/
战斗。骑马/载具、飞船、DLC、在线、建造、照片和菜单等宽口径输入全部排除；详细范围、
E 键载具污染策略和 D7 门槛见
[docs/144](../../../docs/144-three-local-games-rgb-v1-scope.md)。
