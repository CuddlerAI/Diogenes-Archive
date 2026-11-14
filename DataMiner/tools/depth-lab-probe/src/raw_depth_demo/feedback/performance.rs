//! Fixed-size diagnostic checkpoints and wall timings. Never controls capture/admission.
use crate::hook_timing::{TimingCounter, TimingSnapshot};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Performance {
    pub advance: TimingCounter,
    pub gpu_initialization: TimingCounter,
    pub gpu_ready_and_snapshots: TimingCounter,
    pub local_camera: TimingCounter,
    pub candidate_materialize: TimingCounter,
    pub readback_drain_and_submit: TimingCounter,
    pub select_readback_submit: TimingCounter,
    source_armed: Option<Checkpoint>,
    selected_arm: Option<Checkpoint>,
    selected_seal: Option<Checkpoint>,
}

#[derive(Serialize)]
struct Checkpoint {
    qpc: i64,
    present_seq: u64,
    hook_timing: BTreeMap<String, TimingSnapshot>,
    feedback_timing: BTreeMap<String, TimingSnapshot>,
}

#[derive(Clone, Copy)]
pub(super) enum Phase {
    SourceArmed,
    SelectedArm,
    SelectedSeal,
}

impl Performance {
    pub fn checkpoint(
        &mut self,
        phase: Phase,
        qpc: i64,
        present_seq: u64,
        hooks: impl FnOnce() -> BTreeMap<String, TimingSnapshot>,
    ) {
        let slot = match phase {
            Phase::SourceArmed => &self.source_armed,
            Phase::SelectedArm => &self.selected_arm,
            Phase::SelectedSeal => &self.selected_seal,
        };
        if slot.is_some() {
            return;
        }
        let checkpoint = Checkpoint {
            qpc,
            present_seq,
            hook_timing: hooks(),
            feedback_timing: self.timings(),
        };
        *match phase {
            Phase::SourceArmed => &mut self.source_armed,
            Phase::SelectedArm => &mut self.selected_arm,
            Phase::SelectedSeal => &mut self.selected_seal,
        } = Some(checkpoint);
    }

    fn timings(&self) -> BTreeMap<String, TimingSnapshot> {
        BTreeMap::from([
            ("advance".into(), self.advance.snapshot()),
            (
                "gpu_initialization".into(),
                self.gpu_initialization.snapshot(),
            ),
            (
                "gpu_ready_and_snapshots".into(),
                self.gpu_ready_and_snapshots.snapshot(),
            ),
            ("local_camera".into(), self.local_camera.snapshot()),
            (
                "candidate_materialize".into(),
                self.candidate_materialize.snapshot(),
            ),
            (
                "readback_drain_and_submit".into(),
                self.readback_drain_and_submit.snapshot(),
            ),
            (
                "select_readback_submit".into(),
                self.select_readback_submit.snapshot(),
            ),
        ])
    }

    pub fn report(&self, qpc_frequency: i64) -> serde_json::Value {
        serde_json::json!({
            "schema":"dm-cp2077-raw-performance/v1",
            "non_trainable":true,"acceptance":false,
            "qpc_frequency":qpc_frequency,
            "measurement":"wall_elapsed_including_lock_wait; not_gpu_or_thread_cpu_time; nested_and_concurrent_stages_not_additive",
            "checkpoint_scope":"cumulative_completed_calls; relaxed_non_transactional_counters; calls_crossing_boundaries_can_complete_in_next_window",
            "phase_scope":"first_source_armed_Present_before_GPU_ring_allocation; selected_Arm_accepted_before_first_candidate; selected_Seal_accepted_before_final_drain; not_exact_RGB_start_or_end",
            "advance_excludes":"outer_raw_owner_lock_wait_and_camera_archive_enqueue; game_GPU_completion_time",
            "checkpoints":{
                "source_armed":self.source_armed,
                "selected_arm":self.selected_arm,
                "selected_seal":self.selected_seal,
            },
            "feedback_timing_at_report":self.timings(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoints_are_bounded_first_observations_and_keep_missing_phases_null() {
        let mut timing = Performance::default();
        timing
            .advance
            .record_duration(std::time::Duration::from_micros(7));
        timing.checkpoint(Phase::SourceArmed, 123, 4, BTreeMap::new);
        timing.checkpoint(Phase::SourceArmed, 456, 5, || panic!("must not resnapshot"));
        timing
            .advance
            .record_duration(std::time::Duration::from_micros(11));
        let report = timing.report(1000);
        assert_eq!(report["checkpoints"]["source_armed"]["qpc"], 123);
        assert_eq!(
            report["checkpoints"]["source_armed"]["feedback_timing"]["advance"]["total_us"],
            7
        );
        assert!(report["checkpoints"]["selected_arm"].is_null());
        assert!(report["checkpoints"]["selected_seal"].is_null());
        assert_eq!(
            report["feedback_timing_at_report"]["advance"]["total_us"],
            18
        );
        assert_eq!(report["acceptance"], false);
    }
}
