# engine-data-trajgen · MC 计划生成器 + M3a 排程仿真

规则依据 [docs/71](../../DataMiner/docs/71-multi-camera-line-detail.md) §4–6（原语库 / 共视与解耦 / 第四路）
与 §10（M3a 排程仿真）；口径快照见 [engine-data/README.md](../README.md)，
交付 json 的**键名以 [fixtures](../fixtures/) 为准**（`camera_Follow Offset` 带空格、MC 侧 `mouse_x` 不是驼峰）。

纯标准库，无第三方依赖（与 engine-data-qc 同纪律）。

```bash
# 生成一条 4 路计划并落盘次级文件夹（json 面；video 由 M3b 的 SDK 补）
python -m trajgen gen-clip out/clip-001 --scene open_plaza --seed 7

# 第四路形态 b：同区域高空垂直俯视伴飞（docs/69 §8.3，plan 写 cam4.mode="topdown"）
python -m trajgen gen-clip out/clip-002 --scene open_plaza --seed 7 --cam4-mode topdown

# 直接喂给 QC 复核
python -m engine_qc check-clip out/clip-001 --mode mc

# M3a 排程仿真（docs/71 §10，验收线：闸门全过率 ≥80%）
python -m trajgen simulate --n 200 --json out/m3a.json

python -m trajgen list-classes          # 23 类运镜与语料级配额
python -m pytest tests -q               # 96 项
```

## 管线

```
场景占据图(scene) ─┐
动态主体/锚点(anchors) ─┼→ 分段计划+配额(planner) → 原语(primitives)
                        │        ↓
                        │   节点采样 → 限幅 → 自由壳投影 → 五次 B 样条(sampler)
                        │        ↓
                        │   共视窗口收敛 → 粉色诚实性修正场(pink_honesty)
                        │        ↓
                        └→ 第四路：ρ(t) 时间重参数化 + 保速换向粉色窗(cam4/pink·形态 a)
                           或 高空俯视伴飞(cam4_topdown·形态 b)
                                 ↓
                    生成侧自检(metrics/covis) ──失败──→ 层级化重排
                                 ↓过
                          导出次级文件夹(export)
```

## 几个非显然的构造决定（都是实测踩出来的）

| 决定 | 为什么必须这样 |
|---|---|
| 位姿全程走**节点 + 五次 B 样条**（`KNOT_S=0.3s`） | C⁴ ⇒ 段间 C² 是推论，不必逐段解七段 S-curve；jerk 量级由 `Δ³p/h³` 定，节点间隔就是主要杠杆 |
| 端点用 `clamp_ctrl` 复制 **5** 个 | 五次样条要重复「次数」个端点才严格插值端点值；复制 2 个时首帧位姿差 ~22%，QC 的 `init.orientation` 直接挂 |
| 反查/单调函数用 `linear_ctrl` + `linear_eval` | clamp 会把端点斜率压成 0；且线性外推有 3 个控制点的参数偏移，映射错了弧长起点就偏 |
| 第四路走**弧长重参数化**（`ArcPath`）并归一化两遍 | 只有 `|dP/ds|≡1` 才让 `v₄=ρ·v_g` 成为恒等式。按时间参数化 + 反查 τ(s) 会在标称停步处翻车；一遍归一化残留的几个百分点涟漪会把 QC 的「变速恰 1 次」劈成 2 次 |
| guide 与第四路标称只用 `EXTENT_DRIVEN` 类目 | `follow_*`/`target_switch` 位移由主体决定、速度不可控；guide 一慢 `ρ=v₄/v_g` 的分母趋零，噪声被 QC 判成额外变速 |
| ρ 取 `[0.33,0.60]∪[1.65,3.0]`（需求方禁带是 0.65–1.5） | 贴带边取值时测量噪声会让 ρ 在带边反复进出，「禁带驻留」与「变速恰 1 次」都被打挂 |
| Δρ 与 Δ 要过 `metrics.rho_detectable` | QC 的检测器比的是前后 0.5s 中值的对数比（阈 0.223）。Δ 取 3s 而 Δρ 偏小时，申报的变速在检测器眼里根本不存在 |
| 四路**沿同一方位线**按焦距半径排开起步 | 同时满足 69 §2「初始帧尽量一致」与 §5「焦距越长半径越远」；`r/f` 恒定 ⇒ 画面足迹尺度一致，否则窄镜头画面被宽镜头完全包含，min 口径共视必挂 |
| 三路里锁两路进 ±32° 扇区、第三路自由 | 闸门是「前三路**至少一对**成立」，锁两路就够保共视，第三路留给多圈环绕的花样 |
| 共视用**图像空间**均匀采样的重投影 overlap | 等面积加权由构造满足（§5 要求的 `w=area·cosθ/d²`）。场景表面撒 probe 那条在相机贴近锚点时画面里一个 probe 都落不进，会算出恒等于 0 的假失败 |
| 自由壳投影按「推→平滑→抽帧验→加厚再推」迭代 | B 样条是逼近不是插值，节点推出去了曲线绕障碍角时照样切进来 |

