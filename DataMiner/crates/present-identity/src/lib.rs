//! Platform-neutral identity contract shared by the injected Present hook,
//! the WGC recorder and the offline verifier.

pub mod candidate_ring;
pub mod feedback;
#[cfg(windows)]
pub mod transport;
pub mod watermark;

const LOW16_PERIOD: u64 = 1 << 16;
pub const MAX_FORWARD_ACCEPT_BUDGET: u64 = LOW16_PERIOD / 2 - 1;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequenceDecodeError {
    #[error("present sequence accept budget must be in 1..32768, got {0}")]
    InvalidAcceptBudget(u64),
    #[error("present sequence baseline plus accept budget overflows u64")]
    BaselineOverflow,
    #[error("stale present sequence budget must be below 32768, got {0}")]
    InvalidStaleBudget(u64),
    #[error(
        "low16 present sequence {raw} has no candidate in the accepted full-sequence interval [{lower}, {upper}]"
    )]
    OutsideAcceptedInterval { raw: u16, lower: u64, upper: u64 },
    #[error("present sequence observation belongs to generation {actual}, expected {expected}")]
    GenerationMismatch { expected: u64, actual: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceObservation {
    Accepted(u64),
    Stale { present_seq: u64, lag: u64 },
}

/// Expands low-16 watermark observations only inside a bounded forward interval.
///
/// `baseline` is the probe's full `first_present_seq` from the Ready ACK. The
/// first WGC observation may be that Present or a later one; it must not precede
/// the baseline. Every later observation may repeat the last identity, but may
/// not move backwards or jump farther than `accept_budget` Presents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullSequenceDecoder {
    generation: u64,
    lower_bound: u64,
    accept_budget: u64,
    previous: Option<u64>,
}

