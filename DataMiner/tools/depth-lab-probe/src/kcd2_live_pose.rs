//! Bounded Windows-side wiring for KCD2's exact-build live pose extractor.
//!
//! Root-CBV acquisition remains owned by `d3d12_cbv_readback`. This module accepts only the two
//! sealed binding identities, joins them at each Present boundary, and writes an independent audit
//! stream. A missing side is an explicit discard; no previous value is retained. Warmup evidence is
//! never written as formal output, and each run owns a fresh writer/extractor generation.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, RwLock, RwLockReadGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use depth_lab::{
    KCD2_LIVE_POSE_EVENT_SCHEMA_V3, Kcd2LivePoseDiscard, Kcd2LivePoseExtractor,
    Kcd2LivePoseOutcome, Kcd2LivePosePair, Kcd2LivePosePlan, Kcd2LivePoseSourceIdentity,
    Kcd2LivePoseSourceKind,
};
use serde::Serialize;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::camera_writer::CameraPoseIndexWriter;
use crate::player_writer::{PlayerPoseIndexWriter, PlayerRootSample};

pub const EVENT_FILE: &str = "kcd2-d3d12-live-pose.jsonl";
pub const WRITER_READY_FILE: &str = "kcd2-d3d12-live-pose-writer-ready.json";
pub const WRITER_READY_SCHEMA_V1: &str = "kcd2-d3d12-live-pose-writer-ready/v1";

const EVENT_QUEUE_CAPACITY: usize = 256;

static PLAN: RwLock<Option<Kcd2LivePosePlan>> = RwLock::new(None);
static EXTRACTOR: Mutex<Option<Kcd2LivePoseExtractor>> = Mutex::new(None);
static EVENT_TX: Mutex<Option<SyncSender<LivePoseEvent>>> = Mutex::new(None);
static WRITER_JOIN: Mutex<Option<JoinHandle<Result<(), String>>>> = Mutex::new(None);
// Root draw callbacks may still be finishing their bounded double-read while the next Present
// begins on another thread. The read side covers the complete source observation; the Present
// boundary takes the write side before closing an interval. This prevents a correctly stamped
// source from arriving after its interval was already sealed.
static SOURCE_INTERVAL_GATE: RwLock<()> = RwLock::new(());
static FAULTED: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicBool = AtomicBool::new(false);
static FORMAL_ARMED: AtomicBool = AtomicBool::new(false);
static PAIRS: AtomicU64 = AtomicU64::new(0);
static DISCARDS: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);
static QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static LAST_PAIR_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_OBSERVED_PAIR_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static PROJECTION_CONTEXT: Mutex<Option<ProjectionContext>> = Mutex::new(None);
static WARMUP_DIAGNOSTICS: Mutex<WarmupDiagnostics> = Mutex::new(WarmupDiagnostics {
    discards: 0,
    last_discard_present: None,
    missing: Vec::new(),
});

/// A bounded diagnostic snapshot, separate from formal output counters and readiness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WarmupDiagnostics {
    pub discards: u64,
    pub last_discard_present: Option<u64>,
    pub missing: Vec<&'static str>,
}

pub fn warmup_diagnostics() -> Option<WarmupDiagnostics> {
    WARMUP_DIAGNOSTICS.lock().ok().map(|state| state.clone())
}

fn record_warmup_discard(discard: Kcd2LivePoseDiscard) {
    if let Ok(mut state) = WARMUP_DIAGNOSTICS.lock() {
        state.discards = state.discards.saturating_add(1);
        state.last_discard_present = Some(discard.present_sequence);
        // The extractor has two source kinds. Retain only the latest missing set, not history.
        state.missing = discard.missing.into_iter().take(2).collect();
    }
}

fn reset_warmup_diagnostics() -> Result<(), String> {
    *WARMUP_DIAGNOSTICS
        .lock()
        .map_err(|_| "KCD2 warmup diagnostics lock poisoned")? = WarmupDiagnostics::default();
    Ok(())
}

struct ProjectionContext {
    generation: u64,
    performance_validation_only: bool,
    width: u32,
    height: u32,
    viewports: BTreeMap<u64, crate::d3d12_render_camera::CapturedViewport>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub pairs: u64,
    pub discards: u64,
    pub faults: u64,
    pub queue_drops: u64,
    pub last_pair_present: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone)]
struct Kcd2WriterContext {
    identity: depth_lab::StreamIdentity,
    binding: depth_lab::Kcd2CombinedCameraBinding,
    dsv_id: String,
    qpc_frequency_hz: u64,
    performance_validation_only: bool,
}

pub fn initialize_with_binding(
    output_dir: &Path,
    identity: depth_lab::StreamIdentity,
    actual_executable_sha256: &str,
    binding: depth_lab::Kcd2CombinedCameraBinding,
    dsv_id: &str,
    performance_validation_only: bool,
) -> Result<bool, String> {
    depth_lab::CameraBindingDocument::Kcd2Combined(binding.clone())
        .validate_for_executable(actual_executable_sha256)?;
    if identity.session_id.trim().is_empty()
        || identity.run_id.trim().is_empty()
        || identity.generation == 0
        || dsv_id.trim().is_empty()
    {
        return Err("KCD2 live pose stream identity/DSV is incomplete".into());
    }
    let plan = binding.live_pose_plan.clone();
    {
        let held = PLAN
            .read()
            .map_err(|_| "KCD2 live pose plan lock poisoned".to_string())?;
        if !STOPPED.load(Ordering::Acquire)
            && let Some(held) = held.as_ref()
        {
            return if held == &plan
                && !FAULTED.load(Ordering::Acquire)
                && PROJECTION_CONTEXT.lock().is_ok_and(|context| {
                    context.as_ref().is_some_and(|context| {
                        context.generation == identity.generation
                            && context.performance_validation_only == performance_validation_only
                    })
                }) {
                Ok(true)
            } else {
                Err("KCD2 live pose cannot be replaced while the current run is active".into())
            };
        }
    }
    plan.validate(actual_executable_sha256)?;
    reset_warmup_diagnostics()?;
    FAULTED.store(false, Ordering::Release);
    STOPPED.store(false, Ordering::Release);
    FORMAL_ARMED.store(false, Ordering::Release);
    PAIRS.store(0, Ordering::Release);
    DISCARDS.store(0, Ordering::Release);
    FAULTS.store(0, Ordering::Release);
    QUEUE_DROPS.store(0, Ordering::Release);
    LAST_PAIR_PRESENT.store(u64::MAX, Ordering::Release);
    LAST_OBSERVED_PAIR_PRESENT.store(u64::MAX, Ordering::Release);
    if let Ok(mut error) = LAST_ERROR.lock() {
        *error = None;
    }
    let extractor = Kcd2LivePoseExtractor::new(plan.clone(), actual_executable_sha256)?;
    crate::camera_probe::begin_external_projection_observations(
        identity.generation,
        binding.render_width,
        binding.render_height,
    )?;
    *PROJECTION_CONTEXT
        .lock()
        .map_err(|_| "KCD2 projection context poisoned")? = Some(ProjectionContext {
        generation: identity.generation,
        performance_validation_only,
        width: binding.render_width,
        height: binding.render_height,
        viewports: BTreeMap::new(),
        last_error: None,
    });
    start_writer(
        output_dir,
        Kcd2WriterContext {
            identity: identity.clone(),
            binding,
            dsv_id: dsv_id.to_owned(),
            qpc_frequency_hz: current_qpc_frequency()?,
            performance_validation_only,
        },
    )?;
    *EXTRACTOR
        .lock()
        .map_err(|_| "KCD2 live pose extractor lock poisoned".to_string())? = Some(extractor);
    *PLAN
        .write()
        .map_err(|_| "KCD2 live pose plan lock poisoned".to_string())? = Some(plan);
    if let Err(error) = publish_writer_ready(output_dir, &identity.session_id) {
        stop();
        return Err(error);
    }
    Ok(true)
}

#[must_use]
pub fn enabled() -> bool {
    PLAN.read().is_ok_and(|plan| plan.is_some())
}

#[must_use]
pub fn requires_pre_execute_snapshot() -> bool {
    PLAN.read().is_ok_and(|plan| {
        plan.as_ref()
            .is_some_and(|plan| plan.schema == depth_lab::KCD2_LIVE_POSE_PLAN_SCHEMA_V3)
    })
}

