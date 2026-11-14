//! CP2077 2.31 exact-build pose discovery gate.
//!
//! This module deliberately does **not** sample the game and cannot emit a production
//! `CameraBinding`.  It captures the narrow contract that an offline SDK layout and a later
//! exact-build runtime discovery must satisfy before binding authoring is allowed.  In
//! particular, RED4ext SDK type sizes are structural research evidence, not an address binding;
//! the camera and player remain independent sources; and a render view matrix is never treated
//! as player telemetry.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const CP2077_APP_ID: u32 = 1_091_500;
pub const CP2077_BUILD_ID: u64 = 20_383_525;
pub const CP2077_PRODUCT_VERSION: &str = "2.31";
pub const CP2077_EXE_SHA256: &str =
    "sha256:a7de82945c03e041fc7339fcf9066224d98db2f5d80fea50f7947bb350a60991";

pub const RED4EXT_SDK_COMMIT: &str = "ad7277714ad30d6885d7050c5ba24fa0102f6920";
pub const RED4EXT_CAMERA_SYSTEM_BYTES: u32 = 0x730;
pub const RED4EXT_PLAYER_SYSTEM_BYTES: u32 = 0x70;
pub const RED4EXT_WORLD_POSITION_BYTES: u32 = 0x0c;
pub const RED4EXT_WORLD_TRANSFORM_BYTES: u32 = 0x20;
pub const RED4EXT_WORLD_TRANSFORM_ORIENTATION_OFFSET: u32 = 0x10;
pub const RED4EXT_WORLD_POSITION_DIVISOR: i32 = 2 << 16;

pub const CP2077_POSE_DISCOVERY_VERDICT_SCHEMA_V1: &str = "cp2077-pose-discovery-verdict/v1";
pub const MIN_FRESH_PROCESS_LAUNCHES: usize = 3;
pub const MIN_PRESENTS_PER_RUNTIME_RUN: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077ExactBuild {
    pub app_id: u32,
    pub build_id: u64,
    pub product_version: String,
    pub executable_sha256: String,
}

impl Cp2077ExactBuild {
    #[must_use]
    pub fn pinned() -> Self {
        Self {
            app_id: CP2077_APP_ID,
            build_id: CP2077_BUILD_ID,
            product_version: CP2077_PRODUCT_VERSION.into(),
            executable_sha256: CP2077_EXE_SHA256.into(),
        }
    }

