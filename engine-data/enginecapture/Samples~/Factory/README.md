# Samples~/Factory · 通用 GameObject 场景采集层

Megacity 版（`Samples~/Megacity`）绑定 ECS 世界与车流；本目录是**普通 GameObject 场景**
的采集层，首个宿主是 Factory Training（Asset Store 344832），换任意 URP/HDRP 场景工程
只需改 `DIOGENES_SCENE_PATH` 与摆位参数。

执行记录与踩坑见 [BRINGUP-FACTORY.md](../../BRINGUP-FACTORY.md)。

## 组成

| 文件 | 作用 |
|---|---|
| `Runtime/FactoryClipCapture.cs` | 一条 MC clip 的采集编排（McClipCapture 的去-ECS 版） |
| `Runtime/NpcCrowd.cs` | 动态物体铺设器 + `IDynObjectSource`（docs/70 §4.3 的无-NavMesh 实装） |
| `Tests/McClipCaptureTest.cs` | PlayMode 采集入口。**类名刻意与 Megacity 版一致**，好让 `capture-orchestrator/megacity_capture.py` 的 `-testFilter` 通道零改动复用 |
| `Editor/DiogenesProjectSetup.cs` | 空工程一次性建 URP 管线资产并设为默认 |
| `Editor/DiogenesStoreAcquire.cs` | Asset Store 资产获取（见下） |

## 场景 onboarding：先导占据图（必做）

新宿主场景的第一步不是采集，是**导出占据图**——相机轨迹的自由壳与 NPC 的防穿模
都读它。跳过这步就等于拿合成场景的占据图去约束真实几何，穿模防护形同虚设。

```bash
DIOGENES_SCENE_PATH=Assets/Scenes/<scene>.unity \
DIOGENES_OCC_OUT=~/Documents/<scene>-occ.json \
DIOGENES_OCC_EXTENT=25,25 DIOGENES_OCC_HEIGHT=6 \
/Applications/Unity/Hub/Editor/<ver>/Unity.app/Contents/MacOS/Unity -batchmode \
  -projectPath ~/Documents/<project> \
  -executeMethod Diogenes.Capture.Factory.Editor.SceneOccupancyExporter.Run \
  -logFile /tmp/occ.log
```

产物即 trajgen `Scene.from_dict` 口径，直接喂 `--scene-file`（编排器会自动读
`world_offset` 做恒等摆位，并把同一张图传给采集侧的 NpcCrowd）。

**为什么不用 Collider 建图**：实测 Factory Training 有 12711 个 Renderer 却只有
98 个静态 Collider（docs/71 R11 的实例）——按 collider 建图得到的是一张 0.2% 占据率
的空图，穿模断言全部失真。改用 `Renderer.bounds`（可见即障碍，AABB 近似偏保守，
对防穿模是安全方向）+ NavMesh 三角化定地面与推荐中心。

**一处走错留档**：首版把 NavMesh 未覆盖的列整列封死，自由空间从 2500 m² 塌到
**48 m²**，trajgen 直接「找不到可行的四路共线起点」。**不可行走 ≠ 有障碍**——
NavMesh 常常只烘焙玩法区域。封列现为 `DIOGENES_OCC_SEAL_NONWALKABLE=1` 开关，默认关。

## 宿主工程装配

1. `-createProject` 建空工程；manifest 加 `com.diogenes.enginecapture`（file 依赖）、
   URP、`com.unity.test-framework`。**保留全部 `com.unity.modules.*`**——裸 manifest
   会剥掉内建模块，Audio 缺失即编译失败（BRINGUP-MEGACITY 踩坑 6）。
2. 本目录三层拷进 `Assets/DiogenesCapture/`（Editor 脚本放 `Assets/Editor/`）。
3. `-executeMethod DiogenesProjectSetup.CreateUrp` 建管线资产。
4. 用编排器驱动（环境变量指向本工程）：

```bash
DIOGENES_UNITY_BIN=/Applications/Unity/Hub/Editor/<ver>/Unity.app/Contents/MacOS/Unity \
DIOGENES_UNITY_PROJECT=~/Documents/<project> \
DIOGENES_SCENE_PATH=Assets/<...>.unity \
python3 engine-data/capture-orchestrator/megacity_capture.py batch --count 5 \
  --runs-root <runs> --scene open_plaza --world-offset x,y,z
```

