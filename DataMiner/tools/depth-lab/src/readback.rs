//! 异步回读的环形池状态机
//! （[77 号 §5.4](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
//!
//! # 为什么这块要单独抽出来测
//!
//! §5.4 的规则只有一句话——「**宁可丢 depth，不可卡游戏**」——但它是整个方案里
//! 后果最脏的一条（风险登记 R-1）：同步 `Map` = 一次全 GPU 栅栏 = 一次可见卡顿，
//! 而卡顿会被 WGC 录进**同一份配对数据的 RGB 那一半**。丢 depth 只丢一半，卡顿两半全废。
//!
//! 状态机本身与 D3D 无关：槽位轮转、查询就绪、超时丢弃、丢帧计数。
//! 把它抽成纯逻辑之后，Windows 侧只剩「发一次 `CopyResource`、问一次 `GetData`、
//! 做一次 `Map`」三个薄调用，撞墙时归因清楚。

use serde::{Deserialize, Serialize};

/// 环形池槽位的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// 空闲，可接收新的 `CopyResource`。
    Free,
    /// 已发出拷贝，等待 GPU 完成。`frame` 是发起时的 `present_seq`。
    InFlight { present_seq: u64, issued_tick: u64 },
    /// GPU 已就绪，CPU 正在跨多个 tick 分片拷出（#504：整块 memcpy 会把
    /// 就绪帧的 Present 拖出 0.3ms+ 的定额税，切片摊薄后单帧无感）。
    ///
    /// 与 `InFlight` 的 deadline 语义分开：InFlight 的超时丈量「GPU 是否卡死」，
    /// 而分片拷出由调用方逐 tick 确定性推进，不存在可等待的外部事件——
    /// [`ReadbackPool::DRAIN_GRACE_TICKS`] 只是调用方逻辑错误的兜底，
    /// 正常运行中**永不触发**。
    Draining { present_seq: u64, since_tick: u64 },
    /// CPU 已经完整拷出，但还不能交付：池中仍有更早的 `present_seq` 没完成。
    ///
    /// 槽位在按序交付前保持占用，既禁止 staging 被新帧覆盖，也让跨多次 poll 的
    /// 重排缓存天然受池大小约束。否则单纯把字节塞进外部 map、立刻归还槽位，
    /// 一个迟迟不完成的老帧会让缓存无界增长。
    Ready { present_seq: u64 },
}

/// 一次 `poll` 里就绪的一个槽位：调用方应 `Map` 出来，槽位已自动归还。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    pub slot: usize,
    pub present_seq: u64,
}

/// 发起拷贝的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    /// 占用了这个槽位，调用方应对它发 `CopyResource` + 挂 query。
    Accepted { slot: usize },
    /// 池子满了——**丢这一帧 depth，不阻塞渲染线程**。
    DroppedPoolFull,
}

/// 丢帧原因，全部计入 `depth_dropped` 并进 manifest（§5.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DropStats {
    /// 池满：GPU 没跟上，或消费侧没及时归还。
    pub pool_full: u64,
    /// 超过 `deadline_ticks` 仍未就绪，强制回收槽位。
    pub timed_out: u64,
    /// 分片拷出超过宽限仍未结束，强制回收槽位。调用方逐 tick 推进分片，
    /// 这里非零意味着调用方逻辑 bug——它是诊断信号，不是正常丢帧路径。
    /// `serde(default)`：旧 manifest 没有这个字段，读入按 0 处理。
    #[serde(default)]
    pub drain_aborted: u64,
}

impl DropStats {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.pool_full + self.timed_out + self.drain_aborted
    }
}

/// 异步回读池。
///
/// `deadline_ticks` 的单位由调用方定义（probe 侧用输出帧周期数）。
/// 超过它仍未就绪的槽位会被强制回收——**宁可丢，不可等**。
#[derive(Debug, Clone)]
pub struct ReadbackPool {
    slots: Vec<Slot>,
    deadline_ticks: u64,
    tick: u64,
    stats: DropStats,
    issued: u64,
    completed: u64,
}

impl ReadbackPool {
    /// §5.4 要求环形池 ≥4 张。少于这个数在 60fps 下会频繁池满。
    pub const MIN_SLOTS: usize = 4;

    /// 分片拷出（[`Slot::Draining`]）的兜底宽限，单位与 `deadline_ticks` 相同。
    ///
    /// 取值只需满足「远大于最大合理分片数」：720p R32F ≈ 3.7MB，按最小
    /// 64KiB 片也不过 60 片，而正常配置是 8 片。超过它说明推进逻辑本身坏了。
    pub const DRAIN_GRACE_TICKS: u64 = 256;

