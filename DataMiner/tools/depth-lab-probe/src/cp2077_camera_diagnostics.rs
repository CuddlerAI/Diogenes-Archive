//! Debug-only, best-effort sidecars. Never read by admission, coverage, or delivery.
//! All recording hooks are nonblocking; file IO belongs to a separate bounded worker.
pub(crate) mod dense_pairs;
pub(crate) mod early_player;
mod memory;
pub(crate) mod page_query;
mod player;
pub(crate) use player::trace as trace_player_sample;
#[cfg(test)]
mod tests;

use crate::{
    cp2077_pose_wiring::FormalPlayerAddresses, d3d12_render_camera::D3d12RenderDrawObservation,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread::JoinHandle,
};

const FAMILY_FILE: &str = "cp2077-projection-families.jsonl";
const MEMORY_FILE: &str = "cp2077-memory-camera.jsonl";
const PLAYER_FILE: &str = "cp2077-player-sample-timing.jsonl";
const SUMMARY_FILE: &str = "cp2077-camera-diagnostics-summary.json";
const MAX_FAMILIES: usize = 64;
const MAX_LOCATIONS: usize = 256;
static ACTIVE: AtomicU64 = AtomicU64::new(0);
static NEXT_RUN: AtomicU64 = AtomicU64::new(1);
static LOST: AtomicU64 = AtomicU64::new(0);
static STATE: Mutex<Option<State>> = Mutex::new(None);

