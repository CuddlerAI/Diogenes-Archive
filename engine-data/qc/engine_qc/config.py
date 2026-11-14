"""QC 阈值配置。默认值 = docs/70 §4.4 / docs/71 §9 + engine-data/README.md 口径快照。"""

from dataclasses import dataclass

#: 名义时间轴帧率（两单均为 30fps CFR 时间轴；15fps 文件为偶数帧抽取，步进 2）
NOMINAL_FPS = 30.0

#: systeminfo / video 分辨率白名单（1080P 档位，含竖屏）
RESOLUTION_WHITELIST = {(1920, 1080), (1080, 1920), (1440, 1080), (1080, 1440)}

#: 键名怪癖表（engine-data/README.md）：跨口径键名黑名单——出现即 FAIL
WRONG_KEYS = {
    "mc": {
        "camera_follow_offset": "68(SFM) 小写键名混入 MC，应为 'camera_Follow Offset'",
        "mouseX": "68(SFM) 驼峰键名混入 MC，应为 'mouse_x'",
        "mouseY": "68(SFM) 驼峰键名混入 MC，应为 'mouse_y'",
    },
    "sfm": {
        "camera_Follow Offset": "69(MC) 空格键名混入 SFM，应为 'camera_follow_offset'",
        "mouse_x": "69(MC) 下划线键名混入 SFM，应为 'mouseX'",
        "mouse_y": "69(MC) 下划线键名混入 SFM，应为 'mouseY'",
    },
}