    /// `slots` 会被强制抬到 [`MIN_SLOTS`]。
    #[must_use]
    pub fn new(slots: usize, deadline_ticks: u64) -> Self {
        Self {
            slots: vec![Slot::Free; slots.max(Self::MIN_SLOTS)],
            deadline_ticks: deadline_ticks.max(1),
            tick: 0,
            stats: DropStats::default(),
            issued: 0,
            completed: 0,
        }
    }

    /// 推进一个时间刻度（probe 侧在每个 `Present` 调用一次）。
    ///
    /// 顺带回收超时槽位——这是「不可等」的执行点。两类超时分开丈量：
    /// `InFlight` 用 `deadline_ticks`（GPU 卡死检测），`Draining` 用
    /// [`Self::DRAIN_GRACE_TICKS`]（调用方推进逻辑的兜底）。`Ready` 已经完成拷出，
    /// 只等前序帧，所以不超时；它会一直占住原槽，直到按序交付或整代显式 abort。
    pub fn advance_tick(&mut self) {
        self.tick += 1;
        let deadline = self.deadline_ticks;
        let now = self.tick;
        for s in &mut self.slots {
            match *s {
                Slot::InFlight { issued_tick, .. }
                    if now.saturating_sub(issued_tick) > deadline =>
                {
                    *s = Slot::Free;
                    self.stats.timed_out += 1;
                }
                Slot::Draining { since_tick, .. }
                    if now.saturating_sub(since_tick) > Self::DRAIN_GRACE_TICKS =>
                {
                    *s = Slot::Free;
                    self.stats.drain_aborted += 1;
                }
                _ => {}
            }
        }
    }

    /// 尝试为 `present_seq` 占一个槽位。
    #[must_use]
    pub fn issue(&mut self, present_seq: u64) -> Issue {
        match self.slots.iter().position(|s| *s == Slot::Free) {
            Some(slot) => {
                self.slots[slot] = Slot::InFlight {
                    present_seq,
                    issued_tick: self.tick,
                };
                self.issued += 1;
                Issue::Accepted { slot }
            }
            None => {
                self.stats.pool_full += 1;
                Issue::DroppedPoolFull
            }
        }
    }

    /// 用「该槽位的 GPU 查询是否就绪」的判定函数扫一遍池子。
    ///
    /// `is_ready` 对应 Windows 侧的
    /// `Map(..., D3D11_MAP_FLAG_DO_NOT_WAIT)` 是否返回 `DXGI_ERROR_WAS_STILL_DRAWING`
    /// （官方语义见 77 号 §14-③）。**本函数不阻塞**。
    pub fn poll<F: Fn(usize) -> bool>(&mut self, is_ready: F) -> Vec<Ready> {
        let mut out = Vec::new();
        for (slot, s) in self.slots.iter_mut().enumerate() {
            if let Slot::InFlight { present_seq, .. } = *s
                && is_ready(slot)
            {
                *s = Slot::Free;
                out.push(Ready { slot, present_seq });
            }
        }
        self.completed += out.len() as u64;
        out
    }

    /// 把整块拷出的槽位标成「已完成、等待按序交付」。
    ///
    /// 与 [`Self::poll`] 的区别是：本方法**不归还槽位**。调用方把对应帧保存在
    /// 有界缓存后，反复调用 [`Self::take_oldest_ready`]；只有池中最老的帧也已
    /// Ready 时才会释放，因而跨多次 drain 仍保持 `present_seq` 严格递增。
    #[must_use]
    pub fn buffer_ready(&mut self, slot: usize) -> bool {
        let Some(Slot::InFlight { present_seq, .. }) = self.slots.get(slot).copied() else {
            return false;
        };
        self.slots[slot] = Slot::Ready { present_seq };
        self.completed += 1;
        true
    }

    /// 把一个就绪槽位从 `InFlight` 转入分片拷出（`Draining`）。
    ///
    /// 返回 false 表示槽位已不在飞（例如恰在本 tick 被 deadline 回收），
    /// 调用方应放弃这一帧——**不猜、不补**。
    #[must_use]
    pub fn begin_drain(&mut self, slot: usize) -> bool {
        match self.slots.get(slot) {
            Some(Slot::InFlight { present_seq, .. }) => {
                self.slots[slot] = Slot::Draining {
                    present_seq: *present_seq,
                    since_tick: self.tick,
                };
                true
            }
            _ => false,
        }
    }