#[must_use]
pub fn accepts_draw(
    root_signature_sha256: Option<&str>,
    pass_ordinal: u32,
    draw_ordinal: u32,
) -> bool {
    let Some(actual) = root_signature_sha256 else {
        return false;
    };
    let Ok(plan) = PLAN.read() else {
        return false;
    };
    let Some(plan) = plan.as_ref() else {
        return false;
    };
    plan_accepts_draw(plan, actual, pass_ordinal, draw_ordinal)
}

pub fn root_parameter_for_draw(actual: &str, pass_ordinal: u32, draw_ordinal: u32) -> Option<u32> {
    let plan = PLAN.read().ok()?;
    plan_root_parameter_for_draw(plan.as_ref()?, actual, pass_ordinal, draw_ordinal)
}

fn plan_accepts_draw(
    plan: &Kcd2LivePosePlan,
    actual: &str,
    pass_ordinal: u32,
    draw_ordinal: u32,
) -> bool {
    plan_root_parameter_for_draw(plan, actual, pass_ordinal, draw_ordinal).is_some()
}

fn plan_root_parameter_for_draw(
    plan: &Kcd2LivePosePlan,
    actual: &str,
    pass_ordinal: u32,
    draw_ordinal: u32,
) -> Option<u32> {
    plan.binding_for_draw(actual, pass_ordinal, draw_ordinal)
        .map(|(_, binding)| binding.root_parameter_index)
}

pub fn reset_for_next_run() -> Result<(), String> {
    if enabled() && !STOPPED.load(Ordering::Acquire) {
        return Err("cannot reset an active KCD2 live pose run".into());
    }
    if EVENT_TX
        .lock()
        .map_err(|_| "KCD2 live pose writer sender lock poisoned during reset".to_string())?
        .is_some()
        || WRITER_JOIN
            .lock()
            .map_err(|_| "KCD2 live pose writer join lock poisoned during reset".to_string())?
            .is_some()
    {
        return Err("cannot reset KCD2 live pose before its writer has stopped".into());
    }
    *PLAN
        .write()
        .map_err(|_| "KCD2 live pose plan lock poisoned during reset".to_string())? = None;
    *EXTRACTOR
        .lock()
        .map_err(|_| "KCD2 live pose extractor lock poisoned during reset".to_string())? = None;
    FORMAL_ARMED.store(false, Ordering::Release);
    LAST_PAIR_PRESENT.store(u64::MAX, Ordering::Release);
    LAST_OBSERVED_PAIR_PRESENT.store(u64::MAX, Ordering::Release);
    reset_warmup_diagnostics()?;
    *PROJECTION_CONTEXT
        .lock()
        .map_err(|_| "KCD2 projection context poisoned during reset")? = None;
    Ok(())
}

pub fn source_interval_guard() -> Result<Option<RwLockReadGuard<'static, ()>>, String> {
    if !enabled() || FAULTED.load(Ordering::Acquire) || STOPPED.load(Ordering::Acquire) {
        return Ok(None);
    }
    SOURCE_INTERVAL_GATE
        .read()
        .map(Some)
        .map_err(|_| "KCD2 live pose source-interval gate was poisoned".to_string())
}

pub fn record_interval_fault(message: impl Into<String>) {
    fault("kcd2_live_pose_interval_gate_poisoned", message);
}

pub fn record_submission_fault(message: impl Into<String>) {
    fault("kcd2_live_pose_submission_invalid", message);
}

pub fn record_snapshot_qpc_fault(message: impl Into<String>) {
    fault("kcd2_live_pose_snapshot_qpc_failed", message);
}

#[allow(clippy::too_many_arguments)]
pub fn observe_snapshot(
    present_sequence: u64,
    sample_qpc: i64,
    root_signature_sha256: &str,
    root_parameter_index: u32,
    pass_ordinal: u32,
    draw_ordinal: u32,
    read_offset_bytes: u32,
    bytes: &[u8],
    viewport: Option<crate::d3d12_render_camera::CapturedViewport>,
) {
    if !enabled() || FAULTED.load(Ordering::Acquire) || STOPPED.load(Ordering::Acquire) {
        return;
    }
    if sample_qpc <= 0 {
        fault(
            "kcd2_live_pose_snapshot_qpc_failed",
            "draw-time source QPC is not positive",
        );
        return;
    }
    let is_view = PLAN.read().is_ok_and(|plan| {
        plan.as_ref().is_some_and(|plan| {
            plan.classify_source(
                root_signature_sha256,
                root_parameter_index,
                pass_ordinal,
                draw_ordinal,
                read_offset_bytes,
                u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            ) == Some(Kcd2LivePoseSourceKind::ViewProjection)
        })
    });
    if is_view && let Some(viewport) = viewport {
        let recorded = PROJECTION_CONTEXT
            .lock()
            .map_err(|_| "KCD2 viewport context poisoned".to_owned())
            .and_then(|mut context| {
                let context = context.as_mut().ok_or("KCD2 viewport context missing")?;
                if let Some(previous) = context.viewports.get(&present_sequence) {
                    if previous != &viewport {
                        return Err("KCD2 viewport changed within one Present".into());
                    }
                } else {
                    if context.viewports.len() >= 8 {
                        return Err("KCD2 pending viewport cache overflow".into());
                    }
                    context.viewports.insert(present_sequence, viewport);
                }
                Ok(())
            });
        if let Err(error) = recorded {
            fault("kcd2_viewport_invalid", error);
            return;
        }
    }
    let result = EXTRACTOR
        .lock()
        .map_err(|_| "KCD2 live pose extractor lock poisoned".to_string())
        .and_then(|mut extractor| {
            extractor
                .as_mut()
                .ok_or_else(|| "KCD2 live pose extractor is unavailable".to_string())?
                .observe_snapshot(
                    present_sequence,
                    root_signature_sha256,
                    root_parameter_index,
                    pass_ordinal,
                    draw_ordinal,
                    read_offset_bytes,
                    sample_qpc,
                    bytes,
                )
                .map(|_| ())
        });
    if let Err(error) = result {
        fault("kcd2_live_pose_snapshot_invalid", error);
    }
}

pub fn finish_present(present_sequence: u64, depth_copied: bool) {
    if !enabled() || FAULTED.load(Ordering::Acquire) || STOPPED.load(Ordering::Acquire) {
        return;
    }
    let boundary = match SOURCE_INTERVAL_GATE.write() {
        Ok(boundary) => boundary,
        Err(_) => {
            fault(
                "kcd2_live_pose_interval_gate_poisoned",
                "source-interval gate was poisoned at the Present boundary",
            );
            return;
        }
    };
    let present_boundary_qpc = match current_sample_qpc() {
        Ok(value) => value,
        Err(error) => {
            drop(boundary);
            fault("kcd2_live_pose_present_qpc_failed", error);
            return;
        }
    };
    let result = EXTRACTOR
        .lock()
        .map_err(|_| "KCD2 live pose extractor lock poisoned".to_string())
        .and_then(|mut extractor| {
            extractor
                .as_mut()
                .ok_or_else(|| "KCD2 live pose extractor is unavailable".to_string())?
                .finish_present(present_sequence, present_boundary_qpc)
        });
    drop(boundary);
    let viewport = PROJECTION_CONTEXT
        .lock()
        .map_err(|_| "KCD2 projection context poisoned".to_owned())
        .and_then(|mut context| {
            let context = context.as_mut().ok_or("KCD2 projection context missing")?;
            let result = context
                .viewports
                .remove(&present_sequence)
                .ok_or_else(|| {
                    format!("KCD2 Present {present_sequence} has no exact-draw viewport")
                })
                .and_then(|viewport| {
                    viewport.validate(context.width, context.height)?;
                    Ok(viewport)
                });
            context.last_error = result.as_ref().err().cloned();
            result.map(|viewport| (context.generation, viewport))
        });
    match result {
        Ok(Some(Kcd2LivePoseOutcome::Pair(pair))) => {
            let (generation, viewport) = match viewport {
                Ok(value) => value,
                Err(error) => {
                    if FORMAL_ARMED.load(Ordering::Acquire) {
                        fault("kcd2_viewport_invalid", error);
                    }
                    return;
                }
            };
            LAST_OBSERVED_PAIR_PRESENT.store(pair.present_sequence, Ordering::Release);
            if !FORMAL_ARMED.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = publish_pair_projection(generation, &pair, viewport, depth_copied) {
                fault("kcd2_projection_publish_failed", error);
                return;
            }
            LAST_PAIR_PRESENT.store(pair.present_sequence, Ordering::Release);
            PAIRS.fetch_add(1, Ordering::Relaxed);
            send_event(LivePoseEvent::Pair { pair: *pair });
        }
        Ok(Some(Kcd2LivePoseOutcome::Discarded(discard))) => {
            if !FORMAL_ARMED.load(Ordering::Acquire) {
                record_warmup_discard(discard);
                return;
            }
            fail_missing_formal_sources(discard);
        }
        Ok(None) if FORMAL_ARMED.load(Ordering::Acquire) => {
            // No accepted draw at all used to leave no diagnostic or failure here. The next
            // selected readback then reported a missing projection, or the writer saw a pose
            // sequence gap. Preserve the missing frame at its own boundary without reusing pose.
            fail_missing_formal_sources(Kcd2LivePoseDiscard {
                present_sequence,
                missing: vec!["view_projection", "horizontal_player_root"],
            });
        }
        Ok(None) => record_warmup_discard(Kcd2LivePoseDiscard {
            present_sequence,
            missing: vec!["view_projection", "horizontal_player_root"],
        }),
        Err(error) => fault("kcd2_live_pose_present_invalid", error),
    }
}