## 与 QC 的口径对齐

`metrics.py` 把 `engine_qc.checks_motion` 的差分方式、滑窗定义、阈值与**第四路 ρ 反算算法**
逐字复刻了一遍。生成侧自检与 QC 判定必须给出同一个数——否则「自检全绿 QC 打回」会成为常态。
联测方式见上面的 `gen-clip` → `check-clip`。

形态 b（`--cam4-mode topdown`，[cam4_topdown.py](trajgen/cam4_topdown.py)）沿同一纪律：
`metrics.cam4_topdown_check` 逐字复刻 QC `_check_cam4_topdown` 的三条
（偏角占比+硬帽 / 相对前三路当帧最高点高差 / 到 guide 路线 XZ 折线距离——折线抽样、
补末点、逐帧取到**抽样点**的最小距离，连「点而非线段」这一处都照抄，防 #427 复发）；
俯视位声明下初始帧自检对 lane 4 豁免，与 QC `init_exempt_lanes` 联动同语义。
三条判据同时按**构造余量**保证（偏角 ≤12° vs 门 15、高差基准 14–18m + 确定性抬底 vs
门 10、走廊偏移 ~2m vs 门 20/40），自检是复核不是救生索。

## 粉色平移诚实性（QC #597 契约的生成侧收口）

`plan.pink_honesty` 上线后经典管线曾长期「自检全绿 QC 打回」（#427 形态，
实测每条 5-11 窗 FAIL）。收口分四层，各管一类失因：

- **原语基底对齐**（`primitives._translate`）：平移沿**对锚 look-at 的 yaw** 走
  ——声称基底是窗首帧相机 yaw，而平移窗内相机全程 look-at 锚点，二者必须同源。
  旧写法沿上一 move 末位 yaw，pan 之后轴错位可达 90°+（36/144 窗的失因）；
  另配 extent 抬底（1.7m）、预算地板（1.2m）与粉色时长地板（2.2s）。
- **修正场**（[pink_honesty.py](trajgen/pink_honesty.py)，前三路）：样条稀释/壳投影/
  共视收敛改完位置后的最后一道——轴向 S₅ 增益 + 横向抑制 + 窗后衰减 + 俯冲预抬升，
  全部按 jerk 预算（8 m/s³）限幅，间隙装不下的偏移常量携带（不改判据）。
- **第四路保速换向**（`_cam4_pink_run`，复用导演模式 `pink.Cam4Driver`）：cam4 的
  位置不能事后修（|v₄|=ρ·v_g 一破，变速三条把修正判成事件），粉色改在墙钟域
  按 v₄ 谷架排窗、窗内显式换向（|v₄| 逐帧不变 ⇒ ρ 反算无感）、窗尾把位移折进
  恒定偏置续走；标称不再排粉（0.6s 粗节点的样条稀释 + 窗口重映射叠加，声称窗
  与几何对不上）。慢 ρ 档 × 谷架凑不够 0.8m 时可行性预检早退换抽签。
- **自检**（`metrics.pink_honesty_check`，四路全判）：与 QC `check_pink_honesty`
  逐字同式（round 帧定位、窗首帧 yaw 基底、0.8m/50% 双判据），进 `_selfcheck`
  失败打回重排——这条自检此前完全缺席，正是 #427 的形态。

联测口径：timewarp 与 topdown 各 8 种子（open_plaza）`check-clip --mode mc` 全绿
（0 FAIL）。`corridor` 类在诚实契约下第四路可行域进一步收窄（换向偏置在窄走廊里
撞穿模红线；该类收口前的「通过」clip 本来就过不了 QC 的 pink_honesty）——维持
R9 的裁决：按失败原因交叉表在排产里降权，不为它松任何一侧的契约。

## 合成场景 = M3a 的替身，不是产线场景

`scene.synthetic` 的四类（`open_plaza` / `urban_block` / `indoor_rooms` / `corridor`）
是**无美术资产期**的替身：docs/70 R1 的授权场景采购是 M2 硬前置，卡的是 M3b 之后，
M3a 明确不依赖它。真实场景由 SceneKit onboarding 导出同构的 `scene.json`（`Scene.load`）后，
同一套指标可直接复跑。

`corridor` 一类对应 docs/71 R9 的高危分层（可行域最薄），M3a 的失败原因×场景类型交叉表
正是用来把这类场景在排产里降权的。

## 待确认项（随 docs/72 一起问需求方，回复后只改 `export.py` 一处重导）

- `camera_Follow Offset`：自由相机无第三人称 rig ⇒ 零向量
- `player_*`：无角色 ⇒ 相机地面投影 + 水平速度
- `mouse_x/y`：程序化无光标 ⇒ 恒 0.5；`dx/dy` 由朝向增量按满幅归一化
- `keyCode`：程序化段无输入 ⇒ 空数组
- `*_rotation_oula`：Unity `eulerAngles` 口径（顺规未定义，docs/72 C3）
