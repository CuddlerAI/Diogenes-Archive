# quality-check · SFM depth 交付包数据核验能力

Loop-有位移 40h 单（docs/121）的验收自检工具组。**通过这两个脚本 = 达到「发报告截图」门槛**（用户 2026-08-11 口径）。

## 工具

### 1. `quality_check_depth_dataset.py` —— 客户验收 oracle（勿改）

- **来源**：客户 2026-08-11 提供，按字节原样入库（SHA-256 `b703d7ce…3635d85`）。**这是客户的验收口径，任何修改都会让自检失去意义；口径变化以客户新版本整体替换。**
- 覆盖：五件套命名 / video 1920×1080·30fps·**恰 1800 帧（60s 硬口径）** / json 数量·帧号·字段顺序 / 时间戳与 frame/30 误差 ≤1ms / fps 字段 ≥30 / depth 900 张·720p 单通道·`depth_0.exr`…`depth_1798.exr` / 15fps `depth_name` 对账。
- **不覆盖**（另行把关）：闭环精度（用工具 2）、fp16/NaN/量化（engine_qc）、位姿与画面一致性（E-geo，#636 教训）、depth↔RGB 时间对齐/无重影（用工具 7——#801 之前只靠目检，v7 包因此漏到客户手里）。
- 已知边界：帧数硬编码 1800，**只适用 60s 条**；3min 条待客户升级脚本口径再议（docs/121 §7）。
- 依赖 OpenCV：`python3 -m venv qcenv && qcenv/bin/pip install opencv-python`。

```bash
qcenv/bin/python DataMiner/tools/quality-check/quality_check_depth_dataset.py <数据目录>
```

退出码 0=通过；`--json` 出机器可读结果。

### 2. `loop_closure_check.py` —— Loop-有位移 闭环自检（我方补充）

客户脚本不查闭环，而闭环是本类目的标的物。从 `action_camera_30fps.json` 首尾帧算位置差/朝向差（atan2 形式，小角度不病态），阈值沿用 trajgen 生成侧机检闸 0.60 m / 8.0°，另查「确实有位移」（最大离起点 >2 m，防与 Loop-原地摇镜错分）。标准库即可运行。

```bash
python3 DataMiner/tools/quality-check/loop_closure_check.py <数据目录>
```

## 基线实测（2026-08-11）

`sfm-d1-20260805/clip`（60s Loop-有位移，Megacity/UCL 仅内部）过客户脚本 **20 通过 + 2 不计问题，最终结论：通过** ——五件套格式与客户 oracle 已对齐。

## 采集机侧注意（B 线 FO4）

- `depth-lab deliver` 产出的 15fps 文件名是 `action_camera.json`，**必须改名为 `action_camera_15fps.json`**（客户脚本对 fallback 名直接判不通过）。
- `systeminfo.json` deliver 不产，按现行手工流程放入数据目录。
- 60s 条必须**恰好 1800 帧**（1799/1801 都不过）。

### 3. `handheld_feel_check.py` —— 手持感自检（业务方 0811 反馈第 4 条）

量化「手太稳/太猛/站桩」：静止帧占比（客户红线 10%）、最长静止段、旋转速率中位
（手持目标 5–25°/s）、朝向锁定帧占比、瞬时角速度峰值（≤90°/s 甩头帽）。阈值为
v1 建议档，#717 人工档位定形后以彼为准。0012 负样本：5 项全不通过（36%/11.7s/
1.6°/s/45%/571°/s）——正是客户「太稳」+我方闸「甩头」的合成画像。

```bash
python3 DataMiner/tools/quality-check/handheld_feel_check.py <数据目录>
```

### 4. `sopv2_video_check.py` —— SOP v2 内容闸（A–F）

业务方 0811 四条 + 客户「不要」项的机检实现。**直接吃 R2 原始 session 目录**，不需要先转换
——内容项一条不过就废掉整条，先跑它能省掉几分钟转换。

