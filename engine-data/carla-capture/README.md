# carla-capture · CARLA 版 Multi-Camera 采集

CARLA 路线的采集脚本。为什么单开一条线而不是把 Unity SDK 移过去：
**我们在 Unity 侧费力实现的东西，CARLA 全是原生能力**——4 路同帧、
view-space 线性深度、逐帧相机位姿、内参可设。它本来就是为生成带真值的
合成数据设计的。所以这一侧只做三件事：轨迹驱动、按 docs/69 schema 落盘、
对接 `engine_qc`。

## 许可（这条线的最大价值）

🟩 CARLA README 原文核实：
> "CARLA specific code is distributed under MIT License."
> "**CARLA specific assets are distributed under CC-BY License.**"

⇒ **渲染输出可对外交付**，义务是把 CC-BY 署名传递进交付文档
（脚本已把 attribution 写进 provenance.json）。

调研与分档见 [docs/96](../../DataMiner/docs/96-unity-multicam-scene-sourcing-survey.md)。

## 平台前置

**CARLA 不支持 macOS**——只有 Windows 10/11 与 Ubuntu 20.04/22.04，
且需要 NVIDIA RTX 2070 级别 GPU。本机（Apple Silicon）跑不了，
需要一台带 GPU 的 Linux 机器。实测配置：GCE `g2-standard-8` + NVIDIA L4
（us-central1-c；us-central1-a/b 常 STOCKOUT，换区即可）。

### 上云三个坑（都实际踩了，按顺序会省一小时）

1. **OS Login 在外部组织账号下不可用**：`enable-oslogin=TRUE` 会因缺
   `roles/compute.osLoginExternalUser` 直接 SSH 失败。建 VM 时设为 FALSE 走密钥。
2. **安装脚本会随 SSH 断连被杀，且日志停在第一步却报 exit 0**——这是 GCE 上
   长跑的老问题（linger 是逐机属性）。**一切超过几分钟的步骤都要
   `nohup ... &` + 轮询标记文件**，不要靠前台 SSH 跑完。
3. **NVIDIA 驱动 DKMS 编译失败**：Ubuntu 22.04 的内核 6.8 是 GCC 12 编的，
   而系统默认 `gcc-11` 不认 `-ftrivial-auto-var-init=zero`，
   `dkms status` 停在 `added` 而不是 `installed`（`nvidia-smi` 报
   "couldn't communicate with the NVIDIA driver"，很容易误判成没装）。
   修法：装 `gcc-12`/`g++-12` 并 `update-alternatives` 设为默认，再
   `dkms remove` + `dkms install`。

    ```bash
    sudo apt-get install -y gcc-12 g++-12
    sudo update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-12 60 \
        --slave /usr/bin/g++ g++ /usr/bin/g++-12
    sudo dkms remove nvidia/<ver> --all; sudo dkms install nvidia/<ver> -k $(uname -r)
    sudo modprobe nvidia && nvidia-smi
    ```

4. **L4 常 STOCKOUT**：us-central1-a/b 都拿不到，us-central1-c 可以。
   建 VM 失败先换区，不要急着换机型。

## ⚠️ 本目录不是完整复现链（2026-08-17 入库时如实登记）

入库的是 4 个脚本 + 本文。08-10 那批 `CarlaTown10HD` 交付实际还用到过三个
**只存在于当时那台 GCE VM 上、随 VM 一起没了**的胶水脚本，本机与仓库都已无副本：

| 缺失 | 作用 | 影响 |
|---|---|---|
| `gen_carla_plans.py` / `gen_pool.py` | 批量调 trajgen 出候选计划池 | 下方步骤 3 无法照抄，需按 trajgen CLI 重写 |
| `gate_hz.sh` | 按实测 `active_frac ≥0.80`、`too_close_frac ≤0.03` 择优成批 | 数值闸门需重写 |
| `carla_postprocess.py` | 修元信息 + 调 engine_qc + 出目检联表 + 打包 | 可用 `engine_delivery pack-mc` / `audit-mc` 替代大部分 |

四个入库脚本本身是自足的（server → 场景导出 → 回放 → 音轨），
缺的是**批量编排层**。重建它属独立工作，不在本次入库范围。
另见 [docs/147](../../DataMiner/docs/147-carla-town10hd-mc-acceptance-vs-doc69.md)
——那批数据本身已按 docs/69 判为不可交付，重采时这层编排要连同
「跑前比对 trajgen 版本」的自检一起重写。

## 用法

交付形态走的是 **`carla_mc_playback.py`（trajgen 烘焙位姿回放）**；
`carla_mc_capture.py`（相机挂 autopilot 载具）是首版，六项交付复核全打回，
只作为反例保留，别拿它出交付批。

