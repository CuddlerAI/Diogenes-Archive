use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::perf::present_exit::{ExitSlot, FrameCommit, PresentExit};

const SECTION_COUNT: usize = 12;
/// 每帧 hook 入口计数的槽位数（见 [`Counter`]）。
const COUNTER_COUNT: usize = 4;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Section {
    Draw = 0,
    VsSet = 1,
    Map = 2,
    Pose = 3,
    /// unbind 时的唯一目标 pass 快照 CopyResource（渲染线程提交开销）。
    DsvSnapshot = 4,
    /// 旧 cadence 路径的 GPU 池化：CS dispatch + pool→staging CopyResource 提交开销。
    PoolDispatch = 5,
    /// drain 里的 `Map(DO_NOT_WAIT)` 调用本身（含 STILL_DRAWING 的失败查询）。
    DrainMap = 6,
    ReadbackMemcpy = 7,
    /// Camera/Player 指针链周期审计里的 VirtualQuery 部分（后台聚合，不归入某一帧）。
    PoseAudit = 8,
    /// formal 帧的水印写入（GetBuffer + 2×ClearView，#502 缓存后的常驻残余）。
    Watermark = 9,
    /// selected-Present 模式每个 Present 的 pool dispatch + candidate-ring CopyResource。
    CandidateMaterialize = 10,
    /// selected-Present 决策命中后 candidate-ring 到 staging 的提交开销。
    SelectedReadback = 11,
}

/// 每帧 hook 入口计数——为"浅路径税 × 重帧放大"假设服务：三轮修复后 p99 增量
/// 恒定 0.38–0.47 ms、整段尾部右移、平均零损失，指纹指向每 draw/Map/VSSet 的
/// 常数在 draw 数暴涨的重帧上被乘出来。浅路径本身不可计时（QPC 比路径还贵），
/// 但可以计数：离线回归「帧时 vs 入口数」的斜率（D 组对 idle-hook 对照组），
/// 斜率差 × 入口数就是每帧税。
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Counter {
    /// Draw* 七入口在采集期进入 hook 的次数（含 foreign context 与非证据帧）。
    Draw = 0,
    /// Map/Unmap/UpdateSubresource 三入口。
    Map = 1,
    /// VSSetConstantBuffers。
    VsSet = 2,
    /// OMSetRenderTargets（含 UAV 变体）。
    Om = 3,
}

impl Counter {
    const ALL: [Self; COUNTER_COUNT] = [Self::Draw, Self::Map, Self::VsSet, Self::Om];

    const fn label(self) -> &'static str {
        match self {
            Self::Draw => "draw_entries",
            Self::Map => "map_entries",
            Self::VsSet => "vs_entries",
            Self::Om => "om_entries",
        }
    }
}

impl Section {
    const ALL: [Self; SECTION_COUNT] = [
        Self::Draw,
        Self::VsSet,
        Self::Map,
        Self::Pose,
        Self::DsvSnapshot,
        Self::PoolDispatch,
        Self::DrainMap,
        Self::ReadbackMemcpy,
        Self::PoseAudit,
        Self::Watermark,
        Self::CandidateMaterialize,
        Self::SelectedReadback,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Draw => "draw",
            Self::VsSet => "vs_set",
            Self::Map => "map",
            Self::Pose => "pose",
            Self::DsvSnapshot => "dsv_snapshot",
            Self::PoolDispatch => "pool_dispatch",
            Self::DrainMap => "drain_map",
            Self::ReadbackMemcpy => "readback_memcpy",
            Self::PoseAudit => "pose_audit",
            Self::Watermark => "watermark",
            Self::CandidateMaterialize => "candidate_materialize",
            Self::SelectedReadback => "selected_readback",
        }
    }
}