fn fail_missing_formal_sources(discard: Kcd2LivePoseDiscard) {
    let message = format!(
        "KCD2 Present {} is missing same-Present sources: {}",
        discard.present_sequence,
        discard.missing.join(", ")
    );
    DISCARDS.fetch_add(1, Ordering::Relaxed);
    // A full diagnostics queue must not latch a generic fault before the exact source error.
    // send_event_raw still accounts the lost diagnostic; fault records the primary cause below.
    let _ = send_event_raw(LivePoseEvent::Discarded { discard });
    fault("kcd2_live_pose_sources_missing", message);
}

fn publish_pair_projection(
    generation: u64,
    pair: &Kcd2LivePosePair,
    viewport: crate::d3d12_render_camera::CapturedViewport,
    depth_copied: bool,
) -> Result<(), String> {
    if !FORMAL_ARMED.load(Ordering::Acquire) {
        return Err("KCD2 warmup projection cannot be published as production".into());
    }
    if !depth_copied {
        return Ok(());
    }
    // A/B are measured in engine units; the validated profile applies metric scale later.
    crate::camera_probe::record_external_projection_observation(
        generation,
        pair.present_sequence,
        depth_lab::Projection {
            a: pair.projection_a,
            b: pair.projection_b,
            metric_scale: 1.0,
        },
    )?;
    crate::probe::record_external_draw_viewport(
        generation,
        pair.present_sequence,
        viewport.count,
        f64::from(viewport.first.MinDepth),
        f64::from(viewport.first.MaxDepth),
    )
}

pub fn projection_warmup_error() -> Option<String> {
    PROJECTION_CONTEXT
        .lock()
        .ok()
        .and_then(|context| context.as_ref()?.last_error.clone())
}

pub fn current_sample_qpc() -> Result<i64, String> {
    let mut qpc = 0i64;
    unsafe { QueryPerformanceCounter(&mut qpc) }
        .map_err(|error| format!("read KCD2 live pose QPC: {error}"))?;
    (qpc > 0)
        .then_some(qpc)
        .ok_or_else(|| "KCD2 live pose QPC is not positive".into())
}

fn current_qpc_frequency() -> Result<u64, String> {
    let mut frequency = 0i64;
    unsafe { QueryPerformanceFrequency(&mut frequency) }
        .map_err(|error| format!("read KCD2 live pose QPC frequency: {error}"))?;
    u64::try_from(frequency)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "KCD2 live pose QPC frequency is not positive".into())
}

pub fn stop() {
    if !enabled() || STOPPED.swap(true, Ordering::AcqRel) {
        return;
    }
    let boundary = match SOURCE_INTERVAL_GATE.write() {
        Ok(boundary) => boundary,
        Err(_) => {
            fault(
                "kcd2_live_pose_interval_gate_poisoned",
                "source-interval gate was poisoned during stop",
            );
            return;
        }
    };
    let pending = EXTRACTOR
        .lock()
        .map(|mut extractor| {
            extractor
                .as_mut()
                .map(Kcd2LivePoseExtractor::finish_pending)
                .unwrap_or_default()
        })
        .unwrap_or_else(|_| {
            fault(
                "kcd2_live_pose_extractor_poisoned",
                "extractor lock was poisoned during stop",
            );
            Vec::new()
        });
    drop(boundary);
    if FORMAL_ARMED.load(Ordering::Acquire) {
        for outcome in pending {
            match outcome {
                Kcd2LivePoseOutcome::Pair(pair) => {
                    LAST_PAIR_PRESENT.store(pair.present_sequence, Ordering::Release);
                    PAIRS.fetch_add(1, Ordering::Relaxed);
                    send_event(LivePoseEvent::Pair { pair: *pair });
                }
                Kcd2LivePoseOutcome::Discarded(discard) => {
                    DISCARDS.fetch_add(1, Ordering::Relaxed);
                    send_event(LivePoseEvent::Discarded { discard });
                }
            }
        }
    }
    if let Ok(sender) = EVENT_TX.lock()
        && let Some(tx) = sender.as_ref()
    {
        let _ = tx.send(LivePoseEvent::Stopped {
            pairs: PAIRS.load(Ordering::Acquire),
            discards: DISCARDS.load(Ordering::Acquire),
            faults: FAULTS.load(Ordering::Acquire),
            queue_drops: QUEUE_DROPS.load(Ordering::Acquire),
        });
    }
    if let Ok(mut sender) = EVENT_TX.lock() {
        *sender = None;
    }
    if let Ok(mut join) = WRITER_JOIN.lock()
        && let Some(join) = join.take()
    {
        match join.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => latch_writer_failure(error),
            Err(_) => latch_writer_failure("KCD2 live pose writer panicked".into()),
        }
    }
    if let Ok(mut extractor) = EXTRACTOR.lock() {
        *extractor = None;
    }
}

fn latch_writer_failure(message: String) {
    if let Ok(mut error) = LAST_ERROR.lock()
        && error.is_none()
    {
        *error = Some(format!("kcd2_live_pose_writer_failed: {message}"));
    }
    if !FAULTED.swap(true, Ordering::AcqRel) {
        FAULTS.fetch_add(1, Ordering::Relaxed);
    }
}

fn fault(code: &'static str, message: impl Into<String>) {
    let message = message.into();
    FAULTS.fetch_add(1, Ordering::Relaxed);
    if FAULTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Ok(mut error) = LAST_ERROR.lock()
        && error.is_none()
    {
        *error = Some(format!("{code}: {message}"));
    }
    send_event_raw(LivePoseEvent::Fault { code, message });
}

#[must_use]
pub fn pair_ready_for_present(present_sequence: u64) -> bool {
    enabled()
        && !FAULTED.load(Ordering::Acquire)
        && LAST_OBSERVED_PAIR_PRESENT.load(Ordering::Acquire) == present_sequence
}

pub fn begin_production_warmup() -> Result<(), String> {
    if !enabled() {
        return Err("KCD2 live pose is not initialized".into());
    }
    FORMAL_ARMED.store(false, Ordering::Release);
    Ok(())
}

pub fn arm_production() -> Result<(), String> {
    if let Some(context) = PROJECTION_CONTEXT
        .lock()
        .map_err(|_| "KCD2 projection context poisoned during arm")?
        .as_mut()
    {
        context.viewports.clear();
        context.last_error = None;
    }
    let plan = PLAN
        .read()
        .map_err(|_| "KCD2 live pose plan lock poisoned during formal arm".to_string())?
        .as_ref()
        .cloned()
        .ok_or("KCD2 live pose plan is unavailable during formal arm")?;
    *EXTRACTOR
        .lock()
        .map_err(|_| "KCD2 live pose extractor lock poisoned during formal arm".to_string())? =
        Some(Kcd2LivePoseExtractor::new(
            plan.clone(),
            &plan.executable_sha256,
        )?);
    PAIRS.store(0, Ordering::Release);
    DISCARDS.store(0, Ordering::Release);
    LAST_PAIR_PRESENT.store(u64::MAX, Ordering::Release);
    LAST_OBSERVED_PAIR_PRESENT.store(u64::MAX, Ordering::Release);
    FORMAL_ARMED.store(true, Ordering::Release);
    Ok(())
}

#[must_use]
pub fn failure() -> Option<String> {
    if !FAULTED.load(Ordering::Acquire) {
        return None;
    }
    LAST_ERROR
        .lock()
        .ok()
        .and_then(|error| error.clone())
        .or_else(|| Some("KCD2 live pose faulted".into()))
}

