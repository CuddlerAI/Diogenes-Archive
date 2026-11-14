//! 采样节拍：决定「这一帧要不要采深度」。
//!
//! # 为什么不能继续用 `present_seq % 2`
//!
//! `% 2` 的含义是「**Present 率的一半**」，不是「30 Hz」。两者只在游戏恰好
//! 跑在车道标称帧率时才相等。#443 复测那台不封顶跑 187 fps，`% 2` 实际是
//! **93.5 Hz 在回读**——3 倍于 30 Hz 车道该有的量。
//!
//! 这首先是**数据正确性问题**，其次才是性能问题：深度频率静默依赖机器性能，
//! 同一份 profile 在不同机器上产出不同帧率的深度，而元数据里写的还是车道标称值。
//! 下游按标称值做时间对齐就会错，且错得没有任何报错。
//!
//! 所以改成按时间基定频：目标 Hz 由配置给出，与 Present 率无关。
//!
//! # 为什么判定放在这里而不是 probe 里
//!
//! probe 是 Windows-only 的注入 DLL，只能在 Windows 上编译、只能在真机上跑。
//! 节拍逻辑里所有容易错的地方——漂移、追帧、暂停后重锚——都是纯算术，
//! 放在这里就能在任何平台上用单测钉死。probe 侧只剩「读 QPC、调一次、看返回值」。

/// 时间基节拍器。
///
/// 用**整数 tick** 而不是秒：QPC 的单位就是 tick，转成 `f64` 秒再比较会引入
/// 舍入误差，而这个判断每帧都做、误差会累积成可观的频率偏移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cadence {
    /// 一个采样周期有多少 tick。
    period_ticks: u64,
    /// 下一次该采的时刻。`None` 表示还没锚定（第一帧必采）。
    next_due: Option<u64>,
    /// 是否只采偶数 `present_seq`。默认 `false`——见 `should_capture_frame` 的说明。
    require_even_present: bool,
    /// `true` 时跳过时间基限流，每个通过奇偶门的 Present 都采。
    every_present: bool,
}

/// 允许的目标频率范围。
///
/// 上限 240：再高就超过任何车道的 Present 率，配了也拿不到。
/// 下限 1：0 或负数没有意义，会让周期变成无穷大或除零。
pub const MIN_TARGET_HZ: f64 = 1.0;
pub const MAX_TARGET_HZ: f64 = 240.0;

/// 早到容差 = 周期 ÷ 这个数（即 12.5%）。
///
/// # 为什么需要容差
///
/// 车道帧率的游戏上，目标周期与偶数帧间隔**几乎相等**（30 Hz 目标 vs 60 fps 的
/// 偶数帧：333 333 tick vs 333 333.33 tick，只差 0.33 tick ≈ 33 ns）。
/// 没有容差时，任何超过 33 ns 的帧间抖动都会让这一帧「早到」而被跳过，
/// 然后要等**下一个**偶数帧——实际频率在目标与目标一半之间来回跳。
///
/// 而帧间抖动在真机上是微秒量级的，也就是说没有容差它**必然**发生。
///
/// # 为什么 12.5% 是安全的
///
/// 上界：容差必须小于一个偶数帧间隔，否则会把本该属于下一拍的帧提前吃掉。
/// 车道帧率下偶数帧间隔 ≈ 周期，所以只要远小于 100% 即可。
///
/// 下界：要盖过真机抖动。30 Hz 目标下 12.5% = 4.2 ms，量级上足够。
///
/// 长期频率不受影响——相位按 `due += period` 推进，与容差无关。
const EARLY_TOLERANCE_DIVISOR: u64 = 8;

/// 车道标称帧率 → 默认深度频率。
///
/// **除以 2**，与今天 `present_seq % 2` 在「游戏恰好跑车道帧率」时的行为逐字一致
/// （lane-30 → 15 Hz，lane-60 → 30 Hz）。owner 2026-08-04 定。
/// 改的只是让它在**任何**机器上都成立，而不是改变标称产出。
#[must_use]
pub fn default_target_hz(lane_fps: u32) -> f64 {
    f64::from(lane_fps) / 2.0
}

