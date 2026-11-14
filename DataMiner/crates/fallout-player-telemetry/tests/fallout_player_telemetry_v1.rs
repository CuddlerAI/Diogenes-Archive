use std::cell::Cell;
use std::ffi::c_void;
use std::fs;
use std::mem::size_of;
use std::path::Path;

use fallout_player_telemetry::{
    BuildIdentityV1, ConsumerErrorV1, CoordinateTransformV1, ExpectedRuntimeV1, F4SE_VERSION,
    FALL_OUT_PLAYER_TELEMETRY_SINK_ABI_V1, FALLOUT4_RUNTIME_VERSION,
    FALLOUT4_STEAM_BUILD_22848228_EXE_SHA256, GameplayStateV1, InvalidReasonV1,
    NonBlockingSinkAbiV1, NonBlockingSinkV1, ProducerPublishOutcomeV1, RawAvailabilityV1,
    RawPlayerRootV1, RawPlayerSnapshotV1, SCHEMA_VERSION_V1, SpeedProvenanceV1,
    TelemetryConsumerV1, TelemetryFactV1, TelemetryProducerV1, TelemetrySnapshotAdapterV1,
    TryPublishCodeV1, TryPublishStatusV1, ValidityStateV1, encode_json_line_v1,
};

const RUN_ID: &str = "run-f4-a1-fixture-0001";
const PLUGIN_SHA: &str = "git:0123456789abcdef0123456789abcdef01234567";
const QPC_FREQUENCY: u64 = 10_000_000;
const MAX_AGE_QPC: u64 = 500_000;
type DriftCase = (fn(&mut RawPlayerSnapshotV1), InvalidReasonV1);
type StateCase = (RawAvailabilityV1, GameplayStateV1, InvalidReasonV1);

fn build() -> BuildIdentityV1 {
    BuildIdentityV1 {
        exe_sha256: FALLOUT4_STEAM_BUILD_22848228_EXE_SHA256.to_owned(),
        fallout_runtime_version: FALLOUT4_RUNTIME_VERSION.to_owned(),
        f4se_version: F4SE_VERSION.to_owned(),
        plugin_build_sha: PLUGIN_SHA.to_owned(),
    }
}

fn expected() -> ExpectedRuntimeV1 {
    ExpectedRuntimeV1 {
        run_id: RUN_ID.to_owned(),
        build: build(),
        coordinate_transform: CoordinateTransformV1::fallout4_to_dm_world(),
        qpc_frequency_hz: QPC_FREQUENCY,
        max_sample_age_qpc: MAX_AGE_QPC,
    }
}