⚠️ **B 两项（HUD/菜单）在画面几乎不动时判不了**：投票判据靠「世界动了它不动」区分
HUD 与景物，可判帧不足会自动升级 `需人工确认` 交给目检（判据见 docs/128 §3.5）。
绿色场景（Vault 111 整屋 pip-boy 绿）的误报史与双样本实测见 docs/121 §13.2。

```bash
qcenv/bin/python DataMiner/tools/quality-check/sopv2_video_check.py <raw-session-dir> \
    [--window-start 132]          # 录制长于 1800 帧时，指定真正交付的那一段
```

- **FOV 优先读 `camera-pose-index`**（docs/130 起随 session 上云），标定退位成独立交叉核验；
  索引不在时回落到深度+光流的刚性一致性标定。标定方法与两个坑见 docs/128 §3。
  逐帧内参只有 1 组值 = 片中没改过 FOV，这比缩放台阶的间接推断强一个量级。
- **深度线性化参数带试算门**：见工具 6 的第二条。

**B 项判据（2026-08-13 修）：绿掩膜 → 「世界动了它不动」。** 原先只看 pip-boy 绿像素
计数，阈值是照 FO4 **室内**调的；到了 Diamond City（室外，绿漆铁皮屋 + "Publick
Occurrences" 绿霓虹招牌）`hud_total` 被打到 25845（阈值 200），f3cf5bab 整条被误判
5.1% 帧有 HUD。现在改成投票：HUD 钉在屏幕坐标上，景物随全局位移走，
`stay ∧ ¬move` 在足够多可判帧里成立才算 HUD 像素。菜单/控制台另判——
**大面积绿 + 世界被冻住**（菜单会暂停游戏；绿墙帧画面照样在动）。
双样本实测：正样本 2004c0d1 仍判出 HUD（f90–942，47.4%，与 docs/121 §10 逐帧吻合）
与暂停菜单（f1–88）；负样本 f3cf5bab 投出 **0 px**，两项均通过。

已知性质：判别量对**细结构**最灵（真实 HUD 就是准星/血条/文字这类）；实心大色块
平移后与自己重叠，只有前缘投得出票——但那一类正是菜单，走冻结判据。
画面全程不动时两者无法区分，可判帧不足 `HUD_MIN_JUDGEABLE` 会改判**需人工确认**。

**交付窗不从 session_frame 0 开始时**（子集视图、跳过开场），C 项鼠标微扰按
首段 `session_frame_start` 平移取数；此前直接用 `range(n_win)`，f3cf5bab 实测偏差
13.4 个百分点（71.3% vs 真实 84.7%）。

### 5. `rgbd_odometry.py` / `build_check_package.py` —— 核验用轨迹与五件套（**无真位姿时才用**）

R2 上没有位姿，Loop 闭环无从判起；里程计从深度+光流重建轨迹，`build_check_package.py`
据此组出**核验用**五件套让客户脚本能跑。⚠️ **均非交付件**——交付必须回采集机走官方
`depth-lab deliver`。

**session 里有 `camera-pose-index.jsonl` 就别走这条**（docs/130 起会随 session 上云）：
里程计有累积漂移，同一条数据（`222e8f46`）它把闭环判成 **5.10 m / 158.7°**，
真位姿是 0.793 m / 10.43°。用下面的 6。

录制长于 60s 时用 `build_check_package.py --start <偶数帧>` 在里面挑交付窗（窗内帧号从 0
重编，depth 跟着重命名）。docs/121 §13 那次 593 个窗口的穷举就是靠它跑的——
结论是**三条 session 无一窗能过闭环闸**，别再手工试了。

### 6. `build_delivery_package.py` —— 用**真位姿**组五件套

与 5 的分工一句话：**那个用里程计重建位姿，这个读真位姿**。靠 `frames.jsonl` 的
`session_frame ↔ present_seq` 把画面、位姿（`camera-pose-index`）、深度三者对齐。
内参也是读出来的，不再标定。

```bash
qcenv/bin/python DataMiner/tools/quality-check/build_delivery_package.py \
    <out-pkg> <raw-session-dir> --start 132 --frames 1800
```