impl Cadence {
    /// `qpc_frequency` 是 `QueryPerformanceFrequency` 的返回值（tick/秒）。
    ///
    /// 频率越界或 QPC 频率为 0 时返回 `Err`——**不静默兜底成某个默认值**。
    /// 兜底会让「配错了」表现成「跑起来了但频率不对」，那是最难查的一类问题。
    pub fn new(target_hz: f64, qpc_frequency: u64) -> Result<Self, String> {
        if qpc_frequency == 0 {
            return Err("QPC 频率为 0".into());
        }
        if !target_hz.is_finite() || !(MIN_TARGET_HZ..=MAX_TARGET_HZ).contains(&target_hz) {
            return Err(format!(
                "目标频率 {target_hz} 越界，允许范围 {MIN_TARGET_HZ}..={MAX_TARGET_HZ} Hz"
            ));
        }
        // **向下取整，不四舍五入。** 两侧的后果不对称：
        //
        // - 周期偏短 → 实际频率略高于目标。但 `should_capture_frame` 上面还压着
        //   偶数帧判定，频率被硬性卡在偶数帧率上，「略高」拿不到任何多余的帧。
        // - 周期偏长 → 车道帧率的游戏上，每个偶数帧都恰好比 due 早那么一点点，
        //   于是**每次都被跳过、要等下一个偶数帧**，15 Hz 直接塌成 7.5 Hz。
        //
        // 一边无害一边腰斩，那就往无害的一边偏。
        let period_ticks = ((qpc_frequency as f64) / target_hz).floor() as u64;
        if period_ticks == 0 {
            return Err(format!(
                "目标频率 {target_hz} Hz 在 QPC 频率 {qpc_frequency} 下周期不足 1 tick"
            ));
        }
        Ok(Self {
            period_ticks,
            next_due: None,
            require_even_present: false,
            every_present: false,
        })
    }

    /// 建立逐 Present 节拍器。该模式不读取 QPC，也不做时间基限流。
    #[must_use]
    pub const fn every_present() -> Self {
        Self {
            period_ticks: 0,
            next_due: None,
            require_even_present: false,
            every_present: true,
        }
    }

    /// 周期的 tick 数，供日志与自检。
    #[must_use]
    pub fn period_ticks(&self) -> u64 {
        self.period_ticks
    }

    /// 是否为逐 Present 模式。探针据此避免把 QPC 可用性变成该模式的隐含前置条件。
    #[must_use]
    pub const fn is_every_present(&self) -> bool {
        self.every_present
    }

    /// **probe 该调的是这个，不是 [`should_capture`](Self::should_capture)。**
    ///
    /// # ⚠️ `present_seq` 的奇偶 ≠ `session_frame` 的奇偶
    ///
    /// 交付契约确实是按偶数帧定义的，但那是**视频帧号**：文件名是
    /// `depth_<偶数 session_frame>`（[`crate::pairing::depth_name_for`]），
    /// [`crate::pairing::build`] 也按 `session_frame` 判奇偶。
    /// 而探针只看得到 `present_seq`。
    ///
    /// **两者只在「每个视频帧恰好对应一个 present」时才同奇偶。**
    /// 一旦出现重复帧（游戏掉到录制帧率以下、WGC 补一帧），二者的奇偶关系
    /// 就永久错开一位，直到下一次重复。
    ///
    /// 2026-08-05 的 Fallout 4 实采把这件事量出来了（同一份数据，两种口径）：
    ///
    /// | 口径 | 可配对 | 实配 | 覆盖率 |
    /// |---|---|---|---|
    /// | `present_seq` 偶数 | 9,758 | 9,758 | **100%** |
    /// | `session_frame` 偶数（交付口径） | 9,675 | 5,878 | **60.75%** |
    ///
    /// 19,566 个 RGB 帧里有 **839 个重复帧 / 835 个跳过的 present**。探针把深度
    /// 采得完美——采在了错的那一半上。
    ///
    /// # 所以 `require_even_present` 默认关
    ///
    /// 「落在奇数 present 上的深度会被白白丢弃」这个担心，只有在视频帧与 present
    /// 一一对应时才成立。有重复帧时，**偶数视频帧本来就会落到奇数 present 上**，
    /// 那时候奇数 present 的深度不是浪费，是唯一能配上的那一份。
    ///
    /// 探针无法预知某个 present 会不会成为偶数视频帧的来源，所以**唯一能保证
    /// 覆盖率的规则是每个 present 都采**。fp16 落盘（约 100 KiB/帧）让它负担得起：
    /// 30 fps 全采 ≈ 10.8 GB/h，2 小时约 21.6 GiB。
    ///
    /// 只有在能确证「游戏锁死在录制帧率、且全程零重复帧」时，才值得打开
    /// [`require_even_present`](Self::require_even_present) 省一半体积。
    pub fn should_capture_frame(&mut self, present_seq: u64, now: u64) -> bool {
        if self.require_even_present && !present_seq.is_multiple_of(2) {
            return false;
        }
        if self.every_present {
            return true;
        }
        self.should_capture(now)
    }