impl FullSequenceDecoder {
    pub fn new(
        generation: u64,
        baseline: u64,
        accept_budget: u64,
    ) -> Result<Self, SequenceDecodeError> {
        if !(1..=MAX_FORWARD_ACCEPT_BUDGET).contains(&accept_budget) {
            return Err(SequenceDecodeError::InvalidAcceptBudget(accept_budget));
        }
        baseline
            .checked_add(accept_budget)
            .ok_or(SequenceDecodeError::BaselineOverflow)?;
        Ok(Self {
            generation,
            lower_bound: baseline,
            accept_budget,
            previous: None,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn previous(&self) -> Option<u64> {
        self.previous
    }

    pub fn observe(&mut self, generation: u64, raw: u16) -> Result<u64, SequenceDecodeError> {
        match self.observe_with_stale_budget(generation, raw, 0)? {
            SequenceObservation::Accepted(present_seq) => Ok(present_seq),
            SequenceObservation::Stale { .. } => {
                unreachable!("zero stale budget cannot classify a stale observation")
            }
        }
    }

    /// Expands a forward observation or identifies a narrowly bounded stale
    /// observation without advancing decoder state.
    ///
    /// WGC may occasionally deliver an older composed surface after a newer
    /// one. Callers may recycle that observation before selection, but must not
    /// reuse its identity or treat it as the current source. A regression beyond
    /// `stale_budget` remains fatal.
    pub fn observe_with_stale_budget(
        &mut self,
        generation: u64,
        raw: u16,
        stale_budget: u64,
    ) -> Result<SequenceObservation, SequenceDecodeError> {
        if generation != self.generation {
            return Err(SequenceDecodeError::GenerationMismatch {
                expected: self.generation,
                actual: generation,
            });
        }
        if stale_budget > MAX_FORWARD_ACCEPT_BUDGET {
            return Err(SequenceDecodeError::InvalidStaleBudget(stale_budget));
        }
        let lower = self.previous.unwrap_or(self.lower_bound);
        let upper = lower
            .checked_add(self.accept_budget)
            .ok_or(SequenceDecodeError::BaselineOverflow)?;
        let candidate = candidate_at_or_after(lower, raw);
        if let Some(candidate) = candidate.filter(|candidate| *candidate <= upper) {
            self.previous = Some(candidate);
            return Ok(SequenceObservation::Accepted(candidate));
        }
        if let Some(previous) = self.previous
            && let Some(stale) = candidate_at_or_before(previous, raw)
            && stale < previous
        {
            let lag = previous - stale;
            if lag <= stale_budget {
                return Ok(SequenceObservation::Stale {
                    present_seq: stale,
                    lag,
                });
            }
        }
        Err(SequenceDecodeError::OutsideAcceptedInterval { raw, lower, upper })
    }
}

fn candidate_at_or_after(lower: u64, raw: u16) -> Option<u64> {
    let epoch = lower / LOW16_PERIOD;
    let same_epoch = epoch
        .checked_mul(LOW16_PERIOD)?
        .checked_add(u64::from(raw))?;
    if same_epoch >= lower {
        Some(same_epoch)
    } else {
        same_epoch.checked_add(LOW16_PERIOD)
    }
}

fn candidate_at_or_before(upper: u64, raw: u16) -> Option<u64> {
    let epoch = upper / LOW16_PERIOD;
    let same_epoch = epoch
        .checked_mul(LOW16_PERIOD)?
        .checked_add(u64::from(raw))?;
    if same_epoch <= upper {
        Some(same_epoch)
    } else {
        same_epoch.checked_sub(LOW16_PERIOD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_can_equal_first_observation() {
        let mut decoder = FullSequenceDecoder::new(7, 100, 88).unwrap();
        assert_eq!(decoder.observe(7, 100).unwrap(), 100);
    }

    #[test]
    fn startup_unwrap_window_is_independent_from_the_ring_lag_budget() {
        let baseline = 3 * LOW16_PERIOD + 1_000;
        let first_wgc_observation = baseline + 2_400;
        let mut decoder = FullSequenceDecoder::new(7, baseline, MAX_FORWARD_ACCEPT_BUDGET).unwrap();
        assert_eq!(
            decoder.observe(7, first_wgc_observation as u16).unwrap(),
            first_wgc_observation
        );
    }

    #[test]
    fn baseline_anchors_a_random_high_epoch_and_wrap() {
        let baseline = 9 * LOW16_PERIOD + 65_530;
        let mut decoder = FullSequenceDecoder::new(7, baseline, 88).unwrap();
        assert_eq!(decoder.observe(7, 65_534).unwrap(), baseline + 4);
        assert_eq!(decoder.observe(7, 2).unwrap(), baseline + 8);
    }

    #[test]
    fn repeated_observation_is_allowed() {
        let mut decoder = FullSequenceDecoder::new(3, 42, 88).unwrap();
        assert_eq!(decoder.observe(3, 42).unwrap(), 42);
        assert_eq!(decoder.observe(3, 42).unwrap(), 42);
    }

    #[test]
    fn backward_and_oversized_jumps_fail_closed() {
        let mut decoder = FullSequenceDecoder::new(3, 65_530, 88).unwrap();
        assert_eq!(decoder.observe(3, 65_534).unwrap(), 65_534);
        assert!(matches!(
            decoder.observe(3, 65_533),
            Err(SequenceDecodeError::OutsideAcceptedInterval { .. })
        ));
        assert!(matches!(
            decoder.observe(3, 100),
            Err(SequenceDecodeError::OutsideAcceptedInterval { .. })
        ));
    }

    #[test]
    fn bounded_stale_observation_is_reported_without_advancing_state() {
        let mut decoder = FullSequenceDecoder::new(3, 44_260, 88).unwrap();
        assert_eq!(decoder.observe(3, 44_264).unwrap(), 44_264);
        assert_eq!(
            decoder.observe_with_stale_budget(3, 44_263, 8).unwrap(),
            SequenceObservation::Stale {
                present_seq: 44_263,
                lag: 1,
            }
        );
        assert_eq!(decoder.previous(), Some(44_264));
        assert_eq!(decoder.observe(3, 44_265).unwrap(), 44_265);
    }

    #[test]
    fn bounded_stale_observation_is_unambiguous_across_low16_wrap() {
        let mut decoder = FullSequenceDecoder::new(3, 65_530, 88).unwrap();
        assert_eq!(decoder.observe(3, 2).unwrap(), 65_538);
        assert_eq!(
            decoder.observe_with_stale_budget(3, 65_535, 8).unwrap(),
            SequenceObservation::Stale {
                present_seq: 65_535,
                lag: 3,
            }
        );
        assert_eq!(decoder.previous(), Some(65_538));
    }

    #[test]
    fn stale_observation_beyond_budget_remains_fatal() {
        let mut decoder = FullSequenceDecoder::new(3, 44_260, 88).unwrap();
        assert_eq!(decoder.observe(3, 44_264).unwrap(), 44_264);
        assert!(matches!(
            decoder.observe_with_stale_budget(3, 44_255, 8),
            Err(SequenceDecodeError::OutsideAcceptedInterval { .. })
        ));
        assert_eq!(decoder.previous(), Some(44_264));
    }

    #[test]
    fn exact_stale_budget_boundary_is_accepted() {
        let mut decoder = FullSequenceDecoder::new(3, 44_260, 88).unwrap();
        assert_eq!(decoder.observe(3, 44_264).unwrap(), 44_264);
        assert_eq!(
            decoder.observe_with_stale_budget(3, 44_256, 8).unwrap(),
            SequenceObservation::Stale {
                present_seq: 44_256,
                lag: 8,
            }
        );
        assert_eq!(decoder.previous(), Some(44_264));
    }

    #[test]
    fn first_observation_cannot_precede_the_ready_baseline() {
        let mut decoder = FullSequenceDecoder::new(3, 44_264, 88).unwrap();
        assert!(matches!(
            decoder.observe_with_stale_budget(3, 44_263, 8),
            Err(SequenceDecodeError::OutsideAcceptedInterval { .. })
        ));
        assert_eq!(decoder.previous(), None);
    }

    #[test]
    fn stale_budget_must_preserve_low16_uniqueness() {
        let mut decoder = FullSequenceDecoder::new(3, 42, 88).unwrap();
        assert_eq!(
            decoder.observe_with_stale_budget(3, 42, LOW16_PERIOD / 2),
            Err(SequenceDecodeError::InvalidStaleBudget(LOW16_PERIOD / 2))
        );
    }

    #[test]
    fn generation_must_recreate_the_decoder_with_a_fresh_baseline() {
        let mut first = FullSequenceDecoder::new(10, 65_530, 88).unwrap();
        assert_eq!(first.observe(10, 1).unwrap(), 65_537);
        assert_eq!(
            first.observe(11, 2),
            Err(SequenceDecodeError::GenerationMismatch {
                expected: 10,
                actual: 11,
            })
        );

        let mut resumed = FullSequenceDecoder::new(11, 4 * LOW16_PERIOD + 20, 88).unwrap();
        assert_eq!(resumed.observe(11, 24).unwrap(), 4 * LOW16_PERIOD + 24);
    }

    #[test]
    fn half_period_or_larger_budget_is_rejected() {
        assert_eq!(
            FullSequenceDecoder::new(1, 0, LOW16_PERIOD / 2),
            Err(SequenceDecodeError::InvalidAcceptBudget(LOW16_PERIOD / 2))
        );
    }
}