#[must_use]
pub fn summary() -> Summary {
    let last_pair_present = LAST_PAIR_PRESENT.load(Ordering::Acquire);
    Summary {
        pairs: PAIRS.load(Ordering::Acquire),
        discards: DISCARDS.load(Ordering::Acquire),
        faults: FAULTS.load(Ordering::Acquire),
        queue_drops: QUEUE_DROPS.load(Ordering::Acquire),
        last_pair_present: (last_pair_present != u64::MAX).then_some(last_pair_present),
        error: LAST_ERROR.lock().ok().and_then(|error| error.clone()),
    }
}

fn send_event(event: LivePoseEvent) {
    if !send_event_raw(event) {
        FAULTS.fetch_add(1, Ordering::Relaxed);
        FAULTED.store(true, Ordering::Release);
    }
}

fn send_event_raw(event: LivePoseEvent) -> bool {
    let tx = match EVENT_TX.lock() {
        Ok(sender) => sender.as_ref().cloned(),
        Err(_) => None,
    };
    let Some(tx) = tx else {
        return false;
    };
    match tx.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
            QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

fn start_writer(output_dir: &Path, context: Kcd2WriterContext) -> Result<(), String> {
    if EVENT_TX
        .lock()
        .map_err(|_| "KCD2 live pose writer sender lock poisoned".to_string())?
        .is_some()
        || WRITER_JOIN
            .lock()
            .map_err(|_| "KCD2 live pose writer join lock poisoned".to_string())?
            .is_some()
    {
        return Err("KCD2 live pose writer is still active".into());
    }
    let path = output_dir.join(EVENT_FILE);
    let output_dir = output_dir.to_owned();
    let (tx, rx) = sync_channel(EVENT_QUEUE_CAPACITY);
    let (ready_tx, ready_rx) = sync_channel(1);
    let session_id = context.identity.session_id.clone();
    let plan = context.binding.live_pose_plan.clone();
    let join = std::thread::Builder::new()
        .name("dm-kcd2-live-pose-writer".into())
        .spawn(move || {
            let result = (|| {
                let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                    .map_err(|error| format!("create {}: {error}", path.display()))?;
                let mut writer = BufWriter::new(file);
                let mut camera_writer = if context.performance_validation_only {
                    CameraPoseIndexWriter::new_performance_validation(&output_dir)?
                } else {
                    CameraPoseIndexWriter::new(&output_dir)?
                };
                let mut player_writer = Some(PlayerPoseIndexWriter::new(
                    &output_dir,
                    context.identity.clone(),
                    context.binding.executable_sha256.clone(),
                    context.qpc_frequency_hz,
                    if context.binding.live_pose_plan.schema
                        == depth_lab::KCD2_LIVE_POSE_PLAN_SCHEMA_V3
                    {
                        "kcd2-horizontal-player-root-exact-build/v2"
                    } else {
                        "kcd2-horizontal-player-root-exact-build/v1"
                    }
                    .into(),
                )?);
                let mut pair_count = 0u64;
                write_envelope(
                    &mut writer,
                    &session_id,
                    &LivePoseEvent::Header {
                        process_id: std::process::id(),
                        plan,
                        limitation: "exact-build same-Present KCD2 camera/root pose derived from the combined CameraBinding; raw positions remain in engine units and metric conversion is applied only by the separately pinned DepthProfile calibration",
                    },
                )?;
                let _ = ready_tx.send(Ok(()));
                loop {
                    let event = rx
                        .recv()
                        .map_err(|_| "KCD2 live pose writer channel disconnected before stop".to_string())?;
                    match &event {
                        LivePoseEvent::Pair { pair } => {
                            (|| -> Result<(), String> {
                                let (camera, player) = materialize_standard_pose(&context, pair)?;
                                camera_writer.write_row(&camera)?;
                                player_writer.as_mut().ok_or("KCD2 player writer was already finalized")?.push(player)?;
                                pair_count = pair_count.checked_add(1).ok_or("KCD2 standard pose count overflow")?;
                                Ok(())
                            })().inspect_err(|error| {
                                // Publish the first cause before diagnostic I/O or buffer teardown.
                                latch_writer_failure(error.clone());
                                preserve_rejected_pair(&output_dir, &context, pair, error);
                            })?;
                        }
                        LivePoseEvent::Discarded { discard } => {
                            camera_writer.write_diagnostic(&depth_lab::CameraPoseDiagnosticRow {
                                schema: depth_lab::CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1.into(),
                                identity: context.identity.clone(),
                                present_seq: discard.present_sequence,
                                reason: if discard
                                    .missing
                                    .iter()
                                    .any(|reason| reason.contains("ambiguous"))
                                {
                                    depth_lab::CameraPoseDiagnosticReason::BindingAmbiguity
                                } else {
                                    depth_lab::CameraPoseDiagnosticReason::BindingSampleMissing
                                },
                            })?;
                        }
                        LivePoseEvent::Stopped { .. } => {
                            camera_writer.finish()?;
                            let player = player_writer
                                .take()
                                .ok_or("KCD2 player writer was already finalized")?;
                            if pair_count == 0 {
                                drop(player);
                            } else {
                                let player_count = player.finish()?;
                                if player_count != pair_count {
                                    return Err(format!(
                                        "KCD2 camera/player row count mismatch: {pair_count} != {player_count}"
                                    ));
                                }
                            }
                            write_envelope(&mut writer, &session_id, &event)?;
                            break;
                        }
                        LivePoseEvent::Header { .. } | LivePoseEvent::Fault { .. } => {}
                    }
                    write_envelope(&mut writer, &session_id, &event)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                latch_writer_failure(error.clone());
                let _ = ready_tx.send(Err(error.clone()));
                return Err(error);
            }
            Ok(())
        })
        .map_err(|error| format!("spawn KCD2 live pose writer: {error}"))?;
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "KCD2 live pose writer acknowledgement timed out".to_string())??;
    let mut sender = EVENT_TX
        .lock()
        .map_err(|_| "KCD2 live pose writer sender lock poisoned".to_string())?;
    if sender.is_some() {
        return Err("KCD2 live pose writer already initialized".into());
    }
    *sender = Some(tx);
    drop(sender);
    *WRITER_JOIN
        .lock()
        .map_err(|_| "KCD2 live pose writer join lock poisoned".to_string())? = Some(join);
    Ok(())
}

fn preserve_rejected_pair(
    output_dir: &Path,
    context: &Kcd2WriterContext,
    pair: &Kcd2LivePosePair,
    error: &str,
) {
    // Diagnostic only: never append a rejected row to the camera/player indices.
    // Keep the first rejected sample even when later cleanup also fails.
    let path = output_dir.join("kcd2-pose-rejection.json");
    let record = serde_json::json!({
        "schema": "dm-kcd2-rejected-pose/v1",
        "identity": context.identity,
        "performance_validation_only": context.performance_validation_only,
        "error": error,
        "binding": context.binding,
        "dsv_id": context.dsv_id,
        "pair": pair,
    });
    let result = (|| -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        eprintln!("KCD2 rejected-pose diagnostic could not be saved: {e}; original error: {error}");
    }
}

fn materialize_standard_pose(
    context: &Kcd2WriterContext,
    pair: &Kcd2LivePosePair,
) -> Result<(depth_lab::CameraPoseRow, PlayerRootSample), String> {
    let scale = context
        .binding
        .view_metres_per_unit
        .parse::<f64>()
        .map_err(|_| "KCD2 CameraBinding metric scale is not numeric".to_string())?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err("KCD2 CameraBinding metric scale is not positive and finite".into());
    }
    let axes = axis_matrix(context.binding.axis_mapping);
    let camera_position = context
        .binding
        .axis_mapping
        .source_to_delivery(pair.camera_position_engine_units)
        .map(|value| value * scale);
    let player_position = context
        .binding
        .axis_mapping
        .source_to_delivery(pair.horizontal_player_root_position_engine_units)
        .map(|value| value * scale);
    let camera_rotation = transform_rotation(
        quaternion_to_matrix(pair.camera_rotation_quaternion_xyzw),
        axes,
    );
    let root_rotation = pair.horizontal_player_root_rotation_3x3_row_major;
    let player_rotation = transform_rotation(
        [
            [root_rotation[0], root_rotation[1], root_rotation[2]],
            [root_rotation[3], root_rotation[4], root_rotation[5]],
            [root_rotation[6], root_rotation[7], root_rotation[8]],
        ],
        axes,
    );
    let plan = &context.binding.live_pose_plan;
    let v3 = plan.schema == depth_lab::KCD2_LIVE_POSE_PLAN_SCHEMA_V3;
    let player_source = match &pair.horizontal_player_root_source {
        Some(source) => {
            if plan.classify_source(
                &source.root_signature_sha256,
                source.root_parameter_index,
                source.pass_ordinal,
                source.draw_ordinal,
                0,
                256,
            ) != Some(Kcd2LivePoseSourceKind::HorizontalPlayerRoot)
                || pair
                    .horizontal_player_root_pass_ordinal
                    .is_some_and(|pass| pass != source.pass_ordinal)
                || pair
                    .horizontal_player_root_draw_ordinal
                    .is_some_and(|draw| draw != source.draw_ordinal)
                || (v3
                    && (pair.horizontal_player_root_pass_ordinal != Some(source.pass_ordinal)
                        || pair.horizontal_player_root_draw_ordinal != Some(source.draw_ordinal)))
            {
                return Err(
                    "KCD2 observed player root source disagrees with its binding/pair".into(),
                );
            }
            // Legacy output remains unchanged even when a legacy pair explicitly
            // supplies a checked source. V3 carries the actual source into the ZIP.
            v3.then(|| source.clone())
        }
        None if v3 => {
            return Err("KCD2 v3 player pose is missing its observed source identity".into());
        }
        None => None,
    };
    let source = match &pair.view_projection_source {
        Some(source)
            if plan.classify_source(
                &source.root_signature_sha256,
                source.root_parameter_index,
                source.pass_ordinal,
                source.draw_ordinal,
                0,
                256,
            ) == Some(Kcd2LivePoseSourceKind::ViewProjection) =>
        {
            source.clone()
        }
        Some(_) => return Err("KCD2 observed camera source disagrees with its binding".into()),
        None if plan.schema != depth_lab::KCD2_LIVE_POSE_PLAN_SCHEMA_V3 => {
            let configured = &plan.view_projection_binding;
            Kcd2LivePoseSourceIdentity {
                root_signature_sha256: configured.root_signature_sha256.clone(),
                root_parameter_index: configured.root_parameter_index,
                pass_ordinal: configured.pass_ordinal,
                draw_ordinal: configured.draw_ordinal,
            }
        }
        None => return Err("KCD2 v3 camera pose is missing its observed source identity".into()),
    };
    let root_signature_sha256 = if source.root_signature_sha256.starts_with("sha256:") {
        source.root_signature_sha256.clone()
    } else {
        format!("sha256:{}", source.root_signature_sha256)
    };
    let camera = depth_lab::CameraPoseRow {
        schema: if context.performance_validation_only {
            depth_lab::PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1
        } else {
            depth_lab::CAMERA_POSE_INDEX_SCHEMA_V1
        }
        .into(),
        identity: context.identity.clone(),
        present_seq: pair.present_sequence,
        present_qpc: pair.present_boundary_qpc,
        source: depth_lab::CameraPoseSource::ResolvedCombinedViewProjection,
        provenance: depth_lab::CameraPoseProvenance::ResolvedCombinedViewProjection {
            binding_id: context.binding.binding_id.clone(),
            binding_version: context.binding.binding_version.clone(),
            profile_id: context.binding.profile_id.clone(),
            profile_version: context.binding.profile_version.clone(),
            executable_sha256: context.binding.executable_sha256.clone(),
            dsv_id: context.dsv_id.clone(),
            render_width: context.binding.render_width,
            render_height: context.binding.render_height,
            view_metres_per_unit: context.binding.view_metres_per_unit.clone(),
            axis_mapping: context.binding.axis_mapping,
            source_qpc: pair.view_projection_sample_qpc,
            root_signature_sha256,
            root_parameter_index: source.root_parameter_index,
            pass_ordinal: source.pass_ordinal,
            draw_ordinal: source.draw_ordinal,
            projection_a: pair.projection_a,
            projection_b: pair.projection_b,
            projection_decomposition_residual: pair.projection_decomposition_residual,
            held_out_depth_row_residual: pair.held_out_depth_row_residual,
        },
        camera_position,
        camera_rotation_quaternion: matrix_to_quaternion(camera_rotation),
        camera_intrinsics: depth_lab::camera::Intrinsics {
            fx: pair.camera_intrinsics_1920x1080.fx as f32,
            fy: pair.camera_intrinsics_1920x1080.fy as f32,
            cx: pair.camera_intrinsics_1920x1080.cx as f32,
            cy: pair.camera_intrinsics_1920x1080.cy as f32,
        },
        camera_speed: None,
    };
    let validation = if context.performance_validation_only {
        depth_lab::validate_performance_camera_pose_row(&camera)
    } else {
        depth_lab::validate_camera_pose_row(&camera)
    };
    validation.map_err(|reason| {
        format!(
            "KCD2 camera pose failed {} validation at Present {}: {reason}; projection=({:?},{:?})",
            if context.performance_validation_only {
                "performance"
            } else {
                "standard"
            },
            pair.present_sequence,
            pair.projection_a,
            pair.projection_b
        )
    })?;
    Ok((
        camera,
        PlayerRootSample {
            present_seq: pair.present_sequence,
            present_qpc: pair.present_boundary_qpc,
            kcd2_root_source: player_source,
            cp2077_camera_join: None,
            player_position,
            player_rotation_quaternion: matrix_to_quaternion(player_rotation),
        },
    ))
}

fn axis_matrix(mapping: depth_lab::D3d12AxisMapping) -> [[f64; 3]; 3] {
    let columns = [
        mapping.source_to_delivery([1.0, 0.0, 0.0]),
        mapping.source_to_delivery([0.0, 1.0, 0.0]),
        mapping.source_to_delivery([0.0, 0.0, 1.0]),
    ];
    std::array::from_fn(|row| std::array::from_fn(|column| columns[column][row]))
}

fn transform_rotation(rotation: [[f64; 3]; 3], axes: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    matrix_multiply(matrix_multiply(axes, rotation), matrix_transpose(axes))
}

fn matrix_multiply(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|row| {
        std::array::from_fn(|column| (0..3).map(|k| left[row][k] * right[k][column]).sum())
    })
}

