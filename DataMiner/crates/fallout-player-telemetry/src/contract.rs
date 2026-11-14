use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::FALLOUT4_METRES_PER_GAME_UNIT;
use telemetry_geometry::{determinant3, matrix_is_orthonormal};

pub const SCHEMA_VERSION_V1: &str = "fallout-player-telemetry/v1";
pub const FALLOUT_SOURCE_SPACE_V1: &str = "fallout4-world-x-right-y-forward-z-up-rh/v1";
pub const DM_WORLD_SPACE_V1: &str = "dm-world-x-right-y-up-z-forward-lh-m/v1";
pub const FALLOUT_TO_DM_TRANSFORM_V1: &str = "fallout4-root-to-dm-world/v1";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentityV1 {
    pub exe_sha256: String,
    pub fallout_runtime_version: String,
    pub f4se_version: String,
    pub plugin_build_sha: String,
}

impl BuildIdentityV1 {
    pub fn validate_shape(&self) -> Result<(), ContractShapeErrorV1> {
        if !is_prefixed_lower_hex(&self.exe_sha256, "sha256:", 64) {
            return Err(ContractShapeErrorV1::InvalidField("build.exe_sha256"));
        }
        if self.fallout_runtime_version.trim().is_empty() {
            return Err(ContractShapeErrorV1::InvalidField(
                "build.fallout_runtime_version",
            ));
        }
        if self.f4se_version.trim().is_empty() {
            return Err(ContractShapeErrorV1::InvalidField("build.f4se_version"));
        }
        if !is_prefixed_lower_hex(&self.plugin_build_sha, "git:", 40) {
            return Err(ContractShapeErrorV1::InvalidField("build.plugin_build_sha"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinateTransformV1 {
    pub transform_id: String,
    pub source_space_id: String,
    pub target_space_id: String,
    pub metres_per_game_unit: f64,
    /// Row-major matrix applied as `target = axes * source * scale`.
    pub target_from_source_axes: [[f64; 3]; 3],
}

impl CoordinateTransformV1 {
    #[must_use]
    pub fn fallout4_to_dm_world() -> Self {
        Self {
            transform_id: FALLOUT_TO_DM_TRANSFORM_V1.to_owned(),
            source_space_id: FALLOUT_SOURCE_SPACE_V1.to_owned(),
            target_space_id: DM_WORLD_SPACE_V1.to_owned(),
            metres_per_game_unit: FALLOUT4_METRES_PER_GAME_UNIT,
            // Fallout X/Y/Z = right/forward/up (RH). DataMiner X/Y/Z =
            // right/up/forward (LH), so swap source Y and Z.
            target_from_source_axes: [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]],
        }
    }

    pub fn validate_shape(&self) -> Result<(), ContractShapeErrorV1> {
        if self.transform_id.trim().is_empty()
            || self.source_space_id.trim().is_empty()
            || self.target_space_id.trim().is_empty()
        {
            return Err(ContractShapeErrorV1::InvalidField(
                "coordinate_transform identity",
            ));
        }
        if !self.metres_per_game_unit.is_finite() || self.metres_per_game_unit <= 0.0 {
            return Err(ContractShapeErrorV1::InvalidField(
                "coordinate_transform.metres_per_game_unit",
            ));
        }
        if !matrix_is_orthonormal(self.target_from_source_axes, 1e-9)
            || (determinant3(self.target_from_source_axes).abs() - 1.0).abs() > 1e-9
        {
            return Err(ContractShapeErrorV1::InvalidField(
                "coordinate_transform.target_from_source_axes",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidityStateV1 {
    Valid,
    Invalid,
}

/// In-process game-logic state observed at this exact game-update/QPC fact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GameplayStateV1 {
    GameplayPlayerValid,
    Loading,
    MenuOrPipboy,
    TransitionOrFastTravel,
    NoPlayer,
    /// Unclassified evidence that may instead be probe/timing loss. Never infer a
    /// stronger game state from an isolated empty-depth sample.
    UnknownProbeTimingPossible,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidReasonV1 {
    OldRun,
    ExeSha256Mismatch,
    FalloutRuntimeVersionMismatch,
    F4seVersionMismatch,
    PluginBuildShaMismatch,
    QpcFrequencyMismatch,
    QpcInFuture,
    StaleQpc,
    Loading,
    MenuOrPipboy,
    TransitionOrFastTravel,
    NoPlayer,
    UnknownProbeTimingPossible,
    RotationCapabilityInsufficient,
    SequenceDuplicate,
    SequenceRegressed,
    SequenceGap,
    QpcNotIncreasing,
    SpeedUnavailable,
    NonFiniteInput,
    InvalidRotation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidityV1 {
    pub state: ValidityStateV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<InvalidReasonV1>,
}

impl ValidityV1 {
    #[must_use]
    pub fn valid() -> Self {
        Self {
            state: ValidityStateV1::Valid,
            reason: None,
        }
    }

    #[must_use]
    pub fn invalid(reason: InvalidReasonV1) -> Self {
        Self {
            state: ValidityStateV1::Invalid,
            reason: Some(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedProvenanceV1 {
    DirectGameLogicVelocity,
    ActorRootDeltaGameTickQpc,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerTelemetryV1 {
    /// Actor-root authoritative world translation after explicit axis/unit conversion.
    pub position_m: [f64; 3],
    /// Actor-root full world rotation in `[x, y, z, w]` order.
    pub rotation_quaternion_xyzw: [f64; 4],
    /// Scalar speed magnitude in metres per second.
    pub speed_mps: f64,
    pub speed_provenance: SpeedProvenanceV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryFactV1 {
    pub schema_version: String,
    pub run_id: String,
    pub game_update_seq: u64,
    pub sample_qpc: u64,
    pub qpc_frequency_hz: u64,
    pub gameplay_state: GameplayStateV1,
    pub validity: ValidityV1,
    pub build: BuildIdentityV1,
    pub coordinate_transform: CoordinateTransformV1,
    pub player: Option<PlayerTelemetryV1>,
}

impl TelemetryFactV1 {
    pub fn validate_shape(&self) -> Result<(), ContractShapeErrorV1> {
        if self.schema_version != SCHEMA_VERSION_V1 {
            return Err(ContractShapeErrorV1::SchemaVersion {
                actual: self.schema_version.clone(),
            });
        }
        if self.run_id.trim().is_empty() {
            return Err(ContractShapeErrorV1::InvalidField("run_id"));
        }
        if self.sample_qpc == 0 || self.qpc_frequency_hz == 0 {
            return Err(ContractShapeErrorV1::InvalidField("QPC"));
        }
        self.build.validate_shape()?;
        self.coordinate_transform.validate_shape()?;

        let payload_result = match (self.validity.state, self.validity.reason, &self.player) {
            (ValidityStateV1::Valid, None, Some(player)) => validate_player(player),
            (ValidityStateV1::Invalid, Some(_), None) => Ok(()),
            _ => Err(ContractShapeErrorV1::ValidityPayloadMismatch),
        };
        payload_result?;
        if self.gameplay_state != GameplayStateV1::GameplayPlayerValid
            && self.validity.state != ValidityStateV1::Invalid
        {
            return Err(ContractShapeErrorV1::NonGameplayPlayerPayload);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ContractShapeErrorV1 {
    #[error("schema version mismatch: {actual}")]
    SchemaVersion { actual: String },
    #[error("invalid contract field: {0}")]
    InvalidField(&'static str),
    #[error("validity and player payload disagree")]
    ValidityPayloadMismatch,
    #[error("non-gameplay state cannot contain a valid player payload")]
    NonGameplayPlayerPayload,
}

fn validate_player(player: &PlayerTelemetryV1) -> Result<(), ContractShapeErrorV1> {
    if !player.position_m.iter().all(|value| value.is_finite()) {
        return Err(ContractShapeErrorV1::InvalidField("player.position_m"));
    }
    if !player
        .rotation_quaternion_xyzw
        .iter()
        .all(|value| value.is_finite())
    {
        return Err(ContractShapeErrorV1::InvalidField(
            "player.rotation_quaternion_xyzw",
        ));
    }
    let norm_squared = player
        .rotation_quaternion_xyzw
        .iter()
        .map(|value| value * value)
        .sum::<f64>();
    if (norm_squared - 1.0).abs() > 0.002 {
        return Err(ContractShapeErrorV1::InvalidField(
            "player.rotation_quaternion_xyzw",
        ));
    }
    if !player.speed_mps.is_finite() || player.speed_mps < 0.0 {
        return Err(ContractShapeErrorV1::InvalidField("player.speed_mps"));
    }
    Ok(())
}

fn is_prefixed_lower_hex(value: &str, prefix: &str, digits: usize) -> bool {
    value.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == digits
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}