    /// 分片拷出完成：归还槽位并计入 `completed`。
    ///
    /// 返回 false 表示槽位并不处于 `Draining`（宽限回收或调用方状态错乱），
    /// 这一帧**不得**按成功交付处理。
    #[must_use]
    pub fn finish_drain(&mut self, slot: usize) -> bool {
        if matches!(self.slots.get(slot), Some(Slot::Draining { .. })) {
            self.slots[slot] = Slot::Free;
            self.completed += 1;
            true
        } else {
            false
        }
    }

    /// 分片拷出完成，但先保留槽位，等待 [`Self::take_oldest_ready`] 按序交付。
    #[must_use]
    pub fn buffer_finished_drain(&mut self, slot: usize) -> bool {
        let Some(Slot::Draining { present_seq, .. }) = self.slots.get(slot).copied() else {
            return false;
        };
        self.slots[slot] = Slot::Ready { present_seq };
        self.completed += 1;
        true
    }

    /// 若池中 `present_seq` 最小的槽位已经 Ready，归还它并返回归属信息。
    ///
    /// 若更老的槽位还在 GPU 等待或分片拷出，则返回 `None`，即使后面的帧已经
    /// Ready。调用方可在后续 tick 再试；Ready 槽不可复用，所以等待有严格上界。
    pub fn take_oldest_ready(&mut self) -> Option<Ready> {
        let (slot, oldest) = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, state)| match state {
                Slot::InFlight { present_seq, .. }
                | Slot::Draining { present_seq, .. }
                | Slot::Ready { present_seq } => Some((slot, *present_seq)),
                Slot::Free => None,
            })
            .min_by_key(|(_, present_seq)| *present_seq)?;

        if matches!(self.slots[slot], Slot::Ready { present_seq } if present_seq == oldest) {
            self.slots[slot] = Slot::Free;
            Some(Ready {
                slot,
                present_seq: oldest,
            })
        } else {
            None
        }
    }

    /// 分片拷出中途放弃（Map 失败、RowPitch 漂移等 fail-closed 场景）：
    /// 归还槽位并计入 `drain_aborted`，不计 `completed`。
    pub fn abort_drain(&mut self, slot: usize) {
        if matches!(self.slots.get(slot), Some(Slot::Draining { .. })) {
            self.slots[slot] = Slot::Free;
            self.stats.drain_aborted += 1;
        }
    }

    /// 槽位是否仍处于**属于这一帧**的分片拷出。
    ///
    /// 调用方每拷一片前都要过这道门：宽限回收后槽位可能已被新帧复用、
    /// staging 内容随之改写，继续拷会静默污染数据——校验失败就丢弃半成品。
    #[must_use]
    pub fn is_draining(&self, slot: usize, present_seq: u64) -> bool {
        matches!(
            self.slots.get(slot),
            Some(Slot::Draining { present_seq: seq, .. }) if *seq == present_seq
        )
    }

    /// 当前在飞（等待 GPU）的槽位及其归属帧号。**不含** `Draining` 槽位。
    ///
    /// probe 侧用它来逐个尝试 `Map(..., DO_NOT_WAIT)`：Map 成功即就绪，
    /// 返回 `DXGI_ERROR_WAS_STILL_DRAWING` 即未就绪。分片中的槽位已经就绪，
    /// 由调用方自己的进度记录跟踪，不再参与就绪探测。
    #[must_use]
    pub fn in_flight_slots(&self) -> Vec<(usize, u64)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                Slot::InFlight { present_seq, .. } => Some((i, *present_seq)),
                Slot::Free | Slot::Draining { .. } | Slot::Ready { .. } => None,
            })
            .collect()
    }

    /// 池仍然持有的**全部**帧号：`InFlight` + `Draining` + 等待按序交付的 `Ready`。
    ///
    /// 与帧同生命周期的旁路证据（pass 证据、投影观测）必须活到帧真正交付，
    /// 所以证据清理要用这个全集，而不是 [`Self::in_flight_slots`]。
    #[must_use]
    pub fn held_present_seqs(&self) -> Vec<u64> {
        self.slots
            .iter()
            .filter_map(|s| match s {
                Slot::InFlight { present_seq, .. }
                | Slot::Draining { present_seq, .. }
                | Slot::Ready { present_seq } => Some(*present_seq),
                Slot::Free => None,
            })
            .collect()
    }

    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| !matches!(s, Slot::Free))
            .count()
    }

    #[must_use]
    pub fn stats(&self) -> DropStats {
        self.stats
    }

    #[must_use]
    pub fn issued(&self) -> u64 {
        self.issued
    }

    #[must_use]
    pub fn completed(&self) -> u64 {
        self.completed
    }

    /// 实际交付率 = 完成 / (完成 + 丢弃)。
    ///
    /// 注意这**不是** [`crate::pairing::PairingReport::coverage`]：那个是配对表算出来的、
    /// 以 RGB 帧为分母的最终覆盖率，是硬门；这个只是 probe 侧的自检，用于在
    /// Overlay 上实时提示（§6.3「不允许录了两小时才发现覆盖率 0.4」）。
    #[must_use]
    pub fn delivery_rate(&self) -> f64 {
        let denom = self.completed + self.stats.total();
        if denom == 0 {
            1.0
        } else {
            self.completed as f64 / denom as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_the_documented_minimum_pool_size() {
        // §5.4 要求 ≥4 张
        let p = ReadbackPool::new(1, 4);
        assert_eq!(p.slots.len(), ReadbackPool::MIN_SLOTS);
    }

    #[test]
    fn issues_and_completes_in_the_happy_path() {
        let mut p = ReadbackPool::new(4, 4);
        assert_eq!(p.issue(100), Issue::Accepted { slot: 0 });
        assert_eq!(p.in_flight(), 1);
        let ready = p.poll(|_| true);
        assert_eq!(
            ready,
            vec![Ready {
                slot: 0,
                present_seq: 100
            }]
        );
        assert_eq!(p.in_flight(), 0);
        assert_eq!(p.stats().total(), 0);
    }

    #[test]
    fn drops_instead_of_blocking_when_the_pool_is_full() {
        // 这是「宁可丢 depth，不可卡游戏」的核心断言
        let mut p = ReadbackPool::new(4, 100);
        for i in 0..4 {
            assert!(matches!(p.issue(i), Issue::Accepted { .. }));
        }
        assert_eq!(p.issue(99), Issue::DroppedPoolFull);
        assert_eq!(p.stats().pool_full, 1);
        // 关键：池满不改变已在飞的槽位，也不等待
        assert_eq!(p.in_flight(), 4);
    }

    #[test]
    fn reclaims_slots_that_blow_the_deadline() {
        let mut p = ReadbackPool::new(4, 3);
        let _ = p.issue(1);
        for _ in 0..4 {
            p.advance_tick();
        }
        assert_eq!(p.stats().timed_out, 1);
        assert_eq!(p.in_flight(), 0, "超时槽位必须被回收，否则池子会永久漏光");
    }

    #[test]
    fn a_stalled_gpu_degrades_to_dropping_not_to_stalling() {
        // GPU 完全不返回：池子应当持续丢帧，但永远不阻塞、也不泄漏槽位
        let mut p = ReadbackPool::new(4, 2);
        for i in 0..100 {
            p.advance_tick();
            let _ = p.issue(i);
            let ready = p.poll(|_| false); // GPU 从不就绪
            assert!(ready.is_empty());
        }
        assert!(p.stats().total() > 0);
        assert!(p.in_flight() <= 4, "槽位泄漏：in_flight={}", p.in_flight());
        assert_eq!(p.completed(), 0);
        assert!(p.delivery_rate() < 0.01);
    }

    #[test]
    fn partial_readiness_frees_only_the_ready_slots() {
        let mut p = ReadbackPool::new(4, 100);
        for i in 0..4 {
            let _ = p.issue(i);
        }
        let ready = p.poll(|slot| slot % 2 == 0);
        assert_eq!(ready.len(), 2);
        assert_eq!(p.in_flight(), 2);
        // 归还的槽位可以立刻复用
        assert!(matches!(p.issue(50), Issue::Accepted { .. }));
    }

    #[test]
    fn buffered_completion_waits_across_polls_for_the_older_present() {
        // #792 现场顺序：15706 先完成、15704 后完成。单次 drain 内排序不够，
        // 因为老帧可能到下一次调用才 Ready；池必须跨调用保留新帧。
        let mut p = ReadbackPool::new(4, 100);
        assert_eq!(p.issue(15_704), Issue::Accepted { slot: 0 });
        assert_eq!(p.issue(15_706), Issue::Accepted { slot: 1 });

        assert!(p.buffer_ready(1));
        assert_eq!(
            p.take_oldest_ready(),
            None,
            "15704 未完成时不得越过它交付 15706"
        );
        assert_eq!(p.in_flight(), 2, "等待交付的 Ready 槽仍属于池");
        assert_eq!(p.held_present_seqs(), vec![15_704, 15_706]);

        // 模拟下一次 drain 才拿到旧帧；此时一次释放连续 Ready 前缀。
        assert!(p.buffer_ready(0));
        assert_eq!(
            p.take_oldest_ready(),
            Some(Ready {
                slot: 0,
                present_seq: 15_704
            })
        );
        assert_eq!(
            p.take_oldest_ready(),
            Some(Ready {
                slot: 1,
                present_seq: 15_706
            })
        );
        assert_eq!(p.take_oldest_ready(), None);
        assert_eq!(p.in_flight(), 0);
        assert_eq!(p.completed(), 2);
    }

    #[test]
    fn buffered_reorder_is_bounded_by_slots_and_does_not_reuse_staging() {
        let mut p = ReadbackPool::new(4, 100);
        for seq in 100..104 {
            assert!(matches!(p.issue(seq), Issue::Accepted { .. }));
        }
        // 最老的 100 仍在 GPU；其余三帧即使完成也只能占住原槽等待。
        for slot in 1..4 {
            assert!(p.buffer_ready(slot));
        }
        assert_eq!(p.take_oldest_ready(), None);
        assert_eq!(p.issue(104), Issue::DroppedPoolFull);
        assert_eq!(p.in_flight(), 4, "重排等待量不得超过池槽位数");

        assert!(p.buffer_ready(0));
        let delivered: Vec<u64> = std::iter::from_fn(|| p.take_oldest_ready())
            .map(|ready| ready.present_seq)
            .collect();
        assert_eq!(delivered, vec![100, 101, 102, 103]);
        assert_eq!(p.issue(104), Issue::Accepted { slot: 0 });
    }

    #[test]
    fn buffered_finished_drain_obeys_the_same_ordering_barrier() {
        let mut p = ReadbackPool::new(4, 100);
        assert_eq!(p.issue(7), Issue::Accepted { slot: 0 });
        assert_eq!(p.issue(8), Issue::Accepted { slot: 1 });
        assert!(p.begin_drain(0));
        assert!(p.buffer_ready(1));
        assert_eq!(p.take_oldest_ready(), None);

        assert!(p.buffer_finished_drain(0));
        assert_eq!(
            p.take_oldest_ready().map(|ready| ready.present_seq),
            Some(7)
        );
        assert_eq!(
            p.take_oldest_ready().map(|ready| ready.present_seq),
            Some(8)
        );
        assert_eq!(p.in_flight(), 0);
    }

    #[test]
    fn sustained_60hz_with_two_frame_latency_never_drops() {
        // 现实工况：Lane-60 每帧发一次，GPU 稳定滞后 2 tick
        let mut p = ReadbackPool::new(4, 4);
        let mut pending: Vec<(usize, u64)> = Vec::new();
        for seq in 0..600u64 {
            p.advance_tick();
            if let Issue::Accepted { slot } = p.issue(seq) {
                pending.push((slot, seq + 2));
            }
            let now = seq;
            let due: Vec<usize> = pending
                .iter()
                .filter(|(_, at)| *at <= now)
                .map(|(s, _)| *s)
                .collect();
            p.poll(|slot| due.contains(&slot));
            pending.retain(|(_, at)| *at > now);
        }
        assert_eq!(
            p.stats().total(),
            0,
            "2 帧延迟 + 4 槽位不应丢帧，实测 {:?}",
            p.stats()
        );
        assert!(p.delivery_rate() > 0.99);
    }

    #[test]
    fn in_flight_slots_reports_slot_and_owner() {
        let mut p = ReadbackPool::new(4, 100);
        let _ = p.issue(10);
        let _ = p.issue(20);
        assert_eq!(p.in_flight_slots(), vec![(0, 10), (1, 20)]);
        p.poll(|s| s == 0);
        assert_eq!(p.in_flight_slots(), vec![(1, 20)]);
    }

    #[test]
    fn delivery_rate_is_one_before_anything_happens() {
        let p = ReadbackPool::new(4, 4);
        assert!((p.delivery_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn begin_drain_shields_the_slot_from_the_inflight_deadline() {
        // #504：分片拷出跨多个 tick，绝不能被 InFlight 的 4-tick deadline 误杀。
        let mut p = ReadbackPool::new(4, 4);
        let _ = p.issue(7);
        assert!(p.begin_drain(0));
        for _ in 0..64 {
            p.advance_tick();
        }
        assert!(
            p.is_draining(0, 7),
            "Draining 槽位不受 InFlight deadline 约束"
        );
        assert_eq!(p.stats().timed_out, 0);
        assert_eq!(p.stats().drain_aborted, 0);
    }

    #[test]
    fn draining_slot_leaves_probing_but_stays_held_and_unissuable() {
        let mut p = ReadbackPool::new(4, 4);
        let _ = p.issue(7);
        let _ = p.issue(8);
        assert!(p.begin_drain(0));
        // 就绪探测列表里不再出现（不该被反复 Map 探测）……
        assert_eq!(p.in_flight_slots(), vec![(1, 8)]);
        // ……但证据生命周期看到的全集里必须还有它；
        let mut held = p.held_present_seqs();
        held.sort_unstable();
        assert_eq!(held, vec![7, 8]);
        // 槽位也不可被新帧复用。
        assert_eq!(p.issue(9), Issue::Accepted { slot: 2 });
        assert_eq!(p.in_flight(), 3);
    }

    #[test]
    fn finish_drain_frees_the_slot_and_counts_completed() {
        let mut p = ReadbackPool::new(4, 4);
        let _ = p.issue(7);
        assert!(p.begin_drain(0));
        assert!(p.finish_drain(0));
        assert_eq!(p.completed(), 1);
        assert_eq!(p.in_flight(), 0);
        assert_eq!(p.stats().total(), 0);
        // 状态错乱时 finish 必须失败——这一帧不得按成功交付。
        assert!(!p.finish_drain(0));
        assert_eq!(p.completed(), 1);
    }

    #[test]
    fn abort_drain_frees_the_slot_without_counting_completed() {
        let mut p = ReadbackPool::new(4, 4);
        let _ = p.issue(7);
        assert!(p.begin_drain(0));
        p.abort_drain(0);
        assert_eq!(p.completed(), 0);
        assert_eq!(p.stats().drain_aborted, 1);
        assert_eq!(p.in_flight(), 0);
        assert!(matches!(p.issue(9), Issue::Accepted { slot: 0 }));
    }

    #[test]
    fn drain_grace_reclaims_a_stuck_drain_and_flags_it() {
        // 兜底路径：推进逻辑坏死时槽位不能永久泄漏。
        let mut p = ReadbackPool::new(4, 4);
        let _ = p.issue(7);
        assert!(p.begin_drain(0));
        for _ in 0..=ReadbackPool::DRAIN_GRACE_TICKS {
            p.advance_tick();
        }
        assert!(!p.is_draining(0, 7));
        assert_eq!(p.stats().drain_aborted, 1);
        assert_eq!(p.in_flight(), 0, "宽限超时后槽位必须可复用");
    }

    #[test]
    fn is_draining_rejects_a_recycled_slot_owned_by_a_newer_frame() {
        // 宽限回收 → 新帧复用同一槽位：旧帧的分片校验必须失败，
        // 否则继续拷会读到新帧的 staging 内容——静默数据污染。
        let mut p = ReadbackPool::new(4, 4);
        let _ = p.issue(7);
        assert!(p.begin_drain(0));
        for _ in 0..=ReadbackPool::DRAIN_GRACE_TICKS {
            p.advance_tick();
        }
        assert_eq!(p.issue(100), Issue::Accepted { slot: 0 });
        assert!(!p.is_draining(0, 7));
        let _ = p.begin_drain(0);
        assert!(p.is_draining(0, 100));
        assert!(!p.is_draining(0, 7), "槽位复用后旧帧号必须被拒绝");
    }

    #[test]
    fn begin_drain_refuses_slots_that_are_not_in_flight() {
        let mut p = ReadbackPool::new(4, 4);
        assert!(!p.begin_drain(0), "Free 槽位不可进入 Draining");
        assert!(!p.begin_drain(99), "越界槽位不可进入 Draining");
        let _ = p.issue(7);
        assert!(p.begin_drain(0));
        assert!(!p.begin_drain(0), "重复 begin 必须失败");
    }

    #[test]
    fn drop_stats_deserializes_legacy_manifests_without_drain_aborted() {
        // 旧 manifest 没有 drain_aborted 字段：serde(default) 兼容读入。
        let legacy = r#"{"pool_full":3,"timed_out":1}"#;
        let stats: DropStats = serde_json::from_str(legacy).expect("旧格式必须可读");
        assert_eq!(stats.pool_full, 3);
        assert_eq!(stats.timed_out, 1);
        assert_eq!(stats.drain_aborted, 0);
        assert_eq!(stats.total(), 4);
    }
}