/// 本帧发生了什么。dump 里按位名展开，离线分箱的第一把刀。
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum FrameFlag {
    /// 本 Present 发出了证据帧回读（快照已选、dispatch/copy 已提交）。
    Capture = 1,
    /// 本 Present drain 出了至少一帧（做了 Map 成功 + memcpy）。
    Drain = 2,
    /// 本 Present 跑了 camera hook 完整性审计。
    Audit = 4,
    /// 本 Present drain 出的帧里 readback latency ≥ 2。
    Latency2 = 8,
    /// 本 Present 执行了 Camera 指针链审计（60k 相位）。
    AuditCamera = 16,
    /// 兼容旧 dump 的保留位；后台 Player 审计不得设置逐 Present 标志。
    AuditPlayer = 32,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_DIAGNOSTIC_EPOCH: AtomicU64 = AtomicU64::new(1);
static DIAGNOSTIC_EPOCH: AtomicU64 = AtomicU64::new(0);
static SLOW_HOOK_DROPPED: AtomicU64 = AtomicU64::new(0);
const SLOW_HOOK_CAPACITY: usize = 8_192;
pub const SLOW_HOOK_THRESHOLD_NS: u64 = 2_000_000;

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlowHookKind {
    DescriptorCopy,
    DrawRedundancyCheck,
    ExecutePrepare,
    ExecuteOriginal,
    ExecuteComplete,
    ExecuteHook,
    SelectedDraw,
    SelectedDrawLockWait,
    SelectedDrawLocked,
    ExecuteQueryPrefetch,
    ExecutePrepareLocked,
}

#[derive(Clone, Copy, serde::Serialize)]
pub struct SlowHookEvent {
    pub kind: SlowHookKind,
    pub thread_id: u32,
    /// QPC sampled only AFTER an already-timed call exceeds the threshold.
    pub observed_end_qpc: i64,
    pub duration_ns: u64,
    /// Brackets descheduling between the existing elapsed sample and QPC read.
    /// Offline correlation must show this uncertainty, not invent exact starts.
    pub alignment_uncertainty_ns: u64,
    /// v2: optional per-call CP2077 detail. Never emitted for unrelated hook kinds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execute_prepare: Option<crate::execute_prepare::PrepareSample>,
    /// v5: per-copy phases and the slowest shard acquisition, without raw addresses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor_copy: Option<crate::descriptor_timing::DescriptorCopySample>,
}

pub fn record_slow_hook(epoch: u64, event: SlowHookEvent) {
    if epoch == 0 || epoch != diagnostic_epoch() || event.observed_end_qpc <= 0 {
        return;
    }
    // Only >=2ms calls reach here. Reuse the existing ring's NONBLOCKING lock;
    // never queue a game thread behind Present, reset or diagnostic file IO.
    let Ok(mut guard) = FRAME_RING.try_lock() else {
        SLOW_HOOK_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if let Some(ring) = guard.as_mut()
        && ring.epoch == epoch
        && !ring.window_closed
        && event.duration_ns >= SLOW_HOOK_THRESHOLD_NS
    {
        if ring.slow_hooks.len() < SLOW_HOOK_CAPACITY {
            ring.slow_hooks.push(event);
        } else {
            SLOW_HOOK_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn diagnostic_epoch() -> u64 {
    DIAGNOSTIC_EPOCH.load(Ordering::Acquire)
}
static COUNTS: [AtomicU64; SECTION_COUNT] = [const { AtomicU64::new(0) }; SECTION_COUNT];
static NANOS: [AtomicU64; SECTION_COUNT] = [const { AtomicU64::new(0) }; SECTION_COUNT];
/// 本帧分段累加器；Present 尾由 [`commit_frame`] swap(0) 收进环。
static FRAME_NANOS: [AtomicU64; SECTION_COUNT] = [const { AtomicU64::new(0) }; SECTION_COUNT];
static FRAME_FLAGS: AtomicU32 = AtomicU32::new(0);
/// First QPC eligible for the certification result. The control thread sets
/// this only after probe Ready, leaving a fixed operator-return settle window.
static CERTIFICATION_WINDOW_START_QPC: AtomicI64 = AtomicI64::new(0);
/// Fixed end of the 300-second certified window. The probe schedules this at
/// Ready so a delayed UI pause or teardown hitch cannot move the finish line.
static CERTIFICATION_WINDOW_END_QPC: AtomicI64 = AtomicI64::new(0);
/// 本帧 hook 入口计数累加器；Present 尾由 [`commit_frame`] swap(0) 收进环。
static FRAME_COUNTS: [AtomicU32; COUNTER_COUNT] = [const { AtomicU32::new(0) }; COUNTER_COUNT];

pub fn initialize_disabled() {
    ENABLED.store(false, Ordering::Release);
    prepare_rings();
}

/// Apply the immutable per-generation StartRun contract and rebuild the
/// bounded timing rings before capture can be armed.
pub fn begin_run(enabled: bool) {
    CERTIFICATION_WINDOW_START_QPC.store(0, Ordering::Release);
    CERTIFICATION_WINDOW_END_QPC.store(0, Ordering::Release);
    ENABLED.store(enabled, Ordering::Release);
    prepare_rings();
}

/// Disable hot-path timing and release all run-owned buffers. This must run on
/// every terminal path so a later generation cannot inherit A3 state.
pub fn finish_run() {
    CERTIFICATION_WINDOW_START_QPC.store(0, Ordering::Release);
    CERTIFICATION_WINDOW_END_QPC.store(0, Ordering::Release);
    ENABLED.store(false, Ordering::Release);
    prepare_rings();
}

#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub struct Scope {
    section: Section,
    started: Option<Instant>,
    include_in_frame: bool,
}

#[must_use]
pub fn scope(section: Section) -> Scope {
    Scope {
        section,
        started: ENABLED.load(Ordering::Relaxed).then(Instant::now),
        include_in_frame: true,
    }
}

/// Measure control/worker-thread work in aggregate totals without racing the
/// render thread's per-Present accumulators.
#[must_use]
pub fn aggregate_scope(section: Section) -> Scope {
    Scope {
        section,
        started: ENABLED.load(Ordering::Relaxed).then(Instant::now),
        include_in_frame: false,
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        let index = self.section as usize;
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        COUNTS[index].fetch_add(1, Ordering::Relaxed);
        NANOS[index].fetch_add(nanos, Ordering::Relaxed);
        if self.include_in_frame {
            FRAME_NANOS[index].fetch_add(nanos, Ordering::Relaxed);
        }
    }
}

/// 给本帧打事件标：开销一次原子 OR，关闭时一次原子读。
/// hook 入口计数：开销一次原子读 + 一次原子加，浅路径上可负担（计时不行）。
pub fn count(counter: Counter) {
    if ENABLED.load(Ordering::Relaxed) {
        FRAME_COUNTS[counter as usize].fetch_add(1, Ordering::Relaxed);
    }
}

pub fn mark_frame(flag: FrameFlag) {
    if ENABLED.load(Ordering::Relaxed) {
        FRAME_FLAGS.fetch_or(flag as u32, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct FrameRecord {
    present_seq: u64,
    qpc: i64,
    flags: u32,
    hook_pre_ns: u32,
    hook_post_ns: u32,
    sections_ns: [u32; SECTION_COUNT],
    counts: [u32; COUNTER_COUNT],
    present_call: Option<PresentCallTiming>,
    exit_index: Option<usize>,
}

/// Diagnostic QPC boundaries only. None of these replace FrameRecord::qpc in
/// certification. Identity and the row's own write-once exit slot allow offline
/// association across render threads without guessing a previous callback.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct PresentCallTiming {
    pub epoch: u64,
    pub generation: u64,
    pub swapchain_key: usize,
    pub thread_id: u32,
    pub entry_qpc: i64,
    pub entry_lock_acquired_qpc: i64,
    pub original_begin_qpc: i64,
    pub original_end_qpc: i64,
    pub commit_begin_qpc: i64,
    pub commit_lock_acquired_qpc: i64,
    /// Sampled under the ring lock after insertion; does not include unlocking.
    pub commit_recorded_qpc: i64,
    pub exit: Option<PresentExit>,
}

struct FrameRing {
    records: Box<[FrameRecord]>,
    exits: Arc<[ExitSlot]>,
    epoch: u64,
    slow_hooks: Vec<SlowHookEvent>,
    slow_hook_dropped_start: u64,
    next: usize,
    wrapped: bool,
    committed: u64,
    measurement_cutoff_qpc: Option<i64>,
    window_closed: bool,
}

#[derive(Clone, Copy, Default)]
struct EncodeRecord {
    present_seq: u64,
    start_qpc: i64,
    duration_ns: u64,
    preparation: Option<(i64, u64)>,
}

struct EncodeRing {
    records: Box<[EncodeRecord]>,
    next: usize,
    wrapped: bool,
    committed: u64,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct ReadbackCopyRecord {
    pub present_seq: u64,
    pub start_qpc: i64,
    pub duration_ns: u64,
    pub map_ns: u64,
    pub copy_ns: u64,
    pub queue_wait_ns: u64,
}

struct ReadbackCopyRing {
    records: Box<[ReadbackCopyRecord]>,
    next: usize,
    committed: u64,
}

/// Ready 重置后，240 FPS 的 5 秒 settle + 300 秒测量 + 1 条边界记录需要
/// 73,201 条。预分配 131,072 条并保留余量；边界闭合后冻结帧证据，避免延迟
/// StopRun 覆盖测量窗口。超过真实容量仍标记 wrapped，不能签发完整性结论。
const FRAME_RING_CAPACITY: usize = 131_072;
const ENCODE_RING_CAPACITY: usize = 4_096;

static FRAME_RING: Mutex<Option<FrameRing>> = Mutex::new(None);
static ENCODE_RING: Mutex<Option<EncodeRing>> = Mutex::new(None);
static READBACK_COPY_RING: Mutex<Option<ReadbackCopyRing>> = Mutex::new(None);

/// StartRun 时调用：启用则（重新）分配环并清空游标，关闭则释放。
pub fn prepare_rings() {
    prepare_rings_with_cutoff(None);
}

fn prepare_rings_with_cutoff(measurement_cutoff_qpc: Option<i64>) {
    let enabled = ENABLED.load(Ordering::Acquire);
    DIAGNOSTIC_EPOCH.store(0, Ordering::Release);
    if let Ok(mut ring) = FRAME_RING.lock() {
        *ring = enabled.then(|| FrameRing {
            records: vec![FrameRecord::default(); FRAME_RING_CAPACITY].into_boxed_slice(),
            exits: (0..FRAME_RING_CAPACITY)
                .map(|_| ExitSlot::default())
                .collect::<Vec<_>>()
                .into(),
            epoch: NEXT_DIAGNOSTIC_EPOCH.fetch_add(1, Ordering::Relaxed),
            slow_hooks: Vec::with_capacity(SLOW_HOOK_CAPACITY),
            slow_hook_dropped_start: SLOW_HOOK_DROPPED.load(Ordering::Relaxed),
            next: 0,
            wrapped: false,
            committed: 0,
            measurement_cutoff_qpc,
            window_closed: false,
        });
        if let Some(ring) = ring.as_ref() {
            DIAGNOSTIC_EPOCH.store(ring.epoch, Ordering::Release);
        }
    }
    if let Ok(mut ring) = ENCODE_RING.lock() {
        *ring = enabled.then(|| EncodeRing {
            records: vec![EncodeRecord::default(); ENCODE_RING_CAPACITY].into_boxed_slice(),
            next: 0,
            wrapped: false,
            committed: 0,
        });
    }
    if let Ok(mut ring) = READBACK_COPY_RING.lock() {
        *ring = enabled.then(|| ReadbackCopyRing {
            records: vec![ReadbackCopyRecord::default(); ENCODE_RING_CAPACITY].into_boxed_slice(),
            next: 0,
            committed: 0,
        });
    }
    for slot in &FRAME_NANOS {
        slot.store(0, Ordering::Relaxed);
    }
    for slot in &FRAME_COUNTS {
        slot.store(0, Ordering::Relaxed);
    }
    FRAME_FLAGS.store(0, Ordering::Relaxed);
}

/// Reset all timing evidence at Ready and exclude the operator-return settle
/// interval from the certified window. WGC callbacks are deliberately absent
/// from this contract: changing the backbuffer changes WGC callback cadence.
pub fn begin_certification_window(
    ready_qpc: i64,
    qpc_frequency: i64,
    settle_seconds: u64,
    measurement_seconds: u64,
) -> Result<(), String> {
    if !ENABLED.load(Ordering::Acquire) {
        return Err(
            "cannot begin certification window while performance metrics are disabled".into(),
        );
    }
    if ready_qpc <= 0 || qpc_frequency <= 0 {
        return Err(format!(
            "invalid certification QPC basis ready={ready_qpc} frequency={qpc_frequency}"
        ));
    }
    if measurement_seconds == 0 {
        return Err("certification measurement duration must be positive".into());
    }
    let settle_ticks = i128::from(qpc_frequency)
        .checked_mul(i128::from(settle_seconds))
        .ok_or_else(|| "certification settle interval overflowed QPC".to_string())?;
    let start = i128::from(ready_qpc)
        .checked_add(settle_ticks)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| "certification window start overflowed QPC".to_string())?;
    let measurement_ticks = i128::from(qpc_frequency)
        .checked_mul(i128::from(measurement_seconds))
        .ok_or_else(|| "certification measurement interval overflowed QPC".to_string())?;
    let end = i128::from(start)
        .checked_add(measurement_ticks)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| "certification window end overflowed QPC".to_string())?;
    // Install the boundary with the new ring under the same lock. A Present
    // cannot freeze this window against the preceding run's global cutoff.
    prepare_rings_with_cutoff(Some(end));
    CERTIFICATION_WINDOW_END_QPC.store(end, Ordering::Release);
    CERTIFICATION_WINDOW_START_QPC.store(start, Ordering::Release);
    Ok(())
}

fn summarize_present_records(
    records: &[FrameRecord],
    wrapped: bool,
    window_start_qpc: i64,
    measurement_cutoff_qpc: i64,
    stop_cutoff_qpc: i64,
    qpc_frequency: i64,
) -> Result<depth_lab::PresentTimingSummary, String> {
    if window_start_qpc <= 0
        || measurement_cutoff_qpc <= window_start_qpc
        || stop_cutoff_qpc <= 0
        || qpc_frequency <= 0
    {
        return Err(format!(
            "invalid Present timing window start={window_start_qpc} measurement_cutoff={measurement_cutoff_qpc} stop_cutoff={stop_cutoff_qpc} frequency={qpc_frequency}"
        ));
    }
    let available = records
        .iter()
        .filter(|record| record.qpc <= stop_cutoff_qpc)
        .collect::<Vec<_>>();
    let mut settled = available
        .iter()
        .copied()
        .filter(|record| record.qpc >= window_start_qpc && record.qpc <= measurement_cutoff_qpc)
        .collect::<Vec<_>>();
    if stop_cutoff_qpc >= measurement_cutoff_qpc
        && settled
            .last()
            .is_some_and(|record| record.qpc < measurement_cutoff_qpc)
        && let Some(first_after_cutoff) = available
            .iter()
            .copied()
            .find(|record| record.qpc > measurement_cutoff_qpc)
    {
        // One post-cutoff Present closes the frame interval that straddles the
        // scheduled boundary. Later tail-guard frames stay excluded.
        settled.push(first_after_cutoff);
    }
    if stop_cutoff_qpc >= measurement_cutoff_qpc
        && (settled.len() < 2
            || settled
                .last()
                .is_none_or(|record| record.qpc < measurement_cutoff_qpc))
    {
        return Err(format!(
            "Present timing did not reach scheduled measurement cutoff {measurement_cutoff_qpc} before stop {stop_cutoff_qpc}"
        ));
    }
    // A manual pause during the five-second settle interval is a cancelled
    // measurement, not a capture-pipeline fault. Preserve the available
    // Ready-to-cutoff timing so the client can reject it as <300 seconds and
    // offer a retry. A certification-eligible run always has at least two
    // records after window_start_qpc and therefore uses only settled data.
    let retained = if settled.len() >= 2 {
        settled
    } else {
        available
    };
    if retained.len() < 2 {
        return Err(format!(
            "Present timing window has {} samples; at least 2 are required",
            retained.len()
        ));
    }
    let mut intervals = Vec::with_capacity(retained.len() - 1);
    for pair in retained.windows(2) {
        let duration = pair[1].qpc - pair[0].qpc;
        if duration <= 0 {
            return Err(format!(
                "Present QPC is not strictly increasing: {} then {}",
                pair[0].qpc, pair[1].qpc
            ));
        }
        intervals.push(depth_lab::PresentIntervalEvidence {
            start_present_seq: pair[0].present_seq,
            end_present_seq: pair[1].present_seq,
            start_qpc: pair[0].qpc,
            end_qpc: pair[1].qpc,
            duration_qpc: u64::try_from(duration).map_err(|_| "Present duration is negative")?,
        });
    }
    let qpf = u128::try_from(qpc_frequency).map_err(|_| "QPC frequency is not positive")?;
    let interval_count =
        u128::try_from(intervals.len()).map_err(|_| "too many Present intervals")?;
    let elapsed = u128::try_from(retained.last().unwrap().qpc - retained[0].qpc)
        .map_err(|_| "Present elapsed QPC is negative")?;
    let avg_millifps = interval_count
        .checked_mul(qpf)
        .and_then(|value| value.checked_mul(1_000))
        .ok_or_else(|| "average FPS calculation overflowed".to_string())?
        / elapsed;

    // Conventional 1% low: reciprocal of the average duration of the slowest
    // 1% of intervals. Fractional boundary weighting avoids a 99/100 or
    // 199/200 sample-count cliff without introducing floating point.
    intervals.sort_unstable_by(|left, right| {
        right
            .duration_qpc
            .cmp(&left.duration_qpc)
            .then_with(|| left.start_qpc.cmp(&right.start_qpc))
    });
    let interval_count_usize = intervals.len();
    let whole = interval_count_usize / 100;
    let remainder = interval_count_usize % 100;
    let mut slow_duration_scaled = intervals[..whole]
        .iter()
        .try_fold(0u128, |total, interval| {
            total.checked_add(u128::from(interval.duration_qpc).checked_mul(100)?)
        })
        .ok_or_else(|| "1% low duration calculation overflowed".to_string())?;
    if remainder != 0 {
        slow_duration_scaled = slow_duration_scaled
            .checked_add(
                u128::from(intervals[whole].duration_qpc)
                    .checked_mul(u128::try_from(remainder).unwrap_or(u128::MAX))
                    .ok_or_else(|| "1% low boundary calculation overflowed".to_string())?,
            )
            .ok_or_else(|| "1% low duration calculation overflowed".to_string())?;
    }
    if slow_duration_scaled == 0 {
        return Err("1% low duration is zero".into());
    }
    let low1_millifps = qpf
        .checked_mul(1_000)
        .and_then(|value| value.checked_mul(interval_count))
        .ok_or_else(|| "1% low FPS calculation overflowed".to_string())?
        / slow_duration_scaled;
    let slow_evidence_count = whole + usize::from(remainder != 0);
    let slowest_one_percent = intervals[..slow_evidence_count].to_vec();

    Ok(depth_lab::PresentTimingSummary {
        sample_count: u64::try_from(retained.len()).unwrap_or(u64::MAX),
        qpc_frequency: u64::try_from(qpc_frequency).map_err(|_| "QPC frequency is not positive")?,
        qpc_start: retained[0].qpc,
        qpc_end: retained.last().unwrap().qpc,
        measurement_window_start_qpc: window_start_qpc,
        measurement_cutoff_qpc,
        stop_cutoff_qpc,
        avg_millifps: u64::try_from(avg_millifps).unwrap_or(u64::MAX),
        low1_millifps: u64::try_from(low1_millifps).unwrap_or(u64::MAX),
        slowest_one_percent,
        wrapped,
    })
}

/// Stable Present-cadence summary for the probe-scheduled 300-second window.
/// StopRun is a later teardown boundary and can only shorten an early-cancelled
/// run; it can never extend a completed certification measurement.
pub fn present_timing_summary(
    cutoff_qpc: i64,
    qpc_frequency: i64,
) -> Result<Option<depth_lab::PresentTimingSummary>, String> {
    if !ENABLED.load(Ordering::Acquire) {
        return Ok(None);
    }
    let window_start_qpc = CERTIFICATION_WINDOW_START_QPC.load(Ordering::Acquire);
    let measurement_cutoff_qpc = CERTIFICATION_WINDOW_END_QPC.load(Ordering::Acquire);
    if window_start_qpc <= 0 || measurement_cutoff_qpc <= window_start_qpc {
        return Err("performance metrics enabled without a certification window".into());
    }
    let guard = FRAME_RING
        .lock()
        .map_err(|_| "perf frame ring lock poisoned")?;
    let ring = guard
        .as_ref()
        .ok_or("perf frame ring missing while performance metrics are enabled")?;
    let (head, tail) = if ring.wrapped {
        (&ring.records[ring.next..], &ring.records[..ring.next])
    } else {
        (&ring.records[..ring.next], &ring.records[..0])
    };
    let records = head.iter().chain(tail).copied().collect::<Vec<_>>();
    summarize_present_records(
        &records,
        ring.wrapped,
        window_start_qpc,
        measurement_cutoff_qpc,
        cutoff_qpc,
        qpc_frequency,
    )
    .map(Some)
}

/// Present hook 尾调用：收割本帧分段累加器与标志，写一条环记录。
///
/// `hook_pre_ns`/`hook_post_ns` 是 probe 在原始 Present 之前/之后占用的渲染线程时间；
/// 帧时不在这里算——离线用相邻记录的 `qpc` 差除以 QPF。
pub fn commit_frame(
    present_seq: u64,
    qpc: i64,
    hook_pre_ns: u64,
    hook_post_ns: u64,
    present_call: Option<PresentCallTiming>,
) -> Option<FrameCommit> {
    commit_frame_with_clock(
        present_seq,
        qpc,
        hook_pre_ns,
        hook_post_ns,
        present_call,
        || {
            let mut value = 0;
            let _ =
                unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut value) };
            value
        },
    )
}

fn commit_frame_with_clock(
    present_seq: u64,
    qpc: i64,
    hook_pre_ns: u64,
    hook_post_ns: u64,
    present_call: Option<PresentCallTiming>,
    mut diagnostic_clock: impl FnMut() -> i64,
) -> Option<FrameCommit> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let mut sections_ns = [0u32; SECTION_COUNT];
    for (index, slot) in sections_ns.iter_mut().enumerate() {
        *slot = u32::try_from(FRAME_NANOS[index].swap(0, Ordering::Relaxed)).unwrap_or(u32::MAX);
    }
    let flags = FRAME_FLAGS.swap(0, Ordering::Relaxed);
    let mut counts = [0u32; COUNTER_COUNT];
    for (index, slot) in counts.iter_mut().enumerate() {
        *slot = FRAME_COUNTS[index].swap(0, Ordering::Relaxed);
    }
    let mut record = FrameRecord {
        present_seq,
        qpc,
        flags,
        hook_pre_ns: u32::try_from(hook_pre_ns).unwrap_or(u32::MAX),
        hook_post_ns: u32::try_from(hook_post_ns).unwrap_or(u32::MAX),
        sections_ns,
        counts,
        present_call,
        exit_index: None,
    };
    let Ok(mut guard) = FRAME_RING.lock() else {
        return None;
    };
    if let Some(ring) = guard.as_mut() {
        // Keep draining the per-frame accumulators above, and leave writer /
        // readback instrumentation enabled until the normal run teardown.
        if ring.window_closed {
            return None;
        }
        record.present_call = record.present_call.filter(|call| call.epoch == ring.epoch);
        if let Some(timing) = record.present_call.as_mut() {
            timing.commit_lock_acquired_qpc = diagnostic_clock();
        }
        let record_index = ring.next;
        // Write-once slots are intentionally not reused after a wrap. Wrapped
        // evidence is already ineligible; retain missing endpoints explicitly.
        let exit = if record.present_call.is_some() && !ring.wrapped {
            record.exit_index = Some(record_index);
            Some((Arc::clone(&ring.exits), record_index))
        } else {
            None
        };
        ring.records[record_index] = record;
        ring.committed = ring.committed.saturating_add(1);
        ring.next += 1;
        if ring.next == ring.records.len() {
            ring.next = 0;
            ring.wrapped = true;
        }
        // The first Present at or beyond the cutoff closes the final interval.
        // Never clear wrapped: freezing cannot repair already overwritten data.
        ring.window_closed = ring
            .measurement_cutoff_qpc
            .is_some_and(|cutoff| qpc >= cutoff);
        if let Some(timing) = ring.records[record_index].present_call.as_mut() {
            timing.commit_recorded_qpc = diagnostic_clock();
        }
        return Some(FrameCommit { exit });
    }
    None
}

/// writer 线程的 EXR 编码计时（独立环；不混入渲染线程的帧累加器）。
/// `preparation` carries the preceding same-frame decode/verification/convention interval.
/// It is emitted separately in v2 diagnostics; the existing ACK/UI duration stays EXR-only.
pub fn record_encode(
    present_seq: u64,
    start_qpc: i64,
    duration_ns: u64,
    preparation: Option<(i64, u64)>,
) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Ok(mut guard) = ENCODE_RING.lock() else {
        return;
    };
    if let Some(ring) = guard.as_mut() {
        ring.records[ring.next] = EncodeRecord {
            present_seq,
            start_qpc,
            duration_ns,
            preparation,
        };
        ring.committed = ring.committed.saturating_add(1);
        ring.next += 1;
        if ring.next == ring.records.len() {
            ring.next = 0;
            ring.wrapped = true;
        }
    }
}

/// Worker time is separate from Present-thread sections. Called only by the live
/// candidate owner after validating the completed slot/Present identity.
pub(crate) fn record_readback_copy(record: ReadbackCopyRecord) {
    if !enabled() {
        return;
    }
    if let Ok(mut guard) = READBACK_COPY_RING.lock()
        && let Some(ring) = guard.as_mut()
    {
        ring.records[ring.next] = record;
        ring.next = (ring.next + 1) % ring.records.len();
        ring.committed = ring.committed.saturating_add(1);
    }
}

/// 当前 encode 环的稳定摘要。逐帧原始证据仍由 [`dump_rings`] 落盘；这个摘要
/// 随 Flushed 回传，供客户端在 A/B 结束后直接展示 p50/p95/p99/max。
pub fn encode_timing_summary() -> Option<depth_lab::WriterTimingSummary> {
    if !ENABLED.load(Ordering::Acquire) {
        return None;
    }
    let guard = ENCODE_RING.lock().ok()?;
    let ring = guard.as_ref()?;
    let (head, tail) = if ring.wrapped {
        (&ring.records[ring.next..], &ring.records[..ring.next])
    } else {
        (&ring.records[..ring.next], &ring.records[..0])
    };
    let mut durations_us = head
        .iter()
        .chain(tail)
        .map(|record| record.duration_ns.div_ceil(1_000))
        .collect::<Vec<_>>();
    let sample_count = u64::try_from(durations_us.len()).unwrap_or(u64::MAX);
    let over_budget_count = u64::try_from(
        durations_us
            .iter()
            .filter(|duration| **duration > 33_333)
            .count(),
    )
    .unwrap_or(u64::MAX);
    durations_us.sort_unstable();
    let at = |numerator: usize, denominator: usize| {
        if durations_us.is_empty() {
            return 0;
        }
        let rank = durations_us
            .len()
            .saturating_mul(numerator)
            .div_ceil(denominator)
            .saturating_sub(1)
            .min(durations_us.len() - 1);
        durations_us[rank]
    };
    Some(depth_lab::WriterTimingSummary {
        sample_count,
        total_committed: ring.committed,
        wrapped: ring.wrapped,
        over_budget_count,
        duration_us: depth_lab::LatencyQuantiles {
            p50: at(50, 100),
            p95: at(95, 100),
            p99: at(99, 100),
            max: durations_us.last().copied().unwrap_or(0),
        },
    })
}

fn flags_label(flags: u32) -> String {
    let mut parts = Vec::new();
    if flags & FrameFlag::Capture as u32 != 0 {
        parts.push("capture");
    }
    if flags & FrameFlag::Drain as u32 != 0 {
        parts.push("drain");
    }
    if flags & FrameFlag::Audit as u32 != 0 {
        parts.push("audit");
    }
    if flags & FrameFlag::Latency2 as u32 != 0 {
        parts.push("latency2");
    }
    if flags & FrameFlag::AuditCamera as u32 != 0 {
        parts.push("audit_camera");
    }
    if flags & FrameFlag::AuditPlayer as u32 != 0 {
        parts.push("audit_player");
    }
    parts.join("|")
}

/// 把两个环 dump 成 jsonl。首行是 meta（schema 与 QPF，供离线把 QPC 差换算成毫秒）。
///
/// 只在 StopRun 收尾调用（渲染线程已停止提交），环内容按时间顺序展开。
pub fn dump_rings(dir: &Path, qpf: i64) -> Result<(), String> {
    if !ENABLED.load(Ordering::Acquire) {
        return Ok(());
    }
    if qpf <= 0 {
        return Err(format!("invalid QPC frequency {qpf}"));
    }
    let frame_path = dir.join("perf-frames.jsonl");
    {
        let guard = FRAME_RING.lock().map_err(|_| "perf 帧环锁已中毒")?;
        let ring = guard
            .as_ref()
            .ok_or("perf frame ring missing while A3 metrics are enabled")?;
        if ring.committed == 0 {
            return Err("perf frame ring contains no committed Present".into());
        }
        let mut file = std::fs::File::create(&frame_path)
            .map_err(|error| format!("建 {} 失败: {error}", frame_path.display()))?;
        writeln!(
            file,
            "{{\"schema\":\"dm-perf-frames-v6\",\"qpf\":{qpf},\"capacity\":{},\"record_count\":{},\"total_committed\":{},\"wrapped\":{},\"certification_eligible\":{},\"measurement_cutoff_qpc\":{},\"window_closed\":{},\"sections\":[{}],\"counters\":[{}],\"present_call_scope\":\"diagnostic_qpc_only; entry_to_lock_includes_admission; own_write_once_exit_slot; cross_thread_same_epoch_swapchain_generation; not_gpu_timing\"}}",
            ring.records.len(),
            usize::try_from(ring.committed).unwrap_or(usize::MAX).min(ring.records.len()),
            ring.committed,
            ring.wrapped,
            !ring.wrapped,
            ring.measurement_cutoff_qpc.map_or_else(|| "null".into(), |qpc| qpc.to_string()),
            ring.window_closed,
            Section::ALL
                .iter()
                .map(|section| format!("\"{}\"", section.label()))
                .collect::<Vec<_>>()
                .join(","),
            Counter::ALL
                .iter()
                .map(|counter| format!("\"{}\"", counter.label()))
                .collect::<Vec<_>>()
                .join(",")
        )
        .map_err(|error| format!("写 perf meta 失败: {error}"))?;
        let (head, tail) = if ring.wrapped {
            (&ring.records[ring.next..], &ring.records[..ring.next])
        } else {
            (&ring.records[..ring.next], &ring.records[..0])
        };
        for record in head.iter().chain(tail) {
            let sections = record
                .sections_ns
                .iter()
                .map(|nanos| nanos.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let counts = record
                .counts
                .iter()
                .map(|count| count.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut call = record.present_call;
            if let Some(call) = call.as_mut() {
                call.exit = record
                    .exit_index
                    .and_then(|index| ring.exits[index].snapshot());
            }
            let present_call = serde_json::to_string(&call)
                .map_err(|error| format!("serialize Present call timing failed: {error}"))?;
            writeln!(
                file,
                "{{\"seq\":{},\"qpc\":{},\"flags\":\"{}\",\"hook_pre_ns\":{},\"hook_post_ns\":{},\"sections_ns\":[{}],\"counts\":[{}],\"present_call\":{present_call}}}",
                record.present_seq,
                record.qpc,
                flags_label(record.flags),
                record.hook_pre_ns,
                record.hook_post_ns,
                sections,
                counts
            )
            .map_err(|error| format!("写 perf 帧记录失败: {error}"))?;
        }
        let mut file = std::fs::File::create(dir.join("perf-slow-hooks.jsonl"))
            .map_err(|error| format!("create slow-hook evidence failed: {error}"))?;
        let dropped = SLOW_HOOK_DROPPED
            .load(Ordering::Relaxed)
            .saturating_sub(ring.slow_hook_dropped_start);
        writeln!(file, "{}", serde_json::json!({
            "schema": "dm-perf-slow-hooks-v5", "qpf": qpf, "epoch": ring.epoch,
            "record_count": ring.slow_hooks.len(), "capacity": SLOW_HOOK_CAPACITY,
            "threshold_ns": SLOW_HOOK_THRESHOLD_NS, "dropped_since_reset": dropped,
            "scope": "thresholded probe-side wall time; approximate QPC alignment with explicit uncertainty; concurrent durations not additive; absent events do not exclude cumulative short-call cost"
        })).map_err(|error| format!("write slow-hook header failed: {error}"))?;
        for event in &ring.slow_hooks {
            writeln!(
                file,
                "{}",
                serde_json::to_string(event)
                    .map_err(|error| format!("serialize slow-hook failed: {error}"))?
            )
            .map_err(|error| format!("write slow-hook failed: {error}"))?;
        }
    }
    let encode_path = dir.join("perf-encode.jsonl");
    let guard = ENCODE_RING.lock().map_err(|_| "perf 编码环锁已中毒")?;
    let ring = guard
        .as_ref()
        .ok_or("perf encode ring missing while A3 metrics are enabled")?;
    let mut file = std::fs::File::create(&encode_path)
        .map_err(|error| format!("建 {} 失败: {error}", encode_path.display()))?;
    writeln!(
        file,
        "{{\"schema\":\"dm-perf-encode-v2\",\"qpf\":{qpf},\"capacity\":{},\"record_count\":{},\"total_committed\":{},\"wrapped\":{},\"duration_scope\":\"exr_write\",\"preparation_scope\":\"decode_verify_convention\"}}",
        ring.records.len(),
        usize::try_from(ring.committed).unwrap_or(usize::MAX).min(ring.records.len()),
        ring.committed,
        ring.wrapped
    )
        .map_err(|error| format!("写 encode meta 失败: {error}"))?;
    let (head, tail) = if ring.wrapped {
        (&ring.records[ring.next..], &ring.records[..ring.next])
    } else {
        (&ring.records[..ring.next], &ring.records[..0])
    };
    for record in head.iter().chain(tail) {
        let preparation = record.preparation.map_or_else(
            || "null".to_owned(),
            |(start, duration)| format!("{{\"start_qpc\":{start},\"duration_ns\":{duration}}}"),
        );
        writeln!(
            file,
            "{{\"seq\":{},\"start_qpc\":{},\"duration_ns\":{},\"preparation\":{preparation}}}",
            record.present_seq, record.start_qpc, record.duration_ns
        )
        .map_err(|error| format!("写 encode 记录失败: {error}"))?;
    }
    drop(guard);
    let guard = READBACK_COPY_RING
        .lock()
        .map_err(|_| "readback copy timing ring poisoned")?;
    let ring = guard.as_ref().ok_or("readback copy timing ring missing")?;
    let count = usize::try_from(ring.committed)
        .unwrap_or(usize::MAX)
        .min(ring.records.len());
    let wrapped = ring.committed >= ring.records.len() as u64;
    let mut file = std::fs::File::create(dir.join("perf-readback-copy.jsonl"))
        .map_err(|error| format!("create readback timing file: {error}"))?;
    writeln!(file, "{{\"schema\":\"dm-perf-readback-copy-v1\",\"qpf\":{qpf},\"record_count\":{count},\"total_committed\":{},\"wrapped\":{wrapped},\"execution_thread\":\"worker\"}}", ring.committed)
        .map_err(|error| format!("write readback timing header: {error}"))?;
    for i in 0..count {
        let index = if wrapped {
            (ring.next + i) % ring.records.len()
        } else {
            i
        };
        serde_json::to_writer(&mut file, &ring.records[index])
            .map_err(|error| format!("serialize readback timing: {error}"))?;
        writeln!(file).map_err(|error| format!("write readback timing: {error}"))?;
    }
    Ok(())
}

pub fn take_summary() -> Option<String> {
    if !ENABLED.load(Ordering::Acquire) {
        return None;
    }
    let mut parts = Vec::with_capacity(SECTION_COUNT);
    for section in Section::ALL {
        let index = section as usize;
        let count = COUNTS[index].swap(0, Ordering::AcqRel);
        let nanos = NANOS[index].swap(0, Ordering::AcqRel);
        parts.push(format!(
            "{}:count={count},total_us={},avg_us={:.3}",
            section.label(),
            nanos / 1_000,
            if count == 0 {
                0.0
            } else {
                nanos as f64 / count as f64 / 1_000.0
            }
        ));
    }
    Some(parts.join(" "))
}
