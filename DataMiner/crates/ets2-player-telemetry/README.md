# ETS2 / ATS player telemetry v1

`ets2-player-telemetry/v1` 是 ETS2(P-A1)与 ATS 的 `player_*` 侧地基,
对应 [98 号调研](../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md) §5.7.1 的第 ①–⑤ 步。
定位与 `fallout-player-telemetry` 完全对称:**拥有契约与换算,不拥有插件与传输。**

## 它做什么 / 不做什么

| 做 | 不做 |
|---|---|
| SCS 欧拉角(圈)→ 世界旋转矩阵,四个基本方位钉死 | SCS 插件 DLL 本身(需 Windows + MSVC) |
| SCS 世界系 → 68 号交付世界系的轴变换 | 共享内存 / MMF 布局与非阻塞发布边界 |
| `player_position` / `player_rotation_quaternion` / `player_speed` 换算 | `action_camera.json` 组装、mouse 归一化、R2 投递 |
| 速度来源记账(真速度 vs 仪表)与降级标注 | 与配对表的 QPC 对齐(在 `depth-lab::ets2`) |
| **车内 `camera_*` 的三级复合与内参声明**(见 [`camera`](src/camera.rs)) | 采集/交付编排 |
| 越界与非有限值的 fail-closed 校验 | 真机验证声明——本 crate 一条都没有 |

拆这一刀的理由很实际:**上面这一列可以在 macOS 上写完并测完**,
下面那一列必须开 Windows 机器。把符号错误挡在前一列,就不用为了验证一个正负号去开机器。

## 官方依据(全部一手核验,2026-08-06)