```bash
# 1) VM 上：起 CARLA server（headless）
./CarlaUE4.sh -RenderOffScreen -quality-level=Low -carla-rpc-port=2000 &

# 2) VM 上：导出占据图（含行人导航网格可站面）
python3 carla_scene_export.py --out ~/carla-scene.json --extent 60

# 3) 本机：按 docs/69 配额生成计划（三个场景级覆盖都要给）
DIOGENES_ELEV_MAX=8 DIOGENES_ELEV_MIN=-16 \
DIOGENES_ANCHOR_GROUND_M=12 DIOGENES_ANCHOR_GROUND_N=10 \
python3 -c "..."      # 见 gen_carla_plans.py

# 4) VM 上：回放出片（4 路同帧 + 逐帧动态物体实测）
python3 carla_mc_playback.py --plan ~/carla-plans/plan-01 \
    --out ~/out/clip-01 --scene ~/carla-scene.json \
    --vehicles 160 --walkers 160
bash carla_audio_mux.sh ~/out/clip-01 60      # CARLA 不出音频，另配环境声轨
```

三个环境变量都是**场景属性**，不是全局默认值该背的：室外开阔城市要收
俯仰上下限（否则大量帧不是纯天空就是纯路面），要开锚点可站面偏好
（否则四路一起对着马路中央）。

## 同帧保证

`synchronous_mode=True` + `fixed_delta_seconds=1/30` + `world.tick()`——
4 路的 RGB 与 depth 在同一 tick 出数据，**帧对齐由构造保证恒 0ms**
（与 Unity 侧 FrameClock 同思路，不依赖事后校准）。

## 与 Unity 侧必须一致的一处

`fov_from_focal` 用的是与 `NpcCrowd.FovRadiusScale` 同一个公式
`tan(hfov/2)=12·(W/H)/f`——CARLA 的 `fov` 是**水平**视场角，而我方按 35mm
等效焦距排产、内参纵向等效。两条产线必须用同一个换算，否则同一份计划在
两边的画幅语义会不同。

## 轴向约定

按 CARLA 原生轴向出（左手系 x=forward, y=right, z=up），**不做轴变换**——
交付方要的是自洽的位姿 + 内参，多一层变换反而多一类打回面
（docs/70 辩题二「每省一层转换就少一类打回」的同理）。约定写进 provenance。


## 采集侧七层排查（每层都掩盖下一层，按顺序看能省很多时间）

这条线的坑不在环境而在采集时序，且**前六层都修对了却没解决问题**——
每一层的错误现象都指向错误的方向：

| # | 现象 | 真因 | 修法 |
|---|---|---|---|
| 1 | `terminate called ... std::runtime_error` 进程直接死 | 对已死 actor 调 `destroy` 抛的是 **C++ 异常**，Python `except RuntimeError` 接不住，把真异常一起吃掉 | `client.apply_batch([DestroyActor(...)])` 批量销毁 |
| 2 | 露出 `queue.Empty` | `sensor_tick=1/30` 与 `fixed_delta_seconds` 相同时，CARLA 按「最小间隔」处理，浮点误差让某些 tick 不出数据 | `sensor_tick=0`（每 tick 必出） |
| 3 | 吞吐 **0.13 fps**（60s clip 要 3.7 小时），GPU 却 0% | `save_to_disk` 逐帧 PNG 编码，瓶颈在 CPU | raw BGRA → ffmpeg stdin（同 Unity 侧 `FfmpegPipe`） |
| 4 | 写盘量比预期大一倍 | 采了 depth，但 **docs/69 的 MC 交付物是 12 文件、不含 depth**（那是 docs/68 SFM 单的要求） | 去掉 depth 传感器 |
| 5 | 仍 `queue.Empty` | 同步模式下传感器回调**异步投递**，队列里压着上一 tick 的帧，直接 get 会错位且再也追不上 | 按 `world.tick()` 返回的 tick id 对齐，丢弃早于本 tick 的帧 |
| 6 | 跑 200 帧上下必有 lane 停止投递 | **每帧对 sensor 调 `set_transform`** 会打乱其内部状态 | 相机 **attach 到载具**、移动载具（也正好对上 docs/69 的载具视角，物理约束免费） |
| 7 | server 段错误 `Signal=11` | 每条 clip 都 `load_world()` 重载地图——headless 下的高风险操作，批里每条都 load ⇒ 崩溃概率 ×N | 地图已是目标就用 `get_world()` 复用（也省几十秒） |