三条按包实测、**不照游戏硬编码**（照硬编码会在链路变化时静默错掉）：

- **手性**：`--handedness auto`（默认）用跨帧深度互投影二选一。`222e8f46` 实测索引存的是
  **cam→world**（0.07% vs 48.31%），而 68 号交付口径要 world→cam，故写出前转置。
- **投影参数**：优先读 `depth-index`，但**先试算一帧**——`222e8f46` 的索引值
  （`±0.0100000284`，系数套过 viewport 变换而落盘 buffer 没套）会把整段深度线性化成
  **全 NaN 且不报错**，试算门自动弃用并回落。出处与两处不一致见 docs/132 §2.1。
- **绝对尺度**：深度米制与位姿米制必须同一把尺；互投影残差即校验（实测 0.06%）。

⚠️ **仍不是可对外交付件**：`dev_non_trainable` 档 + FO4 绑定 `production_binding_authorized=false`
（#636 / docs/122–123）。交付路径的正反方辩论与裁决见 **docs/132 §4.1**。
### 7. `delivery_cleanliness_check.py` —— 音频 + 水印（另外两个没人把关的闸）

68 号 §7.1 把「视频声音」写成一整行硬要求，但**客户 oracle 全文 0 处涉及音频**，
SOP v2 的 A–F 也没有音频项和水印项。结果是「22 项全过 + 内容闸只差档位」的包，
可能是一条**默片 + 带水印**的片子——2026-08-13 那批 9 条正是如此（9/9 静音、9/9 带水印，docs/133）。

```bash
python3 DataMiner/tools/quality-check/delivery_cleanliness_check.py <数据目录>
```

两个判据都刻意避开已知的坑：

- **音频量响度，不看有没有 audio stream**。`run_acceptance.sh` 封的 `anullsrc` 是**有**音轨的，
  只看 stream 会整批漏掉。地板 −80 dB（全零 = −91，已交付参考件 = −42.5）。
  ⚠️ 对应 `AGENTS.md` 第 27 行「不得伪装成完整静音」。
- **水印看结构，不看颜色**——判左上 192×8 的「接近纯黑 + 接近纯白」像素占比（阈值 0.8）。
  这一条是从 HUD 误报里学来的：旧 HUD 判据数绿像素，被 Diamond City 绿摊位和
  Vault 111 绿灯光各骗了一次。

### 8. `depth_rgb_alignment_check.py` —— depth↔RGB 时间对齐（重影硬闸，#801）

CP2077 v7 包 20 项格式全过、仍被接收方以重影拒收：50% 槽位的深度贴合的是相邻
±1~2 帧而非自己标称的帧（根因是 16Hz 采集时钟 vs 15fps 交付网格，见 #801）。
此前「无重影」只靠抽帧目检，抽在静止段必漏——v7 正是这么漏出去的。本闸把它机检化：
每槽位把 depth 边缘图与 RGB 第 n±4 帧边缘图做归一化互相关取 best_k；静止/弱纹理段
两边分不出高下（margin 小）**不计错**，天然不误杀静止段。

三道阈值（v7 坏包实测 50.3% / 9.1% / 7.7%，对齐数据 ~0，阈值取两者之间）：
错位槽位（best_k≠0 且 margin>0.01）≤5%、远错位（|best_k|≥2）≤1%、重复深度槽位 ≤2%。

```bash
qcenv/bin/python DataMiner/tools/quality-check/depth_rgb_alignment_check.py <数据目录>
```

v7 拒收包实测：453/900 错位 + 82 远错位 + 69 重复，三闸全红（60s 条约半分钟）；
`--json` 出机器可读结果。已接入 `run_acceptance.sh` 并计入总退出码。

### 9. `run_acceptance.sh` + `render_report.py` —— 一键端到端

```bash
DataMiner/tools/quality-check/run_acceptance.sh <session-id>
```

拉数 → 内容闸 → 里程计 → 组包 → 验收脚本组 → 一页 `acceptance.html`（截图发业务方）。
完整说明见 **docs/128**。

