//! Camera discovery 的传输无关纯契约。
//!
//! 旧报告把进程内 `buffer_tag` 当槽位并用整段 run 的 50% hit-rate 过滤；ring buffer
//! 一轮换或 operator 切到下一组 FOV，候选就会被历史分母藏掉。本模块只认 production
//! 能复现的 draw 窗口，并且每个 phase 独立统计；`buffer_tag` 根本不进入类型系统。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::DsvDiscoveryDescriptor;
use crate::camera::{Mat4, MatrixKind, ScannedMatrix, scan_candidates};
use crate::camera_binding::{
    MatrixEncoding, MatrixLayout, MatrixRole, MatrixSourceBinding, ShaderStage, ViewSourceBinding,
};
use crate::probe_config::{
    CAMERA_DISCOVERY_SCHEMA_V1, CAMERA_DISCOVERY_SCHEMA_V2, CAMERA_DISCOVERY_SCHEMA_V3,
    CameraDiscoveryConfig, CameraDiscoveryDsvSelection, CameraDiscoveryRenderIdentity,
    render_identity_from_dsv_descriptor,
};

pub const CAMERA_DISCOVERY_REPORT_SCHEMA_V1: &str = "camera-discovery-report/v1";
pub const CAMERA_DISCOVERY_REPORT_SCHEMA_V2: &str = "camera-discovery-report/v2";
/// v3 changes the exact-build comparison quaternion from the engine's
/// world-to-camera basis to the delivery contract's camera-to-world basis.
pub const CAMERA_DISCOVERY_REPORT_SCHEMA_V3: &str = "camera-discovery-report/v3";
/// v4 retains the rasterizer viewport at each original draw, so a weapon-layer basis cannot
/// masquerade as a world camera merely because its orientation and FOV follow the player.
pub const CAMERA_DISCOVERY_REPORT_SCHEMA_V4: &str = "camera-discovery-report/v4";
pub const CAMERA_DISCOVERY_PROGRESS_SCHEMA_V1: &str = "camera-discovery-progress/v1";
pub const MAX_DISCOVERY_CANDIDATES: usize = 8_192;
/// Global, not per-candidate: a pathological shader must not multiply an unbounded timeline by
/// thousands of plausible matrices. Normal #636 phases are ~80 samples and stay far below this.
pub const MAX_DISCOVERY_VIEW_OBSERVATIONS: usize = 262_144;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryViewport {
    pub count: u32,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

impl DiscoveryViewport {
    /// Conservative authoring eligibility, NOT proof of a world camera. Compressed depth layers
    /// (e.g. first-person weapons at 0..0.01) cannot represent the scene. Keep the measured values;
    /// never replace them with a guessed 0..1 viewport or adjust raw depth to make it pass E-geo.
    pub fn is_main_scene_candidate(self, width: u32, height: u32) -> bool {
        self.count == 1
            && [
                self.x,
                self.y,
                self.width,
                self.height,
                self.min_depth,
                self.max_depth,
            ]
            .iter()
            .all(|value| value.is_finite())
            && self.x == 0.0
            && self.y == 0.0
            && self.width == width as f32
            && self.height == height as f32
            && self.min_depth >= 0.0
            && self.max_depth <= 1.0
            && self.max_depth - self.min_depth >= 0.5
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryDrawContext {
    pub present_seq: u64,
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
    /// Missing or ambiguous state stays explicit; it is never inherited from another draw.
    pub viewport: Option<DiscoveryViewport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryViewObservation {
    pub present_seq: u64,
    pub matrix: Mat4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryReferenceCameraObservation {
    pub present_seq: u64,
    pub present_qpc: i64,
    pub camera_position: [f64; 3],
    pub camera_rotation_quaternion: [f64; 4],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryBoundBuffer {
    pub bind_slot: u32,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryDrawSample {
    pub present_seq: u64,
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
    pub bound_buffers: Vec<DiscoveryBoundBuffer>,
}

/// 同一个 sampled Present 内，一份 constant-buffer 版本覆盖的全部 draw provenance。
///
/// `buffer_tag` / `buffer_version` 只参与 probe 进程内的聚合键，不进入这个传输无关类型，
/// 更不会落进报告。worker 对每个 snapshot 只扫描一次字节，再把候选展开到所有原始 draw。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryBindingSnapshot {
    pub pass_ordinal: u32,
    pub bind_slot: u32,
    pub draw_ordinals: Vec<u32>,
    pub bytes: Arc<[u8]>,
}

/// 一个 sampled Present 的有界后台工作单元。
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryPresentBatch {
    pub present_seq: u64,
    /// 即使某个 draw 没有可扫描的 VS constant buffer，也必须进入处理账本。
    pub logical_draws: u64,
    pub bindings: Vec<DiscoveryBindingSnapshot>,
    pub draw_contexts: Vec<DiscoveryDrawContext>,
    /// Exact-Present sample from the legacy singleton. This is comparison evidence only.
    pub comparison_camera_reference: Option<DiscoveryReferenceCameraObservation>,
    /// Sampling failures are durable blockers; they must not silently turn into a shorter track.
    pub comparison_camera_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySamplingStats {
    pub observed_presents: u64,
    pub scheduled_presents: u64,
    pub observed_draws: u64,
    pub scheduled_draws: u64,
    pub submitted_draws: u64,
    pub queue_dropped_draws: u64,
    pub runtime_dropped_callbacks: u64,
    pub tracked_buffer_overflows: u64,
    pub dsv_ambiguities: u64,
    /// A config-driven phase switch can quiesce after Draw but before that Present reaches the
    /// Present hook. Such a partial batch is never evidence; it is excluded explicitly here.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub phase_boundary_discarded_presents: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub phase_boundary_discarded_draws: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryBlocker {
    NoSubmittedDraws,
    DrawAccountingMismatch,
    CandidateLimit,
    QueueSaturated,
    RuntimeCallbackDropped,
    TrackedBufferLimit,
    AmbiguousDepthTarget,
    ViewObservationLimit,
    ComparisonCameraMissing,
    ComparisonCameraError,
    DrawContextLimit,
    DrawContextAccountingMismatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryViewCandidate {
    pub source: ViewSourceBinding,
    pub frames: u32,
    pub matrix: Mat4,
    pub distinct_values: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<DiscoveryViewObservation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryProjectionCandidate {
    pub source: MatrixSourceBinding,
    pub frames: u32,
    pub matrix: Mat4,
    pub distinct_values: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDiscoveryReport {
    pub schema: String,
    pub phase_id: String,
    pub render: CameraDiscoveryRenderIdentity,
    pub config: CameraDiscoveryConfig,
    /// Exact source binding replayed by the probe at activation. Present only for config v3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsv_selection: Option<CameraDiscoveryDsvSelection>,
    /// Full 17-field D3D11 DSV identity returned by strict source replay. This, not `render`, is
    /// the v3 runtime selection authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_dsv_descriptor: Option<DsvDiscoveryDescriptor>,
    pub sampling: DiscoverySamplingStats,
    pub processed_draws: u64,
    pub candidate_overflows: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub view_observation_overflows: u64,
    pub reliable: bool,
    pub blockers: Vec<DiscoveryBlocker>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparison_camera_samples: Vec<DiscoveryReferenceCameraObservation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comparison_camera_errors: Vec<String>,
    pub view_candidates: Vec<DiscoveryViewCandidate>,
    pub projection_candidates: Vec<DiscoveryProjectionCandidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub draw_contexts: Vec<DiscoveryDrawContext>,
}

impl CameraDiscoveryReport {
    /// Validate the v3 authority boundary. `render` remains in the report for legacy consumers,
    /// but is derived from the exact descriptor and can never replace it as runtime authority.
    pub fn validate_dsv_selection_binding(&self) -> Result<(), String> {
        match self.config.schema.as_str() {
            CAMERA_DISCOVERY_SCHEMA_V3 => {
                if !matches!(
                    self.schema.as_str(),
                    CAMERA_DISCOVERY_REPORT_SCHEMA_V3 | CAMERA_DISCOVERY_REPORT_SCHEMA_V4
                ) {
                    return Err(format!(
                        "camera discovery v3 config requires report schema {CAMERA_DISCOVERY_REPORT_SCHEMA_V3}"
                    ));
                }
                let source = self
                    .config
                    .dsv_selection
                    .as_ref()
                    .ok_or("camera discovery v3 report config is missing dsv_selection")?;
                if self.dsv_selection.as_ref() != Some(source) {
                    return Err(
                        "camera discovery v3 report source binding differs from config".into(),
                    );
                }
                let descriptor = self
                    .selected_dsv_descriptor
                    .ok_or("camera discovery v3 report is missing selected DSV descriptor")?;
                if self.config.render.is_some() {
                    return Err("camera discovery v3 report config carries legacy render".into());
                }
                if self.render != render_identity_from_dsv_descriptor(descriptor)? {
                    return Err(
                        "camera discovery v3 derived render differs from selected descriptor"
                            .into(),
                    );
                }
            }
            CAMERA_DISCOVERY_SCHEMA_V1 | CAMERA_DISCOVERY_SCHEMA_V2 => {
                if self.dsv_selection.is_some() || self.selected_dsv_descriptor.is_some() {
                    return Err(
                        "legacy camera discovery report carries v3 selection authority".into(),
                    );
                }
            }
            _ => return Err("camera discovery report config schema is unsupported".into()),
        }
        Ok(())
    }

    pub fn validate_main_scene_draw(
        &self,
        candidate: &DiscoveryViewCandidate,
    ) -> Result<(), String> {
        if self.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V4 {
            return Err("main-scene selection requires report/v4 exact-draw viewport evidence; resample old reports".into());
        }
        let source = candidate.source.source;
        let mut contexts = BTreeMap::new();
        for row in self.draw_contexts.iter().filter(|row| {
            row.pass_ordinal == source.pass_ordinal && row.draw_ordinal == source.draw_ordinal
        }) {
            if contexts.insert(row.present_seq, row.viewport).is_some() {
                return Err("duplicate exact-draw viewport evidence".into());
            }
        }
        for observation in &candidate.observations {
            let viewport = contexts
                .get(&observation.present_seq)
                .copied()
                .flatten()
                .ok_or_else(|| {
                    format!(
                        "missing exact-draw viewport at Present {}",
                        observation.present_seq
                    )
                })?;
            if !viewport.is_main_scene_candidate(self.render.width, self.render.height) {
                return Err(format!(
                    "non-scene viewport at Present {} pass {} draw {}: {:?}",
                    observation.present_seq, source.pass_ordinal, source.draw_ordinal, viewport
                ));
            }
        }
        Ok(())
    }
}

/// Small mutable control-plane view of a discovery checkpoint.
///
/// The full candidate report can be tens of MiB. Operators and sweep orchestration must never
/// parse that growing file merely to learn whether a phase has enough samples, so the worker
/// publishes this independently bounded sidecar first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraDiscoveryProgress {
    pub schema: String,
    pub report_schema: String,
    pub phase_id: String,
    pub draw_window_start: u32,
    pub draw_window_count: u32,
    pub comparison_camera_samples: u64,
    pub view_candidates: u64,
    pub processed_draws: u64,
    pub candidate_overflows: u64,
    pub view_observation_overflows: u64,
    #[serde(default)]
    pub phase_boundary_discarded_presents: u64,
    #[serde(default)]
    pub phase_boundary_discarded_draws: u64,
    pub reliable: bool,
    pub blockers: Vec<DiscoveryBlocker>,
}

impl From<&CameraDiscoveryReport> for CameraDiscoveryProgress {
    fn from(report: &CameraDiscoveryReport) -> Self {
        Self {
            schema: CAMERA_DISCOVERY_PROGRESS_SCHEMA_V1.into(),
            report_schema: report.schema.clone(),
            phase_id: report.phase_id.clone(),
            draw_window_start: report.config.draw_window_start,
            draw_window_count: report.config.draw_window_count,
            comparison_camera_samples: report.comparison_camera_samples.len() as u64,
            view_candidates: report.view_candidates.len() as u64,
            processed_draws: report.processed_draws,
            candidate_overflows: report.candidate_overflows,
            view_observation_overflows: report.view_observation_overflows,
            phase_boundary_discarded_presents: report.sampling.phase_boundary_discarded_presents,
            phase_boundary_discarded_draws: report.sampling.phase_boundary_discarded_draws,
            reliable: report.reliable,
            blockers: report.blockers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CandidateState {
    frames: u32,
    last_present_seq: Option<u64>,
    matrix: Mat4,
    distinct_values: u32,
    observations: Vec<DiscoveryViewObservation>,
}

impl CandidateState {
    fn observe(&mut self, present_seq: u64, matrix: Mat4) {
        if self.last_present_seq == Some(present_seq) {
            return;
        }
        if self.frames == 0 || self.matrix != matrix {
            self.distinct_values += 1;
        }
        self.frames += 1;
        self.last_present_seq = Some(present_seq);
        self.matrix = matrix;
    }
}

#[derive(Debug)]
pub struct CameraDiscoveryTracker {
    processed_draws: u64,
    candidate_overflows: u64,
    candidate_limit: usize,
    track_view_observations: bool,
    view_observation_count: usize,
    view_observation_limit: usize,
    view_observation_overflows: u64,
    comparison_camera_expected: bool,
    comparison_camera_samples: Vec<DiscoveryReferenceCameraObservation>,
    comparison_camera_errors: Vec<String>,
    comparison_camera_missing: u64,
    selected_dsv_descriptor: Option<DsvDiscoveryDescriptor>,
    views: BTreeMap<ViewSourceBinding, CandidateState>,
    projections: BTreeMap<MatrixSourceBinding, CandidateState>,
    draw_contexts: Vec<DiscoveryDrawContext>,
    draw_context_overflows: u64,
    draw_context_accounting_errors: u64,
    #[cfg(test)]
    scanned_buffer_snapshots: u64,
}

impl Default for CameraDiscoveryTracker {
    fn default() -> Self {
        Self {
            processed_draws: 0,
            candidate_overflows: 0,
            candidate_limit: MAX_DISCOVERY_CANDIDATES,
            track_view_observations: false,
            view_observation_count: 0,
            view_observation_limit: MAX_DISCOVERY_VIEW_OBSERVATIONS,
            view_observation_overflows: 0,
            comparison_camera_expected: false,
            comparison_camera_samples: Vec::new(),
            comparison_camera_errors: Vec::new(),
            comparison_camera_missing: 0,
            selected_dsv_descriptor: None,
            views: BTreeMap::new(),
            projections: BTreeMap::new(),
            draw_contexts: Vec::new(),
            draw_context_overflows: 0,
            draw_context_accounting_errors: 0,
            #[cfg(test)]
            scanned_buffer_snapshots: 0,
        }
    }
}

impl CameraDiscoveryTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn for_config(config: &CameraDiscoveryConfig) -> Self {
        Self {
            track_view_observations: matches!(
                config.schema.as_str(),
                CAMERA_DISCOVERY_SCHEMA_V2 | CAMERA_DISCOVERY_SCHEMA_V3
            ),
            comparison_camera_expected: config.comparison_camera_reference.is_some(),
            ..Self::default()
        }
    }

    /// Construct the v3 tracker only after the attestation and all of its sources were replayed.
    #[must_use]
    pub fn for_validated_dsv_selection(
        config: &CameraDiscoveryConfig,
        selected_dsv_descriptor: DsvDiscoveryDescriptor,
    ) -> Self {
        Self {
            selected_dsv_descriptor: Some(selected_dsv_descriptor),
            ..Self::for_config(config)
        }
    }

    pub fn observe_draw(&mut self, sample: &DiscoveryDrawSample) {
        self.observe_batch(&DiscoveryPresentBatch {
            present_seq: sample.present_seq,
            logical_draws: 1,
            bindings: sample
                .bound_buffers
                .iter()
                .map(|buffer| DiscoveryBindingSnapshot {
                    pass_ordinal: sample.pass_ordinal,
                    bind_slot: buffer.bind_slot,
                    draw_ordinals: vec![sample.draw_ordinal],
                    bytes: buffer.bytes.clone(),
                })
                .collect(),
            comparison_camera_reference: None,
            comparison_camera_error: None,
            draw_contexts: vec![DiscoveryDrawContext {
                present_seq: sample.present_seq,
                pass_ordinal: sample.pass_ordinal,
                draw_ordinal: sample.draw_ordinal,
                viewport: None,
            }],
        });
    }

    pub fn observe_batch(&mut self, batch: &DiscoveryPresentBatch) {
        if self.track_view_observations {
            let mut keys = std::collections::BTreeSet::new();
            if batch.draw_contexts.len() as u64 != batch.logical_draws
                || batch.draw_contexts.iter().any(|row| {
                    row.present_seq != batch.present_seq
                        || !keys.insert((row.pass_ordinal, row.draw_ordinal))
                })
            {
                self.draw_context_accounting_errors += 1;
            }
            for row in &batch.draw_contexts {
                if self.draw_contexts.len() < MAX_DISCOVERY_VIEW_OBSERVATIONS {
                    self.draw_contexts.push(row.clone());
                } else {
                    self.draw_context_overflows += 1;
                }
            }
        }
        if self.comparison_camera_expected {
            match (
                batch.comparison_camera_reference.as_ref(),
                batch.comparison_camera_error.as_ref(),
            ) {
                (Some(sample), None) => self.comparison_camera_samples.push(sample.clone()),
                (None, Some(error)) => self.comparison_camera_errors.push(error.clone()),
                _ => {
                    self.comparison_camera_missing =
                        self.comparison_camera_missing.saturating_add(1)
                }
            }
        }
        self.processed_draws = self.processed_draws.saturating_add(batch.logical_draws);
        for binding in &batch.bindings {
            let Ok(buffer_bytes) = u32::try_from(binding.bytes.len()) else {
                continue;
            };
            #[cfg(test)]
            {
                self.scanned_buffer_snapshots = self.scanned_buffer_snapshots.saturating_add(1);
            }
            for scanned in scan_candidates(&binding.bytes) {
                for &draw_ordinal in &binding.draw_ordinals {
                    self.observe_scanned_matrix(
                        batch.present_seq,
                        binding.pass_ordinal,
                        draw_ordinal,
                        binding.bind_slot,
                        buffer_bytes,
                        scanned,
                    );
                }
            }
        }
    }

    fn observe_scanned_matrix(
        &mut self,
        present_seq: u64,
        pass_ordinal: u32,
        draw_ordinal: u32,
        bind_slot: u32,
        buffer_bytes: u32,
        ScannedMatrix {
            offset,
            matrix,
            kind,
            transpose,
        }: ScannedMatrix,
    ) {
        // production binding 按 constant-buffer register 寻址；不能输出一个生产端
        // 必然拒绝的 4-byte 对齐“候选”。
        if !offset.is_multiple_of(16) {
            return;
        }
        let Ok(offset) = u32::try_from(offset) else {
            return;
        };
        let source = MatrixSourceBinding {
            shader_stage: ShaderStage::Vertex,
            bind_slot,
            pass_ordinal,
            draw_ordinal,
            buffer_bytes,
            offset,
            encoding: MatrixEncoding::F32LittleEndian,
            layout: MatrixLayout::RowMajor,
            transpose,
            role: match kind {
                MatrixKind::View { .. } => MatrixRole::View,
                MatrixKind::Projection => MatrixRole::Projection,
                MatrixKind::Unknown => return,
            },
        };
        match kind {
            MatrixKind::View { translation } => {
                let source = ViewSourceBinding {
                    source,
                    translation,
                };
                if !self.views.contains_key(&source)
                    && self.views.len().saturating_add(self.projections.len())
                        >= self.candidate_limit
                {
                    self.candidate_overflows = self.candidate_overflows.saturating_add(1);
                    return;
                }
                let is_new_present = self
                    .views
                    .get(&source)
                    .is_none_or(|state| state.last_present_seq != Some(present_seq));
                let record_observation = self.track_view_observations && is_new_present;
                let observation_allowed =
                    record_observation && self.view_observation_count < self.view_observation_limit;
                if record_observation && !observation_allowed {
                    self.view_observation_overflows =
                        self.view_observation_overflows.saturating_add(1);
                }
                let state = self.views.entry(source).or_insert(CandidateState {
                    frames: 0,
                    last_present_seq: None,
                    matrix,
                    distinct_values: 0,
                    observations: Vec::new(),
                });
                if observation_allowed {
                    state.observations.push(DiscoveryViewObservation {
                        present_seq,
                        matrix,
                    });
                    self.view_observation_count += 1;
                }
                state.observe(present_seq, matrix);
            }
            MatrixKind::Projection => {
                if !self.projections.contains_key(&source)
                    && self.views.len().saturating_add(self.projections.len())
                        >= self.candidate_limit
                {
                    self.candidate_overflows = self.candidate_overflows.saturating_add(1);
                    return;
                }
                self.projections
                    .entry(source)
                    .or_insert(CandidateState {
                        frames: 0,
                        last_present_seq: None,
                        matrix,
                        distinct_values: 0,
                        observations: Vec::new(),
                    })
                    .observe(present_seq, matrix);
            }
            MatrixKind::Unknown => {}
        }
    }

    #[must_use]
    pub fn report(
        &self,
        config: &CameraDiscoveryConfig,
        sampling: DiscoverySamplingStats,
    ) -> CameraDiscoveryReport {
        let mut blockers = Vec::new();
        if sampling.submitted_draws == 0 {
            blockers.push(DiscoveryBlocker::NoSubmittedDraws);
        }
        if self.processed_draws != sampling.submitted_draws
            || sampling.scheduled_draws
                != sampling
                    .submitted_draws
                    .saturating_add(sampling.queue_dropped_draws)
                    .saturating_add(sampling.phase_boundary_discarded_draws)
            || sampling.phase_boundary_discarded_presents > 1
            || sampling.phase_boundary_discarded_draws
                > u64::from(config.max_main_passes_per_present)
                    .saturating_mul(u64::from(config.draw_window_count))
        {
            blockers.push(DiscoveryBlocker::DrawAccountingMismatch);
        }
        if self.candidate_overflows != 0 {
            blockers.push(DiscoveryBlocker::CandidateLimit);
        }
        if sampling.queue_dropped_draws != 0 {
            blockers.push(DiscoveryBlocker::QueueSaturated);
        }
        if sampling.runtime_dropped_callbacks != 0 {
            blockers.push(DiscoveryBlocker::RuntimeCallbackDropped);
        }
        if sampling.tracked_buffer_overflows != 0 {
            blockers.push(DiscoveryBlocker::TrackedBufferLimit);
        }
        if sampling.dsv_ambiguities != 0 {
            blockers.push(DiscoveryBlocker::AmbiguousDepthTarget);
        }
        if self.view_observation_overflows != 0 {
            blockers.push(DiscoveryBlocker::ViewObservationLimit);
        }
        if self.comparison_camera_expected && self.comparison_camera_missing != 0 {
            blockers.push(DiscoveryBlocker::ComparisonCameraMissing);
        }
        if self.comparison_camera_expected && !self.comparison_camera_errors.is_empty() {
            blockers.push(DiscoveryBlocker::ComparisonCameraError);
        }
        if self.draw_context_overflows != 0 {
            blockers.push(DiscoveryBlocker::DrawContextLimit);
        }
        if self.draw_context_accounting_errors != 0 {
            blockers.push(DiscoveryBlocker::DrawContextAccountingMismatch);
        }
        let render = config
            .render
            .clone()
            .or_else(|| {
                self.selected_dsv_descriptor
                    .and_then(|descriptor| render_identity_from_dsv_descriptor(descriptor).ok())
            })
            .expect("validated camera discovery tracker must have a render identity");
        CameraDiscoveryReport {
            schema: if matches!(
                config.schema.as_str(),
                CAMERA_DISCOVERY_SCHEMA_V2 | CAMERA_DISCOVERY_SCHEMA_V3
            ) {
                CAMERA_DISCOVERY_REPORT_SCHEMA_V4
            } else {
                CAMERA_DISCOVERY_REPORT_SCHEMA_V1
            }
            .into(),
            phase_id: config.phase_id.clone(),
            render,
            config: config.clone(),
            dsv_selection: config.dsv_selection.clone(),
            selected_dsv_descriptor: self.selected_dsv_descriptor,
            sampling,
            processed_draws: self.processed_draws,
            candidate_overflows: self.candidate_overflows,
            view_observation_overflows: self.view_observation_overflows,
            reliable: blockers.is_empty(),
            blockers,
            comparison_camera_samples: self.comparison_camera_samples.clone(),
            comparison_camera_errors: self.comparison_camera_errors.clone(),
            draw_contexts: self.draw_contexts.clone(),
            view_candidates: self
                .views
                .iter()
                .map(|(&source, state)| DiscoveryViewCandidate {
                    source,
                    frames: state.frames,
                    matrix: state.matrix,
                    distinct_values: state.distinct_values,
                    observations: state.observations.clone(),
                })
                .collect(),
            projection_candidates: self
                .projections
                .iter()
                .map(|(&source, state)| DiscoveryProjectionCandidate {
                    source,
                    frames: state.frames,
                    matrix: state.matrix,
                    distinct_values: state.distinct_values,
                })
                .collect(),
        }
    }
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_viewport_filter_preserves_observed_range_and_rejects_invalid_layers() {
        let scene = DiscoveryViewport {
            count: 1,
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            min_depth: 0.0098,
            max_depth: 1.0,
        };
        assert!(scene.is_main_scene_candidate(1920, 1080));
        assert_eq!(scene.min_depth, 0.0098);
        for bad in [
            DiscoveryViewport {
                min_depth: 0.0,
                max_depth: 0.01,
                ..scene
            },
            DiscoveryViewport { count: 0, ..scene },
            DiscoveryViewport { count: 2, ..scene },
            DiscoveryViewport { x: 1.0, ..scene },
            DiscoveryViewport {
                min_depth: -0.1,
                ..scene
            },
            DiscoveryViewport {
                max_depth: 1.1,
                ..scene
            },
            DiscoveryViewport {
                min_depth: f32::NAN,
                ..scene
            },
            DiscoveryViewport {
                width: f32::INFINITY,
                ..scene
            },
        ] {
            assert!(!bad.is_main_scene_candidate(1920, 1080), "{bad:?}");
        }
    }

    #[test]
    fn draw_context_accounting_cannot_silently_omit_or_duplicate_a_draw() {
        for rows in [
            Vec::new(),
            vec![
                DiscoveryDrawContext {
                    present_seq: 1,
                    pass_ordinal: 0,
                    draw_ordinal: 7,
                    viewport: None,
                },
                DiscoveryDrawContext {
                    present_seq: 1,
                    pass_ordinal: 0,
                    draw_ordinal: 7,
                    viewport: None,
                },
            ],
        ] {
            let config = evidence_config("POSE_STATIC");
            let mut tracker = CameraDiscoveryTracker::for_config(&config);
            tracker.observe_batch(&DiscoveryPresentBatch {
                present_seq: 1,
                logical_draws: 2,
                bindings: Vec::new(),
                draw_contexts: rows,
                comparison_camera_reference: None,
                comparison_camera_error: None,
            });
            let report = tracker.report(
                &config,
                DiscoverySamplingStats {
                    scheduled_draws: 2,
                    submitted_draws: 2,
                    ..Default::default()
                },
            );
            assert!(!report.reliable);
            assert!(
                report
                    .blockers
                    .contains(&DiscoveryBlocker::DrawContextAccountingMismatch)
            );
        }
    }
    use crate::GameCameraBinding;
    use crate::probe_config::{
        CAMERA_DISCOVERY_SCHEMA_V1, CAMERA_DISCOVERY_SCHEMA_V2, CAMERA_DISCOVERY_SCHEMA_V3,
        CameraDiscoveryDsvSelection, CameraDiscoveryRenderIdentity, DiscoveryDepthFormat,
    };

    fn config(phase: &str) -> CameraDiscoveryConfig {
        CameraDiscoveryConfig {
            schema: CAMERA_DISCOVERY_SCHEMA_V1.into(),
            phase_id: phase.into(),
            executable_sha256: format!("sha256:{}", "0".repeat(64)),
            render: Some(CameraDiscoveryRenderIdentity {
                width: 2560,
                height: 1600,
                sample_count: 1,
                formats: vec![DiscoveryDepthFormat::R24G8Typeless],
            }),
            dsv_selection: None,
            sample_every_n_presents: 30,
            max_main_passes_per_present: 4,
            draw_window_start: 0,
            draw_window_count: 128,
            comparison_camera_reference: None,
        }
    }

    fn evidence_config(phase: &str) -> CameraDiscoveryConfig {
        let mut config = config(phase);
        config.schema = CAMERA_DISCOVERY_SCHEMA_V2.into();
        config.comparison_camera_reference = Some(GameCameraBinding {
            provider: "fallout4-player-camera-root-exact-build/v1".into(),
            camera_singleton_relocation: 1,
            camera_root_offset: 2,
            world_transform_offset: 3,
            metres_per_game_unit: "1".into(),
            axis_mapping: "x_z_y".into(),
        });
        config
    }

    fn exact_descriptor() -> DsvDiscoveryDescriptor {
        DsvDiscoveryDescriptor {
            view_format: 45,
            view_dimension: 3,
            view_flags: 0,
            mip_slice: 0,
            first_array_slice: 0,
            view_array_size: 1,
            resource_format: 44,
            width: 1920,
            height: 1080,
            resource_mip_levels: 1,
            resource_array_size: 1,
            sample_count: 1,
            sample_quality: 0,
            usage: 0,
            bind_flags: 0x48,
            cpu_access_flags: 0,
            misc_flags: 0,
        }
    }

    fn v3_config(phase: &str) -> CameraDiscoveryConfig {
        CameraDiscoveryConfig {
            schema: CAMERA_DISCOVERY_SCHEMA_V3.into(),
            phase_id: phase.into(),
            executable_sha256: format!("sha256:{}", "a".repeat(64)),
            render: None,
            dsv_selection: Some(CameraDiscoveryDsvSelection {
                evidence_root: "C:/evidence".into(),
                attestation_locator: "selection.json".into(),
                attestation_sha256: format!("sha256:{}", "b".repeat(64)),
            }),
            sample_every_n_presents: 30,
            max_main_passes_per_present: 4,
            draw_window_start: 0,
            draw_window_count: 128,
            comparison_camera_reference: None,
        }
    }

    #[test]
    fn v3_report_repeats_source_and_exact_descriptor_without_restoring_legacy_authority() {
        let config = v3_config("V3-A1");
        let descriptor = exact_descriptor();
        let tracker = CameraDiscoveryTracker::for_validated_dsv_selection(&config, descriptor);
        let report = tracker.report(&config, DiscoverySamplingStats::default());
        report.validate_dsv_selection_binding().unwrap();
        assert_eq!(report.dsv_selection, config.dsv_selection);
        assert_eq!(report.selected_dsv_descriptor, Some(descriptor));
        assert_eq!((report.render.width, report.render.height), (1920, 1080));
        let json = serde_json::to_value(&report).unwrap();
        assert!(json["config"].get("render").is_none());

        let mut missing_source = report.clone();
        missing_source.dsv_selection = None;
        assert!(missing_source.validate_dsv_selection_binding().is_err());
        let mut missing_descriptor = report.clone();
        missing_descriptor.selected_dsv_descriptor = None;
        assert!(missing_descriptor.validate_dsv_selection_binding().is_err());
        let mut forged_render = report;
        forged_render.render.width = 1280;
        assert!(forged_render.validate_dsv_selection_binding().is_err());

        let mut forged_report_schema = tracker.report(&config, DiscoverySamplingStats::default());
        forged_report_schema.schema = CAMERA_DISCOVERY_REPORT_SCHEMA_V2.into();
        assert!(
            forged_report_schema
                .validate_dsv_selection_binding()
                .is_err()
        );

        let mut unknown_config_schema = forged_report_schema;
        unknown_config_schema.config.schema = "camera-discovery-config/v999".into();
        unknown_config_schema.dsv_selection = None;
        unknown_config_schema.selected_dsv_descriptor = None;
        assert!(
            unknown_config_schema
                .validate_dsv_selection_binding()
                .is_err()
        );
    }

    fn projection(horizontal_scale: f32, vertical_scale: f32, near: f32) -> Mat4 {
        [
            [horizontal_scale, 0.0, 0.0, 0.0],
            [0.0, vertical_scale, 0.0, 0.0],
            [0.0, 0.0, 1.0, -1.0],
            [0.0, 0.0, near, 0.0],
        ]
    }

    fn bytes_at(offset: usize, matrix: Mat4) -> Vec<u8> {
        let mut bytes = vec![0x7f; offset + 64];
        for (index, value) in matrix.into_iter().flatten().enumerate() {
            bytes[offset + index * 4..offset + index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn draw(present_seq: u64, tag: u64, matrix: Mat4) -> DiscoveryDrawSample {
        let _transient_tag_must_not_enter_contract = tag;
        DiscoveryDrawSample {
            present_seq,
            pass_ordinal: 1,
            draw_ordinal: 37,
            bound_buffers: vec![DiscoveryBoundBuffer {
                bind_slot: 5,
                bytes: bytes_at(64, matrix).into(),
            }],
        }
    }

    #[test]
    fn phase_reports_keep_stable_and_changed_fov_evidence_separate() {
        let baseline = projection(1.19, 2.11, 1.0);
        let world_changed = projection(0.7, 1.24, 1.0);
        let viewmodel_changed = projection(1.8, 3.2, 0.01);

        for (phase, first, second, expected_distinct) in [
            ("A1", baseline, baseline, 1),
            ("A2-world", baseline, world_changed, 2),
            ("A3-viewmodel", baseline, viewmodel_changed, 2),
        ] {
            let mut tracker = CameraDiscoveryTracker::new();
            tracker.observe_draw(&draw(1, 10, first));
            tracker.observe_draw(&draw(2, 11, second));
            let report = tracker.report(
                &config(phase),
                DiscoverySamplingStats {
                    submitted_draws: 2,
                    ..DiscoverySamplingStats::default()
                },
            );
            assert_eq!(report.phase_id, phase);
            assert_eq!(report.projection_candidates.len(), 1);
            assert_eq!(
                report.projection_candidates[0].distinct_values,
                expected_distinct
            );
        }
    }

    #[test]
    fn ring_buffer_tag_rotation_maps_to_one_production_source() {
        let mut tracker = CameraDiscoveryTracker::new();
        let matrix = projection(1.19, 2.11, 1.0);
        tracker.observe_draw(&draw(10, 0xaaaa, matrix));
        tracker.observe_draw(&draw(11, 0xbbbb, matrix));
        let report = tracker.report(
            &config("ring"),
            DiscoverySamplingStats {
                submitted_draws: 2,
                ..DiscoverySamplingStats::default()
            },
        );
        let candidate = &report.projection_candidates[0];
        assert_eq!(candidate.frames, 2);
        assert_eq!(candidate.source.bind_slot, 5);
        assert_eq!(candidate.source.buffer_bytes, 128);
        assert_eq!(candidate.source.offset, 64);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("buffer_tag"));
        assert!(!json.contains("aaaa"));
        assert!(!json.contains("bbbb"));
        assert!(!json.contains("observations"));
        assert!(!json.contains("comparison_camera_samples"));
        assert!(!json.contains("comparison_camera_errors"));
        assert!(!json.contains("view_observation_overflows"));
        assert!(serde_json::from_str::<crate::CameraBinding>(&json).is_err());
    }

    #[test]
    fn phase_reset_does_not_carry_the_previous_candidate_or_denominator() {
        let mut a1 = CameraDiscoveryTracker::new();
        a1.observe_draw(&draw(10, 1, projection(1.19, 2.11, 1.0)));
        assert_eq!(
            a1.report(
                &config("A1"),
                DiscoverySamplingStats {
                    submitted_draws: 1,
                    ..DiscoverySamplingStats::default()
                }
            )
            .projection_candidates
            .len(),
            1
        );

        let a2 = CameraDiscoveryTracker::new().report(
            &config("A2"),
            DiscoverySamplingStats {
                submitted_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        assert_eq!(a2.phase_id, "A2");
        assert!(a2.projection_candidates.is_empty());
    }

    #[test]
    fn one_buffer_snapshot_is_scanned_once_and_keeps_every_draw_provenance() {
        let mut tracker = CameraDiscoveryTracker::new();
        tracker.observe_batch(&DiscoveryPresentBatch {
            present_seq: 10,
            logical_draws: 3,
            draw_contexts: Vec::new(),
            bindings: vec![DiscoveryBindingSnapshot {
                pass_ordinal: 2,
                bind_slot: 5,
                draw_ordinals: vec![7, 8, 11],
                bytes: bytes_at(64, projection(1.19, 2.11, 1.0)).into(),
            }],
            comparison_camera_reference: None,
            comparison_camera_error: None,
        });

        let report = tracker.report(
            &config("batched"),
            DiscoverySamplingStats {
                scheduled_draws: 3,
                submitted_draws: 3,
                ..DiscoverySamplingStats::default()
            },
        );
        assert_eq!(tracker.scanned_buffer_snapshots, 1);
        assert_eq!(report.processed_draws, 3);
        assert_eq!(report.projection_candidates.len(), 3);
        assert_eq!(
            report
                .projection_candidates
                .iter()
                .map(|candidate| candidate.source.draw_ordinal)
                .collect::<Vec<_>>(),
            vec![7, 8, 11]
        );
        assert!(
            report
                .projection_candidates
                .iter()
                .all(|candidate| candidate.source.pass_ordinal == 2)
        );
        assert!(report.reliable, "{:?}", report.blockers);
    }

    #[test]
    fn distinct_buffer_versions_are_scanned_independently() {
        let mut tracker = CameraDiscoveryTracker::new();
        tracker.observe_batch(&DiscoveryPresentBatch {
            present_seq: 10,
            logical_draws: 2,
            draw_contexts: Vec::new(),
            bindings: vec![
                DiscoveryBindingSnapshot {
                    pass_ordinal: 0,
                    bind_slot: 5,
                    draw_ordinals: vec![1],
                    bytes: bytes_at(64, projection(1.19, 2.11, 1.0)).into(),
                },
                DiscoveryBindingSnapshot {
                    pass_ordinal: 0,
                    bind_slot: 5,
                    draw_ordinals: vec![2],
                    bytes: bytes_at(64, projection(0.7, 1.24, 1.0)).into(),
                },
            ],
            comparison_camera_reference: None,
            comparison_camera_error: None,
        });

        let report = tracker.report(
            &config("versions"),
            DiscoverySamplingStats {
                scheduled_draws: 2,
                submitted_draws: 2,
                ..DiscoverySamplingStats::default()
            },
        );
        assert_eq!(tracker.scanned_buffer_snapshots, 2);
        assert_eq!(report.projection_candidates.len(), 2);
        assert_eq!(report.processed_draws, 2);
        assert!(report.reliable, "{:?}", report.blockers);
    }

    #[test]
    fn d3d11_1_bound_range_is_a_stable_logical_buffer() {
        let mut tracker = CameraDiscoveryTracker::new();
        tracker.observe_batch(&DiscoveryPresentBatch {
            present_seq: 10,
            logical_draws: 1,
            draw_contexts: Vec::new(),
            bindings: vec![DiscoveryBindingSnapshot {
                pass_ordinal: 1,
                bind_slot: 3,
                draw_ordinals: vec![9],
                bytes: bytes_at(64, projection(1.19, 2.11, 1.0)).into(),
            }],
            comparison_camera_reference: None,
            comparison_camera_error: None,
        });

        let report = tracker.report(
            &config("range"),
            DiscoverySamplingStats {
                scheduled_draws: 1,
                submitted_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        assert_eq!(report.projection_candidates.len(), 1);
        let source = report.projection_candidates[0].source;
        assert_eq!(source.buffer_bytes, 128);
        assert_eq!(source.offset, 64);
        assert!(report.reliable, "{:?}", report.blockers);
    }

    #[test]
    fn queue_saturation_is_fail_closed_instead_of_a_footnote() {
        let report = CameraDiscoveryTracker::new().report(
            &config("saturated"),
            DiscoverySamplingStats {
                submitted_draws: 20,
                queue_dropped_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        assert!(!report.reliable);
        assert!(report.blockers.contains(&DiscoveryBlocker::QueueSaturated));
    }

    #[test]
    fn only_register_aligned_candidates_can_become_binding_evidence() {
        let matrix = projection(1.19, 2.11, 1.0);
        let mut bytes = bytes_at(4, matrix);
        bytes.resize(128, 0x7f);
        let mut tracker = CameraDiscoveryTracker::new();
        tracker.observe_draw(&DiscoveryDrawSample {
            present_seq: 1,
            pass_ordinal: 0,
            draw_ordinal: 0,
            bound_buffers: vec![DiscoveryBoundBuffer {
                bind_slot: 0,
                bytes: bytes.into(),
            }],
        });
        let report = tracker.report(
            &config("aligned"),
            DiscoverySamplingStats {
                submitted_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        assert!(report.projection_candidates.is_empty());
    }

    #[test]
    fn candidate_table_limit_is_bounded_and_fail_closed() {
        let mut tracker = CameraDiscoveryTracker {
            candidate_limit: 1,
            ..CameraDiscoveryTracker::default()
        };
        let mut bytes = bytes_at(0, projection(1.19, 2.11, 1.0));
        bytes.extend(bytes_at(0, projection(0.7, 1.24, 1.0)));
        tracker.observe_draw(&DiscoveryDrawSample {
            present_seq: 1,
            pass_ordinal: 0,
            draw_ordinal: 0,
            bound_buffers: vec![DiscoveryBoundBuffer {
                bind_slot: 0,
                bytes: bytes.into(),
            }],
        });
        let report = tracker.report(
            &config("bounded"),
            DiscoverySamplingStats {
                scheduled_draws: 1,
                submitted_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        assert_eq!(report.projection_candidates.len(), 1);
        assert!(report.candidate_overflows > 0);
        assert!(report.blockers.contains(&DiscoveryBlocker::CandidateLimit));
        assert!(!report.reliable);
    }

    #[test]
    fn v3_keeps_exact_present_tracks_and_camera_to_world_reference_samples() {
        let config = evidence_config("POSE_STATIC");
        let mut tracker = CameraDiscoveryTracker::for_config(&config);
        for present_seq in 1..=8 {
            tracker.observe_batch(&DiscoveryPresentBatch {
                present_seq,
                logical_draws: 1,
                draw_contexts: vec![DiscoveryDrawContext {
                    present_seq,
                    pass_ordinal: 0,
                    draw_ordinal: 7,
                    viewport: None,
                }],
                bindings: vec![DiscoveryBindingSnapshot {
                    pass_ordinal: 0,
                    bind_slot: 5,
                    draw_ordinals: vec![7],
                    bytes: bytes_at(
                        64,
                        [
                            [1.0, 0.0, 0.0, 0.0],
                            [0.0, 1.0, 0.0, 0.0],
                            [0.0, 0.0, 1.0, 0.0],
                            [present_seq as f32, 0.0, 0.0, 1.0],
                        ],
                    )
                    .into(),
                }],
                comparison_camera_reference: Some(DiscoveryReferenceCameraObservation {
                    present_seq,
                    present_qpc: present_seq as i64 * 10,
                    camera_position: [present_seq as f64, 0.0, 0.0],
                    camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
                }),
                comparison_camera_error: None,
            });
        }
        let report = tracker.report(
            &config,
            DiscoverySamplingStats {
                scheduled_draws: 8,
                submitted_draws: 8,
                ..DiscoverySamplingStats::default()
            },
        );
        assert_eq!(report.schema, CAMERA_DISCOVERY_REPORT_SCHEMA_V4);
        assert!(report.reliable, "{:?}", report.blockers);
        assert_eq!(report.comparison_camera_samples.len(), 8);
        let exact_4x4 = report
            .view_candidates
            .iter()
            .find(|candidate| {
                candidate.source.source.offset == 64 && !candidate.source.source.transpose
            })
            .expect("exact 4x4 view candidate");
        assert_eq!(exact_4x4.observations.len(), 8);
        assert_eq!(exact_4x4.observations[7].present_seq, 8);
    }

    #[test]
    fn v3_missing_reference_is_a_durable_blocker() {
        let config = evidence_config("POSE_WALK");
        let mut tracker = CameraDiscoveryTracker::for_config(&config);
        tracker.observe_batch(&DiscoveryPresentBatch {
            present_seq: 1,
            logical_draws: 1,
            draw_contexts: vec![DiscoveryDrawContext {
                present_seq: 1,
                pass_ordinal: 0,
                draw_ordinal: 7,
                viewport: None,
            }],
            bindings: vec![DiscoveryBindingSnapshot {
                pass_ordinal: 0,
                bind_slot: 5,
                draw_ordinals: vec![7],
                bytes: bytes_at(
                    64,
                    [
                        [1.0, 0.0, 0.0, 0.0],
                        [0.0, 1.0, 0.0, 0.0],
                        [0.0, 0.0, 1.0, 0.0],
                        [0.0, 0.0, 0.0, 1.0],
                    ],
                )
                .into(),
            }],
            comparison_camera_reference: None,
            comparison_camera_error: None,
        });
        let report = tracker.report(
            &config,
            DiscoverySamplingStats {
                scheduled_draws: 1,
                submitted_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        assert!(!report.reliable);
        assert!(
            report
                .blockers
                .contains(&DiscoveryBlocker::ComparisonCameraMissing)
        );
    }

    #[test]
    fn progress_sidecar_is_small_and_carries_every_fail_closed_counter() {
        let config = evidence_config("POSE_STATIC_W000");
        let mut tracker = CameraDiscoveryTracker::for_config(&config);
        tracker.observe_batch(&DiscoveryPresentBatch {
            present_seq: 1,
            logical_draws: 1,
            draw_contexts: vec![DiscoveryDrawContext {
                present_seq: 1,
                pass_ordinal: 0,
                draw_ordinal: 7,
                viewport: None,
            }],
            bindings: Vec::new(),
            comparison_camera_reference: Some(DiscoveryReferenceCameraObservation {
                present_seq: 1,
                present_qpc: 10,
                camera_position: [0.0; 3],
                camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            }),
            comparison_camera_error: None,
        });
        let report = tracker.report(
            &config,
            DiscoverySamplingStats {
                scheduled_draws: 1,
                submitted_draws: 1,
                ..DiscoverySamplingStats::default()
            },
        );
        let progress = CameraDiscoveryProgress::from(&report);
        assert_eq!(progress.schema, CAMERA_DISCOVERY_PROGRESS_SCHEMA_V1);
        assert_eq!(progress.phase_id, "POSE_STATIC_W000");
        assert_eq!(progress.comparison_camera_samples, 1);
        assert_eq!(progress.candidate_overflows, report.candidate_overflows);
        assert_eq!(
            progress.view_observation_overflows,
            report.view_observation_overflows
        );
        assert_eq!(progress.blockers, report.blockers);
        assert!(serde_json::to_vec(&progress).unwrap().len() < 1_024);
    }

    #[test]
    fn one_partial_phase_boundary_is_accounted_without_becoming_fake_evidence() {
        let config = evidence_config("POSE_STATIC_W000");
        let mut tracker = CameraDiscoveryTracker::for_config(&config);
        tracker.observe_batch(&DiscoveryPresentBatch {
            present_seq: 1,
            logical_draws: 1,
            draw_contexts: vec![DiscoveryDrawContext {
                present_seq: 1,
                pass_ordinal: 0,
                draw_ordinal: 7,
                viewport: None,
            }],
            bindings: Vec::new(),
            comparison_camera_reference: Some(DiscoveryReferenceCameraObservation {
                present_seq: 1,
                present_qpc: 10,
                camera_position: [0.0; 3],
                camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            }),
            comparison_camera_error: None,
        });
        let sampling = DiscoverySamplingStats {
            scheduled_presents: 2,
            scheduled_draws: 2,
            submitted_draws: 1,
            phase_boundary_discarded_presents: 1,
            phase_boundary_discarded_draws: 1,
            ..DiscoverySamplingStats::default()
        };
        let report = tracker.report(&config, sampling);
        assert!(report.reliable, "{:?}", report.blockers);
        assert_eq!(report.comparison_camera_samples.len(), 1);

        let report = tracker.report(
            &config,
            DiscoverySamplingStats {
                phase_boundary_discarded_presents: 2,
                ..sampling
            },
        );
        assert!(!report.reliable);
        assert!(
            report
                .blockers
                .contains(&DiscoveryBlocker::DrawAccountingMismatch)
        );
    }
}
