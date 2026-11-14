//! Pure state machine for the probe's GPU-only depth candidate ring.

use crate::feedback::FaultReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateRingConfig {
    pub capacity: u32,
    pub max_feedback_lag_presents: u32,
    pub guard_slots: u32,
}

impl CandidateRingConfig {
    pub fn validate(self) -> Result<(), RingFault> {
        if self.capacity == 0
            || self.max_feedback_lag_presents == 0
            || self.max_feedback_lag_presents.checked_add(self.guard_slots) != Some(self.capacity)
        {
            return Err(RingFault::new(FaultReason::VramBudgetInsufficient));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Empty,
    Candidate { present_seq: u64 },
    SubmitPending { present_seq: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmitPlan {
    pub slot: usize,
    pub present_seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectPlan {
    Submit(SubmitPlan),
    Alias,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CandidateRingStats {
    pub materialized: u64,
    pub selected_messages: u64,
    pub distinct_selected: u64,
    pub source_identity_aliases: u64,
    pub retired_candidates: u64,
    pub submitted: u64,
    pub max_unresolved: u32,
    pub max_feedback_lag_presents: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("candidate ring fault: {reason:?}")]
pub struct RingFault {
    pub reason: FaultReason,
}

impl RingFault {
    const fn new(reason: FaultReason) -> Self {
        Self { reason }
    }
}

#[derive(Clone, Debug)]
pub struct CandidateRingState {
    config: CandidateRingConfig,
    generation: u64,
    resource_epoch: u64,
    slots: Vec<Slot>,
    last_selected_present_seq: Option<u64>,
    retired_through: Option<u64>,
    first_materialized: Option<u64>,
    last_materialized: Option<u64>,
    armed: bool,
    sealed: bool,
    fault: Option<RingFault>,
    stats: CandidateRingStats,
}

impl CandidateRingState {
    pub fn new(
        config: CandidateRingConfig,
        generation: u64,
        resource_epoch: u64,
    ) -> Result<Self, RingFault> {
        config.validate()?;
        if generation == 0 || resource_epoch == 0 {
            return Err(RingFault::new(FaultReason::IdentityMismatch));
        }
        Ok(Self {
            config,
            generation,
            resource_epoch,
            slots: vec![Slot::Empty; config.capacity as usize],
            last_selected_present_seq: None,
            retired_through: None,
            first_materialized: None,
            last_materialized: None,
            armed: false,
            sealed: false,
            fault: None,
            stats: CandidateRingStats::default(),
        })
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn resource_epoch(&self) -> u64 {
        self.resource_epoch
    }

    #[must_use]
    pub const fn stats(&self) -> CandidateRingStats {
        self.stats
    }

    #[must_use]
    pub fn unresolved(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !matches!(slot, Slot::Empty))
            .count()
    }

    pub fn materialize(&mut self, present_seq: u64) -> Result<usize, RingFault> {
        self.ensure_live()?;
        if !self.armed {
            return Err(self.latch(FaultReason::CandidateBeforeArm));
        }
        if self.sealed {
            return Err(self.latch(FaultReason::SelectAfterSeal));
        }
        if self
            .last_materialized
            .is_some_and(|previous| present_seq <= previous)
        {
            return Err(self.latch(FaultReason::RingIdentityMismatch));
        }
        let slot_index = present_seq as usize % self.slots.len();
        if !matches!(self.slots[slot_index], Slot::Empty) {
            return Err(self.latch(FaultReason::RingOverwriteUnresolved));
        }
        self.slots[slot_index] = Slot::Candidate { present_seq };
        self.first_materialized.get_or_insert(present_seq);
        self.last_materialized = Some(present_seq);
        self.stats.materialized = self.stats.materialized.saturating_add(1);
        self.stats.max_unresolved = self.stats.max_unresolved.max(self.unresolved() as u32);
        Ok(slot_index)
    }

    pub fn select(
        &mut self,
        generation: u64,
        resource_epoch: u64,
        selected_present_seq: u64,
        current_present_seq: u64,
    ) -> Result<SelectPlan, RingFault> {
        self.ensure_identity(generation, resource_epoch)?;
        self.ensure_live()?;
        if !self.armed {
            return Err(self.latch(FaultReason::FeedbackBeforeArm));
        }
        if self.sealed {
            return Err(self.latch(FaultReason::SelectAfterSeal));
        }
        self.stats.selected_messages = self.stats.selected_messages.saturating_add(1);
        if self
            .last_selected_present_seq
            .is_some_and(|previous| selected_present_seq < previous)
        {
            return Err(self.latch(FaultReason::PresentSeqRegressed));
        }
        let lag = current_present_seq
            .checked_sub(selected_present_seq)
            .ok_or_else(|| self.latch(FaultReason::RingIdentityMismatch))?;
        self.stats.max_feedback_lag_presents = self.stats.max_feedback_lag_presents.max(lag);
        if lag > u64::from(self.config.max_feedback_lag_presents) {
            return Err(self.latch(FaultReason::SelectedFeedbackLate));
        }
        if self.last_selected_present_seq == Some(selected_present_seq) {
            self.stats.source_identity_aliases =
                self.stats.source_identity_aliases.saturating_add(1);
            return Ok(SelectPlan::Alias);
        }
        if self
            .retired_through
            .is_some_and(|retired| selected_present_seq <= retired)
        {
            return Err(self.latch(FaultReason::SelectAfterRetire));
        }
        let slot = selected_present_seq as usize % self.slots.len();
        if self.slots[slot]
            != (Slot::Candidate {
                present_seq: selected_present_seq,
            })
        {
            return Err(self.latch(FaultReason::RingIdentityMismatch));
        }
        self.slots[slot] = Slot::SubmitPending {
            present_seq: selected_present_seq,
        };
        self.last_selected_present_seq = Some(selected_present_seq);
        self.stats.distinct_selected = self.stats.distinct_selected.saturating_add(1);
        Ok(SelectPlan::Submit(SubmitPlan {
            slot,
            present_seq: selected_present_seq,
        }))
    }

    pub fn commit_submitted(&mut self, plan: SubmitPlan) -> Result<(), RingFault> {
        self.ensure_live()?;
        if self.slots.get(plan.slot).copied()
            != Some(Slot::SubmitPending {
                present_seq: plan.present_seq,
            })
        {
            return Err(self.latch(FaultReason::RingIdentityMismatch));
        }
        self.slots[plan.slot] = Slot::Empty;
        self.stats.submitted = self.stats.submitted.saturating_add(1);
        Ok(())
    }

    pub fn retire_through(&mut self, present_seq: u64) -> Result<(), RingFault> {
        self.ensure_live()?;
        if !self.armed {
            return Err(self.latch(FaultReason::FeedbackBeforeArm));
        }
        if self.sealed {
            return Err(self.latch(FaultReason::SelectAfterSeal));
        }
        if self
            .retired_through
            .is_some_and(|retired| present_seq < retired)
        {
            return Err(self.latch(FaultReason::RetireFrontierRegressed));
        }
        for slot in &mut self.slots {
            if matches!(slot, Slot::Candidate { present_seq: seq } if *seq <= present_seq) {
                *slot = Slot::Empty;
                self.stats.retired_candidates = self.stats.retired_candidates.saturating_add(1);
            }
        }
        self.retired_through = Some(present_seq);
        Ok(())
    }

    pub fn seal(&mut self) -> Result<(), RingFault> {
        self.ensure_live()?;
        if self.sealed {
            return Err(self.latch(FaultReason::SealCountMismatch));
        }
        if self
            .slots
            .iter()
            .any(|slot| matches!(slot, Slot::SubmitPending { .. }))
        {
            return Err(self.latch(FaultReason::FlushCountMismatch));
        }
        for slot in &mut self.slots {
            if matches!(slot, Slot::Candidate { .. }) {
                *slot = Slot::Empty;
                self.stats.retired_candidates = self.stats.retired_candidates.saturating_add(1);
            }
        }
        self.sealed = true;
        Ok(())
    }

    /// Opens the candidate materialization gate. The first candidate identity
    /// is established by the next successful `materialize`, never predicted by
    /// this transition.
    pub fn arm(&mut self) -> Result<(), RingFault> {
        self.ensure_live()?;
        if self.sealed {
            return Err(self.latch(FaultReason::ArmAfterSeal));
        }
        if self.armed {
            return Err(self.latch(FaultReason::ArmRepeated));
        }
        self.armed = true;
        Ok(())
    }

    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    #[must_use]
    pub const fn first_materialized_present_seq(&self) -> Option<u64> {
        self.first_materialized
    }

    /// Pure lifecycle gate mirrored by the probe control thread. Stop is only
    /// legal after Seal has consumed every earlier decision and cleared the
    /// candidate ring; racing Stop against an unsealed generation is fatal.
    pub fn begin_stop(&mut self) -> Result<(), RingFault> {
        self.ensure_live()?;
        if !self.sealed {
            return Err(self.latch(FaultReason::StopBeforeGenerationSealed));
        }
        if self.unresolved() != 0 {
            return Err(self.latch(FaultReason::FlushCountMismatch));
        }
        Ok(())
    }

    pub fn resize_after_ready(&mut self, readback_in_flight: usize) -> Result<(), RingFault> {
        self.ensure_live()?;
        let reason = if self.unresolved() > 0 || readback_in_flight > 0 {
            FaultReason::ResizeAfterReady
        } else {
            FaultReason::ResourceEpochMismatch
        };
        Err(self.latch(reason))
    }

    fn ensure_identity(&mut self, generation: u64, resource_epoch: u64) -> Result<(), RingFault> {
        if generation != self.generation {
            return Err(self.latch(FaultReason::GenerationMismatch));
        }
        if resource_epoch != self.resource_epoch {
            return Err(self.latch(FaultReason::ResourceEpochMismatch));
        }
        Ok(())
    }

    fn ensure_live(&self) -> Result<(), RingFault> {
        self.fault.map_or(Ok(()), Err)
    }

    fn latch(&mut self, reason: FaultReason) -> RingFault {
        let fault = self.fault.unwrap_or_else(|| RingFault::new(reason));
        self.fault = Some(fault);
        fault
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> CandidateRingState {
        let mut ring = CandidateRingState::new(
            CandidateRingConfig {
                capacity: 96,
                max_feedback_lag_presents: 88,
                guard_slots: 8,
            },
            4,
            2,
        )
        .unwrap();
        ring.arm().unwrap();
        ring
    }

    fn unarmed_ring() -> CandidateRingState {
        CandidateRingState::new(
            CandidateRingConfig {
                capacity: 96,
                max_feedback_lag_presents: 88,
                guard_slots: 8,
            },
            4,
            2,
        )
        .unwrap()
    }

    #[test]
    fn pre_arm_candidate_and_feedback_fail_closed_with_specific_reasons() {
        assert_eq!(
            unarmed_ring().materialize(10).unwrap_err().reason,
            FaultReason::CandidateBeforeArm
        );
        assert_eq!(
            unarmed_ring().select(4, 2, 10, 10).unwrap_err().reason,
            FaultReason::FeedbackBeforeArm
        );
        assert_eq!(
            unarmed_ring().retire_through(10).unwrap_err().reason,
            FaultReason::FeedbackBeforeArm
        );
    }

    #[test]
    fn arm_is_single_shot_and_first_candidate_is_observed_not_predicted() {
        let mut state = unarmed_ring();
        assert_eq!(state.first_materialized_present_seq(), None);
        state.arm().unwrap();
        assert_eq!(state.arm().unwrap_err().reason, FaultReason::ArmRepeated);

        let mut state = unarmed_ring();
        state.arm().unwrap();
        state.materialize(1234).unwrap();
        state.materialize(1235).unwrap();
        assert_eq!(state.first_materialized_present_seq(), Some(1234));
    }

    #[test]
    fn zero_select_pre_arm_seal_and_stop_are_legal_but_arm_after_seal_faults() {
        let mut state = unarmed_ring();
        state.seal().unwrap();
        state.begin_stop().unwrap();

        let mut sealed = unarmed_ring();
        sealed.seal().unwrap();
        assert_eq!(sealed.arm().unwrap_err().reason, FaultReason::ArmAfterSeal);
    }

    #[test]
    fn feedback_lag_88_is_accepted_and_89_fails_closed() {
        let mut accepted = ring();
        accepted.materialize(100).unwrap();
        assert!(matches!(
            accepted.select(4, 2, 100, 188).unwrap(),
            SelectPlan::Submit(_)
        ));

        let mut late = ring();
        late.materialize(100).unwrap();
        assert_eq!(
            late.select(4, 2, 100, 189).unwrap_err().reason,
            FaultReason::SelectedFeedbackLate
        );
    }

    #[test]
    fn physical_wrap_requires_the_old_candidate_to_be_retired() {
        let mut state = ring();
        state.materialize(1).unwrap();
        for seq in 2..=96 {
            state.materialize(seq).unwrap();
        }
        assert_eq!(
            state.materialize(97).unwrap_err().reason,
            FaultReason::RingOverwriteUnresolved
        );

        let mut retired = ring();
        retired.materialize(1).unwrap();
        retired.retire_through(1).unwrap();
        for seq in 2..=97 {
            retired.materialize(seq).unwrap();
            retired.retire_through(seq).unwrap();
        }
    }

    #[test]
    fn duplicate_alias_does_not_submit_twice() {
        let mut state = ring();
        state.materialize(500).unwrap();
        let SelectPlan::Submit(plan) = state.select(4, 2, 500, 501).unwrap() else {
            panic!("first selection was not submitted");
        };
        state.commit_submitted(plan).unwrap();
        assert_eq!(state.select(4, 2, 500, 502).unwrap(), SelectPlan::Alias);
        assert_eq!(state.stats().submitted, 1);
        assert_eq!(state.stats().source_identity_aliases, 1);
    }

    #[test]
    fn duplicate_alias_is_still_subject_to_the_feedback_lag_budget() {
        let mut state = ring();
        state.materialize(500).unwrap();
        let SelectPlan::Submit(plan) = state.select(4, 2, 500, 501).unwrap() else {
            panic!("first selection was not submitted");
        };
        state.commit_submitted(plan).unwrap();
        assert_eq!(
            state.select(4, 2, 500, 589).unwrap_err().reason,
            FaultReason::SelectedFeedbackLate
        );
        assert_eq!(state.stats().max_feedback_lag_presents, 89);
    }

    #[test]
    fn alias_must_be_the_immediately_previous_selected_identity() {
        let mut state = ring();
        state.materialize(100).unwrap();
        let first = match state.select(4, 2, 100, 101).unwrap() {
            SelectPlan::Submit(plan) => plan,
            SelectPlan::Alias => panic!("first selection cannot alias"),
        };
        state.commit_submitted(first).unwrap();
        state.materialize(200).unwrap();
        let second = match state.select(4, 2, 200, 201).unwrap() {
            SelectPlan::Submit(plan) => plan,
            SelectPlan::Alias => panic!("new selection cannot alias"),
        };
        state.commit_submitted(second).unwrap();
        assert_eq!(
            state.select(4, 2, 100, 202).unwrap_err().reason,
            FaultReason::PresentSeqRegressed
        );
    }

    #[test]
    fn identity_mismatch_retire_regression_and_resize_are_sticky_faults() {
        let mut generation = ring();
        generation.materialize(10).unwrap();
        assert_eq!(
            generation.select(5, 2, 10, 10).unwrap_err().reason,
            FaultReason::GenerationMismatch
        );
        assert_eq!(
            generation.retire_through(10).unwrap_err().reason,
            FaultReason::GenerationMismatch
        );

        let mut retire = ring();
        retire.materialize(10).unwrap();
        retire.retire_through(10).unwrap();
        assert_eq!(
            retire.retire_through(9).unwrap_err().reason,
            FaultReason::RetireFrontierRegressed
        );

        let mut resize = ring();
        resize.materialize(10).unwrap();
        assert_eq!(
            resize.resize_after_ready(1).unwrap_err().reason,
            FaultReason::ResizeAfterReady
        );
    }

    #[test]
    fn seal_requires_submissions_and_clears_unselected_candidates() {
        let mut pending = ring();
        pending.materialize(20).unwrap();
        let _ = pending.select(4, 2, 20, 20).unwrap();
        assert_eq!(
            pending.seal().unwrap_err().reason,
            FaultReason::FlushCountMismatch
        );

        let mut state = ring();
        state.materialize(20).unwrap();
        state.materialize(21).unwrap();
        let SelectPlan::Submit(plan) = state.select(4, 2, 20, 21).unwrap() else {
            unreachable!()
        };
        state.commit_submitted(plan).unwrap();
        state.seal().unwrap();
        assert_eq!(state.unresolved(), 0);
        assert_eq!(state.stats().retired_candidates, 1);
        state.begin_stop().unwrap();
    }

    #[test]
    fn stop_racing_seal_is_a_sticky_fail_closed_fault() {
        let mut state = ring();
        state.materialize(20).unwrap();
        assert_eq!(
            state.begin_stop().unwrap_err().reason,
            FaultReason::StopBeforeGenerationSealed
        );
        assert_eq!(
            state.seal().unwrap_err().reason,
            FaultReason::StopBeforeGenerationSealed
        );
    }
}
