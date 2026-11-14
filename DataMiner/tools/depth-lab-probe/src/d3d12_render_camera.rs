//! Bounded D3D12 render-camera observation for the exact selected-depth pass.
//!
//! This module deliberately does not author a production binding. It accepts only bytes from a
//! direct root CBV or an exactly tracked descriptor-table-base CBV whose owning resource was
//! directly and successfully mapped by the game, and only for draws that had the already-armed
//! exact DSV bound. The resulting candidates remain discovery evidence until cross-launch
//! stability, input correlation, metric scale and E-geo pass.

pub mod depth_order;
pub mod draw_pass;
#[cfg(test)]
mod gpuva_lookup_tests;
pub(crate) mod local_admission;
#[cfg(test)]
mod local_admission_tests;
pub(crate) mod observation_archive;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

mod cbv_descriptors;
#[cfg(test)]
mod copy_versions_tests;
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
pub(crate) use cbv_descriptors::lab_lock::LockKind as LabDescriptorLock;
#[cfg(all(feature = "local-perf-lab-lock-trace", debug_assertions))]
pub(crate) use cbv_descriptors::lab_trace as descriptor_lab_trace;
pub(crate) use cbv_descriptors::{CbvDescriptorStore, CopyTiming, PreparedCopy};

use depth_lab::camera::{
    Mat4, MatrixKind, TranslationSlot, intrinsics_from_projection, scan_candidates,
    scan_view_projection_pairs,
};
use depth_lab::camera_track::projection_from_matrix;
use depth_lab::d3d12_camera_alias::{CopyCommandIdentity, CopyReceipt};
use depth_lab::{
    D3d12DescriptorTableBinding, D3d12DrawRelation, D3d12MatrixSourceBinding, MatrixEncoding,
    MatrixLayout, MatrixRole,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::d3d12_abi::{DsvIdentity, QueueId};

pub const ROOT_CBV_SNAPSHOT_BYTES: usize = 256;
const MAX_ROOT_CBV_BINDINGS_PER_LIST: usize = 32;
const MAX_ROOT_CBV_SOURCES_PER_DRAW: usize = 64;
// Sparse discovery is not proof that every draw has the same camera. Keep at most two selected
// samples per list generation; the hook's matching outer budget must have the SAME ownership,
// never a CPU Present-wide quota shared by independently recorded/submitted lists. Local exact
// Present admission still fails on missing/conflicting sources. Production/KCD2 use their
// separately bounded exact-binding paths. An unexpected inner-budget hit remains a loss.
pub(crate) const MAX_SELECTED_DRAWS_SCANNED_PER_LIST_GENERATION: u32 = 2;
const EARLY_SELECTED_DRAWS_SCANNED_PER_LIST_GENERATION: u32 = 2;
const LATE_SELECTED_DRAW_SAMPLE_STRIDE: u32 = u32::MAX;
const MAX_SELECTED_DRAWS_RESOLVED_PER_PRESENT: u32 = 1;
const MAX_MATRIX_CANDIDATES_PER_DRAW: usize = 32;
const MAX_PENDING_DRAWS: usize = 4096;
pub(crate) const MAX_PRESENT_OBSERVATIONS: usize = 4096;
const MIN_ADMITTED_PRESENTS_PER_MATRIX_SOURCE: usize = 120;
const MAX_DESCRIPTOR_COPY_HANDLES: usize = 4096;
const MAX_UPLOAD_UNMAP_SNAPSHOTS_PER_RESOURCE: usize = 256;
const MAX_UPLOAD_UNMAP_SNAPSHOT_BYTES_TOTAL: usize = 8 * 1024 * 1024;
const MAX_EXACT_CBV_COPY_REQUESTS_PER_LIST: usize = 64;
const MAX_COPY_BUFFER_SPANS_PER_LIST: usize = 4096;
const MAX_EXACT_CBV_COPY_SHADOW_BYTES_TOTAL: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES: usize = 4;

fn should_scan_selected_draw(draw_ordinal: u32, already_scanned: u32) -> bool {
    already_scanned < EARLY_SELECTED_DRAWS_SCANNED_PER_LIST_GENERATION
        || (already_scanned < MAX_SELECTED_DRAWS_SCANNED_PER_LIST_GENERATION
            && draw_ordinal.is_multiple_of(LATE_SELECTED_DRAW_SAMPLE_STRIDE))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RootBindingSource {
    Direct,
    DescriptorTableBase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservedRootBinding {
    pub root_parameter_index: u32,
    pub source: RootBindingSource,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipelineDepthState {
    pub enable: bool,
    /// Enable && WriteMask=ALL && DSVFormat!=UNKNOWN; observed, not admission.
    pub write: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedDrawBindings {
    /// Process-local pointer; never serialized into camera evidence.
    pub pipeline_state: usize,
    pub pass_context: Option<draw_pass::DrawPassContext>,
    pub pipeline_depth: Option<PipelineDepthState>,
    pub list_generation: u64,
    pub draw_ordinal: u32,
    pub pass_ordinal: u32,
    pub draw_ordinal_in_pass: u32,
    pub descriptor_heaps: [usize; 2],
    pub root_signature: usize,
    pub roots: Vec<ObservedRootBinding>,
}

/// A Draw can be represented by an earlier Draw only when every authored camera source resolves
/// to the same exact CBV descriptor generation and to a CopyBufferRegion command recorded on this
/// command-list generation. The backend combines this key with its lock-free command-list state
/// revision; either side changing forces the next Draw through the full tracker again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProductionDrawDedupKey {
    pub(crate) descriptor_identities: [(u64, u64); MAX_PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES],
    pub(crate) descriptor_count: u8,
    pub(crate) root_bindings: [(u32, u64); MAX_PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES],
    pub(crate) root_binding_count: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SelectedDrawObservation {
    /// True only when this call actually appended work consumed by ExecuteCommandLists.
    pub(crate) staged: bool,
    /// Present only for the conservative CP2077 exact-copy path described above.
    pub(crate) production_dedup_key: Option<ProductionDrawDedupKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootCbvRange {
    pub table_offset: u32,
    pub descriptor_count: u32,
    pub descriptor_range_index: u32,
    pub base_shader_register: u32,
    pub register_space: u32,
}

#[derive(Clone, Debug)]
struct RootSignatureObservation {
    sha256: Option<String>,
    cbv_ranges: BTreeMap<u32, Vec<RootCbvRange>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BufferBinding {
    pub source_tag: u64,
    pub resource_raw: usize,
    pub resource_offset: u64,
    pub root_parameter_index: u32,
    pub source: RootBindingSource,
    pub declared_bytes: Option<u32>,
    pub stable_descriptor: Option<D3d12DescriptorTableBinding>,
    pub descriptor_identity: Option<(u64, u64)>,
    content_source: BufferContentSource,
    content_generation: u64,
    copy_shadow_queue: Option<QueueId>,
    // Only the deferred-copy path needs this identity; keep ordinary bindings compact.
    pending_copy_request: Option<Arc<ExactCbvCopyRequest>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BufferContentSource {
    LiveMapped,
    UploadUnmapSnapshot,
    CopyBufferRegionShadow,
}

#[derive(Clone, Debug)]
struct BufferResource {
    source_tag: u64,
    gpu_virtual_address: u64,
    buffer_bytes: u64,
    mapped_cpu_address: Option<usize>,
    observer_mapped_cpu_address: Option<usize>,
    map_count: u32,
    upload_heap: bool,
    content_generation: u64,
    unmap_snapshots: BTreeMap<u64, UploadUnmapSnapshot>,
}

impl BufferResource {
    fn cpu_address(&self) -> Option<usize> {
        self.mapped_cpu_address.or(self.observer_mapped_cpu_address)
    }
}

/// Identity only, not a lifetime lease. Production obtains this only for the observer's
/// retained COM reference + nested Map. Queries may run outside the tracker guard within ONE
/// Execute, but reads must reacquire it, obtain a fresh identity, and validate each request.
/// Never retain query evidence across Execute. Borrowed application Maps are ineligible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObserverMappedIdentity {
    pub resource_raw: usize,
    pub source_tag: u64,
    pub content_generation: u64,
    pub base: usize,
    pub bytes: u64,
}

/// An optional page-query hint, NOT a validated copy request or permission to read bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObserverMappedQuery {
    pub owner: ObserverMappedIdentity,
    pub address: usize,
    pub bytes: usize,
}

#[derive(Clone, Debug)]
struct UploadUnmapSnapshot {
    content_generation: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ExactCbvCopyRequest {
    list_raw: usize,
    list_tag: u64,
    list_generation: u64,
    copy_ordinal: usize,
    destination_resource_raw: usize,
    destination_source_tag: u64,
    destination_content_generation: u64,
    destination_offset: u64,
    source_resource_raw: usize,
    source_source_tag: u64,
    source_content_generation: u64,
    source_offset: u64,
    descriptor_identity: (u64, u64),
    bytes: usize,
}

/// Physical command and sampled range, independent of which descriptor observes it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PhysicalCbvCopyKey {
    command: CopyCommandIdentity,
    destination_source_tag: u64,
    destination_content_generation: u64,
    destination_offset: u64,
    source_source_tag: u64,
    source_content_generation: u64,
    source_offset: u64,
    bytes: usize,
}

impl ExactCbvCopyRequest {
    fn physical_key(&self) -> PhysicalCbvCopyKey {
        PhysicalCbvCopyKey {
            command: CopyCommandIdentity {
                command_list_tag: self.list_tag,
                command_list_generation: self.list_generation,
                copy_ordinal: self.copy_ordinal as u64 + 1,
            },
            destination_source_tag: self.destination_source_tag,
            destination_content_generation: self.destination_content_generation,
            destination_offset: self.destination_offset,
            source_source_tag: self.source_source_tag,
            source_content_generation: self.source_content_generation,
            source_offset: self.source_offset,
            bytes: self.bytes,
        }
    }
}

#[derive(Clone, Debug)]
struct CopyBufferSpan {
    /// Unsupported/partial writes must shadow earlier copies, never expose stale bytes.
    readable: bool,
    list_generation: u64,
    destination_resource_raw: usize,
    destination_source_tag: u64,
    destination_content_generation: u64,
    destination_offset: u64,
    source_resource_raw: usize,
    source_source_tag: u64,
    source_content_generation: u64,
    source_offset: u64,
    bytes: u64,
}

#[derive(Clone, Debug)]
pub struct PreparedCbvCopyShadow {
    request: ExactCbvCopyRequest,
    bytes: Vec<u8>,
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(crate) diagnostic_snapshot:
        Option<crate::cp2077_camera_diagnostics::early_player::SnapshotTiming>,
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(crate) diagnostic_player:
        Option<crate::cp2077_camera_diagnostics::early_player::SampleToken>,
}

#[derive(Clone, Debug)]
struct ExecutedCbvCopyShadow {
    receipt: CopyReceipt,
    source_resource_raw: usize,
    source_content_generation: u64,
    destination_content_generation: u64,
    descriptor_identity: (u64, u64),
    queue: QueueId,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct DescriptorHeap {
    cpu_start: u64,
    gpu_start: u64,
    descriptor_count: u32,
    increment: u32,
    shader_visible: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CbvDescriptor {
    gpu_virtual_address: u64,
    size_in_bytes: u32,
    generation: u64,
}

#[derive(Default)]
struct DescriptorTableResolution {
    bindings: Vec<BufferBinding>,
    root_parameter_without_declared_cbv_ranges: u64,
    no_bound_descriptor_heap: u64,
    bound_descriptor_heap_untracked: u64,
    table_base_outside_bound_heap: u64,
    table_base_misaligned: u64,
    table_base_ambiguous_bound_heap: u64,
    declared_descriptor_outside_heap: u64,
    descriptor_not_current_cbv: u64,
    cbv_gpuva_without_resource: u64,
    cbv_gpuva_resource_unmapped: u64,
    cbv_gpuva_upload_snapshot_missing: u64,
    cbv_gpuva_resource_ambiguous: u64,
    cbv_size_outside_resource: u64,
    resolved_descriptors: u64,
    resolved_from_upload_unmap_snapshot: u64,
    resolved_from_exact_cbv_copy_shadow: u64,
}

#[derive(Debug, PartialEq)]
enum DescriptorGpuVaResolution {
    Unique(BufferBinding),
    WithoutResource,
    ResourceUnmapped {
        raw: usize,
        offset: u64,
        content_generation: u64,
    },
    UploadSnapshotMissing,
    ResourceAmbiguous,
}

#[derive(Clone, Debug)]
struct MatrixCandidate {
    copy_receipt: Option<CopyReceipt>,
    exact_copy_request: Option<ExactCbvCopyRequest>,
    source_tag: u64,
    root_parameter_index: u32,
    cbv_resource_offset: u64,
    matrix_offset: u32,
    transpose: bool,
    matrix: Mat4,
    kind: MatrixKind,
    source: RootBindingSource,
    content_source: BufferContentSource,
    resource_raw: usize,
    resource_content_generation: u64,
    copy_shadow_queue: Option<QueueId>,
    descriptor_identity: Option<(u64, u64)>,
    declared_bytes: Option<u32>,
    stable_descriptor: Option<D3d12DescriptorTableBinding>,
}

#[derive(Clone, Debug, Default)]
struct ListState {
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    diagnostic_draw_weights: BTreeMap<u32, Arc<crate::cp2077_camera_diagnostics::DrawWeight>>,
    depth_boundaries: depth_order::ListBoundaries,
    list_tag: u64,
    list_generation: u64,
    bound_depth: Option<DsvIdentity>,
    root_cbvs: BTreeMap<u32, u64>,
    draw_ordinal: u32,
    selected_draws_scanned: u32,
    pending: Vec<PendingDraw>,
    exact_cbv_copy_requests: Vec<ExactCbvCopyRequest>,
    copy_buffer_spans: Vec<CopyBufferSpan>,
    copy_span_overflow: bool,
    deferred_copy_draws: Vec<DeferredCopyDraw>,
    viewport: Option<CapturedViewport>,
    scissor: Option<draw_pass::ScissorObservation>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturedViewport {
    pub count: u32,
    pub first: windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT,
}

#[derive(Debug)]
pub(crate) struct ProductionCameraSample {
    pub row: depth_lab::CameraPoseRow,
    pub depth_copied: bool,
    /// Frozen at the admitted draw, never reconstructed from the profile or a later Present.
    pub viewport: Option<draw_pass::ViewportObservation>,
}

impl CapturedViewport {
    pub fn validate(&self, width: u32, height: u32) -> Result<(), String> {
        let v = self.first;
        if self.count != 1
            || v.TopLeftX != 0.0
            || v.TopLeftY != 0.0
            || v.Width != width as f32
            || v.Height != height as f32
        {
            return Err(format!(
                "KCD2 exact draw viewport is not one full {width}x{height} viewport: {self:?}"
            ));
        }
        if !v.MinDepth.is_finite()
            || !v.MaxDepth.is_finite()
            || v.MinDepth < 0.0
            || v.MaxDepth > 1.0
            || v.MinDepth >= v.MaxDepth
        {
            return Err(format!("KCD2 viewport depth range is invalid: {self:?}"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct DeferredCopyDraw {
    pass_context: Option<draw_pass::DrawPassContext>,
    pipeline_depth: Option<PipelineDepthState>,
    draw_ordinal: u32,
    pass_ordinal: u32,
    draw_ordinal_in_pass: u32,
    root_signature_sha256: Option<String>,
    selected_depth: DsvIdentity,
    bindings: Vec<BufferBinding>,
}

#[derive(Clone, Debug)]
struct PendingDraw {
    pass_context: Option<draw_pass::DrawPassContext>,
    pipeline_depth: Option<PipelineDepthState>,
    list_tag: u64,
    list_generation: u64,
    draw_ordinal: u32,
    /// Render-pass context, when the path that recorded this draw knows it.
    /// `None` on the direct root-CBV path, which carries no pass grouping; the
    /// 2026-09-10 CP2077 run resolved every camera through descriptor tables
    /// (`root_cbv_calls` 0 against 16,920,678 descriptor-table calls), so that
    /// path is the one this provenance has to describe.
    pass_ordinal: Option<u32>,
    draw_ordinal_in_pass: Option<u32>,
    root_signature_sha256: Option<String>,
    selected_depth: DsvIdentity,
    matrices: Vec<MatrixCandidate>,
    kcd2_snapshots: Vec<Kcd2DirectRootSnapshot>,
    kcd2_pre_execute_sources: Vec<Kcd2PreExecuteSource>,
    kcd2_pre_execute_prepared: bool,
    kcd2_pre_execute_queue: Option<QueueId>,
}

#[derive(Clone, Debug)]
struct Kcd2PreExecuteSource {
    binding: BufferBinding,
    gpu_base: u64,
    buffer_bytes: u64,
    root_signature_sha256: String,
    pass_ordinal: u32,
    draw_ordinal: u32,
    viewport: Option<CapturedViewport>,
}

#[derive(Clone, Debug)]
struct Kcd2DirectRootSnapshot {
    sample_qpc: i64,
    root_signature_sha256: String,
    root_parameter_index: u32,
    pass_ordinal: u32,
    draw_ordinal: u32,
    bytes: Vec<u8>,
    viewport: Option<CapturedViewport>,
}

#[derive(Clone, Copy, Debug)]
struct DrawLocation<'a> {
    pass_context: Option<draw_pass::DrawPassContext>,
    pipeline_depth: Option<PipelineDepthState>,
    global_ordinal: u32,
    pass_ordinal: u32,
    ordinal_in_pass: u32,
    root_signature_sha256: Option<&'a str>,
}

#[derive(Clone, Debug)]
struct SubmittedDraw {
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    diagnostic_draw_weight: Option<u64>,
    queue: QueueId,
    /// Which `ExecuteCommandLists` call submitted this draw's list, and where
    /// the list sat inside that call. `draw_ordinal` and `pass_ordinal` are
    /// both list-local, so two draws from different command lists cannot be
    /// ordered without this: the 2026-09-11 run found 57 Presents carrying one
    /// draw from each of two command-list families and could not say which one
    /// the captured depth belonged to.
    execute_batch: u64,
    list_index_in_batch: u32,
    draw: PendingDraw,
}

/// Where the injected depth copy for one Present was submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DepthCopySubmission {
    execute_batch: u64,
    list_index_in_batch: Option<u32>,
    command_list_tag: Option<u64>,
    draws_before_in_list: Option<u32>,
}

impl DepthCopySubmission {
    /// Whether a draw submitted at `execute_batch` / `list_index_in_batch`, with
    /// the list-local `draw_ordinal`, ran before this copy. Batches order
    /// Execute calls, list positions order one call, and the copy's recording
    /// stamp orders its own list; that stamp exists only on the hot-list
    /// counter, whose draws carry a pass. `None` means the recorded fields
    /// cannot order the pair.
    fn precedes(
        &self,
        execute_batch: u64,
        list_index_in_batch: u32,
        draw_ordinal: u32,
        pass_ordinal: Option<u32>,
    ) -> Option<bool> {
        if execute_batch != self.execute_batch {
            return Some(execute_batch < self.execute_batch);
        }
        let copy_list_index = self.list_index_in_batch?;
        if list_index_in_batch != copy_list_index {
            return Some(list_index_in_batch < copy_list_index);
        }
        let draws_before = pass_ordinal.and(self.draws_before_in_list)?;
        Some(draw_ordinal <= draws_before)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct D3d12RenderMatrixObservation {
    pub copy_receipt: Option<CopyReceipt>,
    pub source_tag: u64,
    pub root_parameter_index: u32,
    pub cbv_resource_offset: u64,
    pub matrix_offset: u32,
    pub transpose: bool,
    pub matrix_kind: String,
    pub matrix: Mat4,
    pub translation_slot: Option<String>,
    pub basis_determinant: Option<f64>,
    pub clip_w_sign: Option<i8>,
    pub projection_a: Option<f64>,
    pub projection_b: Option<f64>,
    pub vertical_fov_degrees: Option<f64>,
    pub horizontal_fov_degrees: Option<f64>,
    pub configured_vertical_fov_matches: Option<bool>,
    pub handedness_observation: String,
    pub axis_mapping: String,
    pub physical_source: String,
    /// Fully bounded cross-launch source identity. `None` means this observation is useful only
    /// for discovery and must never be authored into CameraBinding v2.
    pub stable_source: Option<D3d12MatrixSourceBinding>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct D3d12RenderDrawObservation {
    /// Optional diagnostic snapshot, frozen at the exact observed draw.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, serde(skip_deserializing))]
    pub pass_context: Option<draw_pass::DrawPassContext>,
    /// Bound PSO depth state at this draw. None = unregistered/unknown PSO or
    /// legacy observe_draw path. Local holstered v2 requires corroborating draw-pass state.
    pub pipeline_depth_enable: Option<bool>,
    /// True only for DepthEnable && WriteMask=ALL && DSVFormat!=UNKNOWN.
    /// This records PSO configuration, not proof that any sample passed depth.
    pub pipeline_depth_write: Option<bool>,
    pub present_seq: u64,
    pub present_qpc: i64,
    pub queue_tag: u64,
    pub command_list_tag: u64,
    pub command_list_generation: u64,
    pub selected_depth_resource_tag: u64,
    pub selected_depth_resource_epoch: u64,
    pub selected_depth_descriptor_generation: u64,
    pub draw_ordinal: u32,
    /// Which render pass recorded this draw, and where inside it. Two draws that
    /// write the same camera slot in one Present are only contradictory if they
    /// belong to the same view; a differing `pass_ordinal` or root signature says
    /// they do not. `None` on the direct root-CBV path, which has no pass
    /// grouping. Added after the 2026-09-10 run reported six same-Present matrix
    /// conflicts whose cause could not be settled from the recorded fields.
    pub pass_ordinal: Option<u32>,
    pub draw_ordinal_in_pass: Option<u32>,
    pub root_signature_sha256: Option<String>,
    /// Cross-list submission order. `draw_ordinal` and `pass_ordinal` are both
    /// list-local, so these are the only fields that can order two draws from
    /// different command lists: compare `list_index_in_batch` within one batch,
    /// `execute_batch` across batches. `depth_copy_execute_batch` is the batch
    /// that carried this Present's depth copy, so a draw is before or after the
    /// copy that produced the captured frame. Added after the 2026-09-11 run
    /// found 57 Presents with one draw from each of two command-list families -
    /// far planes about 3,995 and 16,777 - and no way to say which the captured
    /// depth used.
    pub execute_batch: u64,
    pub list_index_in_batch: u32,
    pub depth_copy_execute_batch: Option<u64>,
    /// Position of the list that carried the depth copy inside that batch, and
    /// its tag. A draw in the copy's batch can be ordered by list position;
    /// a draw in the copy's own list can use `depth_copy_draws_before_in_list`
    /// when that stamp is available. The 2026-09-13 run had 71 of 629 draws in
    /// the copy's batch with no order, and 3 recorded after the copy batch that
    /// were still admitted. Admission now follows this order; see
    /// `copy_before_clear_submitted`.
    pub depth_copy_list_index_in_batch: Option<u32>,
    pub depth_copy_command_list_tag: Option<u64>,
    /// HotListSlot draw count N at copy recording time. Only when this draw and
    /// the copy have the same `execute_batch` and `list_index_in_batch`,
    /// `draw_ordinal <= N` means recorded before the copy and `> N` after it.
    /// This uses the same recording-local counter as `SelectedDrawBindings`
    /// (descriptor-table and direct root-CBV bindings). `None` for the legacy
    /// tracker-counted `observe_draw` path, identified by absent pass provenance,
    /// or if the copy's hot-list entry/count was unavailable or saturated.
    pub depth_copy_draws_before_in_list: Option<u32>,
    /// Both flags admit this draw's camera for its Present's captured depth.
    /// They are decided per draw, not per Present: the Present needs a submitted
    /// depth copy and this draw must provably precede it by the order above. A
    /// draw after the copy renders the next frame. The fourth 2026-09-13 run
    /// still admitted 7 such draws, each five Execute batches after its copy,
    /// while these flags were set for the whole Present.
    pub copy_before_clear_submitted: bool,
    pub selected_present_identity_passed: bool,
    pub physical_source: String,
    pub matrices: Vec<D3d12RenderMatrixObservation>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct D3d12RenderCameraSnapshot {
    pub depth_order: depth_order::DepthOrderSnapshot,
    pub hook_cpu_timing_scope: String,
    pub hook_timing_measurement: String,
    pub hook_cpu_timing: BTreeMap<String, crate::hook_timing::TimingSnapshot>,
    pub original_target_cache_entries: usize,
    pub selected_depth_armed: bool,
    pub tracked_buffer_resources: u32,
    pub gpuva_ownership_cache_hits: u64,
    pub gpuva_ownership_cache_misses: u64,
    pub gpuva_ownership_resources_scanned: u64,
    pub gpuva_ownership_cache_invalidations: u64,
    pub gpuva_ownership_cache_evictions: u64,
    pub directly_mapped_buffer_resources: u32,
    pub tracked_upload_buffer_resources: u32,
    pub unmapped_upload_buffer_resources: u32,
    pub root_cbv_calls: u64,
    pub selected_direct_root_bindings: u64,
    pub selected_direct_root_unresolved: u64,
    pub kcd2_direct_snapshots: u64,
    pub direct_root_resolution_samples: Vec<DirectRootResolutionSample>,
    pub descriptor_table_calls: u64,
    pub tracked_cbv_descriptors: u32,
    pub selected_descriptor_table_draws: u64,
    pub tracked_root_signatures: u32,
    pub tracked_root_signature_cbv_ranges: u32,
    pub selected_depth_draw_calls: u64,
    pub selected_draws_without_root_signature: u64,
    pub selected_draws_with_unknown_root_signature: u64,
    pub selected_draws_with_root_signature_without_cbv_ranges: u64,
    pub selected_draws_with_declared_cbv_ranges: u64,
    pub selected_draws_without_descriptor_table_bindings: u64,
    pub selected_table_root_parameter_without_declared_cbv_ranges: u64,
    pub selected_table_no_bound_descriptor_heap: u64,
    pub selected_table_bound_descriptor_heap_untracked: u64,
    pub selected_table_base_outside_bound_heap: u64,
    pub selected_table_base_misaligned: u64,
    pub selected_table_base_ambiguous_bound_heap: u64,
    pub selected_table_declared_descriptor_outside_heap: u64,
    pub selected_table_descriptor_not_current_cbv: u64,
    pub selected_table_cbv_gpuva_without_resource: u64,
    pub selected_table_cbv_gpuva_resource_unmapped: u64,
    pub selected_table_cbv_gpuva_upload_snapshot_missing: u64,
    pub selected_table_cbv_gpuva_resource_ambiguous: u64,
    pub selected_table_cbv_size_outside_resource: u64,
    pub selected_table_resolved_descriptors: u64,
    pub selected_table_resolved_from_upload_unmap_snapshot: u64,
    pub selected_table_matrix_candidates_invalidated_by_generation: u64,
    pub upload_unmap_snapshot_attempts: u64,
    pub upload_unmap_snapshot_ranges_captured: u64,
    pub upload_unmap_snapshot_read_rejections: u64,
    pub upload_unmap_snapshot_budget_exhausted: u64,
    pub upload_unmap_snapshot_invalid_written_ranges: u64,
    pub upload_unmap_snapshot_live_ranges: u32,
    pub upload_unmap_snapshot_live_bytes: u64,
    pub exact_cbv_copy_calls: u64,
    pub exact_cbv_copy_requests: u64,
    pub exact_cbv_copy_spans_recorded: u64,
    pub exact_cbv_copy_span_budget_exhausted: u64,
    pub exact_cbv_copy_unknown_destination_resource: u64,
    pub exact_cbv_copy_unknown_source_resource: u64,
    pub exact_cbv_copy_range_rejections: u64,
    pub exact_cbv_copy_request_budget_exhausted: u64,
    pub exact_cbv_copy_source_not_upload_or_mapped: u64,
    pub exact_cbv_copy_prepare_read_rejections: u64,
    pub exact_cbv_copy_shadows_committed: u64,
    pub exact_cbv_copy_shadow_budget_exhausted: u64,
    pub exact_cbv_copy_shadow_generation_rejections: u64,
    pub exact_cbv_copy_shadow_queue_rejections: u64,
    pub exact_cbv_copy_shadow_live_ranges: u32,
    pub exact_cbv_copy_shadow_live_bytes: u64,
    pub selected_table_resolved_from_exact_cbv_copy_shadow: u64,
    pub direct_mapped_draws_with_matrices: u64,
    pub unmapped_root_cbv_rejections: u64,
    pub matrix_candidate_overflow: u64,
    pub pending_draw_overflow: u64,
    pub draw_scan_budget_exhausted: u64,
    pub present_observation_overflow: u64,
    pub tracker_contention_drops: u64,
    pub hot_state_drops: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_archive: Option<observation_archive::ArchiveState>,
    pub observations: Vec<D3d12RenderDrawObservation>,
    pub blockers: Vec<String>,
    pub authoring_blockers: Vec<String>,
    pub candidate_admitted_for_cross_launch_discovery: bool,
    pub production_camera_binding_emitted: bool,
    pub recording_ready: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DirectRootResolutionSample {
    root_parameter: u32,
    gpu_va: u64,
    matching_resources: usize,
    outcome: &'static str,
    first_resource: Option<DirectRootResourceSample>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct DirectRootResourceSample {
    raw: usize,
    gpu_base: u64,
    buffer_bytes: u64,
    upload_heap: bool,
    mapped: bool,
    map_count: u32,
}

impl D3d12RenderCameraSnapshot {
    fn authoring_blockers(&self) -> Vec<String> {
        let mut blockers = self.blockers.clone();
        if self.local_archive.is_some() {
            blockers.push(
                "local archived camera evidence does not author a cross-launch binding".into(),
            );
        }
        let lossy_counters = [
            ("matrix_candidate_overflow", self.matrix_candidate_overflow),
            ("pending_draw_overflow", self.pending_draw_overflow),
            (
                "draw_scan_budget_exhausted",
                self.draw_scan_budget_exhausted,
            ),
            (
                "present_observation_overflow",
                self.present_observation_overflow,
            ),
            ("tracker_contention_drops", self.tracker_contention_drops),
            ("hot_state_drops", self.hot_state_drops),
            (
                "upload_unmap_snapshot_budget_exhausted",
                self.upload_unmap_snapshot_budget_exhausted,
            ),
            (
                "exact_cbv_copy_span_budget_exhausted",
                self.exact_cbv_copy_span_budget_exhausted,
            ),
            (
                "exact_cbv_copy_request_budget_exhausted",
                self.exact_cbv_copy_request_budget_exhausted,
            ),
            (
                "exact_cbv_copy_shadow_budget_exhausted",
                self.exact_cbv_copy_shadow_budget_exhausted,
            ),
        ];
        for (name, value) in lossy_counters {
            if value > 0 {
                blockers.push(format!(
                    "D3D12 render-camera authoring observed {name}={value}"
                ));
            }
        }

        let mut view_sources = BTreeMap::<&D3d12MatrixSourceBinding, BTreeSet<u64>>::new();
        let mut projection_sources = BTreeMap::<&D3d12MatrixSourceBinding, BTreeSet<u64>>::new();
        let mut missing_stable_source = false;
        for observation in self.observations.iter().filter(|observation| {
            observation.copy_before_clear_submitted && observation.selected_present_identity_passed
        }) {
            for matrix in &observation.matrices {
                if matrix.matrix_kind != "view" && matrix.matrix_kind != "projection" {
                    continue;
                }
                let Some(source) = matrix.stable_source.as_ref() else {
                    missing_stable_source = true;
                    continue;
                };
                if matrix.matrix_kind == "view" {
                    view_sources
                        .entry(source)
                        .or_default()
                        .insert(observation.present_seq);
                } else if matrix.configured_vertical_fov_matches == Some(true) {
                    projection_sources
                        .entry(source)
                        .or_default()
                        .insert(observation.present_seq);
                }
            }
        }
        if missing_stable_source {
            blockers.push(
                "D3D12 render-camera matrix lacks a bounded cross-launch semantic source".into(),
            );
        }
        let admitted_views = view_sources
            .iter()
            .filter(|(_, presents)| presents.len() >= MIN_ADMITTED_PRESENTS_PER_MATRIX_SOURCE)
            .map(|(source, _)| source)
            .collect::<Vec<_>>();
        let admitted_projections = projection_sources
            .iter()
            .filter(|(_, presents)| presents.len() >= MIN_ADMITTED_PRESENTS_PER_MATRIX_SOURCE)
            .map(|(source, _)| source)
            .collect::<Vec<_>>();
        if admitted_views.len() != 1 {
            blockers.push(format!(
                "D3D12 render-camera authoring found {} stable view sources; expected one",
                admitted_views.len()
            ));
        }
        if admitted_projections.len() != 1 {
            blockers.push(format!(
                "D3D12 render-camera authoring found {} stable projection sources; expected one",
                admitted_projections.len()
            ));
        }
        if let ([view], [projection]) = (admitted_views.as_slice(), admitted_projections.as_slice())
            && (view.descriptor_table != projection.descriptor_table
                || view.cbv_resource_offset != projection.cbv_resource_offset
                || view.declared_cbv_bytes != projection.declared_cbv_bytes
                || view.draw_relation != projection.draw_relation
                || view.matrix_offset == projection.matrix_offset)
        {
            blockers.push(
                "D3D12 render-camera view/projection are not distinct matrices in one stable selected-DSV CBV"
                    .into(),
            );
        }
        blockers.sort();
        blockers.dedup();
        blockers
    }

    /// Exact-Present summary for pose ingress: no history/resource/receipt clone. Admission
    /// uses the same bounded history and predicates as the full report, never a stale cached pass.
    pub(crate) fn exact_present_fields(
        &self,
        present_seq: u64,
        external_loss: bool,
    ) -> (bool, bool, bool) {
        let mut view = false;
        let mut projection = false;
        for draw in self.observations.iter().filter(|draw| {
            draw.present_seq == present_seq
                && draw.copy_before_clear_submitted
                && draw.selected_present_identity_passed
        }) {
            view |= draw
                .matrices
                .iter()
                .any(|matrix| matrix.matrix_kind == "view");
            projection |= draw
                .matrices
                .iter()
                .any(|matrix| matrix.matrix_kind == "projection");
        }
        (
            view,
            projection,
            view && projection && !external_loss && self.authoring_blockers().is_empty(),
        )
    }

    /// Recompute fail-closed authoring admission after every producer-owned counter has been
    /// attached. The backend sets contention/hot-state counters outside the tracker lock, so this
    /// method is intentionally public and idempotent.
    pub fn refresh_authoring_admission(&mut self) {
        self.authoring_blockers = self.authoring_blockers();
        self.candidate_admitted_for_cross_launch_discovery = self.authoring_blockers.is_empty();
        // This observer remains discovery-only even when all authoring gates pass.
        self.production_camera_binding_emitted = false;
        self.recording_ready = false;
    }
}

#[derive(Default)]
struct GpuVaOwnershipCache {
    entries: BTreeMap<u64, (Option<usize>, usize)>,
    hits: u64,
    misses: u64,
    resources_scanned: u64,
    invalidations: u64,
    evictions: u64,
}
impl GpuVaOwnershipCache {
    fn invalidate(&mut self) {
        self.entries.clear();
        self.invalidations = self.invalidations.saturating_add(1);
    }
}

pub struct RenderCameraTracker {
    production: Option<(
        depth_lab::D3d12CameraBinding,
        depth_lab::CameraPoseStreamContext,
    )>,
    bound_holstered: depth_lab::cp2077_bound_camera::BoundCamera,
    bound_present_copy: Option<(u64, bool)>,
    production_contention_baseline: (u64, u64),
    next_source_tag: u64,
    next_copy_submission: u64,
    selected_depth: Option<DsvIdentity>,
    expected_vertical_fov_degrees: f64,
    render_width: u32,
    render_height: u32,
    resources: BTreeMap<usize, BufferResource>,
    // Only address ownership is cached. Registration/release invalidates all entries;
    // map state, content generation, range checks and source bytes stay live.
    gpuva_ownership_cache: std::cell::RefCell<GpuVaOwnershipCache>,
    descriptor_heaps: BTreeMap<usize, DescriptorHeap>,
    root_signatures: BTreeMap<usize, RootSignatureObservation>,
    cbv_descriptor_store: Arc<CbvDescriptorStore>,
    upload_unmap_snapshot_bytes: usize,
    // Immutable per-copy versions, scoped to one Execute. A destination address is not a version.
    exact_cbv_copy_shadows: BTreeMap<ExactCbvCopyRequest, ExecutedCbvCopyShadow>,
    exact_cbv_copy_shadow_bytes: usize,
    lists: BTreeMap<usize, ListState>,
    depth_boundary_count: usize,
    submitted: Vec<SubmittedDraw>,
    /// Increments once per `execute`, giving draws from different command
    /// lists a comparable submission order.
    next_execute_batch: u64,
    /// Present -> the execute batch that carried its depth copy, so draws can
    /// be placed before or after the copy that produced the captured frame.
    depth_copy_submitted: BTreeMap<(QueueId, u64), DepthCopySubmission>,
    snapshot: D3d12RenderCameraSnapshot,
    selected_draws_resolved_since_present: u32,
    kcd2_pre_execute_errors: BTreeMap<usize, String>,
    kcd2_invalidated_prepared_lists: BTreeSet<usize>,
}

impl RenderCameraTracker {
    #[must_use]
    pub fn new(render_width: u32, render_height: u32, expected_vertical_fov_degrees: f64) -> Self {
        Self::with_descriptor_store(
            render_width,
            render_height,
            expected_vertical_fov_degrees,
            Arc::new(CbvDescriptorStore::new()),
        )
    }

    pub(crate) fn with_descriptor_store(
        render_width: u32,
        render_height: u32,
        expected_vertical_fov_degrees: f64,
        cbv_descriptor_store: Arc<CbvDescriptorStore>,
    ) -> Self {
        Self {
            next_source_tag: 1,
            next_copy_submission: 1,
            selected_depth: None,
            expected_vertical_fov_degrees,
            render_width,
            render_height,
            resources: BTreeMap::new(),
            gpuva_ownership_cache: Default::default(),
            descriptor_heaps: BTreeMap::new(),
            root_signatures: BTreeMap::new(),
            cbv_descriptor_store,
            upload_unmap_snapshot_bytes: 0,
            exact_cbv_copy_shadows: BTreeMap::new(),
            exact_cbv_copy_shadow_bytes: 0,
            lists: BTreeMap::new(),
            depth_boundary_count: 0,
            submitted: Vec::new(),
            next_execute_batch: 1,
            depth_copy_submitted: BTreeMap::new(),
            snapshot: D3d12RenderCameraSnapshot::default(),
            selected_draws_resolved_since_present: 0,
            kcd2_pre_execute_errors: BTreeMap::new(),
            kcd2_invalidated_prepared_lists: BTreeSet::new(),
            production: None,
            bound_holstered: Default::default(),
            bound_present_copy: None,
            production_contention_baseline: (0, 0),
        }
    }

    pub fn arm_selected_depth(&mut self, identity: DsvIdentity) {
        self.selected_depth = Some(identity);
        self.kcd2_pre_execute_errors.clear();
        self.kcd2_invalidated_prepared_lists.clear();
        self.snapshot.selected_depth_armed = true;
        self.submitted.clear();
        self.depth_copy_submitted.clear();
        for list in self.lists.values_mut() {
            list.pending.clear();
        }
    }

    pub(crate) fn authoring_boundary_is_drained(&self) -> bool {
        self.production.is_none()
            && self
                .snapshot
                .local_archive
                .as_ref()
                .is_some_and(|s| !s.frozen)
            && self.submitted.is_empty()
            && self.depth_copy_submitted.is_empty()
            && self.exact_cbv_copy_shadows.is_empty()
            && self.lists.values().all(|l| {
                l.pending.is_empty()
                    && l.deferred_copy_draws.is_empty()
                    && (l.depth_boundaries.executions != 0 || l.depth_boundaries.events.is_empty())
            })
    }

    pub(crate) fn rebind_authoring_source(&mut self, identity: DsvIdentity) -> Result<(), String> {
        if !self.authoring_boundary_is_drained() {
            return Err(
                "authoring camera boundary still owns unaccounted work or is not diagnostic".into(),
            );
        }
        // Do not call arm_selected_depth/disarm: those clear outstanding work. Keep every
        // historical observation, execution counter, copy receipt and failure counter.
        self.selected_depth = Some(identity);
        self.snapshot.selected_depth_armed = true;
        Ok(())
    }

    pub fn configure_production(
        &mut self,
        binding: depth_lab::D3d12CameraBinding,
        context: depth_lab::CameraPoseStreamContext,
        contention: (u64, u64),
    ) -> Result<(), String> {
        binding
            .validate_for_executable(&binding.executable_sha256)
            .map_err(|e| e.to_string())?;
        for source in binding.matrix_sources() {
            if source.matrix_offset as usize + 64 > ROOT_CBV_SNAPSHOT_BYTES {
                return Err(
                    "D3D12 production binding exceeds the bounded CBV snapshot window".into(),
                );
            }
        }
        self.render_width = binding.render_width;
        self.render_height = binding.render_height;
        self.production = Some((binding, context));
        self.bound_holstered = Default::default();
        self.bound_present_copy = None;
        self.production_contention_baseline = contention;
        self.snapshot = D3d12RenderCameraSnapshot::default();
        self.snapshot.selected_depth_armed = self.selected_depth.is_some();
        self.selected_draws_resolved_since_present = 0;
        self.submitted.clear();
        self.depth_copy_submitted.clear();
        for list in self.lists.values_mut() {
            list.pending.clear();
            list.deferred_copy_draws.clear();
            list.viewport = None;
            list.scissor = None;
            list.selected_draws_scanned = 0;
        }
        Ok(())
    }

    pub fn matches_production_draw(&self, bindings: &SelectedDrawBindings) -> bool {
        let Some((binding, _)) = &self.production else {
            return true;
        };
        binding.view_sources.iter().any(|view| {
            let source = &view.source;
            self.root_signatures
                .get(&bindings.root_signature)
                .and_then(|signature| signature.sha256.as_ref())
                == Some(&source.descriptor_table.root_signature_sha256)
                && bindings.roots.iter().any(|root| {
                    root.source == RootBindingSource::DescriptorTableBase
                        && root.root_parameter_index == source.descriptor_table.root_parameter_index
                })
        })
    }

    /// Consume this Present once. Unlike bounded discovery, production releases observations
    /// continuously and must not silently discard changed matrices under the same stable key.
    pub(crate) fn take_production_camera_sample(
        &mut self,
        present_seq: u64,
        contention: (u64, u64),
    ) -> Result<Option<ProductionCameraSample>, String> {
        let Some((binding, context)) = &self.production else {
            return Ok(None);
        };
        if contention != self.production_contention_baseline
            || self.snapshot.pending_draw_overflow != 0
            || self.snapshot.present_observation_overflow != 0
            || self
                .snapshot
                .selected_table_matrix_candidates_invalidated_by_generation
                != 0
            || self.snapshot.matrix_candidate_overflow != 0
            || self.snapshot.upload_unmap_snapshot_budget_exhausted != 0
            || self.snapshot.exact_cbv_copy_shadow_budget_exhausted != 0
            || !self.snapshot.blockers.is_empty()
        {
            return Err(format!(
                "D3D12 production lost draw/queue observation coverage: \
                 tracker_contention_drops={} (baseline={}), hot_state_drops={} (baseline={}), \
                 pending_draw_overflow={}, present_observation_overflow={}, \
                 selected_table_matrix_candidates_invalidated_by_generation={}, \
                 matrix_candidate_overflow={}, upload_unmap_snapshot_budget_exhausted={}, \
                 exact_cbv_copy_shadow_budget_exhausted={}, blockers={:?}",
                contention.0,
                self.production_contention_baseline.0,
                contention.1,
                self.production_contention_baseline.1,
                self.snapshot.pending_draw_overflow,
                self.snapshot.present_observation_overflow,
                self.snapshot
                    .selected_table_matrix_candidates_invalidated_by_generation,
                self.snapshot.matrix_candidate_overflow,
                self.snapshot.upload_unmap_snapshot_budget_exhausted,
                self.snapshot.exact_cbv_copy_shadow_budget_exhausted,
                self.snapshot.blockers,
            ));
        }
        let Some(selected) = self.selected_depth else {
            return Ok(None);
        };
        if binding.holstered_camera.is_some() {
            if !self.bound_holstered.started()
                && self.bound_present_copy == Some((present_seq, false))
            {
                // The first callback after DSV arm has no earlier injected copy. Keep
                // waiting under the existing StartRun timeout; this never emits Ready.
                self.snapshot.observations.clear();
                return Ok(None);
            }
            // No raw-demo sidecar: use the normal StartRun binding and camera writer.
            // The same physical draw policy is applied before any pose can become Ready.
            let samples = self.snapshot.local_camera_samples(
                present_seq,
                self.render_width,
                self.render_height,
            ).map_err(|error| format!(
                "{error}; present={present_seq}; actual_copy={:?}; observed_draws={}; copied_draws={}",
                self.bound_present_copy,
                self.snapshot.observations.len(),
                self.snapshot.observations.iter().filter(|d| d.present_seq == present_seq
                    && d.copy_before_clear_submitted && d.selected_present_identity_passed).count(),
            ));
            let crossed = self.snapshot.observations.iter().any(|draw| {
                draw.present_seq != present_seq
                    || draw.selected_depth_resource_tag != selected.resource.0
                    || draw.selected_depth_resource_epoch != selected.resource_epoch
                    || draw.selected_depth_descriptor_generation != selected.descriptor_generation
            });
            let viewport = self
                .snapshot
                .observations
                .iter()
                .find(|draw| {
                    draw.present_seq == present_seq
                        && draw.copy_before_clear_submitted
                        && draw.selected_present_identity_passed
                        && draw.pipeline_depth_write == Some(true)
                })
                .and_then(|draw| draw.pass_context.as_ref()?.viewport);
            self.snapshot.observations.clear();
            if crossed {
                return Err("bound camera crossed Present or selected-depth identity".into());
            }
            let (views, projections) = samples?;
            // local_camera_samples checked every contributing draw has the same full
            // viewport. Preserve one actual observation for the depth-index consumer.
            let viewport = viewport.ok_or("bound camera lost its admitted draw viewport")?;
            return self
                .bound_holstered
                .observe(binding, views, projections)
                .map_err(|e| e.to_string())?
                .map(|(view, projection, qpc)| {
                    depth_lab::d3d12_camera::assemble_pose(
                        binding,
                        context,
                        present_seq,
                        qpc,
                        view,
                        projection,
                    )
                    .map(|row| ProductionCameraSample {
                        row,
                        depth_copied: true,
                        viewport: Some(viewport),
                    })
                    .map_err(|error| format!(
                        "{error}; present={present_seq}; view={view:?}; projection={projection:?}"
                    ))
                })
                .transpose();
        }
        let mut exact: Option<D3d12RenderDrawObservation> = None;
        for mut draw in std::mem::take(&mut self.snapshot.observations) {
            if draw.present_seq != present_seq {
                return Err("D3D12 camera observation crossed a Present boundary".into());
            }
            if draw.selected_depth_resource_tag != selected.resource.0
                || draw.selected_depth_resource_epoch != selected.resource_epoch
                || draw.selected_depth_descriptor_generation != selected.descriptor_generation
            {
                return Err("D3D12 camera selected depth identity changed".into());
            }
            draw.matrices.retain(|matrix| {
                matrix.stable_source.as_ref() == Some(&binding.view_sources[0].source)
                    || matrix.stable_source.as_ref() == Some(&binding.projection_sources[0])
            });
            if draw.matrices.is_empty() {
                continue;
            }
            if draw.matrices.len() != 2 {
                return Err(
                    "D3D12 camera exact draw is missing a unique view/projection pair".into(),
                );
            }
            if let Some(prior) = &exact {
                if prior.queue_tag != draw.queue_tag || prior.matrices != draw.matrices {
                    return Err("D3D12 camera binding is ambiguous within one Present".into());
                }
            } else {
                exact = Some(draw);
            }
        }
        let Some(draw) = exact else {
            return Ok(None);
        };
        let view = draw
            .matrices
            .iter()
            .find(|m| m.stable_source.as_ref() == Some(&binding.view_sources[0].source))
            .ok_or("D3D12 bound view is absent")?;
        let projection = draw
            .matrices
            .iter()
            .find(|m| m.stable_source.as_ref() == Some(&binding.projection_sources[0]))
            .ok_or("D3D12 bound projection is absent")?;
        if view.source_tag != projection.source_tag {
            return Err("D3D12 camera matrices came from different CBVs".into());
        }
        depth_lab::d3d12_camera::assemble_pose(
            binding,
            context,
            present_seq,
            draw.present_qpc,
            view.matrix,
            projection.matrix,
        )
        .map(|row| {
            Some(ProductionCameraSample {
                row,
                depth_copied: draw.copy_before_clear_submitted
                    && draw.selected_present_identity_passed,
                viewport: draw.pass_context.and_then(|context| context.viewport),
            })
        })
        .map_err(str::to_owned)
    }

    #[cfg(test)]
    pub fn take_production_pose(
        &mut self,
        present_seq: u64,
        contention: (u64, u64),
    ) -> Result<Option<(depth_lab::CameraPoseRow, bool)>, String> {
        self.take_production_camera_sample(present_seq, contention)
            .map(|sample| sample.map(|sample| (sample.row, sample.depth_copied)))
    }

    pub fn disarm(&mut self) {
        self.production = None;
        self.bound_holstered = Default::default();
        self.selected_depth = None;
        self.snapshot.selected_depth_armed = false;
        self.submitted.clear();
        self.depth_copy_submitted.clear();
        for list in self.lists.values_mut() {
            list.pending.clear();
        }
    }

    pub fn register_list(&mut self, raw: usize, list_tag: u64) {
        self.discard_depth_boundaries(raw);
        self.note_kcd2_prepared_list_invalidation(raw);
        self.lists.insert(
            raw,
            ListState {
                list_tag,
                list_generation: 1,
                ..ListState::default()
            },
        );
    }

    pub fn reset_list(&mut self, raw: usize) {
        self.discard_depth_boundaries(raw);
        self.note_kcd2_prepared_list_invalidation(raw);
        if let Some(list) = self.lists.get_mut(&raw) {
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            list.diagnostic_draw_weights.clear();
            list.viewport = None;
            list.scissor = None;
            list.list_generation = list.list_generation.saturating_add(1);
            list.bound_depth = None;
            list.root_cbvs.clear();
            list.draw_ordinal = 0;
            list.selected_draws_scanned = 0;
            list.pending.clear();
            list.exact_cbv_copy_requests.clear();
            list.copy_buffer_spans.clear();
            list.copy_span_overflow = false;
            list.deferred_copy_draws.clear();
        }
    }

    pub fn release_list(&mut self, raw: usize) {
        self.discard_depth_boundaries(raw);
        self.note_kcd2_prepared_list_invalidation(raw);
        self.lists.remove(&raw);
    }

    fn note_kcd2_prepared_list_invalidation(&mut self, raw: usize) {
        if self.lists.get(&raw).is_some_and(|list| {
            list.pending
                .iter()
                .any(|draw| draw.kcd2_pre_execute_prepared)
        }) {
            self.kcd2_invalidated_prepared_lists.insert(raw);
            self.kcd2_pre_execute_errors.entry(raw).or_insert_with(|| {
                "KCD2 prepared command list was reset/released/replaced before submission completed".into()
            });
        }
        if !self.kcd2_invalidated_prepared_lists.contains(&raw) {
            self.kcd2_pre_execute_errors.remove(&raw);
        }
        // A prepared-source failure remains visible through a following Register/Reset until
        // the submission reports its fatal error. A new arm clears the entire run's state.
    }

    pub fn set_viewport(&mut self, raw: usize, viewport: CapturedViewport) {
        if let Some(list) = self.lists.get_mut(&raw) {
            list.viewport = Some(viewport);
        }
    }

    pub fn register_buffer(&mut self, raw: usize, gpu_virtual_address: u64, buffer_bytes: u64) {
        self.register_buffer_with_heap_kind(raw, gpu_virtual_address, buffer_bytes, false);
    }

    #[must_use]
    pub fn has_buffer(&self, raw: usize) -> bool {
        self.resources.contains_key(&raw)
    }

    /// Only the uniquely identified UPLOAD resource of a pinned Direct CBV may be mapped.
    /// No DEFAULT-heap readback, stale pointer, or ambiguous address is substituted.
    pub fn direct_root_mapping_candidate(
        &mut self,
        root_parameter: u32,
        gpu_va: u64,
    ) -> Option<(usize, u64)> {
        let (first_raw, count) = self.gpuva_resource_matches(gpu_va);
        let first =
            first_raw.and_then(|raw| self.resources.get(&raw).map(|resource| (raw, resource)));
        let first_sample = first.map(|(raw, resource)| DirectRootResourceSample {
            raw,
            gpu_base: resource.gpu_virtual_address,
            buffer_bytes: resource.buffer_bytes,
            upload_heap: resource.upload_heap,
            mapped: resource.cpu_address().is_some(),
            map_count: resource.map_count,
        });
        let (outcome, candidate) = match first {
            None => ("unregistered_gpu_address", None),
            Some(_) if count != 1 => ("ambiguous_gpu_address", None),
            Some((_, resource))
                if (gpu_va - resource.gpu_virtual_address)
                    .checked_add(ROOT_CBV_SNAPSHOT_BYTES as u64)
                    .is_none_or(|end| end > resource.buffer_bytes) =>
            {
                ("short_cbv_range", None)
            }
            Some((_, resource)) if resource.cpu_address().is_some() => ("mapped", None),
            Some((_, resource)) if !resource.upload_heap => ("unmapped_non_upload", None),
            Some((raw, resource)) => (
                "unmapped_upload",
                Some((raw, gpu_va - resource.gpu_virtual_address)),
            ),
        };
        if self.snapshot.direct_root_resolution_samples.len() < 8
            && !self
                .snapshot
                .direct_root_resolution_samples
                .iter()
                .any(|sample| sample.root_parameter == root_parameter && sample.outcome == outcome)
        {
            self.snapshot
                .direct_root_resolution_samples
                .push(DirectRootResolutionSample {
                    root_parameter,
                    gpu_va,
                    matching_resources: count,
                    outcome,
                    first_resource: first_sample,
                });
        }
        candidate
    }

    #[must_use]
    pub fn buffer_needs_observer_map(&self, raw: usize) -> bool {
        self.resources
            .get(&raw)
            .is_some_and(|resource| resource.upload_heap && resource.cpu_address().is_none())
    }

    pub fn register_buffer_with_heap_kind(
        &mut self,
        raw: usize,
        gpu_virtual_address: u64,
        buffer_bytes: u64,
        upload_heap: bool,
    ) {
        if raw == 0 || gpu_virtual_address == 0 || buffer_bytes < 64 {
            return;
        }
        if let Some(existing) = self.resources.get(&raw) {
            // The resource hook is installed both at creation and when a DSV later exposes the
            // same object. Exact duplicate discovery is idempotent; only contradictory identity
            // is unsafe.
            if existing.gpu_virtual_address == gpu_virtual_address
                && existing.buffer_bytes == buffer_bytes
                && existing.upload_heap == upload_heap
            {
                return;
            }
            // A COM interface pointer may be recycled after Release reached zero. The release
            // callback is diagnostic best-effort under shared runtime vtables, so a contradictory
            // registration must replace all stale per-object state rather than permanently poison
            // a fresh exact resource identity. Every descriptor/copy proof is generation-bound and
            // therefore still fails closed across this replacement.
            self.release_resource(raw);
        }
        let source_tag = self.next_source_tag;
        self.next_source_tag = self.next_source_tag.saturating_add(1);
        self.gpuva_ownership_cache.get_mut().invalidate();
        self.resources.insert(
            raw,
            BufferResource {
                source_tag,
                gpu_virtual_address,
                buffer_bytes,
                mapped_cpu_address: None,
                observer_mapped_cpu_address: None,
                map_count: 0,
                upload_heap,
                content_generation: 0,
                unmap_snapshots: BTreeMap::new(),
            },
        );
    }

    pub fn map_buffer(&mut self, raw: usize, mapped_cpu_address: usize) {
        if mapped_cpu_address == 0 {
            return;
        }
        if let Some(resource) = self.resources.get_mut(&raw) {
            if resource.map_count == 0 {
                resource.content_generation = resource.content_generation.saturating_add(1);
                self.upload_unmap_snapshot_bytes = self.upload_unmap_snapshot_bytes.saturating_sub(
                    resource
                        .unmap_snapshots
                        .values()
                        .map(|snapshot| snapshot.bytes.len())
                        .sum(),
                );
                resource.unmap_snapshots.clear();
            }
            resource.map_count = resource.map_count.saturating_add(1);
            resource.mapped_cpu_address = Some(mapped_cpu_address);
        }
    }

    pub fn register_observer_mapping(&mut self, raw: usize, address: usize) -> Result<(), String> {
        let resource = self
            .resources
            .get_mut(&raw)
            .ok_or("observer mapping resource missing")?;
        if !resource.upload_heap || address == 0 {
            return Err("observer mapping must reference an UPLOAD resource".into());
        }
        resource.observer_mapped_cpu_address = Some(address);
        Ok(())
    }

    pub fn unmap_buffer(&mut self, raw: usize) {
        if let Some(resource) = self.resources.get_mut(&raw) {
            resource.map_count = resource.map_count.saturating_sub(1);
            if resource.map_count == 0 {
                resource.mapped_cpu_address = None;
            }
        }
    }

    pub fn snapshot_upload_cbvs_before_final_unmap<F>(
        &mut self,
        raw: usize,
        written_range: Option<(usize, usize)>,
        mut read_bytes: F,
    ) where
        F: FnMut(usize, usize) -> Option<Vec<u8>>,
    {
        let Some(resource) = self.resources.get(&raw) else {
            return;
        };
        if !resource.upload_heap || resource.map_count != 1 {
            return;
        }
        self.snapshot.upload_unmap_snapshot_attempts = self
            .snapshot
            .upload_unmap_snapshot_attempts
            .saturating_add(1);
        if written_range.is_some_and(|(begin, end)| {
            begin > end || u64::try_from(end).map_or(true, |end| end > resource.buffer_bytes)
        }) {
            self.snapshot.upload_unmap_snapshot_invalid_written_ranges = self
                .snapshot
                .upload_unmap_snapshot_invalid_written_ranges
                .saturating_add(1);
            return;
        }

        let base_gpuva = resource.gpu_virtual_address;
        let end_gpuva = base_gpuva.saturating_add(resource.buffer_bytes);
        let mapped_cpu_address = resource.mapped_cpu_address;
        let content_generation = resource.content_generation;
        let buffer_bytes = resource.buffer_bytes;
        let mut offsets = BTreeSet::new();
        for gpuva in self
            .cbv_descriptor_store
            .gpuvas_in_range(base_gpuva, end_gpuva)
        {
            let offset = gpuva - base_gpuva;
            if offset
                .checked_add(ROOT_CBV_SNAPSHOT_BYTES as u64)
                .is_some_and(|end| end <= buffer_bytes)
            {
                offsets.insert(offset);
            }
        }
        if offsets.len() > MAX_UPLOAD_UNMAP_SNAPSHOTS_PER_RESOURCE {
            self.snapshot.upload_unmap_snapshot_budget_exhausted = self
                .snapshot
                .upload_unmap_snapshot_budget_exhausted
                .saturating_add(
                    u64::try_from(offsets.len() - MAX_UPLOAD_UNMAP_SNAPSHOTS_PER_RESOURCE)
                        .unwrap_or(u64::MAX),
                );
        }
        let remaining_bytes =
            MAX_UPLOAD_UNMAP_SNAPSHOT_BYTES_TOTAL.saturating_sub(self.upload_unmap_snapshot_bytes);
        let global_slots = remaining_bytes / ROOT_CBV_SNAPSHOT_BYTES;
        let capture_slots = offsets
            .len()
            .min(MAX_UPLOAD_UNMAP_SNAPSHOTS_PER_RESOURCE)
            .min(global_slots);
        if capture_slots < offsets.len().min(MAX_UPLOAD_UNMAP_SNAPSHOTS_PER_RESOURCE) {
            self.snapshot.upload_unmap_snapshot_budget_exhausted = self
                .snapshot
                .upload_unmap_snapshot_budget_exhausted
                .saturating_add(
                    u64::try_from(
                        offsets.len().min(MAX_UPLOAD_UNMAP_SNAPSHOTS_PER_RESOURCE) - capture_slots,
                    )
                    .unwrap_or(u64::MAX),
                );
        }
        let Some(mapped_cpu_address) = mapped_cpu_address else {
            self.snapshot.upload_unmap_snapshot_read_rejections = self
                .snapshot
                .upload_unmap_snapshot_read_rejections
                .saturating_add(u64::try_from(capture_slots).unwrap_or(u64::MAX));
            return;
        };

        let mut captured = Vec::new();
        for offset in offsets.into_iter().take(capture_slots) {
            let Some(address) = usize::try_from(offset)
                .ok()
                .and_then(|offset| mapped_cpu_address.checked_add(offset))
            else {
                self.snapshot.upload_unmap_snapshot_read_rejections = self
                    .snapshot
                    .upload_unmap_snapshot_read_rejections
                    .saturating_add(1);
                continue;
            };
            let Some(bytes) = read_bytes(address, ROOT_CBV_SNAPSHOT_BYTES) else {
                self.snapshot.upload_unmap_snapshot_read_rejections = self
                    .snapshot
                    .upload_unmap_snapshot_read_rejections
                    .saturating_add(1);
                continue;
            };
            if bytes.len() != ROOT_CBV_SNAPSHOT_BYTES {
                self.snapshot.upload_unmap_snapshot_read_rejections = self
                    .snapshot
                    .upload_unmap_snapshot_read_rejections
                    .saturating_add(1);
                continue;
            }
            captured.push((offset, bytes));
        }
        let Some(resource) = self.resources.get_mut(&raw) else {
            return;
        };
        if resource.content_generation != content_generation || resource.map_count != 1 {
            return;
        }
        for (offset, bytes) in captured {
            let prior = resource.unmap_snapshots.insert(
                offset,
                UploadUnmapSnapshot {
                    content_generation,
                    bytes,
                },
            );
            if let Some(prior) = prior {
                self.upload_unmap_snapshot_bytes = self
                    .upload_unmap_snapshot_bytes
                    .saturating_sub(prior.bytes.len());
            }
            self.upload_unmap_snapshot_bytes = self
                .upload_unmap_snapshot_bytes
                .saturating_add(ROOT_CBV_SNAPSHOT_BYTES);
            self.snapshot.upload_unmap_snapshot_ranges_captured = self
                .snapshot
                .upload_unmap_snapshot_ranges_captured
                .saturating_add(1);
        }
    }

    pub fn release_resource(&mut self, raw: usize) {
        if let Some(resource) = self.resources.remove(&raw) {
            self.gpuva_ownership_cache.get_mut().invalidate();
            self.upload_unmap_snapshot_bytes = self.upload_unmap_snapshot_bytes.saturating_sub(
                resource
                    .unmap_snapshots
                    .values()
                    .map(|snapshot| snapshot.bytes.len())
                    .sum(),
            );
        }
        let removed: Vec<_> = self
            .exact_cbv_copy_shadows
            .iter()
            .filter_map(|(key, shadow)| {
                (key.destination_resource_raw == raw || shadow.source_resource_raw == raw)
                    .then_some(*key)
            })
            .collect();
        for key in removed {
            if let Some(shadow) = self.exact_cbv_copy_shadows.remove(&key) {
                self.exact_cbv_copy_shadow_bytes = self
                    .exact_cbv_copy_shadow_bytes
                    .saturating_sub(shadow.bytes.len());
            }
        }
    }

    pub fn register_descriptor_heap(
        &mut self,
        raw: usize,
        cpu_start: u64,
        gpu_start: u64,
        descriptor_count: u32,
        increment: u32,
        shader_visible: bool,
    ) {
        if raw == 0 || cpu_start == 0 || descriptor_count == 0 || increment == 0 {
            return;
        }
        let value = DescriptorHeap {
            cpu_start,
            gpu_start,
            descriptor_count,
            increment,
            shader_visible,
        };
        if self.descriptor_heaps.get(&raw).is_some_and(|old| {
            old.cpu_start == value.cpu_start
                && old.gpu_start == value.gpu_start
                && old.descriptor_count == value.descriptor_count
                && old.increment == value.increment
                && old.shader_visible == value.shader_visible
        }) {
            return;
        }
        self.descriptor_heaps.insert(raw, value);
    }

    pub fn release_descriptor_heap(&mut self, raw: usize) {
        let Some(heap) = self.descriptor_heaps.remove(&raw) else {
            return;
        };
        let bytes = u64::from(heap.descriptor_count).saturating_mul(u64::from(heap.increment));
        let end = heap.cpu_start.saturating_add(bytes);
        self.cbv_descriptor_store
            .release_descriptor_heap(heap.cpu_start, end);
    }

    pub fn register_root_signature(
        &mut self,
        raw: usize,
        cbv_ranges: BTreeMap<u32, Vec<RootCbvRange>>,
    ) {
        if raw != 0 {
            self.root_signatures.insert(
                raw,
                RootSignatureObservation {
                    sha256: None,
                    cbv_ranges,
                },
            );
        }
    }

    pub fn register_root_signature_with_fingerprint(
        &mut self,
        raw: usize,
        sha256: String,
        cbv_ranges: BTreeMap<u32, Vec<RootCbvRange>>,
    ) {
        if raw == 0 || !depth_lab::camera_binding::is_sha256_digest(&sha256) {
            self.block("D3D12 render-camera root-signature fingerprint is invalid");
            return;
        }
        self.root_signatures.insert(
            raw,
            RootSignatureObservation {
                sha256: Some(sha256),
                cbv_ranges,
            },
        );
    }

    pub fn release_root_signature(&mut self, raw: usize) {
        self.root_signatures.remove(&raw);
    }

    #[must_use]
    pub fn root_signature_sha256(&self, raw: usize) -> Option<String> {
        self.root_signatures.get(&raw)?.sha256.clone()
    }

    pub fn write_cbv_descriptor(&mut self, cpu_handle: u64, gpu_va: u64, size_in_bytes: u32) {
        self.cbv_descriptor_store
            .write_cbv_descriptor(cpu_handle, gpu_va, size_in_bytes);
    }

    pub fn invalidate_cbv_descriptor(&mut self, cpu_handle: u64) {
        self.cbv_descriptor_store
            .invalidate_cbv_descriptor(cpu_handle);
    }

    /// Copy against one shared descriptor store; the store snapshots all sources
    /// while holding the required shards before it updates any destination.
    pub fn copy_cbv_descriptors(&mut self, destinations: &[u64], sources: &[u64]) -> bool {
        self.cbv_descriptor_store
            .copy_cbv_descriptors(destinations, sources)
    }

    pub fn record_copy_buffer_region(
        &mut self,
        list_raw: usize,
        destination_resource_raw: usize,
        destination_offset: u64,
        source_resource_raw: usize,
        source_offset: u64,
        bytes: u64,
    ) {
        self.snapshot.exact_cbv_copy_calls = self.snapshot.exact_cbv_copy_calls.saturating_add(1);
        let Some(list) = self.lists.get(&list_raw) else {
            return;
        };
        let Some(destination) = self.resources.get(&destination_resource_raw) else {
            self.snapshot.exact_cbv_copy_unknown_destination_resource = self
                .snapshot
                .exact_cbv_copy_unknown_destination_resource
                .saturating_add(1);
            return;
        };
        if bytes == 0 {
            return;
        }
        let source = self.resources.get(&source_resource_raw);
        let source_readable =
            source.is_some_and(|source| source.upload_heap && source.cpu_address().is_some());
        if source.is_none() {
            self.snapshot.exact_cbv_copy_unknown_source_resource = self
                .snapshot
                .exact_cbv_copy_unknown_source_resource
                .saturating_add(1);
        } else if !source_readable {
            self.snapshot.exact_cbv_copy_source_not_upload_or_mapped = self
                .snapshot
                .exact_cbv_copy_source_not_upload_or_mapped
                .saturating_add(1);
        }
        let range_valid = destination_offset
            .checked_add(bytes)
            .is_some_and(|end| end <= destination.buffer_bytes)
            && source.is_some_and(|source| {
                source_offset
                    .checked_add(bytes)
                    .is_some_and(|end| end <= source.buffer_bytes)
            });
        if !range_valid {
            self.snapshot.exact_cbv_copy_range_rejections = self
                .snapshot
                .exact_cbv_copy_range_rejections
                .saturating_add(1);
        }
        // Keep even unreadable writes as invalidation spans. Falling through to an older
        // readable copy would attribute stale bytes to a later draw.
        let span = CopyBufferSpan {
            readable: source_readable && range_valid,
            list_generation: list.list_generation,
            destination_resource_raw,
            destination_source_tag: destination.source_tag,
            destination_content_generation: destination.content_generation,
            destination_offset,
            source_resource_raw,
            source_source_tag: source.map_or(0, |source| source.source_tag),
            source_content_generation: source.map_or(0, |source| source.content_generation),
            source_offset,
            bytes,
        };
        if let Some(list) = self.lists.get_mut(&list_raw) {
            if list.copy_buffer_spans.len() < MAX_COPY_BUFFER_SPANS_PER_LIST {
                list.copy_buffer_spans.push(span);
                self.snapshot.exact_cbv_copy_spans_recorded = self
                    .snapshot
                    .exact_cbv_copy_spans_recorded
                    .saturating_add(1);
            } else {
                list.copy_span_overflow = true;
                self.snapshot.exact_cbv_copy_span_budget_exhausted = self
                    .snapshot
                    .exact_cbv_copy_span_budget_exhausted
                    .saturating_add(1);
            }
        }
    }

    fn copy_request_is_live(&self, request: &ExactCbvCopyRequest) -> bool {
        self.copy_request_is_live_measured(request, None)
    }

    fn copy_request_is_live_measured(
        &self,
        request: &ExactCbvCopyRequest,
        metrics: Option<&mut crate::execute_prepare::PrepareSample>,
    ) -> bool {
        let live = self.lists.get(&request.list_raw).is_some_and(|list| {
            list.list_tag == request.list_tag
                && list.list_generation == request.list_generation
                && list.exact_cbv_copy_requests.contains(request)
        }) && self
            .resources
            .get(&request.source_resource_raw)
            .is_some_and(|resource| {
                resource.source_tag == request.source_source_tag
                    && resource.content_generation == request.source_content_generation
            })
            && self
                .resources
                .get(&request.destination_resource_raw)
                .is_some_and(|resource| {
                    resource.source_tag == request.destination_source_tag
                        && resource.content_generation == request.destination_content_generation
                });
        if !live {
            return false;
        }
        let descriptor = if let Some(metrics) = metrics {
            let (descriptor, timing) = self
                .cbv_descriptor_store
                .get_measured(&request.descriptor_identity.0);
            metrics.descriptor_wait_ns += crate::execute_prepare::nanos(timing.wait);
            metrics.descriptor_lookup_ns += crate::execute_prepare::nanos(timing.apply);
            descriptor
        } else {
            self.cbv_descriptor_store
                .get(&request.descriptor_identity.0)
        };
        descriptor.is_some_and(|descriptor| descriptor.generation == request.descriptor_identity.1)
    }

    pub fn prepare_exact_cbv_copy_shadows<F>(
        &mut self,
        lists: &[usize],
        read_bytes: F,
    ) -> Vec<PreparedCbvCopyShadow>
    where
        F: FnMut(usize, u64, usize) -> Option<Vec<u8>>,
    {
        self.prepare_exact_cbv_copy_shadows_measured(lists, None, read_bytes)
    }

    pub(crate) fn prepare_exact_cbv_copy_shadows_measured<F>(
        &mut self,
        lists: &[usize],
        mut metrics: Option<&mut crate::execute_prepare::PrepareSample>,
        mut read_bytes: F,
    ) -> Vec<PreparedCbvCopyShadow>
    where
        F: FnMut(usize, u64, usize) -> Option<Vec<u8>>,
    {
        let mut prepared: Vec<PreparedCbvCopyShadow> = Vec::new();
        // This cache lives only through THIS prepare/Execute. Keep per-descriptor outputs and
        // validation; share bytes only for the same physical command + exact resource/range
        // generations. Different commands and failed reads never borrow successful siblings.
        let mut physical_reads = BTreeMap::<PhysicalCbvCopyKey, usize>::new();
        for list_raw in lists {
            let Some(list) = self.lists.get(list_raw) else {
                continue;
            };
            for request in &list.exact_cbv_copy_requests {
                let start = metrics.is_some().then(std::time::Instant::now);
                let valid = self.copy_request_is_live_measured(request, metrics.as_deref_mut())
                    && self
                        .resources
                        .get(&request.source_resource_raw)
                        .is_some_and(|resource| resource.cpu_address().is_some());
                if let Some(metrics) = metrics.as_deref_mut() {
                    metrics.requests += 1;
                    metrics.validation_ns += crate::execute_prepare::elapsed(start);
                    metrics.live_requests += u64::from(valid);
                    metrics.validation_rejected += u64::from(!valid);
                }
                if !valid {
                    self.snapshot.exact_cbv_copy_shadow_generation_rejections = self
                        .snapshot
                        .exact_cbv_copy_shadow_generation_rejections
                        .saturating_add(1);
                    continue;
                }
                let physical = request.physical_key();
                if let Some(&index) = physical_reads.get(&physical) {
                    let start = metrics.is_some().then(std::time::Instant::now);
                    let bytes = prepared[index].bytes.clone();
                    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                    let diagnostic_snapshot = prepared[index].diagnostic_snapshot;
                    if let Some(metrics) = metrics.as_deref_mut() {
                        metrics.reuse_clone_ns += crate::execute_prepare::elapsed(start);
                        metrics.reused_snapshots += 1;
                        metrics.reused_bytes += bytes.len() as u64;
                    }
                    prepared.push(PreparedCbvCopyShadow {
                        request: *request,
                        bytes,
                        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                        diagnostic_snapshot,
                        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                        diagnostic_player: None,
                    });
                    continue;
                }
                if let Some(metrics) = metrics.as_deref_mut() {
                    metrics.read_attempts += 1;
                }
                #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                let diagnostic_snapshot =
                    crate::cp2077_camera_diagnostics::early_player::SnapshotTiming::begin();
                let read_result = read_bytes(
                    request.source_resource_raw,
                    request.source_offset,
                    request.bytes,
                );
                #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                let diagnostic_snapshot = diagnostic_snapshot.and_then(|timing| timing.finish());
                let Some(bytes) = read_result else {
                    if let Some(metrics) = metrics.as_deref_mut() {
                        metrics.read_rejected += 1;
                    }
                    self.snapshot.exact_cbv_copy_prepare_read_rejections = self
                        .snapshot
                        .exact_cbv_copy_prepare_read_rejections
                        .saturating_add(1);
                    continue;
                };
                if bytes.len() != request.bytes {
                    if let Some(metrics) = metrics.as_deref_mut() {
                        metrics.read_rejected += 1;
                    }
                    self.snapshot.exact_cbv_copy_prepare_read_rejections = self
                        .snapshot
                        .exact_cbv_copy_prepare_read_rejections
                        .saturating_add(1);
                    continue;
                }
                physical_reads.insert(physical, prepared.len());
                prepared.push(PreparedCbvCopyShadow {
                    request: *request,
                    bytes,
                    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                    diagnostic_snapshot,
                    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                    diagnostic_player: None,
                });
            }
        }
        prepared
    }

    pub fn commit_exact_cbv_copy_shadows(
        &mut self,
        queue: QueueId,
        prepared: Vec<PreparedCbvCopyShadow>,
    ) {
        // Versions from the previous Execute cannot satisfy requests in this submission.
        self.exact_cbv_copy_shadows.clear();
        self.exact_cbv_copy_shadow_bytes = 0;
        // Local to this Execute: bindings share a physical ID, while later executions of
        // the same recorded command cannot borrow it. Keep all binding checks and payloads.
        let mut submissions = BTreeMap::new();
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        let mut diagnostic_bindings =
            crate::cp2077_camera_diagnostics::early_player::Bindings::default();
        for prepared in prepared {
            if !self.copy_request_is_live(&prepared.request) {
                self.snapshot.exact_cbv_copy_shadow_generation_rejections = self
                    .snapshot
                    .exact_cbv_copy_shadow_generation_rejections
                    .saturating_add(1);
                continue;
            }
            let key = prepared.request;
            let prior_bytes = self
                .exact_cbv_copy_shadows
                .get(&key)
                .map_or(0, |shadow| shadow.bytes.len());
            let projected = self
                .exact_cbv_copy_shadow_bytes
                .saturating_sub(prior_bytes)
                .saturating_add(prepared.bytes.len());
            if projected > MAX_EXACT_CBV_COPY_SHADOW_BYTES_TOTAL {
                self.snapshot.exact_cbv_copy_shadow_budget_exhausted = self
                    .snapshot
                    .exact_cbv_copy_shadow_budget_exhausted
                    .saturating_add(1);
                continue;
            }
            self.exact_cbv_copy_shadow_bytes = projected;
            let physical = key.physical_key();
            let copy_submission = *submissions.entry(physical).or_insert_with(|| {
                let id = self.next_copy_submission;
                self.next_copy_submission = self.next_copy_submission.saturating_add(1);
                id
            });
            let receipt = CopyReceipt {
                schema: "dm-d3d12-executed-cbv-copy/v2".into(),
                copy_submission,
                queue_tag: queue.0,
                destination_source_tag: self.resources[&prepared.request.destination_resource_raw]
                    .source_tag,
                destination_content_generation: prepared.request.destination_content_generation,
                destination_offset: prepared.request.destination_offset,
                source_source_tag: self.resources[&prepared.request.source_resource_raw].source_tag,
                source_content_generation: prepared.request.source_content_generation,
                source_offset: prepared.request.source_offset,
                descriptor_generation: prepared.request.descriptor_identity.1,
                copied_bytes: prepared.bytes.len() as u64,
                bytes_sha256: format!("sha256:{:x}", Sha256::digest(&prepared.bytes)),
                copy_command: Some(physical.command),
            };
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            diagnostic_bindings.observe(
                prepared.diagnostic_player,
                prepared.diagnostic_snapshot,
                &receipt,
            );
            self.exact_cbv_copy_shadows.insert(
                key,
                ExecutedCbvCopyShadow {
                    receipt,
                    source_resource_raw: prepared.request.source_resource_raw,
                    source_content_generation: prepared.request.source_content_generation,
                    destination_content_generation: prepared.request.destination_content_generation,
                    descriptor_identity: prepared.request.descriptor_identity,
                    queue,
                    bytes: prepared.bytes,
                },
            );
            self.snapshot.exact_cbv_copy_shadows_committed = self
                .snapshot
                .exact_cbv_copy_shadows_committed
                .saturating_add(1);
        }
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        diagnostic_bindings.submit();
    }

    pub fn materialize_deferred_copy_draws(&mut self, queue: QueueId, lists: &[usize]) {
        for list_raw in lists {
            let deferred = self
                .lists
                .get_mut(list_raw)
                .map(|list| std::mem::take(&mut list.deferred_copy_draws))
                .unwrap_or_default();
            for mut draw in deferred {
                let before = draw.bindings.len();
                draw.bindings.retain_mut(|binding| {
                    if binding.content_source != BufferContentSource::CopyBufferRegionShadow {
                        return true;
                    }
                    let Some(descriptor_identity) = binding.descriptor_identity else {
                        return false;
                    };
                    let valid = binding
                        .pending_copy_request
                        .as_deref()
                        .and_then(|request| self.exact_cbv_copy_shadows.get(request))
                        .is_some_and(|shadow| {
                            shadow.queue == queue
                                && shadow.destination_content_generation
                                    == binding.content_generation
                                && shadow.descriptor_identity == descriptor_identity
                        });
                    if valid {
                        binding.copy_shadow_queue = Some(queue);
                    }
                    valid
                });
                if draw.bindings.len() != before {
                    self.snapshot.exact_cbv_copy_shadow_generation_rejections = self
                        .snapshot
                        .exact_cbv_copy_shadow_generation_rejections
                        .saturating_add(
                            u64::try_from(before - draw.bindings.len()).unwrap_or(u64::MAX),
                        );
                }
                if !draw.bindings.is_empty() {
                    self.scan_resolved_bindings(
                        *list_raw,
                        DrawLocation {
                            pass_context: draw.pass_context,
                            pipeline_depth: draw.pipeline_depth,
                            global_ordinal: draw.draw_ordinal,
                            pass_ordinal: draw.pass_ordinal,
                            ordinal_in_pass: draw.draw_ordinal_in_pass,
                            root_signature_sha256: draw.root_signature_sha256.as_deref(),
                        },
                        draw.selected_depth,
                        draw.bindings,
                        &mut |_, _, _, _| None,
                    );
                }
            }
        }
    }

    pub fn bind_depth(&mut self, list_raw: usize, identity: Option<DsvIdentity>) {
        if let Some(list) = self.lists.get_mut(&list_raw) {
            list.bound_depth = identity;
        }
    }

    pub fn bind_root_cbv(&mut self, list_raw: usize, root_parameter_index: u32, gpu_va: u64) {
        self.snapshot.root_cbv_calls = self.snapshot.root_cbv_calls.saturating_add(1);
        let Some(list) = self.lists.get_mut(&list_raw) else {
            return;
        };
        if !list.root_cbvs.contains_key(&root_parameter_index)
            && list.root_cbvs.len() >= MAX_ROOT_CBV_BINDINGS_PER_LIST
        {
            self.block("D3D12 render-camera root-CBV binding bound exceeded");
            return;
        }
        if gpu_va == 0 {
            list.root_cbvs.remove(&root_parameter_index);
        } else {
            list.root_cbvs.insert(root_parameter_index, gpu_va);
        }
    }

    pub fn observe_draw<F>(&mut self, list_raw: usize, mut read_bytes: F)
    where
        F: FnMut(usize, u64, usize) -> Option<Vec<u8>>,
    {
        let Some(selected_depth) = self.selected_depth else {
            return;
        };
        let Some(list) = self.lists.get_mut(&list_raw) else {
            return;
        };
        list.draw_ordinal = list.draw_ordinal.saturating_add(1);
        let list_snapshot = list.clone();
        if list_snapshot.bound_depth != Some(selected_depth) {
            return;
        }
        self.snapshot.selected_depth_draw_calls =
            self.snapshot.selected_depth_draw_calls.saturating_add(1);
        if self.selected_draws_resolved_since_present >= MAX_SELECTED_DRAWS_RESOLVED_PER_PRESENT {
            self.snapshot.draw_scan_budget_exhausted =
                self.snapshot.draw_scan_budget_exhausted.saturating_add(1);
            return;
        }
        let next_draw = list_snapshot.draw_ordinal;
        if !should_scan_selected_draw(next_draw, list_snapshot.selected_draws_scanned) {
            self.snapshot.draw_scan_budget_exhausted =
                self.snapshot.draw_scan_budget_exhausted.saturating_add(1);
            return;
        }
        if let Some(list) = self.lists.get_mut(&list_raw) {
            list.selected_draws_scanned = list.selected_draws_scanned.saturating_add(1);
        }
        self.selected_draws_resolved_since_present =
            self.selected_draws_resolved_since_present.saturating_add(1);
        let mut matrices = Vec::new();
        let mut saw_unmapped = false;
        for (root_parameter_index, gpu_va) in list_snapshot
            .root_cbvs
            .into_iter()
            .take(MAX_ROOT_CBV_SOURCES_PER_DRAW)
        {
            let Some(binding) = self.resolve_binding(root_parameter_index, gpu_va) else {
                saw_unmapped = true;
                continue;
            };
            let available = self
                .resources
                .get(&binding.resource_raw)
                .map_or(0, |resource| {
                    resource
                        .buffer_bytes
                        .saturating_sub(binding.resource_offset)
                });
            let snapshot_bytes = usize::try_from(available.min(ROOT_CBV_SNAPSHOT_BYTES as u64))
                .unwrap_or(ROOT_CBV_SNAPSHOT_BYTES);
            if snapshot_bytes < 64 {
                continue;
            }
            let Some(bytes) = read_bytes(
                binding.resource_raw,
                binding.resource_offset,
                snapshot_bytes,
            ) else {
                saw_unmapped = true;
                continue;
            };
            for candidate in scan_candidates(&bytes)
                .into_iter()
                .chain(scan_view_projection_pairs(&bytes))
            {
                if matrices.len() >= MAX_MATRIX_CANDIDATES_PER_DRAW {
                    self.snapshot.matrix_candidate_overflow =
                        self.snapshot.matrix_candidate_overflow.saturating_add(1);
                    break;
                }
                matrices.push(MatrixCandidate {
                    exact_copy_request: None,
                    copy_receipt: None,
                    source_tag: binding.source_tag,
                    root_parameter_index,
                    cbv_resource_offset: binding.resource_offset,
                    matrix_offset: candidate.offset as u32,
                    transpose: candidate.transpose,
                    matrix: candidate.matrix,
                    kind: candidate.kind,
                    source: binding.source,
                    content_source: binding.content_source,
                    resource_raw: binding.resource_raw,
                    resource_content_generation: binding.content_generation,
                    copy_shadow_queue: binding.copy_shadow_queue,
                    descriptor_identity: binding.descriptor_identity,
                    declared_bytes: binding.declared_bytes,
                    stable_descriptor: binding.stable_descriptor.clone(),
                });
            }
            for candidate in scan_view_projection_pairs(&bytes) {
                if matrices.len() >= MAX_MATRIX_CANDIDATES_PER_DRAW {
                    self.snapshot.matrix_candidate_overflow =
                        self.snapshot.matrix_candidate_overflow.saturating_add(1);
                    break;
                }
                matrices.push(MatrixCandidate {
                    exact_copy_request: None,
                    copy_receipt: None,
                    source_tag: binding.source_tag,
                    root_parameter_index: binding.root_parameter_index,
                    cbv_resource_offset: binding.resource_offset,
                    matrix_offset: candidate.offset as u32,
                    transpose: candidate.transpose,
                    matrix: candidate.matrix,
                    kind: candidate.kind,
                    source: binding.source,
                    content_source: binding.content_source,
                    resource_raw: binding.resource_raw,
                    resource_content_generation: binding.content_generation,
                    copy_shadow_queue: binding.copy_shadow_queue,
                    descriptor_identity: binding.descriptor_identity,
                    declared_bytes: binding.declared_bytes,
                    stable_descriptor: binding.stable_descriptor.clone(),
                });
            }
        }
        if saw_unmapped {
            self.snapshot.unmapped_root_cbv_rejections =
                self.snapshot.unmapped_root_cbv_rejections.saturating_add(1);
        }
        if matrices.is_empty() {
            return;
        }
        self.snapshot.direct_mapped_draws_with_matrices = self
            .snapshot
            .direct_mapped_draws_with_matrices
            .saturating_add(1);
        let pending_count: usize = self.lists.values().map(|list| list.pending.len()).sum();
        if pending_count >= MAX_PENDING_DRAWS {
            self.snapshot.pending_draw_overflow =
                self.snapshot.pending_draw_overflow.saturating_add(1);
            return;
        }
        if let Some(list) = self.lists.get_mut(&list_raw) {
            list.pending.push(PendingDraw {
                pass_context: None,
                pipeline_depth: None,
                list_tag: list.list_tag,
                list_generation: list.list_generation,
                draw_ordinal: next_draw,
                // The direct root-CBV path has no pass grouping to report.
                pass_ordinal: None,
                draw_ordinal_in_pass: None,
                root_signature_sha256: None,
                selected_depth,
                matrices,
                kcd2_snapshots: Vec::new(),
                kcd2_pre_execute_sources: Vec::new(),
                kcd2_pre_execute_prepared: false,
                kcd2_pre_execute_queue: None,
            });
        }
    }

    pub fn observe_selected_draw<F>(
        &mut self,
        list_raw: usize,
        bindings: &SelectedDrawBindings,
        mut read_bytes: F,
    ) -> SelectedDrawObservation
    where
        F: FnMut(usize, u64, usize) -> Option<Vec<u8>>,
    {
        self.observe_selected_draw_with_reader(list_raw, bindings, |_, raw, offset, bytes| {
            read_bytes(raw, offset, bytes)
        })
    }

    /// Resolve only the already admitted binding's live mapping under the caller's
    /// tracker lock. No per-draw clone of every unrelated mapped resource is needed.
    pub fn observe_selected_draw_mapped<F>(
        &mut self,
        list_raw: usize,
        bindings: &SelectedDrawBindings,
        mut read_bytes: F,
    ) -> SelectedDrawObservation
    where
        F: FnMut(usize, usize) -> Option<Vec<u8>>,
    {
        self.observe_selected_draw_with_reader(list_raw, bindings, |tracker, raw, offset, bytes| {
            read_bytes(tracker.mapped_read_address(raw, offset, bytes)?, bytes)
        })
    }

    fn observe_selected_draw_with_reader<F>(
        &mut self,
        list_raw: usize,
        bindings: &SelectedDrawBindings,
        mut read_bytes: F,
    ) -> SelectedDrawObservation
    where
        F: FnMut(&Self, usize, u64, usize) -> Option<Vec<u8>>,
    {
        let Some(selected_depth) = self.selected_depth else {
            return SelectedDrawObservation::default();
        };
        let Some(list) = self.lists.get(&list_raw) else {
            return SelectedDrawObservation::default();
        };
        if list.list_generation != bindings.list_generation {
            return SelectedDrawObservation::default();
        }
        self.snapshot.selected_depth_draw_calls =
            self.snapshot.selected_depth_draw_calls.saturating_add(1);
        let has_descriptor_table_binding = bindings
            .roots
            .iter()
            .any(|root| root.source == RootBindingSource::DescriptorTableBase);
        if !has_descriptor_table_binding {
            self.snapshot
                .selected_draws_without_descriptor_table_bindings = self
                .snapshot
                .selected_draws_without_descriptor_table_bindings
                .saturating_add(1);
        }
        if bindings.root_signature == 0 {
            self.snapshot.selected_draws_without_root_signature = self
                .snapshot
                .selected_draws_without_root_signature
                .saturating_add(1);
        } else {
            match self.root_signatures.get(&bindings.root_signature) {
                None => {
                    self.snapshot.selected_draws_with_unknown_root_signature = self
                        .snapshot
                        .selected_draws_with_unknown_root_signature
                        .saturating_add(1);
                }
                Some(signature) if signature.cbv_ranges.values().all(Vec::is_empty) => {
                    self.snapshot
                        .selected_draws_with_root_signature_without_cbv_ranges = self
                        .snapshot
                        .selected_draws_with_root_signature_without_cbv_ranges
                        .saturating_add(1);
                }
                Some(_) => {
                    self.snapshot.selected_draws_with_declared_cbv_ranges = self
                        .snapshot
                        .selected_draws_with_declared_cbv_ranges
                        .saturating_add(1);
                }
            }
        }
        let root_signature_sha256 = self.root_signature_sha256(bindings.root_signature);
        let kcd2_enabled = crate::kcd2_live_pose::enabled();
        if kcd2_enabled
            && !crate::kcd2_live_pose::accepts_draw(
                root_signature_sha256.as_deref(),
                bindings.pass_ordinal,
                bindings.draw_ordinal_in_pass,
            )
        {
            return SelectedDrawObservation::default();
        }
        if !kcd2_enabled
            && self.production.is_none()
            && !should_scan_selected_draw(bindings.draw_ordinal, list.selected_draws_scanned)
        {
            self.snapshot.draw_scan_budget_exhausted =
                self.snapshot.draw_scan_budget_exhausted.saturating_add(1);
            return SelectedDrawObservation::default();
        }
        if !kcd2_enabled
            && self.production.is_none()
            && let Some(list) = self.lists.get_mut(&list_raw)
        {
            list.selected_draws_scanned = list.selected_draws_scanned.saturating_add(1);
        }
        let mut resolved = Vec::new();
        for root in bindings.roots.iter().take(MAX_ROOT_CBV_SOURCES_PER_DRAW) {
            if kcd2_enabled && root.source != RootBindingSource::Direct {
                continue;
            }
            if let Some((production, _)) = &self.production
                && production.holstered_camera.is_some()
                && (root.source != RootBindingSource::DescriptorTableBase
                    || !production.view_sources.iter().any(|view| {
                        view.source.descriptor_table.root_parameter_index
                            == root.root_parameter_index
                    }))
            {
                // The bound holstered camera accepts only its authored table
                // sources. Do not resolve unrelated roots just to discard them.
                continue;
            }
            let bindings = match root.source {
                RootBindingSource::Direct => {
                    self.snapshot.root_cbv_calls = self.snapshot.root_cbv_calls.saturating_add(1);
                    self.snapshot.selected_direct_root_bindings = self
                        .snapshot
                        .selected_direct_root_bindings
                        .saturating_add(1);
                    let resolved = self.resolve_binding(root.root_parameter_index, root.value);
                    if resolved.is_none() {
                        self.snapshot.selected_direct_root_unresolved = self
                            .snapshot
                            .selected_direct_root_unresolved
                            .saturating_add(1);
                    }
                    resolved.into_iter().collect()
                }
                RootBindingSource::DescriptorTableBase => {
                    self.snapshot.descriptor_table_calls =
                        self.snapshot.descriptor_table_calls.saturating_add(1);
                    let resolution = self.resolve_descriptor_table_binding(
                        list_raw,
                        root.root_parameter_index,
                        root.value,
                        bindings.descriptor_heaps,
                        bindings.root_signature,
                    );
                    self.record_descriptor_table_resolution(&resolution);
                    resolution.bindings
                }
            };
            resolved.extend(bindings);
        }
        if let Some((production, _)) = &self.production {
            resolved.retain(|binding| {
                production.view_sources.iter().any(|view| {
                    let source = &view.source;
                    binding.stable_descriptor.as_ref() == Some(&source.descriptor_table)
                        && binding.resource_offset == source.cbv_resource_offset
                        && binding.declared_bytes == Some(source.declared_cbv_bytes)
                })
            });
        }
        let used_descriptor_table = resolved
            .iter()
            .any(|binding| binding.source == RootBindingSource::DescriptorTableBase);
        if used_descriptor_table {
            self.snapshot.selected_descriptor_table_draws = self
                .snapshot
                .selected_descriptor_table_draws
                .saturating_add(1);
        }
        if resolved.iter().any(|binding| {
            binding.content_source == BufferContentSource::CopyBufferRegionShadow
                && binding.copy_shadow_queue.is_none()
        }) {
            let production_dedup_key = self.production_draw_dedup_key(
                list_raw,
                bindings.list_generation,
                bindings,
                &resolved,
            );
            let late_requests: Vec<_> = resolved
                .iter()
                .filter_map(|binding| binding.pending_copy_request.as_deref().copied())
                .collect();
            if let Some(list) = self.lists.get_mut(&list_raw) {
                for request in late_requests {
                    if list.exact_cbv_copy_requests.contains(&request) {
                        continue;
                    }
                    if list.exact_cbv_copy_requests.len() >= MAX_EXACT_CBV_COPY_REQUESTS_PER_LIST {
                        self.snapshot.exact_cbv_copy_request_budget_exhausted = self
                            .snapshot
                            .exact_cbv_copy_request_budget_exhausted
                            .saturating_add(1);
                        continue;
                    }
                    list.exact_cbv_copy_requests.push(request);
                    self.snapshot.exact_cbv_copy_requests =
                        self.snapshot.exact_cbv_copy_requests.saturating_add(1);
                }
            }
            let deferred_count: usize = self
                .lists
                .values()
                .map(|list| list.deferred_copy_draws.len())
                .sum();
            if deferred_count >= MAX_PENDING_DRAWS {
                self.snapshot.pending_draw_overflow =
                    self.snapshot.pending_draw_overflow.saturating_add(1);
                return SelectedDrawObservation::default();
            }
            let staged = if let Some(list) = self.lists.get_mut(&list_raw) {
                list.deferred_copy_draws.push(DeferredCopyDraw {
                    pass_context: bindings.pass_context,
                    pipeline_depth: bindings.pipeline_depth,
                    draw_ordinal: bindings.draw_ordinal,
                    pass_ordinal: bindings.pass_ordinal,
                    draw_ordinal_in_pass: bindings.draw_ordinal_in_pass,
                    root_signature_sha256: root_signature_sha256.clone(),
                    selected_depth,
                    bindings: resolved,
                });
                true
            } else {
                false
            };
            return SelectedDrawObservation {
                staged,
                production_dedup_key: production_dedup_key.filter(|_| staged),
            };
        }
        let staged = self.scan_resolved_bindings(
            list_raw,
            DrawLocation {
                pass_context: bindings.pass_context,
                pipeline_depth: bindings.pipeline_depth,
                global_ordinal: bindings.draw_ordinal,
                pass_ordinal: bindings.pass_ordinal,
                ordinal_in_pass: bindings.draw_ordinal_in_pass,
                root_signature_sha256: root_signature_sha256.as_deref(),
            },
            selected_depth,
            resolved,
            &mut read_bytes,
        );
        SelectedDrawObservation {
            staged,
            production_dedup_key: None,
        }
    }

    fn production_draw_dedup_key(
        &self,
        list_raw: usize,
        list_generation: u64,
        draw: &SelectedDrawBindings,
        resolved_bindings: &[BufferBinding],
    ) -> Option<ProductionDrawDedupKey> {
        self.production
            .as_ref()
            .filter(|(binding, _)| binding.holstered_camera.is_some())?;
        let mut descriptor_identities = [(0, 0); MAX_PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES];
        let mut descriptor_count = 0usize;
        let mut root_bindings = [(0, 0); MAX_PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES];
        let mut root_binding_count = 0usize;
        for binding in resolved_bindings {
            if binding.content_source != BufferContentSource::CopyBufferRegionShadow {
                return None;
            }
            let descriptor_identity = binding.descriptor_identity?;
            let request = binding.pending_copy_request.as_deref()?;
            if request.list_raw != list_raw
                || request.list_generation != list_generation
                || request.descriptor_identity != descriptor_identity
            {
                return None;
            }
            if !descriptor_identities[..descriptor_count].contains(&descriptor_identity) {
                if descriptor_count == descriptor_identities.len() {
                    return None;
                }
                descriptor_identities[descriptor_count] = descriptor_identity;
                descriptor_count += 1;
            }

            let root_binding = draw.roots.iter().find(|root| {
                root.source == RootBindingSource::DescriptorTableBase
                    && root.root_parameter_index == binding.root_parameter_index
            })?;
            let root_identity = (root_binding.root_parameter_index, root_binding.value);
            if root_bindings[..root_binding_count].contains(&root_identity) {
                continue;
            }
            if root_binding_count == root_bindings.len() {
                return None;
            }
            root_bindings[root_binding_count] = root_identity;
            root_binding_count += 1;
        }
        if descriptor_count == 0 || root_binding_count == 0 {
            return None;
        }
        descriptor_identities[..descriptor_count].sort_unstable();
        root_bindings[..root_binding_count].sort_unstable();
        Some(ProductionDrawDedupKey {
            descriptor_identities,
            descriptor_count: descriptor_count.try_into().ok()?,
            root_bindings,
            root_binding_count: root_binding_count.try_into().ok()?,
        })
    }

    // Freeze the Draw's exact binding. Later root arguments or viewports cannot replace it.
    fn stage_kcd2_pre_execute_draw(
        &mut self,
        list_raw: usize,
        draw: DrawLocation<'_>,
        selected_depth: DsvIdentity,
        bindings: Vec<BufferBinding>,
    ) -> Result<(), String> {
        if bindings.len() != 1 {
            return Err("KCD2 v3 Draw must resolve exactly one Direct CBV".into());
        }
        let binding = bindings.into_iter().next().unwrap();
        if binding.source != RootBindingSource::Direct {
            return Err("KCD2 v3 source is not a Direct CBV".into());
        }
        let resource = self
            .resources
            .get(&binding.resource_raw)
            .ok_or("KCD2 v3 Draw source resource is missing")?;
        let list = self
            .lists
            .get(&list_raw)
            .ok_or("KCD2 v3 Draw list is missing")?;
        let signature = draw
            .root_signature_sha256
            .ok_or("KCD2 v3 Draw root signature is missing")?;
        let source = Kcd2PreExecuteSource {
            binding,
            gpu_base: resource.gpu_virtual_address,
            buffer_bytes: resource.buffer_bytes,
            root_signature_sha256: signature
                .strip_prefix("sha256:")
                .unwrap_or(signature)
                .to_owned(),
            pass_ordinal: draw.pass_ordinal,
            draw_ordinal: draw.ordinal_in_pass,
            viewport: list.viewport,
        };
        self.validate_kcd2_pre_execute_source(&source)?;
        if source.viewport.is_none() || self.selected_depth != Some(selected_depth) {
            return Err("KCD2 v3 Draw viewport or selected DSV is missing/stale".into());
        }
        let pending_count: usize = self.lists.values().map(|list| list.pending.len()).sum();
        if pending_count >= MAX_PENDING_DRAWS {
            return Err("KCD2 v3 pending Draw bound exceeded".into());
        }
        let list = self.lists.get_mut(&list_raw).unwrap();
        list.pending.push(PendingDraw {
            pass_context: draw.pass_context,
            pipeline_depth: draw.pipeline_depth,
            list_tag: list.list_tag,
            list_generation: list.list_generation,
            draw_ordinal: draw.global_ordinal,
            pass_ordinal: Some(draw.pass_ordinal),
            draw_ordinal_in_pass: Some(draw.ordinal_in_pass),
            root_signature_sha256: draw.root_signature_sha256.map(str::to_owned),
            selected_depth,
            matrices: Vec::new(),
            kcd2_snapshots: Vec::new(),
            kcd2_pre_execute_sources: vec![source],
            kcd2_pre_execute_prepared: false,
            kcd2_pre_execute_queue: None,
        });
        Ok(())
    }

    fn validate_kcd2_pre_execute_source(
        &self,
        source: &Kcd2PreExecuteSource,
    ) -> Result<(), String> {
        let binding = &source.binding;
        let resource = self
            .resources
            .get(&binding.resource_raw)
            .ok_or("KCD2 pre-Execute resource was released")?;
        if resource.source_tag != binding.source_tag
            || resource.gpu_virtual_address != source.gpu_base
            || resource.buffer_bytes != source.buffer_bytes
            || !resource.upload_heap
        {
            return Err("KCD2 pre-Execute resource identity/UPLOAD heap changed".into());
        }
        let end = binding
            .resource_offset
            .checked_add(ROOT_CBV_SNAPSHOT_BYTES as u64)
            .ok_or("KCD2 pre-Execute read range overflow")?;
        if end > resource.buffer_bytes
            || binding
                .declared_bytes
                .is_some_and(|bytes| bytes < ROOT_CBV_SNAPSHOT_BYTES as u32)
        {
            return Err("KCD2 pre-Execute read range is not fully covered".into());
        }
        let start_gpu = source
            .gpu_base
            .checked_add(binding.resource_offset)
            .ok_or("KCD2 pre-Execute GPU range overflow")?;
        let end_gpu = source
            .gpu_base
            .checked_add(end)
            .ok_or("KCD2 pre-Execute GPU range overflow")?;
        if self.resources.iter().any(|(&raw, other)| {
            raw != binding.resource_raw
                && other
                    .gpu_virtual_address
                    .checked_add(other.buffer_bytes)
                    .is_some_and(|other_end| {
                        other.gpu_virtual_address < end_gpu && start_gpu < other_end
                    })
        }) {
            return Err("KCD2 pre-Execute GPU read range overlaps another resource".into());
        }
        Ok(())
    }

    pub fn has_kcd2_pre_execute_work(&self, lists: &[usize]) -> bool {
        lists.iter().any(|raw| {
            self.kcd2_pre_execute_errors.contains_key(raw)
                || self.lists.get(raw).is_some_and(|list| {
                    list.pending
                        .iter()
                        .any(|draw| !draw.kcd2_pre_execute_sources.is_empty())
                })
        })
    }

    /// Called only before the application's original Execute. A later Map content generation
    /// is legitimate, but resource identity is immutable. Publish the batch only after all reads
    /// succeed; no previously prepared or Draw-time bytes may survive a failed preparation.
    pub fn prepare_kcd2_pre_execute_snapshots<F, Q>(
        &mut self,
        lists: &[usize],
        direct_queue: Option<QueueId>,
        mut read_bytes: F,
        mut sample_qpc: Q,
    ) -> Result<(), String>
    where
        F: FnMut(usize, usize) -> Option<Vec<u8>>,
        Q: FnMut() -> Result<i64, String>,
    {
        // COPY/COMPUTE submissions and unrelated unknown lists must not gain a new KCD2 gate.
        if !self.has_kcd2_pre_execute_work(lists) {
            return Ok(());
        }
        let already_prepared = lists.iter().any(|raw| {
            self.lists.get(raw).is_some_and(|list| {
                list.pending
                    .iter()
                    .any(|draw| draw.kcd2_pre_execute_prepared)
            })
        });
        for raw in lists {
            let Some(list) = self.lists.get_mut(raw) else {
                continue;
            };
            for draw in &mut list.pending {
                if !draw.kcd2_pre_execute_sources.is_empty() {
                    draw.kcd2_snapshots.clear();
                    draw.kcd2_pre_execute_prepared = false;
                    draw.kcd2_pre_execute_queue = None;
                }
            }
        }
        if already_prepared {
            return Err("KCD2 source was prepared again before its submission completed".into());
        }
        if let Some(error) = lists
            .iter()
            .find_map(|raw| self.kcd2_pre_execute_errors.get(raw))
        {
            return Err(error.clone());
        }
        let queue = direct_queue.ok_or("KCD2 pre-Execute source queue is unknown/not DIRECT")?;
        let mut prepared = Vec::new();
        for raw in lists {
            let Some(list) = self.lists.get(raw) else {
                continue;
            };
            for (index, draw) in list.pending.iter().enumerate() {
                if draw.kcd2_pre_execute_sources.is_empty() {
                    continue;
                }
                if draw.list_tag != list.list_tag
                    || draw.list_generation != list.list_generation
                    || self.selected_depth != Some(draw.selected_depth)
                {
                    return Err("KCD2 pre-Execute Draw/list/DSV identity changed".into());
                }
                let mut snapshots = Vec::new();
                for source in &draw.kcd2_pre_execute_sources {
                    self.validate_kcd2_pre_execute_source(source)?;
                    let binding = &source.binding;
                    let resource = &self.resources[&binding.resource_raw];
                    let bytes = if let Some(address) = self.mapped_read_address(
                        binding.resource_raw,
                        binding.resource_offset,
                        ROOT_CBV_SNAPSHOT_BYTES,
                    ) {
                        let first = read_bytes(address, ROOT_CBV_SNAPSHOT_BYTES)
                            .ok_or("KCD2 pre-Execute current mapped bytes are unreadable")?;
                        let second = read_bytes(address, ROOT_CBV_SNAPSHOT_BYTES)
                            .ok_or("KCD2 pre-Execute verification read failed")?;
                        if first.len() != ROOT_CBV_SNAPSHOT_BYTES || first != second {
                            return Err(
                                "KCD2 pre-Execute current source is incomplete/unstable".into()
                            );
                        }
                        first
                    } else {
                        let snapshot = resource
                            .unmap_snapshots
                            .get(&binding.resource_offset)
                            .filter(|snapshot| {
                                snapshot.content_generation == resource.content_generation
                                    && snapshot.bytes.len() >= ROOT_CBV_SNAPSHOT_BYTES
                            })
                            .ok_or(
                                "KCD2 pre-Execute source has no current mapping/exact Unmap bytes",
                            )?;
                        snapshot.bytes[..ROOT_CBV_SNAPSHOT_BYTES].to_vec()
                    };
                    let qpc = sample_qpc()?;
                    if qpc <= 0 {
                        return Err("KCD2 pre-Execute source QPC is not positive".into());
                    }
                    snapshots.push(Kcd2DirectRootSnapshot {
                        sample_qpc: qpc,
                        root_signature_sha256: source.root_signature_sha256.clone(),
                        root_parameter_index: binding.root_parameter_index,
                        pass_ordinal: source.pass_ordinal,
                        draw_ordinal: source.draw_ordinal,
                        bytes,
                        viewport: source.viewport,
                    });
                }
                prepared.push((*raw, index, snapshots));
            }
        }
        for (raw, index, snapshots) in prepared {
            self.snapshot.kcd2_direct_snapshots = self
                .snapshot
                .kcd2_direct_snapshots
                .saturating_add(snapshots.len() as u64);
            let draw = &mut self.lists.get_mut(&raw).unwrap().pending[index];
            draw.kcd2_snapshots = snapshots;
            draw.kcd2_pre_execute_prepared = true;
            draw.kcd2_pre_execute_queue = Some(queue);
        }
        Ok(())
    }

    fn validate_kcd2_prepared_submission(
        &self,
        queue: QueueId,
        lists: &[usize],
    ) -> Result<(), String> {
        if let Some(error) = lists
            .iter()
            .find_map(|raw| self.kcd2_pre_execute_errors.get(raw))
        {
            return Err(error.clone());
        }
        for raw in lists {
            let Some(list) = self.lists.get(raw) else {
                continue;
            };
            for draw in &list.pending {
                if draw.kcd2_pre_execute_sources.is_empty() {
                    continue;
                }
                if !draw.kcd2_pre_execute_prepared
                    || draw.kcd2_pre_execute_queue != Some(queue)
                    || draw.kcd2_snapshots.len() != draw.kcd2_pre_execute_sources.len()
                    || (draw.list_tag, draw.list_generation)
                        != (list.list_tag, list.list_generation)
                    || self.selected_depth != Some(draw.selected_depth)
                    || draw.kcd2_snapshots.iter().any(|snapshot| {
                        snapshot.bytes.len() != ROOT_CBV_SNAPSHOT_BYTES || snapshot.sample_qpc <= 0
                    })
                {
                    return Err("KCD2 v3 submission was not fully prepared or Draw/list/DSV/queue identity changed".into());
                }
                for source in &draw.kcd2_pre_execute_sources {
                    self.validate_kcd2_pre_execute_source(source)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(crate) fn record_diagnostic_draw_weight(
        &mut self,
        raw: usize,
        ordinal: u32,
        weight: Arc<crate::cp2077_camera_diagnostics::DrawWeight>,
    ) {
        if let Some(list) = self.lists.get_mut(&raw) {
            if list.diagnostic_draw_weights.len() < MAX_PENDING_DRAWS {
                list.diagnostic_draw_weights.insert(ordinal, weight);
            } else {
                crate::cp2077_camera_diagnostics::note_loss();
            }
        } else {
            crate::cp2077_camera_diagnostics::note_loss();
        }
    }

    pub fn execute(&mut self, queue: QueueId, lists: &[usize]) {
        // Original Execute has already run. Validate the WHOLE v3 batch before moving any of
        // its sources to `submitted`; a valid alias/player must never mask a failed sibling.
        if let Err(error) = self.validate_kcd2_prepared_submission(queue, lists) {
            for raw in lists {
                if let Some(list) = self.lists.get_mut(raw) {
                    list.pending
                        .retain(|draw| draw.kcd2_pre_execute_sources.is_empty());
                }
            }
            self.block(&error);
            crate::kcd2_live_pose::record_submission_fault(error);
        }
        let execute_batch = self.next_execute_batch;
        self.submit_depth_boundaries(queue, execute_batch, lists);
        self.next_execute_batch = self.next_execute_batch.saturating_add(1);
        for (list_index_in_batch, raw) in lists.iter().enumerate() {
            let list_index_in_batch = u32::try_from(list_index_in_batch).unwrap_or(u32::MAX);
            let Some(list) = self.lists.get_mut(raw) else {
                continue;
            };
            list.exact_cbv_copy_requests.clear();
            list.copy_buffer_spans.clear();
            list.copy_span_overflow = false;
            list.deferred_copy_draws.clear();
            let pending = std::mem::take(&mut list.pending);
            for mut draw in pending {
                let before = draw.matrices.len();
                let copy_queue_rejections = draw
                    .matrices
                    .iter()
                    .filter(|matrix| {
                        matrix.content_source == BufferContentSource::CopyBufferRegionShadow
                            && matrix.copy_shadow_queue != Some(queue)
                    })
                    .count();
                self.snapshot.exact_cbv_copy_shadow_queue_rejections = self
                    .snapshot
                    .exact_cbv_copy_shadow_queue_rejections
                    .saturating_add(u64::try_from(copy_queue_rejections).unwrap_or(u64::MAX));
                draw.matrices.retain(|matrix| {
                    let descriptor_current =
                        matrix
                            .descriptor_identity
                            .is_none_or(|(handle, generation)| {
                                self.cbv_descriptor_store
                                    .get(&handle)
                                    .is_some_and(|descriptor| descriptor.generation == generation)
                            });
                    let content_current =
                        self.resources
                            .get(&matrix.resource_raw)
                            .is_some_and(|resource| {
                                resource.content_generation == matrix.resource_content_generation
                                    && match matrix.content_source {
                                        BufferContentSource::LiveMapped => {
                                            resource.cpu_address().is_some()
                                        }
                                        BufferContentSource::UploadUnmapSnapshot => resource
                                            .unmap_snapshots
                                            .get(&matrix.cbv_resource_offset)
                                            .is_some_and(|snapshot| {
                                                snapshot.content_generation
                                                    == matrix.resource_content_generation
                                            }),
                                        BufferContentSource::CopyBufferRegionShadow => matrix
                                            .exact_copy_request
                                            .as_ref()
                                            .and_then(|request| {
                                                self.exact_cbv_copy_shadows.get(request)
                                            })
                                            .is_some_and(|shadow| {
                                                shadow.destination_content_generation
                                                    == matrix.resource_content_generation
                                                    && shadow.queue == queue
                                                    && matrix.copy_shadow_queue == Some(queue)
                                                    && shadow.source_content_generation
                                                        == self
                                                            .resources
                                                            .get(&shadow.source_resource_raw)
                                                            .map_or(u64::MAX, |resource| {
                                                                resource.content_generation
                                                            })
                                            }),
                                    }
                            });
                    descriptor_current && content_current
                });
                self.snapshot
                    .selected_table_matrix_candidates_invalidated_by_generation = self
                    .snapshot
                    .selected_table_matrix_candidates_invalidated_by_generation
                    .saturating_add((before - draw.matrices.len()) as u64);
                if !draw.matrices.is_empty() || !draw.kcd2_snapshots.is_empty() {
                    self.submitted.push(SubmittedDraw {
                        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                        diagnostic_draw_weight: list
                            .diagnostic_draw_weights
                            .remove(&draw.draw_ordinal)
                            .and_then(|weight| weight.snapshot()),
                        queue,
                        execute_batch,
                        list_index_in_batch,
                        draw,
                    });
                }
            }
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            if !list.diagnostic_draw_weights.is_empty() {
                // A staged diagnostic representative never reached submitted (decode,
                // generation validation, or bounded tracker failure). Never call that coverage.
                crate::cp2077_camera_diagnostics::note_loss();
                list.diagnostic_draw_weights.clear();
            }
        }
        self.exact_cbv_copy_shadows.clear();
        self.exact_cbv_copy_shadow_bytes = 0;
    }

    #[must_use]
    pub fn has_pending_for_lists(&self, lists: &[usize]) -> bool {
        lists.iter().any(|raw| {
            self.lists
                .get(raw)
                .is_some_and(|list| !list.pending.is_empty())
        })
    }

    pub fn present(&mut self, queue: QueueId, present_seq: u64, present_qpc: i64) {
        self.selected_draws_resolved_since_present = 0;
        if self
            .snapshot
            .local_archive
            .as_ref()
            .is_some_and(|s| s.frozen)
        {
            self.submitted.clear();
            return;
        }
        // The hook's first call is an interval baseline, not a frame identity. Discard anything
        // recorded before it without turning the expected sequence-zero bootstrap into a blocker.
        if present_seq == 0 {
            self.submitted.clear();
            return;
        }
        if present_qpc <= 0 {
            self.block("D3D12 render-camera Present identity is invalid");
            self.submitted.clear();
            return;
        }
        let selected_depth = self.selected_depth;
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        let mut diagnostic = crate::cp2077_camera_diagnostics::active()
            .then(|| crate::cp2077_camera_diagnostics::FamilyFrame::new(present_seq, present_qpc));
        let submitted = std::mem::take(&mut self.submitted);
        for submitted in submitted {
            if submitted.queue != queue {
                self.submitted.push(submitted);
                continue;
            }
            if Some(submitted.draw.selected_depth) != selected_depth {
                continue;
            }
            for snapshot in &submitted.draw.kcd2_snapshots {
                crate::kcd2_live_pose::observe_snapshot(
                    present_seq,
                    snapshot.sample_qpc,
                    &snapshot.root_signature_sha256,
                    snapshot.root_parameter_index,
                    snapshot.pass_ordinal,
                    snapshot.draw_ordinal,
                    0,
                    &snapshot.bytes,
                    snapshot.viewport,
                );
            }
            if submitted.draw.matrices.is_empty() {
                #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                if let Some(diagnostic) = diagnostic.as_mut() {
                    diagnostic.note_tracker_loss(true);
                }
                continue;
            }
            if self.snapshot.observations.len() >= MAX_PRESENT_OBSERVATIONS {
                self.snapshot.present_observation_overflow =
                    self.snapshot.present_observation_overflow.saturating_add(1);
                continue;
            }
            let all_descriptor_table = submitted
                .draw
                .matrices
                .iter()
                .all(|matrix| matrix.source == RootBindingSource::DescriptorTableBase);
            let all_live_mapped = submitted
                .draw
                .matrices
                .iter()
                .all(|matrix| matrix.content_source == BufferContentSource::LiveMapped);
            let all_upload_unmap_snapshot =
                submitted.draw.matrices.iter().all(|matrix| {
                    matrix.content_source == BufferContentSource::UploadUnmapSnapshot
                });
            let all_exact_copy_shadow =
                submitted.draw.matrices.iter().all(|matrix| {
                    matrix.content_source == BufferContentSource::CopyBufferRegionShadow
                });
            let physical_source = if all_descriptor_table && all_live_mapped {
                "game_mapped_root_signature_declared_cbv_at_selected_dsv_draw"
            } else if all_descriptor_table && all_upload_unmap_snapshot {
                "upload_unmap_snapshot_root_signature_declared_cbv_at_selected_dsv_draw"
            } else if all_descriptor_table && all_exact_copy_shadow {
                "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
            } else if submitted
                .draw
                .matrices
                .iter()
                .all(|matrix| matrix.source == RootBindingSource::Direct)
            {
                "direct_game_mapped_root_cbv_at_selected_dsv_draw"
            } else {
                "mixed_game_mapped_cbv_sources_at_selected_dsv_draw"
            };
            let mut matrices = submitted
                .draw
                .matrices
                .into_iter()
                .map(|candidate| self.materialize_matrix(candidate))
                .filter(|matrix| matrix.matrix_kind == "view" || matrix.matrix_kind == "projection")
                .collect::<Vec<_>>();
            // CP2077's exact camera CBV contains one FOV-75 projection plus several affine
            // matrices, including a history view and an overlapping false-positive immediately
            // before the projection. Select the first physical view in the same CBV before the
            // unique configured projection. Multiple configured projections or no preceding view
            // remain unselected and therefore cannot be admitted.
            if self.production.is_none() {
                // Apply the existing pair selection separately to each original binding. Two
                // aliases in one draw must not cause history/overlapping affine candidates to
                // leak back in simply because there are two copies of the configured projection.
                let mut bindings = BTreeMap::<(u64, u32, u64), Vec<usize>>::new();
                for (index, matrix) in matrices.iter().enumerate() {
                    bindings
                        .entry((
                            matrix.source_tag,
                            matrix.root_parameter_index,
                            matrix.cbv_resource_offset,
                        ))
                        .or_default()
                        .push(index);
                }
                let mut retain = BTreeSet::new();
                for indices in bindings.values() {
                    let projections = indices
                        .iter()
                        .copied()
                        .filter(|index| {
                            matrices[*index].matrix_kind == "projection"
                                && matrices[*index].configured_vertical_fov_matches == Some(true)
                        })
                        .collect::<Vec<_>>();
                    if let [projection_index] = projections.as_slice() {
                        let projection = &matrices[*projection_index];
                        let view = indices
                            .iter()
                            .copied()
                            .filter(|index| {
                                let matrix = &matrices[*index];
                                matrix.matrix_kind == "view"
                                    && matrix.matrix_offset < projection.matrix_offset
                                    && matrix.physical_source == projection.physical_source
                                    && matrix.copy_receipt == projection.copy_receipt
                            })
                            .min_by_key(|index| matrices[*index].matrix_offset);
                        if let Some(view) = view {
                            retain.extend([view, *projection_index]);
                            continue;
                        }
                    }
                    // Preserve unresolved bindings for the existing ambiguity/refusal gates.
                    retain.extend(indices.iter().copied());
                }
                matrices = matrices
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, matrix)| retain.contains(&index).then_some(matrix))
                    .collect();
            }
            // Do not deduplicate across draws. Equal camera bytes do not imply equal PSO
            // depth state or copy order, and every retained draw is independently audited.
            if matrices.is_empty() {
                #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                if let Some(diagnostic) = diagnostic.as_mut() {
                    diagnostic.note_tracker_loss(true);
                }
                continue;
            }
            let depth_copy = self
                .depth_copy_submitted
                .get(&(queue, present_seq))
                .copied();
            let admitted = depth_copy.is_some_and(|copy| {
                copy.precedes(
                    submitted.execute_batch,
                    submitted.list_index_in_batch,
                    submitted.draw.draw_ordinal,
                    submitted.draw.pass_ordinal,
                ) == Some(true)
            });
            self.snapshot.observations.push(D3d12RenderDrawObservation {
                pass_context: submitted.draw.pass_context,
                pipeline_depth_enable: submitted.draw.pipeline_depth.map(|state| state.enable),
                pipeline_depth_write: submitted.draw.pipeline_depth.map(|state| state.write),
                present_seq,
                present_qpc,
                queue_tag: queue.0,
                command_list_tag: submitted.draw.list_tag,
                command_list_generation: submitted.draw.list_generation,
                selected_depth_resource_tag: submitted.draw.selected_depth.resource.0,
                selected_depth_resource_epoch: submitted.draw.selected_depth.resource_epoch,
                selected_depth_descriptor_generation: submitted
                    .draw
                    .selected_depth
                    .descriptor_generation,
                draw_ordinal: submitted.draw.draw_ordinal,
                pass_ordinal: submitted.draw.pass_ordinal,
                draw_ordinal_in_pass: submitted.draw.draw_ordinal_in_pass,
                root_signature_sha256: submitted.draw.root_signature_sha256.clone(),
                execute_batch: submitted.execute_batch,
                list_index_in_batch: submitted.list_index_in_batch,
                depth_copy_execute_batch: depth_copy.map(|copy| copy.execute_batch),
                depth_copy_list_index_in_batch: depth_copy
                    .and_then(|copy| copy.list_index_in_batch),
                depth_copy_command_list_tag: depth_copy.and_then(|copy| copy.command_list_tag),
                depth_copy_draws_before_in_list: submitted
                    .draw
                    .pass_ordinal
                    .and(depth_copy.and_then(|copy| copy.draws_before_in_list)),
                copy_before_clear_submitted: admitted,
                selected_present_identity_passed: admitted,
                physical_source: physical_source.into(),
                matrices,
            });
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            if let Some(diagnostic) = diagnostic.as_mut()
                && let Some(draw) = self.snapshot.observations.last()
            {
                diagnostic.observe(draw, submitted.diagnostic_draw_weight);
            }
        }
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        if let Some(mut diagnostic) = diagnostic {
            diagnostic.note_tracker_loss(
                self.snapshot.present_observation_overflow > 0
                    || self
                        .snapshot
                        .selected_table_matrix_candidates_invalidated_by_generation
                        > 0
                    || !self.snapshot.blockers.is_empty(),
            );
            crate::cp2077_camera_diagnostics::submit_family(diagnostic);
        }
        let kcd2_depth_copied = self
            .depth_copy_submitted
            .contains_key(&(queue, present_seq));
        self.bound_present_copy = Some((present_seq, kcd2_depth_copied));
        self.depth_copy_submitted
            .retain(|(_, seq), _| *seq > present_seq);
        crate::kcd2_live_pose::finish_present(present_seq, kcd2_depth_copied);
    }

    /// Test shorthand: production marks carry the copy's list via
    /// `mark_depth_copy_submitted_in_list`. It places the copy after every list
    /// of the batch that just ran, which is what these tests' Execute-then-mark
    /// order means; admission refuses a copy whose position is unknown.
    #[cfg(test)]
    pub fn mark_depth_copy_submitted(&mut self, queue: QueueId, present_seq: u64) {
        self.mark_depth_copy_submitted_in_list(queue, present_seq, None, Some(u32::MAX), None);
    }

    /// Records the depth copy for one Present and where it was submitted:
    /// `copy_list` is the raw command list that carried the injected copy and
    /// `list_index_in_batch` its position in that Execute call. The backend calls
    /// this immediately after `execute`, so the batch that just ran is the one
    /// that carried the copy. `draws_before_in_list` is the HotListSlot counter
    /// sampled when recording the copy, never a count sampled at Execute time.
    pub fn mark_depth_copy_submitted_in_list(
        &mut self,
        queue: QueueId,
        present_seq: u64,
        copy_list: Option<usize>,
        list_index_in_batch: Option<u32>,
        draws_before_in_list: Option<u32>,
    ) {
        let copy = DepthCopySubmission {
            execute_batch: self.next_execute_batch.saturating_sub(1),
            list_index_in_batch,
            command_list_tag: copy_list
                .and_then(|raw| self.lists.get(&raw))
                .map(|list| list.list_tag),
            draws_before_in_list,
        };
        self.depth_copy_submitted.insert((queue, present_seq), copy);
        // Observations built before this mark (a late Execute) must carry the
        // same copy identity a Present-time build would have given them, not
        // only the two admission flags.
        for observation in self.snapshot.observations.iter_mut().filter(|observation| {
            observation.present_seq == present_seq && observation.queue_tag == queue.0
        }) {
            let admitted = copy.precedes(
                observation.execute_batch,
                observation.list_index_in_batch,
                observation.draw_ordinal,
                observation.pass_ordinal,
            ) == Some(true);
            observation.copy_before_clear_submitted = admitted;
            observation.selected_present_identity_passed = admitted;
            observation.depth_copy_execute_batch = Some(copy.execute_batch);
            observation.depth_copy_list_index_in_batch = copy.list_index_in_batch;
            observation.depth_copy_command_list_tag = copy.command_list_tag;
            observation.depth_copy_draws_before_in_list =
                observation.pass_ordinal.and(copy.draws_before_in_list);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> D3d12RenderCameraSnapshot {
        let mut snapshot = self.snapshot_without_admission();
        snapshot.refresh_authoring_admission();
        snapshot
    }

    pub(crate) fn exact_present_fields(
        &self,
        present_seq: u64,
        external_loss: bool,
    ) -> (bool, bool, bool) {
        self.snapshot
            .exact_present_fields(present_seq, external_loss)
    }

    pub(crate) fn snapshot_without_admission(&self) -> D3d12RenderCameraSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.depth_order.unsubmitted_boundaries = self
            .lists
            .values()
            .filter(|list| list.depth_boundaries.executions == 0)
            .flat_map(|list| list.depth_boundaries.events.iter().cloned())
            .collect();
        snapshot.tracked_buffer_resources = self.resources.len().try_into().unwrap_or(u32::MAX);
        let cache = self.gpuva_ownership_cache.borrow();
        snapshot.gpuva_ownership_cache_hits = cache.hits;
        snapshot.gpuva_ownership_cache_misses = cache.misses;
        snapshot.gpuva_ownership_resources_scanned = cache.resources_scanned;
        snapshot.gpuva_ownership_cache_invalidations = cache.invalidations;
        snapshot.gpuva_ownership_cache_evictions = cache.evictions;
        snapshot.directly_mapped_buffer_resources = self
            .resources
            .values()
            .filter(|resource| resource.cpu_address().is_some())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.tracked_upload_buffer_resources = self
            .resources
            .values()
            .filter(|resource| resource.upload_heap)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.unmapped_upload_buffer_resources = self
            .resources
            .values()
            .filter(|resource| resource.upload_heap && resource.cpu_address().is_none())
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.tracked_cbv_descriptors = self
            .cbv_descriptor_store
            .len()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.tracked_root_signatures =
            self.root_signatures.len().try_into().unwrap_or(u32::MAX);
        snapshot.tracked_root_signature_cbv_ranges = self
            .root_signatures
            .values()
            .flat_map(|signature| signature.cbv_ranges.values())
            .map(Vec::len)
            .sum::<usize>()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.upload_unmap_snapshot_live_ranges = self
            .resources
            .values()
            .map(|resource| resource.unmap_snapshots.len())
            .sum::<usize>()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.upload_unmap_snapshot_live_bytes = self
            .upload_unmap_snapshot_bytes
            .try_into()
            .unwrap_or(u64::MAX);
        snapshot.exact_cbv_copy_shadow_live_ranges = self
            .exact_cbv_copy_shadows
            .len()
            .try_into()
            .unwrap_or(u32::MAX);
        snapshot.exact_cbv_copy_shadow_live_bytes = self
            .exact_cbv_copy_shadow_bytes
            .try_into()
            .unwrap_or(u64::MAX);
        snapshot
    }

    #[must_use]
    pub fn mapped_read_address(
        &self,
        resource_raw: usize,
        resource_offset: u64,
        bytes: usize,
    ) -> Option<usize> {
        let resource = self.resources.get(&resource_raw)?;
        let end = resource_offset.checked_add(bytes as u64)?;
        (end <= resource.buffer_bytes).then(|| {
            resource
                .cpu_address()?
                .checked_add(usize::try_from(resource_offset).ok()?)
        })?
    }

    #[must_use]
    pub fn mapped_ranges(&self) -> BTreeMap<usize, (usize, u64)> {
        self.resources
            .iter()
            .filter_map(|(raw, resource)| {
                resource
                    .cpu_address()
                    .map(|address| (*raw, (address, resource.buffer_bytes)))
            })
            .collect()
    }

    /// Only exact selected CBV sources are needed at Execute. Avoid an allocation and full-scene
    /// resource traversal on every game submission (including those with no camera requests).
    pub fn exact_copy_mapped_ranges(&self, lists: &[usize]) -> BTreeMap<usize, (usize, u64)> {
        lists
            .iter()
            .filter_map(|raw| self.lists.get(raw))
            .flat_map(|list| list.exact_cbv_copy_requests.iter())
            .filter_map(|request| {
                let resource = self.resources.get(&request.source_resource_raw)?;
                Some((
                    request.source_resource_raw,
                    (resource.cpu_address()?, resource.buffer_bytes),
                ))
            })
            .collect()
    }

    /// Bounded hints only: all actual list/copy/descriptor/resource checks still run in prepare
    /// after reacquiring the guard. Stale hints can waste a query but cannot admit stale bytes.
    pub(crate) fn exact_copy_query_hints(
        &self,
        lists: &[usize],
        capacity: usize,
    ) -> Vec<ObserverMappedQuery> {
        let mut hints = Vec::new();
        for list in lists.iter().filter_map(|raw| self.lists.get(raw)) {
            for request in &list.exact_cbv_copy_requests {
                if hints.len() >= capacity {
                    return hints;
                }
                let Some(resource) = self.resources.get(&request.source_resource_raw) else {
                    continue;
                };
                if resource.source_tag != request.source_source_tag
                    || resource.content_generation != request.source_content_generation
                {
                    continue;
                }
                let Some(base) = resource.cpu_address() else {
                    continue;
                };
                let Some(owner) = self.observer_mapped_identity(request.source_resource_raw, base)
                else {
                    continue;
                };
                let Some(offset) = usize::try_from(request.source_offset).ok() else {
                    continue;
                };
                let Some(address) = base.checked_add(offset) else {
                    continue;
                };
                if !request
                    .source_offset
                    .checked_add(request.bytes as u64)
                    .is_some_and(|end| end <= owner.bytes)
                    || address.checked_add(request.bytes).is_none()
                {
                    continue;
                }
                let hint = ObserverMappedQuery {
                    owner,
                    address,
                    bytes: request.bytes,
                };
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
            }
        }
        hints
    }

    pub(crate) fn observer_mapped_identity(
        &self,
        raw: usize,
        base: usize,
    ) -> Option<ObserverMappedIdentity> {
        let resource = self.resources.get(&raw)?;
        // register_observer_mapping is called only after a successful original Map while an
        // owned COM reference is held. That reference moves to observer_upload_mappings,
        // which is retained by ACTIVE_BACKEND and has no removal path. Do not infer ownership
        // from map_count: Release forwards to the runtime before tracker retirement.
        (resource.upload_heap
            && base != 0
            && resource.observer_mapped_cpu_address == Some(base)
            && resource.cpu_address() == Some(base))
        .then_some(ObserverMappedIdentity {
            resource_raw: raw,
            source_tag: resource.source_tag,
            content_generation: resource.content_generation,
            base,
            bytes: resource.buffer_bytes,
        })
    }

    fn gpuva_resource_matches(&self, gpu_va: u64) -> (Option<usize>, usize) {
        let mut cache = self.gpuva_ownership_cache.borrow_mut();
        if let Some(result) = cache.entries.get(&gpu_va).copied() {
            cache.hits = cache.hits.saturating_add(1);
            return result;
        }
        cache.misses = cache.misses.saturating_add(1);
        cache.resources_scanned = cache
            .resources_scanned
            .saturating_add(self.resources.len() as u64);
        let mut matches = self.resources.iter().filter(|(_, resource)| {
            resource
                .gpu_virtual_address
                .checked_add(resource.buffer_bytes)
                .is_some_and(|end| (resource.gpu_virtual_address..end).contains(&gpu_va))
        });
        let first = matches.next().map(|(&raw, _)| raw);
        let count = usize::from(first.is_some()) + matches.count();
        let result = (first, count);
        // Cache ONLY ownership/ambiguity, including misses. Registration/release invalidate
        // the whole bounded cache. Map/content generations and bytes are checked live below.
        if cache.entries.len() >= 256 {
            cache.entries.clear();
            cache.evictions = cache.evictions.saturating_add(1);
        }
        cache.entries.insert(gpu_va, result);
        result
    }

    fn resolve_binding(&self, root_parameter_index: u32, gpu_va: u64) -> Option<BufferBinding> {
        let (raw, count) = self.gpuva_resource_matches(gpu_va);
        if count != 1 {
            return None;
        }
        let raw = raw?;
        let resource = self.resources.get(&raw)?;
        resource.cpu_address()?;
        Some(BufferBinding {
            source_tag: resource.source_tag,
            resource_raw: raw,
            resource_offset: gpu_va - resource.gpu_virtual_address,
            root_parameter_index,
            source: RootBindingSource::Direct,
            declared_bytes: None,
            stable_descriptor: None,
            descriptor_identity: None,
            content_source: BufferContentSource::LiveMapped,
            content_generation: resource.content_generation,
            copy_shadow_queue: None,
            pending_copy_request: None,
        })
    }

    fn resolve_descriptor_gpuva(
        &self,
        root_parameter_index: u32,
        gpu_va: u64,
        snapshot_bytes: usize,
    ) -> DescriptorGpuVaResolution {
        let (raw, count) = self.gpuva_resource_matches(gpu_va);
        if count == 0 {
            return DescriptorGpuVaResolution::WithoutResource;
        }
        if count != 1 {
            return DescriptorGpuVaResolution::ResourceAmbiguous;
        }
        let Some(resource) = raw.and_then(|raw| self.resources.get(&raw)) else {
            return DescriptorGpuVaResolution::WithoutResource;
        };
        let raw = raw.unwrap();
        let resource_offset = gpu_va - resource.gpu_virtual_address;
        let (content_source, copy_shadow_queue) = if resource.cpu_address().is_some() {
            (BufferContentSource::LiveMapped, None)
        } else if resource.upload_heap {
            let Some(snapshot) = resource.unmap_snapshots.get(&resource_offset) else {
                return DescriptorGpuVaResolution::UploadSnapshotMissing;
            };
            if snapshot.content_generation != resource.content_generation
                || snapshot.bytes.len() < snapshot_bytes
            {
                return DescriptorGpuVaResolution::UploadSnapshotMissing;
            }
            (BufferContentSource::UploadUnmapSnapshot, None)
        } else {
            return DescriptorGpuVaResolution::ResourceUnmapped {
                raw,
                offset: resource_offset,
                content_generation: resource.content_generation,
            };
        };
        DescriptorGpuVaResolution::Unique(BufferBinding {
            source_tag: resource.source_tag,
            resource_raw: raw,
            resource_offset,
            root_parameter_index,
            source: RootBindingSource::DescriptorTableBase,
            declared_bytes: None,
            stable_descriptor: None,
            descriptor_identity: None,
            content_source,
            content_generation: resource.content_generation,
            copy_shadow_queue,
            pending_copy_request: None,
        })
    }

    fn resolve_descriptor_table_binding(
        &self,
        list_raw: usize,
        root_parameter_index: u32,
        gpu_handle: u64,
        bound_heaps: [usize; 2],
        root_signature: usize,
    ) -> DescriptorTableResolution {
        let mut resolution = DescriptorTableResolution::default();
        let Some(signature) = self.root_signatures.get(&root_signature) else {
            resolution.root_parameter_without_declared_cbv_ranges = 1;
            return resolution;
        };
        let Some(ranges) = signature.cbv_ranges.get(&root_parameter_index) else {
            resolution.root_parameter_without_declared_cbv_ranges = 1;
            return resolution;
        };
        let root_signature_sha256 = signature.sha256.clone();

        if bound_heaps.iter().all(|raw| *raw == 0) {
            resolution.no_bound_descriptor_heap = 1;
            return resolution;
        }
        let tracked_heaps: Vec<_> = bound_heaps
            .into_iter()
            .filter_map(|raw| self.descriptor_heaps.get(&raw))
            .filter(|heap| heap.shader_visible && heap.gpu_start != 0)
            .collect();
        if tracked_heaps.is_empty() {
            resolution.bound_descriptor_heap_untracked = 1;
            return resolution;
        }

        let mut saw_misaligned = false;
        let mut matching_heaps = Vec::new();
        for heap in tracked_heaps {
            if gpu_handle < heap.gpu_start {
                continue;
            }
            let byte_offset = gpu_handle - heap.gpu_start;
            let increment = u64::from(heap.increment);
            if !byte_offset.is_multiple_of(increment) {
                saw_misaligned = true;
                continue;
            }
            let table_base_index = byte_offset / increment;
            if table_base_index >= u64::from(heap.descriptor_count) {
                continue;
            }
            matching_heaps.push((heap, table_base_index));
        }
        if matching_heaps.is_empty() {
            if saw_misaligned {
                resolution.table_base_misaligned = 1;
            } else {
                resolution.table_base_outside_bound_heap = 1;
            }
            return resolution;
        }
        if matching_heaps.len() != 1 {
            resolution.table_base_ambiguous_bound_heap = 1;
            return resolution;
        }
        let (heap, table_base_index) = matching_heaps[0];

        for range in ranges {
            for descriptor_offset in 0..range.descriptor_count {
                if let Some((production, _)) = &self.production
                    && production.holstered_camera.is_some()
                    && !production.view_sources.iter().any(|view| {
                        let source = &view.source.descriptor_table;
                        root_signature_sha256.as_deref()
                            == Some(source.root_signature_sha256.as_str())
                            && source.root_parameter_index == root_parameter_index
                            && source.descriptor_range_index == range.descriptor_range_index
                            && source.descriptor_offset_in_range == descriptor_offset
                            && source.descriptor_count == range.descriptor_count
                            && source.base_shader_register == range.base_shader_register
                            && source.register_space == range.register_space
                    })
                {
                    // Apply the existing stable-source filter before GPUVA
                    // lookup. All authored aliases still get fully resolved,
                    // then checked for CBV size, offset, bytes and provenance.
                    continue;
                }
                let Some(index) = table_base_index
                    .checked_add(u64::from(range.table_offset))
                    .and_then(|value| value.checked_add(u64::from(descriptor_offset)))
                else {
                    resolution.declared_descriptor_outside_heap = resolution
                        .declared_descriptor_outside_heap
                        .saturating_add(1);
                    continue;
                };
                if index >= u64::from(heap.descriptor_count) {
                    resolution.declared_descriptor_outside_heap = resolution
                        .declared_descriptor_outside_heap
                        .saturating_add(1);
                    continue;
                }
                let Some(cpu_handle) = heap
                    .cpu_start
                    .checked_add(index.saturating_mul(u64::from(heap.increment)))
                else {
                    resolution.declared_descriptor_outside_heap = resolution
                        .declared_descriptor_outside_heap
                        .saturating_add(1);
                    continue;
                };
                let Some(descriptor) = self.cbv_descriptor_store.get(&cpu_handle) else {
                    resolution.descriptor_not_current_cbv =
                        resolution.descriptor_not_current_cbv.saturating_add(1);
                    continue;
                };
                let required_snapshot_bytes = usize::try_from(
                    u64::from(descriptor.size_in_bytes).min(ROOT_CBV_SNAPSHOT_BYTES as u64),
                )
                .unwrap_or(ROOT_CBV_SNAPSHOT_BYTES);
                let mut binding = match self.resolve_descriptor_gpuva(
                    root_parameter_index,
                    descriptor.gpu_virtual_address,
                    required_snapshot_bytes,
                ) {
                    DescriptorGpuVaResolution::Unique(binding) => binding,
                    DescriptorGpuVaResolution::WithoutResource => {
                        resolution.cbv_gpuva_without_resource =
                            resolution.cbv_gpuva_without_resource.saturating_add(1);
                        continue;
                    }
                    DescriptorGpuVaResolution::ResourceUnmapped {
                        raw,
                        offset,
                        content_generation,
                    } => {
                        let exact_pending_copy = self.lists.get(&list_raw).and_then(|list| {
                            if list.copy_span_overflow {
                                return None;
                            }
                            let bytes = required_snapshot_bytes;
                            let end = offset.checked_add(bytes as u64)?;
                            let (copy_ordinal, span) =
                                list.copy_buffer_spans.iter().enumerate().rev().find(
                                    |(_, span)| {
                                        span.destination_resource_raw == raw
                                            && span.destination_offset < end
                                            && span.destination_offset.saturating_add(span.bytes)
                                                > offset
                                    },
                                )?;
                            // An unsupported/partial overwrite invalidates the whole matrix range.
                            // We deliberately do not assemble bytes from several physical copies.
                            if !span.readable
                                || span.destination_content_generation != content_generation
                                || span.destination_source_tag
                                    != self.resources.get(&raw)?.source_tag
                                || span.list_generation != list.list_generation
                                || offset < span.destination_offset
                                || end > span.destination_offset.checked_add(span.bytes)?
                            {
                                return None;
                            }
                            Some(ExactCbvCopyRequest {
                                list_raw,
                                list_tag: list.list_tag,
                                list_generation: list.list_generation,
                                copy_ordinal,
                                destination_resource_raw: raw,
                                destination_source_tag: span.destination_source_tag,
                                destination_content_generation: content_generation,
                                destination_offset: offset,
                                source_resource_raw: span.source_resource_raw,
                                source_source_tag: span.source_source_tag,
                                source_content_generation: span.source_content_generation,
                                source_offset: span
                                    .source_offset
                                    .checked_add(offset - span.destination_offset)?,
                                descriptor_identity: (cpu_handle, descriptor.generation),
                                bytes,
                            })
                        });
                        let Some(exact_pending_copy) = exact_pending_copy else {
                            resolution.cbv_gpuva_resource_unmapped =
                                resolution.cbv_gpuva_resource_unmapped.saturating_add(1);
                            continue;
                        };
                        BufferBinding {
                            source_tag: self.resources[&raw].source_tag,
                            resource_raw: raw,
                            resource_offset: offset,
                            root_parameter_index,
                            source: RootBindingSource::DescriptorTableBase,
                            declared_bytes: None,
                            stable_descriptor: None,
                            descriptor_identity: None,
                            content_source: BufferContentSource::CopyBufferRegionShadow,
                            content_generation,
                            copy_shadow_queue: None,
                            pending_copy_request: Some(Arc::new(exact_pending_copy)),
                        }
                    }
                    DescriptorGpuVaResolution::UploadSnapshotMissing => {
                        resolution.cbv_gpuva_upload_snapshot_missing = resolution
                            .cbv_gpuva_upload_snapshot_missing
                            .saturating_add(1);
                        continue;
                    }
                    DescriptorGpuVaResolution::ResourceAmbiguous => {
                        resolution.cbv_gpuva_resource_ambiguous =
                            resolution.cbv_gpuva_resource_ambiguous.saturating_add(1);
                        continue;
                    }
                };
                let Some(available) =
                    self.resources
                        .get(&binding.resource_raw)
                        .and_then(|resource| {
                            resource.buffer_bytes.checked_sub(binding.resource_offset)
                        })
                else {
                    resolution.cbv_gpuva_without_resource =
                        resolution.cbv_gpuva_without_resource.saturating_add(1);
                    continue;
                };
                if u64::from(descriptor.size_in_bytes) > available {
                    resolution.cbv_size_outside_resource =
                        resolution.cbv_size_outside_resource.saturating_add(1);
                    continue;
                }
                if binding.content_source == BufferContentSource::CopyBufferRegionShadow
                    && binding.copy_shadow_queue.is_some()
                    && binding
                        .pending_copy_request
                        .as_deref()
                        .and_then(|request| self.exact_cbv_copy_shadows.get(request))
                        .is_none_or(|shadow| {
                            shadow.descriptor_identity != (cpu_handle, descriptor.generation)
                        })
                {
                    resolution.descriptor_not_current_cbv =
                        resolution.descriptor_not_current_cbv.saturating_add(1);
                    continue;
                }
                binding.declared_bytes = Some(descriptor.size_in_bytes);
                binding.stable_descriptor =
                    root_signature_sha256
                        .as_ref()
                        .map(|sha256| D3d12DescriptorTableBinding {
                            root_signature_sha256: sha256.clone(),
                            root_parameter_index,
                            descriptor_range_index: range.descriptor_range_index,
                            descriptor_offset_in_range: descriptor_offset,
                            descriptor_count: range.descriptor_count,
                            base_shader_register: range.base_shader_register,
                            register_space: range.register_space,
                        });
                binding.descriptor_identity = Some((cpu_handle, descriptor.generation));
                resolution.resolved_descriptors = resolution.resolved_descriptors.saturating_add(1);
                if binding.content_source == BufferContentSource::UploadUnmapSnapshot {
                    resolution.resolved_from_upload_unmap_snapshot = resolution
                        .resolved_from_upload_unmap_snapshot
                        .saturating_add(1);
                } else if binding.content_source == BufferContentSource::CopyBufferRegionShadow {
                    resolution.resolved_from_exact_cbv_copy_shadow = resolution
                        .resolved_from_exact_cbv_copy_shadow
                        .saturating_add(1);
                }
                resolution.bindings.push(binding);
            }
        }
        resolution
    }

    fn record_descriptor_table_resolution(&mut self, resolution: &DescriptorTableResolution) {
        let snapshot = &mut self.snapshot;
        snapshot.selected_table_root_parameter_without_declared_cbv_ranges = snapshot
            .selected_table_root_parameter_without_declared_cbv_ranges
            .saturating_add(resolution.root_parameter_without_declared_cbv_ranges);
        snapshot.selected_table_no_bound_descriptor_heap = snapshot
            .selected_table_no_bound_descriptor_heap
            .saturating_add(resolution.no_bound_descriptor_heap);
        snapshot.selected_table_bound_descriptor_heap_untracked = snapshot
            .selected_table_bound_descriptor_heap_untracked
            .saturating_add(resolution.bound_descriptor_heap_untracked);
        snapshot.selected_table_base_outside_bound_heap = snapshot
            .selected_table_base_outside_bound_heap
            .saturating_add(resolution.table_base_outside_bound_heap);
        snapshot.selected_table_base_misaligned = snapshot
            .selected_table_base_misaligned
            .saturating_add(resolution.table_base_misaligned);
        snapshot.selected_table_base_ambiguous_bound_heap = snapshot
            .selected_table_base_ambiguous_bound_heap
            .saturating_add(resolution.table_base_ambiguous_bound_heap);
        snapshot.selected_table_declared_descriptor_outside_heap = snapshot
            .selected_table_declared_descriptor_outside_heap
            .saturating_add(resolution.declared_descriptor_outside_heap);
        snapshot.selected_table_descriptor_not_current_cbv = snapshot
            .selected_table_descriptor_not_current_cbv
            .saturating_add(resolution.descriptor_not_current_cbv);
        snapshot.selected_table_cbv_gpuva_without_resource = snapshot
            .selected_table_cbv_gpuva_without_resource
            .saturating_add(resolution.cbv_gpuva_without_resource);
        snapshot.selected_table_cbv_gpuva_resource_unmapped = snapshot
            .selected_table_cbv_gpuva_resource_unmapped
            .saturating_add(resolution.cbv_gpuva_resource_unmapped);
        snapshot.selected_table_cbv_gpuva_upload_snapshot_missing = snapshot
            .selected_table_cbv_gpuva_upload_snapshot_missing
            .saturating_add(resolution.cbv_gpuva_upload_snapshot_missing);
        snapshot.selected_table_cbv_gpuva_resource_ambiguous = snapshot
            .selected_table_cbv_gpuva_resource_ambiguous
            .saturating_add(resolution.cbv_gpuva_resource_ambiguous);
        snapshot.selected_table_cbv_size_outside_resource = snapshot
            .selected_table_cbv_size_outside_resource
            .saturating_add(resolution.cbv_size_outside_resource);
        snapshot.selected_table_resolved_descriptors = snapshot
            .selected_table_resolved_descriptors
            .saturating_add(resolution.resolved_descriptors);
        snapshot.selected_table_resolved_from_upload_unmap_snapshot = snapshot
            .selected_table_resolved_from_upload_unmap_snapshot
            .saturating_add(resolution.resolved_from_upload_unmap_snapshot);
        snapshot.selected_table_resolved_from_exact_cbv_copy_shadow = snapshot
            .selected_table_resolved_from_exact_cbv_copy_shadow
            .saturating_add(resolution.resolved_from_exact_cbv_copy_shadow);
    }

    fn scan_resolved_bindings<F>(
        &mut self,
        list_raw: usize,
        draw: DrawLocation<'_>,
        selected_depth: DsvIdentity,
        bindings: Vec<BufferBinding>,
        read_bytes: &mut F,
    ) -> bool
    where
        F: FnMut(&Self, usize, u64, usize) -> Option<Vec<u8>>,
    {
        if crate::kcd2_live_pose::requires_pre_execute_snapshot() {
            return match self.stage_kcd2_pre_execute_draw(list_raw, draw, selected_depth, bindings)
            {
                Ok(()) => true,
                Err(error) => {
                    self.block(&error);
                    self.kcd2_pre_execute_errors
                        .entry(list_raw)
                        .or_insert(error);
                    false
                }
            };
        }
        let mut matrices = Vec::new();
        let mut kcd2_snapshots = Vec::new();
        let mut saw_unmapped = false;
        for binding in bindings {
            let available = self
                .resources
                .get(&binding.resource_raw)
                .map_or(0, |resource| {
                    resource
                        .buffer_bytes
                        .saturating_sub(binding.resource_offset)
                });
            let declared = binding.declared_bytes.map_or(available, u64::from);
            let snapshot_bytes =
                usize::try_from(available.min(declared).min(ROOT_CBV_SNAPSHOT_BYTES as u64))
                    .unwrap_or(ROOT_CBV_SNAPSHOT_BYTES);
            if snapshot_bytes < 64 {
                continue;
            }
            let bytes = match binding.content_source {
                BufferContentSource::LiveMapped => {
                    let Some(bytes) = read_bytes(
                        self,
                        binding.resource_raw,
                        binding.resource_offset,
                        snapshot_bytes,
                    ) else {
                        saw_unmapped = true;
                        continue;
                    };
                    bytes
                }
                BufferContentSource::UploadUnmapSnapshot => {
                    let Some(snapshot) = self
                        .resources
                        .get(&binding.resource_raw)
                        .and_then(|resource| {
                            resource
                                .unmap_snapshots
                                .get(&binding.resource_offset)
                                .filter(|snapshot| {
                                    snapshot.content_generation == resource.content_generation
                                })
                        })
                        .filter(|snapshot| snapshot.bytes.len() >= snapshot_bytes)
                    else {
                        saw_unmapped = true;
                        continue;
                    };
                    snapshot.bytes[..snapshot_bytes].to_vec()
                }
                BufferContentSource::CopyBufferRegionShadow => {
                    let Some(shadow) = binding
                        .pending_copy_request
                        .as_deref()
                        .and_then(|request| self.exact_cbv_copy_shadows.get(request))
                        .filter(|shadow| {
                            shadow.destination_content_generation == binding.content_generation
                                && shadow.bytes.len() >= snapshot_bytes
                        })
                    else {
                        saw_unmapped = true;
                        continue;
                    };
                    shadow.bytes[..snapshot_bytes].to_vec()
                }
            };
            if crate::kcd2_live_pose::enabled() {
                if binding.source == RootBindingSource::Direct
                    && bytes.len() >= ROOT_CBV_SNAPSHOT_BYTES
                    && let Some(root_signature_sha256) = draw.root_signature_sha256
                {
                    let sample_qpc = match crate::kcd2_live_pose::current_sample_qpc() {
                        Ok(value) => value,
                        Err(error) => {
                            crate::kcd2_live_pose::record_snapshot_qpc_fault(error);
                            continue;
                        }
                    };
                    kcd2_snapshots.push(Kcd2DirectRootSnapshot {
                        sample_qpc,
                        root_signature_sha256: root_signature_sha256
                            .strip_prefix("sha256:")
                            .unwrap_or(root_signature_sha256)
                            .to_owned(),
                        root_parameter_index: binding.root_parameter_index,
                        pass_ordinal: draw.pass_ordinal,
                        draw_ordinal: draw.ordinal_in_pass,
                        bytes: bytes[..ROOT_CBV_SNAPSHOT_BYTES].to_vec(),
                        viewport: self.lists.get(&list_raw).and_then(|list| list.viewport),
                    });
                    self.snapshot.kcd2_direct_snapshots =
                        self.snapshot.kcd2_direct_snapshots.saturating_add(1);
                }
                continue;
            }
            let candidates = if let Some((production, _)) = &self.production {
                let mut decode_failure = None;
                let candidates = production
                    .matrix_sources()
                    .filter(|source| {
                        binding.stable_descriptor.as_ref() == Some(&source.descriptor_table)
                            && binding.resource_offset == source.cbv_resource_offset
                            && binding.declared_bytes == Some(source.declared_cbv_bytes)
                    })
                    .filter_map(|source| {
                        let matrix = depth_lab::d3d12_camera::read_bound_camera_matrix(
                            production, source, &bytes,
                        );
                        if matrix.is_none()
                            && decode_failure.is_none()
                            && production.holstered_camera.as_ref().is_some_and(|p| {
                                p.schema == depth_lab::cp2077_bound_camera::ROW_VIEW_PROJECTION_POLICY
                            })
                        {
                            // One bounded, failed CPU snapshot is enough for exact offline
                            // diagnosis. Healthy draws allocate no extra diagnostic payload.
                            decode_failure = Some(format!(
                                "bound row-VP decode rejected root={} offset={} role={:?}; cbv_prefix={bytes:02x?}",
                                source.descriptor_table.root_parameter_index,
                                source.matrix_offset,
                                source.role,
                            ));
                        }
                        matrix.map(|matrix| depth_lab::camera::ScannedMatrix {
                            offset: source.matrix_offset as usize,
                            transpose: source.transpose,
                            matrix,
                            kind: match source.role {
                                MatrixRole::View => MatrixKind::View {
                                    translation: production.view_sources[0].translation,
                                },
                                MatrixRole::Projection => MatrixKind::Projection,
                            },
                        })
                    })
                    .collect::<Vec<_>>();
                if let Some(error) = decode_failure {
                    if !self
                        .snapshot
                        .blockers
                        .iter()
                        .any(|b| b.starts_with("bound row-VP decode rejected"))
                    {
                        self.block(&error);
                    }
                    return false;
                }
                candidates
            } else {
                scan_candidates(&bytes)
                    .into_iter()
                    .chain(scan_view_projection_pairs(&bytes))
                    .collect()
            };
            for candidate in candidates {
                if matrices.len() >= MAX_MATRIX_CANDIDATES_PER_DRAW {
                    self.snapshot.matrix_candidate_overflow =
                        self.snapshot.matrix_candidate_overflow.saturating_add(1);
                    break;
                }
                matrices.push(MatrixCandidate {
                    exact_copy_request: binding.pending_copy_request.as_deref().copied(),
                    copy_receipt: if binding.content_source
                        == BufferContentSource::CopyBufferRegionShadow
                    {
                        binding
                            .pending_copy_request
                            .as_deref()
                            .and_then(|request| self.exact_cbv_copy_shadows.get(request))
                            .map(|shadow| shadow.receipt.clone())
                    } else {
                        None
                    },
                    source_tag: binding.source_tag,
                    root_parameter_index: binding.root_parameter_index,
                    cbv_resource_offset: binding.resource_offset,
                    matrix_offset: candidate.offset as u32,
                    transpose: candidate.transpose,
                    matrix: candidate.matrix,
                    kind: candidate.kind,
                    source: binding.source,
                    content_source: binding.content_source,
                    resource_raw: binding.resource_raw,
                    resource_content_generation: binding.content_generation,
                    copy_shadow_queue: binding.copy_shadow_queue,
                    descriptor_identity: binding.descriptor_identity,
                    declared_bytes: binding.declared_bytes,
                    stable_descriptor: binding.stable_descriptor.clone(),
                });
            }
        }
        if saw_unmapped {
            self.snapshot.unmapped_root_cbv_rejections =
                self.snapshot.unmapped_root_cbv_rejections.saturating_add(1);
        }
        if matrices.is_empty() && kcd2_snapshots.is_empty() {
            return false;
        }
        self.snapshot.direct_mapped_draws_with_matrices = self
            .snapshot
            .direct_mapped_draws_with_matrices
            .saturating_add(1);
        let pending_count: usize = self.lists.values().map(|list| list.pending.len()).sum();
        if pending_count >= MAX_PENDING_DRAWS {
            self.snapshot.pending_draw_overflow =
                self.snapshot.pending_draw_overflow.saturating_add(1);
            return false;
        }
        if let Some(list) = self.lists.get_mut(&list_raw) {
            list.pending.push(PendingDraw {
                pass_context: draw.pass_context,
                pipeline_depth: draw.pipeline_depth,
                list_tag: list.list_tag,
                list_generation: list.list_generation,
                draw_ordinal: draw.global_ordinal,
                pass_ordinal: Some(draw.pass_ordinal),
                draw_ordinal_in_pass: Some(draw.ordinal_in_pass),
                root_signature_sha256: draw.root_signature_sha256.map(str::to_owned),
                selected_depth,
                matrices,
                kcd2_snapshots,
                kcd2_pre_execute_sources: Vec::new(),
                kcd2_pre_execute_prepared: false,
                kcd2_pre_execute_queue: None,
            });
            true
        } else {
            false
        }
    }

    fn materialize_matrix(&self, candidate: MatrixCandidate) -> D3d12RenderMatrixObservation {
        let (matrix_kind, translation_slot, determinant, projection) = match candidate.kind {
            MatrixKind::View { translation } => (
                "view",
                Some(match translation {
                    TranslationSlot::Row3 => "row3".to_string(),
                    TranslationSlot::Col3 => "col3".to_string(),
                }),
                Some(f64::from(determinant3(&candidate.matrix))),
                None,
            ),
            MatrixKind::Projection => (
                "projection",
                None,
                None,
                projection_from_matrix(&candidate.matrix),
            ),
            MatrixKind::Unknown => ("unknown", None, None, None),
        };
        let intrinsics = (candidate.kind == MatrixKind::Projection).then(|| {
            intrinsics_from_projection(
                &candidate.matrix,
                self.render_width as f32,
                self.render_height as f32,
            )
        });
        let vertical_fov = intrinsics.and_then(|value| {
            (value.fy > 0.0)
                .then(|| 2.0 * ((self.render_height as f64 * 0.5) / f64::from(value.fy)).atan())
        });
        let horizontal_fov = intrinsics.and_then(|value| {
            (value.fx > 0.0)
                .then(|| 2.0 * ((self.render_width as f64 * 0.5) / f64::from(value.fx)).atan())
        });
        let role = match candidate.kind {
            MatrixKind::View { .. } => Some(MatrixRole::View),
            MatrixKind::Projection => Some(MatrixRole::Projection),
            MatrixKind::Unknown => None,
        };
        let stable_source = candidate
            .stable_descriptor
            .clone()
            .zip(candidate.declared_bytes)
            .zip(role)
            .map(
                |((descriptor_table, declared_cbv_bytes), role)| D3d12MatrixSourceBinding {
                    descriptor_table,
                    cbv_resource_offset: candidate.cbv_resource_offset,
                    declared_cbv_bytes,
                    matrix_offset: candidate.matrix_offset,
                    encoding: MatrixEncoding::F32LittleEndian,
                    layout: MatrixLayout::RowMajor,
                    transpose: candidate.transpose,
                    role,
                    draw_relation: D3d12DrawRelation::ExactSelectedDepthDsvDraw,
                },
            );
        let stable_source = if let Some((production, _)) = &self.production {
            let expected = production.matrix_sources().find(|source| {
                Some(source.role) == role
                    && candidate.stable_descriptor.as_ref() == Some(&source.descriptor_table)
                    && source.matrix_offset == candidate.matrix_offset
            });
            stable_source
                .zip(expected)
                .and_then(|(mut actual, expected)| {
                    // The exact decoder used the authored layout. All physical fields still come
                    // from the observed descriptor/CBV and must agree before provenance is emitted.
                    actual.layout = expected.layout;
                    (actual == *expected).then_some(actual)
                })
        } else {
            stable_source
        };
        D3d12RenderMatrixObservation {
            copy_receipt: candidate.copy_receipt,
            source_tag: candidate.source_tag,
            root_parameter_index: candidate.root_parameter_index,
            cbv_resource_offset: candidate.cbv_resource_offset,
            matrix_offset: candidate.matrix_offset,
            transpose: candidate.transpose,
            matrix_kind: matrix_kind.into(),
            matrix: candidate.matrix,
            translation_slot,
            basis_determinant: determinant,
            clip_w_sign: (candidate.kind == MatrixKind::Projection).then(|| {
                if candidate.matrix[2][3].is_sign_negative() {
                    -1
                } else {
                    1
                }
            }),
            projection_a: projection.map(|value| value.a),
            projection_b: projection.map(|value| value.b),
            vertical_fov_degrees: vertical_fov.map(f64::to_degrees),
            horizontal_fov_degrees: horizontal_fov.map(f64::to_degrees),
            // The v1 field name predates the CP2077 physical observation. CP2077's native FOV
            // control corresponds to the horizontal projection angle at 16:9 (the exact game
            // matrix is 75.6 degrees for the configured value 75), not the 47.1-degree vertical
            // angle. Preserve the schema key but attest the actual game setting here.
            configured_vertical_fov_matches: horizontal_fov.map(|value| {
                (value.to_degrees() - self.expected_vertical_fov_degrees).abs() <= 1.0
            }),
            handedness_observation: match candidate.kind {
                MatrixKind::Projection if candidate.matrix[2][3].is_sign_negative() => {
                    "projection_clip_w_negative_camera_z".into()
                }
                MatrixKind::Projection => "projection_clip_w_positive_camera_z".into(),
                MatrixKind::View { .. } if determinant.is_some_and(|value| value < 0.0) => {
                    "view_basis_reflection".into()
                }
                MatrixKind::View { .. } => "view_basis_proper_rotation".into(),
                MatrixKind::Unknown => "unresolved".into(),
            },
            // A render matrix alone does not prove the game's world-axis mapping. Keep this
            // explicit so a visually plausible candidate cannot silently claim delivery axes.
            axis_mapping: "unresolved_requires_motion_calibration".into(),
            physical_source: match candidate.source {
                RootBindingSource::Direct => "direct_root_cbv",
                RootBindingSource::DescriptorTableBase
                    if candidate.content_source == BufferContentSource::UploadUnmapSnapshot =>
                {
                    "upload_unmap_snapshot_root_signature_declared_cbv"
                }
                RootBindingSource::DescriptorTableBase
                    if candidate.content_source == BufferContentSource::CopyBufferRegionShadow =>
                {
                    "copy_buffer_region_root_signature_declared_cbv"
                }
                RootBindingSource::DescriptorTableBase => "root_signature_declared_cbv",
            }
            .into(),
            stable_source,
        }
    }

    fn block(&mut self, message: &str) {
        if self.snapshot.blockers.len() < 64
            && !self.snapshot.blockers.iter().any(|entry| entry == message)
        {
            self.snapshot.blockers.push(message.into());
        }
    }
}

fn determinant3(matrix: &Mat4) -> f32 {
    let [a, b, c] = [matrix[0][0], matrix[0][1], matrix[0][2]];
    let [d, e, f] = [matrix[1][0], matrix[1][1], matrix[1][2]];
    let [g, h, i] = [matrix[2][0], matrix[2][1], matrix[2][2]];
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `draw_ordinal` and `pass_ordinal` are both list-local, so the only way to
    /// order two draws from different command lists is the execute batch and the
    /// list's position inside it. Pin that one `execute` call hands every list
    /// the same batch with ascending indices, that the next call advances the
    /// batch, and that a Present's depth copy records the batch that carried it.
    #[test]
    fn execute_batches_order_draws_across_command_lists() {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        let queue = QueueId(5);

        // Two lists in one submission, then a third in the next.
        tracker.register_list(0x133, 133);
        tracker.execute(queue, &[0x115, 0x133]);
        let first_batch = tracker.next_execute_batch - 1;
        tracker.mark_depth_copy_submitted_in_list(queue, 2749, Some(0x133), Some(1), Some(7));
        tracker.execute(queue, &[0x134]);
        let second_batch = tracker.next_execute_batch - 1;

        assert_eq!(
            second_batch,
            first_batch + 1,
            "each execute must advance the batch so cross-batch draws are ordered"
        );
        assert_eq!(
            tracker
                .depth_copy_submitted
                .get(&(queue, 2749))
                .map(|copy| copy.execute_batch),
            Some(first_batch),
            "the copy belongs to the batch that had just run, not the next one"
        );
        let copy = tracker.depth_copy_submitted[&(queue, 2749)];
        assert_eq!(copy.list_index_in_batch, Some(1));
        assert_eq!(copy.draws_before_in_list, Some(7));
        assert_eq!(
            copy.command_list_tag,
            Some(133),
            "the copy's list resolves to its registered tag"
        );
        // A Present with no copy stays absent rather than defaulting to a batch.
        assert_eq!(tracker.depth_copy_submitted.get(&(queue, 2750)), None);
    }

    /// The fourth 2026-09-13 run admitted 7 draws submitted five Execute batches
    /// after their Present's depth copy. Admission follows the recorded order:
    /// a later batch is refused, an earlier list in the copy's batch is
    /// admitted, and an order the fields cannot settle is refused.
    #[test]
    fn depth_copy_admits_only_draws_that_provably_precede_it() {
        let copy = DepthCopySubmission {
            execute_batch: 32_234,
            list_index_in_batch: Some(5),
            command_list_tag: Some(336),
            draws_before_in_list: Some(11),
        };
        // Other batches order by batch alone.
        assert_eq!(copy.precedes(32_239, 0, 1, Some(0)), Some(false));
        assert_eq!(copy.precedes(32_233, 9, 99, Some(0)), Some(true));
        // The copy's batch orders by list position.
        assert_eq!(copy.precedes(32_234, 4, 99, Some(0)), Some(true));
        assert_eq!(copy.precedes(32_234, 6, 1, Some(0)), Some(false));
        // The copy's own list orders by its recording stamp.
        assert_eq!(copy.precedes(32_234, 5, 11, Some(0)), Some(true));
        assert_eq!(copy.precedes(32_234, 5, 12, Some(0)), Some(false));
        // The legacy counter carries no pass and cannot use that stamp.
        assert_eq!(copy.precedes(32_234, 5, 1, None), None);
        // Without the copy's list position its own batch stays unordered.
        let unplaced = DepthCopySubmission {
            list_index_in_batch: None,
            ..copy
        };
        assert_eq!(unplaced.precedes(32_234, 0, 1, Some(0)), None);
        assert_eq!(unplaced.precedes(32_235, 0, 1, Some(0)), Some(false));
    }

    /// The 2026-09-10 run reported six Presents where two draws wrote the same
    /// camera slot with matrices that disagreed at the 1e-6 level. Nothing in
    /// the recorded fields could say whether those draws belonged to one view,
    /// so the report now carries each draw's pass and root signature. Pin that
    /// the descriptor-table path fills them in and the direct root-CBV path
    /// admits it cannot - a silent `Some(0)` there would fabricate provenance.
    #[test]
    fn draw_observations_carry_pass_provenance_or_admit_they_lack_it() {
        let table_draw = PendingDraw {
            pass_context: None,
            pipeline_depth: None,
            list_tag: 1,
            list_generation: 2,
            draw_ordinal: 9,
            pass_ordinal: Some(3),
            draw_ordinal_in_pass: Some(4),
            root_signature_sha256: Some("sha256:abc".into()),
            selected_depth: selected_depth(),
            matrices: Vec::new(),
            kcd2_snapshots: Vec::new(),
            kcd2_pre_execute_sources: Vec::new(),
            kcd2_pre_execute_prepared: false,
            kcd2_pre_execute_queue: None,
        };
        let direct_draw = PendingDraw {
            pass_context: None,
            pipeline_depth: None,
            pass_ordinal: None,
            draw_ordinal_in_pass: None,
            root_signature_sha256: None,
            ..table_draw.clone()
        };

        assert_eq!(table_draw.pass_ordinal, Some(3));
        assert_eq!(table_draw.draw_ordinal_in_pass, Some(4));
        assert_eq!(
            table_draw.root_signature_sha256.as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(direct_draw.pass_ordinal, None);
        assert_eq!(direct_draw.root_signature_sha256, None);

        // Two draws are only contradictory evidence for one view when they
        // agree on where they came from; differing provenance is the answer,
        // not a conflict.
        let other_pass = PendingDraw {
            pass_context: None,
            pipeline_depth: None,
            pass_ordinal: Some(5),
            ..table_draw.clone()
        };
        assert_ne!(table_draw.pass_ordinal, other_pass.pass_ordinal);
    }

    /// `cbv_descriptors` is a keyed lookup, but `release_descriptor_heap` still
    /// has to drop every handle that belonged to the released heap. That used to
    /// fall out of an ordered `range`; with a hash map it is an explicit scan, so
    /// pin the behaviour - including that neighbouring heaps keep their entries.
    #[test]
    fn releasing_a_descriptor_heap_drops_exactly_its_own_cbv_handles() {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        let increment = 64u64;
        let released_start = 0x1_0000_0000u64;
        let neighbour_start = released_start + increment * 8;
        for (raw, cpu_start) in [(0x10usize, released_start), (0x20, neighbour_start)] {
            tracker.register_descriptor_heap(raw, cpu_start, cpu_start, 8, increment as u32, true);
        }
        for slot in 0..16u64 {
            tracker.write_cbv_descriptor(
                released_start + slot * increment,
                0x20_0000 + slot * 256,
                256,
            );
        }
        assert_eq!(tracker.cbv_descriptor_store.len(), 16);

        tracker.release_descriptor_heap(0x10);

        assert_eq!(tracker.cbv_descriptor_store.len(), 8);
        for slot in 0..8u64 {
            assert!(
                !tracker
                    .cbv_descriptor_store
                    .contains_key(&(released_start + slot * increment)),
                "released heap must drop its own handles"
            );
            assert!(
                tracker
                    .cbv_descriptor_store
                    .contains_key(&(neighbour_start + slot * increment)),
                "an adjacent heap must keep its handles"
            );
        }

        // Releasing an unknown heap stays a no-op.
        tracker.release_descriptor_heap(0x30);
        assert_eq!(tracker.cbv_descriptor_store.len(), 8);
    }

    fn run_pre_execute_test_in_isolation(test_name: &str) -> bool {
        const CHILD: &str = "DM_KCD2_PRE_EXECUTE_TEST_CHILD";
        if std::env::var(CHILD).as_deref() == Ok(test_name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, test_name)
            .output()
            .expect("start isolated KCD2 pre-Execute test");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
            "isolated {test_name} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn pre_execute_tracker() -> RenderCameraTracker {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        tracker.register_buffer_with_heap_kind(1, 0x1000, 512, true);
        tracker.map_buffer(1, 0x3000);
        tracker.register_list(10, 20);
        tracker.arm_selected_depth(selected_depth());
        tracker.set_viewport(
            10,
            CapturedViewport {
                count: 1,
                first: windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT {
                    Width: 1920.0,
                    Height: 1080.0,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                    ..Default::default()
                },
            },
        );
        tracker
    }

    fn stage_pre_execute(tracker: &mut RenderCameraTracker) {
        let binding = tracker.resolve_binding(2, 0x1080).unwrap();
        tracker
            .stage_kcd2_pre_execute_draw(
                10,
                DrawLocation {
                    pass_context: None,
                    pipeline_depth: None,
                    global_ordinal: 12,
                    pass_ordinal: 3,
                    ordinal_in_pass: 4,
                    root_signature_sha256: Some(
                        depth_lab::KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256,
                    ),
                },
                selected_depth(),
                vec![binding],
            )
            .unwrap();
    }

    #[test]
    fn kcd2_pre_execute_uses_updated_content_but_the_original_draw_binding() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_uses_updated_content_but_the_original_draw_binding",
        ) {
            return;
        }
        let mut tracker = pre_execute_tracker();
        stage_pre_execute(&mut tracker);
        let viewport = tracker.lists[&10].viewport;
        assert!(tracker.lists[&10].pending[0].kcd2_snapshots.is_empty());
        // A new game Map generation is legitimate. Later root/viewport changes do not change
        // the recorded Draw source, and no read was needed when that Draw was recorded.
        tracker.unmap_buffer(1);
        tracker.map_buffer(1, 0x4000);
        tracker.register_buffer_with_heap_kind(2, 0x2000, 512, true);
        tracker.map_buffer(2, 0x5000);
        tracker.bind_root_cbv(10, 2, 0x2000);
        tracker.lists.get_mut(&10).unwrap().viewport = None;
        let mut reads = 0;
        tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[10],
                Some(QueueId(3)),
                |address, size| {
                    assert_eq!(address, 0x4080);
                    assert_eq!(size, 256);
                    reads += 1;
                    Some(vec![7; size])
                },
                || Ok(900),
            )
            .unwrap();
        assert_eq!(reads, 2);
        tracker.execute(QueueId(3), &[10]);
        let snapshot = &tracker.submitted[0].draw.kcd2_snapshots[0];
        assert_eq!(snapshot.bytes, vec![7; 256]);
        assert_eq!(snapshot.sample_qpc, 900);
        assert_eq!(snapshot.root_parameter_index, 2);
        assert_eq!((snapshot.pass_ordinal, snapshot.draw_ordinal), (3, 4));
        assert_eq!(snapshot.viewport, viewport);
    }

    #[test]
    fn kcd2_pre_execute_release_replacement_overlap_and_generation_changes_fail_closed() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_release_replacement_overlap_and_generation_changes_fail_closed",
        ) {
            return;
        }
        for invalidation in 0..5 {
            let mut tracker = pre_execute_tracker();
            stage_pre_execute(&mut tracker);
            match invalidation {
                0 => tracker.release_resource(1),
                1 => {
                    tracker.release_resource(1);
                    tracker.register_buffer_with_heap_kind(1, 0x1000, 512, true);
                    tracker.map_buffer(1, 0x3000);
                }
                // This overlaps the tail of the 256-byte read, not its starting GPUVA.
                2 => tracker.register_buffer_with_heap_kind(2, 0x1100, 512, true),
                3 => tracker.lists.get_mut(&10).unwrap().list_generation += 1,
                4 => tracker.selected_depth = None,
                _ => unreachable!(),
            }
            assert!(
                tracker
                    .prepare_kcd2_pre_execute_snapshots(
                        &[10],
                        Some(QueueId(3)),
                        |_, _| { panic!("invalid identity must not be read") },
                        || Ok(900)
                    )
                    .is_err()
            );
            tracker.execute(QueueId(3), &[10]);
            assert!(tracker.submitted.is_empty());
        }
    }

    #[test]
    fn kcd2_pre_execute_reset_and_unprepared_sources_never_publish() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_reset_and_unprepared_sources_never_publish",
        ) {
            return;
        }
        let mut unprepared = pre_execute_tracker();
        stage_pre_execute(&mut unprepared);
        unprepared.execute(QueueId(3), &[10]);
        assert!(unprepared.submitted.is_empty());
        assert!(!unprepared.snapshot.blockers.is_empty());
        let mut reset = pre_execute_tracker();
        stage_pre_execute(&mut reset);
        reset.reset_list(10);
        reset
            .prepare_kcd2_pre_execute_snapshots(
                &[10],
                Some(QueueId(3)),
                |_, _| panic!("Reset must erase the recorded source"),
                || Ok(900),
            )
            .unwrap();
        reset.execute(QueueId(3), &[10]);
        assert!(reset.submitted.is_empty());
    }

    #[test]
    fn kcd2_pre_execute_failed_repeat_discards_prepared_bytes_and_requires_current_mapping() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_failed_repeat_discards_prepared_bytes_and_requires_current_mapping",
        ) {
            return;
        }
        let mut tracker = pre_execute_tracker();
        stage_pre_execute(&mut tracker);
        tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[10],
                Some(QueueId(3)),
                |_, n| Some(vec![1; n]),
                || Ok(900),
            )
            .unwrap();
        let mut reads = 0;
        assert!(
            tracker
                .prepare_kcd2_pre_execute_snapshots(
                    &[10],
                    Some(QueueId(3)),
                    |_, n| {
                        reads += 1;
                        Some(vec![reads; n])
                    },
                    || Ok(901)
                )
                .is_err()
        );
        assert!(tracker.lists[&10].pending[0].kcd2_snapshots.is_empty());
        tracker.unmap_buffer(1);
        assert!(
            tracker
                .prepare_kcd2_pre_execute_snapshots(
                    &[10],
                    Some(QueueId(3)),
                    |_, _| { panic!("unmapped source must not be read") },
                    || Ok(902)
                )
                .is_err()
        );
        tracker.execute(QueueId(3), &[10]);
        assert!(tracker.submitted.is_empty());
    }

    #[test]
    fn kcd2_pre_execute_current_exact_unmap_is_allowed_but_old_generation_is_rejected() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_current_exact_unmap_is_allowed_but_old_generation_is_rejected",
        ) {
            return;
        }
        for current in [false, true] {
            let mut tracker = pre_execute_tracker();
            stage_pre_execute(&mut tracker);
            tracker.unmap_buffer(1);
            let resource = tracker.resources.get_mut(&1).unwrap();
            resource.unmap_snapshots.insert(
                128,
                UploadUnmapSnapshot {
                    content_generation: if current {
                        resource.content_generation
                    } else {
                        0
                    },
                    bytes: vec![5; 256],
                },
            );
            let result = tracker.prepare_kcd2_pre_execute_snapshots(
                &[10],
                Some(QueueId(3)),
                |_, _| panic!("exact Unmap bytes need no live mapping"),
                || Ok(900),
            );
            assert_eq!(result.is_ok(), current);
            tracker.execute(QueueId(3), &[10]);
            assert_eq!(!tracker.submitted.is_empty(), current);
        }
    }

    #[test]
    fn kcd2_pre_execute_does_not_change_legacy_draw_snapshots() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_does_not_change_legacy_draw_snapshots",
        ) {
            return;
        }
        let mut tracker = pre_execute_tracker();
        let list = tracker.lists.get_mut(&10).unwrap();
        list.pending.push(PendingDraw {
            pass_context: None,
            pipeline_depth: None,
            list_tag: list.list_tag,
            list_generation: list.list_generation,
            draw_ordinal: 1,
            pass_ordinal: Some(0),
            draw_ordinal_in_pass: Some(0),
            root_signature_sha256: Some("legacy".into()),
            selected_depth: selected_depth(),
            matrices: Vec::new(),
            kcd2_snapshots: vec![Kcd2DirectRootSnapshot {
                sample_qpc: 123,
                root_signature_sha256: "legacy".into(),
                root_parameter_index: 2,
                pass_ordinal: 0,
                draw_ordinal: 0,
                bytes: vec![8; 256],
                viewport: list.viewport,
            }],
            kcd2_pre_execute_sources: Vec::new(),
            kcd2_pre_execute_prepared: false,
            kcd2_pre_execute_queue: None,
        });
        tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[10],
                Some(QueueId(3)),
                |_, _| panic!("legacy snapshots must retain Draw-time bytes"),
                || panic!("legacy QPC must remain unchanged"),
            )
            .unwrap();
        tracker.execute(QueueId(3), &[10]);
        assert_eq!(
            tracker.submitted[0].draw.kcd2_snapshots[0].bytes,
            vec![8; 256]
        );
        assert_eq!(tracker.submitted[0].draw.kcd2_snapshots[0].sample_qpc, 123);
    }

    #[test]
    fn kcd2_pre_execute_unrelated_batches_are_noops_and_mixed_unknown_lists_are_allowed() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_unrelated_batches_are_noops_and_mixed_unknown_lists_are_allowed",
        ) {
            return;
        }
        let mut tracker = pre_execute_tracker();
        tracker.register_list(20, 30);
        tracker
            .kcd2_pre_execute_errors
            .insert(10, "error belongs to another list".into());
        tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[999, 20],
                None,
                |_, _| panic!("unrelated COPY/COMPUTE/unknown lists must not read"),
                || panic!("unrelated submission must not sample QPC"),
            )
            .unwrap();
        tracker.kcd2_pre_execute_errors.clear();
        stage_pre_execute(&mut tracker);
        tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[999, 10, 20],
                Some(QueueId(3)),
                |_, n| Some(vec![2; n]),
                || Ok(900),
            )
            .unwrap();
        tracker.execute(QueueId(3), &[999, 10, 20]);
        assert_eq!(tracker.submitted.len(), 1);
    }

    #[test]
    fn kcd2_pre_execute_sources_require_the_same_direct_queue() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_sources_require_the_same_direct_queue",
        ) {
            return;
        }
        let mut tracker = pre_execute_tracker();
        stage_pre_execute(&mut tracker);
        let error = tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[10],
                None,
                |_, _| panic!("source on unknown/non-DIRECT queue must not read"),
                || Ok(900),
            )
            .unwrap_err();
        assert!(error.contains("DIRECT"));
        assert!(tracker.lists[&10].pending[0].kcd2_snapshots.is_empty());
        tracker
            .prepare_kcd2_pre_execute_snapshots(
                &[10],
                Some(QueueId(3)),
                |_, n| Some(vec![2; n]),
                || Ok(900),
            )
            .unwrap();
        tracker.execute(QueueId(4), &[10]);
        assert!(tracker.submitted.is_empty());
    }

    #[test]
    fn kcd2_pre_execute_preparation_is_atomic_across_required_sources() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_preparation_is_atomic_across_required_sources",
        ) {
            return;
        }
        let mut tracker = pre_execute_tracker();
        stage_pre_execute(&mut tracker);
        let binding = tracker.resolve_binding(2, 0x1000).unwrap();
        tracker
            .stage_kcd2_pre_execute_draw(
                10,
                DrawLocation {
                    pass_context: None,
                    pipeline_depth: None,
                    global_ordinal: 13,
                    pass_ordinal: 3,
                    ordinal_in_pass: 5,
                    root_signature_sha256: Some(
                        depth_lab::KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256,
                    ),
                },
                selected_depth(),
                vec![binding],
            )
            .unwrap();
        assert!(
            tracker
                .prepare_kcd2_pre_execute_snapshots(
                    &[10],
                    Some(QueueId(3)),
                    |address, n| (address == 0x3080).then(|| vec![1; n]),
                    || Ok(900)
                )
                .is_err()
        );
        assert!(
            tracker.lists[&10]
                .pending
                .iter()
                .all(|draw| draw.kcd2_snapshots.is_empty())
        );
        tracker.execute(QueueId(3), &[10]);
        assert!(tracker.submitted.is_empty());
    }

    #[test]
    fn kcd2_pre_execute_post_submit_failure_clears_whole_batch_and_reports_first_fatal() {
        if run_pre_execute_test_in_isolation(
            "d3d12_render_camera::tests::kcd2_pre_execute_post_submit_failure_clears_whole_batch_and_reports_first_fatal",
        ) {
            return;
        }
        let mut first_error = None;
        for invalidation in 0..7 {
            let mut tracker = pre_execute_tracker();
            stage_pre_execute(&mut tracker);
            tracker.register_buffer_with_heap_kind(2, 0x2000, 512, true);
            tracker.map_buffer(2, 0x5000);
            tracker.register_list(20, 30);
            tracker.set_viewport(20, tracker.lists[&10].viewport.unwrap());
            let binding = tracker.resolve_binding(0, 0x2080).unwrap();
            tracker
                .stage_kcd2_pre_execute_draw(
                    20,
                    DrawLocation {
                        pass_context: None,
                        pipeline_depth: None,
                        global_ordinal: 30,
                        pass_ordinal: 5,
                        ordinal_in_pass: 0,
                        root_signature_sha256: Some(
                            depth_lab::kcd2_pose::KCD2_PLAYER_ROOT_SIGNATURE_SHA256,
                        ),
                    },
                    selected_depth(),
                    vec![binding],
                )
                .unwrap();
            tracker
                .prepare_kcd2_pre_execute_snapshots(
                    &[10, 20],
                    Some(QueueId(3)),
                    |_, n| Some(vec![7; n]),
                    || Ok(900),
                )
                .unwrap();
            let mut queue = QueueId(3);
            match invalidation {
                0 => tracker.release_resource(1),
                1 => {
                    tracker.release_resource(1);
                    tracker.register_buffer_with_heap_kind(1, 0x1000, 512, true);
                    tracker.map_buffer(1, 0x3000);
                }
                2 => tracker.selected_depth = None,
                3 => tracker.reset_list(10),
                4 => tracker.release_list(10),
                5 => tracker.lists.get_mut(&10).unwrap().list_generation += 1,
                6 => queue = QueueId(4),
                _ => unreachable!(),
            }
            let faults = crate::kcd2_live_pose::summary().faults;
            tracker.execute(queue, &[10, 20]);
            assert!(
                tracker.submitted.is_empty(),
                "failed batch retained a valid sibling source"
            );
            assert!(tracker.lists.values().all(|list| {
                list.pending
                    .iter()
                    .all(|draw| draw.kcd2_pre_execute_sources.is_empty())
            }));
            assert_eq!(crate::kcd2_live_pose::summary().faults, faults + 1);
            let error =
                crate::kcd2_live_pose::failure().expect("post-submit failure must reach KCD2");
            assert!(error.contains("kcd2_live_pose_submission_invalid"));
            if let Some(first) = &first_error {
                assert_eq!(
                    &error, first,
                    "later failures must not replace the first cause"
                );
            } else {
                first_error = Some(error);
            }
        }
    }

    #[test]
    fn viewport_is_list_local_resettable_and_requires_exact_geometry() {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        tracker.register_list(0x1000, 1);
        tracker.register_list(0x2000, 2);
        let viewport = CapturedViewport {
            count: 1,
            first: windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT {
                Width: 1920.0,
                Height: 1080.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
                ..Default::default()
            },
        };
        viewport.validate(1920, 1080).unwrap();
        tracker.set_viewport(0x1000, viewport);
        assert_eq!(tracker.lists[&0x1000].viewport, Some(viewport));
        assert_eq!(tracker.lists[&0x2000].viewport, None);
        tracker.reset_list(0x1000);
        assert_eq!(tracker.lists[&0x1000].viewport, None);
        assert!(viewport.validate(1280, 720).is_err());
        for count in [0, 2] {
            assert!(
                CapturedViewport { count, ..viewport }
                    .validate(1920, 1080)
                    .is_err()
            );
        }
        for (min_depth, max_depth) in [
            (0.5, 0.5),
            (1.0, 0.0),
            (-0.1, 1.0),
            (0.0, 1.1),
            (f32::NAN, 1.0),
        ] {
            let mut invalid = viewport;
            invalid.first.MinDepth = min_depth;
            invalid.first.MaxDepth = max_depth;
            assert!(invalid.validate(1920, 1080).is_err());
        }
        let mut offset = viewport;
        offset.first.TopLeftX = 1.0;
        assert!(offset.validate(1920, 1080).is_err());
    }
    use crate::d3d12_abi::{DescriptorHandle, ResourceId};

    #[test]
    fn direct_cbv_mapping_requires_unique_upload_range_and_current_mapping() {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10000, 1024, true);
        assert_eq!(
            tracker.direct_root_mapping_candidate(2, 0x10100),
            Some((0x1000, 256))
        );
        assert!(tracker.resolve_binding(2, 0x10100).is_none());
        tracker.map_buffer(0x1000, 0x800000);
        assert_eq!(tracker.direct_root_mapping_candidate(2, 0x10100), None);
        assert_eq!(
            tracker.resolve_binding(2, 0x10100).unwrap().resource_offset,
            256
        );
        tracker.register_buffer_with_heap_kind(0x2000, 0x20000, 256, false);
        assert_eq!(tracker.direct_root_mapping_candidate(2, 0x20000), None);
        assert!(tracker.resolve_binding(2, 0x20000).is_none());
        // Even one mapped match cannot override another live object at the same GPU address.
        tracker.register_buffer_with_heap_kind(0x3000, 0x10100, 256, true);
        assert_eq!(tracker.direct_root_mapping_candidate(2, 0x10100), None);
        assert!(tracker.resolve_binding(2, 0x10100).is_none());
        tracker.release_resource(0x3000);
        assert!(tracker.resolve_binding(2, 0x10100).is_some());
        assert_eq!(tracker.direct_root_mapping_candidate(2, 0x103c0), None);
        assert_eq!(tracker.direct_root_mapping_candidate(2, 0x90000), None);
        let outcomes: Vec<_> = tracker
            .snapshot
            .direct_root_resolution_samples
            .iter()
            .map(|sample| sample.outcome)
            .collect();
        assert_eq!(
            outcomes,
            [
                "unmapped_upload",
                "mapped",
                "unmapped_non_upload",
                "ambiguous_gpu_address",
                "short_cbv_range",
                "unregistered_gpu_address"
            ]
        );
        tracker.unmap_buffer(0x1000);
        assert!(tracker.resolve_binding(2, 0x10100).is_none());
    }

    #[test]
    fn warp_direct_upload_can_be_remapped_after_game_unmap() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::core::Interface;
        let (device, _) = crate::d3d12_candidate::tests::warp_device();
        let mut resource = None;
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_UPLOAD,
                    CreationNodeMask: 1,
                    VisibleNodeMask: 1,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: 256,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut resource,
            )
        }
        .unwrap();
        let resource: ID3D12Resource = resource.unwrap();
        let mut data = std::ptr::null_mut();
        unsafe {
            resource
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut data))
                .unwrap();
            std::ptr::write_bytes(data, 0x5a, 256);
            resource.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 256 }));
        }
        let raw = resource.as_raw() as usize;
        let gpu_va = unsafe { resource.GetGPUVirtualAddress() };
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        tracker.register_buffer_with_heap_kind(raw, gpu_va, 256, true);
        assert_eq!(
            tracker.direct_root_mapping_candidate(2, gpu_va),
            Some((raw, 0))
        );
        unsafe {
            resource.Map(
                0,
                Some(&D3D12_RANGE { Begin: 0, End: 256 }),
                Some(&mut data),
            )
        }
        .unwrap();
        tracker
            .register_observer_mapping(raw, data as usize)
            .unwrap();
        assert!(tracker.resolve_binding(2, gpu_va).is_some());
        assert!(
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), 256) }
                .iter()
                .all(|byte| *byte == 0x5a)
        );
        assert_eq!(tracker.resources[&raw].map_count, 0);
        let prior_generation = tracker
            .resolve_binding(2, gpu_va)
            .unwrap()
            .content_generation;
        unsafe { resource.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut data)) }
            .unwrap();
        tracker.map_buffer(raw, data as usize);
        unsafe { std::ptr::write_bytes(data, 0x6b, 256) };
        tracker.unmap_buffer(raw);
        unsafe { resource.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 256 })) };
        assert!(
            tracker
                .resolve_binding(2, gpu_va)
                .unwrap()
                .content_generation
                > prior_generation
        );
        assert_eq!(tracker.resources[&raw].map_count, 0);
        assert!(tracker.resolve_binding(2, gpu_va).is_some());
        assert!(
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), 256) }
                .iter()
                .all(|byte| *byte == 0x6b)
        );
        tracker.release_resource(raw);
        unsafe { resource.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
        assert!(tracker.resolve_binding(2, gpu_va).is_none());
    }

    const ROOT_SIGNATURE_SHA256: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn selected_depth() -> DsvIdentity {
        DsvIdentity {
            handle: DescriptorHandle(4),
            descriptor_generation: 2,
            resource: ResourceId(7),
            resource_epoch: 3,
        }
    }

    fn row_major_bytes(matrix: Mat4) -> Vec<u8> {
        matrix
            .into_iter()
            .flatten()
            .flat_map(f32::to_le_bytes)
            .collect()
    }

    fn view() -> Mat4 {
        [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [1.0, 2.0, 3.0, 1.0],
        ]
    }

    fn configured_75_projection() -> Mat4 {
        let x = 1.0 / (75.0_f32.to_radians() * 0.5).tan();
        let y = x * (16.0 / 9.0);
        [
            [x, 0.0, 0.0, 0.0],
            [0.0, y, 0.0, 0.0],
            [0.0, 0.0, 0.0001, 1.0],
            [0.0, 0.0, 0.1, 0.0],
        ]
    }

    #[test]
    fn legacy_draw_counter_never_receives_hot_list_copy_stamp() {
        for late_mark in [false, true] {
            let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
            tracker.register_buffer(0x1000, 0x10_0000, 512);
            tracker.map_buffer(0x1000, 0x20_0000);
            tracker.register_list(0x2000, 9);
            tracker.arm_selected_depth(selected_depth());
            tracker.bind_depth(0x2000, Some(selected_depth()));
            tracker.bind_root_cbv(0x2000, 3, 0x10_0000);
            let mut bytes = row_major_bytes(view());
            bytes.extend(row_major_bytes(configured_75_projection()));
            bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
            tracker.observe_draw(0x2000, |_, _, size| Some(bytes[..size].to_vec()));
            tracker.execute(QueueId(5), &[0x2000]);
            if !late_mark {
                tracker.mark_depth_copy_submitted_in_list(
                    QueueId(5),
                    44,
                    Some(0x2000),
                    Some(0),
                    Some(42),
                );
            }
            tracker.present(QueueId(5), 44, 1234);
            if late_mark {
                tracker.mark_depth_copy_submitted_in_list(
                    QueueId(5),
                    44,
                    Some(0x2000),
                    Some(0),
                    Some(42),
                );
            }
            let snapshot = tracker.snapshot();
            assert!(
                snapshot
                    .observations
                    .iter()
                    .all(|draw| draw.pipeline_depth_enable.is_none()
                        && draw.pipeline_depth_write.is_none())
            );
            assert_eq!(snapshot.observations.len(), 1);
            let draw = &snapshot.observations[0];
            assert_eq!(draw.draw_ordinal, 1);
            assert_eq!(draw.pass_ordinal, None);
            assert_eq!(draw.depth_copy_execute_batch, Some(draw.execute_batch));
            assert_eq!(draw.depth_copy_list_index_in_batch, Some(0));
            assert_eq!(draw.depth_copy_command_list_tag, Some(9));
            assert_eq!(draw.depth_copy_draws_before_in_list, None);
            assert!(
                serde_json::to_value(draw).unwrap()["depth_copy_draws_before_in_list"].is_null()
            );
            // The legacy counter cannot order a draw inside the copy's own
            // list, so admission refuses it instead of assuming it came first.
            assert!(!draw.copy_before_clear_submitted);
            assert!(!draw.selected_present_identity_passed);
        }
    }

    #[test]
    fn exact_selected_dsv_draw_is_joined_to_the_submitting_queue_and_present() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        tracker.bind_depth(0x2000, Some(selected_depth()));
        tracker.bind_root_cbv(0x2000, 3, 0x10_0000);
        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        tracker.observe_draw(0x2000, |_, _, size| Some(bytes[..size].to_vec()));
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        tracker.mark_depth_copy_submitted(QueueId(5), 44);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.observations.len(), 1);
        let observation = &snapshot.observations[0];
        assert_eq!(observation.present_seq, 44);
        assert_eq!(observation.queue_tag, 5);
        assert_eq!(observation.command_list_tag, 9);
        assert_eq!(observation.selected_depth_resource_tag, 7);
        assert!(observation.copy_before_clear_submitted);
        assert!(observation.selected_present_identity_passed);
        assert!(observation.matrices.iter().any(|matrix| {
            matrix.matrix_kind == "projection"
                && matrix.configured_vertical_fov_matches == Some(true)
                && matrix.projection_a.is_some()
                && matrix.projection_b.is_some()
        }));
        assert!(observation.matrices.iter().any(|matrix| {
            matrix.matrix_kind == "view"
                && matrix.axis_mapping == "unresolved_requires_motion_calibration"
        }));
    }

    #[test]
    fn wrong_dsv_unmapped_source_and_wrong_queue_never_emit_a_candidate() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        tracker.bind_depth(
            0x2000,
            Some(DsvIdentity {
                resource: ResourceId(8),
                ..selected_depth()
            }),
        );
        tracker.bind_root_cbv(0x2000, 3, 0x10_0000);
        tracker.observe_draw(0x2000, |_, _, _| Some(vec![0; 512]));
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(6), 44, 1234);
        assert!(tracker.snapshot().observations.is_empty());
    }

    #[test]
    fn matrix_source_address_is_never_serialized() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(0xdead_beef, 0x1000_0000, 512);
        tracker.map_buffer(0xdead_beef, 0x2000_0000);
        let encoded = serde_json::to_string(&tracker.snapshot()).unwrap();
        assert!(!encoded.contains("dead_beef"));
        assert!(!encoded.contains("268435456"));
        assert!(!encoded.contains("536870912"));
    }

    #[test]
    fn authoring_admission_rejects_contention_and_overflow_counters() {
        let mut snapshot = D3d12RenderCameraSnapshot {
            tracker_contention_drops: 3,
            matrix_candidate_overflow: 1,
            draw_scan_budget_exhausted: 2,
            ..D3d12RenderCameraSnapshot::default()
        };
        snapshot.refresh_authoring_admission();
        assert!(!snapshot.candidate_admitted_for_cross_launch_discovery);
        assert!(
            snapshot
                .authoring_blockers
                .iter()
                .any(|blocker| blocker.contains("tracker_contention_drops=3"))
        );
        assert!(
            snapshot
                .authoring_blockers
                .iter()
                .any(|blocker| blocker.contains("matrix_candidate_overflow=1"))
        );
        assert!(
            snapshot
                .authoring_blockers
                .iter()
                .any(|blocker| blocker.contains("draw_scan_budget_exhausted=2"))
        );
        assert!(!snapshot.production_camera_binding_emitted);
        assert!(!snapshot.recording_ready);
    }

    #[test]
    fn duplicate_gpu_ranges_are_not_guessed() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(1, 0x1000, 512);
        tracker.register_buffer(2, 0x1000, 512);
        tracker.map_buffer(1, 0x3000);
        tracker.map_buffer(2, 0x4000);
        assert!(tracker.resolve_binding(0, 0x1000).is_none());
    }

    #[test]
    fn descriptor_table_base_cbv_is_physical_bounded_and_present_joined() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature_with_fingerprint(
            0x6000,
            ROOT_SIGNATURE_SHA256.into(),
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 256);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pass_context: None,
                pipeline_depth: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 0,
                draw_ordinal_in_pass: 0,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            },
            |_, _, size| Some(bytes[..size].to_vec()),
        );
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        tracker.mark_depth_copy_submitted(QueueId(5), 44);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.selected_descriptor_table_draws, 1);
        assert_eq!(snapshot.selected_table_resolved_descriptors, 1);
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(
            snapshot.observations[0].physical_source,
            "game_mapped_root_signature_declared_cbv_at_selected_dsv_draw"
        );
        assert!(
            snapshot.observations[0]
                .matrices
                .iter()
                .all(|matrix| matrix.physical_source == "root_signature_declared_cbv")
        );
        assert!(snapshot.observations[0].matrices.iter().all(|matrix| {
            matrix.stable_source.as_ref().is_some_and(|source| {
                source.descriptor_table.root_signature_sha256 == ROOT_SIGNATURE_SHA256
                    && source.descriptor_table.root_parameter_index == 4
                    && source.descriptor_table.descriptor_range_index == 0
                    && source.descriptor_table.descriptor_offset_in_range == 0
                    && source.declared_cbv_bytes == 256
                    && source.draw_relation == D3d12DrawRelation::ExactSelectedDepthDsvDraw
            })
        }));
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("4194368"));
        assert!(!encoded.contains("5242944"));
    }

    fn production_tracker() -> RenderCameraTracker {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        let mut binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../depth-lab/tests/fixtures/camera-binding-v2.json"
        ))
        .unwrap();
        binding.view_sources[0]
            .source
            .descriptor_table
            .root_signature_sha256 = ROOT_SIGNATURE_SHA256.into();
        binding.projection_sources[0]
            .descriptor_table
            .root_signature_sha256 = ROOT_SIGNATURE_SHA256.into();
        tracker
            .configure_production(
                binding,
                depth_lab::CameraPoseStreamContext {
                    identity: depth_lab::StreamIdentity {
                        session_id: "session".into(),
                        run_id: "run".into(),
                        generation: 3,
                    },
                    dsv_id: "scene".into(),
                },
                (0, 0),
            )
            .unwrap();
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature_with_fingerprint(
            0x6000,
            ROOT_SIGNATURE_SHA256.into(),
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 256);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        tracker
    }

    fn production_draw(tracker: &mut RenderCameraTracker, view: Mat4) {
        let mut bytes = row_major_bytes(view);
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pass_context: None,
                pipeline_depth: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 1,
                draw_ordinal_in_pass: 12,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            },
            |_, _, size| Some(bytes[..size].to_vec()),
        );
        tracker.execute(QueueId(5), &[0x2000]);
    }

    #[test]
    fn cp2077_catalog_decodes_authored_cbv_bytes_before_assembling_pose() {
        let mut tracker = production_tracker();
        let (_, context) = tracker.production.clone().unwrap();
        let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
        ))
        .unwrap();
        tracker
            .configure_production(binding.clone(), context.clone(), (0, 0))
            .unwrap();
        tracker.register_buffer(0x1000, 0x10_0000, 1024);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 1024);
        tracker.register_root_signature_with_fingerprint(
            0x6000,
            binding.view_sources[0]
                .source
                .descriptor_table
                .root_signature_sha256
                .clone(),
            BTreeMap::from([0, 18].map(|root| {
                (
                    root,
                    vec![RootCbvRange {
                        table_offset: 1,
                        descriptor_count: 4,
                        descriptor_range_index: 0,
                        base_shader_register: 0,
                        register_space: 0,
                    }],
                )
            })),
        );
        // Synthetic native byte image, not a replay of already recovered views:
        // offset 0 = transposed row-vector VP, offset 128 = transposed P.
        let view: Mat4 = [
            [1., 0., 0., 0.],
            [0., -1., 0., 0.],
            [0., 0., 1., 0.],
            [-1., 2., -3., 1.],
        ];
        let projection: Mat4 = [
            [1.2891924, 0., 0., 0.],
            [0., 2.2918978, 0., 0.],
            [-0.00018310547, 0.000249566, -1.1920929e-6, 1.],
            [0., 0., 0.020000024, 0.],
        ];
        let vp: Mat4 = std::array::from_fn(|r| {
            std::array::from_fn(|c| (0..4).map(|i| view[r][i] * projection[i][c]).sum())
        });
        let mut bytes = vec![0u8; ROOT_CBV_SNAPSHOT_BYTES];
        for (offset, matrix) in [(0, vp), (128, projection)] {
            for (c, column) in matrix.iter().enumerate() {
                for (r, value) in column.iter().enumerate() {
                    let k = offset + (r * 4 + c) * 4;
                    bytes[k..k + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
        assert!(
            scan_view_projection_pairs(&bytes)
                .iter()
                .any(|candidate| { candidate.offset == 0 && candidate.transpose })
        );
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pipeline_depth: None,
                pass_context: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 1,
                draw_ordinal_in_pass: 12,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: [0, 18]
                    .map(|root| ObservedRootBinding {
                        root_parameter_index: root,
                        source: RootBindingSource::DescriptorTableBase,
                        value: 0x50_0000,
                    })
                    .to_vec(),
            },
            |_, _, size| Some(bytes[..size].to_vec()),
        );
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.mark_depth_copy_submitted(QueueId(5), 1);
        tracker.present(QueueId(5), 1, 1000);
        let draw = &tracker.snapshot.observations[0];
        assert_eq!(draw.matrices.len(), 4);
        for root in [0, 18] {
            let v = draw
                .matrices
                .iter()
                .find(|m| m.root_parameter_index == root && m.matrix_kind == "view")
                .unwrap();
            let p = draw
                .matrices
                .iter()
                .find(|m| m.root_parameter_index == root && m.matrix_kind == "projection")
                .unwrap();
            let row = depth_lab::d3d12_camera::assemble_pose(
                &binding, &context, 1, 1000, v.matrix, p.matrix,
            )
            .expect("authored byte decode must recover a rigid view before assembling pose");
            for (actual, expected) in row.camera_position.into_iter().zip([1., 3., 2.]) {
                assert!((actual - expected).abs() < 1e-4);
            }
            depth_lab::validate_camera_pose_row(&row).unwrap();
        }
    }

    #[test]
    fn production_v3_decodes_each_authored_alias_once_without_candidate_scanning() {
        let mut tracker = production_tracker();
        let (mut binding, context) = tracker.production.clone().unwrap();
        binding.schema = depth_lab::CAMERA_BINDING_SCHEMA_V3.into();
        binding.executable_sha256 = depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into();
        binding.holstered_camera = Some(depth_lab::Cp2077HolsteredCamera {
            schema: depth_lab::cp2077_bound_camera::POLICY.into(),
            camera_axis_mapping: binding.axis_mapping,
        });
        for source in binding
            .view_sources
            .iter_mut()
            .map(|v| &mut v.source)
            .chain(binding.projection_sources.iter_mut())
        {
            source.descriptor_table.descriptor_count = 3;
            source.descriptor_table.descriptor_offset_in_range = 1;
        }
        let mut v = binding.view_sources[0].clone();
        v.source.descriptor_table.root_parameter_index = 5;
        let mut p = binding.projection_sources[0].clone();
        p.descriptor_table.root_parameter_index = 5;
        binding.view_sources.push(v);
        binding.projection_sources.push(p);
        tracker
            .configure_production(binding, context, (0, 0))
            .unwrap();
        tracker.register_root_signature_with_fingerprint(
            0x6000,
            ROOT_SIGNATURE_SHA256.into(),
            BTreeMap::from([4, 5, 6].map(|root| {
                (
                    root,
                    vec![
                        RootCbvRange {
                            table_offset: 1,
                            descriptor_count: 3,
                            descriptor_range_index: 0,
                            base_shader_register: 0,
                            register_space: 0,
                        },
                        RootCbvRange {
                            table_offset: 4,
                            descriptor_count: 2,
                            descriptor_range_index: 1,
                            base_shader_register: 10,
                            register_space: 0,
                        },
                    ],
                )
            })),
        );
        // All six table entries and the direct root are resolvable, but only
        // offset 1 of range 0 at roots 4 and 5 belongs to the authored camera.
        for index in 0..6 {
            tracker.write_cbv_descriptor(0x40_0000 + index * 32, 0x10_0000, 256);
        }
        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pipeline_depth: None,
                pass_context: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 1,
                draw_ordinal_in_pass: 12,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: [4, 5, 6]
                    .map(|root| ObservedRootBinding {
                        root_parameter_index: root,
                        source: RootBindingSource::DescriptorTableBase,
                        value: 0x50_0000,
                    })
                    .into_iter()
                    .chain([ObservedRootBinding {
                        root_parameter_index: 7,
                        source: RootBindingSource::Direct,
                        value: 0x10_0000,
                    }])
                    .collect(),
            },
            |_, _, size| Some(bytes[..size].to_vec()),
        );
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.mark_depth_copy_submitted(QueueId(5), 1);
        tracker.present(QueueId(5), 1, 1000);
        assert_eq!(tracker.snapshot.descriptor_table_calls, 2);
        assert_eq!(tracker.snapshot.selected_table_resolved_descriptors, 2);
        assert_eq!(tracker.snapshot.selected_direct_root_bindings, 0);
        let draw = &tracker.snapshot.observations[0];
        assert_eq!(draw.matrices.len(), 4);
        assert!(draw.matrices.iter().all(|m| m.stable_source.is_some()));
        for matrix in &draw.matrices {
            assert_eq!(
                matrix.matrix,
                match matrix.matrix_kind.as_str() {
                    "view" => view(),
                    "projection" => configured_75_projection(),
                    other => panic!("unexpected matrix role {other}"),
                }
            );
        }
        assert_eq!(
            draw.matrices
                .iter()
                .filter(|m| m.matrix_kind == "view")
                .count(),
            2
        );
        // Exact decoding alone cannot admit this mapped-buffer fixture: no executed copy,
        // viewport or depth-write provenance exists. It must not become Ready.
        assert!(tracker.take_production_pose(1, (0, 0)).is_err());
    }

    #[test]
    fn selected_mapped_reader_matches_snapshot_reader_and_rejects_stale_mappings() {
        let bindings = SelectedDrawBindings {
            pipeline_state: 0,
            pass_context: None,
            pipeline_depth: None,
            list_generation: 1,
            draw_ordinal: 12,
            pass_ordinal: 1,
            draw_ordinal_in_pass: 12,
            descriptor_heaps: [0x3000, 0],
            root_signature: 0x6000,
            roots: vec![ObservedRootBinding {
                root_parameter_index: 4,
                source: RootBindingSource::DescriptorTableBase,
                value: 0x50_0000,
            }],
        };
        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        let mut expected = production_tracker();
        production_draw(&mut expected, view());
        let mut actual = production_tracker();
        let mut reads = Vec::new();
        actual.observe_selected_draw_mapped(0x2000, &bindings, |address, size| {
            reads.push((address, size));
            Some(bytes[..size].to_vec())
        });
        actual.execute(QueueId(5), &[0x2000]);
        assert!(!reads.is_empty());
        assert!(
            reads
                .iter()
                .all(|&(address, size)| address == 0x20_0000 && size == ROOT_CBV_SNAPSHOT_BYTES)
        );
        for tracker in [&mut expected, &mut actual] {
            tracker.mark_depth_copy_submitted(QueueId(5), 1);
            tracker.present(QueueId(5), 1, 100);
        }
        assert_eq!(
            serde_json::to_value(expected.take_production_pose(1, (0, 0)).unwrap()).unwrap(),
            serde_json::to_value(actual.take_production_pose(1, (0, 0)).unwrap()).unwrap(),
        );
        let mut tracker = production_tracker();
        assert_eq!(
            tracker.mapped_read_address(0x1000, 256, 256),
            Some(0x20_0100)
        );
        assert_eq!(tracker.mapped_read_address(0x1000, 257, 256), None);
        tracker.unmap_buffer(0x1000);
        assert_eq!(tracker.mapped_read_address(0x1000, 0, 256), None);
        tracker
            .observe_selected_draw_mapped(0x2000, &bindings, |_, _| panic!("unmapped source read"));
        tracker.release_resource(0x1000);
        assert_eq!(tracker.mapped_read_address(0x1000, 0, 1), None);
    }

    #[test]
    fn cached_direct_ownership_tracks_resource_changes_and_live_mapping_state() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        assert!(tracker.resolve_binding(2, 0x1010).is_none());
        tracker.register_buffer_with_heap_kind(0x10, 0x1000, 1024, true);
        assert_eq!(
            tracker.direct_root_mapping_candidate(2, 0x1010),
            Some((0x10, 16))
        );
        tracker.map_buffer(0x10, 0xa000);
        let original = tracker.resolve_binding(2, 0x1010).unwrap();
        assert_eq!(original.resource_raw, 0x10);
        assert_eq!(original.resource_offset, 16);
        assert!(tracker.direct_root_mapping_candidate(2, 0x1010).is_none());
        tracker.unmap_buffer(0x10);
        assert!(tracker.resolve_binding(2, 0x1010).is_none());
        assert_eq!(
            tracker.direct_root_mapping_candidate(2, 0x1010),
            Some((0x10, 16))
        );
        tracker.map_buffer(0x10, 0xb000);
        assert_eq!(tracker.mapped_read_address(0x10, 16, 256), Some(0xb010));
        assert!(
            tracker
                .resolve_binding(2, 0x1010)
                .unwrap()
                .content_generation
                > original.content_generation
        );

        // An overlapping registration invalidates a previously unique address even
        // if the new resource is not mapped and cannot itself supply source bytes.
        tracker.register_buffer(0x20, 0x1008, 1024);
        assert!(tracker.resolve_binding(2, 0x1010).is_none());
        assert!(tracker.direct_root_mapping_candidate(2, 0x1010).is_none());
        tracker.release_resource(0x20);
        assert_eq!(
            tracker.resolve_binding(2, 0x1010).unwrap().source_tag,
            original.source_tag
        );

        // Eviction does not turn a miss into an accepted source or bypass rescanning.
        for address in 0x8000..0x8400 {
            assert!(tracker.resolve_binding(2, address).is_none());
        }
        assert!(tracker.gpuva_ownership_cache.borrow().entries.len() <= 256);
        assert_eq!(
            tracker.resolve_binding(2, 0x1010).unwrap().source_tag,
            original.source_tag
        );

        // Recycled COM identity replaces both the old range and its ownership cache.
        tracker.register_buffer_with_heap_kind(0x10, 0x2000, 1024, true);
        assert!(tracker.resolve_binding(2, 0x1010).is_none());
        assert_eq!(
            tracker.direct_root_mapping_candidate(2, 0x2010),
            Some((0x10, 16))
        );
        tracker.map_buffer(0x10, 0xc000);
        assert_ne!(
            tracker.resolve_binding(2, 0x2010).unwrap().source_tag,
            original.source_tag
        );
        tracker.release_resource(0x10);
        assert!(tracker.resolve_binding(2, 0x2010).is_none());
        assert!(tracker.direct_root_mapping_candidate(2, 0x2010).is_none());
    }

    #[test]
    fn production_v2_stream_does_not_exhaust_the_discovery_history_budget() {
        let mut tracker = production_tracker();
        for seq in 1..=4200 {
            production_draw(&mut tracker, view());
            tracker.mark_depth_copy_submitted(QueueId(5), seq);
            tracker.present(QueueId(5), seq, seq as i64 * 100);
            let (row, copied) = tracker.take_production_pose(seq, (0, 0)).unwrap().unwrap();
            assert!(copied);
            assert_eq!(row.present_seq, seq);
            depth_lab::validate_camera_pose_row(&row).unwrap();
            assert!(tracker.snapshot.observations.is_empty());
        }
        assert_eq!(tracker.snapshot.present_observation_overflow, 0);
        tracker.disarm();
        assert!(
            tracker
                .take_production_pose(4201, (0, 0))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn production_coverage_failure_reports_the_trigger_without_admitting_pose() {
        let failures = [
            (
                "pending_draw_overflow",
                D3d12RenderCameraSnapshot {
                    pending_draw_overflow: 1,
                    ..Default::default()
                },
            ),
            (
                "present_observation_overflow",
                D3d12RenderCameraSnapshot {
                    present_observation_overflow: 1,
                    ..Default::default()
                },
            ),
            (
                "selected_table_matrix_candidates_invalidated_by_generation",
                D3d12RenderCameraSnapshot {
                    selected_table_matrix_candidates_invalidated_by_generation: 1,
                    ..Default::default()
                },
            ),
            (
                "matrix_candidate_overflow",
                D3d12RenderCameraSnapshot {
                    matrix_candidate_overflow: 1,
                    ..Default::default()
                },
            ),
            (
                "upload_unmap_snapshot_budget_exhausted",
                D3d12RenderCameraSnapshot {
                    upload_unmap_snapshot_budget_exhausted: 1,
                    ..Default::default()
                },
            ),
            (
                "exact_cbv_copy_shadow_budget_exhausted",
                D3d12RenderCameraSnapshot {
                    exact_cbv_copy_shadow_budget_exhausted: 1,
                    ..Default::default()
                },
            ),
        ];
        for (field, snapshot) in failures {
            let mut tracker = production_tracker();
            tracker.snapshot = snapshot;
            let error = tracker.take_production_pose(1, (0, 0)).unwrap_err();
            assert!(error.contains(&format!("{field}=1")), "{error}");
        }
        let mut tracker = production_tracker();
        tracker.production_contention_baseline = (3, 4);
        let error = tracker.take_production_pose(1, (5, 6)).unwrap_err();
        assert!(error.contains("tracker_contention_drops=5 (baseline=3)"));
        assert!(error.contains("hot_state_drops=6 (baseline=4)"));
        let mut tracker = production_tracker();
        tracker.block("synthetic coverage blocker");
        assert!(
            tracker
                .take_production_pose(1, (0, 0))
                .unwrap_err()
                .contains("synthetic coverage blocker")
        );
    }

    #[test]
    fn production_v2_rejects_changed_matrices_and_missing_source_coverage() {
        let mut tracker = production_tracker();
        production_draw(&mut tracker, view());
        let mut different = view();
        different[3][0] += 1.0;
        production_draw(&mut tracker, different);
        tracker.present(QueueId(5), 1, 100);
        assert!(
            tracker
                .take_production_pose(1, (0, 0))
                .unwrap_err()
                .contains("ambiguous")
        );
        let mut tracker = production_tracker();
        production_draw(&mut tracker, view());
        tracker.present(QueueId(5), 1, 100);
        assert!(tracker.take_production_pose(1, (1, 0)).is_err());
        let mut tracker = production_tracker();
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 512);
        production_draw(&mut tracker, view());
        tracker.present(QueueId(5), 1, 100);
        assert!(tracker.take_production_pose(1, (0, 0)).unwrap().is_none());
    }

    #[test]
    fn cbv_created_after_final_unmap_scan_is_rejected_without_snapshot() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10_0000, 512, true);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.snapshot_upload_cbvs_before_final_unmap(0x1000, None, |_, _| {
            panic!("no CBV existed during this scan")
        });
        // Models a creator arriving after its shard was scanned (also possible
        // after the former atomic whole-table scan had released its locks).
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 256);
        tracker.unmap_buffer(0x1000);
        let resolved =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert!(resolved.bindings.is_empty());
        assert_eq!(resolved.cbv_gpuva_upload_snapshot_missing, 1);
        assert!(tracker.snapshot().observations.is_empty());
    }

    #[test]
    fn final_upload_unmap_snapshot_is_exact_versioned_and_present_joined() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10_0000, 512, true);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 256);
        tracker.map_buffer(0x1000, 0x20_0000);
        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        tracker.snapshot_upload_cbvs_before_final_unmap(0x1000, None, |_, size| {
            Some(bytes[..size].to_vec())
        });
        tracker.unmap_buffer(0x1000);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pass_context: None,
                pipeline_depth: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 0,
                draw_ordinal_in_pass: 0,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            },
            |_, _, _| None,
        );
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        tracker.mark_depth_copy_submitted(QueueId(5), 44);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.upload_unmap_snapshot_ranges_captured, 1);
        assert_eq!(snapshot.upload_unmap_snapshot_live_ranges, 1);
        assert_eq!(snapshot.upload_unmap_snapshot_live_bytes, 256);
        assert_eq!(snapshot.selected_table_resolved_descriptors, 1);
        assert_eq!(
            snapshot.selected_table_resolved_from_upload_unmap_snapshot,
            1
        );
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(
            snapshot.observations[0].physical_source,
            "upload_unmap_snapshot_root_signature_declared_cbv_at_selected_dsv_draw"
        );
        assert!(snapshot.observations[0].matrices.iter().all(|matrix| {
            matrix.physical_source == "upload_unmap_snapshot_root_signature_declared_cbv"
        }));

        // A new Map is a new CPU-content generation. It invalidates the cache
        // immediately, before any later selected draw can consume stale bytes.
        tracker.map_buffer(0x1000, 0x21_0000);
        tracker.unmap_buffer(0x1000);
        let stale =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert!(stale.bindings.is_empty());
        assert_eq!(stale.cbv_gpuva_upload_snapshot_missing, 1);
    }

    #[test]
    fn descriptor_late_copy_buffer_region_is_deferred_to_exact_execute_bytes() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10_0000, 512, true);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_buffer_with_heap_kind(0x1100, 0x11_0000, 512, false);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.register_list(0x2000, 9);
        // Model the real descriptor-late ordering: the physical upload→default
        // copy is recorded before the CBV descriptor exists.
        tracker.record_copy_buffer_region(0x2000, 0x1100, 0, 0x1000, 0, 256);
        tracker.write_cbv_descriptor(0x40_0040, 0x11_0000, 256);
        tracker.arm_selected_depth(selected_depth());
        tracker.set_scissor(
            0x2000,
            draw_pass::ScissorObservation {
                count: 1,
                single_rect: Some([0, 0, 1280, 720]),
            },
        );
        let frozen_pass = tracker.draw_pass_context(0x2000, Some(1));
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0x8000,
                pass_context: Some(frozen_pass),
                pipeline_depth: Some(PipelineDepthState {
                    enable: true,
                    write: false,
                }),
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 0,
                draw_ordinal_in_pass: 0,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            },
            |_, _, _| None,
        );
        tracker.set_scissor(
            0x2000,
            draw_pass::ScissorObservation {
                count: 0,
                single_rect: None,
            },
        );
        assert_eq!(tracker.snapshot().selected_descriptor_table_draws, 1);
        assert!(tracker.lists[&0x2000].pending.is_empty());
        assert_eq!(tracker.lists[&0x2000].deferred_copy_draws.len(), 1);

        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        // A scene may retain many unrelated upload mappings. Execute lookup must include only
        // exact requests of submitted lists and must not grow with those unrelated resources.
        tracker.register_buffer_with_heap_kind(0x1200, 0x12_0000, 512, true);
        tracker.map_buffer(0x1200, 0x30_0000);
        assert_eq!(tracker.mapped_ranges().len(), 2);
        assert_eq!(tracker.exact_copy_mapped_ranges(&[0x2000]).len(), 1);
        assert!(tracker.exact_copy_mapped_ranges(&[0x9999]).is_empty());
        let prepared = tracker.prepare_exact_cbv_copy_shadows(&[0x2000], |raw, offset, size| {
            assert_eq!((raw, offset), (0x1000, 0));
            Some(bytes[..size].to_vec())
        });
        assert_eq!(prepared.len(), 1);
        tracker.commit_exact_cbv_copy_shadows(QueueId(5), prepared);
        tracker.materialize_deferred_copy_draws(QueueId(5), &[0x2000]);
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        tracker.mark_depth_copy_submitted(QueueId(5), 44);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.exact_cbv_copy_spans_recorded, 1);
        assert_eq!(snapshot.exact_cbv_copy_requests, 1);
        assert_eq!(snapshot.exact_cbv_copy_shadows_committed, 1);
        assert_eq!(
            snapshot.selected_table_resolved_from_exact_cbv_copy_shadow,
            1
        );
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(snapshot.observations[0].pass_context, Some(frozen_pass));
        assert_eq!(snapshot.observations[0].pipeline_depth_enable, Some(true));
        assert_eq!(snapshot.observations[0].pipeline_depth_write, Some(false));
        assert_eq!(
            snapshot.observations[0].physical_source,
            "copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw"
        );
        assert!(snapshot.observations[0].matrices.iter().all(|matrix| {
            matrix.physical_source == "copy_buffer_region_root_signature_declared_cbv"
        }));
    }

    #[test]
    fn production_exact_copy_draw_returns_a_descriptor_generation_dedup_key() {
        let mut tracker = production_tracker();
        let (mut binding, context) = tracker.production.clone().unwrap();
        binding.schema = depth_lab::CAMERA_BINDING_SCHEMA_V3.into();
        binding.executable_sha256 = depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into();
        binding.holstered_camera = Some(depth_lab::Cp2077HolsteredCamera {
            schema: depth_lab::cp2077_bound_camera::POLICY.into(),
            camera_axis_mapping: binding.axis_mapping,
        });
        tracker
            .configure_production(binding, context, (0, 0))
            .unwrap();
        tracker.release_resource(0x1000);
        tracker.register_buffer_with_heap_kind(0x1000, 0x12_0000, 512, true);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_buffer_with_heap_kind(0x1100, 0x10_0000, 512, false);
        tracker.record_copy_buffer_region(0x2000, 0x1100, 0, 0x1000, 0, 256);
        tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 256);
        let descriptor_generation = tracker
            .cbv_descriptor_store
            .get(&0x40_0040)
            .unwrap()
            .generation;

        let observation = tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0x8000,
                pass_context: None,
                pipeline_depth: Some(PipelineDepthState {
                    enable: true,
                    write: true,
                }),
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 0,
                draw_ordinal_in_pass: 0,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            },
            |_, _, _| panic!("a deferred exact-copy Draw must not read bytes before Execute"),
        );

        assert!(observation.staged);
        assert_eq!(
            observation.production_dedup_key,
            Some(ProductionDrawDedupKey {
                descriptor_identities: [(0x40_0040, descriptor_generation), (0, 0), (0, 0), (0, 0),],
                descriptor_count: 1,
                root_bindings: [(4, 0x50_0000), (0, 0), (0, 0), (0, 0)],
                root_binding_count: 1,
            })
        );
        assert_eq!(tracker.lists[&0x2000].deferred_copy_draws.len(), 1);
    }

    #[test]
    fn descriptor_late_copy_buffer_region_rejects_descriptor_generation_drift() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10_0000, 512, true);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_buffer_with_heap_kind(0x1100, 0x11_0000, 512, false);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.register_list(0x2000, 9);
        tracker.record_copy_buffer_region(0x2000, 0x1100, 0, 0x1000, 0, 256);
        tracker.write_cbv_descriptor(0x40_0040, 0x11_0000, 256);
        tracker.arm_selected_depth(selected_depth());
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pass_context: None,
                pipeline_depth: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 0,
                draw_ordinal_in_pass: 0,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            },
            |_, _, _| None,
        );

        // A descriptor overwrite between the selected draw and Execute breaks the
        // exact descriptor-generation proof. The upload bytes must not be read.
        tracker.write_cbv_descriptor(0x40_0040, 0x11_0100, 256);
        let prepared = tracker.prepare_exact_cbv_copy_shadows(&[0x2000], |_, _, _| {
            panic!("stale descriptor identity must be rejected before the CPU read")
        });
        assert!(prepared.is_empty());
        tracker.commit_exact_cbv_copy_shadows(QueueId(5), prepared);
        tracker.materialize_deferred_copy_draws(QueueId(5), &[0x2000]);
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        tracker.mark_depth_copy_submitted(QueueId(5), 44);

        let snapshot = tracker.snapshot();
        assert!(snapshot.observations.is_empty());
        assert!(snapshot.exact_cbv_copy_shadow_generation_rejections > 0);
        assert_eq!(snapshot.exact_cbv_copy_shadows_committed, 0);
    }

    #[test]
    fn same_draw_aliases_keep_one_pair_per_binding_and_the_same_copy_receipt() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10_0000, 512, true);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_buffer_with_heap_kind(0x1100, 0x11_0000, 512, false);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            [4, 18]
                .into_iter()
                .map(|root| {
                    (
                        root,
                        vec![RootCbvRange {
                            table_offset: 2,
                            descriptor_count: 1,
                            descriptor_range_index: 0,
                            base_shader_register: 0,
                            register_space: 0,
                        }],
                    )
                })
                .collect(),
        );
        tracker.register_list(0x2000, 9);
        tracker.record_copy_buffer_region(0x2000, 0x1100, 0, 0x1000, 0, 256);
        tracker.write_cbv_descriptor(0x40_0040, 0x11_0000, 256);
        tracker.arm_selected_depth(selected_depth());
        tracker.observe_selected_draw(
            0x2000,
            &SelectedDrawBindings {
                pipeline_state: 0,
                pass_context: None,
                pipeline_depth: None,
                list_generation: 1,
                draw_ordinal: 12,
                pass_ordinal: 0,
                draw_ordinal_in_pass: 0,
                descriptor_heaps: [0x3000, 0],
                root_signature: 0x6000,
                roots: [4, 18]
                    .into_iter()
                    .map(|root_parameter_index| ObservedRootBinding {
                        root_parameter_index,
                        source: RootBindingSource::DescriptorTableBase,
                        value: 0x50_0000,
                    })
                    .collect(),
            },
            |_, _, _| None,
        );
        let mut bytes = row_major_bytes(view());
        bytes.extend(row_major_bytes(view())); // history view must not reappear because of aliases
        bytes.extend(row_major_bytes(configured_75_projection()));
        bytes.resize(ROOT_CBV_SNAPSHOT_BYTES, 0);
        let prepared = tracker
            .prepare_exact_cbv_copy_shadows(&[0x2000], |_, _, size| Some(bytes[..size].to_vec()));
        assert_eq!(prepared.len(), 1);
        tracker.commit_exact_cbv_copy_shadows(QueueId(5), prepared);
        tracker.materialize_deferred_copy_draws(QueueId(5), &[0x2000]);
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        tracker.mark_depth_copy_submitted(QueueId(5), 44);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.observations.len(), 1);
        let matrices = &snapshot.observations[0].matrices;
        assert_eq!(matrices.len(), 4);
        let receipt = matrices[0].copy_receipt.as_ref().unwrap();
        assert_eq!(receipt.copied_bytes, 256);
        assert_eq!(
            receipt.bytes_sha256,
            format!("sha256:{:x}", Sha256::digest(&bytes))
        );
        assert!(
            matrices
                .iter()
                .all(|matrix| matrix.copy_receipt.as_ref() == Some(receipt))
        );
        assert_eq!(
            matrices
                .iter()
                .map(|matrix| (matrix.root_parameter_index, matrix.matrix_offset))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([(4, 0), (4, 128), (18, 0), (18, 128)])
        );
    }

    #[test]
    fn descriptor_table_requires_exact_bound_heap_base_and_current_descriptor() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.map_buffer(0x1000, 0x20_0000);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 0,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.write_cbv_descriptor(0x40_0000, 0x10_0000, 256);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        let bindings = SelectedDrawBindings {
            pipeline_state: 0,
            pass_context: None,
            pipeline_depth: None,
            list_generation: 1,
            draw_ordinal: 1,
            pass_ordinal: 0,
            draw_ordinal_in_pass: 0,
            descriptor_heaps: [0x3000, 0],
            root_signature: 0x6000,
            roots: vec![ObservedRootBinding {
                root_parameter_index: 4,
                source: RootBindingSource::DescriptorTableBase,
                value: 0x50_0000,
            }],
        };
        tracker.observe_selected_draw(0x2000, &bindings, |_, _, _| {
            let mut bytes = row_major_bytes(configured_75_projection());
            bytes.resize(256, 0);
            Some(bytes)
        });
        // Overwrite before Execute invalidates the draw-to-descriptor identity join.
        tracker.write_cbv_descriptor(0x40_0000, 0x10_0100, 256);
        tracker.execute(QueueId(5), &[0x2000]);
        tracker.present(QueueId(5), 44, 1234);
        let snapshot = tracker.snapshot();
        assert!(snapshot.observations.is_empty());
        assert!(snapshot.selected_table_matrix_candidates_invalidated_by_generation > 0);

        let unaligned =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0001, [0x3000, 0], 0x6000);
        assert!(unaligned.bindings.is_empty());
        assert_eq!(unaligned.table_base_misaligned, 1);
        let unbound =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0, 0], 0x6000);
        assert!(unbound.bindings.is_empty());
        assert_eq!(unbound.no_bound_descriptor_heap, 1);
    }

    #[test]
    fn descriptor_table_diagnostics_partition_each_exact_resolution_gate() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
        tracker.register_root_signature(
            0x6000,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 0,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );

        let missing_parameter =
            tracker.resolve_descriptor_table_binding(0x2000, 3, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(
            missing_parameter.root_parameter_without_declared_cbv_ranges,
            1
        );
        let untracked_heap =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3001, 0], 0x6000);
        assert_eq!(untracked_heap.bound_descriptor_heap_untracked, 1);
        let outside_heap =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x60_0000, [0x3000, 0], 0x6000);
        assert_eq!(outside_heap.table_base_outside_bound_heap, 1);

        let missing_cbv =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(missing_cbv.descriptor_not_current_cbv, 1);
        tracker.record_descriptor_table_resolution(&missing_cbv);
        assert_eq!(
            tracker.snapshot().selected_table_descriptor_not_current_cbv,
            1
        );

        tracker.write_cbv_descriptor(0x40_0000, 0x10_0000, 256);
        let without_resource =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(without_resource.cbv_gpuva_without_resource, 1);

        tracker.register_buffer(0x1000, 0x10_0000, 512);
        let unmapped =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(unmapped.cbv_gpuva_resource_unmapped, 1);
        tracker.map_buffer(0x1000, 0x20_0000);
        let resolved =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(resolved.resolved_descriptors, 1);
        assert_eq!(resolved.bindings.len(), 1);

        tracker.register_buffer(0x1001, 0x10_0000, 512);
        tracker.map_buffer(0x1001, 0x21_0000);
        let ambiguous =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(ambiguous.cbv_gpuva_resource_ambiguous, 1);

        tracker.release_resource(0x1001);
        tracker.write_cbv_descriptor(0x40_0000, 0x10_0180, 256);
        let overwide =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6000);
        assert_eq!(overwide.cbv_size_outside_resource, 1);

        tracker.register_root_signature(
            0x6001,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 8,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        let outside_declared_range =
            tracker.resolve_descriptor_table_binding(0x2000, 4, 0x50_0000, [0x3000, 0], 0x6001);
        assert_eq!(outside_declared_range.declared_descriptor_outside_heap, 1);
    }

    #[test]
    fn exact_duplicate_buffer_registration_and_present_zero_are_non_blocking_baselines() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.register_buffer(0x1000, 0x10_0000, 512);
        tracker.present(QueueId(5), 0, 1234);
        assert!(tracker.snapshot().blockers.is_empty());
    }

    #[test]
    fn contradictory_buffer_registration_replaces_recycled_com_pointer_state() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_buffer_with_heap_kind(0x1000, 0x10_0000, 512, true);
        tracker.map_buffer(0x1000, 0x20_0000);
        let first_tag = tracker.resources[&0x1000].source_tag;

        tracker.register_buffer_with_heap_kind(0x1000, 0x30_0000, 1024, false);

        let replacement = &tracker.resources[&0x1000];
        assert_ne!(replacement.source_tag, first_tag);
        assert_eq!(replacement.gpu_virtual_address, 0x30_0000);
        assert_eq!(replacement.buffer_bytes, 1024);
        assert!(!replacement.upload_heap);
        assert_eq!(replacement.map_count, 0);
        assert_eq!(replacement.mapped_cpu_address, None);
    }

    #[test]
    fn armed_non_drawing_depth_target_keeps_all_selected_draw_diagnostics_zero() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_root_signature(0x6000, BTreeMap::new());
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());

        let snapshot = tracker.snapshot();
        assert!(snapshot.selected_depth_armed);
        assert_eq!(snapshot.tracked_root_signatures, 1);
        assert_eq!(snapshot.tracked_root_signature_cbv_ranges, 0);
        assert_eq!(snapshot.selected_depth_draw_calls, 0);
        assert_eq!(snapshot.selected_draws_without_root_signature, 0);
        assert_eq!(snapshot.selected_draws_with_unknown_root_signature, 0);
        assert_eq!(
            snapshot.selected_draws_with_root_signature_without_cbv_ranges,
            0
        );
        assert_eq!(snapshot.selected_draws_with_declared_cbv_ranges, 0);
    }

    #[test]
    fn selected_draw_diagnostics_separate_root_signature_failure_modes() {
        let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
        tracker.register_list(0x2000, 9);
        tracker.arm_selected_depth(selected_depth());
        let draw = |root_signature, roots| SelectedDrawBindings {
            pipeline_state: 0,
            pass_context: None,
            pipeline_depth: None,
            list_generation: 1,
            draw_ordinal: 1,
            pass_ordinal: 0,
            draw_ordinal_in_pass: 0,
            descriptor_heaps: [0, 0],
            root_signature,
            roots,
        };
        tracker.observe_selected_draw(0x2000, &draw(0, Vec::new()), |_, _, _| None);
        tracker.observe_selected_draw(
            0x2000,
            &draw(
                0x6001,
                vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            ),
            |_, _, _| None,
        );
        tracker.register_root_signature(0x6002, BTreeMap::new());
        tracker.observe_selected_draw(
            0x2000,
            &draw(
                0x6002,
                vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            ),
            |_, _, _| None,
        );
        tracker.register_root_signature(
            0x6003,
            BTreeMap::from([(
                4,
                vec![RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }],
            )]),
        );
        tracker.observe_selected_draw(
            0x2000,
            &draw(
                0x6003,
                vec![ObservedRootBinding {
                    root_parameter_index: 4,
                    source: RootBindingSource::DescriptorTableBase,
                    value: 0x50_0000,
                }],
            ),
            |_, _, _| None,
        );

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.selected_depth_draw_calls, 4);
        assert_eq!(snapshot.selected_draws_without_root_signature, 1);
        assert_eq!(snapshot.selected_draws_with_unknown_root_signature, 1);
        assert_eq!(
            snapshot.selected_draws_with_root_signature_without_cbv_ranges,
            1
        );
        assert_eq!(snapshot.selected_draws_with_declared_cbv_ranges, 1);
        assert_eq!(snapshot.selected_draws_without_descriptor_table_bindings, 1);
        assert!(snapshot.observations.is_empty());
    }
}
