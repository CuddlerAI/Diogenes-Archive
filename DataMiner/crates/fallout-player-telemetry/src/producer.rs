use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::contract::{
    BuildIdentityV1, ContractShapeErrorV1, CoordinateTransformV1, GameplayStateV1, InvalidReasonV1,
    PlayerTelemetryV1, SCHEMA_VERSION_V1, SpeedProvenanceV1, TelemetryFactV1, ValidityStateV1,
    ValidityV1,
};
use telemetry_geometry::{transform_point, transform_rotation, transform_speed_magnitude};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawAvailabilityV1 {
    GameplayPlayerValid,
    Loading,
    MenuOrPipboy,
    TransitionOrFastTravel,
    NoPlayer,
    UnknownProbeTimingPossible,
    RotationCapabilityInsufficient,
}

impl RawAvailabilityV1 {
    fn gameplay_state(self) -> GameplayStateV1 {
        match self {
            Self::GameplayPlayerValid | Self::RotationCapabilityInsufficient => {
                GameplayStateV1::GameplayPlayerValid
            }
            Self::Loading => GameplayStateV1::Loading,
            Self::MenuOrPipboy => GameplayStateV1::MenuOrPipboy,
            Self::TransitionOrFastTravel => GameplayStateV1::TransitionOrFastTravel,
            Self::NoPlayer => GameplayStateV1::NoPlayer,
            Self::UnknownProbeTimingPossible => GameplayStateV1::UnknownProbeTimingPossible,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawPlayerRootV1 {
    /// Authoritative actor-root world translation in Fallout game units.
    pub translation_game_units: [f64; 3],
    /// Authoritative actor-root full world rotation, row-major.
    pub rotation_matrix: [[f64; 3]; 3],
    /// Direct game-logic velocity in game units per second, when the runtime exposes it.
    pub direct_velocity_game_units_per_second: Option<[f64; 3]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawPlayerSnapshotV1 {
    pub run_id: String,
    pub game_update_seq: u64,
    pub sample_qpc: u64,
    pub qpc_frequency_hz: u64,
    pub build: BuildIdentityV1,
    pub availability: RawAvailabilityV1,
    pub player_root: Option<RawPlayerRootV1>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpectedRuntimeV1 {
    pub run_id: String,
    pub build: BuildIdentityV1,
    pub coordinate_transform: CoordinateTransformV1,
    pub qpc_frequency_hz: u64,
    pub max_sample_age_qpc: u64,
}

impl ExpectedRuntimeV1 {
    pub fn validate(&self) -> Result<(), AdapterConfigErrorV1> {
        if self.run_id.trim().is_empty() {
            return Err(AdapterConfigErrorV1::InvalidRunId);
        }
        self.build.validate_shape()?;
        self.coordinate_transform.validate_shape()?;
        if self.qpc_frequency_hz == 0 || self.max_sample_age_qpc == 0 {
            return Err(AdapterConfigErrorV1::InvalidQpcConfiguration);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AdapterConfigErrorV1 {
    #[error("invalid expected run id")]
    InvalidRunId,
    #[error("invalid QPC configuration")]
    InvalidQpcConfiguration,
    #[error(transparent)]
    Contract(#[from] ContractShapeErrorV1),
}

#[derive(Clone, Debug)]
struct DerivationBaseline {
    game_update_seq: u64,
    sample_qpc: u64,
    position_m: [f64; 3],
}

#[derive(Clone, Debug)]
pub struct TelemetrySnapshotAdapterV1 {
    expected: ExpectedRuntimeV1,
    last_seen_seq: Option<u64>,
    last_seen_qpc: Option<u64>,
    derivation_baseline: Option<DerivationBaseline>,
}

impl TelemetrySnapshotAdapterV1 {
    pub fn new(expected: ExpectedRuntimeV1) -> Result<Self, AdapterConfigErrorV1> {
        expected.validate()?;
        Ok(Self {
            expected,
            last_seen_seq: None,
            last_seen_qpc: None,
            derivation_baseline: None,
        })
    }

    #[must_use]
    pub fn expected(&self) -> &ExpectedRuntimeV1 {
        &self.expected
    }

    /// Converts one game-update snapshot without performing I/O.
    ///
    /// Invalid inputs still produce a versioned diagnostic fact, but never a
    /// player payload. Any invalid interval clears the displacement baseline.
    pub fn snapshot(&mut self, raw: RawPlayerSnapshotV1, now_qpc: u64) -> TelemetryFactV1 {
        if raw.run_id != self.expected.run_id {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::OldRun);
        }
        if let Some(reason) = identity_mismatch(&self.expected.build, &raw.build) {
            self.derivation_baseline = None;
            return self.invalid(&raw, reason);
        }
        if raw.qpc_frequency_hz != self.expected.qpc_frequency_hz {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::QpcFrequencyMismatch);
        }
        if raw.sample_qpc > now_qpc {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::QpcInFuture);
        }
        if now_qpc - raw.sample_qpc > self.expected.max_sample_age_qpc {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::StaleQpc);
        }

        if let Some(previous) = self.last_seen_seq {
            if raw.game_update_seq == previous {
                self.derivation_baseline = None;
                return self.invalid(&raw, InvalidReasonV1::SequenceDuplicate);
            }
            if raw.game_update_seq < previous {
                self.derivation_baseline = None;
                return self.invalid(&raw, InvalidReasonV1::SequenceRegressed);
            }
        }
        if self
            .last_seen_qpc
            .is_some_and(|previous| raw.sample_qpc <= previous)
        {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::QpcNotIncreasing);
        }
        if self
            .last_seen_seq
            .is_some_and(|previous| previous.checked_add(1) != Some(raw.game_update_seq))
        {
            self.last_seen_seq = Some(raw.game_update_seq);
            self.last_seen_qpc = Some(raw.sample_qpc);
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::SequenceGap);
        }
        self.last_seen_seq = Some(raw.game_update_seq);
        self.last_seen_qpc = Some(raw.sample_qpc);

        let availability_reason = match raw.availability {
            RawAvailabilityV1::GameplayPlayerValid => None,
            RawAvailabilityV1::Loading => Some(InvalidReasonV1::Loading),
            RawAvailabilityV1::MenuOrPipboy => Some(InvalidReasonV1::MenuOrPipboy),
            RawAvailabilityV1::TransitionOrFastTravel => {
                Some(InvalidReasonV1::TransitionOrFastTravel)
            }
            RawAvailabilityV1::NoPlayer => Some(InvalidReasonV1::NoPlayer),
            RawAvailabilityV1::UnknownProbeTimingPossible => {
                Some(InvalidReasonV1::UnknownProbeTimingPossible)
            }
            RawAvailabilityV1::RotationCapabilityInsufficient => {
                Some(InvalidReasonV1::RotationCapabilityInsufficient)
            }
        };
        if let Some(reason) = availability_reason {
            self.derivation_baseline = None;
            return self.invalid(&raw, reason);
        }
        let Some(root) = raw.player_root.as_ref() else {
            self.derivation_baseline = None;
            return self.invalid_with_state(
                &raw,
                GameplayStateV1::NoPlayer,
                InvalidReasonV1::NoPlayer,
            );
        };

        let transform = &self.expected.coordinate_transform;
        let Some(position_m) = transform_point(
            transform.target_from_source_axes,
            transform.metres_per_game_unit,
            root.translation_game_units,
        ) else {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::NonFiniteInput);
        };
        let Some(rotation_quaternion_xyzw) =
            transform_rotation(transform.target_from_source_axes, root.rotation_matrix)
        else {
            self.derivation_baseline = None;
            return self.invalid(&raw, InvalidReasonV1::InvalidRotation);
        };

        let baseline = DerivationBaseline {
            game_update_seq: raw.game_update_seq,
            sample_qpc: raw.sample_qpc,
            position_m,
        };
        let speed = match root.direct_velocity_game_units_per_second {
            Some(velocity) => match transform_speed_magnitude(
                transform.target_from_source_axes,
                transform.metres_per_game_unit,
                velocity,
            ) {
                Some(speed) => (speed, SpeedProvenanceV1::DirectGameLogicVelocity),
                None => {
                    self.derivation_baseline = None;
                    return self.invalid(&raw, InvalidReasonV1::NonFiniteInput);
                }
            },
            None => {
                let derived = self.derive_speed(&baseline);
                self.derivation_baseline = Some(baseline.clone());
                let Some(speed) = derived else {
                    return self.invalid(&raw, InvalidReasonV1::SpeedUnavailable);
                };
                speed
            }
        };
        self.derivation_baseline = Some(baseline);
        let (speed_mps, speed_provenance) = speed;

        TelemetryFactV1 {
            schema_version: SCHEMA_VERSION_V1.to_owned(),
            run_id: raw.run_id,
            game_update_seq: raw.game_update_seq,
            sample_qpc: raw.sample_qpc,
            qpc_frequency_hz: raw.qpc_frequency_hz,
            gameplay_state: GameplayStateV1::GameplayPlayerValid,
            validity: ValidityV1::valid(),
            build: raw.build,
            coordinate_transform: self.expected.coordinate_transform.clone(),
            player: Some(PlayerTelemetryV1 {
                position_m,
                rotation_quaternion_xyzw,
                speed_mps,
                speed_provenance,
            }),
        }
    }

    fn derive_speed(&self, current: &DerivationBaseline) -> Option<(f64, SpeedProvenanceV1)> {
        let previous = self.derivation_baseline.as_ref()?;
        if previous.game_update_seq.checked_add(1) != Some(current.game_update_seq)
            || current.sample_qpc <= previous.sample_qpc
        {
            return None;
        }
        let delta_qpc = current.sample_qpc - previous.sample_qpc;
        let delta_seconds = delta_qpc as f64 / self.expected.qpc_frequency_hz as f64;
        if !delta_seconds.is_finite() || delta_seconds <= 0.0 {
            return None;
        }
        let distance_m = current
            .position_m
            .iter()
            .zip(previous.position_m)
            .map(|(current, previous)| (current - previous).powi(2))
            .sum::<f64>()
            .sqrt();
        let speed = distance_m / delta_seconds;
        speed
            .is_finite()
            .then_some((speed, SpeedProvenanceV1::ActorRootDeltaGameTickQpc))
    }

    fn invalid(&self, raw: &RawPlayerSnapshotV1, reason: InvalidReasonV1) -> TelemetryFactV1 {
        self.invalid_with_state(raw, raw.availability.gameplay_state(), reason)
    }

    fn invalid_with_state(
        &self,
        raw: &RawPlayerSnapshotV1,
        gameplay_state: GameplayStateV1,
        reason: InvalidReasonV1,
    ) -> TelemetryFactV1 {
        TelemetryFactV1 {
            schema_version: SCHEMA_VERSION_V1.to_owned(),
            run_id: raw.run_id.clone(),
            game_update_seq: raw.game_update_seq,
            sample_qpc: raw.sample_qpc,
            qpc_frequency_hz: raw.qpc_frequency_hz,
            gameplay_state,
            validity: ValidityV1::invalid(reason),
            build: raw.build.clone(),
            coordinate_transform: self.expected.coordinate_transform.clone(),
            player: None,
        }
    }
}

fn identity_mismatch(
    expected: &BuildIdentityV1,
    actual: &BuildIdentityV1,
) -> Option<InvalidReasonV1> {
    if actual.exe_sha256 != expected.exe_sha256 {
        Some(InvalidReasonV1::ExeSha256Mismatch)
    } else if actual.fallout_runtime_version != expected.fallout_runtime_version {
        Some(InvalidReasonV1::FalloutRuntimeVersionMismatch)
    } else if actual.f4se_version != expected.f4se_version {
        Some(InvalidReasonV1::F4seVersionMismatch)
    } else if actual.plugin_build_sha != expected.plugin_build_sha {
        Some(InvalidReasonV1::PluginBuildShaMismatch)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryPublishStatusV1 {
    Accepted,
    WouldBlock,
    Closed,
}

/// Replaceable transport interface. Implementations must return immediately.
pub trait NonBlockingSinkV1 {
    fn try_publish(&mut self, payload: &[u8]) -> TryPublishStatusV1;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProducerPublishOutcomeV1 {
    Accepted {
        validity: ValidityStateV1,
        bytes: usize,
    },
    DroppedWouldBlock {
        validity: ValidityStateV1,
        bytes: usize,
    },
    SinkClosed {
        validity: ValidityStateV1,
        bytes: usize,
    },
}

pub struct TelemetryProducerV1<S> {
    adapter: TelemetrySnapshotAdapterV1,
    sink: S,
}

impl<S: NonBlockingSinkV1> TelemetryProducerV1<S> {
    #[must_use]
    pub fn new(adapter: TelemetrySnapshotAdapterV1, sink: S) -> Self {
        Self { adapter, sink }
    }

    pub fn on_game_update(
        &mut self,
        raw: RawPlayerSnapshotV1,
        now_qpc: u64,
    ) -> Result<ProducerPublishOutcomeV1, serde_json::Error> {
        let fact = self.adapter.snapshot(raw, now_qpc);
        let validity = fact.validity.state;
        let payload = encode_json_line_v1(&fact)?;
        let bytes = payload.len();
        Ok(match self.sink.try_publish(&payload) {
            TryPublishStatusV1::Accepted => ProducerPublishOutcomeV1::Accepted { validity, bytes },
            TryPublishStatusV1::WouldBlock => {
                ProducerPublishOutcomeV1::DroppedWouldBlock { validity, bytes }
            }
            TryPublishStatusV1::Closed => ProducerPublishOutcomeV1::SinkClosed { validity, bytes },
        })
    }

    #[must_use]
    pub fn into_parts(self) -> (TelemetrySnapshotAdapterV1, S) {
        (self.adapter, self.sink)
    }
}

pub fn encode_json_line_v1(fact: &TelemetryFactV1) -> Result<Vec<u8>, serde_json::Error> {
    let mut encoded = serde_json::to_vec(fact)?;
    encoded.push(b'\n');
    Ok(encoded)
}