fn identity_rotation() -> [[f64; 3]; 3] {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

fn raw(seq: u64, qpc: u64, direct_velocity: Option<[f64; 3]>) -> RawPlayerSnapshotV1 {
    RawPlayerSnapshotV1 {
        run_id: RUN_ID.to_owned(),
        game_update_seq: seq,
        sample_qpc: qpc,
        qpc_frequency_hz: QPC_FREQUENCY,
        build: build(),
        availability: RawAvailabilityV1::GameplayPlayerValid,
        player_root: Some(RawPlayerRootV1 {
            translation_game_units: [100.0, 200.0, 300.0],
            rotation_matrix: identity_rotation(),
            direct_velocity_game_units_per_second: direct_velocity,
        }),
    }
}

fn adapter() -> TelemetrySnapshotAdapterV1 {
    TelemetrySnapshotAdapterV1::new(expected()).unwrap()
}

fn invalid_reason(fact: &TelemetryFactV1) -> InvalidReasonV1 {
    assert_eq!(fact.validity.state, ValidityStateV1::Invalid);
    assert!(fact.player.is_none());
    fact.validity.reason.unwrap()
}

fn valid_fixture() -> TelemetryFactV1 {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fallout-player-telemetry-v1.valid.jsonl");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn fixture_is_a_valid_exact_build_consumer_fact() {
    let fixture = valid_fixture();
    fixture.validate_shape().unwrap();
    assert_eq!(fixture.schema_version, SCHEMA_VERSION_V1);
    assert_eq!(fixture.gameplay_state, GameplayStateV1::GameplayPlayerValid);

    let encoded = encode_json_line_v1(&fixture).unwrap();
    assert_eq!(encoded.last(), Some(&b'\n'));
    let mut consumer = TelemetryConsumerV1::new(expected());
    assert_eq!(
        consumer.accept_json_line(&encoded, 10_200_000).unwrap(),
        fixture
    );
}

#[test]
fn exact_identity_and_direct_velocity_produce_a_valid_fact() {
    let fact = adapter().snapshot(raw(42, 10_100_000, Some([3.0, 4.0, 0.0])), 10_200_000);
    fact.validate_shape().unwrap();
    let player = fact.player.unwrap();
    assert_eq!(fact.gameplay_state, GameplayStateV1::GameplayPlayerValid);
    assert_eq!(player.position_m, [1.42875, 4.28625, 2.8575]);
    assert_eq!(player.rotation_quaternion_xyzw, [0.0, 0.0, 0.0, 1.0]);
    assert!((player.speed_mps - 0.0714375).abs() < 1e-12);
    assert_eq!(
        player.speed_provenance,
        SpeedProvenanceV1::DirectGameLogicVelocity
    );
}

#[test]
fn exe_f4se_and_plugin_drift_each_fail_closed() {
    let cases: [DriftCase; 3] = [
        (
            |raw| raw.build.exe_sha256.replace_range(7..8, "f"),
            InvalidReasonV1::ExeSha256Mismatch,
        ),
        (
            |raw| raw.build.f4se_version = "0.7.9".to_owned(),
            InvalidReasonV1::F4seVersionMismatch,
        ),
        (
            |raw| raw.build.plugin_build_sha.replace_range(4..5, "f"),
            InvalidReasonV1::PluginBuildShaMismatch,
        ),
    ];
    for (mutate, expected_reason) in cases {
        let mut raw = raw(42, 10_100_000, Some([0.0; 3]));
        mutate(&mut raw);
        assert_eq!(
            invalid_reason(&adapter().snapshot(raw, 10_200_000)),
            expected_reason
        );
    }
}

#[test]
fn fallout_runtime_drift_fails_closed() {
    let mut raw = raw(42, 10_100_000, Some([0.0; 3]));
    raw.build.fallout_runtime_version = "1.11.222.0".to_owned();
    assert_eq!(
        invalid_reason(&adapter().snapshot(raw, 10_200_000)),
        InvalidReasonV1::FalloutRuntimeVersionMismatch
    );
}

#[test]
fn explicit_game_states_and_yaw_only_capability_are_invalid_without_player_payload() {
    let cases: [StateCase; 6] = [
        (
            RawAvailabilityV1::Loading,
            GameplayStateV1::Loading,
            InvalidReasonV1::Loading,
        ),
        (
            RawAvailabilityV1::MenuOrPipboy,
            GameplayStateV1::MenuOrPipboy,
            InvalidReasonV1::MenuOrPipboy,
        ),
        (
            RawAvailabilityV1::TransitionOrFastTravel,
            GameplayStateV1::TransitionOrFastTravel,
            InvalidReasonV1::TransitionOrFastTravel,
        ),
        (
            RawAvailabilityV1::NoPlayer,
            GameplayStateV1::NoPlayer,
            InvalidReasonV1::NoPlayer,
        ),
        (
            RawAvailabilityV1::UnknownProbeTimingPossible,
            GameplayStateV1::UnknownProbeTimingPossible,
            InvalidReasonV1::UnknownProbeTimingPossible,
        ),
        (
            RawAvailabilityV1::RotationCapabilityInsufficient,
            GameplayStateV1::GameplayPlayerValid,
            InvalidReasonV1::RotationCapabilityInsufficient,
        ),
    ];
    for (availability, expected_state, expected_reason) in cases {
        let mut raw = raw(42, 10_100_000, Some([0.0; 3]));
        raw.availability = availability;
        let fact = adapter().snapshot(raw, 10_200_000);
        assert_eq!(fact.game_update_seq, 42);
        assert_eq!(fact.sample_qpc, 10_100_000);
        assert_eq!(fact.gameplay_state, expected_state);
        assert_eq!(invalid_reason(&fact), expected_reason);
    }

    let mut no_player = raw(42, 10_100_000, Some([0.0; 3]));
    no_player.player_root = None;
    let fact = adapter().snapshot(no_player, 10_200_000);
    assert_eq!(fact.gameplay_state, GameplayStateV1::NoPlayer);
    assert_eq!(invalid_reason(&fact), InvalidReasonV1::NoPlayer);
}

#[test]
fn non_gameplay_state_never_carries_player_and_clears_speed_baseline() {
    let mut adapter = adapter();
    let gameplay = adapter.snapshot(raw(42, 10_100_000, Some([0.0; 3])), 10_200_000);
    assert!(gameplay.player.is_some());

    let mut menu = raw(43, 10_200_000, Some([0.0; 3]));
    menu.availability = RawAvailabilityV1::MenuOrPipboy;
    let menu = adapter.snapshot(menu, 10_300_000);
    assert_eq!(menu.gameplay_state, GameplayStateV1::MenuOrPipboy);
    assert_eq!(invalid_reason(&menu), InvalidReasonV1::MenuOrPipboy);

    let resumed = adapter.snapshot(raw(44, 10_300_000, None), 10_400_000);
    assert_eq!(resumed.gameplay_state, GameplayStateV1::GameplayPlayerValid);
    assert_eq!(
        invalid_reason(&resumed),
        InvalidReasonV1::SpeedUnavailable,
        "gameplay resume must establish a fresh baseline instead of carrying menu-era player data"
    );
}

#[test]
fn duplicate_and_regressed_game_ticks_fail_closed() {
    let mut sequence_adapter = adapter();
    assert!(
        sequence_adapter
            .snapshot(raw(42, 10_100_000, Some([0.0; 3])), 10_200_000)
            .player
            .is_some()
    );
    assert_eq!(
        invalid_reason(&sequence_adapter.snapshot(raw(42, 10_200_000, Some([0.0; 3])), 10_300_000)),
        InvalidReasonV1::SequenceDuplicate
    );
    assert_eq!(
        invalid_reason(&sequence_adapter.snapshot(raw(41, 10_300_000, Some([0.0; 3])), 10_400_000)),
        InvalidReasonV1::SequenceRegressed
    );
}

#[test]
fn stale_qpc_fails_closed() {
    assert_eq!(
        invalid_reason(&adapter().snapshot(raw(42, 10_100_000, Some([0.0; 3])), 10_700_001)),
        InvalidReasonV1::StaleQpc
    );
}

#[test]
fn speed_falls_back_only_across_contiguous_valid_game_ticks() {
    let mut adapter = adapter();
    let first = adapter.snapshot(raw(42, 10_000_000, None), 10_100_000);
    assert_eq!(invalid_reason(&first), InvalidReasonV1::SpeedUnavailable);

    let mut second = raw(43, 11_000_000, None);
    second.player_root.as_mut().unwrap().translation_game_units[0] = 200.0;
    let second = adapter.snapshot(second, 11_100_000);
    let player = second.player.unwrap();
    assert!((player.speed_mps - 14.2875).abs() < 1e-12);
    assert_eq!(
        player.speed_provenance,
        SpeedProvenanceV1::ActorRootDeltaGameTickQpc
    );

    let skipped = adapter.snapshot(raw(45, 12_000_000, None), 12_100_000);
    assert_eq!(invalid_reason(&skipped), InvalidReasonV1::SequenceGap);
    let after_gap = adapter.snapshot(raw(46, 13_000_000, None), 13_100_000);
    assert_eq!(
        invalid_reason(&after_gap),
        InvalidReasonV1::SpeedUnavailable
    );
}

#[test]
fn non_finite_position_velocity_and_rotation_are_rejected() {
    let mut position = raw(42, 10_100_000, Some([0.0; 3]));
    position
        .player_root
        .as_mut()
        .unwrap()
        .translation_game_units[0] = f64::NAN;
    assert_eq!(
        invalid_reason(&adapter().snapshot(position, 10_200_000)),
        InvalidReasonV1::NonFiniteInput
    );

    let mut velocity = raw(42, 10_100_000, Some([0.0; 3]));
    velocity
        .player_root
        .as_mut()
        .unwrap()
        .direct_velocity_game_units_per_second = Some([f64::INFINITY, 0.0, 0.0]);
    assert_eq!(
        invalid_reason(&adapter().snapshot(velocity, 10_200_000)),
        InvalidReasonV1::NonFiniteInput
    );

    let mut sequence_adapter = adapter();
    let mut invalid_velocity = raw(42, 10_100_000, Some([0.0; 3]));
    invalid_velocity
        .player_root
        .as_mut()
        .unwrap()
        .direct_velocity_game_units_per_second = Some([f64::NAN, 0.0, 0.0]);
    assert_eq!(
        invalid_reason(&sequence_adapter.snapshot(invalid_velocity, 10_200_000)),
        InvalidReasonV1::NonFiniteInput
    );
    assert_eq!(
        invalid_reason(&sequence_adapter.snapshot(raw(43, 10_200_000, None), 10_300_000)),
        InvalidReasonV1::SpeedUnavailable,
        "an invalid direct-velocity tick must not become a displacement baseline"
    );

    let mut rotation = raw(42, 10_100_000, Some([0.0; 3]));
    rotation.player_root.as_mut().unwrap().rotation_matrix[0][0] = 2.0;
    assert_eq!(
        invalid_reason(&adapter().snapshot(rotation, 10_200_000)),
        InvalidReasonV1::InvalidRotation
    );
}

#[test]
fn consumer_rejects_old_run_stale_duplicate_and_regressed_facts() {
    let fixture = valid_fixture();

    let mut old = fixture.clone();
    old.run_id = "old-run".to_owned();
    assert!(matches!(
        TelemetryConsumerV1::new(expected()).accept_fact(old, 10_200_000),
        Err(ConsumerErrorV1::OldRun { .. })
    ));

    assert_eq!(
        TelemetryConsumerV1::new(expected()).accept_fact(fixture.clone(), 10_700_001),
        Err(ConsumerErrorV1::StaleQpc)
    );

    let mut duplicate_consumer = TelemetryConsumerV1::new(expected());
    duplicate_consumer
        .accept_fact(fixture.clone(), 10_200_000)
        .unwrap();
    assert!(matches!(
        duplicate_consumer.accept_fact(fixture.clone(), 10_200_000),
        Err(ConsumerErrorV1::SequenceDuplicate { value: 42 })
    ));
    assert_eq!(
        duplicate_consumer.accept_fact(fixture.clone(), 10_200_000),
        Err(ConsumerErrorV1::StreamPoisoned)
    );

    let mut regressed_consumer = TelemetryConsumerV1::new(expected());
    regressed_consumer
        .accept_fact(fixture.clone(), 10_200_000)
        .unwrap();
    let mut regressed = fixture;
    regressed.game_update_seq = 41;
    regressed.sample_qpc += 1;
    assert!(matches!(
        regressed_consumer.accept_fact(regressed, 10_200_000),
        Err(ConsumerErrorV1::SequenceRegressed {
            previous: 42,
            current: 41
        })
    ));
}

struct CountingSink {
    calls: Cell<usize>,
    status: TryPublishStatusV1,
}

impl NonBlockingSinkV1 for CountingSink {
    fn try_publish(&mut self, _payload: &[u8]) -> TryPublishStatusV1 {
        self.calls.set(self.calls.get() + 1);
        self.status
    }
}

#[test]
fn producer_does_not_retry_a_full_non_blocking_sink() {
    let sink = CountingSink {
        calls: Cell::new(0),
        status: TryPublishStatusV1::WouldBlock,
    };
    let mut producer = TelemetryProducerV1::new(adapter(), sink);
    let outcome = producer
        .on_game_update(raw(42, 10_100_000, Some([0.0; 3])), 10_200_000)
        .unwrap();
    assert!(matches!(
        outcome,
        ProducerPublishOutcomeV1::DroppedWouldBlock {
            validity: ValidityStateV1::Valid,
            ..
        }
    ));
    let (_, sink) = producer.into_parts();
    assert_eq!(sink.calls.get(), 1);
}

unsafe extern "C" fn accept_callback(
    _context: *mut c_void,
    _payload: *const u8,
    _payload_len: usize,
) -> TryPublishCodeV1 {
    TryPublishCodeV1::Accepted
}

#[test]
fn c_sink_abi_is_pinned_for_windows_x64() {
    assert_eq!(FALL_OUT_PLAYER_TELEMETRY_SINK_ABI_V1, 1);
    assert_eq!(size_of::<NonBlockingSinkAbiV1>(), 24);
    let sink = NonBlockingSinkAbiV1 {
        abi_version: FALL_OUT_PLAYER_TELEMETRY_SINK_ABI_V1,
        reserved: 0,
        context: std::ptr::null_mut(),
        try_publish: accept_callback,
    };
    assert_eq!(sink.abi_version, 1);
}