// CPU recording-window counters, deliberately NOT attributed to a GPU Present.
// In particular, source-filter rejections cannot be called non-depth-writing draws.
pub(crate) enum DrawCount {
    Selected,
    SourceRejected,
    Redundant,
    TrackerEntered,
    SourceRejectedWriting,
    SourceRejectedNonwriting,
    SourceRejectedUnknownPso,
}
pub(crate) struct DrawCounters([AtomicU64; 7]);
impl DrawCounters {
    pub(crate) fn new() -> Self {
        Self([const { AtomicU64::new(0) }; 7])
    }
    pub(crate) fn add(&self, kind: DrawCount) {
        if active() {
            self.0[kind as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn add_to(&self, totals: &mut [u64; 7]) {
        for (sum, count) in totals.iter_mut().zip(&self.0) {
            *sum = sum.saturating_add(count.load(Ordering::Relaxed));
        }
    }
}
#[derive(Debug, Default, Serialize)]
struct DrawCounts {
    selected_dsv_draws: u64,
    source_prefilter_rejected: u64,
    dedup_hits: u64,
    tracker_entered: u64,
    source_rejected_depth_writing: u64,
    source_rejected_nonwriting: u64,
    source_rejected_unknown_pso: u64,
}
fn draw_counts_since(baseline: [u64; 7]) -> DrawCounts {
    let current = crate::d3d12_backend::diagnostic_draw_counts();
    let values: [u64; 7] = std::array::from_fn(|i| current[i].saturating_sub(baseline[i]));
    DrawCounts {
        selected_dsv_draws: values[0],
        source_prefilter_rejected: values[1],
        dedup_hits: values[2],
        tracker_entered: values[3],
        source_rejected_depth_writing: values[4],
        source_rejected_nonwriting: values[5],
        source_rejected_unknown_pso: values[6],
    }
}

struct State {
    run: u64,
    generation: u64,
    sender: SyncSender<Event>,
    join: JoinHandle<()>,
    sampler: Result<memory::Sampler, String>,
    last_memory_present: Option<u64>,
    last_player_present: Option<u64>,
    early_config: early_player::Config,
}

pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Acquire) != 0
}
pub(crate) fn note_loss() {
    if active() {
        LOST.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct DrawWeight {
    generation: u64,
    count: AtomicU64,
}
impl DrawWeight {
    pub(crate) fn new() -> Option<Arc<Self>> {
        let generation = ACTIVE.load(Ordering::Acquire);
        (generation != 0).then(|| {
            Arc::new(Self {
                generation,
                count: AtomicU64::new(1),
            })
        })
    }
    pub(crate) fn increment(&self) {
        if self.generation == ACTIVE.load(Ordering::Acquire) {
            self.count.fetch_add(1, Ordering::Relaxed);
        } else {
            note_loss();
        }
    }
    pub(crate) fn snapshot(&self) -> Option<u64> {
        (self.generation == ACTIVE.load(Ordering::Acquire))
            .then(|| self.count.load(Ordering::Acquire))
    }
}

#[derive(Debug, Serialize, Default)]
pub(crate) struct FamilyFrame {
    schema: &'static str,
    pub present_seq: u64,
    qpc: i64,
    family_definition: &'static str,
    scope: &'static str,
    pub families: Vec<Family>,
    family_count_observed: usize,
    representative_draws: u64,
    represented_draws: u64,
    nonwriting_representatives: u64,
    unknown_pso_representatives: u64,
    missing_projection_representatives: u64,
    missing_weight_representatives: u64,
    alias_conflict_representatives: u64,
    bounded_overflow: bool,
    location_count: usize,
    tracker_loss_observed: bool,
    #[serde(skip)]
    early_links: early_player::PresentLinks,
    #[serde(skip)]
    diagnostic_run: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Family {
    m22: f32,
    m32: f32,
    m22_bits: u32,
    m32_bits: u32,
    representative_draws: u64,
    represented_draws: u64,
    locations: Vec<Location>,
}

#[derive(Debug, Serialize)]
struct Location {
    queue_tag: u64,
    execute_batch: u64,
    command_list_tag: u64,
    command_list_generation: u64,
    selected_depth_resource_tag: u64,
    selected_depth_resource_epoch: u64,
    selected_depth_descriptor_generation: u64,
    root_signature_sha256: Option<String>,
    first_projection_sources: Vec<ProjectionSource>,
    pass_ordinal: Option<u32>,
    first_draw_ordinal: u32,
    first_draw_ordinal_in_pass: Option<u32>,
    last_representative_draw_ordinal: u32,
    pso_depth_enable: Option<bool>,
    pso_depth_write: Option<bool>,
    representative_draws: u64,
    represented_draws: u64,
    before_selected_depth_copy: bool,
}

#[derive(Debug, Serialize)]
struct ProjectionSource {
    source_tag: u64,
    root_parameter_index: u32,
    cbv_resource_offset: u64,
    matrix_offset: u32,
    copy_receipt: Option<depth_lab::d3d12_camera_alias::CopyReceipt>,
}

impl FamilyFrame {
    pub(crate) fn new(present_seq: u64, qpc: i64) -> Self {
        Self {
            schema: "dm-cp2077-projection-families/v1",
            present_seq,
            qpc,
            diagnostic_run: ACTIVE.load(Ordering::Acquire),
            family_definition: "exact_f32_bits_of_canonical_projection_m22_m32",
            scope: "authored_cbv_sources_on_selected_dsv; not_all_possible_shader_camera_sources",
            ..Self::default()
        }
    }
    pub(crate) fn observe(&mut self, draw: &D3d12RenderDrawObservation, weight: Option<u64>) {
        if draw.present_seq != self.present_seq || draw.present_qpc != self.qpc {
            self.tracker_loss_observed = true;
            return;
        }
        self.early_links.observe(draw);
        self.representative_draws += 1;
        let count = weight.unwrap_or(1);
        self.represented_draws += count;
        if weight.is_none() {
            self.missing_weight_representatives += 1;
        }
        if draw.pipeline_depth_write != Some(true) {
            if draw.pipeline_depth_write == Some(false) {
                self.nonwriting_representatives += 1;
            } else {
                self.unknown_pso_representatives += 1;
            }
            return;
        }
        let keys: BTreeSet<_> = draw
            .matrices
            .iter()
            .filter(|m| m.matrix_kind == "projection")
            .filter(|m| m.matrix[2][2].is_finite() && m.matrix[3][2].is_finite())
            .map(|m| (m.matrix[2][2].to_bits(), m.matrix[3][2].to_bits()))
            .collect();
        if keys.is_empty() {
            self.missing_projection_representatives += 1;
        }
        if keys.len() > 1 {
            self.alias_conflict_representatives += 1;
        }
        for (a, b) in keys {
            let index = self
                .families
                .iter()
                .position(|f| (f.m22_bits, f.m32_bits) == (a, b));
            let index = match index {
                Some(index) => index,
                None if self.families.len() < MAX_FAMILIES => {
                    self.families.push(Family {
                        m22: f32::from_bits(a),
                        m32: f32::from_bits(b),
                        m22_bits: a,
                        m32_bits: b,
                        representative_draws: 0,
                        represented_draws: 0,
                        locations: Vec::new(),
                    });
                    self.family_count_observed = self.families.len();
                    self.families.len() - 1
                }
                None => {
                    self.bounded_overflow = true;
                    continue;
                }
            };
            let family = &mut self.families[index];
            family.representative_draws += 1;
            family.represented_draws += count;
            let location = family.locations.iter_mut().find(|l| {
                l.queue_tag == draw.queue_tag
                    && l.execute_batch == draw.execute_batch
                    && l.command_list_tag == draw.command_list_tag
                    && l.command_list_generation == draw.command_list_generation
                    && l.root_signature_sha256 == draw.root_signature_sha256
                    && l.pass_ordinal == draw.pass_ordinal
                    && l.before_selected_depth_copy == draw.copy_before_clear_submitted
            });
            if let Some(location) = location {
                location.representative_draws += 1;
                location.represented_draws += count;
                location.last_representative_draw_ordinal = draw.draw_ordinal;
            } else if self.location_count < MAX_LOCATIONS {
                self.location_count += 1;
                family.locations.push(Location {
                    queue_tag: draw.queue_tag,
                    execute_batch: draw.execute_batch,
                    command_list_tag: draw.command_list_tag,
                    command_list_generation: draw.command_list_generation,
                    selected_depth_resource_tag: draw.selected_depth_resource_tag,
                    selected_depth_resource_epoch: draw.selected_depth_resource_epoch,
                    selected_depth_descriptor_generation: draw.selected_depth_descriptor_generation,
                    root_signature_sha256: draw.root_signature_sha256.clone(),
                    first_projection_sources: draw
                        .matrices
                        .iter()
                        .filter(|m| {
                            m.matrix_kind == "projection"
                                && (m.matrix[2][2].to_bits(), m.matrix[3][2].to_bits()) == (a, b)
                        })
                        .take(4)
                        .map(|m| ProjectionSource {
                            source_tag: m.source_tag,
                            root_parameter_index: m.root_parameter_index,
                            cbv_resource_offset: m.cbv_resource_offset,
                            matrix_offset: m.matrix_offset,
                            copy_receipt: m.copy_receipt.clone(),
                        })
                        .collect(),
                    pass_ordinal: draw.pass_ordinal,
                    first_draw_ordinal: draw.draw_ordinal,
                    first_draw_ordinal_in_pass: draw.draw_ordinal_in_pass,
                    last_representative_draw_ordinal: draw.draw_ordinal,
                    pso_depth_enable: draw.pipeline_depth_enable,
                    pso_depth_write: draw.pipeline_depth_write,
                    representative_draws: 1,
                    represented_draws: count,
                    before_selected_depth_copy: draw.copy_before_clear_submitted,
                });
            } else {
                self.bounded_overflow = true;
            }
        }
    }
    pub(crate) fn note_tracker_loss(&mut self, lost: bool) {
        self.tracker_loss_observed |= lost;
    }
}

#[derive(Serialize)]
struct MemoryRow {
    schema: &'static str,
    generation: u64,
    present_seq: u64,
    qpc: i64,
    read_begin_qpc: i64,
    read_end_qpc: i64,
    sample_phase: &'static str,
    source_chain_id: &'static str,
    #[serde(flatten)]
    sample: memory::MemorySample,
}

enum Event {
    Family(FamilyFrame),
    Memory(MemoryRow),
    Player(player::Row),
    EarlySample(early_player::SampleRow),
    EarlyBindings(early_player::Bindings),
    PageQuery(page_query::Batch),
    DensePair(dense_pairs::Row),
    DenseSummary(serde_json::Value),
}

fn qpc() -> i64 {
    let mut value = 0;
    let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut value) };
    value
}

pub(crate) fn begin(
    root: &Path,
    identity: depth_lab::StreamIdentity,
    binding: &depth_lab::D3d12CameraBinding,
    player: Option<FormalPlayerAddresses>,
) {
    if binding.executable_sha256 != depth_lab::CP2077_BUILD_20383525_EXE_SHA256
        || binding.holstered_camera.is_none()
    {
        return;
    }
    // Manual switch for performance comparisons: these sidecars cost CPU on game threads and
    // tens of megabytes of disk per minute that a build without diagnostics never pays.
    if std::env::var_os("DM_CP2077_DIAGNOSTIC_SIDECARS_OFF").is_some() {
        crate::probe::log("CP2077 camera diagnostics disabled by environment");
        return;
    }
    if let Err(error) = start(root, identity, binding, player) {
        crate::probe::log(&format!(
            "CP2077 camera diagnostics unavailable (capture policy unchanged): {error}"
        ));
    }
}

fn start(
    root: &Path,
    identity: depth_lab::StreamIdentity,
    binding: &depth_lab::D3d12CameraBinding,
    player: Option<FormalPlayerAddresses>,
) -> Result<(), String> {
    let mut state = STATE.lock().map_err(|_| "diagnostic state poisoned")?;
    if state.is_some() {
        return Err("previous diagnostic run still active".into());
    }
    let mut family_file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(FAMILY_FILE))
            .map_err(|e| e.to_string())?,
    );
    let mut memory_file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(MEMORY_FILE))
            .map_err(|e| e.to_string())?,
    );
    let mut player_file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(PLAYER_FILE))
            .map_err(|e| e.to_string())?,
    );
    let summary_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(SUMMARY_FILE))
        .map_err(|e| e.to_string())?;
    let sampler = player
        .ok_or_else(|| "exact player sampler unavailable".into())
        .and_then(|addresses| {
            let scale = binding
                .view_metres_per_unit
                .parse::<f64>()
                .map_err(|_| "invalid metric scale")?;
            memory::Sampler::locate(addresses, binding.axis_mapping, scale)
        });
    let mut frequency = 0;
    unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
        .map_err(|_| "diagnostic QPC frequency unavailable")?;
    if frequency <= 0 {
        return Err("diagnostic QPC frequency is invalid".into());
    }
    let scale = binding
        .view_metres_per_unit
        .parse::<f64>()
        .map_err(|_| "invalid diagnostic metric scale")?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err("invalid diagnostic metric scale".into());
    }
    let early_config = early_player::Config {
        addresses: player,
        axis: binding.axis_mapping,
        scale,
        frequency,
    };
    let mut early_file =
        early_player::Writer::new(root, &identity, early_config).map_err(|e| e.to_string())?;
    let mut query_file =
        page_query::Writer::new(root, &identity, frequency).map_err(|e| e.to_string())?;
    let mut dense_file = dense_pairs::create(root, &identity, frequency, binding.axis_mapping)
        .map_err(|e| e.to_string())?;
    let header = serde_json::json!({"schema":"dm-cp2077-camera-diagnostics-header/v1", "identity":identity,
        "qpc_frequency":frequency, "diagnostic_only":true, "non_trainable":true, "production_rules_unchanged":true,
        "source_chain_id":memory::CHAIN_ID, "memory_candidate_resolved":sampler.is_ok(),
        "position_units":"metres", "world_axis_mapping":binding.axis_mapping, "quaternion_order":"xyzw",
        "executable_sha256":binding.executable_sha256,
        "render_width":binding.render_width,"render_height":binding.render_height,
        "metres_per_native_unit":binding.view_metres_per_unit,
        "rotation_semantics":"component_local_to_delivery_world_after_axis_basis_conversion; active_camera_equivalence_unproven",
        "fov_semantics":"base_component_parameter_degrees; axis_and_final_zoom_override_composition_unverified",
        "render_reference":"camera-pose-index.jsonl", "max_families_per_present":MAX_FAMILIES,
        "max_locations_per_present":MAX_LOCATIONS,
        "cpu_counter_scope":"recording_window_only_not_submitted_present_draw_counts",
        "exhaustive_all_selected_dsv_depth_writers":false});
    write_line(&mut family_file, &header).map_err(|e| e.to_string())?;
    write_line(&mut memory_file, &header).map_err(|e| e.to_string())?;
    let player_header = serde_json::json!({
        "schema":"dm-cp2077-player-sample-timing-header/v1", "identity":identity,
        "qpc_frequency":frequency, "diagnostic_only":true, "non_trainable":true,
        "production_rules_unchanged":true, "source_chain_id":crate::cp2077_player_probe::PROVIDER,
        "world_axis_mapping":binding.axis_mapping, "position_units":"metres", "quaternion_order":"xyzw",
        "sample_scope":"existing_formal_sampler_including_audit_identity_checks_RPM_and_decode; not_RPM_only",
        "present_qpc_semantics":"inherited_render_anchor_not_engine_state_frame",
        "fingerprint":"sha256_of_decoded_f64_le_position_then_xyzw; not_native_memory_or_engine_tick",
        "player_reference":"player-pose-index.jsonl", "render_reference":"camera-pose-index.jsonl",
        "engine_state_frame_known":false, "stable_read_attempt_count_recorded":false
    });
    write_line(&mut player_file, &player_header).map_err(|e| e.to_string())?;
    family_file.flush().map_err(|e| e.to_string())?;
    memory_file.flush().map_err(|e| e.to_string())?;
    player_file.flush().map_err(|e| e.to_string())?;
    let (sender, receiver) = sync_channel::<Event>(256);
    LOST.store(0, Ordering::Release);
    let counter_baseline = crate::d3d12_backend::diagnostic_draw_counts();
    let run = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    let generation = identity.generation;
    let join = std::thread::Builder::new().name("dm-cp2077-camera-diagnostics".into()).spawn(move || {
        let mut presents = 0u64;
        let mut multi = 0u64;
        let mut empty = 0u64;
        let mut memory_rows = 0u64;
        let mut player_rows = 0u64;
        let mut incomplete = 0u64;
        let mut events_since_flush = 0u32;
        let mut fault = None;
        for event in receiver {
            let result = match event {
                Event::Family(row) => {
                    let result = write_line(&mut family_file, &row);
                    if result.is_ok() {
                        presents += 1; multi += u64::from(row.families.len() > 1); empty += u64::from(row.families.is_empty());
                        incomplete += u64::from(row.families.is_empty() || row.bounded_overflow || row.tracker_loss_observed
                            || row.unknown_pso_representatives > 0 || row.missing_projection_representatives > 0 || row.missing_weight_representatives > 0);
                    }
                    result.and_then(|_| early_file.present(&row))
                }
                Event::Memory(row) => {
                    let result = write_line(&mut memory_file, &row);
                    if result.is_ok() { memory_rows += 1; }
                    result
                }
                Event::Player(mut row) => {
                    row.fingerprint_on_writer();
                    let result = write_line(&mut player_file, &row);
                    if result.is_ok() { player_rows += 1; }
                    result
                }
                Event::EarlySample(row) => early_file.sample(row),
                Event::EarlyBindings(row) => early_file.bindings(row),
                Event::PageQuery(row) => query_file.batch(row),
                Event::DensePair(row) => write_line(&mut dense_file, &row),
                Event::DenseSummary(row) => write_line(&mut dense_file, &row),
            };
            if let Err(error) = result { fault = Some(error.to_string()); break; }
            // Count every event kind once. An unchanged Present counter divisible by 60
            // must not flush all files again for every query/early-sample event in that gap.
            events_since_flush += 1;
            if events_since_flush == 60
                && let Err(error) = family_file.flush().and_then(|_| memory_file.flush()).and_then(|_| player_file.flush()).and_then(|_| early_file.flush()).and_then(|_| query_file.flush()).and_then(|_| dense_file.flush()) { fault = Some(error.to_string()); break; }
            if events_since_flush == 60 { events_since_flush = 0; }
        }
        if let Err(error) = family_file.flush().and_then(|_| memory_file.flush()).and_then(|_| player_file.flush()).and_then(|_| early_file.flush()).and_then(|_| query_file.flush()).and_then(|_| dense_file.flush()) { fault = Some(error.to_string()); }
        let summary = serde_json::json!({"schema":"dm-cp2077-camera-diagnostics-summary/v1", "identity":identity,
            "family_present_rows":presents,"multiple_family_presents":multi,"zero_family_presents":empty,
            "multiple_family_fraction":if presents == 0 {None} else {Some(multi as f64 / presents as f64)},
            "multiple_family_fraction_observed_nonempty":if presents == empty {None} else {Some(multi as f64 / (presents-empty) as f64)},
            "incomplete_family_presents":incomplete,"memory_rows":memory_rows,"player_sample_timing_rows":player_rows,
            "cbv_player_sampling":early_file.counts,
            "page_queries":query_file.counts,
            "diagnostic_losses":LOST.load(Ordering::Acquire),"io_error":fault,
            "cpu_recording_counters":draw_counts_since(counter_baseline),
            "exhaustive_all_selected_dsv_depth_writers":false,
            "scope":"authored_sources_only; nonwriting_PSO_is_not_pixel_contribution; zero_multifamily_is_not_proof_of_absence",
            "changes_admission_or_delivery":false});
        let mut summary_file = BufWriter::new(summary_file);
        if let Err(error) = write_line(&mut summary_file, &summary).and_then(|_| summary_file.flush()) {
            crate::probe::log(&format!("CP2077 diagnostic summary write failed: {error}"));
        }
    }).map_err(|e| e.to_string())?;
    dense_pairs::begin(run, generation, frequency, sender.clone());
    *state = Some(State {
        run,
        generation,
        sender,
        join,
        sampler,
        last_memory_present: None,
        last_player_present: None,
        early_config,
    });
    ACTIVE.store(run, Ordering::Release);
    Ok(())
}