fn matrix_transpose(matrix: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|row| std::array::from_fn(|column| matrix[column][row]))
}

fn quaternion_to_matrix([x, y, z, w]: [f64; 4]) -> [[f64; 3]; 3] {
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

fn matrix_to_quaternion(matrix: [[f64; 3]; 3]) -> [f64; 4] {
    let mut quaternion =
        depth_lab::camera::mat3_to_quat(&matrix.map(|row| row.map(|value| value as f32)))
            .map(f64::from);
    if quaternion[3] < 0.0 {
        quaternion = quaternion.map(|value| -value);
    }
    quaternion
}

fn write_envelope(
    writer: &mut BufWriter<std::fs::File>,
    session_id: &str,
    event: &LivePoseEvent,
) -> Result<(), String> {
    let envelope = LivePoseEnvelope {
        schema: KCD2_LIVE_POSE_EVENT_SCHEMA_V3,
        session_id,
        event,
    };
    serde_json::to_writer(&mut *writer, &envelope)
        .map_err(|error| format!("serialize KCD2 live pose event: {error}"))?;
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(|error| format!("write KCD2 live pose event: {error}"))
}

fn publish_writer_ready(output_dir: &Path, session_id: &str) -> Result<(), String> {
    let path = output_dir.join(WRITER_READY_FILE);
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let plan_schema = PLAN
        .read()
        .map_err(|_| "KCD2 writer-ready plan lock poisoned".to_string())?
        .as_ref()
        .ok_or("KCD2 writer-ready plan is absent")?
        .schema
        .clone();
    let ready = WriterReady {
        schema: WRITER_READY_SCHEMA_V1,
        status: "ready",
        process_id: std::process::id(),
        session_id,
        plan_schema: &plan_schema,
        event_file: EVENT_FILE,
    };
    let bytes = serde_json::to_vec_pretty(&ready)
        .map_err(|error| format!("serialize KCD2 live pose writer ready: {error}"))?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        drop(file);
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("publish {}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum LivePoseEvent {
    Header {
        process_id: u32,
        plan: Kcd2LivePosePlan,
        limitation: &'static str,
    },
    Pair {
        #[serde(flatten)]
        pair: Kcd2LivePosePair,
    },
    Discarded {
        #[serde(flatten)]
        discard: Kcd2LivePoseDiscard,
    },
    Fault {
        code: &'static str,
        message: String,
    },
    Stopped {
        pairs: u64,
        discards: u64,
        faults: u64,
        queue_drops: u64,
    },
}

#[derive(Serialize)]
struct LivePoseEnvelope<'a> {
    schema: &'static str,
    session_id: &'a str,
    #[serde(flatten)]
    event: &'a LivePoseEvent,
}

#[derive(Serialize)]
struct WriterReady<'a> {
    schema: &'static str,
    status: &'static str,
    process_id: u32,
    session_id: &'a str,
    plan_schema: &'a str,
    event_file: &'static str,
}