    fn is_pinned(&self) -> bool {
        self == &Self::pinned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoseRole {
    Camera,
    Player,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolverKind {
    /// A generated class size or field layout with no exact-build root address.
    SdkLayoutOnly,
    /// A module-relative anchor plus a fully pinned, read-only pointer chain.
    ModuleRelativePointerChain,
    /// A pinned exact-build game-system getter followed by a read-only object chain.
    ExactBuildGameSystemLookup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolverProvenance {
    ExactBuildRuntimeDiscovery,
    Red4extSdkLayout,
    LegacyIgcsSignature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoseResolverCandidate {
    /// Stable, non-address identifier carried into every runtime evidence run.
    pub resolver_id: String,
    pub role: PoseRole,
    pub kind: ResolverKind,
    pub provenance: ResolverProvenance,
    /// RVA of the exact-build anchor or getter. Absolute process pointers must not be persisted.
    pub exact_build_anchor_rva: Option<u64>,
    /// Every pointer/object offset after the anchor, in traversal order.
    pub pointer_offsets: Vec<u32>,
    /// Offset of a `WorldTransform`-compatible value within the resolved object.
    pub world_transform_offset: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionEncoding {
    /// Three signed 17-fraction-bit fixed-point values (SDK `WorldPosition`).
    RedengineWorldPositionFixed17,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrientationEncoding {
    /// SDK `Quaternion`: i, j, k, r as four little-endian `f32` values.
    RedengineQuaternionIjkrF32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignedAxis {
    PositiveX,
    NegativeX,
    PositiveY,
    NegativeY,
    PositiveZ,
    NegativeZ,
}

impl SignedAxis {
    fn source_axis(self) -> u8 {
        match self {
            Self::PositiveX | Self::NegativeX => 0,
            Self::PositiveY | Self::NegativeY => 1,
            Self::PositiveZ | Self::NegativeZ => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinateCandidate {
    pub position_encoding: PositionEncoding,
    pub orientation_encoding: OrientationEncoding,
    /// DataMiner X/Y/Z expressed as a signed permutation of REDengine X/Y/Z.
    pub dataminer_axis_mapping: Option<[SignedAxis; 3]>,
    /// Decimal string, deliberately separated from the SDK fixed-point divisor.
    pub metres_per_world_unit: Option<String>,
    /// Opaque identifier of a real measured calibration/E-geo artifact.
    pub metric_calibration_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077PoseCandidate {
    pub exact_build: Cp2077ExactBuild,
    pub camera: PoseResolverCandidate,
    pub player: PoseResolverCandidate,
    pub coordinates: CoordinateCandidate,
}

impl Cp2077PoseCandidate {
    /// Honest offline result after reading the pinned SDK layouts. It is intentionally blocked.
    #[must_use]
    pub fn sdk_layout_scaffold() -> Self {
        Self {
            exact_build: Cp2077ExactBuild::pinned(),
            camera: PoseResolverCandidate {
                resolver_id: "red4ext-sdk-game-camera-system-layout".into(),
                role: PoseRole::Camera,
                kind: ResolverKind::SdkLayoutOnly,
                provenance: ResolverProvenance::Red4extSdkLayout,
                exact_build_anchor_rva: None,
                pointer_offsets: Vec::new(),
                world_transform_offset: None,
            },
            player: PoseResolverCandidate {
                resolver_id: "red4ext-sdk-game-player-system-layout".into(),
                role: PoseRole::Player,
                kind: ResolverKind::SdkLayoutOnly,
                provenance: ResolverProvenance::Red4extSdkLayout,
                exact_build_anchor_rva: None,
                pointer_offsets: Vec::new(),
                world_transform_offset: None,
            },
            coordinates: CoordinateCandidate {
                position_encoding: PositionEncoding::RedengineWorldPositionFixed17,
                orientation_encoding: OrientationEncoding::RedengineQuaternionIjkrF32,
                dataminer_axis_mapping: None,
                metres_per_world_unit: None,
                metric_calibration_id: None,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryScenario {
    StationaryHeadSweep,
    OnFootTranslation,
    AxisDirectionCalibration,
    TerminalLoopReturn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePoseEvidenceRun {
    /// Opaque launch identity. It proves process freshness but must not be a PID or pointer.
    pub process_launch_id: String,
    pub exact_build: Cp2077ExactBuild,
    pub camera_resolver_id: String,
    pub player_resolver_id: String,
    pub metric_calibration_id: String,
    pub scenarios: Vec<DiscoveryScenario>,
    pub selected_present_count: u64,
    pub camera_pose_count: u64,
    pub player_pose_count: u64,
    pub same_present_pair_count: u64,
    pub missing_pose_count: u64,
    pub stale_pose_reuse_count: u64,
    pub non_finite_value_count: u64,
    pub invalid_quaternion_count: u64,
    pub camera_changed_during_head_sweep: bool,
    pub player_stable_during_head_sweep: bool,
    pub camera_translated_on_foot: bool,
    pub player_translated_on_foot: bool,
    pub camera_player_streams_identical: bool,
    pub terminal_loop_return_observed: bool,
    pub selected_present_identity_passed: bool,
    pub projection_intrinsics_passed: bool,
    pub metric_e_geo_passed: bool,
    /// Committed evidence contains only RVAs, offsets and opaque IDs.
    pub absolute_addresses_redacted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoseDiscoveryStage {
    Blocked,
    /// Runtime facts are sufficient to author/review a binding. This is not recording readiness.
    EligibleForBindingAuthoring,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoseDiscoveryBlocker {
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cp2077PoseDiscoveryVerdict {
    pub schema: String,
    pub exact_build: Cp2077ExactBuild,
    pub stage: PoseDiscoveryStage,
    pub fresh_process_launches: usize,
    pub production_binding_emitted: bool,
    pub recording_ready: bool,
    pub blockers: Vec<PoseDiscoveryBlocker>,
}

fn block(blockers: &mut Vec<PoseDiscoveryBlocker>, code: &str, detail: impl Into<String>) {
    blockers.push(PoseDiscoveryBlocker {
        code: code.into(),
        detail: detail.into(),
    });
}

fn validate_resolver(
    resolver: &PoseResolverCandidate,
    expected_role: PoseRole,
    blockers: &mut Vec<PoseDiscoveryBlocker>,
) {
    if resolver.resolver_id.trim().is_empty() {
        block(
            blockers,
            "resolver_id_missing",
            format!("{expected_role:?}"),
        );
    }
    if resolver.role != expected_role {
        block(
            blockers,
            "resolver_role_mismatch",
            format!("expected {expected_role:?}, got {:?}", resolver.role),
        );
    }
    if resolver.kind == ResolverKind::SdkLayoutOnly
        || resolver.provenance == ResolverProvenance::Red4extSdkLayout
    {
        block(
            blockers,
            "sdk_layout_is_not_exact_binding",
            format!("{expected_role:?} has no exact-build runtime anchor"),
        );
    }
    if resolver.provenance == ResolverProvenance::LegacyIgcsSignature {
        block(
            blockers,
            "legacy_igcs_signature_forbidden",
            format!("{expected_role:?} must come from build 20383525 runtime evidence"),
        );
    }
    if resolver.provenance != ResolverProvenance::ExactBuildRuntimeDiscovery {
        block(
            blockers,
            "exact_runtime_provenance_missing",
            format!("{expected_role:?}"),
        );
    }
    if resolver.exact_build_anchor_rva.is_none_or(|rva| rva == 0) {
        block(
            blockers,
            "exact_build_anchor_missing",
            format!("{expected_role:?}"),
        );
    }
    if resolver.pointer_offsets.is_empty() {
        block(
            blockers,
            "pointer_chain_missing",
            format!("{expected_role:?}"),
        );
    }
    if resolver.world_transform_offset.is_none() {
        block(
            blockers,
            "world_transform_offset_missing",
            format!("{expected_role:?}"),
        );
    }
}

fn validate_coordinates(
    coordinates: &CoordinateCandidate,
    blockers: &mut Vec<PoseDiscoveryBlocker>,
) {
    if let Some(mapping) = coordinates.dataminer_axis_mapping {
        let source_axes = mapping.map(SignedAxis::source_axis);
        if BTreeSet::from(source_axes).len() != 3 {
            block(
                blockers,
                "axis_mapping_not_permutation",
                "DataMiner axes must use each source axis exactly once",
            );
        }
    } else {
        block(
            blockers,
            "axis_mapping_missing",
            "runtime direction evidence is required",
        );
    }

    if let Some(scale) = coordinates.metres_per_world_unit.as_deref() {
        if scale
            .parse::<f64>()
            .ok()
            .is_none_or(|value| !value.is_finite() || value <= 0.0)
        {
            block(blockers, "metric_scale_invalid", scale);
        }
    } else {
        block(
            blockers,
            "metric_scale_missing",
            "SDK fixed-point units are not metres proof",
        );
    }
    if coordinates
        .metric_calibration_id
        .as_deref()
        .is_none_or(str::is_empty)
    {
        block(
            blockers,
            "metric_calibration_missing",
            "an independently measured calibration/E-geo artifact is required",
        );
    }
}

/// Evaluate discovery evidence without ever emitting a production binding or readiness claim.
#[must_use]
pub fn evaluate_pose_discovery(
    candidate: &Cp2077PoseCandidate,
    runs: &[RuntimePoseEvidenceRun],
) -> Cp2077PoseDiscoveryVerdict {
    let mut blockers = Vec::new();
    if !candidate.exact_build.is_pinned() {
        block(
            &mut blockers,
            "candidate_exact_build_mismatch",
            "candidate is not pinned to CP2077 Steam build 20383525 / 2.31",
        );
    }
    validate_resolver(&candidate.camera, PoseRole::Camera, &mut blockers);
    validate_resolver(&candidate.player, PoseRole::Player, &mut blockers);
    if candidate.camera.resolver_id == candidate.player.resolver_id {
        block(
            &mut blockers,
            "camera_player_source_alias",
            "camera and player must have independent resolver identities",
        );
    }
    if candidate.camera.exact_build_anchor_rva == candidate.player.exact_build_anchor_rva
        && candidate.camera.pointer_offsets == candidate.player.pointer_offsets
        && candidate.camera.world_transform_offset == candidate.player.world_transform_offset
    {
        block(
            &mut blockers,
            "camera_player_chain_alias",
            "different labels cannot disguise one physical transform chain",
        );
    }
    validate_coordinates(&candidate.coordinates, &mut blockers);

    let launch_ids: BTreeSet<_> = runs
        .iter()
        .filter_map(|run| {
            let value = run.process_launch_id.trim();
            (!value.is_empty()).then_some(value)
        })
        .collect();
    if launch_ids.len() < MIN_FRESH_PROCESS_LAUNCHES {
        block(
            &mut blockers,
            "fresh_launch_coverage_insufficient",
            format!(
                "need {MIN_FRESH_PROCESS_LAUNCHES} unique fresh process launches, got {}",
                launch_ids.len()
            ),
        );
    }

    let observed_scenarios: BTreeSet<_> = runs
        .iter()
        .flat_map(|run| run.scenarios.iter().copied())
        .collect();
    for scenario in [
        DiscoveryScenario::StationaryHeadSweep,
        DiscoveryScenario::OnFootTranslation,
        DiscoveryScenario::AxisDirectionCalibration,
        DiscoveryScenario::TerminalLoopReturn,
    ] {
        if !observed_scenarios.contains(&scenario) {
            block(
                &mut blockers,
                "runtime_scenario_missing",
                format!("{scenario:?}"),
            );
        }
    }

    let calibration_id = candidate
        .coordinates
        .metric_calibration_id
        .as_deref()
        .unwrap_or_default();
    for (index, run) in runs.iter().enumerate() {
        let label = format!("run[{index}]");
        if !run.exact_build.is_pinned() || run.exact_build != candidate.exact_build {
            block(&mut blockers, "runtime_exact_build_mismatch", label.clone());
        }
        if run.camera_resolver_id != candidate.camera.resolver_id
            || run.player_resolver_id != candidate.player.resolver_id
        {
            block(&mut blockers, "runtime_resolver_drift", label.clone());
        }
        if run.metric_calibration_id != calibration_id {
            block(
                &mut blockers,
                "runtime_metric_calibration_drift",
                label.clone(),
            );
        }
        if run.selected_present_count < MIN_PRESENTS_PER_RUNTIME_RUN {
            block(
                &mut blockers,
                "runtime_present_coverage_too_short",
                label.clone(),
            );
        }
        if run.camera_pose_count != run.selected_present_count
            || run.player_pose_count != run.selected_present_count
            || run.same_present_pair_count != run.selected_present_count
        {
            block(
                &mut blockers,
                "runtime_present_pose_coverage_incomplete",
                label.clone(),
            );
        }
        if run.missing_pose_count != 0 || run.stale_pose_reuse_count != 0 {
            block(&mut blockers, "missing_or_stale_pose", label.clone());
        }
        if run.non_finite_value_count != 0 || run.invalid_quaternion_count != 0 {
            block(&mut blockers, "invalid_pose_values", label.clone());
        }
        if run.camera_player_streams_identical {
            block(
                &mut blockers,
                "camera_player_streams_identical",
                label.clone(),
            );
        }
        if run
            .scenarios
            .contains(&DiscoveryScenario::StationaryHeadSweep)
            && (!run.camera_changed_during_head_sweep || !run.player_stable_during_head_sweep)
        {
            block(&mut blockers, "head_sweep_separation_failed", label.clone());
        }
        if run
            .scenarios
            .contains(&DiscoveryScenario::OnFootTranslation)
            && (!run.camera_translated_on_foot || !run.player_translated_on_foot)
        {
            block(
                &mut blockers,
                "translation_correlation_failed",
                label.clone(),
            );
        }
        if run
            .scenarios
            .contains(&DiscoveryScenario::TerminalLoopReturn)
            && !run.terminal_loop_return_observed
        {
            block(&mut blockers, "terminal_loop_return_failed", label.clone());
        }
        if !run.selected_present_identity_passed
            || !run.projection_intrinsics_passed
            || !run.metric_e_geo_passed
        {
            block(
                &mut blockers,
                "present_projection_egeo_failed",
                label.clone(),
            );
        }
        if !run.absolute_addresses_redacted {
            block(&mut blockers, "absolute_addresses_not_redacted", label);
        }
    }

    let stage = if blockers.is_empty() {
        PoseDiscoveryStage::EligibleForBindingAuthoring
    } else {
        PoseDiscoveryStage::Blocked
    };
    Cp2077PoseDiscoveryVerdict {
        schema: CP2077_POSE_DISCOVERY_VERDICT_SCHEMA_V1.into(),
        exact_build: candidate.exact_build.clone(),
        stage,
        fresh_process_launches: launch_ids.len(),
        // A separate reviewed authoring step must translate evidence into CameraBinding/player
        // sampler code and then pass full capture validation.
        production_binding_emitted: false,
        recording_ready: false,
        blockers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(role: PoseRole, id: &str, rva: u64) -> PoseResolverCandidate {
        PoseResolverCandidate {
            resolver_id: id.into(),
            role,
            kind: ResolverKind::ModuleRelativePointerChain,
            provenance: ResolverProvenance::ExactBuildRuntimeDiscovery,
            exact_build_anchor_rva: Some(rva),
            pointer_offsets: vec![0x20, 0x18],
            world_transform_offset: Some(0x40),
        }
    }

    fn candidate() -> Cp2077PoseCandidate {
        Cp2077PoseCandidate {
            exact_build: Cp2077ExactBuild::pinned(),
            camera: resolver(PoseRole::Camera, "camera-exact-v1", 0x1000),
            player: resolver(PoseRole::Player, "player-exact-v1", 0x2000),
            coordinates: CoordinateCandidate {
                position_encoding: PositionEncoding::RedengineWorldPositionFixed17,
                orientation_encoding: OrientationEncoding::RedengineQuaternionIjkrF32,
                dataminer_axis_mapping: Some([
                    SignedAxis::PositiveX,
                    SignedAxis::PositiveZ,
                    SignedAxis::PositiveY,
                ]),
                metres_per_world_unit: Some("1.0".into()),
                metric_calibration_id: Some("synthetic-metric-egeo-v1".into()),
            },
        }
    }

    fn run(id: &str) -> RuntimePoseEvidenceRun {
        RuntimePoseEvidenceRun {
            process_launch_id: id.into(),
            exact_build: Cp2077ExactBuild::pinned(),
            camera_resolver_id: "camera-exact-v1".into(),
            player_resolver_id: "player-exact-v1".into(),
            metric_calibration_id: "synthetic-metric-egeo-v1".into(),
            scenarios: vec![
                DiscoveryScenario::StationaryHeadSweep,
                DiscoveryScenario::OnFootTranslation,
                DiscoveryScenario::AxisDirectionCalibration,
                DiscoveryScenario::TerminalLoopReturn,
            ],
            selected_present_count: 300,
            camera_pose_count: 300,
            player_pose_count: 300,
            same_present_pair_count: 300,
            missing_pose_count: 0,
            stale_pose_reuse_count: 0,
            non_finite_value_count: 0,
            invalid_quaternion_count: 0,
            camera_changed_during_head_sweep: true,
            player_stable_during_head_sweep: true,
            camera_translated_on_foot: true,
            player_translated_on_foot: true,
            camera_player_streams_identical: false,
            terminal_loop_return_observed: true,
            selected_present_identity_passed: true,
            projection_intrinsics_passed: true,
            metric_e_geo_passed: true,
            absolute_addresses_redacted: true,
        }
    }

    fn has_code(verdict: &Cp2077PoseDiscoveryVerdict, code: &str) -> bool {
        verdict.blockers.iter().any(|blocker| blocker.code == code)
    }

    #[test]
    fn sdk_layout_scaffold_is_explicitly_not_a_binding() {
        let verdict = evaluate_pose_discovery(&Cp2077PoseCandidate::sdk_layout_scaffold(), &[]);
        assert_eq!(verdict.stage, PoseDiscoveryStage::Blocked);
        assert!(!verdict.production_binding_emitted);
        assert!(!verdict.recording_ready);
        assert!(has_code(&verdict, "sdk_layout_is_not_exact_binding"));
        assert!(has_code(&verdict, "exact_build_anchor_missing"));
        assert!(has_code(&verdict, "metric_scale_missing"));
        assert!(has_code(&verdict, "fresh_launch_coverage_insufficient"));
    }

    #[test]
    fn legacy_igcs_candidate_is_rejected_even_with_an_address() {
        let mut candidate = candidate();
        candidate.camera.provenance = ResolverProvenance::LegacyIgcsSignature;
        let verdict = evaluate_pose_discovery(
            &candidate,
            &[run("launch-a"), run("launch-b"), run("launch-c")],
        );
        assert!(has_code(&verdict, "legacy_igcs_signature_forbidden"));
        assert_eq!(verdict.stage, PoseDiscoveryStage::Blocked);
    }

    #[test]
    fn camera_and_player_alias_is_never_accepted() {
        let mut candidate = candidate();
        candidate.player.resolver_id = candidate.camera.resolver_id.clone();
        let verdict = evaluate_pose_discovery(
            &candidate,
            &[run("launch-a"), run("launch-b"), run("launch-c")],
        );
        assert!(has_code(&verdict, "camera_player_source_alias"));
        assert_eq!(verdict.stage, PoseDiscoveryStage::Blocked);
    }

    #[test]
    fn different_labels_cannot_disguise_one_transform_chain() {
        let mut candidate = candidate();
        candidate.player.exact_build_anchor_rva = candidate.camera.exact_build_anchor_rva;
        candidate.player.pointer_offsets = candidate.camera.pointer_offsets.clone();
        candidate.player.world_transform_offset = candidate.camera.world_transform_offset;
        let verdict = evaluate_pose_discovery(
            &candidate,
            &[run("launch-a"), run("launch-b"), run("launch-c")],
        );
        assert!(has_code(&verdict, "camera_player_chain_alias"));
        assert_eq!(verdict.stage, PoseDiscoveryStage::Blocked);
    }

    #[test]
    fn three_evidence_rows_from_one_process_are_not_three_launches() {
        let verdict = evaluate_pose_discovery(
            &candidate(),
            &[run("same-launch"), run("same-launch"), run("same-launch")],
        );
        assert_eq!(verdict.fresh_process_launches, 1);
        assert!(has_code(&verdict, "fresh_launch_coverage_insufficient"));
    }

    #[test]
    fn complete_synthetic_evidence_only_reaches_binding_authoring() {
        let verdict = evaluate_pose_discovery(
            &candidate(),
            &[run("launch-a"), run("launch-b"), run("launch-c")],
        );
        assert!(verdict.blockers.is_empty(), "{:?}", verdict.blockers);
        assert_eq!(
            verdict.stage,
            PoseDiscoveryStage::EligibleForBindingAuthoring
        );
        assert!(!verdict.production_binding_emitted);
        assert!(!verdict.recording_ready);
    }

    #[test]
    fn missing_player_rows_and_camera_player_identity_fail_closed() {
        let mut bad = run("launch-a");
        bad.player_pose_count -= 1;
        bad.camera_player_streams_identical = true;
        let verdict =
            evaluate_pose_discovery(&candidate(), &[bad, run("launch-b"), run("launch-c")]);
        assert!(has_code(
            &verdict,
            "runtime_present_pose_coverage_incomplete"
        ));
        assert!(has_code(&verdict, "camera_player_streams_identical"));
        assert_eq!(verdict.stage, PoseDiscoveryStage::Blocked);
    }
}