依据是官方 SDK **1.14**(`scs_sdk_1_14.zip`,
SHA-256 `c6c1f7376b7324994d9f9c567f3c4141fbbf305b6bf803bc4cfeef2437b2023a`,
自 <https://download.eurotrucksimulator2.com/scs_sdk_1_14.zip> 取得)。

- **许可是 MIT。** `sdk_license.txt` 原文 "Permission is hereby granted, free of charge, …
  including without limitation the rights to use, copy, modify, merge, publish, distribute,
  sublicense, and/or **sell**",Copyright (C) 2016 SCS Software。
  ⚠️ 它覆盖的是 **SDK 代码**,不覆盖游戏画面——画面侧的授权面见 [99 号](../../docs/99-steam-depth-prereq-execution/README.md)。
- **坐标系**(`include/scssdk_value.h`):
  > In local space the X points to right, Y up and Z **backwards**.
  > In world space the X points to east, Y up and Z **south**.
- **heading**:`<0,1)` ↔ `<0,360)`,俯视逆时针,**0=北 / 0.25=西 / 0.5=南 / 0.75=东**。
- **pitch**:`<-0.25,0.25>` ↔ `<-90,90>`,正值朝上,0.25 指天顶。
- **roll**:`<-0.5,0.5>` ↔ `<-180,180>`,"counterclockwise when looking in direction of the roll axis"。

## 三个已钉测试的陷阱

1. **角度单位是「圈」。** 不是弧度也不是度。把度当圈传进来会以 `heading=90` 的形式出现,
   被 `EulerRangeError::Heading` 挡下(`out_of_range_angles_are_rejected_rather_than_wrapped`)。
2. **局部 Z 指向后方,前向是 −Z。** 照搬「Z 前」会让整批朝向反 180°
   (`local_forward_is_negative_z_not_positive_z`)。
3. **`truck.speed` 是仪表读数且带符号。** 官方逐字 "**Speedometer** speed … Uses negative value
   to represent reverse movement"。真实速度应走 `truck.local.velocity.linear`
   ——98 号 §5.7.1 只提了 `truck.speed`,**本轮改用 velocity 通道并把仪表降为兜底**
   (`true_velocity_wins_over_speedometer` / `missing_velocity_channel_degrades_explicitly`)。

另有一条不是陷阱但必须写清楚的语义:**ETS2 的 `player_*` 指「卡车」不是「人」**
(98 号 §6 争点 III 反方论点 2),交付说明里不能让读者以为是人体位姿。

## camera_* 走三级复合,不走 D3D11 hook

98 号 §5.7.1 原本把 `camera_*` 列为「D3D11 hook 取 view/projection 矩阵」,
三级复合 `world_placement ∘ cabin ∘ head` 只作交叉校验。[`camera`](src/camera.rs)
把两者对调:**复合是主路,hook 不做。**

理由是 [95 号](../../docs/95-camera-projection-binding-fieldguide.md) 的结论——
投影矩阵在无变焦时恒定,认矩阵这件事在 Fallout 4 上卡了整整一轮;而复合的三项
全部来自官方 MIT 通道,一个 hook 都不需要。代价写在明处:内参改由**采集前钉死的
FOV** 给出(95 号 §3.3 路线 D),因此采集期禁止切视角与改 FOV。

## 两条未关闭的约定

两者同形:**没有默认值,调用方必须显式选**,标定前用 `Unvalidated`
——它会原样进交付元数据,使符号/次序选错时数据仍可重算。

| 约定 | 差别在哪 | 什么时候显形 |
|---|---|---|
| [`RollConvention`](src/orientation.rs) | roll 的正负号 | 只在侧倾时;平地上永远看不出来 |
| [`OffsetComposition`](src/camera.rs) | 偏移是左乘还是右乘 | 只在偏移带旋转时;5° 的读法差 → 相机位置错十几厘米 |

`OffsetComposition::Unvalidated` 下,任何超过 5 mm / 0.18° 的偏移都会被**拒绝**
而不是按某一种读法算出来。所以采集前必须 `g_cam_physics 0` / `g_cam_steering 0` /
`g_cam_blinker 0`,采集中不做头部环视。

## roll 符号标定(唯一未关闭项)

官方那句 "looking in direction of the roll axis" 有两种都说得通的读法,差一个正负号,
而且**平地上永远看不出来**(roll≈0)。所以 [`RollConvention`] 没有默认值,调用方必须显式选,
标定前用 `Unvalidated`——它会原样进交付元数据,使符号选错时数据仍可重算。

**实机标定步骤**(与 98 号 §5.7.3 给 DCS 3×3 基向量定的做法同形):

1. 找一段**已知侧倾方向**的路面(环形赛道弯道内侧、或直接把卡车开上路肩单侧抬升);
2. 同帧记录 `truck.world.placement.orientation.roll` 与一张截图;
3. 判据:车**向右倾**(驾驶员右肩下沉)时 `roll` 的符号是正还是负;
4. 对照本 crate:`RollAxisIsForward` 下,正 roll 表示绕前向轴逆时针
   ——从驾驶员视角看是**向左倾**。若实测「右倾 = 正 roll」,则应选 `RollAxisIsLocalZBackward`;
5. 结论落进本 README 并把调用处从 `Unvalidated` 改成实测值,同时评估既有数据是否需重算。

标定所需时间约 10 分钟,与 99 号 §9 动作 3 的那次装机合并做即可。

## 与地图边界的衔接

`depth-lab` 的 `SectorGrid::to_delivery_bounds_m()` 输出的边界**与本 crate 的
`position_m` 同一个坐标系**,所以可以直接用 `WorldBounds::contains_xz` 做落域闸
(82 号 §5 那条纪律:有点落在边界外就说明边界值或原点约定错了,**值作废**,不是把点裁进来)。

## 尚未接上的上游

本 crate 的输入 `RawTruckSampleV1` 目前由调用方构造。真实链路的两侧都已落库:

- **上游**:[`ets2-telemetry-plugin`](../ets2-telemetry-plugin/README.md) 订阅
  [`channels`](src/lib.rs) 里那五个通道并逐帧写出 `ets2-telemetry-frame/v2`
  (含 `host_qpc` 对齐键与两级相机偏移);
- **下游**:`depth-lab --features ets2` 的 [`ets2`](../../tools/depth-lab/src/ets2.rs)
  模块按 QPC 把遥测对齐到配对表,产出 `deliver --pose` 吃的位姿旁车。

**仍然缺的只有实机验证**:插件从未加载进真实游戏进程,两条约定
(roll 符号、偏移复合次序)都还没标定。见 `tools/pr405-ets2/README.md`。