@dataclass
class QCConfig:
    # --- 数值 ---
    quat_norm_tol: float = 1e-3       # 四元数模长容差
    float_eq_tol: float = 1e-6        # fx=fy / metric_scale=1.0 等相等判定容差

    # --- 时长 ---
    mc_frames: int = 1800             # MC 60s x 30fps，±0
    mc_duration_s: float = 60.0       # MC video 时长目标（--expect-duration-s 可覆盖）
    sfm_durations_s: tuple = (60.0, 180.0)
    #: 初始帧闸门豁免车道（1-based）。默认空 ⇒ 行为不变。导演模式传 (4,)：
    #: cam4 是统览位，按 docs/78 §10 定义必须远离三车并覆盖整段航迹，其初始
    #: 朝向/距离与载机不同是设计产物（🟨 与需求「初始帧尽量一致」的偏离，
    #: 随样片送审说明）。三载机之间仍照常判定。
    init_exempt_lanes: tuple = ()

    # --- 物理阈值（p95 口径，反算用有限差分）---
    speed_p95_max: float = 6.0        # m/s
    speed_low_hint: float = 0.2       # m/s，下限提示（占比只报不判）
    speed_cam4_cap: float = 10.0      # m/s，第四路瞬时硬帽
    speed_inst_cap: float = 10.0      # m/s，全路帧间瞬时硬帽（内控值，README「全路帧间瞬时速度硬帽」节）
    accel_p95_max: float = 8.0        # m/s^2
    jerk_p95_max: float = 20.0        # m/s^3
    angspeed_p95_max: float = 90.0    # deg/s
    #: 角速度瞬时硬帽（§3.4「不得超过」；p95 看不见单帧甩镜）
    angspeed_max_cap: float = 90.0    # deg/s
    angaccel_p95_max: float = 150.0   # deg/s^2
    angjerk_p95_max: float = 600.0    # deg/s^3
    #: MC 歪头（roll）闸门（docs/69 §3.2）。刻意旋转运镜合法（歪头 ±5~15°、占比
    #: ≤2%），所以超规格帧占比的阈值是 2% 而非 0；20° = ±15° 规格 + 姿态噪声容差。
    #: 拦的是环绕 up 向量未锁 world-up 的**累积漂移**（capture-v7 实证 |roll| 漂到
    #: 74° 而 angspeed/angaccel/angjerk 全不触发——漂移是慢变化，速率链规则天然盲）。
    roll_limit_deg: float = 15.0      # deg，规格幅值 ±15°
    roll_over_frac_max: float = 0.02  # |roll|>15° 帧占比上限
    roll_max_deg: float = 20.0        # deg，max|roll| 硬帽（规格 + 容差）
    #: player_speed 与 player_position 差分的允许偏差（导出精度量级）
    speed_consistency_tol: float = 1e-3   # m/s
    #: num.input_stream 的「镜头在转」判据：全片累计转动（相邻帧夹角之和）。
    #: 累计量对采样步长不变——逐步阈值会让同一条轨迹 30fps 判「没转」、
    #: 15fps 判「在转」（0.36°/步的阈值恰好卡在 0.2°/帧与 0.4°/帧之间）。
    input_total_rot_min_deg: float = 10.0
    #: 69 号 §4.3/§6：逐路动态物体占时下限
    dyn_active_min: float = 0.80

    # --- 解耦（docs/71 §5 口径）---
    window_s: float = 1.0
    step_s: float = 0.5
    static_v: float = 0.2             # m/s
    static_w_deg: float = 3.0         # deg/s
    dir_cos_min: float = 0.85
    ratio_lo: float = 0.7
    ratio_hi: float = 1.4
    yaw_min_deg: float = 5.0          # deg/s
    pair_sync_max: float = 0.5        # 每对同步窗占比 <50%
    all4_sync_max: float = 0.2        # 四路同时同步 <20%

    # --- 初始帧（内控值，可配置）---
    init_angle_max_deg: float = 15.0
    init_dist_max_m: float = 30.0

    # --- 第四路（需 plan.json）---
    tsw_lo: float = 20.0
    tsw_hi: float = 40.0
    band_lo: float = 0.65
    band_hi: float = 1.5
    band_dwell_max_s: float = 1.0     # 扣除一次单调穿越后
    rho_change_log: float = 0.223     # ~log(1.25)，变速事件检测阈
    tsw_measure_tol_s: float = 2.0    # 实测切换中心 vs plan(t_sw+delta/2) 容差

    # --- 第四路俯视伴飞（plan.cam4.mode="topdown"，docs/69 §8.3 形态 b）---
    #: 验收 demo mc2 只给形态不给数值——「垂直俯视、高空、沿主体路线上空漂移」；
    #: 下面四组阈值全部是内控标定值，随首批真实俯视样片回填。
    #: pitch 组结构同 phys.roll：占比闸 + 硬帽，抓「短暂摆头」与「整段斜视」两种失形。
    topdown_pitch_tol_deg: float = 15.0    # deg，forward 与世界 -y 的偏角规格
    topdown_pitch_frac_min: float = 0.95   # 偏角达标帧占比下限
    topdown_pitch_max_deg: float = 30.0    # deg，偏角瞬时硬帽（过渡摆动的绝对上限）
    #: 「高空」用相对量定义：cam4 高度须高出前三路当帧最高者一个余量——
    #: 绝对海拔随场景漂移（室外山地 vs 广场），相对量不随场景标定。
    topdown_alt_min_m: float = 10.0        # m，相对前三路当帧最高点的最小高差
    topdown_alt_frac_min: float = 0.95     # 高差达标帧占比下限
    #: 「路径相关」（§3.5「不能与前三路没关系」）：到 guide 路线 XZ 折线的水平距离。
    #: 折线口径而非逐帧同步距离——demo 实测俯视位相对地面机位带时间超前（§8.3），
    #: 逐帧距离会把合法的时间错位判成脱队。
    topdown_route_xz_p95_max: float = 20.0  # m，水平距离 p95 上限
    topdown_route_xz_max_m: float = 40.0    # m，水平距离瞬时硬帽（兼作 t=0 锚定同区域的下界）

    # --- 粉色平移诚实性（plan.pink_honesty；判据即与生成侧的契约，两侧同值）---
    #: 声称轴向净位移下限（m）
    pink_min_disp_m: float = 0.8
    #: 轴向净位移须占窗内路径长的下限（防「原地打转也算平移」）
    pink_axis_frac_min: float = 0.5

    # --- 共视元数据 ---
    covis_min: float = 0.5
    #: 单对双向 overlap 的达标线（69 号 §0「≥30% 相同的区域」）
    covis_overlap_min: float = 0.30
    #: 声明的 covis_ratio 与按严读重算值的允许偏差。
    #: trajgen 写出时 round 到 4 位，曲线本身也是同一套采样点，所以真一致时偏差在 1e-4 量级；
    #: 放到 0.01 只是留出取整与采样端点的余量——宽读/严读的差距是 0.4 量级，不会被这条容差吞掉。
    covis_declare_tol: float = 0.01

    # --- depth 内容组（docs/68 §6.4/§6.5，docs/83 §5）---
    depth_width: int = 1280
    depth_height: int = 720
    depth_channel_names: tuple = ("Y", "Z")   # 68 号「推荐通道命名为 Y 或 Z」
    depth_sample_frames: int = 8              # 逐帧全像素检的抽样帧数（首尾必取）
    depth_nan_frac_max: float = 0.98          # 近乎全 NaN 判异常（天空占比再高也不该到此）
    depth_far_max: float = 20000.0            # 有效深度上限，仅拦离谱值；实际远平面进 provenance

    # --- depth 方向（上下翻转检测，docs/88）---
    #: 顶/底行带高度占比。中间的行不参与：远平面截断产生的 NaN 贴着地平线
    #: （离地 h、远平面 F 时约 atan(h/F) rad，几行到几十行），落在中带，不该算天空证据。
    orient_band_frac: float = 0.25
    #: NaN 证据可判门槛：抽样帧顶带+底带 NaN 像素合计低于此数则该路证据不判。
    orient_min_nan_px: int = 2000
    #: 顶带 NaN 占比 ≥ 此值 → 方向正确；≤ orient_nan_top_flip → 判上下翻转。
    #: 两阈值之间（天空一半在顶一半在底）物理上不该出现——按「方向不可信」FAIL。
    orient_nan_top_min: float = 0.65
    orient_nan_top_flip: float = 0.35
    #: 带中位证据：每带最少有效像素数，低于则该帧不投票。
    orient_band_valid_min: int = 500
    #: 顶带中位 / 底带中位 ≥ 此比值记「正确」票，反向记「翻转」票，其余弃权。
    orient_median_ratio: float = 1.05
    #: 带中位证据最少可判帧数与多数票占比。
    orient_median_frames_min: int = 3
    orient_median_maj: float = 0.7
    #: 横滚门槛：|相机 right 向量的世界 y 分量|（≈sin 横滚角）超过则该帧不可判——
    #: 横滚会把「图像上下」转离「世界上下」，行带统计失去物理意义。
    orient_max_roll_sin: float = 0.15

    # --- 跨模态反投影自检（docs/83 §5.3）---
    #: 相对残差中位数阈值。松到能容 cm 截断 + fp16 间隔 + 最近邻重采样，
    #: 紧到坐标系/内参/帧错位这类结构性错必然爆表（它们产生的是数量级偏差）。
    #: 🟨 D2 用真渲染深度定标后回填。
    reproject_rel_median_max: float = 0.02
    reproject_points: int = 2000              # 抽样像素目标数（按栅格步长换算）
    reproject_min_samples: int = 200          # 共视样本不足则 SKIP，不硬判
    reproject_min_baseline_m: float = 0.05    # 帧对最小位移
    reproject_min_quat_delta: float = 1e-4    # 或最小转动（1-|dot|）
    #: 帧对齐（depth.align）：偏移 0 的残差须比 ±1 小到这个倍数以上。
    #: 无量纲，不随场景尺度/相机速度漂移——绝对阈值在此处有盲区（见 checks_depth 注释）。
    align_margin_min: float = 1.5

    # --- 地图边界与 pose 坐标系一致性（plan.map_bounds，docs/88 §6 #1 / 辩题二十四）---
    #: pose 相对边界 AABB 的允许出界余量：边界本就是外接盒口径（over-estimate），
    #: 这里只吸收数值毛刺，不该吸收真正的坐标系错位（sfm-d1 的错位是百米级）。
    map_bounds_margin_m: float = 0.5
    #: map-world 摆位解算的最坏残差门槛：plan↔clip 不是刚体关系就拒绝产出边界。
    placement_residual_tol_m: float = 1e-3
    #: 可判别性下限：相对位姿随 ±1 偏移的变化（平移 m + 旋转 rad）。低于此值说明
    #: 运动近似匀速直线，整体错一帧不改变相对位姿、几何上不可观测 → SKIP 而非 PASS。
    align_min_discriminability: float = 0.02
    #: 残差噪声地板：偏移 0 的残差低于此值时，比值判据无意义（三者同量级），判 SKIP
    align_noise_floor: float = 0.002

    # --- video ---
    video_dur_tol_s: float = 0.5
    #: 音轨静音判据（69 号 §8.1「有声音」）：volumedetect 的 max_volume 低于此值
    #: 判 FAIL「音轨为静音」。数字静音实测为 -91.0 dB（16bit 量化地板）；正常游戏
    #: 录音的 max_volume 在 0 ~ -30 dB 量级——-70 放在两者之间，留足编码底噪余量。
    audio_silence_max_db: float = -70.0

    # --- 模拟需求方抽检协议（docs/71 §9 G-18）---
    sample_json_n: int = 2            # 抽 2 条 json 全检
    sample_video_frac: float = 0.05   # 5% video 检
    video_pass_line: float = 0.9      # 90% 线
