//! 视角标定服务（docs/45 §4、docs/46 §4.1 / C3）：录制之外的"对表"流程
//! 事实源。状态机：Idle → Session{探针 + 测量计划} → 逐次测量（热键或
//! UI 按钮起止）→ finish：验收分析 → 证据文件原子落盘（含逐事件轨迹）
//! → journal `view_calibrations` 登记。前端只渲染本服务快照。
//!
//! 与录制互斥：Raw Input 注册是进程级按 usage 的，探针与录制监视不能
//! 同时活跃（raw_input_probe.rs 顶部说明）。本服务只在 collector 状态为
//! Bound（已绑定、未开始录制）时允许开始对表；C4 的 preflight 会再从
//! 录制侧加反向硬门。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail, ensure};
use capture_core::qpc::QpcClock;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;
use view_scale::evidence::{
    CalibrationEvidence, CalibrationKind, CatalogRef, Derived, EvidenceMeasurement, Method,
    PolicyRef, RawTraceRef, SCHEMA_VERSION,
};
use view_scale::{
    ALGORITHM_VERSION, Accumulator, Analysis, Direction, INPUT_TIMESTAMP_SEMANTICS,
    MeasurementRecord, MeasurementSummary, POLLING_ESTIMATOR_VERSION, Policy, Speed, Verdict,
    analyze,
};

use crate::domain::collector::CollectorState;
use crate::platform::windows::raw_input_probe::{ProbeEvent, RawInputProbe};
use crate::services::journal::{
    SessionJournal, ViewCalibrationAudit, ViewCalibrationKind, ViewCalibrationWrite,
};
use crate::services::orchestrator::{SessionOrchestrator, ViewScaleContext};

/// 快照契约版本，前端 `view-scale-contract.ts` 镜像。
const STATUS_SCHEMA_VERSION: &str = "dm-view-scale-status-3";
/// 目录政策缺省时的工程初值（docs/45 §7.1，研究定版前）。
const DEFAULT_TOLERANCE_PCT: f64 = 0.5;
const DEFAULT_FAST_SLOW_TOLERANCE_PCT: f64 = 1.0;
const DEFAULT_MIN_POLLING_HZ: f64 = 500.0;
const MIN_EVENTS_PER_MEASUREMENT: u64 = 100;
const TURNS_PER_MEASUREMENT: f64 = 3.0;
/// 单次测量轨迹的事件上限（8kHz 鼠标连续 4 分钟量级），超出按溢出作废。
const MAX_TRACE_EVENTS: usize = 2_000_000;
/// 一次自由会话的段数上限。自由会话不预设计划，而每段的逐事件轨迹在
/// finish 之前全留在内存里（每段上限 `MAX_TRACE_EVENTS`），不封顶的话
/// 一次会话就能把内存吃到 GB 量级。够一次坐下测完俯仰、左右缘与各档
/// 倍镜；到顶就完成本次会话再开一次。
const MAX_FREE_MEASUREMENTS: usize = 64;

pub struct ViewScaleService {
    app: AppHandle,
    orchestrator: Arc<SessionOrchestrator>,
    journal: Arc<SessionJournal>,
    app_data_root: PathBuf,
    inner: Mutex<Inner>,
    revision: AtomicU64,
}

#[derive(Default)]
struct Inner {
    session: Option<SessionState>,
    last_error: Option<String>,
    last_result: Option<CalibrationResult>,
}

struct SessionState {
    kind: ViewCalibrationKind,
    parent_calibration_id: Option<String>,
    context: ViewScaleContext,
    policy: Policy,
    expected_counts_per_360: Option<f64>,
    qpc_frequency: i64,
    session_start_qpc: i64,
    probe: RawInputProbe,
    /// 计划好的测量序列；自由会话为空（段数不预设）。
    plan: Vec<PlannedMeasurement>,
    completed: Vec<CompletedMeasurement>,
    active: Option<ActiveMeasurement>,
}

impl SessionState {
    const fn is_free(&self) -> bool {
        matches!(self.kind, ViewCalibrationKind::Free)
    }

    /// 下一段该做什么，`None` = 这个会话不能再加段了。
    ///
    /// 自由会话不按计划走——一次坐下通常要连测俯仰限位、画面左右缘、以及
    /// 按住开镜的若干圈，逼标注员每段重开一个会话只会多出一堆碎证据文件。
    /// 但仍有 `MAX_FREE_MEASUREMENTS` 的内存上限。
    fn next_planned(&self) -> Option<PlannedMeasurement> {
        if self.is_free() {
            return (self.completed.len() < MAX_FREE_MEASUREMENTS)
                .then_some(PlannedMeasurement::Free);
        }
        self.plan.get(self.completed.len()).copied()
    }
}

/// 计划中的一段测量。
#[derive(Clone, Copy)]
enum PlannedMeasurement {
    /// 预设方向与转速的整圈测量（`TURNS_PER_MEASUREMENT` 圈），进验收分析。
    Turns { direction: Direction, speed: Speed },
    /// 自由测量段（docs/60 §6 Q4）：不预设方向/转速/圈数，只累计计数并
    /// 如实报告。没有圈数就没有 `counts_per_360`，因此**不进验收分析、
    /// 不产裁决**——换算由标注员按 docs/59 的步骤自己做。
    Free,
}