fn write_line(writer: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")
}

pub(crate) fn submit_family(row: FamilyFrame) {
    if !active() {
        return;
    }
    if let Ok(state) = STATE.try_lock()
        && let Some(state) = state.as_ref()
        && state.run == ACTIVE.load(Ordering::Acquire)
        && row.diagnostic_run == state.run
    {
        if state.sender.try_send(Event::Family(row)).is_err() {
            note_loss();
        }
    } else {
        note_loss();
    }
}

pub(crate) fn sample_memory(present_seq: u64, present_qpc: i64) {
    if !active() || present_seq == 0 {
        return;
    }
    if present_qpc <= 0 {
        note_loss();
        return;
    }
    let Ok(mut state) = STATE.try_lock() else {
        note_loss();
        return;
    };
    let Some(state) = state.as_mut() else {
        return;
    };
    if state.run != ACTIVE.load(Ordering::Acquire)
        || state
            .last_memory_present
            .is_some_and(|last| present_seq <= last)
    {
        note_loss();
        return;
    }
    state.last_memory_present = Some(present_seq);
    let started = qpc();
    let sample = state
        .sampler
        .as_ref()
        .map_err(Clone::clone)
        .and_then(memory::Sampler::sample)
        .unwrap_or_else(memory::MemorySample::unavailable);
    let ended = qpc();
    let row = MemoryRow {
        schema: "dm-cp2077-memory-camera/v1",
        generation: state.generation,
        present_seq,
        qpc: present_qpc,
        read_begin_qpc: started,
        read_end_qpc: ended,
        sample_phase: "after_render_tracker_present_before_original_present",
        source_chain_id: memory::CHAIN_ID,
        sample,
    };
    if state.sender.try_send(Event::Memory(row)).is_err() {
        note_loss();
    }
}

pub(crate) fn finish() {
    // Serialize lifecycle through worker drain: no later run may reset the counters that the
    // closing worker is writing. Draw/Present callers only try_lock, never wait for this join.
    if let Ok(mut guard) = STATE.lock() {
        ACTIVE.store(0, Ordering::Release);
        if let Some(state) = guard.take() {
            // Before the channel closes: the last dense entry and its summary go through it.
            dense_pairs::finish();
            drop(state.sender);
            if state.join.join().is_err() {
                crate::probe::log(
                    "CP2077 diagnostic worker panicked; sidecars incomplete, capture policy unchanged",
                );
            }
        }
    }
}
