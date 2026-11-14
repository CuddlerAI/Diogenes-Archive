//! CP2077 玩家位姿按「相机内容」归帧。
//!
//! # 为什么不能用「这个 Present 读到的玩家」
//!
//! 渲染相机来自当帧的 GPU 常量，它就是这一帧画面用的相机。玩家根是从游戏逻辑的内存里
//! 读的，而逻辑通常比渲染领先一帧，相位还会滑：卡顿后可能不领先，也可能在一个渲染间隔
//! 里连发两个状态。所以「当帧读到的」「固定挪一帧的」「提前到提交前读的」都有真机反例
//! （2026-09-18 的 44 秒短测：1,368 帧里固定 +1 错 5 帧，增量误差最大 120 mm）。
//!
//! # 为什么相机能当连接键
//!
//! 玩家身上的 FPP 相机组件与玩家根属于同一个逻辑状态，而渲染器拷走的正是这个相机。
//! 同一轮数据里，正确帧差下两者朝向只差约 1e-5°（浮点舍入）、位置差 0.05–1.5 mm；
//! 错一帧则位置差 100 mm 以上。于是：哪条样本的内存相机等于这一帧的渲染相机，
//! 它同时读到的玩家根就是这一帧的玩家根。连接键是两侧各自观测到的同一个物理量，
//! 不按 QPC 就近、不插值、不拟合变换；容差就是残余错位的上界。
//!
//! # 撕裂对与「最早」规则
//!
//! 引擎一步之内先写玩家根、后写相机（P1490 实测：相机内容重复而玩家根前进 123 mm），
//! 两次写入之间内存里是（新根, 旧相机）。所以同一个相机内容可能配着两个玩家根，
//! 与该相机一起发布的那一个，是带这个相机内容的**最早**一条样本。
//! 「最早」只在与匹配条目紧邻的连续段里取：相机长时间不动时，环里更早的同内容条目
//! 是几秒前的状态，不是这一帧渲染的状态。
//!
//! # 环对写入者的要求
//!
//! 「段首」规则依赖环里的条目按观测时间排列且彼此连续，所以环只接受**一个**写入者、
//! 按时间先后喂进来；时间倒退的观测直接拒收。两次成功读取之间隔得太久（读取持续失败，
//! 或采样线程没被调度），会在下一条条目上留下断点：这么久没人看，相机可能离开又回到同一
//! 内容，回溯段首不能跨过断点。断点按**时间**定而不按「是否失败过」定：读取失败最容易
//! 发生在状态切换的瞬间，失败后读到的第一条往往正是撕裂对，此时恰恰需要回溯到失败之前
//! 的那条；把一次瞬时失败当成硬断点，会直接选中撕裂对、错一整步。
//! 连接时只考虑首次读取**完成**不晚于该 Present 锚的状态：这一帧渲染的状态早在 Present
//! 之前就在内存里了，之后才第一次看到的同内容状态说明早先漏采了，宁可拒绝也不拿它顶替。
//!
//! # 采样必须是活的
//!
//! 相机站定或画面冻结时，一条很旧的状态也能一直匹配当帧相机。采样线程活着不等于采样
//! 健康：审计过期、玩家对象被替换之后读取会持续失败，环不再更新，而旧状态照样匹配。
//! 所以连接先看最近一次成功读取离 Present 锚有多久，太久就拒绝。看的是「最近一次成功
//! 读取」，不是条目的首次观测：健康站定时同一条目的首次观测本来就可以很老。
//!
//! # 为什么放在这里而不是 probe 里
//!
//! probe 只能在 Windows 真机上跑。归帧规则里容易错的地方全是纯算术，
//! 放在这里任何平台都能用单测钉死；probe 侧只剩「读内存、塞进环、调一次 join」。

use std::collections::VecDeque;

/// 环的默认容量。只存**不同内容**的状态，30 fps 下 64 条约等于两秒，
/// 远大于逻辑领先渲染的一两帧。
pub const POSE_JOIN_RING_CAPACITY: usize = 64;

/// 连接容差，同时也是归帧后残余错位的上界。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoseJoinTolerance {
    pub position_metres: f64,
    pub rotation_degrees: f64,
}

