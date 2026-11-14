use thiserror::Error;

use crate::contract::{
    BuildIdentityV1, ContractShapeErrorV1, CoordinateTransformV1, InvalidReasonV1, TelemetryFactV1,
    ValidityStateV1,
};
use crate::producer::ExpectedRuntimeV1;

#[derive(Clone, Debug)]
pub struct TelemetryConsumerV1 {
    expected: ExpectedRuntimeV1,
    last_game_update_seq: Option<u64>,
    last_sample_qpc: Option<u64>,
    poisoned: bool,
}

impl TelemetryConsumerV1 {
    #[must_use]
    pub fn new(expected: ExpectedRuntimeV1) -> Self {
        Self {
            expected,
            last_game_update_seq: None,
            last_sample_qpc: None,
            poisoned: false,
        }
    }

    /// Accepts exactly one JSON-line fact. Any failure poisons this run so a
    /// caller cannot silently resume after a gap, drift, stale fact, or old run.
    pub fn accept_json_line(
        &mut self,
        payload: &[u8],
        now_qpc: u64,
    ) -> Result<TelemetryFactV1, ConsumerErrorV1> {
        if self.poisoned {
            return Err(ConsumerErrorV1::StreamPoisoned);
        }
        let fact: TelemetryFactV1 = serde_json::from_slice(payload)
            .map_err(|error| self.fail(ConsumerErrorV1::Json(error.to_string())))?;
        self.accept_fact(fact, now_qpc)
    }

    pub fn accept_fact(
        &mut self,
        fact: TelemetryFactV1,
        now_qpc: u64,
    ) -> Result<TelemetryFactV1, ConsumerErrorV1> {
        if self.poisoned {
            return Err(ConsumerErrorV1::StreamPoisoned);
        }
        fact.validate_shape()
            .map_err(|error| self.fail(ConsumerErrorV1::Shape(error)))?;
        if fact.run_id != self.expected.run_id {
            return Err(self.fail(ConsumerErrorV1::OldRun {
                expected: self.expected.run_id.clone(),
                actual: fact.run_id,
            }));
        }
        require_build(&self.expected.build, &fact.build).map_err(|error| self.fail(error))?;
        if fact.coordinate_transform != self.expected.coordinate_transform {
            return Err(self.fail(ConsumerErrorV1::CoordinateTransformMismatch));
        }
        if fact.qpc_frequency_hz != self.expected.qpc_frequency_hz {
            return Err(self.fail(ConsumerErrorV1::QpcFrequencyMismatch));
        }
        if fact.validity.state != ValidityStateV1::Valid {
            return Err(self.fail(ConsumerErrorV1::InvalidState {
                reason: fact.validity.reason,
            }));
        }
        if fact.sample_qpc > now_qpc {
            return Err(self.fail(ConsumerErrorV1::QpcInFuture));
        }
        if now_qpc - fact.sample_qpc > self.expected.max_sample_age_qpc {
            return Err(self.fail(ConsumerErrorV1::StaleQpc));
        }
        if let Some(previous) = self.last_game_update_seq {
            if fact.game_update_seq == previous {
                return Err(self.fail(ConsumerErrorV1::SequenceDuplicate {
                    value: fact.game_update_seq,
                }));
            }
            if fact.game_update_seq < previous {
                return Err(self.fail(ConsumerErrorV1::SequenceRegressed {
                    previous,
                    current: fact.game_update_seq,
                }));
            }
            if previous.checked_add(1) != Some(fact.game_update_seq) {
                return Err(self.fail(ConsumerErrorV1::SequenceGap {
                    previous,
                    current: fact.game_update_seq,
                }));
            }
        }
        if self
            .last_sample_qpc
            .is_some_and(|previous| fact.sample_qpc <= previous)
        {
            return Err(self.fail(ConsumerErrorV1::QpcNotIncreasing));
        }
        self.last_game_update_seq = Some(fact.game_update_seq);
        self.last_sample_qpc = Some(fact.sample_qpc);
        Ok(fact)
    }

    fn fail(&mut self, error: ConsumerErrorV1) -> ConsumerErrorV1 {
        self.poisoned = true;
        error
    }
}

fn require_build(
    expected: &BuildIdentityV1,
    actual: &BuildIdentityV1,
) -> Result<(), ConsumerErrorV1> {
    if expected.exe_sha256 != actual.exe_sha256 {
        Err(ConsumerErrorV1::BuildMismatch("exe_sha256"))
    } else if expected.fallout_runtime_version != actual.fallout_runtime_version {
        Err(ConsumerErrorV1::BuildMismatch("fallout_runtime_version"))
    } else if expected.f4se_version != actual.f4se_version {
        Err(ConsumerErrorV1::BuildMismatch("f4se_version"))
    } else if expected.plugin_build_sha != actual.plugin_build_sha {
        Err(ConsumerErrorV1::BuildMismatch("plugin_build_sha"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ConsumerErrorV1 {
    #[error("telemetry stream is poisoned")]
    StreamPoisoned,
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error(transparent)]
    Shape(ContractShapeErrorV1),
    #[error("old or unexpected run: expected {expected}, got {actual}")]
    OldRun { expected: String, actual: String },
    #[error("build identity mismatch: {0}")]
    BuildMismatch(&'static str),
    #[error("coordinate transform mismatch")]
    CoordinateTransformMismatch,
    #[error("QPC frequency mismatch")]
    QpcFrequencyMismatch,
    #[error("invalid telemetry state: {reason:?}")]
    InvalidState { reason: Option<InvalidReasonV1> },
    #[error("sample QPC is in the future")]
    QpcInFuture,
    #[error("sample QPC is stale")]
    StaleQpc,
    #[error("duplicate game_update_seq {value}")]
    SequenceDuplicate { value: u64 },
    #[error("game_update_seq regressed from {previous} to {current}")]
    SequenceRegressed { previous: u64, current: u64 },
    #[error("game_update_seq gap from {previous} to {current}")]
    SequenceGap { previous: u64, current: u64 },
    #[error("sample QPC did not increase")]
    QpcNotIncreasing,
}

// Keep these types referenced in rustdoc/API search near the consumer gate.
const _: Option<CoordinateTransformV1> = None;