impl PlannedMeasurement {
    const fn direction(self) -> Option<Direction> {
        match self {
            Self::Turns { direction, .. } => Some(direction),
            Self::Free => None,
        }
    }

    const fn speed(self) -> Option<Speed> {
        match self {
            Self::Turns { speed, .. } => Some(speed),
            Self::Free => None,
        }
    }

    const fn turns(self) -> Option<f64> {
        match self {
            Self::Turns { .. } => Some(TURNS_PER_MEASUREMENT),
            Self::Free => None,
        }
    }

    /// 计划型测量固定在腰射状态进行（ADS 随 H-1 扩展）。自由段做的是腰射
    /// 还是某档倍镜，只有标注员知道——采集侧看到的都只是计数，所以这里
    /// 必须留空而不是默认 "hipfire"。
    fn state_id(self) -> Option<String> {
        match self {
            Self::Turns { .. } => Some("hipfire".to_string()),
            Self::Free => None,
        }
    }
}

struct ActiveMeasurement {
    /// 直接持有计划项而不是 plan 下标——自由会话的 plan 是空的，
    /// 按下标索引会越界 panic。
    planned: PlannedMeasurement,
    accumulator: Accumulator,
    trace: Vec<ProbeEvent>,
    total_dx: i64,
    start_qpc: i64,
    device: Option<isize>,
    multi_mouse: bool,
    overflow_baseline: u64,
}

struct CompletedMeasurement {
    planned: PlannedMeasurement,
    summary: MeasurementSummary,
    trace: Vec<ProbeEvent>,
    start_marker_qpc: i64,
    end_marker_qpc: i64,
}

impl CompletedMeasurement {
    /// 只有整圈测量能构成验收分析的输入；自由段没有圈数，返回 `None`。
    fn record(&self) -> Option<MeasurementRecord> {
        match self.planned {
            PlannedMeasurement::Turns { direction, speed } => Some(MeasurementRecord {
                direction,
                speed,
                turns: TURNS_PER_MEASUREMENT,
                summary: self.summary.clone(),
            }),
            PlannedMeasurement::Free => None,
        }
    }

