use super::*;
use crate::candidate_ring::{CandidateRingConfig, CandidateRingState, SelectPlan};
use std::collections::BTreeSet;

struct Run {
    state: FeedbackState,
    ring: CandidateRingState,
    materialized: u64,
    submitted: BTreeSet<u64>,
}

impl Run {
    fn new(first: u64) -> Self {
        let stream = StreamEpoch {
            identity: WireIdentity::new("raw-rgb-test", "r0000").unwrap(),
            generation: 1,
            resource_epoch: 1,
        };
        let mut state = FeedbackState::with_cadence(stream, DepthSelectionCadence::EvenCfrFrames);
        let mut ring = CandidateRingState::new(
            CandidateRingConfig {
                capacity: 96,
                max_feedback_lag_presents: 88,
                guard_slots: 8,
            },
            1,
            1,
        )
        .unwrap();
        let arm = Envelope {
            schema: FEEDBACK_SCHEMA_V4,
            stream: state.stream.clone(),
            ordinal: 1,
            payload: Decision::ArmGeneration,
        };
        state.stage_arm(&arm).unwrap();
        ring.arm().unwrap();
        ring.materialize(first).unwrap();
        state.apply_arm(arm, first).unwrap();
        Self {
            state,
            ring,
            materialized: first,
            submitted: BTreeSet::new(),
        }
    }

    fn envelope(&self, payload: Decision) -> Envelope<Decision> {
        Envelope {
            schema: FEEDBACK_SCHEMA_V4,
            stream: self.state.stream.clone(),
            ordinal: self.state.next_ordinal,
            payload,
        }
    }

    fn decision(frame: u64, present: u64, duplicate: bool) -> Decision {
        Decision::Select {
            session_frame: frame,
            present_seq: present,
            depth_required: frame.is_multiple_of(2),
            duplicate,
            source_superseded: 0,
            source_qpc: present as i64 * 10,
            callback_start_qpc: present as i64 * 10 + 1,
            callback_ready_qpc: present as i64 * 10 + 2,
            decision_qpc: present as i64 * 10 + 3,
        }
    }

    fn frame(&mut self, frame: u64, present: u64, duplicate: bool) -> Ack {
        for candidate in self.materialized + 1..=present {
            self.ring.materialize(candidate).unwrap();
        }
        self.materialized = present;
        let ack = self
            .state
            .apply(self.envelope(Self::decision(frame, present, duplicate)))
            .unwrap();
        if frame.is_multiple_of(2) {
            match self.ring.select(1, 1, present, present).unwrap() {
                SelectPlan::Submit(plan) => {
                    assert!(self.submitted.insert(present));
                    assert!(matches!(
                        ack,
                        Ack::SelectAccepted {
                            depth_already_selected: false,
                            ..
                        }
                    ));
                    self.ring.commit_submitted(plan).unwrap();
                }
                SelectPlan::Alias => assert!(matches!(
                    ack,
                    Ack::SelectAccepted {
                        depth_already_selected: true,
                        ..
                    }
                )),
            }
        }
        let retired = if self.submitted.contains(&present) {
            present
        } else {
            present - 1
        };
        self.state
            .apply(self.envelope(Decision::RetireThrough {
                present_seq: retired,
            }))
            .unwrap();
        self.ring.retire_through(retired).unwrap();
        ack
    }

    fn seal(&mut self, frames: u64) {
        self.state
            .apply(self.envelope(Decision::SealGeneration {
                final_session_frame: Some(frames - 1),
                expected_select_count: frames,
                expected_distinct_depth_count: self.submitted.len() as u64,
            }))
            .unwrap();
        self.ring.seal().unwrap();
        self.ring.begin_stop().unwrap();
        assert_eq!(self.ring.unresolved(), 0);
        assert_eq!(self.ring.stats().selected_messages, frames.div_ceil(2));
        assert_eq!(self.state.counts(), (frames, self.submitted.len() as u64));
    }
}

