//! CBV-batch diagnostic: RPM runs AFTER the tracker unlock/prepare timing, BEFORE real Execute.
//! Three factual records (sample -> committed copies -> actual Present uses), never guessed seqs.
use super::{ACTIVE, Event, Ordering, STATE, note_loss, qpc, write_line};
use crate::d3d12_render_camera::{D3d12RenderDrawObservation, PreparedCbvCopyShadow};
use crate::{cp2077_player_probe::diagnostic, cp2077_pose_wiring::FormalPlayerAddresses};
use depth_lab::d3d12_camera_alias::CopyReceipt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicU64;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
};

pub(super) const FILE: &str = "cp2077-player-cbv-samples.jsonl";
const MAX_COPIES: usize = 2048;
static NEXT_SAMPLE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(super) struct Config {
    pub addresses: Option<FormalPlayerAddresses>,
    pub axis: depth_lab::D3d12AxisMapping,
    pub scale: f64,
    pub frequency: i64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SnapshotTiming {
    run: u64,
    begin: i64,
    end: i64,
}
impl SnapshotTiming {
    pub(crate) fn begin() -> Option<Self> {
        let run = ACTIVE.load(Ordering::Acquire);
        (run != 0).then(|| Self {
            run,
            begin: qpc(),
            end: 0,
        })
    }
    pub(crate) fn finish(mut self) -> Option<Self> {
        self.end = qpc();
        if self.run != ACTIVE.load(Ordering::Acquire) {
            return None;
        }
        if self.begin > 0 && self.end >= self.begin {
            Some(self)
        } else {
            note_loss();
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleToken {
    run: u64,
    generation: u64,
    sample_id: u64,
    queue_tag: u64,
    begin: i64,
}

#[derive(Debug, Serialize)]
pub(super) struct SampleRow {
    schema: &'static str,
    generation: u64,
    sample_id: u64,
    queue_tag: u64,
    present_seq: Option<u64>,
    engine_state_frame: Option<u64>,
    sample_phase: &'static str,
    source_chain_id: &'static str,
    sample_begin_qpc: i64,
    sample_end_qpc: i64,
    snapshot_batch_read_begin_qpc: i64,
    snapshot_batch_read_end_qpc: i64,
    prepared_binding_count: usize,
    status: &'static str,
    reason: Option<String>,
    position: Option<[f64; 3]>,
    rotation: Option<[f64; 4]>,
    stable_read_attempts: Option<usize>,
    audit_qpc: Option<i64>,
    native_transform_sha256: Option<String>,
    #[serde(skip)]
    native_words: Option<[u32; 8]>,
}
impl SampleRow {
    fn fingerprint(&mut self) {
        if let Some(words) = self.native_words.take() {
            let mut hash = Sha256::new();
            for word in words {
                hash.update(word.to_le_bytes());
            }
            self.native_transform_sha256 = Some(format!("sha256:{:x}", hash.finalize()));
        }
    }
}

// Never clone a sender out of State: finish() must be able to close/drain the worker while
// callbacks are in flight. Reject old run tokens even when a caller reuses generation IDs.
fn send(run: u64, generation: u64, event: Event) -> bool {
    if let Ok(state) = STATE.try_lock()
        && let Some(state) = state.as_ref()
        && state.run == run
        && state.generation == generation
        && ACTIVE.load(Ordering::Acquire) == run
    {
        if state.sender.try_send(event).is_ok() {
            return true;
        }
        note_loss();
    } else if ACTIVE.load(Ordering::Acquire) == run {
        note_loss();
    }
    false
}

pub(crate) fn sample_after_prepare(queue_tag: u64, prepared: &mut [PreparedCbvCopyShadow]) {
    let run = ACTIVE.load(Ordering::Acquire);
    if run == 0 {
        return;
    }
    let (first_read, last_read, binding_count) = prepared
        .iter()
        .filter_map(|p| p.diagnostic_snapshot)
        .filter(|t| t.run == run)
        .fold((i64::MAX, 0, 0usize), |(first, last, count), t| {
            (first.min(t.begin), last.max(t.end), count + 1)
        });
    if binding_count == 0 {
        return;
    }
    let (generation, config) = {
        let Ok(state) = STATE.try_lock() else {
            note_loss();
            return;
        };
        let Some(state) = state.as_ref().filter(|s| s.run == run) else {
            return;
        };
        (state.generation, state.early_config)
    };
    let begin = qpc();
    if begin <= 0 || begin < last_read {
        note_loss();
        return;
    }
    let result = config
        .addresses
        .ok_or_else(|| "exact player sampler unavailable".into())
        .and_then(|addresses| {
            diagnostic::sample(
                addresses,
                config.axis,
                config.scale,
                generation,
                begin,
                config.frequency,
            )
        });
    let end = qpc();
    if end < begin {
        note_loss();
        return;
    }
    let sample_id = NEXT_SAMPLE.fetch_add(1, Ordering::Relaxed);
    let token = SampleToken {
        run,
        generation,
        sample_id,
        queue_tag,
        begin,
    };
    let row = SampleRow {
        schema: "dm-cp2077-player-cbv-sample/v1",
        generation,
        sample_id,
        queue_tag,
        present_seq: None,
        engine_state_frame: None,
        sample_phase: "after_cbv_snapshot_batch_unlock_and_prepare_timing_before_original_execute",
        source_chain_id: crate::cp2077_player_probe::PROVIDER,
        sample_begin_qpc: begin,
        sample_end_qpc: end,
        snapshot_batch_read_begin_qpc: first_read,
        snapshot_batch_read_end_qpc: last_read,
        prepared_binding_count: binding_count,
        status: if result.is_ok() {
            "sampled"
        } else {
            "unavailable"
        },
        reason: result.as_ref().err().cloned(),
        position: result.as_ref().ok().map(|s| s.position),
        rotation: result.as_ref().ok().map(|s| s.rotation),
        stable_read_attempts: result.as_ref().ok().map(|s| s.stable_read_attempts),
        audit_qpc: result.as_ref().ok().map(|s| s.audit_qpc),
        native_words: result.as_ref().ok().map(|s| s.words),
        native_transform_sha256: None,
    };
    if send(run, generation, Event::EarlySample(row)) {
        for p in prepared {
            if p.diagnostic_snapshot.is_some_and(|t| t.run == run) {
                p.diagnostic_player = Some(token);
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct Binding {
    snapshot_read_begin_qpc: i64,
    snapshot_read_end_qpc: i64,
    copy_receipt: CopyReceipt,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct Bindings {
    #[serde(skip)]
    token: Option<SampleToken>,
    schema: &'static str,
    generation: u64,
    sample_id: u64,
    copies: Vec<Binding>,
    omitted_bindings: usize,
}
impl Bindings {
    pub(crate) fn observe(
        &mut self,
        sample: Option<SampleToken>,
        timing: Option<SnapshotTiming>,
        receipt: &CopyReceipt,
    ) {
        let (Some(sample), Some(timing)) = (sample, timing) else {
            return;
        };
        if sample.run != timing.run
            || sample.queue_tag != receipt.queue_tag
            || timing.end > sample.begin
            || self.token.is_some_and(|t| {
                (t.run, t.generation, t.sample_id)
                    != (sample.run, sample.generation, sample.sample_id)
            })
        {
            self.omitted_bindings += 1;
            if sample.run == ACTIVE.load(Ordering::Acquire) {
                note_loss();
            }
            return;
        }
        self.token = Some(sample);
        self.schema = "dm-cp2077-player-cbv-bindings/v1";
        self.generation = sample.generation;
        self.sample_id = sample.sample_id;
        if self.copies.len() == MAX_COPIES {
            self.omitted_bindings += 1;
            return;
        }
        self.copies.push(Binding {
            snapshot_read_begin_qpc: timing.begin,
            snapshot_read_end_qpc: timing.end,
            copy_receipt: receipt.clone(),
        });
    }
    pub(crate) fn submit(self) {
        if let Some(t) = self.token {
            send(t.run, t.generation, Event::EarlyBindings(self));
        }
    }
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize)]
struct CopyReference {
    queue_tag: u64,
    copy_submission: u64,
}

#[derive(Default, Debug, Serialize)]
pub(super) struct PresentLinks {
    copies: BTreeSet<CopyReference>,
    omitted_references: usize,
}
impl PresentLinks {
    pub(super) fn observe(&mut self, draw: &D3d12RenderDrawObservation) {
        if draw.pipeline_depth_write != Some(true) || !draw.copy_before_clear_submitted {
            return;
        }
        for matrix in &draw.matrices {
            let Some(r) = &matrix.copy_receipt else {
                continue;
            };
            if r.queue_tag != draw.queue_tag {
                self.omitted_references += 1;
                continue;
            }
            let key = CopyReference {
                queue_tag: r.queue_tag,
                copy_submission: r.copy_submission,
            };
            if self.copies.contains(&key) {
                continue;
            }
            if self.copies.len() == MAX_COPIES {
                self.omitted_references += 1;
                continue;
            }
            self.copies.insert(key);
        }
    }
}

#[derive(Default, Debug, Serialize)]
pub(super) struct Counts {
    sample_rows: u64,
    unavailable_samples: u64,
    binding_rows: u64,
    committed_bindings: u64,
    omitted_bindings: u64,
    present_link_rows: u64,
    present_copy_references: u64,
    omitted_present_references: u64,
}

pub(super) struct Writer {
    file: BufWriter<std::fs::File>,
    generation: u64,
    pub counts: Counts,
}
impl Writer {
    pub(super) fn new(
        root: &Path,
        identity: &depth_lab::StreamIdentity,
        config: Config,
    ) -> std::io::Result<Self> {
        let mut file = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(FILE))?,
        );
        write_line(
            &mut file,
            &serde_json::json!({
                "schema":"dm-cp2077-player-cbv-header/v1","identity":identity,"qpc_frequency":config.frequency,
                "diagnostic_only":true,"non_trainable":true,"production_rules_unchanged":true,
                "source_chain_id":crate::cp2077_player_probe::PROVIDER,"world_axis_mapping":config.axis,
                "position_units":"metres","metres_per_native_unit":config.scale,"quaternion_order":"xyzw",
                "engine_state_frame_known":false,"exact_player_chain_available":config.addresses.is_some(),
                "sample_scope":"once_per_Execute_batch_after_successful_CBV_snapshots; outside_tracker_lock; includes_audit_RPM_decode",
                "snapshot_read_qpc_scope":"exact_read_callback_including_bounds_query_allocation_and_byte_read; not_GPU_copy_time",
                "join":"identity + (queue_tag,copy_submission) from present-links to bindings, then sample_id; never nearest QPC or row adjacency",
                "present_link_scope":"observed_selected_DSV_PSO_depth_write_true_and_proven_before_selected_depth_copy",
                "unlinked_samples":"not_proven_used_by_selected_Present; retain_as_unlinked",
                "max_bindings_per_batch":MAX_COPIES,"max_copy_references_per_present":MAX_COPIES,
                "render_reference":"camera-pose-index.jsonl","late_player_reference":"cp2077-player-sample-timing.jsonl",
                "native_transform_fingerprint":"sha256_of_eight_u32_le_including_padding; not_engine_tick_or_atomic_snapshot"
            }),
        )?;
        file.flush()?;
        Ok(Self {
            file,
            generation: identity.generation,
            counts: Counts::default(),
        })
    }
    pub(super) fn sample(&mut self, mut row: SampleRow) -> std::io::Result<()> {
        row.fingerprint();
        write_line(&mut self.file, &row)?;
        self.counts.sample_rows += 1;
        self.counts.unavailable_samples += u64::from(row.status != "sampled");
        Ok(())
    }
    pub(super) fn bindings(&mut self, row: Bindings) -> std::io::Result<()> {
        write_line(&mut self.file, &row)?;
        self.counts.binding_rows += 1;
        self.counts.committed_bindings += row.copies.len() as u64;
        self.counts.omitted_bindings += row.omitted_bindings as u64;
        Ok(())
    }
    pub(super) fn present(&mut self, frame: &super::FamilyFrame) -> std::io::Result<()> {
        write_line(
            &mut self.file,
            &serde_json::json!({
                "schema":"dm-cp2077-player-cbv-present-links/v1","generation":self.generation,
                "present_seq":frame.present_seq,"present_qpc":frame.qpc,
                "copies":frame.early_links.copies,"omitted_references":frame.early_links.omitted_references,
                "tracker_loss_observed":frame.tracker_loss_observed
            }),
        )?;
        self.counts.present_link_rows += 1;
        self.counts.present_copy_references += frame.early_links.copies.len() as u64;
        self.counts.omitted_present_references += frame.early_links.omitted_references as u64;
        Ok(())
    }
    pub(super) fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests;