    /// 打开「只采偶数 `present_seq`」。**默认关**，理由见
    /// [`should_capture_frame`](Self::should_capture_frame)。
    ///
    /// 打开它等于断言「本次采集全程零重复帧」。这个断言探针验不了，
    /// 只有离线配对才知道——而那时数据已经采完了。
    #[must_use]
    pub fn require_even_present(mut self, yes: bool) -> Self {
        self.require_even_present = yes;
        self
    }

    /// 这一帧要不要采。`now` 是当前 QPC 计数。
    ///
    /// # 累加而不是重置
    ///
    /// 到点后 `next_due += period`，**不是** `next_due = now + period`。
    /// 后者会把「本帧比 due 晚了多少」永久累进下一个周期，几百帧之后
    /// 实际频率会明显低于目标（Present 几乎不可能恰好落在 due 上，
    /// 每次都晚一点点，而那点点会一直攒）。
    ///
    /// # 落后太多就重锚，不追帧
    ///
    /// 游戏被 alt-tab、加载关卡、断点停住时，Present 会停很久。
    /// 恢复后若一味累加，`now` 已经越过好几个 due，接下来会**连续每帧都返回 true**
    /// 直到追平——正好在游戏最卡的时候再压一波回读上去。
    /// 所以落后超过一个周期就直接重锚到 `now + period`，丢掉欠的那些帧。
    pub fn should_capture(&mut self, now: u64) -> bool {
        let Some(due) = self.next_due else {
            // 第一帧必采，并以它为锚。
            self.next_due = Some(now.saturating_add(self.period_ticks));
            return true;
        };
        // 早到容差：见 [`EARLY_TOLERANCE_DIVISOR`]。
        let tolerance = self.period_ticks / EARLY_TOLERANCE_DIVISOR;
        if now.saturating_add(tolerance) < due {
            return false;
        }
        // 相位以 due 为准推进，与容差无关——容差只放行「早到一点点」的这一帧，
        // 不会让长期频率跑高。
        let behind = now.saturating_sub(due);
        self.next_due = Some(if behind >= self.period_ticks {
            now.saturating_add(self.period_ticks)
        } else {
            due.saturating_add(self.period_ticks)
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz，Windows 上 QPC 的常见值
    const QPC: u64 = 10_000_000;

    #[test]
    fn default_matches_what_modulo_two_meant_on_a_lane_paced_game() {
        // 这是本次改动的兼容性锚点：游戏恰好跑车道帧率时，
        // 新旧行为必须给出同一个标称频率。
        assert!((default_target_hz(30) - 15.0).abs() < f64::EPSILON);
        assert!((default_target_hz(60) - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_187fps_game_no_longer_captures_at_935hz() {
        // #443 的实际情形：不封顶 187 fps。旧的 % 2 会采 93.5 次/秒。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let frame = QPC / 187; // 一帧的 tick 数
        let hits = (0..187).filter(|i| c.should_capture(i * frame)).count();
        // 1 秒内应当接近 30 次，而不是 93 次
        assert!((29..=31).contains(&hits), "1 秒采了 {hits} 次，期望 ≈30");
    }

    #[test]
    fn the_first_frame_always_captures() {
        let mut c = Cadence::new(30.0, QPC).unwrap();
        assert!(c.should_capture(0));
        // 紧接着的一帧不该采
        assert!(!c.should_capture(1));
    }

    #[test]
    fn every_present_mode_never_thins_the_present_stream() {
        let mut c = Cadence::every_present();
        for seq in 0..10_000u64 {
            assert!(c.should_capture_frame(seq, seq.saturating_mul(17)));
        }
    }

    #[test]
    fn accumulating_instead_of_resetting_keeps_the_long_run_rate_exact() {
        // 每帧都比 due 晚一点点（60 fps 的游戏采 30 Hz，帧边界与 due 不对齐）。
        // 若用 next_due = now + period，长跑后频率会明显偏低。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let frame = QPC / 60;
        let seconds = 60;
        let hits = (0..60 * seconds)
            .filter(|i| c.should_capture(i * frame))
            .count();
        let rate = hits as f64 / seconds as f64;
        assert!(
            (rate - 30.0).abs() < 0.5,
            "60 秒长跑实际 {rate:.2} Hz，期望 ≈30"
        );
    }

    #[test]
    fn a_long_stall_does_not_produce_a_catch_up_burst() {
        // alt-tab 10 秒回来，不该连采 300 帧把最卡的时刻压死。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        assert!(c.should_capture(0));
        let after_stall = 10 * QPC;
        assert!(c.should_capture(after_stall), "恢复后的第一帧应当采");

        // 「不补采」的判据不是「接下来 N 帧都不采」——30 Hz 的周期恰好是 60 fps 的
        // 两帧，第 3 帧本来就该采。真正要排除的是**欠下的 300 次被连续补回来**。
        let frame = QPC / 60;
        let mut consecutive = 0;
        let mut hits = 0;
        for i in 1..=60 {
            if c.should_capture(after_stall + i * frame) {
                hits += 1;
                consecutive += 1;
                assert!(consecutive < 2, "第 {i} 帧起出现连续补采");
            } else {
                consecutive = 0;
            }
        }
        // 恢复后的 1 秒仍是 30 Hz 的量，不是把停顿期间欠的 300 次倒出来
        assert!(
            (29..=31).contains(&hits),
            "恢复后 1 秒采了 {hits} 次，期望 ≈30"
        );
    }

    #[test]
    fn falling_behind_by_less_than_one_period_still_accumulates() {
        // 只晚了半个周期属于正常抖动，应当保持原节拍而不是重锚，
        // 否则每次抖动都会让节拍往后挪，等价于频率偏低。
        let mut c = Cadence::new(10.0, QPC).unwrap();
        let period = c.period_ticks();
        assert!(c.should_capture(0)); // due 变成 period
        assert!(c.should_capture(period + period / 2)); // 晚半个周期
        // 若正确累加，下一个 due 是 2×period，此刻还没到
        assert!(!c.should_capture(period + period / 2 + 1));
        assert!(c.should_capture(2 * period));
    }

    #[test]
    fn a_slower_game_than_the_target_captures_every_frame() {
        // 目标 30 Hz 但游戏只跑 20 fps：每帧都该采（拿不到更多了），
        // 且不能因为「永远追不上」而进入补采爆发。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let frame = QPC / 20;
        for i in 0..40 {
            assert!(c.should_capture(i * frame), "第 {i} 帧应当采");
        }
    }

    #[test]
    fn out_of_range_targets_are_refused_not_clamped() {
        // 静默兜底会让「配错了」表现成「频率不对」，那是最难查的一类问题。
        assert!(Cadence::new(0.0, QPC).is_err());
        assert!(Cadence::new(-5.0, QPC).is_err());
        assert!(Cadence::new(1000.0, QPC).is_err());
        assert!(Cadence::new(f64::NAN, QPC).is_err());
        assert!(Cadence::new(f64::INFINITY, QPC).is_err());
        assert!(Cadence::new(30.0, 0).is_err());
    }

    #[test]
    fn every_capture_lands_on_an_even_frame_when_explicitly_required() {
        // 显式打开时才按 present 奇偶筛。默认关的理由见 should_capture_frame。
        let mut c = Cadence::new(30.0, QPC).unwrap().require_even_present(true);
        let frame = QPC / 187;
        for seq in 0..2000u64 {
            if c.should_capture_frame(seq, seq * frame) {
                assert!(seq.is_multiple_of(2), "第 {seq} 帧是奇数却被采了");
            }
        }
    }

    /// **这条测试是 2026-08-05 Fallout 4 实采的复现。**
    ///
    /// 19,566 个 RGB 帧里有 839 个重复帧。每出现一个重复帧，`session_frame` 与
    /// `present_seq` 的奇偶关系就永久错开一位。按 present 奇偶采样时：
    /// present 口径覆盖率 100%，而交付口径（session_frame 偶数）只有 60.75%。
    ///
    /// 这里用一个小规模等价场景把它钉住：每 5 个视频帧插一个重复帧
    /// （重复帧不消耗 present），看偶数视频帧里有多少配得到深度。
    #[test]
    fn gating_on_present_parity_misses_even_video_frames_once_duplicates_appear() {
        let frame_ticks = QPC / 30;
        // 造视频帧序列：(session_frame, 它对应的 present_seq)
        // 重复帧沿用上一帧的 present_seq，于是后续帧的奇偶关系整体挪一位
        let mut mapping = Vec::new();
        let mut present = 0u64;
        for session_frame in 0..600u64 {
            let is_duplicate = session_frame % 5 == 4;
            if !is_duplicate {
                present += 1;
            }
            mapping.push((session_frame, present));
        }

        let covered = |require_even: bool| {
            let mut c = Cadence::new(30.0, QPC)
                .unwrap()
                .require_even_present(require_even);
            // 探针按 present 顺序决定采不采
            let mut captured = std::collections::HashSet::new();
            let mut seen = std::collections::HashSet::new();
            for (session_frame, present_seq) in &mapping {
                if seen.insert(*present_seq)
                    && c.should_capture_frame(*present_seq, session_frame * frame_ticks)
                {
                    captured.insert(*present_seq);
                }
            }
            // 交付要的是「偶数 session_frame 有没有深度」
            let eligible: Vec<_> = mapping
                .iter()
                .filter(|(sf, _)| sf.is_multiple_of(2))
                .collect();
            let hit = eligible
                .iter()
                .filter(|(_, ps)| captured.contains(ps))
                .count();
            hit as f64 / eligible.len() as f64
        };

        let with_gate = covered(true);
        let without_gate = covered(false);
        assert!(
            with_gate < 0.8,
            "按 present 奇偶筛应当漏掉大量偶数视频帧，实得覆盖率 {with_gate:.2}"
        );
        assert!(
            without_gate > with_gate,
            "去掉 present 奇偶筛应当提高交付口径覆盖率：{without_gate:.2} vs {with_gate:.2}"
        );
    }

    /// 默认必须是「不按 present 奇偶筛」——正确性不该依赖「全程零重复帧」这个
    /// 探针验不了的前提。
    #[test]
    fn the_even_present_gate_is_off_by_default() {
        let mut c = Cadence::new(30.0, QPC).unwrap();
        // 第一帧必采，即使它是奇数 present
        assert!(c.should_capture_frame(1, 0));
    }

    #[test]
    fn asking_on_odd_frames_does_not_shift_the_phase() {
        // 短路顺序写反（先问时间再判奇偶）会在奇数帧上消耗掉一拍，
        // 实际频率掉到目标值的一半左右。这个测试专门钉住那个错误。
        let mut with_odds = Cadence::new(30.0, QPC).unwrap();
        let frame = QPC / 60;
        let hits = (0..600u64)
            .filter(|&seq| with_odds.should_capture_frame(seq, seq * frame))
            .count();
        // 10 秒 @60fps，目标 30 Hz、且只在偶数帧上采 → 偶数帧恰好 30 Hz，应当 ≈300
        assert!(
            (299..=301).contains(&hits),
            "10 秒采了 {hits} 次，期望 ≈300"
        );
    }

    #[test]
    fn on_a_lane_paced_game_it_captures_every_even_frame_exactly_like_the_old_code() {
        // 兼容性锚点：游戏跑 60 fps、目标 30 Hz 时，新代码必须与旧的
        // `present_seq % 2 == 0` 给出**逐帧相同**的结论。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let frame = QPC / 60;
        for seq in 0..600u64 {
            assert_eq!(
                c.should_capture_frame(seq, seq * frame),
                seq.is_multiple_of(2),
                "第 {seq} 帧与旧行为不一致"
            );
        }
    }

    #[test]
    fn an_uncapped_game_is_thinned_but_still_only_on_even_frames() {
        // 187 fps：偶数帧本身就有 93.5 Hz，时间基要把它压到 30 Hz。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let frame = QPC / 187;
        let hits = (0..187u64)
            .filter(|&seq| c.should_capture_frame(seq, seq * frame))
            .count();
        assert!((28..=31).contains(&hits), "1 秒采了 {hits} 次，期望 ≈30");
    }

    #[test]
    fn the_period_is_floored_not_rounded() {
        // QPC=10MHz、目标 3 Hz → 3333333.33 tick，取 3333333。
        // 见 Cadence::new 里对「为什么往短的一边偏」的说明。
        let c = Cadence::new(3.0, QPC).unwrap();
        assert_eq!(c.period_ticks(), 3_333_333);
        // 会被四舍五入拉长的那一档：7 Hz → 1428571.43，floor 仍是 1428571
        // （round 也一样），换一个能区分的：QPC/6 = 1666666.67
        let c = Cadence::new(6.0, QPC).unwrap();
        assert_eq!(c.period_ticks(), 1_666_666, "四舍五入会给出 1666667");
    }

    #[test]
    fn frame_jitter_does_not_halve_the_rate_on_a_lane_paced_game() {
        // 没有早到容差时，这个测试会掉到 ≈半速——目标周期与偶数帧间隔只差 0.33 tick，
        // 任何抖动都会让偶数帧「早到」被跳过、要等下一个偶数帧。
        // 抖动用确定性的伪随机（不能用 rand，测试必须可复现）。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let nominal = QPC as f64 / 60.0; // 60 fps
        let mut t = 0f64;
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut hits = 0;
        for seq in 0..600u64 {
            if c.should_capture_frame(seq, t as u64) {
                hits += 1;
            }
            // xorshift，抖动 ±0.5 ms
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let jitter = ((seed % 10_000) as f64 / 10_000.0 - 0.5) * (QPC as f64 * 0.001);
            t += nominal + jitter;
        }
        assert!(
            (295..=305).contains(&hits),
            "10 秒带抖动采了 {hits} 次，期望 ≈300（掉到 ≈150 说明容差没生效）"
        );
    }

    #[test]
    fn a_qpc_counter_near_overflow_does_not_panic() {
        // QPC 从开机起算，10 MHz 下 u64 用满要 5.8 万年，这个场景不会真发生。
        // 但 saturating 是免费的，而 debug 构建里的溢出 panic 发生在**游戏进程内**，
        // 表现为游戏崩溃——那个代价与「免费」不成比例。
        //
        // 只断言不 panic。计数器贴着上限时 `next_due` 已经饱和到 u64::MAX，
        // 之后返回什么都是饱和的产物，没有值得断言的语义。
        let mut c = Cadence::new(30.0, QPC).unwrap();
        let _ = c.should_capture(u64::MAX - 10);
        let _ = c.should_capture(u64::MAX - 9);
        let _ = c.should_capture(u64::MAX);
        let _ = c.should_capture_frame(u64::MAX - 1, u64::MAX);
    }
}