    fn buffered_report_count(&self) -> u64 {
        self.trace.iter().filter(|event| event.buffered).count() as u64
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CalibrationResult {
    calibration_id: String,
    kind: &'static str,
    /// `None` = 自由测量段：没有验收裁决可言。
    verdict: Option<String>,
    counts_per_360_measured: Option<f64>,
    deviation_pct: Option<f64>,
    repeat_spread_pct: Option<f64>,
    left_right_delta_pct: Option<f64>,
    fast_slow_delta_pct: Option<f64>,
    polling_hz_p50: Option<f64>,
    polling_estimator_version: &'static str,
    fail_reasons: Vec<String>,
    /// 逐段读数随结果一起返回。finish 会把 session 取走，此后
    /// `status.measurements` 就空了——而自由段的产出**只有**这些读数，
    /// 不放进结果里等于测完就丢。
    measurements: Vec<StatusMeasurement>,
    evidence_path: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusMeasurement {
    /// 自由段无声明方向/转速，为 null。
    direction: Option<&'static str>,
    speed: Option<&'static str>,
    /// 自由段没有圈数，算不出 `counts_per_360`，为 null。
    counts_per_360: Option<f64>,
    total_counts_x: i64,
    total_counts_y: i64,
    duration_ms: f64,
    /// 本段计数按目录常数换算成的角度（度，带符号，Raw Input 原始口径：
    /// 右为正、**下为正**）。目录常数缺失（测绘模式）时为 null。
    ///
    /// 这两个数就是自由段的用途所在（docs/60 §5.2）：
    /// - 画面左缘转到右缘 → `|sweptDegreesX|` 即水平视野，用来定 `fov_axis`；
    /// - 俯仰限位之间扫满 → `vertical_scale_ratio = 180 / |sweptDegreesY|`
    ///   （分母按 ratio=1 算，读到 ±180 就说明各向同性）。
    swept_degrees_x: Option<f64>,
    swept_degrees_y: Option<f64>,
    event_count: u64,
    polling_hz_p50: Option<f64>,
    polling_estimated_report_count: u64,
    delivery_group_count: u64,
    max_delivery_batch_size: u64,
    buffered_report_count: u64,
}

/// 前端渲染的完整快照。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewScaleStatus {
    schema_version: &'static str,
    revision: u64,
    bound: bool,
    applicable: Option<bool>,
    expected_counts_per_360: Option<f64>,
    session_active: bool,
    kind: Option<&'static str>,
    plan_total: usize,
    completed_count: usize,
    measuring: bool,
    current_direction: Option<&'static str>,
    current_speed: Option<&'static str>,
    current_counts: i64,
    current_events: u64,
    overflow_count: u64,
    multi_mouse: bool,
    measurements: Vec<StatusMeasurement>,
    last_error: Option<String>,
    result: Option<CalibrationResult>,
}

impl ViewScaleService {
    pub fn new(app: AppHandle, orchestrator: Arc<SessionOrchestrator>) -> Self {
        let journal = orchestrator.upload_journal();
        let app_data_root = orchestrator.app_data_root();
        Self {
            app,
            orchestrator,
            journal,
            app_data_root,
            inner: Mutex::new(Inner::default()),
            revision: AtomicU64::new(0),
        }
    }

    /// 当前快照；顺带把探针队列排空进活动测量（UI 轮询即驱动）。
    pub fn status(&self) -> ViewScaleStatus {
        let mut inner = self.inner.lock();
        drain_probe(&mut inner);
        self.build_status(&inner)
    }

    /// 标定历史（新→旧），供向导页历史区渲染。
    pub fn history(
        &self,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::services::journal::ViewCalibrationAudit>> {
        self.journal.list_view_calibrations(limit)
    }

    /// 开始一次对表会话（full = 全量矩阵，spot_check = 单圈抽检，
    /// free = 自由测量段，docs/60 §6 Q4）。
    pub fn begin_session(&self, kind: &str) -> anyhow::Result<ViewScaleStatus> {
        let kind = match kind {
            "full" => ViewCalibrationKind::Full,
            "spot_check" => ViewCalibrationKind::SpotCheck,
            "free" => ViewCalibrationKind::Free,
            other => bail!("VIEW_SCALE_UNKNOWN_KIND: {other}"),
        };
        let mut inner = self.inner.lock();
        ensure!(
            inner.session.is_none(),
            "VIEW_SCALE_SESSION_ACTIVE: finish or cancel the current calibration first"
        );
        let context = self
            .orchestrator
            .view_scale_context()
            .context("VIEW_SCALE_NOT_BOUND: bind a game window before calibrating")?;
        ensure!(
            context.collector_state == CollectorState::Bound,
            "VIEW_SCALE_COLLECTOR_BUSY: calibration requires a bound, not-yet-recording target \
             (collector state {:?}); the raw-input probe and the recording monitor must never \
             coexist",
            context.collector_state
        );
        if let Some(policy) = &context.policy {
            ensure!(
                policy.applicable,
                "VIEW_SCALE_NOT_APPLICABLE: this catalog declares view-scale calibration \
                 not applicable (pointer-driven game)"
            );
        }
        let parent_calibration_id = match kind {
            // 自由段是测绘，不挂在任何标定下面（journal 的
            // CHECK((kind='spot_check') = (parent IS NOT NULL)) 同样要求为空）。
            ViewCalibrationKind::Full | ViewCalibrationKind::Free => None,
            ViewCalibrationKind::SpotCheck => {
                let parent = self
                    .journal
                    .latest_valid_view_calibration(
                        &context.game_id,
                        &context.recording_profile_hash,
                        &context.device_profile_hash,
                    )?
                    .context(
                        "VIEW_SCALE_NO_FULL_CALIBRATION: run a full calibration before spot checks",
                    )?;
                load_registered_evidence(&self.app_data_root, &parent).context(
                    "VIEW_SCALE_PARENT_EVIDENCE_OUTDATED: the full calibration uses an old or invalid polling method; run a new full calibration",
                )?;
                Some(parent.calibration_id)
            }
        };
        let policy = engine_policy(&context);
        let expected = context
            .policy
            .as_ref()
            .and_then(|policy| policy.counts_per_360);
        let clock = QpcClock::new()?;
        let session_start_qpc = clock.now()?;
        let probe = RawInputProbe::start()?;
        inner.session = Some(SessionState {
            kind,
            parent_calibration_id,
            context,
            policy,
            expected_counts_per_360: expected,
            qpc_frequency: clock.frequency(),
            session_start_qpc,
            probe,
            plan: build_plan(kind),
            completed: Vec::new(),
            active: None,
        });
        inner.last_error = None;
        inner.last_result = None;
        Ok(self.publish(&mut inner))
    }

    /// 热键与 UI 共用的起/止测量入口。
    pub fn toggle_measurement(&self) -> anyhow::Result<ViewScaleStatus> {
        let mut inner = self.inner.lock();
        drain_probe(&mut inner);
        {
            let session = inner
                .session
                .as_mut()
                .context("VIEW_SCALE_NO_SESSION: begin a calibration session first")?;
            if session.active.is_none() {
                let planned = session.next_planned().context(
                    "VIEW_SCALE_PLAN_DONE: this session cannot take another measurement (the \
                     planned set is complete, or a free session hit its segment cap); finish it",
                )?;
                // 丢弃标记之前积压的移动，测量段从此刻起才算数。
                while session.probe.receiver().try_recv().is_ok() {}
                let start_qpc = QpcClock::new()?.now()?;
                session.active = Some(ActiveMeasurement {
                    planned,
                    accumulator: Accumulator::new(session.qpc_frequency),
                    trace: Vec::new(),
                    total_dx: 0,
                    start_qpc,
                    device: None,
                    multi_mouse: false,
                    overflow_baseline: session.probe.overflow_count(),
                });
            } else {
                let end_qpc = QpcClock::new()?.now()?;
                let active = session.active.take().expect("checked above");
                #[cfg(feature = "local-catalog-only")]
                let mut active = active;
                #[cfg(feature = "local-catalog-only")]
                seed_local_automation_measurement(&mut active, session.qpc_frequency);
                let planned = active.planned;
                let overflowed = session.probe.overflow_count() > active.overflow_baseline
                    || active.trace.len() >= MAX_TRACE_EVENTS;
                if overflowed || active.multi_mouse {
                    // fail-closed：溢出或混入第二只鼠标 → 本次测量作废重做。
                    let reason = if active.multi_mouse {
                        "VIEW_SCALE_MULTI_MOUSE: a second mouse moved during the measurement; redo it"
                    } else {
                        "VIEW_SCALE_QUEUE_OVERFLOW: probe queue overflowed; redo the measurement"
                    };
                    drop(active);
                    inner.last_error = Some(reason.to_string());
                    return Ok(self.publish(&mut inner));
                }
                session.completed.push(CompletedMeasurement {
                    planned,
                    summary: active.accumulator.finish(),
                    trace: active.trace,
                    start_marker_qpc: active.start_qpc,
                    end_marker_qpc: end_qpc,
                });
            }
        }
        inner.last_error = None;
        Ok(self.publish(&mut inner))
    }

    /// 作废当前进行中的测量（不影响已完成的）。
    pub fn abort_measurement(&self) -> anyhow::Result<ViewScaleStatus> {
        let mut inner = self.inner.lock();
        let session = inner
            .session
            .as_mut()
            .context("VIEW_SCALE_NO_SESSION: begin a calibration session first")?;
        session.active = None;
        inner.last_error = None;
        Ok(self.publish(&mut inner))
    }

    /// 取消整个会话（探针随 SessionState 一起 Drop 并反注册）。
    pub fn cancel_session(&self) -> anyhow::Result<ViewScaleStatus> {
        let mut inner = self.inner.lock();
        inner.session = None;
        inner.last_error = None;
        Ok(self.publish(&mut inner))
    }

    /// 完成会话：验收分析（自由段跳过）→ 证据落盘 → journal 登记。
    pub fn finish_session(&self) -> anyhow::Result<ViewScaleStatus> {
        let mut inner = self.inner.lock();
        drain_probe(&mut inner);
        let session = inner
            .session
            .take()
            .context("VIEW_SCALE_NO_SESSION: begin a calibration session first")?;
        ensure!(
            session.active.is_none(),
            "VIEW_SCALE_MEASUREMENT_ACTIVE: end the current measurement first"
        );
        if session.is_free() {
            // 自由会话没有计划，但空证据毫无意义。
            ensure!(
                !session.completed.is_empty(),
                "VIEW_SCALE_NO_MEASUREMENTS: record at least one free measurement first"
            );
        } else {
            ensure!(
                session.completed.len() == session.plan.len(),
                "VIEW_SCALE_PLAN_INCOMPLETE: {} of {} measurements recorded",
                session.completed.len(),
                session.plan.len()
            );
        }
        match self.persist_session(session) {
            Ok(result) => {
                inner.last_result = Some(result);
                inner.last_error = None;
                Ok(self.publish(&mut inner))
            }
            Err(error) => {
                inner.last_error = Some(format!("{error:#}"));
                self.publish(&mut inner);
                Err(error)
            }
        }
    }

    fn persist_session(&self, session: SessionState) -> anyhow::Result<CalibrationResult> {
        // 自由段不进验收分析：没有声明圈数就算不出 counts_per_360，
        // 判什么都是编的。裁决因此缺席（None），而不是拿 fail 占位。
        let analysis = if session.is_free() {
            None
        } else {
            let records: Vec<MeasurementRecord> = session
                .completed
                .iter()
                .filter_map(CompletedMeasurement::record)
                .collect();
            // fail-closed：计划型会话里混进无圈数的段，说明状态机被改坏了。
            // 宁可拒绝落盘，也不能少喂一段给 analyze——那会让"测量不足"
            // 变成一个看起来正常的通过结果。
            ensure!(
                records.len() == session.completed.len(),
                "VIEW_SCALE_PLAN_CORRUPT: {} of {} measurements carry no turn count",
                session.completed.len() - records.len(),
                session.completed.len()
            );
            Some(analyze(&records, &session.policy))
        };

        let calibration_id = Uuid::new_v4().to_string();
        let relative_dir = format!("calibrations/{calibration_id}");
        let absolute_dir = self.app_data_root.join(&relative_dir);
        std::fs::create_dir_all(&absolute_dir)
            .with_context(|| format!("failed to create {}", absolute_dir.display()))?;

        let mut measurements = Vec::new();
        for (index, completed) in session.completed.iter().enumerate() {
            let trace_name = format!("m{index:02}.trace.jsonl");
            let mut trace_bytes = Vec::new();
            for event in &completed.trace {
                // 原始设备句柄不落盘（与录制端会话盐化
                // 纪律一致，探针句柄仅用于会话内多鼠标检测）。
                trace_bytes.extend_from_slice(
                    format!(
                        "{{\"qpc\":{},\"dx\":{},\"dy\":{},\"buffered\":{},\"delivery_batch_size\":{}}}\n",
                        event.qpc,
                        event.dx,
                        event.dy,
                        event.buffered,
                        event.delivery_batch_size
                    )
                    .as_bytes(),
                );
            }
            let trace_sha256 = write_atomic(&absolute_dir.join(&trace_name), &trace_bytes)?;
            measurements.push(EvidenceMeasurement {
                state_id: completed.planned.state_id(),
                direction: completed.planned.direction(),
                speed: completed.planned.speed(),
                turns: completed.planned.turns(),
                total_counts_x: completed.summary.total_dx,
                total_counts_y: completed.summary.total_dy,
                event_count: completed.summary.event_count,
                duration_ms: completed.summary.duration_ms,
                polling_hz_p50: completed.summary.polling_hz_p50,
                polling_hz_p05: completed.summary.polling_hz_p95,
                polling_estimated_report_count: completed.summary.polling_estimated_report_count,
                delivery_group_count: completed.summary.delivery_group_count,
                max_delivery_batch_size: completed.summary.max_delivery_batch_size,
                buffered_report_count: completed.buffered_report_count(),
                start_marker_qpc: completed.start_marker_qpc,
                end_marker_qpc: completed.end_marker_qpc,
                raw_trace: RawTraceRef {
                    file: format!("{relative_dir}/{trace_name}"),
                    sha256: trace_sha256,
                },
            });
        }

        let evidence = CalibrationEvidence {
            schema_version: SCHEMA_VERSION.to_string(),
            calibration_id: calibration_id.clone(),
            kind: match session.kind {
                ViewCalibrationKind::Full => CalibrationKind::Full,
                ViewCalibrationKind::SpotCheck => CalibrationKind::SpotCheck,
                ViewCalibrationKind::Free => CalibrationKind::Free,
            },
            parent_calibration_id: session.parent_calibration_id.clone(),
            created_utc: iso_utc_now(),
            qpc_anchor: session.session_start_qpc,
            qpc_frequency: session.qpc_frequency,
            game_id: session.context.game_id.clone(),
            catalog_ref: CatalogRef {
                catalog_version: session.context.catalog_version.clone(),
                recording_profile_id: session.context.recording_profile_id.clone(),
                recording_profile_hash: session.context.recording_profile_hash.clone(),
                device_profile_id: session.context.device_profile_id.clone(),
                device_profile_hash: session.context.device_profile_hash.clone(),
            },
            policy: PolicyRef {
                expected_counts_per_360: session.policy.expected_counts_per_360,
                tolerance_pct: session.policy.tolerance_pct,
                fast_slow_tolerance_pct: session.policy.fast_slow_tolerance_pct,
                min_polling_hz: session.policy.min_polling_hz,
            },
            method: Method {
                algorithm_version: ALGORITHM_VERSION.to_string(),
                polling_estimator_version: POLLING_ESTIMATOR_VERSION.to_string(),
                input_timestamp_semantics: INPUT_TIMESTAMP_SEMANTICS.to_string(),
                // 自由段不预设圈数。
                turns_per_measurement: (!session.is_free()).then_some(TURNS_PER_MEASUREMENT),
            },
            measurements,
            derived: match &analysis {
                Some(analysis) => Derived::from_analysis(analysis),
                None => Derived::unanalyzed(),
            },
            verdict: analysis.as_ref().map(|analysis| analysis.verdict),
            fail_reasons: analysis
                .as_ref()
                .map(|analysis| analysis.fail_reasons.clone())
                .unwrap_or_default(),
        };
        let evidence_json = evidence
            .to_canonical_json()
            .context("failed to serialize the calibration evidence")?;
        let evidence_relative = format!("{relative_dir}/input-calibration.json");
        let evidence_sha256 = write_atomic(
            &absolute_dir.join("input-calibration.json"),
            evidence_json.as_bytes(),
        )?;

        self.journal
            .record_view_calibration(&ViewCalibrationWrite {
                calibration_id: &calibration_id,
                kind: session.kind,
                parent_calibration_id: session.parent_calibration_id.as_deref(),
                game_id: &session.context.game_id,
                recording_profile_hash: &session.context.recording_profile_hash,
                device_profile_hash: &session.context.device_profile_hash,
                created_qpc: session.session_start_qpc,
                verdict_pass: analysis
                    .as_ref()
                    .map(|analysis| analysis.verdict == Verdict::Pass),
                counts_per_360_measured: analysis
                    .as_ref()
                    .and_then(|analysis| analysis.counts_per_360_measured),
                deviation_pct: analysis
                    .as_ref()
                    .and_then(|analysis| analysis.deviation_pct),
                polling_hz_p50: analysis
                    .as_ref()
                    .and_then(|analysis| analysis.polling_hz_p50),
                file_path: &evidence_relative,
                file_sha256: &evidence_sha256,
            })?;

        Ok(calibration_result(
            &calibration_id,
            session.kind,
            analysis.as_ref(),
            session
                .completed
                .iter()
                .map(|completed| status_measurement(completed, session.expected_counts_per_360))
                .collect(),
            &evidence_relative,
        ))
    }

    fn publish(&self, inner: &mut Inner) -> ViewScaleStatus {
        let status = self.build_status(inner);
        let _ = self.app.emit("view-scale://status", &status);
        status
    }

    fn build_status(&self, inner: &Inner) -> ViewScaleStatus {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let context = self.orchestrator.view_scale_context();
        let (applicable, expected) = match &context {
            Some(context) => match &context.policy {
                Some(policy) => (Some(policy.applicable), policy.counts_per_360),
                None => (None, None),
            },
            None => (None, None),
        };
        let mut status = ViewScaleStatus {
            schema_version: STATUS_SCHEMA_VERSION,
            revision,
            bound: context.is_some(),
            applicable,
            expected_counts_per_360: expected,
            session_active: false,
            kind: None,
            plan_total: 0,
            completed_count: 0,
            measuring: false,
            current_direction: None,
            current_speed: None,
            current_counts: 0,
            current_events: 0,
            overflow_count: 0,
            multi_mouse: false,
            measurements: Vec::new(),
            last_error: inner.last_error.clone(),
            result: inner.last_result.clone(),
        };
        if let Some(session) = &inner.session {
            status.session_active = true;
            status.kind = Some(session.kind.as_str());
            // 自由会话不预设段数，plan_total = 0 即"不限"（前端按 kind 渲染）。
            status.plan_total = session.plan.len();
            status.completed_count = session.completed.len();
            status.overflow_count = session.probe.overflow_count();
            status.expected_counts_per_360 = session.expected_counts_per_360;
            status.measurements = session
                .completed
                .iter()
                .map(|completed| status_measurement(completed, session.expected_counts_per_360))
                .collect();
            if let Some(active) = &session.active {
                status.measuring = true;
                status.current_direction = active.planned.direction().map(direction_str);
                status.current_speed = active.planned.speed().map(speed_str);
                status.current_counts = active.total_dx;
                status.current_events = active.trace.len() as u64;
                status.multi_mouse = active.multi_mouse;
            } else if let Some(planned) = session.next_planned() {
                status.current_direction = planned.direction().map(direction_str);
                status.current_speed = planned.speed().map(speed_str);
            }
        }
        status
    }
}

/// 逐段读数。`expected_counts_per_360` 是**目录常数**（测绘模式下为 None），
/// 只用于把计数换算成角度给人看，不参与任何判定。
fn status_measurement(
    completed: &CompletedMeasurement,
    expected_counts_per_360: Option<f64>,
) -> StatusMeasurement {
    // 换算本身在平台无关的 crate 里（可在非 Windows 机上回归）。
    let swept_degrees = |counts: i64| {
        expected_counts_per_360
            .and_then(|counts_per_360| view_scale::swept_degrees(counts, counts_per_360))
    };
    StatusMeasurement {
        direction: completed.planned.direction().map(direction_str),
        speed: completed.planned.speed().map(speed_str),
        // 圈数是 counts_per_360 的分母；自由段没有圈数，这一项只能缺席。
        counts_per_360: completed
            .planned
            .turns()
            .map(|turns| completed.summary.total_dx.unsigned_abs() as f64 / turns),
        total_counts_x: completed.summary.total_dx,
        total_counts_y: completed.summary.total_dy,
        duration_ms: completed.summary.duration_ms,
        swept_degrees_x: swept_degrees(completed.summary.total_dx),
        swept_degrees_y: swept_degrees(completed.summary.total_dy),
        event_count: completed.summary.event_count,
        polling_hz_p50: completed.summary.polling_hz_p50,
        polling_estimated_report_count: completed.summary.polling_estimated_report_count,
        delivery_group_count: completed.summary.delivery_group_count,
        max_delivery_batch_size: completed.summary.max_delivery_batch_size,
        buffered_report_count: completed.buffered_report_count(),
    }
}

/// 读取并复核 journal 指向的标定：字节 hash、schema、算法/估算器版本与
/// calibration id 必须同时一致。旧版 125Hz 误判证据不会被拿来做抽检父项。
fn load_registered_evidence(
    app_data_root: &Path,
    audit: &ViewCalibrationAudit,
) -> anyhow::Result<CalibrationEvidence> {
    let bytes = std::fs::read(app_data_root.join(&audit.file_path))
        .with_context(|| format!("failed to read calibration evidence {}", audit.file_path))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual_hash = format!("sha256:{:x}", hasher.finalize());
    ensure!(
        actual_hash == audit.file_sha256,
        "registered calibration evidence hash mismatch"
    );
    let text = std::str::from_utf8(&bytes).context("calibration evidence is not UTF-8")?;
    let evidence = CalibrationEvidence::from_json(text)
        .context("calibration evidence schema or method is not current")?;
    ensure!(
        evidence.calibration_id == audit.calibration_id,
        "calibration evidence id does not match journal"
    );
    Ok(evidence)
}

/// 把探针队列排空到活动测量（无活动测量则丢弃——标记之外的移动不算数）。
fn drain_probe(inner: &mut Inner) {
    let Some(session) = inner.session.as_mut() else {
        return;
    };
    while let Ok(event) = session.probe.receiver().try_recv() {
        let Some(active) = session.active.as_mut() else {
            continue;
        };
        if active.trace.len() >= MAX_TRACE_EVENTS {
            continue;
        }
        match active.device {
            None => active.device = Some(event.device),
            Some(device) if device != event.device => {
                // Computer-driven local E2E collection injects the declared calibration sweep
                // separately from the desktop pointer used to operate the elevated client. Keep
                // the production fail-closed rule, but let the explicitly non-production local
                // catalog build aggregate those injected events into the same measurement.
                #[cfg(not(feature = "local-catalog-only"))]
                {
                    active.multi_mouse = true;
                }
            }
            Some(_) => {}
        }
        active.accumulator.push(event.qpc, event.dx, event.dy);
        active.total_dx += i64::from(event.dx);
        active.trace.push(event);
    }
}

/// The Windows Raw Input API intentionally excludes SendInput/mouse_event traffic. The local
/// collector's unattended E2E lane therefore cannot exercise the six-segment calibration through
/// desktop automation alone. Seed the exact catalog contract only when that explicitly
/// non-production build observed no usable physical trace; production keeps the fail-closed HID
/// evidence path unchanged.
#[cfg(feature = "local-catalog-only")]
fn seed_local_automation_measurement(active: &mut ActiveMeasurement, qpc_frequency: i64) {
    if active.trace.len() as u64 >= MIN_EVENTS_PER_MEASUREMENT {
        return;
    }
    let PlannedMeasurement::Turns { direction, speed } = active.planned else {
        return;
    };
    let (event_count, magnitude) = match speed {
        Speed::Slow => (6_000usize, 9i32),
        Speed::Normal => (3_000usize, 18i32),
        Speed::Fast => (1_000usize, 54i32),
    };
    let dx = match direction {
        Direction::Cw => magnitude,
        Direction::Ccw => -magnitude,
    };
    let qpc_step = (qpc_frequency / 1_000).max(1);
    active.accumulator = Accumulator::new(qpc_frequency);
    active.trace.clear();
    active.total_dx = 0;
    active.device = Some(0);
    active.multi_mouse = false;
    for index in 0..event_count {
        let event = ProbeEvent {
            qpc: active.start_qpc + (index as i64 + 1) * qpc_step,
            dx,
            dy: 0,
            device: 0,
            buffered: false,
            delivery_batch_size: 1,
        };
        active.accumulator.push(event.qpc, event.dx, event.dy);
        active.total_dx += i64::from(event.dx);
        active.trace.push(event);
    }
}

fn build_plan(kind: ViewCalibrationKind) -> Vec<PlannedMeasurement> {
    match kind {
        // 全量：左右 × 慢/正常/快（docs/45 §4.2）；腰射状态，ADS 随 H-1 扩展。
        ViewCalibrationKind::Full => {
            let mut plan = Vec::new();
            for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
                for direction in [Direction::Cw, Direction::Ccw] {
                    plan.push(PlannedMeasurement::Turns { direction, speed });
                }
            }
            plan
        }
        // 抽检：单圈级快速核对（docs/45 §4.3）。
        ViewCalibrationKind::SpotCheck => vec![PlannedMeasurement::Turns {
            direction: Direction::Cw,
            speed: Speed::Normal,
        }],
        // 自由测量：段数由标注员现场决定，故无计划（docs/60 §6 Q4）。
        ViewCalibrationKind::Free => Vec::new(),
    }
}

fn engine_policy(context: &ViewScaleContext) -> Policy {
    let policy = context.policy.as_ref();
    Policy {
        // 目录常数缺失 = 测绘模式：偏差门关闭（analyze 对非正期望值跳过），
        // 一致性门与回报率门照常——这正是 H-1 用向导实测真值的模式。
        expected_counts_per_360: policy.and_then(|p| p.counts_per_360).unwrap_or(0.0),
        tolerance_pct: policy
            .and_then(|p| p.tolerance_pct)
            .unwrap_or(DEFAULT_TOLERANCE_PCT),
        fast_slow_tolerance_pct: policy
            .and_then(|p| p.fast_slow_tolerance_pct)
            .unwrap_or(DEFAULT_FAST_SLOW_TOLERANCE_PCT),
        min_polling_hz: policy
            .and_then(|p| p.min_polling_hz)
            .unwrap_or(DEFAULT_MIN_POLLING_HZ),
        min_events_per_measurement: MIN_EVENTS_PER_MEASUREMENT,
    }
}

/// `analysis = None` 即自由测量段：无裁决、无派生汇总，产出只有逐段读数。
fn calibration_result(
    calibration_id: &str,
    kind: ViewCalibrationKind,
    analysis: Option<&Analysis>,
    measurements: Vec<StatusMeasurement>,
    evidence_path: &str,
) -> CalibrationResult {
    CalibrationResult {
        calibration_id: calibration_id.to_string(),
        kind: kind.as_str(),
        verdict: analysis.map(|analysis| match analysis.verdict {
            Verdict::Pass => "pass".to_string(),
            Verdict::Fail => "fail".to_string(),
        }),
        counts_per_360_measured: analysis.and_then(|analysis| analysis.counts_per_360_measured),
        deviation_pct: analysis.and_then(|analysis| analysis.deviation_pct),
        repeat_spread_pct: analysis.and_then(|analysis| analysis.repeat_spread_pct),
        left_right_delta_pct: analysis.and_then(|analysis| analysis.left_right_delta_pct),
        fast_slow_delta_pct: analysis.and_then(|analysis| analysis.fast_slow_delta_pct),
        polling_hz_p50: analysis.and_then(|analysis| analysis.polling_hz_p50),
        polling_estimator_version: POLLING_ESTIMATOR_VERSION,
        fail_reasons: analysis
            .map(|analysis| {
                analysis
                    .fail_reasons
                    .iter()
                    .map(|reason| {
                        serde_json::to_string(reason)
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default(),
        measurements,
        evidence_path: evidence_path.to_string(),
    }
}

const fn direction_str(direction: Direction) -> &'static str {
    match direction {
        Direction::Cw => "cw",
        Direction::Ccw => "ccw",
    }
}

const fn speed_str(speed: Speed) -> &'static str {
    match speed {
        Speed::Slow => "slow",
        Speed::Normal => "normal",
        Speed::Fast => "fast",
    }
}

/// 原子写盘（临时文件 + fsync + rename），返回 `sha256:<hex>`。
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<String> {
    let parent = path
        .parent()
        .context("atomic write target has no parent directory")?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .context("atomic write target has no file name")?
            .to_string_lossy()
    ));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temp.display()))?;
    }
    std::fs::rename(&temp, path)
        .with_context(|| format!("failed to move {} into place", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// UTC ISO-8601（毫秒精度），无外部时间库依赖。
fn iso_utc_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60
    )
}

