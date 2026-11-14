//! DEV-only Fallout 4 player telemetry contract and fail-closed adapters.
//!
//! This crate deliberately does not contain an F4SE plugin or choose a transport.
//! It owns the versioned message, exact-build gate, coordinate conversion,
//! non-blocking publication boundary, and consumer validation used by a future
//! in-process producer.

mod abi;
mod consumer;
mod contract;
mod producer;

pub use abi::{
    FALL_OUT_PLAYER_TELEMETRY_SINK_ABI_V1, NonBlockingSinkAbiV1, TryPublishCodeV1, TryPublishFnV1,
};
pub use consumer::{ConsumerErrorV1, TelemetryConsumerV1};
pub use contract::{
    BuildIdentityV1, ContractShapeErrorV1, CoordinateTransformV1, GameplayStateV1, InvalidReasonV1,
    PlayerTelemetryV1, SCHEMA_VERSION_V1, SpeedProvenanceV1, TelemetryFactV1, ValidityStateV1,
    ValidityV1,
};
pub use producer::{
    AdapterConfigErrorV1, ExpectedRuntimeV1, NonBlockingSinkV1, ProducerPublishOutcomeV1,
    RawAvailabilityV1, RawPlayerRootV1, RawPlayerSnapshotV1, TelemetryProducerV1,
    TelemetrySnapshotAdapterV1, TryPublishStatusV1, encode_json_line_v1,
};

/// Pinned by `config/catalogs/dev/fallout4-steam-release-test/game-profile.json`.
pub const FALLOUT4_STEAM_BUILD_22848228_EXE_SHA256: &str =
    "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61";
/// File version pinned by the same DEV profile.
pub const FALLOUT4_RUNTIME_VERSION: &str = "1.11.221.0";
/// F4SE build published for Fallout 4 runtime 1.11.221.
pub const F4SE_VERSION: &str = "0.7.8";
/// Creation Kit engine scale recorded in `config/depth-map-bounds/public-survey.json`.
pub const FALLOUT4_METRES_PER_GAME_UNIT: f64 = 0.014_287_5;