---

## 能力总览与共用模块

| 文件 | 作用 | 备注 |
|---|---|---|
| `quality_check_depth_dataset.py` | **客户验收 oracle** | 客户提供，**按字节原样入库，禁改** |
| `sopv2_video_check.py` | 内容闸 A–F，直接吃 R2 原始 session | 每项标注口径出处；**尚未接入 qclib/qcthresholds**（#764 的 fov_read/投影试算门已落地，统一接入待后续） |
| `loop_closure_check.py` | Loop 闭环专项 | 阈值为**我方引擎线档** |
| `handheld_feel_check.py` | 手持感（静止/微扰/甩头） | 阈值多为**我方 v1 档** |
| `dynamics_check.py` | 动态物体 60-30 | ⚠️ **未通过目检验证，勿用其数字** |
| `rgbd_odometry.py` | 无位姿索引时重建轨迹 | 核验用，非交付件 |
| `build_check_package.py` | 组核验用五件套 | 核验用，非交付件 |
| `build_delivery_package.py` | 用**真位姿**组五件套（§6） | 尚未接入 qclib（同 sopv2，待统一） |
| `delivery_cleanliness_check.py` | 音频响度 + 水印结构闸（§7） | — |
| `run_acceptance.sh` / `render_report.py` | 一键端到端 + 报告 | 见 docs/128 |
| **`qclib.py`** | 共用实现：EXR 读取/深度线性化/DepthBank/位姿数学/刚体拟合 | 新增 |
| **`qcthresholds.py`** | **所有阈值的唯一出处，每条带口径来源** | 新增 |

### 为什么要有 `qcthresholds.py`

2026-08-12 的教训（docs/121 §13.2）：卡死整批数据的三个闸——单方向 ≤2s、甩头 ≤90°/s、
闭环 0.60m——**没有一个是客户硬要求**（分别是客户原文标注的「非硬性」、我方 v1 建议档、
引擎线生成侧自检）。阈值散在各脚本又不写出处，下游就会把我方的档当客户要求去打回数据。

**新增任何阈值必须落在 `qcthresholds.py` 并声明 `source`**；判定侧只有
`CUSTOMER_HARD` 允许判「不通过」，其余降级「需人工确认」。查全表：

```bash
python3 DataMiner/tools/quality-check/qcthresholds.py
```

### 已固化的六个「机检踩坑」教训

| # | 坑 | 触发 |
|---|---|---|
| 1 | HUD 判据三 ROI 求和被景物顶穿 | 点位选对 → 场景变绿 |
| 2 | FOV 标定在平滑运镜下缺样本 | 运镜变稳 → 单帧对转动变小 |
| 3 | 补帧被判成「原地静止」 | 掉帧被采集端补帧 |
| 4 | 姿态插值就近取整造出「旋转中位 0.0°/s」 | 里程计只在偶数帧有位姿 |
| 5 | 格式偏差（camera_speed 标量 / depth 通道 Y / keyCode 空串）全被客户脚本放过 | 客户脚本只查键顺序与尺寸 |
| 6 | 运动分割把遮挡边界当动态物体（89% 是假的） | 未做遮挡掩膜 |

**共同规律：加严采集口径时必须同步复核机检口径；新指标先跟目检对齐再用。**

## 测试

`tests/` 锁住 B 项判别量与交付窗偏移、depth↔RGB 对齐闸的 best_k 判别与三道阈值，
用合成帧（不读视频文件），跑在 `dataminer-ci` 的 `quality-check` job 里。同一 job 还校验客户 oracle 的 sha256
未被改动——这个文件一旦被「顺手清理」，自检就不再等于客户口径。

```bash
python -m pytest DataMiner/tools/quality-check/tests -q
```

> 这套工具此前**完全没有 CI 覆盖**（不在 engine-data-ci 的 roots 里，dataminer-ci 也没跑）。
> 阈值类判据没有测试 = 下次改动会静默改判，而改判的后果是错误的验收结论。