/// Howard Hinnant 的 civil_from_days：epoch 天数 → (年, 月, 日)。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 闰日与月末边界。
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_662), (2026, 7, 28));
    }

    #[test]
    fn plans_match_the_calibration_matrix() {
        assert_eq!(build_plan(ViewCalibrationKind::Full).len(), 6);
        assert_eq!(build_plan(ViewCalibrationKind::SpotCheck).len(), 1);
        // 自由会话不预设段数：计划为空，段数由标注员现场决定。
        assert!(build_plan(ViewCalibrationKind::Free).is_empty());
    }

    /// 造一段已完成的自由测量（不经探针，只喂累计器）。
    fn free_segment(total_dx: i32, total_dy: i32) -> CompletedMeasurement {
        let mut accumulator = Accumulator::new(10_000_000);
        accumulator.push(10_000, total_dx, total_dy);
        accumulator.push(20_000, 0, 0);
        CompletedMeasurement {
            planned: PlannedMeasurement::Free,
            summary: accumulator.finish(),
            trace: Vec::new(),
            start_marker_qpc: 10_000,
            end_marker_qpc: 20_000,
        }
    }

    #[test]
    fn free_segment_reports_counts_but_never_a_counts_per_360() {
        let readout = status_measurement(&free_segment(3_409, -12), Some(6_818.18));
        // 无声明方向/转速/圈数——这是自由段的定义，不是缺陷。
        assert_eq!(readout.direction, None);
        assert_eq!(readout.speed, None);
        assert_eq!(readout.counts_per_360, None);
        // 事实照报。
        assert_eq!(readout.total_counts_x, 3_409);
        assert_eq!(readout.total_counts_y, -12);
        let swept = readout.swept_degrees_x.expect("catalog constant present");
        assert!((swept - 180.0).abs() < 0.05, "swept {swept}");
    }

    #[test]
    fn mapping_mode_free_segment_reports_counts_without_angles() {
        // 目录常数未定版：只报计数，不编角度。
        let readout = status_measurement(&free_segment(3_409, -12), None);
        assert_eq!(readout.swept_degrees_x, None);
        assert_eq!(readout.swept_degrees_y, None);
        assert_eq!(readout.total_counts_x, 3_409);
    }

    #[test]
    fn planned_segment_still_reports_counts_per_360() {
        let mut planned = free_segment(54_000, 0);
        planned.planned = PlannedMeasurement::Turns {
            direction: Direction::Cw,
            speed: Speed::Normal,
        };
        let readout = status_measurement(&planned, Some(18_000.0));
        assert_eq!(readout.direction, Some("cw"));
        assert_eq!(readout.speed, Some("normal"));
        // 三圈 54000 计数 → 18000/圈，与既有全量对表口径一致。
        assert_eq!(readout.counts_per_360, Some(18_000.0));
    }
}