/// 正式路径使用的容差。
///
/// 实测正确状态下的残差：位置最大 1.48 mm（CBV 侧 float32 视图矩阵求逆的舍入），
/// 朝向 p99 1.7e-5°；错一个状态时走路差 100 mm 以上、转身差几十倍。3 mm / 2e-4°
/// 离两边都有一个数量级，放宽到 5 mm 时匹配集合一帧不变。
pub const CP2077_POSE_JOIN_TOLERANCE: PoseJoinTolerance = PoseJoinTolerance {
    position_metres: 0.003,
    rotation_degrees: 2e-4,
};

/// 一段连续观测到的、原生内容完全相同的（玩家根, FPP 相机）状态。
#[derive(Debug, Clone, PartialEq)]
pub struct SimulationPair {
    /// 第一次读取的起止，和最后一次读取的结束。是观测区间，不是引擎发布时刻。
    /// 单次读取构造时 `first_end_qpc == last_qpc`。
    pub first_qpc: i64,
    pub first_end_qpc: i64,
    pub last_qpc: i64,
    pub reads: u32,
    /// 原生 WorldTransform 的 8 个 u32，用作内容身份；解码值的浮点比较不能代替它。
    pub player_words: [u32; 8],
    pub camera_words: [u32; 8],
    pub player_position: [f64; 3],
    pub player_rotation: [f64; 4],
    pub camera_position: [f64; 3],
    pub camera_rotation: [f64; 4],
    /// 本条之前隔了太久没有成功读取，或它是第一条：它与前一条之间的观测不连续。
    /// 由环在入环时填写，调用方给什么值都会被覆盖。
    pub follows_gap: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoseJoin {
    pub pair: SimulationPair,
    pub position_residual_metres: f64,
    pub rotation_residual_degrees: f64,
    /// 容差内的条目数（含被选中的）。静止时很多条都满足，属正常。
    pub candidates: usize,
    /// 与被选条目相机内容相同、玩家内容不同的条目数，即被「最早」规则排除的撕裂对。
    pub torn_candidates: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PoseJoinRefusal {
    Empty,
    /// 最近一次成功读取离 Present 锚太久：采样线程停了，或读取在持续失败。
    SamplerStale {
        newest_observation_age_qpc: i64,
    },
    /// 没有任何样本的相机等于这一帧的渲染相机：要么那个状态没被采到，
    /// 要么当前渲染相机根本不是 FPP 相机（载具、过场、拍照模式）。不沿用旧值。
    NoCameraMatch {
        entries: usize,
        /// 首次观测不晚于 Present 锚、因而有资格参与连接的条目数。
        eligible: usize,
        nearest_position_metres: f64,
        nearest_rotation_degrees: f64,
    },
}

#[derive(Debug, Clone)]
pub struct SimulationPairRing {
    entries: VecDeque<SimulationPair>,
    capacity: usize,
    /// 两次成功读取相隔超过它，就不再算连续观测。
    continuity_qpc: i64,
}

impl SimulationPairRing {
    #[must_use]
    pub fn new(capacity: usize, continuity_qpc: i64) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            continuity_qpc,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 记一次成功的成对读取。`pair` 的 `first_qpc`/`last_qpc` 是本次读取的起止 QPC。
    /// 返回 `false` 表示这次观测自身区间倒挂、或早于环里最新的观测，被拒收。
    /// 失败的读取不用告诉环：它只是「这一刻没有观测」，环只看成功读取之间隔了多久。
    pub fn observe(&mut self, mut pair: SimulationPair) -> bool {
        if !(pair.first_qpc <= pair.first_end_qpc && pair.first_end_qpc <= pair.last_qpc) {
            return false;
        }
        let Some(newest) = self.entries.back_mut() else {
            pair.follows_gap = true;
            self.entries.push_back(pair);
            return true;
        };
        if pair.first_qpc < newest.last_qpc {
            // 两个写入者交错、或时钟倒退：放不进一个按时间连续的序列里。
            return false;
        }
        pair.follows_gap = pair.first_qpc - newest.last_qpc > self.continuity_qpc;
        if !pair.follows_gap
            && newest.player_words == pair.player_words
            && newest.camera_words == pair.camera_words
        {
            newest.last_qpc = pair.last_qpc;
            newest.reads = newest.reads.saturating_add(pair.reads);
            return true;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(pair);
        true
    }

    /// 找出 `present_qpc` 这一帧的渲染相机对应的逻辑状态。`freshness_qpc` 是最近一次成功
    /// 读取允许落后 Present 锚多久。
    pub fn join(
        &self,
        present_qpc: i64,
        camera_position: [f64; 3],
        camera_rotation: [f64; 4],
        tolerance: PoseJoinTolerance,
        freshness_qpc: i64,
    ) -> Result<PoseJoin, PoseJoinRefusal> {
        let Some(newest) = self.entries.back() else {
            return Err(PoseJoinRefusal::Empty);
        };
        let newest_observation_age_qpc = present_qpc.saturating_sub(newest.last_qpc);
        if newest_observation_age_qpc > freshness_qpc {
            return Err(PoseJoinRefusal::SamplerStale {
                newest_observation_age_qpc,
            });
        }
        // 条目按时间排列，所以有资格的条目是一个前缀。
        let eligible = self
            .entries
            .iter()
            .take_while(|entry| entry.first_end_qpc <= present_qpc)
            .count();
        let scored = self
            .entries
            .iter()
            .take(eligible)
            .enumerate()
            .map(|(index, entry)| {
                (
                    rotation_angle_degrees(entry.camera_rotation, camera_rotation),
                    distance(entry.camera_position, camera_position),
                    index,
                )
            });
        let mut nearest = (f64::INFINITY, f64::INFINITY);
        let mut best: Option<(f64, f64, usize)> = None;
        let mut candidates = 0;
        for (rotation, position, index) in scored {
            if position < nearest.1 || (position == nearest.1 && rotation < nearest.0) {
                nearest = (rotation, position);
            }
            // NaN 残差两个比较都为 false，自然落选。
            if rotation <= tolerance.rotation_degrees && position <= tolerance.position_metres {
                candidates += 1;
                // 残差相同（同一相机内容的多次造访）时取最新的观测，再从它回溯到段首。
                if best.is_none_or(|(r, p, _)| (rotation, position) <= (r, p)) {
                    best = Some((rotation, position, index));
                }
            }
        }
        let Some((rotation, position, matched)) = best else {
            return Err(PoseJoinRefusal::NoCameraMatch {
                entries: self.entries.len(),
                eligible,
                nearest_position_metres: nearest.1,
                nearest_rotation_degrees: nearest.0,
            });
        };
        // 与匹配条目紧邻、相机内容相同、观测连续的一段：段首是与这个相机一起发布的玩家根，
        // 段内其余玩家内容不同的条目是撕裂对。断点之前的同内容条目属于另一次造访。
        let camera_words = self.entries[matched].camera_words;
        let mut first = matched;
        while first > 0
            && !self.entries[first].follows_gap
            && self.entries[first - 1].camera_words == camera_words
        {
            first -= 1;
        }
        let earliest = &self.entries[first];
        // 只数这个 Present 已经能看到的撕裂对；锚之后才出现的与这一帧无关。
        let torn_candidates = self
            .entries
            .iter()
            .take(eligible)
            .skip(first + 1)
            .take_while(|entry| !entry.follows_gap && entry.camera_words == camera_words)
            .filter(|entry| entry.player_words != earliest.player_words)
            .count();
        Ok(PoseJoin {
            pair: earliest.clone(),
            position_residual_metres: position,
            rotation_residual_degrees: rotation,
            candidates,
            torn_candidates,
        })
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

/// 两个四元数之间的夹角（度）；`q` 与 `-q` 是同一个旋转。
///
/// 用弦长而不是点积的 `acos`：残差在 1e-5° 量级，点积离 1 只差 1e-15，
/// `acos` 在那里会把有效数字吃光，弦长不会。
#[must_use]
pub fn rotation_angle_degrees(a: [f64; 4], b: [f64; 4]) -> f64 {
    let norm = |q: [f64; 4]| q.iter().map(|v| v * v).sum::<f64>().sqrt();
    let (norm_a, norm_b) = (norm(a), norm(b));
    if !(norm_a > 0.0 && norm_b > 0.0) {
        return f64::NAN;
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let sign = if dot >= 0.0 { 1.0 } else { -1.0 };
    let chord = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let difference = x / norm_a - sign * y / norm_b;
            difference * difference
        })
        .sum::<f64>()
        .sqrt();
    (4.0 * (chord / 2.0).min(1.0).asin()).to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: PoseJoinTolerance = CP2077_POSE_JOIN_TOLERANCE;
    const STEP: f64 = 0.120;
    /// 测试里一个 QPC tick 当作一微秒：两次成功读取隔 50 ms 以上算断点，
    /// 最近一次成功读取落后 Present 锚 100 ms 以上算采样已停。
    const CONTINUITY: i64 = 50_000;
    const FRESHNESS: i64 = 100_000;

    fn ring() -> SimulationPairRing {
        SimulationPairRing::new(POSE_JOIN_RING_CAPACITY, CONTINUITY)
    }

    fn yaw(degrees: f64) -> [f64; 4] {
        let half = degrees.to_radians() / 2.0;
        [0.0, half.sin(), 0.0, half.cos()]
    }

    /// 第 `index` 个逻辑状态；`root` 给出时表示撕裂对（相机还是 `index` 的）。
    fn pair(index: u32, qpc: i64, root: Option<u32>) -> SimulationPair {
        let root = root.unwrap_or(index);
        SimulationPair {
            first_qpc: qpc,
            first_end_qpc: qpc,
            last_qpc: qpc,
            reads: 1,
            player_words: [root; 8],
            camera_words: [1000 + index; 8],
            player_position: [STEP * f64::from(root), 0.0, 0.0],
            player_rotation: yaw(0.0),
            camera_position: [STEP * f64::from(index), 1.7, 0.0],
            camera_rotation: yaw(0.5 * f64::from(index)),
            follows_gap: false,
        }
    }

    /// 渲染相机相对内存相机带着浮点舍入级的噪声。
    fn rendered(index: u32) -> ([f64; 3], [f64; 4]) {
        (
            [STEP * f64::from(index) + 0.0011, 1.7, -0.0004],
            yaw(0.5 * f64::from(index) + 1e-5),
        )
    }

    fn join(
        ring: &SimulationPairRing,
        present_qpc: i64,
        index: u32,
    ) -> Result<PoseJoin, PoseJoinRefusal> {
        let (position, rotation) = rendered(index);
        ring.join(present_qpc, position, rotation, TOLERANCE, FRESHNESS)
    }

    /// 一个紧跟在环里最新观测之后的 Present 锚：采样是活的。
    fn now(ring: &SimulationPairRing) -> i64 {
        ring.entries
            .back()
            .map_or(0, |newest| newest.last_qpc + 500)
    }

    #[test]
    fn micro_degree_residuals_keep_precision_and_sign_is_ignored() {
        let angle = rotation_angle_degrees(yaw(10.0), yaw(10.000_01));
        assert!((angle - 1e-5).abs() < 1e-9, "{angle}");
        let negated = yaw(37.0).map(|v| -v);
        assert!(rotation_angle_degrees(yaw(37.0), negated) < 1e-12);
        assert!(rotation_angle_degrees([0.0; 4], yaw(1.0)).is_nan());
    }

    #[test]
    fn identical_contents_extend_one_entry_until_observation_stops_for_too_long() {
        let mut ring = SimulationPairRing::new(4, CONTINUITY);
        assert!(ring.observe(pair(1, 10, None)));
        assert!(ring.observe(pair(1, 11, None)));
        assert_eq!(
            (ring.len(), ring.entries[0].last_qpc, ring.entries[0].reads),
            (1, 11, 2)
        );
        assert!(
            ring.entries[0].follows_gap,
            "nothing was observed before the first entry"
        );
        // 隔得正好等于上限仍算连续；再长一点就是断点，内容相同也另起一条。
        assert!(ring.observe(pair(1, 11 + CONTINUITY, None)));
        assert_eq!(ring.len(), 1);
        let resumed = 11 + CONTINUITY + CONTINUITY + 1;
        ring.observe(pair(1, resumed, None));
        assert_eq!(ring.len(), 2);
        assert!(ring.entries[1].follows_gap);
        for index in 2..=5 {
            ring.observe(pair(index, resumed + i64::from(index), None));
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.entries[0].camera_words, [1002; 8]);
        assert!(!ring.entries[3].follows_gap);
    }

    #[test]
    fn simulation_lead_stall_and_double_step_all_join_to_the_rendered_state() {
        let mut ring = ring();
        assert_eq!(join(&ring, 0, 0), Err(PoseJoinRefusal::Empty));
        // 逻辑领先一帧：渲染第 n 帧时内存里已经是 n+1；卡顿后第 6、7 个状态挤在一个间隔里。
        for index in 0..=8 {
            ring.observe(pair(index, 1_000 * i64::from(index), None));
        }
        for frame in 0..=8 {
            let joined = join(&ring, now(&ring), frame).unwrap();
            assert_eq!(joined.pair.player_words, [frame; 8]);
            assert!(joined.position_residual_metres < 0.0015);
            assert_eq!((joined.candidates, joined.torn_candidates), (1, 0));
        }
    }

    #[test]
    fn a_camera_content_seen_with_two_roots_belongs_to_the_earliest_root() {
        let mut ring = ring();
        ring.observe(pair(4, 100, None));
        // 根先写：相机 4 还在，玩家根已经是 5。
        ring.observe(pair(4, 130, Some(5)));
        ring.observe(pair(5, 131, None));
        let joined = join(&ring, now(&ring), 4).unwrap();
        assert_eq!(joined.pair.player_words, [4; 8]);
        assert_eq!((joined.candidates, joined.torn_candidates), (2, 1));
        assert_eq!(
            join(&ring, now(&ring), 5).unwrap().pair.player_words,
            [5; 8]
        );
    }

    #[test]
    fn a_torn_pair_first_seen_after_the_anchor_is_not_counted_for_that_present() {
        let mut ring = ring();
        ring.observe(pair(1, 100, None));
        ring.observe(pair(1, 300, Some(2)));
        // Present 锚 200 时撕裂对还没出现：选对了根，计数也不该把它算进来。
        let joined = join(&ring, 200, 1).unwrap();
        assert_eq!(joined.pair.player_words, [1; 8]);
        assert_eq!((joined.candidates, joined.torn_candidates), (1, 0));
        assert_eq!(join(&ring, 300, 1).unwrap().torn_candidates, 1);
    }

    #[test]
    fn an_older_visit_to_the_same_camera_content_is_not_this_frames_state() {
        let mut ring = ring();
        // 相机早先也是这个内容，中间去过别处；那时的玩家根不是这一帧的。
        ring.observe(pair(4, 100, Some(1)));
        ring.observe(pair(9, 200, None));
        ring.observe(pair(4, 300, None));
        ring.observe(pair(4, 330, Some(5)));
        let joined = join(&ring, now(&ring), 4).unwrap();
        assert_eq!(
            (joined.pair.first_qpc, joined.pair.player_words),
            (300, [4; 8])
        );
        assert_eq!((joined.candidates, joined.torn_candidates), (3, 1));
    }

    #[test]
    fn an_observation_older_than_the_newest_one_is_refused_instead_of_reordering_the_ring() {
        let mut ring = ring();
        ring.observe(pair(1, 100, None));
        ring.observe(pair(2, 120, None));
        // 第二个写入者读得早、发布得晚：放进去会把 (根2, 相机1) 排到相机 2 之后，
        // 相机 1 的连续段被隔断，连接就会选中这条撕裂对。
        assert!(!ring.observe(pair(1, 110, Some(2))));
        assert_eq!(ring.len(), 2);
        assert_eq!(
            join(&ring, now(&ring), 1).unwrap().pair.player_words,
            [1; 8]
        );
        // 同内容的迟到观测也不能把区间改成 first > last。
        assert!(!ring.observe(pair(2, 115, None)));
        let newest = &ring.entries[1];
        assert!(newest.first_qpc <= newest.last_qpc);
        // 被拒收的观测不留痕迹：之后按时到达的同内容观测照常续上。
        assert!(ring.observe(pair(2, 130, None)));
        assert_eq!((ring.len(), ring.entries[1].last_qpc), (2, 130));
    }

    #[test]
    fn an_observation_whose_own_interval_runs_backwards_is_refused_even_into_an_empty_ring() {
        let mut ring = ring();
        let mut backwards = pair(1, 200, None);
        backwards.last_qpc = 100;
        assert!(!ring.observe(backwards));
        assert!(ring.is_empty());
        let mut unfinished = pair(1, 200, None);
        unfinished.first_end_qpc = 250;
        assert!(
            !ring.observe(unfinished),
            "the first read ends after the last one"
        );
        assert!(ring.observe(pair(1, 300, None)));
    }

    #[test]
    fn a_state_whose_first_read_had_not_completed_by_the_anchor_cannot_serve_that_present() {
        let mut ring = ring();
        ring.observe(pair(3, 190, None));
        let mut straddling = pair(4, 199, None);
        straddling.first_end_qpc = 203;
        straddling.last_qpc = 203;
        ring.observe(straddling);
        // Present 锚 200 渲染的是状态 4，但状态 4 的首次读取在锚之后才完成：早先漏采了。
        let Err(PoseJoinRefusal::NoCameraMatch {
            entries, eligible, ..
        }) = join(&ring, 200, 4)
        else {
            panic!("a state first observed after the Present anchor must not join");
        };
        assert_eq!((entries, eligible), (2, 1));
        assert_eq!(join(&ring, 203, 4).unwrap().pair.first_qpc, 199);
        // 锚之前看到过的状态，之后继续被看到也不影响资格。
        ring.observe(pair(4, 260, None));
        assert_eq!(join(&ring, 203, 4).unwrap().pair.last_qpc, 260);
    }

    #[test]
    fn the_earliest_rule_never_walks_back_across_a_long_stretch_without_observations() {
        let mut ring = ring();
        ring.observe(pair(7, 100, Some(1)));
        // 读取持续失败或线程没被调度了 60 ms：相机可能离开又回到同一内容，
        // 断点之前的玩家根不属于这次造访。
        let resumed = 100 + 60_000;
        ring.observe(pair(7, resumed, Some(2)));
        ring.observe(pair(7, resumed + 30, Some(3)));
        let joined = join(&ring, now(&ring), 7).unwrap();
        assert_eq!(
            (joined.pair.first_qpc, joined.pair.player_words),
            (resumed, [2; 8])
        );
        assert_eq!((joined.candidates, joined.torn_candidates), (3, 1));
    }

    #[test]
    fn a_read_that_fails_while_the_state_changes_does_not_hand_the_frame_to_the_torn_pair() {
        let mut ring = ring();
        ring.observe(pair(4, 100, None));
        ring.observe(pair(4, 28_000, None));
        // 引擎正在写下一步，这一毫秒的稳定读没通过（没有任何观测入环）；
        // 两毫秒后读到的第一条就是撕裂对：新根、旧相机。
        ring.observe(pair(4, 30_000, Some(5)));
        ring.observe(pair(5, 31_000, None));
        let joined = join(&ring, now(&ring), 4).unwrap();
        assert_eq!(joined.pair.player_words, [4; 8]);
        assert_eq!(joined.torn_candidates, 1);
    }

    #[test]
    fn a_sampler_that_keeps_failing_cannot_keep_serving_an_old_matching_state() {
        let mut ring = ring();
        ring.observe(pair(3, 1_000_000, None));
        // 审计过期或玩家对象被替换：线程还在转，读取一直失败（没有任何观测入环），
        // 而相机站定、旧状态照样匹配。
        assert!(join(&ring, 1_000_000 + FRESHNESS, 3).is_ok());
        assert_eq!(
            join(&ring, 300_000_000, 3),
            Err(PoseJoinRefusal::SamplerStale {
                newest_observation_age_qpc: 299_000_000
            })
        );
        // 健康站定：同一条目很老，但一直有新的成功读取，照常连接。
        for tick in 1..=200 {
            ring.observe(pair(3, 1_000_000 + 2_000 * tick, None));
        }
        let joined = join(&ring, now(&ring), 3).unwrap();
        assert_eq!(joined.pair.first_qpc, 1_000_000);
        assert!(joined.pair.reads > 200);
        assert!(now(&ring) - joined.pair.first_qpc > FRESHNESS);
    }

    #[test]
    fn a_state_that_was_never_sampled_is_refused_not_approximated() {
        let mut ring = ring();
        ring.observe(pair(2, 10, None));
        ring.observe(pair(4, 30, None));
        let Err(PoseJoinRefusal::NoCameraMatch {
            entries,
            eligible,
            nearest_position_metres,
            ..
        }) = join(&ring, now(&ring), 3)
        else {
            panic!("an unsampled state must not join");
        };
        assert_eq!((entries, eligible), (2, 2));
        assert!((nearest_position_metres - STEP).abs() < 0.002);
    }

    #[test]
    fn standing_still_with_a_swaying_camera_takes_the_closest_content() {
        let mut ring = ring();
        // 玩家根不动，相机每步只晃 1e-4°：都在容差内，取朝向残差最小的那条。
        for step in 0..3u32 {
            let mut still = pair(7, 10 * i64::from(step), None);
            still.camera_words = [2000 + step; 8];
            still.camera_rotation = yaw(3.5 + 1e-4 * f64::from(step));
            ring.observe(still);
        }
        let joined = ring
            .join(
                now(&ring),
                [STEP * 7.0, 1.7, 0.0],
                yaw(3.5 + 1e-4),
                TOLERANCE,
                FRESHNESS,
            )
            .unwrap();
        assert_eq!(joined.pair.camera_words, [2001; 8]);
        assert_eq!((joined.candidates, joined.torn_candidates), (3, 0));
    }
}