#[test]
fn odd_source_inherited_by_even_is_a_first_depth_submission() {
    let mut run = Run::new(100);
    run.frame(0, 100, false);
    run.frame(1, 101, false);
    let ack = run.frame(2, 101, true);
    assert!(matches!(
        ack,
        Ack::SelectAccepted {
            depth_required: true,
            depth_already_selected: false,
            ..
        }
    ));
    run.frame(3, 102, false); // replaced before the next required slot
    run.frame(4, 103, false);
    run.frame(5, 103, true);
    run.frame(6, 103, true); // repeated even slot is a depth alias
    run.frame(7, 104, false); // unselected final odd candidate is released by Seal
    assert_eq!(run.submitted, BTreeSet::from([100, 101, 103]));
    run.seal(8);
}

#[test]
fn timed_64_seconds_of_unique_rgb_stays_at_960_original_depths_and_wraps_ring() {
    let mut run = Run::new(100);
    for frame in 0..1920 {
        run.frame(frame, 100 + frame, false);
    }
    assert_eq!(run.submitted.len(), 960);
    run.seal(1920);
}

#[test]
fn last_even_slot_is_included_and_repeated_slots_do_not_create_originals() {
    let mut run = Run::new(100);
    for frame in 0..9 {
        run.frame(frame, 100, frame != 0);
    }
    assert_eq!(run.submitted.len(), 1);
    run.seal(9);
}

#[test]
fn premature_odd_retirement_wrong_cadence_and_missing_slot_fail_without_advancing() {
    let mut run = Run::new(100);
    run.frame(0, 100, false);
    run.frame(1, 101, false);
    let before = run.state.next_ordinal;
    assert!(
        run.state
            .apply(run.envelope(Decision::RetireThrough { present_seq: 101 }))
            .is_err()
    );
    assert_eq!(run.state.next_ordinal, before);
    assert!(
        run.state
            .apply(run.envelope(Run::decision(4, 101, true)))
            .is_err()
    );
    let mut wrong = Run::decision(2, 101, true);
    if let Decision::Select { depth_required, .. } = &mut wrong {
        *depth_required = false;
    }
    assert!(run.state.apply(run.envelope(wrong)).is_err());
    assert_eq!(run.state.next_ordinal, before);
    run.frame(2, 101, true);
    run.seal(3);
}

#[test]
fn v3_and_v4_cannot_be_mixed_or_silently_skip_depth() {
    let mut run = Run::new(100);
    let mut old = run.envelope(Run::decision(0, 100, false));
    old.schema = FEEDBACK_SCHEMA_V3;
    assert_eq!(run.state.apply(old), Err(ProtocolError::SchemaMismatch));
    let mut ordinary = FeedbackState::new(run.state.stream.clone());
    let new = run.envelope(Decision::ArmGeneration);
    assert_eq!(ordinary.apply(new), Err(ProtocolError::SchemaMismatch));
}

#[test]
#[ignore = "requires an external, immutable private RGB identity TSV"]
fn replay_private_rgb_identities_through_protocol_and_ring() {
    let path = std::env::var_os("CP2077_EVEN_CFR_REPLAY_TSV").expect("set private TSV path");
    let input = std::fs::read_to_string(path).unwrap();
    let rows: Vec<_> = input
        .lines()
        .map(|line| {
            let values: Vec<_> = line.split('\t').collect();
            (
                values[0].parse::<u64>().unwrap(),
                values[1].parse::<u64>().unwrap(),
                values[2] == "1",
            )
        })
        .collect();
    let mut run = Run::new(rows[0].1);
    let expected: BTreeSet<_> = rows
        .iter()
        .filter(|r| r.0.is_multiple_of(2))
        .map(|r| r.1)
        .collect();
    for &(frame, present, duplicate) in &rows {
        run.frame(frame, present, duplicate);
    }
    assert_eq!(run.submitted, expected);
    run.seal(rows.len() as u64);
    eprintln!(
        "private replay: {} RGB slots, {} required slots, {} original depths",
        rows.len(),
        rows.len().div_ceil(2),
        expected.len()
    );
}