`DIOGENES_SCENE_PATH` 为空时在测试空场景里跑（链路冒烟用，画面为空）。
场景自带的 Camera/AudioListener 会被自动关闭，避免与采集 rig 冲突。

## 动态物体（NpcCrowd）

宿主场景静态（Factory Training 就是）时，docs/69 的「画面含较多动态物体 ≥80% 时长」
靠 `NpcCrowd` 铺设满足。**入画是结构性保证的，不靠运气**：agent 绕 `plan.json` 的
`anchors`（三路相机的共视注视区，随时间移动）做环带游走 —— 与 docs/78 §8
「锚点区落在车流走廊」同源，区别是那边搬相机就车流、这边搬人群就相机。

- 运动分层按 docs/69：普通走 / 跑 / 大幅度动作（原地转身+挥臂）三档，配额可配，
  实际计数写进 provenance（诚实入账）。
- 视觉体：给了预制体就用真模型，没给就程序化拼装（安全帽工人 / AGV）——
  与 Megacity 的 `CarrierVehicleSpawner` 同思路：画面里必须真有东西，
  不能只让几何投影断言过。
- 穿模防护：移动前向射线预检，命中即掉头并收缩环带半径。
- **诚实边界**：这些是我方注入的程序化 agent，不是宿主场景的原生角色，
  provenance 里逐字写明（`procedural NpcCrowd orbiting plan anchors (not native scene actors)`）。
  生物类目（docs/69 配比 40%）在工厂语境下无合理对应物，本层不伪造。

环境变量：`DIOGENES_NPC=0` 关闭；`DIOGENES_NPC_HUMANOID` / `_VEHICLE` 调数量；
`DIOGENES_GROUND_Y` 给地面高度（锚点 y 是相机高度，人脚要落地）；
`DIOGENES_NPC_HUMANOID_PREFAB` / `_VEHICLE_PREFAB` 指定宿主场景的真模型。

三处实测教训（都是先翻车再修，详见 BRINGUP-FACTORY §4.1–4.2）：
四肢要挂**关节 pivot**（直接转视觉体会绕自身中心转，手臂横在身前）；
环带半径用**绝对区间**（按锚点半径缩放会把人群堆在镜头前把场景挡死）；
**窄画幅要按 `tan(hfov/2)` 收近环带**（9:16@24mm 水平视场仅 31°，不收近则
`dyn.active_frac` 必然打回 —— 与 docs/71 §10.1 缺陷 8 同一个公式）。

## Editor/DiogenesStoreAcquire.cs

Asset Store 资产的获取通道（改 `ProductId` 即可用于清单里其它 `kind: assetstore` 条目）。
要点（实测，见 BRINGUP-FACTORY §3）：

- **必须 GUI 模式跑**：`CloudProjectSettings.accessToken` 在 batchmode 下恒为空
  （实测 90s 轮询始终 `tokenLen=0`），GUI 下 1s 内就绪。
- 端点取自 `AssetStoreRestAPI` 常量，不要猜：
  `/-/api/purchases`、`/-/api/product/{id}`、`/-/api/legacy-package-download-info/{id}`。
- 免费资产**无需先在网站 "Add to My Assets"**——未持有状态下 download-info 依然 200。
- token 只在编辑器进程内使用，不打印、不落盘；日志里下载 URL 的 query 已抹除。

## 许可纪律

宿主场景的许可决定产出数据的去向。Asset Store EULA / UCL 场景一律
`*-internal-validation-only` provenance 标记，**产出永不对外交付**
（docs/70 R1 收紧版：4 路深度+位姿可重建几何 ⇒ 按最严口径评）；
Apache/CC0/CC-BY 等干净场景走 `*-deliverable*` 标记进交付面。
分档见 [docs/96](../../../DataMiner/docs/96-unity-multicam-scene-sourcing-survey.md)
与 [engine-data/scenes](../../scenes/README.md)。

**本采集层不持有任何许可默认值**（[docs/106](../../../DataMiner/docs/106-warehouse-provenance-license-correction.md)）：
`LicenseId` / `License` / `Attribution` 由调用方从 `scenes/manifest.json` 的场景登记
注入（编排器 `--scene-id` 一步到位，或手工导 `DIOGENES_LICENSE*`），留空则
`Run` 拒绝出片。这条约束是拿事故换来的——标记曾写死在本文件里，工程复制到
Apache-2.0 的 Robotics-Warehouse 时被一并复制，三批数据都标错了许可。
把工程复制到新宿主场景时，需要改的是 manifest 登记，不是这里的代码。