#[must_use]
pub fn event_path(output_dir: &Path) -> PathBuf {
    output_dir.join(EVENT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These lifecycle tests enable a process-wide game mode and also replace
    /// camera_probe/probe projection caches. A module-local mutex cannot isolate
    /// them from the parallel CP2077 tracker tests, which must see KCD2 disabled.
    /// Run the real lifecycle in a fresh test process instead of changing runtime
    /// globals, retrying a failure, or serializing the entire test suite.
    fn run_in_isolated_process(test_name: &str) -> bool {
        const CHILD_TEST: &str = "DM_KCD2_LIVE_POSE_TEST_CHILD";
        if std::env::var(CHILD_TEST).as_deref() == Ok(test_name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_TEST, test_name)
            .output()
            .expect("start isolated KCD2 lifecycle test");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
            "isolated {test_name} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn binding() -> depth_lab::Kcd2CombinedCameraBinding {
        serde_json::from_str(include_str!(
            "../../../config/depth-profiles/kcd2-camera-binding-candidate.json"
        ))
        .unwrap()
    }

    fn sample_pair(present_sequence: u64, present_boundary_qpc: i64) -> Kcd2LivePosePair {
        Kcd2LivePosePair {
            present_sequence,
            view_projection_source: None,
            horizontal_player_root_source: None,
            view_projection_sample_qpc: present_boundary_qpc - 2,
            player_root_sample_qpc: present_boundary_qpc - 1,
            present_boundary_qpc,
            camera_position_engine_units: [1.0, 2.0, 3.0],
            camera_rotation_quaternion_xyzw: [0.0, 0.0, 0.0, 1.0],
            camera_intrinsics_1920x1080: depth_lab::Kcd2CameraIntrinsics {
                fx: 960.0,
                fy: 960.0,
                cx: 960.0,
                cy: 540.0,
            },
            horizontal_fov_degrees: 90.0,
            aspect_ratio: 16.0 / 9.0,
            projection_a: depth_lab::KCD2_PROJECTION_A,
            projection_b: depth_lab::KCD2_PROJECTION_B,
            projection_decomposition_residual: 0.0,
            horizontal_player_root_position_engine_units: [4.0, 5.0, 6.0],
            horizontal_player_root_rotation_quaternion_xyzw: [0.0, 0.0, 0.0, 1.0],
            horizontal_player_root_pass_ordinal: Some(5),
            horizontal_player_root_draw_ordinal: Some(0),
            camera_minus_root_engine_units: [-3.0, -3.0, -3.0],
            held_out_depth_row_residual: 0.0,
            horizontal_player_root_rotation_3x3_row_major: [
                1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
            ],
        }
    }

    #[test]
    fn exact_draw_gate_keeps_both_kcd2_sources_and_rejects_generic_draws() {
        let plan = Kcd2LivePosePlan::pinned();
        assert!(plan_accepts_draw(
            &plan,
            &plan.view_projection_binding.root_signature_sha256,
            0,
            0,
        ));
        assert!(plan_accepts_draw(
            &plan,
            &plan.horizontal_player_root_binding.root_signature_sha256,
            39,
            15,
        ));
        assert!(!plan_accepts_draw(
            &plan,
            &plan.horizontal_player_root_binding.root_signature_sha256,
            39,
            16,
        ));
        assert!(!plan_accepts_draw(&plan, &"0".repeat(64), 0, 0));
    }

    #[test]
    fn v3_routes_only_pinned_sources_across_command_list_pass_changes() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        let alias = &plan.additional_view_projection_bindings[0];
        assert_eq!(
            plan_root_parameter_for_draw(&plan, &alias.root_signature_sha256, 32, 0),
            Some(1)
        );
        assert_eq!(
            plan_root_parameter_for_draw(&plan, &alias.root_signature_sha256, 32, 1),
            None
        );
        assert!(plan_accepts_draw(
            &plan,
            &plan.horizontal_player_root_binding.root_signature_sha256,
            15,
            0
        ));
        assert!(!plan_accepts_draw(
            &plan,
            &plan.horizontal_player_root_binding.root_signature_sha256,
            15,
            16
        ));
        assert!(!plan_accepts_draw(
            &Kcd2LivePosePlan::pinned(),
            &alias.root_signature_sha256,
            32,
            0
        ));
    }

    #[test]
    fn v3_materialization_keeps_the_observed_alias_and_rejects_missing_provenance() {
        let mut binding = binding();
        binding.schema = depth_lab::KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V2.into();
        binding.binding_id = depth_lab::KCD2_COMBINED_BINDING_ID_V2.into();
        binding.binding_version = depth_lab::KCD2_COMBINED_BINDING_VERSION_V2.into();
        binding.live_pose_plan = Kcd2LivePosePlan::pinned_v3();
        let alias = binding.live_pose_plan.additional_view_projection_bindings[0].clone();
        let context = Kcd2WriterContext {
            identity: depth_lab::StreamIdentity {
                session_id: "session".into(),
                run_id: "r0000".into(),
                generation: 1,
            },
            binding,
            dsv_id: depth_lab::KCD2_MAIN_SCENE_DSV_ID.into(),
            qpc_frequency_hz: 10_000_000,
            performance_validation_only: false,
        };
        let mut pair = sample_pair(10, 101);
        assert!(materialize_standard_pose(&context, &pair).is_err());
        pair.view_projection_source = Some(Kcd2LivePoseSourceIdentity {
            root_signature_sha256: alias.root_signature_sha256.clone(),
            root_parameter_index: 1,
            pass_ordinal: 32,
            draw_ordinal: 0,
        });
        assert!(materialize_standard_pose(&context, &pair).is_err());
        let player_source = Kcd2LivePoseSourceIdentity {
            root_signature_sha256: context
                .binding
                .live_pose_plan
                .horizontal_player_root_binding
                .root_signature_sha256
                .clone(),
            root_parameter_index: 0,
            pass_ordinal: 15,
            draw_ordinal: 2,
        };
        pair.horizontal_player_root_source = Some(player_source.clone());
        pair.horizontal_player_root_pass_ordinal = Some(15);
        pair.horizontal_player_root_draw_ordinal = Some(2);
        let (camera, player) = materialize_standard_pose(&context, &pair).unwrap();
        assert_eq!(player.kcd2_root_source, Some(player_source));
        let depth_lab::CameraPoseProvenance::ResolvedCombinedViewProjection {
            ref root_signature_sha256,
            root_parameter_index,
            pass_ordinal,
            draw_ordinal,
            ..
        } = camera.provenance
        else {
            panic!("expected combined provenance")
        };
        assert_eq!(
            root_signature_sha256,
            &format!("sha256:{}", alias.root_signature_sha256)
        );
        assert_eq!(
            (root_parameter_index, pass_ordinal, draw_ordinal),
            (1, 32, 0)
        );
        let mut mislabeled = camera.clone();
        if let depth_lab::CameraPoseProvenance::ResolvedCombinedViewProjection {
            binding_id,
            binding_version,
            ..
        } = &mut mislabeled.provenance
        {
            *binding_id = depth_lab::KCD2_COMBINED_BINDING_ID.into();
            *binding_version = depth_lab::KCD2_COMBINED_BINDING_VERSION.into();
        }
        assert!(depth_lab::validate_camera_pose_row(&mislabeled).is_err());
        for defect in 0..6 {
            let mut bad = pair.clone();
            match defect {
                0 => {
                    bad.horizontal_player_root_source
                        .as_mut()
                        .unwrap()
                        .root_signature_sha256 = "0".repeat(64)
                }
                1 => {
                    bad.horizontal_player_root_source
                        .as_mut()
                        .unwrap()
                        .root_parameter_index = 1
                }
                2 => {
                    bad.horizontal_player_root_source
                        .as_mut()
                        .unwrap()
                        .draw_ordinal = 16
                }
                3 => bad.horizontal_player_root_pass_ordinal = Some(5),
                4 => bad.horizontal_player_root_draw_ordinal = Some(1),
                _ => bad.horizontal_player_root_pass_ordinal = None,
            }
            assert!(
                materialize_standard_pose(&context, &bad).is_err(),
                "defect {defect}"
            );
        }
        pair.view_projection_source
            .as_mut()
            .unwrap()
            .root_parameter_index = 2;
        assert!(materialize_standard_pose(&context, &pair).is_err());
    }

    #[test]
    fn performance_run_keeps_both_projection_families_in_explicit_evidence_rows() {
        if run_in_isolated_process(
            "kcd2_live_pose::tests::performance_run_keeps_both_projection_families_in_explicit_evidence_rows",
        ) {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "dm-kcd2-performance-family-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let binding = binding();
        let identity = depth_lab::StreamIdentity {
            session_id: "performance-family".into(),
            run_id: "r0002".into(),
            generation: 9,
        };
        initialize_with_binding(
            &root,
            identity.clone(),
            &binding.executable_sha256,
            binding.clone(),
            depth_lab::KCD2_MAIN_SCENE_DSV_ID,
            true,
        )
        .unwrap();
        assert!(
            initialize_with_binding(
                &root,
                identity,
                &binding.executable_sha256,
                binding.clone(),
                depth_lab::KCD2_MAIN_SCENE_DSV_ID,
                false
            )
            .is_err()
        );
        let first = sample_pair(107200, 10_000_000);
        let mut second = sample_pair(107201, 10_100_000);
        second.projection_a = -2.5000061997793486e-6;
        second.projection_b = 0.020000049844384193;
        send_event_raw(LivePoseEvent::Pair { pair: first });
        send_event_raw(LivePoseEvent::Pair { pair: second });
        PAIRS.store(2, Ordering::Release);
        stop();
        assert_eq!(FAULTS.load(Ordering::Acquire), 0);
        let rows = std::fs::read_to_string(root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE))
            .unwrap();
        let rows = rows
            .lines()
            .map(|line| serde_json::from_str::<depth_lab::CameraPoseRow>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            depth_lab::validate_performance_camera_pose_row(row).unwrap();
            assert!(depth_lab::validate_camera_pose_row(row).is_err());
        }
        let events = std::fs::read_to_string(event_path(&root)).unwrap();
        assert_eq!(events.matches("\"event\":\"pair\"").count(), 2);
        reset_for_next_run().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn performance_near_plane_transition_keeps_actual_values_and_rejected_evidence() {
        let root =
            std::env::temp_dir().join(format!("dm-kcd2-rejected-pair-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut context = Kcd2WriterContext {
            identity: depth_lab::StreamIdentity {
                session_id: "near-plane-test".into(),
                run_id: "r0003".into(),
                generation: 7,
            },
            binding: binding(),
            dsv_id: depth_lab::KCD2_MAIN_SCENE_DSV_ID.into(),
            qpc_frequency_hz: 10_000_000,
            performance_validation_only: false,
        };
        let mut pair = sample_pair(107201, 10_000_000);
        pair.projection_a = -2.5000061997793486e-6;
        pair.projection_b = 0.020000049844384193;
        let error = materialize_standard_pose(&context, &pair).unwrap_err();
        preserve_rejected_pair(&root, &context, &pair, &error);
        let path = root.join("kcd2-pose-rejection.json");
        let original = std::fs::read(&path).unwrap();
        let diagnostic: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(diagnostic["pair"]["present_sequence"], 107201);
        assert_eq!(
            diagnostic["pair"]["projection_b"].as_f64(),
            Some(pair.projection_b)
        );
        preserve_rejected_pair(&root, &context, &pair, "later cleanup error");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        context.performance_validation_only = true;
        let (camera, _) = materialize_standard_pose(&context, &pair).unwrap();
        assert_eq!(
            camera.schema,
            depth_lab::PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1
        );
        depth_lab::validate_performance_camera_pose_row(&camera).unwrap();
        assert!(depth_lab::validate_camera_pose_row(&camera).is_err());
        if let depth_lab::CameraPoseProvenance::ResolvedCombinedViewProjection {
            projection_a,
            projection_b,
            ..
        } = camera.provenance
        {
            assert_eq!(projection_a, pair.projection_a);
            assert_eq!(projection_b, pair.projection_b);
        } else {
            panic!("wrong provenance");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writer_failure_is_visible_before_cleanup_and_is_latched_once() {
        if run_in_isolated_process(
            "kcd2_live_pose::tests::writer_failure_is_visible_before_cleanup_and_is_latched_once",
        ) {
            return;
        }
        FAULTED.store(false, Ordering::Release);
        FAULTS.store(0, Ordering::Release);
        *LAST_ERROR.lock().unwrap() = None;
        latch_writer_failure("first projection rejection".into());
        assert!(FAULTED.load(Ordering::Acquire));
        assert!(
            LAST_ERROR
                .lock()
                .unwrap()
                .as_deref()
                .unwrap()
                .contains("first projection rejection")
        );
        latch_writer_failure("later cleanup".into());
        assert_eq!(FAULTS.load(Ordering::Acquire), 1);
        assert!(
            !LAST_ERROR
                .lock()
                .unwrap()
                .as_deref()
                .unwrap()
                .contains("later cleanup")
        );
        FAULTED.store(false, Ordering::Release);
        FAULTS.store(0, Ordering::Release);
        *LAST_ERROR.lock().unwrap() = None;
    }

    #[test]
    fn standard_pose_materialization_applies_the_pinned_metric_axis_mapping() {
        let binding = binding();
        let context = Kcd2WriterContext {
            identity: depth_lab::StreamIdentity {
                session_id: "session".into(),
                run_id: "r0000".into(),
                generation: 1,
            },
            binding,
            dsv_id: "kcd2-r24g8-d24-main-scene-v1".into(),
            qpc_frequency_hz: 10_000_000,
            performance_validation_only: false,
        };
        let pair = sample_pair(10, 101);

        let (camera, player) = materialize_standard_pose(&context, &pair).unwrap();
        assert_eq!(camera.camera_position, [1.0, 3.0, 2.0]);
        assert_eq!(player.player_position, [4.0, 6.0, 5.0]);
        assert_eq!(camera.camera_rotation_quaternion, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(player.player_rotation_quaternion, [0.0, 0.0, 0.0, 1.0]);
        assert!(player.kcd2_root_source.is_none());
        depth_lab::validate_camera_pose_row(&camera).unwrap();

        let mut explicit_legacy_pair = pair;
        let root = &context
            .binding
            .live_pose_plan
            .horizontal_player_root_binding;
        explicit_legacy_pair.horizontal_player_root_source = Some(Kcd2LivePoseSourceIdentity {
            root_signature_sha256: root.root_signature_sha256.clone(),
            root_parameter_index: root.root_parameter_index,
            pass_ordinal: root.pass_ordinal,
            draw_ordinal: root.draw_ordinal,
        });
        assert!(
            materialize_standard_pose(&context, &explicit_legacy_pair)
                .unwrap()
                .1
                .kcd2_root_source
                .is_none()
        );
        explicit_legacy_pair
            .horizontal_player_root_source
            .as_mut()
            .unwrap()
            .root_parameter_index = 1;
        assert!(materialize_standard_pose(&context, &explicit_legacy_pair).is_err());
    }

    #[test]
    fn missing_sources_reason_survives_a_full_diagnostic_queue() {
        if run_in_isolated_process(
            "kcd2_live_pose::tests::missing_sources_reason_survives_a_full_diagnostic_queue",
        ) {
            return;
        }
        let (tx, rx) = sync_channel(1);
        tx.send(LivePoseEvent::Stopped {
            pairs: 0,
            discards: 0,
            faults: 0,
            queue_drops: 0,
        })
        .unwrap();
        *EVENT_TX.lock().unwrap() = Some(tx);
        fail_missing_formal_sources(Kcd2LivePoseDiscard {
            present_sequence: 33_739,
            missing: vec!["view_projection"],
        });
        let error = failure().unwrap();
        assert!(error.contains("kcd2_live_pose_sources_missing"), "{error}");
        assert!(error.contains("33739"), "{error}");
        assert!(error.contains("view_projection"), "{error}");
        assert_eq!(summary().discards, 1);
        assert_eq!(summary().queue_drops, 2);
        assert!(matches!(rx.try_recv(), Ok(LivePoseEvent::Stopped { .. })));
    }

    #[test]
    fn formal_present_without_sources_faults_at_its_boundary_and_preserves_diagnostics() {
        if run_in_isolated_process(
            "kcd2_live_pose::tests::formal_present_without_sources_faults_at_its_boundary_and_preserves_diagnostics",
        ) {
            return;
        }
        let root =
            std::env::temp_dir().join(format!("dm-kcd2-missing-sources-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let binding = binding();
        let executable_sha256 = binding.executable_sha256.clone();
        initialize_with_binding(
            &root,
            depth_lab::StreamIdentity {
                session_id: "session-missing-sources".into(),
                run_id: "r0000".into(),
                generation: 1,
            },
            &executable_sha256,
            binding,
            "kcd2-r24g8-d24-main-scene-v1",
            false,
        )
        .unwrap();
        begin_production_warmup().unwrap();
        // A partial-source warmup discard is diagnostic only, never a formal row or fault.
        record_warmup_discard(Kcd2LivePoseDiscard {
            present_sequence: 0,
            missing: vec!["horizontal_player_root"],
        });
        let partial = warmup_diagnostics().unwrap();
        assert_eq!(partial.discards, 1);
        assert_eq!(partial.last_discard_present, Some(0));
        assert_eq!(partial.missing, vec!["horizontal_player_root"]);
        assert_eq!(
            warmup_diagnostics().unwrap(),
            partial,
            "snapshot is read-only"
        );
        finish_present(1, false);
        let absent = warmup_diagnostics().unwrap();
        assert_eq!(absent.discards, 2);
        assert_eq!(absent.last_discard_present, Some(1));
        assert_eq!(
            absent.missing,
            vec!["view_projection", "horizontal_player_root"]
        );
        assert!(!pair_ready_for_present(1));
        assert_eq!(summary().pairs, 0);
        assert_eq!(summary().discards, 0);
        assert_eq!(summary().faults, 0);
        assert!(
            failure().is_none(),
            "warmup may still be collecting sources"
        );
        arm_production().unwrap();
        finish_present(33_739, true);
        let error = failure().expect("missing formal sources must fault before a later readback");
        assert!(error.contains("kcd2_live_pose_sources_missing"), "{error}");
        assert!(error.contains("33739"), "{error}");
        assert!(
            error.contains("view_projection, horizontal_player_root"),
            "{error}"
        );
        assert!(!pair_ready_for_present(33_739));
        finish_present(33_740, true);
        assert_eq!(failure().as_deref(), Some(error.as_str()));
        stop();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(root.join(EVENT_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.iter().filter(|row| row["event"] == "pair").count(), 0);
        let discarded = rows.iter().find(|row| row["event"] == "discarded").unwrap();
        assert_eq!(discarded["present_sequence"], 33_739);
        let fault = rows.iter().find(|row| row["event"] == "fault").unwrap();
        assert_eq!(fault["code"], "kcd2_live_pose_sources_missing");
        assert_eq!(summary().discards, 1);
        assert_eq!(
            warmup_diagnostics().unwrap(),
            absent,
            "formal fault is separate"
        );
        reset_for_next_run().unwrap();
        assert_eq!(warmup_diagnostics().unwrap(), WarmupDiagnostics::default());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writer_publishes_standard_camera_and_player_indexes() {
        if run_in_isolated_process(
            "kcd2_live_pose::tests::writer_publishes_standard_camera_and_player_indexes",
        ) {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "dm-kcd2-standard-pose-writer-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binding = binding();
        let executable_sha256 = binding.executable_sha256.clone();
        initialize_with_binding(
            &root,
            depth_lab::StreamIdentity {
                session_id: "session-standard".into(),
                run_id: "r0000".into(),
                generation: 1,
            },
            &executable_sha256,
            binding,
            "kcd2-r24g8-d24-main-scene-v1",
            false,
        )
        .unwrap();
        let pair = sample_pair(20, 1_000);
        let viewport = crate::d3d12_render_camera::CapturedViewport {
            count: 1,
            first: windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT {
                Width: 1920.0,
                Height: 1080.0,
                MinDepth: 0.125,
                MaxDepth: 0.875,
                ..Default::default()
            },
        };
        begin_production_warmup().unwrap();
        assert!(publish_pair_projection(1, &pair, viewport, true).is_err());
        assert!(
            crate::camera_probe::take_observed_projection(1, 20)
                .unwrap()
                .is_none()
        );
        arm_production().unwrap();
        publish_pair_projection(1, &pair, viewport, false).unwrap();
        assert!(
            crate::camera_probe::take_observed_projection(1, 20)
                .unwrap()
                .is_none()
        );
        assert!(crate::probe::take_exact_draw_viewport(1, 20).is_err());
        assert!(publish_pair_projection(2, &pair, viewport, true).is_err());
        publish_pair_projection(1, &pair, viewport, true).unwrap();
        assert!(publish_pair_projection(1, &pair, viewport, true).is_err());
        assert!(crate::camera_probe::take_observed_projection(2, 20).is_err());
        assert!(
            crate::camera_probe::take_observed_projection(1, 19)
                .unwrap()
                .is_none()
        );
        let projection = crate::camera_probe::take_observed_projection(1, 20)
            .unwrap()
            .unwrap();
        assert_eq!(projection.a, pair.projection_a);
        assert_eq!(projection.b, pair.projection_b);
        assert_eq!(projection.metric_scale, 1.0);
        assert_eq!(
            crate::probe::take_exact_draw_viewport(1, 20).unwrap(),
            (0.125, 0.875)
        );
        assert!(
            crate::camera_probe::take_observed_projection(1, 20)
                .unwrap()
                .is_none()
        );
        assert!(crate::probe::take_exact_draw_viewport(1, 20).is_err());
        PAIRS.store(2, Ordering::Release);
        LAST_PAIR_PRESENT.store(21, Ordering::Release);
        send_event(LivePoseEvent::Pair {
            pair: sample_pair(20, 1_000),
        });
        send_event(LivePoseEvent::Pair {
            pair: sample_pair(21, 1_100),
        });
        stop();
        assert!(failure().is_none(), "{:?}", failure());

        let camera_path = root.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE);
        let player_path = root.join(crate::player_writer::PLAYER_POSE_INDEX_FILE);
        let camera_rows = std::fs::read_to_string(camera_path).unwrap();
        let player_rows = std::fs::read_to_string(player_path).unwrap();
        assert_eq!(camera_rows.lines().count(), 2);
        assert_eq!(player_rows.lines().count(), 2);
        for line in camera_rows.lines() {
            let row: depth_lab::CameraPoseRow = serde_json::from_str(line).unwrap();
            depth_lab::validate_camera_pose_row(&row).unwrap();
        }
        for line in player_rows.lines() {
            let row: depth_lab::PlayerPoseRow = serde_json::from_str(line).unwrap();
            depth_lab::validate_player_pose_row(&row).unwrap();
        }
        reset_for_next_run().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stopped_writer_can_restart_for_the_next_run_in_the_same_process() {
        if run_in_isolated_process(
            "kcd2_live_pose::tests::stopped_writer_can_restart_for_the_next_run_in_the_same_process",
        ) {
            return;
        }
        let root =
            std::env::temp_dir().join(format!("dm-kcd2-live-pose-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = root.join("r0000");
        let second = root.join("r0001");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let binding = binding();
        let executable_sha256 = binding.executable_sha256.clone();

        assert!(
            initialize_with_binding(
                &first,
                depth_lab::StreamIdentity {
                    session_id: "session-restart".into(),
                    run_id: "r0000".into(),
                    generation: 1,
                },
                &executable_sha256,
                binding.clone(),
                "kcd2-r24g8-d24-main-scene-v1",
                false,
            )
            .unwrap()
        );
        stop();
        assert!(event_path(&first).is_file());
        reset_for_next_run().unwrap();
        assert!(!enabled());

        assert!(
            initialize_with_binding(
                &second,
                depth_lab::StreamIdentity {
                    session_id: "session-restart".into(),
                    run_id: "r0001".into(),
                    generation: 2,
                },
                &executable_sha256,
                binding,
                "kcd2-r24g8-d24-main-scene-v1",
                false,
            )
            .unwrap()
        );
        stop();
        assert!(event_path(&second).is_file());

        let first_events = std::fs::read_to_string(event_path(&first)).unwrap();
        let second_events = std::fs::read_to_string(event_path(&second)).unwrap();
        for events in [&first_events, &second_events] {
            assert_eq!(events.matches("\"event\":\"header\"").count(), 1);
            assert_eq!(events.matches("\"event\":\"stopped\"").count(), 1);
            assert!(!events.contains("\"event\":\"pair\""));
            assert!(!events.contains("\"event\":\"discarded\""));
        }
        for run in [&first, &second] {
            assert!(
                run.join(crate::camera_writer::CAMERA_POSE_INDEX_FILE)
                    .is_file()
            );
            assert!(
                run.join(crate::camera_writer::CAMERA_POSE_DIAGNOSTIC_FILE)
                    .is_file()
            );
            assert!(
                run.join(crate::player_writer::PLAYER_POSE_INDEX_FILE)
                    .is_file()
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }
}
