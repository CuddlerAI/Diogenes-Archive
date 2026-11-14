//! Wraps the EXISTING formal player sampler once; never performs another memory read.
use super::{ACTIVE, Event, Ordering, STATE, note_loss, qpc};
use crate::player_writer::PlayerRootSample;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
pub(super) struct Row {
    schema: &'static str,
    generation: u64,
    present_seq: u64,
    present_qpc: i64,
    sample_begin_qpc: i64,
    sample_end_qpc: i64,
    sample_phase: &'static str,
    source_chain_id: &'static str,
    status: &'static str,
    position: Option<[f64; 3]>,
    rotation: Option<[f64; 4]>,
    // No engine tick/version is available; an inherited Present anchor does not supply one.
    engine_state_frame: Option<u64>,
    decoded_pose_sha256: Option<String>,
}

impl Row {
    pub(super) fn fingerprint_on_writer(&mut self) {
        if let (Some(position), Some(rotation)) = (self.position, self.rotation) {
            let mut hash = Sha256::new();
            for value in position.into_iter().chain(rotation) {
                hash.update(value.to_le_bytes());
            }
            self.decoded_pose_sha256 = Some(format!("sha256:{hash:x}", hash = hash.finalize()));
        }
    }
}

pub(crate) fn trace(
    present_seq: u64,
    present_qpc: i64,
    generation: u64,
    sample: impl FnOnce() -> Result<PlayerRootSample, String>,
) -> Result<PlayerRootSample, String> {
    let run = ACTIVE.load(Ordering::Acquire);
    if run == 0 {
        return sample();
    }
    let begin = qpc();
    let result = sample();
    let end = qpc();
    if ACTIVE.load(Ordering::Acquire) != run {
        return result;
    }
    if begin <= 0 || end < begin || present_qpc <= 0 || present_seq == 0 {
        note_loss();
        return result;
    }
    let row = Row {
        schema: "dm-cp2077-player-sample-timing/v1",
        generation,
        present_seq,
        present_qpc,
        sample_begin_qpc: begin,
        sample_end_qpc: end,
        sample_phase: "formal_on_d3d12_pose_after_render_tracker_before_original_present",
        source_chain_id: crate::cp2077_player_probe::PROVIDER,
        status: if result.is_ok() {
            "sampled"
        } else {
            "unavailable"
        },
        position: result.as_ref().ok().map(|p| p.player_position),
        rotation: result.as_ref().ok().map(|p| p.player_rotation_quaternion),
        engine_state_frame: None,
        decoded_pose_sha256: None,
    };
    if let Ok(mut state) = STATE.try_lock()
        && let Some(state) = state.as_mut()
        && state.run == run
        && state.generation == generation
        && !state
            .last_player_present
            .is_some_and(|last| present_seq <= last)
    {
        state.last_player_present = Some(present_seq);
        if state.sender.try_send(Event::Player(row)).is_err() {
            note_loss();
        }
    } else {
        note_loss();
    }
    result
}