**给后来者的一条判断**：CARLA 报出来的错几乎从不指向真因。
遇到 `queue.Empty` 先怀疑时序（sensor_tick / tick 对齐 / set_transform），
遇到进程直接 terminate 先怀疑 actor 清理把真异常吃了。

## 内容层四层排查（时序跑通之后才开始的那一半）

时序修完只是「能出片」。**能出片和能交付之间还隔着四层**，而且这四层
全部是 `engine_qc` 测不到、只有目检或专门的实测才看得见的：

| # | 目检打回的现象 | 真因 | 修法 |
|---|---|---|---|
| 1 | 四路全程大幅俯视地面 | 位置换了轴、**四元数没换**。QC 的物理阈值查的是位姿序列自身的导数，轴装错了照样平滑 ⇒ 121 PASS / 0 FAIL | `quat_to_carla_rot` 先在 trajgen 系取 forward/up，再整体换轴 |
| 2 | 白橙路障全程占满画面 | dummy 载机自身的网格入镜（正是复核点名的「载机入镜」，只是从摩托车换成了路障） | dummy 沉 12m 埋进地下，相机 attach 时给同样大小的 +z 偏移抵消 |
| 3 | cam4 有 5/9 帧在公交车厢内部 | 可站面取自 `generate_waypoints` = **机动车道中心线**，相机被规划在车道正中；车流不会避让相机，而占据图只含静态几何 ⇒ 规划期躲不开 | 选址而非避让：相机放人行道 |
| 4 | 一整条 60s 只有石墙与空地 | 见下节 | 见下节 |

## 动态物体：为什么必须实测

CARLA 画面里的动态物体是 Traffic Manager 的车与 Pedestrian AI 的人，
而 `action_camera` / `plan` 里的 subject 是 **trajgen 排产用的虚拟主体**，
两者毫无关系。拿后者去证明 docs/69「≥80% 时长画面含较多动态物体」，
等于用一个与视频无关的量当证据——目检判某条空画面时，它是全绿的。

故回放时逐帧做视锥内计数，并按 `engine_qc` 认的 `dyn_metrics.json` schema
落盘（**名字不同就等于没测**：早期落的是自家命名的文件，QC 对这条规则
一直 SKIP）。判据取 `count > 0`，与 Unity 侧 `DynObjectProbe` 和已交付的
Robotics-Warehouse 批一致；`active_frac_ge3` 与视锥内均值作为补充证据留档
（长焦路取景窗小、天然偏低，如实记但不当闸门）。

装上实测之后，「怎么让画面里有东西」这件事走了四步才走对：

| 试法 | 结果 | 学到 |
|---|---|---|
| 行人 60 → 140 | 0.17 不变 | 密度不是瓶颈 |
| 行人改布到镜头扫过的取景体积 | 0.17 不变 | 也不是「没对准」 |
| 车 80→160、人 115→230 | 0.17 → 0.23 | 加人只有边际改善 |
| **锚点选址加可站面偏好** | 四路 **1.000** | 是选址问题 |

逐帧分布才说清楚：那一路是 **t=10–50s 恒为 0**、其余时段 60–99 的双峰。
恒零不是「少」，是镜头指向了一片什么都不可能有的区域——查视线落点发现
**锚点周围 12m 内可站面格数为 0**，那是马路/停车场，行人在导航网格上
根本到不了。而四路全部 look-at 锚点，于是四路一起对着这片空地。

两处配套修改：

- **可站面改采行人导航网格**。从 OpenDRIVE 找 sidewalk 车道只得 111 格且
  挤在一角（10 条计划的锚点全落在同一个 15×30m 里，反复拍同一片柱廊）；
  `get_random_location_from_navigation` 采样得 771 格铺满 120×120m。
  行人能站的地方就是可站面，且与「行人布点」用的是同一张网。
- **锚点选址按可站面格数打分**（`DIOGENES_ANCHOR_GROUND_M/N`）。
  做成硬过滤时一轮候选全落空会退回退化中心、成功率崩到十分钟出 1 条；
  只问「有没有可站面」则贴着大楼的人行道也过，而那里相机排不开、
  共视崩到 5%–22%。**数格数**同时表达了「有人能来」和「够开阔」，
  选出来的是广场。

## 批脚本的两条纪律

- **落盘后自查帧数**，短了非零退出。曾有一条被 simulator 中途超时打断、
  只写了 600/1800 帧，批脚本照常继续、报告照常写「10 条 ×60s」，
  是拉回本机 ffprobe 才发现的。
- **重试前先探 server**：连得上就判脚本级错误、整批立即中止。
  否则一个 `Transform`/`Location` 用错会让批脚本重启 30 次 server、
  跑半小时、0 条产出——把脚本 bug 当环境问题重试是纯粹的浪费。
