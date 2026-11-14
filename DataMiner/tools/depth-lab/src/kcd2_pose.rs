//! Build-pinned KCD2 D3D12 live pose extraction.
//!
//! This module contains only the platform-independent validation and same-Present join. The
//! Windows probe owns root-CBV acquisition and JSONL I/O. The resulting positions remain in
//! engine units and the horizontal root is still a research candidate; neither is silently
//! promoted to metric camera/player pose.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    calibration::MetricScaleSource,
    profile::{DepthProfile, RendererBackend},
};

pub const KCD2_LIVE_POSE_PLAN_SCHEMA_V1: &str = "kcd2-d3d12-live-pose-plan/v1";
pub const KCD2_LIVE_POSE_PLAN_SCHEMA_V2: &str = "kcd2-d3d12-live-pose-plan/v2";
pub const KCD2_LIVE_POSE_PLAN_SCHEMA_V3: &str = "kcd2-d3d12-live-pose-plan/v3";
pub const KCD2_LIVE_POSE_EVENT_SCHEMA_V2: &str = "kcd2-d3d12-live-pose/v2";
pub const KCD2_LIVE_POSE_EVENT_SCHEMA_V3: &str = "kcd2-d3d12-live-pose/v3";
pub const KCD2_EXECUTABLE_SHA256: &str =
    "sha256:d85b5355842d16290dc0d97e1d02856c6f2811a2021c8957aa24d7782a771b77";
pub const KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256: &str =
    "6286d44b972d430f9c835cf948552688db67c01c8765a74fd7fafb40e4d3897c";
pub const KCD2_VIEW_PROJECTION_ALIAS_ROOT_SIGNATURE_SHA256: &str =
    "fd5323ef4ecaa156517a8cfab0b2cc1792609d746aa432cf8857c41353fce3f4";
pub const KCD2_PLAYER_ROOT_SIGNATURE_SHA256: &str =
    "45dc805df7d14560cb98909e6949d085841d42a972da0aa9e1c6e44c62e3859e";
pub const KCD2_DEPTH_SOURCE_ROOT_SIGNATURE_SHA256: &str =
    "sha256:9d963c1d850bfb03d1094e0e34162640552c4b0e8e6f07d44e2758cf2e4c0391";
pub const KCD2_DEPTH_SOURCE_DRAW_ORDINAL: u32 = 0;
pub const KCD2_MAINLINE_DEPTH_CANDIDATE_SCHEMA_V1: &str = "kcd2-mainline-depth-candidate/v1";
pub const KCD2_MAINLINE_GAME_ID: &str = "kingdom-come-deliverance-2-steam";
pub const KCD2_COMBINED_BINDING_ID: &str = "kcd2-steam-23914554-combined-vp-v1";
pub const KCD2_COMBINED_BINDING_VERSION: &str = "1";
pub const KCD2_COMBINED_BINDING_ID_V2: &str = "kcd2-steam-23914554-combined-vp-v2";
pub const KCD2_COMBINED_BINDING_VERSION_V2: &str = "2";
pub const KCD2_MAIN_SCENE_DSV_ID: &str = "kcd2-r24g8-d24-main-scene-v1";
pub const KCD2_METRIC_EVIDENCE_SHA256: &str =
    "sha256:4d6ae9e814dd33c3b8d0ed559fc513036fce3db3f1c6dc5eac41dac7c735054c";
pub const KCD2_PROJECTION_A: f64 = -6.250038930842396e-6;
pub const KCD2_PROJECTION_B: f64 = 0.05000031366944313;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2MainlineDepthCandidate {
    pub schema: String,
    pub status: String,
    pub blockers: Vec<String>,
    pub profile: DepthProfile,
    pub live_pose_plan: Kcd2LivePosePlan,
    pub engine_to_delivery_axes: [[f64; 3]; 3],
    pub position_unit: String,
    pub metres_per_unit: f64,
}

impl Kcd2MainlineDepthCandidate {
    pub const STATUS: &'static str = "catalog_payload_authored_pending_runtime_acceptance";
    pub const REQUIRED_BLOCKERS: [&'static str; 3] = [
        "the integrated mainline runtime has not yet passed a fresh-machine capture verification",
        "the central four-stage performance certification manifest has no KCD2 entry",
        "a mainline 60-second depth-pose artifact and final delivery materialization have not yet passed together",
    ];

    /// Validates the KCD2 facts that can already be represented by mainline.
    /// A candidate with no blocker is rejected: catalog promotion is a separate,
    /// reviewed operation and must not happen by deleting one status string.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != KCD2_MAINLINE_DEPTH_CANDIDATE_SCHEMA_V1
            || self.status != Self::STATUS
            || self.blockers.len() != Self::REQUIRED_BLOCKERS.len()
            || Self::REQUIRED_BLOCKERS
                .iter()
                .any(|required| !self.blockers.iter().any(|actual| actual == required))
        {
            return Err("KCD2 mainline candidate status or blockers are invalid".into());
        }
        let profile = &self.profile;
        if profile.schema_version != DepthProfile::D3D12_STABLE_SCHEMA_VERSION
            || profile.game_id != KCD2_MAINLINE_GAME_ID
            || profile.renderer_backend != RendererBackend::D3d12
            || profile.executable_sha256 != KCD2_EXECUTABLE_SHA256
            || profile.dsv.expected_width != 1920
            || profile.dsv.expected_height != 1080
            || (profile.projection_a - KCD2_PROJECTION_A).abs() > 1.0e-15
            || (profile.projection_b - KCD2_PROJECTION_B).abs() > 1.0e-15
        {
            return Err("KCD2 mainline candidate build, geometry, or projection drifted".into());
        }
        let validation = profile.validate(1920, 1080);
        if !validation.is_empty() {
            return Err(format!(
                "KCD2 mainline candidate profile is invalid: {validation:?}"
            ));
        }
        self.live_pose_plan.validate(&profile.executable_sha256)?;
        if self.engine_to_delivery_axes != [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]
            || self.position_unit != "meter"
            || self.metres_per_unit != 1.0
        {
            return Err("KCD2 mainline candidate axes or metric position unit drifted".into());
        }
        match &profile.metric_scale.source {
            MetricScaleSource::AuthoredLevelMetadata {
                levels: 3,
                unit_size_metres,
            } if *unit_size_metres == 1.0
                && profile.metric_scale.value == 1.0
                && profile.metric_scale.evidence_sha256.as_deref()
                    == Some(KCD2_METRIC_EVIDENCE_SHA256) => {}
            _ => {
                return Err(
                    "KCD2 mainline candidate metric scale is not the pinned three-level evidence"
                        .into(),
                );
            }
        }
        Ok(())
    }
}

const VIEW_PROJECTION_PARAMETER: u32 = 2;
const VIEW_PROJECTION_PASS: u32 = 0;
const VIEW_PROJECTION_DRAW: u32 = 0;
const PLAYER_ROOT_PARAMETER: u32 = 0;
const PLAYER_ROOT_PASS: u32 = 5;
// The normal-exploration candidate moved the same exact root signature between these passes.
// Pass 39 occurred only for three transition Presents, but its unique near-camera root passed the
// same matrix and spatial gates; no other pass is admitted.
const PLAYER_ROOT_ADDITIONAL_PASSES: [u32; 3] = [7, 8, 39];
const PLAYER_ROOT_DRAW: u32 = 0;
const LEGACY_PLAYER_ROOT_MAX_DRAW: u32 = 3;
const PLAYER_ROOT_MAX_DRAW: u32 = 15;
const SNAPSHOT_BYTES: u32 = 256;
const TRANSLATION_FREE_MATRIX_OFFSET: u32 = 0;
const TRANSLATED_MATRIX_OFFSET: u32 = 80;
const PLAYER_ROOT_MATRIX_OFFSET: u32 = 0;
const PLAYER_ROOT_MATRIX_BYTES: u32 = 48;
const LAYOUT_TOLERANCE: f64 = 1.0e-6;
const ROOT_ROTATION_TOLERANCE: f64 = 1.0e-4;
const PROJECTION_AXIS_TOLERANCE: f64 = 5.0e-4;
const PROJECTION_DECOMPOSITION_TOLERANCE: f64 = 1.0e-9;
const MAX_PENDING_PRESENTS: usize = 4;
const MAX_PLAYER_ROOT_CANDIDATES: usize = 16;
const MAX_VIEW_PROJECTION_CANDIDATES: usize = 16;
// Across the sealed v2 evidence, accepted player roots stayed within 0.25 engine units
// horizontally of the same-Present camera. Keep 0.10 units of tolerance while rejecting nearby
// NPC roots that the former 1.0-unit gate admitted as ambiguous.
const PLAYER_ROOT_MAX_HORIZONTAL_CAMERA_OFFSET: f64 = 0.35;
const PLAYER_ROOT_MIN_VERTICAL_CAMERA_OFFSET: f64 = 0.5;
const PLAYER_ROOT_MAX_VERTICAL_CAMERA_OFFSET: f64 = 2.5;
const PLAYER_ROOT_DEDUPLICATION_DISTANCE: f64 = 0.01;
const RENDER_WIDTH: f64 = 1920.0;
const RENDER_HEIGHT: f64 = 1080.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2LivePosePlan {
    pub schema: String,
    pub executable_sha256: String,
    pub require_same_present: bool,
    pub maximum_depth_row_residual: f64,
    pub view_projection_binding: Kcd2LivePoseBinding,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_view_projection_bindings: Vec<Kcd2LivePoseBinding>,
    pub horizontal_player_root_binding: Kcd2LivePoseBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2LivePoseBinding {
    pub root_signature_sha256: String,
    pub root_parameter_index: u32,
    pub pass_ordinal: u32,
    /// Exact preserves legacy v1/v2. Observed records the list-local ordinal
    /// without treating command-list batching order as a source's identity.
    #[serde(default, skip_serializing_if = "Kcd2PassOrdinalPolicy::is_exact")]
    pub pass_ordinal_policy: Kcd2PassOrdinalPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_pass_ordinals: Vec<u32>,
    pub draw_ordinal: u32,
    pub maximum_draw_ordinal: u32,
    pub read_offset_bytes: u32,
    pub read_size_bytes: u32,
    pub primary_matrix_byte_offset: u32,
    pub secondary_matrix_byte_offset: Option<u32>,
    pub matrix_bytes: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kcd2PassOrdinalPolicy {
    #[default]
    Exact,
    Observed,
}

impl Kcd2PassOrdinalPolicy {
    fn is_exact(&self) -> bool {
        *self == Self::Exact
    }
}

impl Kcd2LivePoseBinding {
    fn accepts_draw(&self, pass_ordinal: u32, draw_ordinal: u32) -> bool {
        (self.pass_ordinal_policy == Kcd2PassOrdinalPolicy::Observed
            || self.pass_ordinal == pass_ordinal
            || self.additional_pass_ordinals.contains(&pass_ordinal))
            && self.draw_ordinal <= draw_ordinal
            && draw_ordinal <= self.maximum_draw_ordinal
    }
}

impl Kcd2LivePosePlan {
    #[must_use]
    pub fn pinned() -> Self {
        Self {
            schema: KCD2_LIVE_POSE_PLAN_SCHEMA_V2.into(),
            executable_sha256: KCD2_EXECUTABLE_SHA256.into(),
            require_same_present: true,
            maximum_depth_row_residual: 1.0e-6,
            view_projection_binding: Kcd2LivePoseBinding {
                root_signature_sha256: KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256.into(),
                root_parameter_index: VIEW_PROJECTION_PARAMETER,
                pass_ordinal: VIEW_PROJECTION_PASS,
                pass_ordinal_policy: Kcd2PassOrdinalPolicy::Exact,
                additional_pass_ordinals: Vec::new(),
                draw_ordinal: VIEW_PROJECTION_DRAW,
                maximum_draw_ordinal: VIEW_PROJECTION_DRAW,
                read_offset_bytes: 0,
                read_size_bytes: SNAPSHOT_BYTES,
                primary_matrix_byte_offset: TRANSLATION_FREE_MATRIX_OFFSET,
                secondary_matrix_byte_offset: Some(TRANSLATED_MATRIX_OFFSET),
                matrix_bytes: 64,
            },
            additional_view_projection_bindings: Vec::new(),
            horizontal_player_root_binding: Kcd2LivePoseBinding {
                root_signature_sha256: KCD2_PLAYER_ROOT_SIGNATURE_SHA256.into(),
                root_parameter_index: PLAYER_ROOT_PARAMETER,
                pass_ordinal: PLAYER_ROOT_PASS,
                pass_ordinal_policy: Kcd2PassOrdinalPolicy::Exact,
                additional_pass_ordinals: PLAYER_ROOT_ADDITIONAL_PASSES.to_vec(),
                draw_ordinal: PLAYER_ROOT_DRAW,
                maximum_draw_ordinal: PLAYER_ROOT_MAX_DRAW,
                read_offset_bytes: 0,
                read_size_bytes: SNAPSHOT_BYTES,
                primary_matrix_byte_offset: PLAYER_ROOT_MATRIX_OFFSET,
                secondary_matrix_byte_offset: None,
                matrix_bytes: PLAYER_ROOT_MATRIX_BYTES,
            },
        }
    }

    /// Explicit research candidate. `pinned()` intentionally remains v2 until
    /// a separately reviewed binding/catalog elects this source contract.
    #[must_use]
    pub fn pinned_v3() -> Self {
        let mut plan = Self::pinned();
        plan.schema = KCD2_LIVE_POSE_PLAN_SCHEMA_V3.into();
        plan.view_projection_binding.pass_ordinal_policy = Kcd2PassOrdinalPolicy::Observed;
        plan.horizontal_player_root_binding.pass_ordinal_policy = Kcd2PassOrdinalPolicy::Observed;
        plan.horizontal_player_root_binding
            .additional_pass_ordinals
            .clear();
        let mut alias = plan.view_projection_binding.clone();
        alias.root_signature_sha256 = KCD2_VIEW_PROJECTION_ALIAS_ROOT_SIGNATURE_SHA256.into();
        alias.root_parameter_index = 1;
        plan.additional_view_projection_bindings.push(alias);
        plan
    }

    pub fn validate(&self, actual_executable_sha256: &str) -> Result<(), String> {
        if self.schema != KCD2_LIVE_POSE_PLAN_SCHEMA_V1
            && self.schema != KCD2_LIVE_POSE_PLAN_SCHEMA_V2
            && self.schema != KCD2_LIVE_POSE_PLAN_SCHEMA_V3
        {
            return Err(format!(
                "KCD2 live pose schema must be {KCD2_LIVE_POSE_PLAN_SCHEMA_V1}, {KCD2_LIVE_POSE_PLAN_SCHEMA_V2}, or {KCD2_LIVE_POSE_PLAN_SCHEMA_V3}"
            ));
        }
        if self.executable_sha256 != KCD2_EXECUTABLE_SHA256
            || !self
                .executable_sha256
                .eq_ignore_ascii_case(actual_executable_sha256)
        {
            return Err("KCD2 live pose executable identity is not the pinned build".into());
        }
        if !self.require_same_present {
            return Err("KCD2 live pose must require a same-Present pair".into());
        }
        if !self.maximum_depth_row_residual.is_finite()
            || self.maximum_depth_row_residual <= 0.0
            || self.maximum_depth_row_residual > 1.0e-3
        {
            return Err("KCD2 live pose residual limit is invalid or too permissive".into());
        }
        let pinned = match self.schema.as_str() {
            KCD2_LIVE_POSE_PLAN_SCHEMA_V1 => Self::legacy_pinned_v1(),
            KCD2_LIVE_POSE_PLAN_SCHEMA_V3 => Self::pinned_v3(),
            _ => Self::pinned(),
        };
        if self.view_projection_binding != pinned.view_projection_binding
            || self.horizontal_player_root_binding != pinned.horizontal_player_root_binding
            || self.additional_view_projection_bindings
                != pinned.additional_view_projection_bindings
        {
            return Err(
                "KCD2 live pose binding identity/layout drifted from its pinned schema".into(),
            );
        }
        Ok(())
    }

    fn legacy_pinned_v1() -> Self {
        let mut plan = Self::pinned();
        plan.schema = KCD2_LIVE_POSE_PLAN_SCHEMA_V1.into();
        plan.horizontal_player_root_binding
            .additional_pass_ordinals
            .clear();
        plan.horizontal_player_root_binding.maximum_draw_ordinal = LEGACY_PLAYER_ROOT_MAX_DRAW;
        plan
    }

    /// Shared draw router for the hook and durable source provenance validator.
    #[must_use]
    pub fn binding_for_draw(
        &self,
        root_signature_sha256: &str,
        pass_ordinal: u32,
        draw_ordinal: u32,
    ) -> Option<(Kcd2LivePoseSourceKind, &Kcd2LivePoseBinding)> {
        let matches = |binding: &&Kcd2LivePoseBinding| {
            binding.root_signature_sha256.eq_ignore_ascii_case(
                root_signature_sha256
                    .strip_prefix("sha256:")
                    .unwrap_or(root_signature_sha256),
            ) && binding.accepts_draw(pass_ordinal, draw_ordinal)
        };
        std::iter::once(&self.view_projection_binding)
            .chain(&self.additional_view_projection_bindings)
            .find(matches)
            .map(|binding| (Kcd2LivePoseSourceKind::ViewProjection, binding))
            .or_else(|| {
                matches(&&self.horizontal_player_root_binding).then_some((
                    Kcd2LivePoseSourceKind::HorizontalPlayerRoot,
                    &self.horizontal_player_root_binding,
                ))
            })
    }

    #[must_use]
    pub fn classify_source(
        &self,
        root_signature_sha256: &str,
        root_parameter_index: u32,
        pass_ordinal: u32,
        draw_ordinal: u32,
        read_offset_bytes: u32,
        read_size_bytes: u32,
    ) -> Option<Kcd2LivePoseSourceKind> {
        let matches = |binding: &Kcd2LivePoseBinding| {
            binding.root_signature_sha256.eq_ignore_ascii_case(
                root_signature_sha256
                    .strip_prefix("sha256:")
                    .unwrap_or(root_signature_sha256),
            ) && binding.root_parameter_index == root_parameter_index
                && binding.accepts_draw(pass_ordinal, draw_ordinal)
                && binding.read_offset_bytes == read_offset_bytes
                && binding.read_size_bytes == read_size_bytes
        };
        if matches(&self.view_projection_binding)
            || self.additional_view_projection_bindings.iter().any(matches)
        {
            Some(Kcd2LivePoseSourceKind::ViewProjection)
        } else if matches(&self.horizontal_player_root_binding) {
            Some(Kcd2LivePoseSourceKind::HorizontalPlayerRoot)
        } else {
            None
        }
    }

    #[must_use]
    pub fn accepts_player_root_source(&self, pass_ordinal: u32, draw_ordinal: u32) -> bool {
        self.horizontal_player_root_binding
            .accepts_draw(pass_ordinal, draw_ordinal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kcd2LivePoseSourceKind {
    ViewProjection,
    HorizontalPlayerRoot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2LivePoseSourceIdentity {
    pub root_signature_sha256: String,
    pub root_parameter_index: u32,
    pub pass_ordinal: u32,
    pub draw_ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2LivePosePair {
    pub present_sequence: u64,
    pub view_projection_sample_qpc: i64,
    pub player_root_sample_qpc: i64,
    pub present_boundary_qpc: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_projection_source: Option<Kcd2LivePoseSourceIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizontal_player_root_source: Option<Kcd2LivePoseSourceIdentity>,
    pub camera_position_engine_units: [f64; 3],
    pub camera_rotation_quaternion_xyzw: [f64; 4],
    pub camera_intrinsics_1920x1080: Kcd2CameraIntrinsics,
    pub horizontal_fov_degrees: f64,
    pub aspect_ratio: f64,
    pub projection_a: f64,
    pub projection_b: f64,
    pub projection_decomposition_residual: f64,
    pub horizontal_player_root_position_engine_units: [f64; 3],
    pub horizontal_player_root_rotation_quaternion_xyzw: [f64; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizontal_player_root_pass_ordinal: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizontal_player_root_draw_ordinal: Option<u32>,
    pub camera_minus_root_engine_units: [f64; 3],
    pub held_out_depth_row_residual: f64,
    pub horizontal_player_root_rotation_3x3_row_major: [f64; 9],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kcd2CameraIntrinsics {
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kcd2LivePoseDiscard {
    pub present_sequence: u64,
    pub missing: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Kcd2LivePoseOutcome {
    Pair(Box<Kcd2LivePosePair>),
    Discarded(Kcd2LivePoseDiscard),
}

#[derive(Debug, Clone)]
struct ViewProjectionObservation {
    sample_qpc: i64,
    source: Option<Kcd2LivePoseSourceIdentity>,
    camera_position: [f64; 3],
    camera_rotation_quaternion: [f64; 4],
    camera_intrinsics: Kcd2CameraIntrinsics,
    horizontal_fov_degrees: f64,
    aspect_ratio: f64,
    projection_a: f64,
    projection_b: f64,
    projection_decomposition_residual: f64,
    depth_row_residual: f64,
}

#[derive(Debug, Clone)]
struct PlayerRootObservation {
    sample_qpc: i64,
    source: Option<Kcd2LivePoseSourceIdentity>,
    pass_ordinal: u32,
    draw_ordinal: u32,
    position: [f64; 3],
    rotation: [f64; 9],
    rotation_quaternion: [f64; 4],
}

#[derive(Debug, Default)]
struct PendingPose {
    view_projection: Option<ViewProjectionObservation>,
    view_projection_bytes: Option<[u8; SNAPSHOT_BYTES as usize]>,
    view_projection_candidates: usize,
    rejected: Option<&'static str>,
    player_roots: Vec<PlayerRootObservation>,
}

#[derive(Debug)]
pub struct Kcd2LivePoseExtractor {
    plan: Kcd2LivePosePlan,
    pending: BTreeMap<u64, PendingPose>,
    last_finished_present: Option<u64>,
    last_finished_qpc: Option<i64>,
}

impl Kcd2LivePoseExtractor {
    pub fn new(plan: Kcd2LivePosePlan, actual_executable_sha256: &str) -> Result<Self, String> {
        plan.validate(actual_executable_sha256)?;
        Ok(Self {
            plan,
            pending: BTreeMap::new(),
            last_finished_present: None,
            last_finished_qpc: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe_snapshot(
        &mut self,
        present_sequence: u64,
        root_signature_sha256: &str,
        root_parameter_index: u32,
        pass_ordinal: u32,
        draw_ordinal: u32,
        read_offset_bytes: u32,
        sample_qpc: i64,
        bytes: &[u8],
    ) -> Result<bool, String> {
        if sample_qpc <= 0 {
            return Err("KCD2 live pose snapshot QPC must be positive".into());
        }
        if self
            .last_finished_present
            .is_some_and(|finished| present_sequence <= finished)
        {
            return Err(format!(
                "KCD2 live pose snapshot arrived after Present {present_sequence} was closed"
            ));
        }
        let read_size_bytes = u32::try_from(bytes.len())
            .map_err(|_| "KCD2 live pose snapshot length exceeds u32".to_string())?;
        let Some(kind) = self.plan.classify_source(
            root_signature_sha256,
            root_parameter_index,
            pass_ordinal,
            draw_ordinal,
            read_offset_bytes,
            read_size_bytes,
        ) else {
            return Ok(false);
        };
        let v3 = self.plan.schema == KCD2_LIVE_POSE_PLAN_SCHEMA_V3;
        let actual_source = v3.then(|| Kcd2LivePoseSourceIdentity {
            root_signature_sha256: root_signature_sha256
                .strip_prefix("sha256:")
                .unwrap_or(root_signature_sha256)
                .to_ascii_lowercase(),
            root_parameter_index,
            pass_ordinal,
            draw_ordinal,
        });
        if !self.pending.contains_key(&present_sequence)
            && self.pending.len() >= MAX_PENDING_PRESENTS
        {
            return Err(format!(
                "KCD2 live pose exceeded {MAX_PENDING_PRESENTS} pending Presents without a boundary"
            ));
        }
        let pending = self.pending.entry(present_sequence).or_default();
        if let Some(reason) = pending.rejected {
            return Err(format!(
                "KCD2 Present {present_sequence} was already rejected: {reason}"
            ));
        }
        match kind {
            Kcd2LivePoseSourceKind::ViewProjection => {
                if !v3 && pending.view_projection.is_some() {
                    return Err(format!(
                        "duplicate KCD2 view-projection snapshot at Present {present_sequence}"
                    ));
                }
                if v3 && pending.view_projection_candidates >= MAX_VIEW_PROJECTION_CANDIDATES {
                    pending.rejected = Some("view_projection_candidate_limit");
                    return Err(format!(
                        "too many KCD2 view-projection candidates at Present {present_sequence}"
                    ));
                }
                let decoded =
                    decode_view_projection(bytes, sample_qpc, self.plan.maximum_depth_row_residual);
                if v3 && decoded.is_err() {
                    pending.rejected = Some("view_projection_invalid");
                }
                let mut observation = decoded?;
                observation.source = actual_source;
                if v3 {
                    pending.view_projection_candidates += 1;
                    let bytes: [u8; SNAPSHOT_BYTES as usize] =
                        bytes.try_into().expect("decoder validated snapshot length");
                    if let Some(previous) = &pending.view_projection_bytes {
                        if previous != &bytes {
                            pending.rejected = Some("view_projection_conflicting_bytes");
                            return Err(format!(
                                "conflicting KCD2 view-projection bytes at Present {present_sequence}"
                            ));
                        }
                        // Keep the first actual source and its paired QPC. Identical
                        // later consumers corroborate bytes; they do not overwrite provenance.
                        return Ok(true);
                    }
                    pending.view_projection_bytes = Some(bytes);
                }
                pending.view_projection = Some(observation);
            }
            Kcd2LivePoseSourceKind::HorizontalPlayerRoot => {
                if pending.player_roots.len() >= MAX_PLAYER_ROOT_CANDIDATES {
                    if v3 {
                        pending.rejected = Some("horizontal_player_root_candidate_limit");
                    }
                    return Err(format!(
                        "too many KCD2 horizontal root candidates at Present {present_sequence}"
                    ));
                }
                let decoded = decode_player_root(bytes, sample_qpc, pass_ordinal, draw_ordinal);
                if v3 && decoded.is_err() {
                    pending.rejected = Some("horizontal_player_root_invalid");
                }
                let mut observation = decoded?;
                observation.source = actual_source;
                pending.player_roots.push(observation);
            }
        }
        Ok(true)
    }

    pub fn finish_present(
        &mut self,
        present_sequence: u64,
        present_boundary_qpc: i64,
    ) -> Result<Option<Kcd2LivePoseOutcome>, String> {
        if present_boundary_qpc <= 0
            || self
                .last_finished_qpc
                .is_some_and(|previous| present_boundary_qpc <= previous)
        {
            return Err(format!(
                "KCD2 live pose Present boundary QPC is invalid or non-increasing: {present_boundary_qpc}"
            ));
        }
        if self
            .last_finished_present
            .is_some_and(|finished| present_sequence <= finished)
        {
            return Err(format!(
                "KCD2 live pose Present boundary is duplicate or out of order: {present_sequence}"
            ));
        }
        if self
            .pending
            .first_key_value()
            .is_some_and(|(&pending, _)| pending < present_sequence)
        {
            return Err(format!(
                "KCD2 live pose missed a Present boundary before {present_sequence}"
            ));
        }
        self.last_finished_present = Some(present_sequence);
        self.last_finished_qpc = Some(present_boundary_qpc);
        let Some(pending) = self.pending.remove(&present_sequence) else {
            return Ok(None);
        };
        Ok(Some(outcome_for_pending(
            present_sequence,
            present_boundary_qpc,
            pending,
        )))
    }

    pub fn finish_pending(&mut self) -> Vec<Kcd2LivePoseOutcome> {
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(present_sequence, pending)| {
                let mut missing = missing_for_pending(&pending);
                missing.push("present_boundary");
                Kcd2LivePoseOutcome::Discarded(Kcd2LivePoseDiscard {
                    present_sequence,
                    missing,
                })
            })
            .collect()
    }
}

fn missing_for_pending(pending: &PendingPose) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if let Some(reason) = pending.rejected {
        missing.push(reason);
    }
    if pending.view_projection.is_none() {
        missing.push("view_projection");
    }
    if pending.player_roots.is_empty() {
        missing.push("horizontal_player_root");
    } else if let Some(view_projection) = pending.view_projection.as_ref()
        && let Err(reason) = select_horizontal_player_root(view_projection, &pending.player_roots)
    {
        missing.push(reason);
    }
    missing
}

fn outcome_for_pending(
    present_sequence: u64,
    present_boundary_qpc: i64,
    pending: PendingPose,
) -> Kcd2LivePoseOutcome {
    let missing = missing_for_pending(&pending);
    if !missing.is_empty() {
        return Kcd2LivePoseOutcome::Discarded(Kcd2LivePoseDiscard {
            present_sequence,
            missing,
        });
    }
    match pending.view_projection {
        Some(view_projection) => {
            let player_root =
                select_horizontal_player_root(&view_projection, &pending.player_roots)
                    .expect("missing_for_pending accepted one unique same-Present player root");
            let camera_minus_root = std::array::from_fn(|index| {
                view_projection.camera_position[index] - player_root.position[index]
            });
            Kcd2LivePoseOutcome::Pair(Box::new(Kcd2LivePosePair {
                present_sequence,
                view_projection_sample_qpc: view_projection.sample_qpc,
                player_root_sample_qpc: player_root.sample_qpc,
                present_boundary_qpc,
                view_projection_source: view_projection.source.clone(),
                horizontal_player_root_source: player_root.source.clone(),
                camera_position_engine_units: view_projection.camera_position,
                camera_rotation_quaternion_xyzw: view_projection.camera_rotation_quaternion,
                camera_intrinsics_1920x1080: view_projection.camera_intrinsics,
                horizontal_fov_degrees: view_projection.horizontal_fov_degrees,
                aspect_ratio: view_projection.aspect_ratio,
                projection_a: view_projection.projection_a,
                projection_b: view_projection.projection_b,
                projection_decomposition_residual: view_projection
                    .projection_decomposition_residual,
                horizontal_player_root_position_engine_units: player_root.position,
                horizontal_player_root_rotation_quaternion_xyzw: player_root.rotation_quaternion,
                horizontal_player_root_pass_ordinal: Some(player_root.pass_ordinal),
                horizontal_player_root_draw_ordinal: Some(player_root.draw_ordinal),
                camera_minus_root_engine_units: camera_minus_root,
                held_out_depth_row_residual: view_projection.depth_row_residual,
                horizontal_player_root_rotation_3x3_row_major: player_root.rotation,
            }))
        }
        None => unreachable!("missing_for_pending rejected an absent view-projection"),
    }
}

fn select_horizontal_player_root<'a>(
    view_projection: &ViewProjectionObservation,
    candidates: &'a [PlayerRootObservation],
) -> Result<&'a PlayerRootObservation, &'static str> {
    let mut selected: Vec<&PlayerRootObservation> = Vec::new();
    for candidate in candidates {
        let delta = std::array::from_fn::<_, 3, _>(|axis| {
            view_projection.camera_position[axis] - candidate.position[axis]
        });
        let horizontal = delta[0].hypot(delta[1]);
        if horizontal <= PLAYER_ROOT_MAX_HORIZONTAL_CAMERA_OFFSET
            && (PLAYER_ROOT_MIN_VERTICAL_CAMERA_OFFSET..=PLAYER_ROOT_MAX_VERTICAL_CAMERA_OFFSET)
                .contains(&delta[2])
        {
            let duplicate = selected.iter().find(|existing| {
                existing
                    .position
                    .iter()
                    .zip(candidate.position)
                    .map(|(left, right)| (left - right) * (left - right))
                    .sum::<f64>()
                    .sqrt()
                    <= PLAYER_ROOT_DEDUPLICATION_DISTANCE
            });
            if let Some(existing) = duplicate {
                if view_projection.source.is_some()
                    && existing
                        .rotation
                        .iter()
                        .zip(candidate.rotation)
                        .any(|(left, right)| (left - right).abs() > ROOT_ROTATION_TOLERANCE)
                {
                    return Err("horizontal_player_root_rotation_conflict");
                }
                continue;
            }
            selected.push(candidate);
        }
    }
    match selected.as_slice() {
        [selected] => Ok(*selected),
        [] => Err("horizontal_player_root_not_near_camera"),
        _ => Err("horizontal_player_root_ambiguous"),
    }
}

fn decode_view_projection(
    bytes: &[u8],
    sample_qpc: i64,
    maximum_depth_row_residual: f64,
) -> Result<ViewProjectionObservation, String> {
    if bytes.len() != SNAPSHOT_BYTES as usize {
        return Err("KCD2 view-projection snapshot is not 256 bytes".into());
    }
    let translation_free = read_f32_values(bytes, TRANSLATION_FREE_MATRIX_OFFSET as usize, 16)?;
    let translated = read_f32_values(bytes, TRANSLATED_MATRIX_OFFSET as usize, 16)?;
    for index in [0usize, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14] {
        if (translation_free[index] - translated[index]).abs() > LAYOUT_TOLERANCE {
            return Err(format!(
                "KCD2 view-projection coefficient {index} diverged between pinned layouts"
            ));
        }
    }
    if [3usize, 7, 15]
        .into_iter()
        .any(|index| translation_free[index].abs() > LAYOUT_TOLERANCE)
    {
        return Err("KCD2 translation-free view-projection layout is invalid".into());
    }
    let camera_position = solve_three_by_three(
        [
            [translated[0], translated[1], translated[2]],
            [translated[4], translated[5], translated[6]],
            [translated[12], translated[13], translated[14]],
        ],
        [-translated[3], -translated[7], -translated[15]],
    )?;
    let depth_row_residual = translated[8] * camera_position[0]
        + translated[9] * camera_position[1]
        + translated[10] * camera_position[2]
        + translated[11]
        - translation_free[11];
    if !depth_row_residual.is_finite() || depth_row_residual.abs() > maximum_depth_row_residual {
        return Err(format!(
            "KCD2 held-out depth-row residual {depth_row_residual} exceeds {maximum_depth_row_residual}"
        ));
    }
    let row0 = [
        translation_free[0],
        translation_free[1],
        translation_free[2],
    ];
    let row1 = [
        translation_free[4],
        translation_free[5],
        translation_free[6],
    ];
    let row2 = [
        translation_free[8],
        translation_free[9],
        translation_free[10],
    ];
    let row3 = [
        translation_free[12],
        translation_free[13],
        translation_free[14],
    ];
    let forward_norm = vector_norm(row3);
    if forward_norm <= 0.0 || (forward_norm - 1.0).abs() > PROJECTION_AXIS_TOLERANCE {
        return Err("KCD2 combined view-projection forward axis is invalid".into());
    }
    let forward = scale_vector(row3, 1.0 / forward_norm);
    let projection_offset_x = dot(row0, forward);
    let projection_offset_y = dot(row1, forward);
    let right_raw = subtract_vector(row0, scale_vector(forward, projection_offset_x));
    let projected_up_raw = subtract_vector(row1, scale_vector(forward, projection_offset_y));
    let horizontal_scale = vector_norm(right_raw);
    let vertical_scale = vector_norm(projected_up_raw);
    if horizontal_scale <= 0.0 || vertical_scale <= 0.0 {
        return Err("KCD2 combined view-projection scale is degenerate".into());
    }
    let right = scale_vector(right_raw, 1.0 / horizontal_scale);
    // The D3D viewport convention flips the projected Y axis. Undo that flip before
    // constructing the proper right-handed CryEngine view basis.
    let up = scale_vector(projected_up_raw, -1.0 / vertical_scale);
    validate_orthonormal_basis([right, up, forward], PROJECTION_AXIS_TOLERANCE)?;
    let camera_to_world = [
        [right[0], up[0], forward[0]],
        [right[1], up[1], forward[1]],
        [right[2], up[2], forward[2]],
    ];
    let camera_rotation_quaternion = matrix_to_quaternion(camera_to_world);
    let projection_a = dot(row2, row3) / (forward_norm * forward_norm);
    let projection_residual = vector_norm(subtract_vector(row2, scale_vector(row3, projection_a)));
    let projection_b = translation_free[11];
    if !projection_a.is_finite()
        || !projection_b.is_finite()
        || projection_b == 0.0
        || projection_residual > PROJECTION_DECOMPOSITION_TOLERANCE
    {
        return Err(format!(
            "KCD2 projection decomposition is invalid: a={projection_a} b={projection_b} residual={projection_residual}"
        ));
    }
    let horizontal_fov_degrees = 2.0 * (1.0 / horizontal_scale).atan().to_degrees();
    let aspect_ratio = vertical_scale / horizontal_scale;
    let expected_aspect = RENDER_WIDTH / RENDER_HEIGHT;
    if (aspect_ratio - expected_aspect).abs() > PROJECTION_AXIS_TOLERANCE {
        return Err(format!(
            "KCD2 projection aspect {aspect_ratio} does not match {expected_aspect}"
        ));
    }
    let camera_intrinsics = Kcd2CameraIntrinsics {
        fx: 0.5 * RENDER_WIDTH * horizontal_scale,
        fy: 0.5 * RENDER_HEIGHT * vertical_scale,
        cx: 0.5 * RENDER_WIDTH * (1.0 + projection_offset_x),
        cy: 0.5 * RENDER_HEIGHT * (1.0 - projection_offset_y),
    };
    if ![
        camera_intrinsics.fx,
        camera_intrinsics.fy,
        camera_intrinsics.cx,
        camera_intrinsics.cy,
        horizontal_fov_degrees,
        aspect_ratio,
    ]
    .into_iter()
    .all(|value| value.is_finite() && value > 0.0)
        || (camera_intrinsics.fx / camera_intrinsics.fy - 1.0).abs() > PROJECTION_AXIS_TOLERANCE
    {
        return Err("KCD2 decomposed camera intrinsics are invalid".into());
    }
    Ok(ViewProjectionObservation {
        sample_qpc,
        source: None,
        camera_position,
        camera_rotation_quaternion,
        camera_intrinsics,
        horizontal_fov_degrees,
        aspect_ratio,
        projection_a,
        projection_b,
        projection_decomposition_residual: projection_residual,
        depth_row_residual,
    })
}

fn decode_player_root(
    bytes: &[u8],
    sample_qpc: i64,
    pass_ordinal: u32,
    draw_ordinal: u32,
) -> Result<PlayerRootObservation, String> {
    if bytes.len() != SNAPSHOT_BYTES as usize {
        return Err("KCD2 horizontal root snapshot is not 256 bytes".into());
    }
    let matrix = read_f32_values(bytes, PLAYER_ROOT_MATRIX_OFFSET as usize, 12)?;
    let rotation = [
        matrix[0], matrix[1], matrix[2], matrix[4], matrix[5], matrix[6], matrix[8], matrix[9],
        matrix[10],
    ];
    validate_horizontal_root_rotation(rotation)?;
    let rotation_quaternion = matrix_to_quaternion([
        [rotation[0], rotation[1], rotation[2]],
        [rotation[3], rotation[4], rotation[5]],
        [rotation[6], rotation[7], rotation[8]],
    ]);
    Ok(PlayerRootObservation {
        sample_qpc,
        source: None,
        pass_ordinal,
        draw_ordinal,
        position: [matrix[3], matrix[7], matrix[11]],
        rotation,
        rotation_quaternion,
    })
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter().zip(right).map(|(a, b)| a * b).sum()
}

fn vector_norm(value: [f64; 3]) -> f64 {
    dot(value, value).sqrt()
}

fn scale_vector(value: [f64; 3], scale: f64) -> [f64; 3] {
    value.map(|component| component * scale)
}

fn subtract_vector(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|index| left[index] - right[index])
}

fn validate_orthonormal_basis(rows: [[f64; 3]; 3], tolerance: f64) -> Result<(), String> {
    for row in rows {
        if (vector_norm(row) - 1.0).abs() > tolerance {
            return Err("KCD2 decomposed camera basis row is not unit length".into());
        }
    }
    for (left, right) in [(0usize, 1usize), (0, 2), (1, 2)] {
        if dot(rows[left], rows[right]).abs() > tolerance {
            return Err("KCD2 decomposed camera basis is not orthogonal".into());
        }
    }
    let determinant = rows[0][0] * (rows[1][1] * rows[2][2] - rows[1][2] * rows[2][1])
        - rows[0][1] * (rows[1][0] * rows[2][2] - rows[1][2] * rows[2][0])
        + rows[0][2] * (rows[1][0] * rows[2][1] - rows[1][1] * rows[2][0]);
    if (determinant - 1.0).abs() > tolerance {
        return Err("KCD2 decomposed camera basis is not proper".into());
    }
    Ok(())
}

fn matrix_to_quaternion(matrix: [[f64; 3]; 3]) -> [f64; 4] {
    let matrix = matrix.map(|row| row.map(|value| value as f32));
    let mut quaternion = crate::camera::mat3_to_quat(&matrix).map(f64::from);
    if quaternion[3] < 0.0 {
        quaternion = quaternion.map(|value| -value);
    }
    quaternion
}

fn validate_horizontal_root_rotation(rotation: [f64; 9]) -> Result<(), String> {
    let rows = [
        [rotation[0], rotation[1], rotation[2]],
        [rotation[3], rotation[4], rotation[5]],
        [rotation[6], rotation[7], rotation[8]],
    ];
    for (index, row) in rows.iter().enumerate() {
        let norm_squared = row.iter().map(|value| value * value).sum::<f64>();
        if (norm_squared - 1.0).abs() > ROOT_ROTATION_TOLERANCE {
            return Err(format!(
                "KCD2 horizontal root rotation row {index} is not unit length"
            ));
        }
    }
    for (left, right) in [(0usize, 1usize), (0, 2), (1, 2)] {
        let dot = rows[left]
            .iter()
            .zip(rows[right])
            .map(|(left, right)| left * right)
            .sum::<f64>();
        if dot.abs() > ROOT_ROTATION_TOLERANCE {
            return Err("KCD2 horizontal root rotation rows are not orthogonal".into());
        }
    }
    let determinant = rotation[0] * (rotation[4] * rotation[8] - rotation[5] * rotation[7])
        - rotation[1] * (rotation[3] * rotation[8] - rotation[5] * rotation[6])
        + rotation[2] * (rotation[3] * rotation[7] - rotation[4] * rotation[6]);
    if (determinant - 1.0).abs() > ROOT_ROTATION_TOLERANCE {
        return Err("KCD2 horizontal root rotation is not proper".into());
    }
    if [rotation[2], rotation[5], rotation[6], rotation[7]]
        .into_iter()
        .any(|value| value.abs() > ROOT_ROTATION_TOLERANCE)
        || (rotation[8] - 1.0).abs() > ROOT_ROTATION_TOLERANCE
    {
        return Err("KCD2 horizontal root rotation contains non-horizontal components".into());
    }
    Ok(())
}

fn read_f32_values(bytes: &[u8], byte_offset: usize, count: usize) -> Result<Vec<f64>, String> {
    let byte_len = count
        .checked_mul(4)
        .ok_or_else(|| "KCD2 float read length overflow".to_string())?;
    let raw = bytes
        .get(byte_offset..byte_offset + byte_len)
        .ok_or_else(|| "KCD2 matrix range exceeds the captured CBV".to_string())?;
    raw.chunks_exact(4)
        .enumerate()
        .map(|(index, chunk)| {
            let value = f32::from_le_bytes(chunk.try_into().expect("four-byte chunk")) as f64;
            value
                .is_finite()
                .then_some(value)
                .ok_or_else(|| format!("KCD2 matrix float {index} is not finite"))
        })
        .collect()
}

fn solve_three_by_three(mut matrix: [[f64; 3]; 3], mut rhs: [f64; 3]) -> Result<[f64; 3], String> {
    for column in 0..3 {
        let pivot = (column..3)
            .max_by(|left, right| {
                matrix[*left][column]
                    .abs()
                    .total_cmp(&matrix[*right][column].abs())
            })
            .expect("non-empty pivot range");
        if matrix[pivot][column].abs() < 1.0e-12 {
            return Err("KCD2 translated view-projection solve is singular".into());
        }
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        let scale = matrix[column][column];
        for value in &mut matrix[column][column..] {
            *value /= scale;
        }
        rhs[column] /= scale;
        let pivot_row = matrix[column];
        for row in 0..3 {
            if row == column {
                continue;
            }
            let scale = matrix[row][column];
            for (current, pivot) in matrix[row][column..].iter_mut().zip(&pivot_row[column..]) {
                *current -= scale * pivot;
            }
            rhs[row] -= scale * rhs[column];
        }
    }
    rhs.iter()
        .all(|value| value.is_finite())
        .then_some(rhs)
        .ok_or_else(|| "KCD2 camera position solve produced non-finite values".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_f32(bytes: &mut [u8], float_index: usize, value: f32) {
        bytes[float_index * 4..float_index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn view_projection_bytes(position: [f32; 3]) -> Vec<u8> {
        let mut bytes = vec![0u8; SNAPSHOT_BYTES as usize];
        let translation_free = [
            1.5,
            0.0,
            0.0,
            0.0,
            0.0,
            -2.666_666_7,
            0.0,
            0.0,
            0.0,
            0.0,
            4.0,
            5.0,
            0.0,
            0.0,
            1.0,
            0.0,
        ];
        let mut translated = translation_free;
        translated[3] = -1.5 * position[0];
        translated[7] = 2.666_666_7 * position[1];
        translated[11] = 5.0 - 4.0 * position[2];
        translated[15] = -position[2];
        for (index, value) in translation_free.into_iter().enumerate() {
            write_f32(&mut bytes, index, value);
        }
        for (index, value) in translated.into_iter().enumerate() {
            write_f32(&mut bytes, 20 + index, value);
        }
        bytes
    }

    fn player_root_bytes(position: [f32; 3]) -> Vec<u8> {
        let mut bytes = vec![0u8; SNAPSHOT_BYTES as usize];
        let matrix = [
            1.0,
            0.0,
            0.0,
            position[0],
            0.0,
            1.0,
            0.0,
            position[1],
            0.0,
            0.0,
            1.0,
            position[2],
        ];
        for (index, value) in matrix.into_iter().enumerate() {
            write_f32(&mut bytes, index, value);
        }
        bytes
    }

    fn observe(
        extractor: &mut Kcd2LivePoseExtractor,
        present: u64,
        binding: &Kcd2LivePoseBinding,
        bytes: &[u8],
    ) -> Result<bool, String> {
        extractor.observe_snapshot(
            present,
            &binding.root_signature_sha256,
            binding.root_parameter_index,
            binding.pass_ordinal,
            binding.draw_ordinal,
            binding.read_offset_bytes,
            i64::try_from(present * 10 + u64::from(binding.root_parameter_index) + 1).unwrap(),
            bytes,
        )
    }

    fn assert_pair(extractor: &mut Kcd2LivePoseExtractor, present: u64) -> Kcd2LivePosePair {
        match extractor
            .finish_present(present, present as i64 * 100)
            .unwrap()
        {
            Some(Kcd2LivePoseOutcome::Pair(pair)) => *pair,
            other => panic!("expected same-Present pair, got {other:?}"),
        }
    }

    #[test]
    fn v3_is_explicit_and_pins_only_one_additional_source_without_changing_legacy_wire() {
        let legacy = Kcd2LivePosePlan::pinned();
        assert_eq!(legacy.schema, KCD2_LIVE_POSE_PLAN_SCHEMA_V2);
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        assert!(!legacy_json.contains("pass_ordinal_policy"));
        assert!(!legacy_json.contains("additional_view_projection_bindings"));
        assert_eq!(
            serde_json::from_str::<Kcd2LivePosePlan>(&legacy_json).unwrap(),
            legacy
        );
        let plan = Kcd2LivePosePlan::pinned_v3();
        assert!(plan.validate(KCD2_EXECUTABLE_SHA256).is_ok());
        assert_eq!(plan.additional_view_projection_bindings.len(), 1);
        assert!(
            serde_json::to_string(&plan)
                .unwrap()
                .contains("\"pass_ordinal_policy\":\"observed\"")
        );
        assert!(
            legacy
                .binding_for_draw(KCD2_VIEW_PROJECTION_ALIAS_ROOT_SIGNATURE_SHA256, 0, 0)
                .is_none()
        );
        assert!(!legacy.accepts_player_root_source(15, 0));

        let mut extra = plan.clone();
        extra
            .additional_view_projection_bindings
            .push(plan.view_projection_binding.clone());
        assert!(extra.validate(KCD2_EXECUTABLE_SHA256).is_err());
        let mut wrong_policy = plan.clone();
        wrong_policy.view_projection_binding.pass_ordinal_policy = Kcd2PassOrdinalPolicy::Exact;
        assert!(wrong_policy.validate(KCD2_EXECUTABLE_SHA256).is_err());
        let mut legacy_with_alias = legacy;
        legacy_with_alias.additional_view_projection_bindings =
            plan.additional_view_projection_bindings;
        assert!(legacy_with_alias.validate(KCD2_EXECUTABLE_SHA256).is_err());
    }

    #[test]
    fn v3_alias_without_primary_joins_at_observed_pass_and_keeps_actual_sources() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        let mut alias = plan.additional_view_projection_bindings[0].clone();
        alias.pass_ordinal = 32;
        let mut root = plan.horizontal_player_root_binding.clone();
        root.pass_ordinal = 15;
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        observe(
            &mut extractor,
            1,
            &root,
            &player_root_bytes([9.9, 20.0, 1.5]),
        )
        .unwrap();
        observe(
            &mut extractor,
            1,
            &alias,
            &view_projection_bytes([10.0, 20.0, 3.0]),
        )
        .unwrap();
        let pair = assert_pair(&mut extractor, 1);
        assert_eq!(
            pair.view_projection_source.as_ref().unwrap(),
            &Kcd2LivePoseSourceIdentity {
                root_signature_sha256: KCD2_VIEW_PROJECTION_ALIAS_ROOT_SIGNATURE_SHA256.into(),
                root_parameter_index: 1,
                pass_ordinal: 32,
                draw_ordinal: 0,
            }
        );
        assert_eq!(
            pair.horizontal_player_root_source
                .as_ref()
                .unwrap()
                .pass_ordinal,
            15
        );
        assert_eq!(pair.horizontal_player_root_pass_ordinal, Some(15));
        let encoded = serde_json::to_string(&pair).unwrap();
        let decoded = serde_json::from_str::<Kcd2LivePosePair>(&encoded).unwrap();
        assert_eq!(decoded.view_projection_source, pair.view_projection_source);
        assert_eq!(
            decoded.horizontal_player_root_source,
            pair.horizontal_player_root_source
        );
        assert_eq!(
            (
                decoded.present_sequence,
                decoded.view_projection_sample_qpc,
                decoded.player_root_sample_qpc,
                decoded.present_boundary_qpc,
                decoded.horizontal_player_root_pass_ordinal,
                decoded.horizontal_player_root_draw_ordinal,
            ),
            (
                pair.present_sequence,
                pair.view_projection_sample_qpc,
                pair.player_root_sample_qpc,
                pair.present_boundary_qpc,
                pair.horizontal_player_root_pass_ordinal,
                pair.horizontal_player_root_draw_ordinal,
            )
        );
        // JSON parsing can round f64 coordinates by one ULP without changing
        // source identity. This test seals provenance, not decimal bit identity.
        for (actual, expected) in decoded
            .camera_position_engine_units
            .into_iter()
            .chain(decoded.horizontal_player_root_position_engine_units)
            .zip(
                pair.camera_position_engine_units
                    .into_iter()
                    .chain(pair.horizontal_player_root_position_engine_units),
            )
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
    }

    #[test]
    fn v3_identical_primary_and_alias_deduplicate_in_either_order() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        for alias_first in [false, true] {
            let primary = &plan.view_projection_binding;
            let alias = &plan.additional_view_projection_bindings[0];
            let sources = if alias_first {
                [alias, primary]
            } else {
                [primary, alias]
            };
            let mut extractor =
                Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
            for source in sources {
                observe(
                    &mut extractor,
                    2,
                    source,
                    &view_projection_bytes([10.0, 20.0, 3.0]),
                )
                .unwrap();
            }
            observe(
                &mut extractor,
                2,
                &plan.horizontal_player_root_binding,
                &player_root_bytes([9.9, 20.0, 1.5]),
            )
            .unwrap();
            let pair = assert_pair(&mut extractor, 2);
            assert_eq!(
                pair.view_projection_source.unwrap().root_signature_sha256,
                sources[0].root_signature_sha256
            );
            assert_eq!(
                pair.view_projection_sample_qpc,
                21 + i64::from(sources[0].root_parameter_index)
            );
        }
    }

    #[test]
    fn v3_full_snapshot_conflict_remains_rejected_even_if_caller_ignores_error() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        let primary = &plan.view_projection_binding;
        let alias = &plan.additional_view_projection_bindings[0];
        let root = &plan.horizontal_player_root_binding;
        let bytes = view_projection_bytes([10.0, 20.0, 3.0]);
        for conflict in [
            view_projection_bytes([11.0, 20.0, 3.0]),
            {
                let mut tail = bytes.clone();
                tail[255] = 1;
                tail
            },
            {
                let mut invalid = bytes.clone();
                write_f32(&mut invalid, 20, 99.0);
                invalid
            },
        ] {
            let mut extractor =
                Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
            observe(
                &mut extractor,
                3,
                root,
                &player_root_bytes([9.9, 20.0, 1.5]),
            )
            .unwrap();
            observe(&mut extractor, 3, primary, &bytes).unwrap();
            assert!(observe(&mut extractor, 3, alias, &conflict).is_err());
            assert!(observe(&mut extractor, 3, alias, &bytes).is_err());
            assert!(matches!(
                extractor.finish_present(3, 300).unwrap(),
                Some(Kcd2LivePoseOutcome::Discarded(_))
            ));
        }
    }

    #[test]
    fn v3_observed_pass_does_not_relax_hash_root_draw_size_or_same_present() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        let mut alias = plan.additional_view_projection_bindings[0].clone();
        let mut root = plan.horizontal_player_root_binding.clone();
        let mut extractor =
            Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
        for (present, pass) in [(4, 0), (5, 15), (6, 32), (7, 77)] {
            alias.pass_ordinal = pass;
            root.pass_ordinal = pass + 1;
            observe(
                &mut extractor,
                present,
                &alias,
                &view_projection_bytes([10.0, 20.0, 3.0]),
            )
            .unwrap();
            observe(
                &mut extractor,
                present,
                &root,
                &player_root_bytes([9.9, 20.0, 1.5]),
            )
            .unwrap();
            assert_eq!(
                assert_pair(&mut extractor, present)
                    .view_projection_source
                    .unwrap()
                    .pass_ordinal,
                pass
            );
        }
        let mut bad_hash = alias.clone();
        bad_hash.root_signature_sha256 = "0".repeat(64);
        assert!(
            !observe(
                &mut extractor,
                8,
                &bad_hash,
                &view_projection_bytes([10.0, 20.0, 3.0])
            )
            .unwrap()
        );
        let mut bad_root = alias.clone();
        bad_root.root_parameter_index = 2;
        assert!(
            !observe(
                &mut extractor,
                8,
                &bad_root,
                &view_projection_bytes([10.0, 20.0, 3.0])
            )
            .unwrap()
        );
        let mut bad_draw = alias.clone();
        bad_draw.draw_ordinal = 1;
        assert!(
            !observe(
                &mut extractor,
                8,
                &bad_draw,
                &view_projection_bytes([10.0, 20.0, 3.0])
            )
            .unwrap()
        );
        assert!(!observe(&mut extractor, 8, &alias, &[0; 255]).unwrap());
        let mut bad_offset = alias.clone();
        bad_offset.read_offset_bytes = 16;
        assert!(
            !observe(
                &mut extractor,
                8,
                &bad_offset,
                &view_projection_bytes([10.0, 20.0, 3.0])
            )
            .unwrap()
        );
        let mut bad_player = root.clone();
        bad_player.draw_ordinal = 16;
        assert!(
            !observe(
                &mut extractor,
                8,
                &bad_player,
                &player_root_bytes([9.9, 20.0, 1.5])
            )
            .unwrap()
        );

        observe(
            &mut extractor,
            8,
            &alias,
            &view_projection_bytes([10.0, 20.0, 3.0]),
        )
        .unwrap();
        assert!(matches!(
            extractor.finish_present(8, 800).unwrap(),
            Some(Kcd2LivePoseOutcome::Discarded(_))
        ));
        observe(
            &mut extractor,
            9,
            &root,
            &player_root_bytes([9.9, 20.0, 1.5]),
        )
        .unwrap();
        let Some(Kcd2LivePoseOutcome::Discarded(discard)) =
            extractor.finish_present(9, 900).unwrap()
        else {
            panic!("previous VP must not fill next Present")
        };
        assert_eq!(discard.missing, ["view_projection"]);
    }

    #[test]
    fn source_matching_normalizes_digest_notation_consistently() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        let alias = &plan.additional_view_projection_bindings[0];
        let prefixed = format!(
            "sha256:{}",
            alias.root_signature_sha256.to_ascii_uppercase()
        );
        assert_eq!(
            plan.binding_for_draw(&prefixed, 32, 0).unwrap().0,
            Kcd2LivePoseSourceKind::ViewProjection
        );
        assert_eq!(
            plan.classify_source(&prefixed, 1, 32, 0, 0, 256),
            Some(Kcd2LivePoseSourceKind::ViewProjection)
        );
        assert_eq!(plan.classify_source(&prefixed, 2, 32, 0, 0, 256), None);
        assert_eq!(
            plan.binding_for_draw(&format!("sha256:{}", "0".repeat(64)), 32, 0),
            None
        );
    }

    #[test]
    fn v3_near_duplicate_player_rotations_conflict_but_legacy_behavior_is_unchanged() {
        let mut rotated = player_root_bytes([9.9, 20.0, 1.5]);
        for (index, value) in [(0, 0.0), (1, -1.0), (4, 1.0), (5, 0.0)] {
            write_f32(&mut rotated, index, value);
        }
        for plan in [Kcd2LivePosePlan::pinned(), Kcd2LivePosePlan::pinned_v3()] {
            let v3 = plan.schema == KCD2_LIVE_POSE_PLAN_SCHEMA_V3;
            let mut extractor =
                Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
            observe(
                &mut extractor,
                10,
                &plan.view_projection_binding,
                &view_projection_bytes([10.0, 20.0, 3.0]),
            )
            .unwrap();
            observe(
                &mut extractor,
                10,
                &plan.horizontal_player_root_binding,
                &player_root_bytes([9.9, 20.0, 1.5]),
            )
            .unwrap();
            let mut next_root = plan.horizontal_player_root_binding.clone();
            next_root.draw_ordinal = 1;
            observe(&mut extractor, 10, &next_root, &rotated).unwrap();
            let outcome = extractor.finish_present(10, 1000).unwrap();
            if v3 {
                let Some(Kcd2LivePoseOutcome::Discarded(discard)) = outcome else {
                    panic!("v3 rotation conflict must refuse")
                };
                assert_eq!(
                    discard.missing,
                    ["horizontal_player_root_rotation_conflict"]
                );
            } else {
                let Some(Kcd2LivePoseOutcome::Pair(pair)) = outcome else {
                    panic!("legacy behavior changed")
                };
                assert!(pair.view_projection_source.is_none());
                assert!(pair.horizontal_player_root_source.is_none());
            }
        }
    }

    #[test]
    fn v3_view_and_player_observation_budgets_remain_hard_limits() {
        let plan = Kcd2LivePosePlan::pinned_v3();
        for view in [false, true] {
            let mut extractor =
                Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
            let source = if view {
                &plan.view_projection_binding
            } else {
                &plan.horizontal_player_root_binding
            };
            let bytes = if view {
                view_projection_bytes([10.0, 20.0, 3.0])
            } else {
                player_root_bytes([9.9, 20.0, 1.5])
            };
            for _ in 0..16 {
                assert!(observe(&mut extractor, 11, source, &bytes).unwrap());
            }
            assert!(observe(&mut extractor, 11, source, &bytes).is_err());
            assert!(matches!(
                extractor.finish_present(11, 1100).unwrap(),
                Some(Kcd2LivePoseOutcome::Discarded(_))
            ));
        }
    }

    #[test]
    fn plan_rejects_any_build_or_binding_drift() {
        let plan = Kcd2LivePosePlan::pinned();
        assert!(plan.validate(KCD2_EXECUTABLE_SHA256).is_ok());
        assert!(
            Kcd2LivePosePlan::legacy_pinned_v1()
                .validate(KCD2_EXECUTABLE_SHA256)
                .is_ok()
        );

        let mut changed = plan.clone();
        changed.view_projection_binding.draw_ordinal += 1;
        assert!(changed.validate(KCD2_EXECUTABLE_SHA256).is_err());

        let mut changed = plan.clone();
        changed
            .horizontal_player_root_binding
            .additional_pass_ordinals
            .push(6);
        assert!(changed.validate(KCD2_EXECUTABLE_SHA256).is_err());

        let mut changed = plan;
        changed.require_same_present = false;
        assert!(changed.validate(KCD2_EXECUTABLE_SHA256).is_err());
        assert!(
            Kcd2LivePosePlan::pinned()
                .validate(&format!("sha256:{}", "0".repeat(64)))
                .is_err()
        );
    }

    #[test]
    fn same_present_pair_is_decoded_without_metric_claims() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let root = plan.horizontal_player_root_binding.clone();
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        assert!(
            observe(
                &mut extractor,
                7,
                &view,
                &view_projection_bytes([9.0, 4.0, 2.0])
            )
            .unwrap()
        );
        assert!(
            observe(
                &mut extractor,
                7,
                &root,
                &player_root_bytes([8.8, 4.0, 1.0])
            )
            .unwrap()
        );

        let Some(Kcd2LivePoseOutcome::Pair(pair)) = extractor.finish_present(7, 700).unwrap()
        else {
            panic!("expected a same-Present pair");
        };
        assert_eq!(pair.camera_position_engine_units, [9.0, 4.0, 2.0]);
        assert_eq!(
            pair.horizontal_player_root_position_engine_units,
            [8.800000190734863, 4.0, 1.0]
        );
        assert!((pair.camera_minus_root_engine_units[0] - 0.2).abs() < 1.0e-6);
        assert_eq!(
            [
                pair.camera_minus_root_engine_units[1],
                pair.camera_minus_root_engine_units[2]
            ],
            [0.0, 1.0]
        );
        assert!(pair.held_out_depth_row_residual.abs() < 1.0e-12);
        assert!((pair.aspect_ratio - 1920.0 / 1080.0).abs() < 1.0e-6);
        assert!((pair.camera_intrinsics_1920x1080.fx - 1440.0).abs() < 1.0e-6);
        assert!((pair.camera_intrinsics_1920x1080.fy - 1440.0).abs() < 1.0e-4);
        assert_eq!(pair.present_boundary_qpc, 700);
    }

    #[test]
    fn missing_side_is_discarded_and_never_reused() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let root = plan.horizontal_player_root_binding.clone();
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        observe(
            &mut extractor,
            10,
            &view,
            &view_projection_bytes([1.0, 2.0, 3.0]),
        )
        .unwrap();
        let Some(Kcd2LivePoseOutcome::Discarded(discarded)) =
            extractor.finish_present(10, 1_000).unwrap()
        else {
            panic!("expected an explicit discard");
        };
        assert_eq!(discarded.missing, vec!["horizontal_player_root"]);

        observe(
            &mut extractor,
            11,
            &root,
            &player_root_bytes([1.0, 2.0, 3.0]),
        )
        .unwrap();
        let Some(Kcd2LivePoseOutcome::Discarded(discarded)) =
            extractor.finish_present(11, 1_100).unwrap()
        else {
            panic!("expected an explicit discard");
        };
        assert_eq!(discarded.missing, vec!["view_projection"]);
    }

    #[test]
    fn duplicate_and_invalid_layout_fail_closed() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        let bytes = view_projection_bytes([1.0, 2.0, 3.0]);
        observe(&mut extractor, 1, &view, &bytes).unwrap();
        assert!(observe(&mut extractor, 1, &view, &bytes).is_err());

        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        let mut invalid = view_projection_bytes([1.0, 2.0, 3.0]);
        write_f32(&mut invalid, 20, 99.0);
        assert!(observe(&mut extractor, 2, &view, &invalid).is_err());
    }

    #[test]
    fn stop_discards_unbounded_intervals_even_when_both_sources_arrived() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let root = plan.horizontal_player_root_binding.clone();
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        observe(
            &mut extractor,
            20,
            &view,
            &view_projection_bytes([4.0, 5.0, 6.0]),
        )
        .unwrap();
        observe(
            &mut extractor,
            20,
            &root,
            &player_root_bytes([4.0, 5.0, 4.5]),
        )
        .unwrap();
        observe(
            &mut extractor,
            21,
            &view,
            &view_projection_bytes([7.0, 8.0, 9.0]),
        )
        .unwrap();

        let outcomes = extractor.finish_pending();
        assert_eq!(outcomes.len(), 2);
        let Kcd2LivePoseOutcome::Discarded(complete_without_boundary) = &outcomes[0] else {
            panic!("a final interval without a Present boundary must be discarded");
        };
        assert_eq!(complete_without_boundary.missing, vec!["present_boundary"]);
        let Kcd2LivePoseOutcome::Discarded(discarded) = &outcomes[1] else {
            panic!("expected the incomplete final interval to be discarded");
        };
        assert_eq!(
            discarded.missing,
            vec!["horizontal_player_root", "present_boundary"]
        );
    }

    #[test]
    fn same_present_player_root_selector_rejects_a_far_draw_and_keeps_the_near_draw() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let root_draw_zero = plan.horizontal_player_root_binding.clone();
        let mut root_draw_one = root_draw_zero.clone();
        root_draw_one.draw_ordinal = 1;
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        observe(
            &mut extractor,
            40,
            &view,
            &view_projection_bytes([10.0, 20.0, 3.0]),
        )
        .unwrap();
        observe(
            &mut extractor,
            40,
            &root_draw_zero,
            &player_root_bytes([30.0, 20.0, 1.5]),
        )
        .unwrap();
        observe(
            &mut extractor,
            40,
            &root_draw_one,
            &player_root_bytes([9.9, 20.0, 1.5]),
        )
        .unwrap();

        let Some(Kcd2LivePoseOutcome::Pair(pair)) = extractor.finish_present(40, 4_000).unwrap()
        else {
            panic!("expected the unique near-camera root to be selected");
        };
        let selected = pair.horizontal_player_root_position_engine_units;
        assert!((selected[0] - 9.9).abs() < 1.0e-5);
        assert_eq!([selected[1], selected[2]], [20.0, 1.5]);
    }

    #[test]
    fn same_present_player_root_accepts_only_the_pinned_additional_passes() {
        for pass_ordinal in PLAYER_ROOT_ADDITIONAL_PASSES {
            let plan = Kcd2LivePosePlan::pinned();
            let view = plan.view_projection_binding.clone();
            let mut root = plan.horizontal_player_root_binding.clone();
            root.pass_ordinal = pass_ordinal;
            let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
            observe(
                &mut extractor,
                50 + u64::from(pass_ordinal),
                &view,
                &view_projection_bytes([10.0, 20.0, 3.0]),
            )
            .unwrap();
            assert!(
                observe(
                    &mut extractor,
                    50 + u64::from(pass_ordinal),
                    &root,
                    &player_root_bytes([9.9, 20.0, 1.5]),
                )
                .unwrap()
            );
            assert!(matches!(
                extractor
                    .finish_present(
                        50 + u64::from(pass_ordinal),
                        10_000 + i64::from(pass_ordinal)
                    )
                    .unwrap(),
                Some(Kcd2LivePoseOutcome::Pair(_))
            ));
        }

        let plan = Kcd2LivePosePlan::pinned();
        let mut unpinned = plan.horizontal_player_root_binding.clone();
        unpinned.pass_ordinal = 6;
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        assert!(
            !observe(
                &mut extractor,
                99,
                &unpinned,
                &player_root_bytes([9.9, 20.0, 1.5]),
            )
            .unwrap()
        );
    }

    #[test]
    fn bounded_multi_draw_search_can_select_the_sixteenth_root_candidate() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let mut extractor =
            Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
        observe(
            &mut extractor,
            120,
            &view,
            &view_projection_bytes([10.0, 20.0, 3.0]),
        )
        .unwrap();
        for draw_ordinal in 0..MAX_PLAYER_ROOT_CANDIDATES as u32 {
            let mut root = plan.horizontal_player_root_binding.clone();
            root.pass_ordinal = 7;
            root.draw_ordinal = draw_ordinal;
            let position = if draw_ordinal == PLAYER_ROOT_MAX_DRAW {
                [9.9, 20.0, 1.5]
            } else {
                [30.0 + draw_ordinal as f32, 20.0, 1.5]
            };
            assert!(observe(&mut extractor, 120, &root, &player_root_bytes(position),).unwrap());
        }
        let Some(Kcd2LivePoseOutcome::Pair(pair)) = extractor.finish_present(120, 12_000).unwrap()
        else {
            panic!("expected the bounded draw-15 player root to be selected");
        };
        assert!((pair.horizontal_player_root_position_engine_units[0] - 9.9).abs() < 1.0e-5);
    }

    #[test]
    fn multiple_distinct_near_camera_roots_remain_ambiguous() {
        let plan = Kcd2LivePosePlan::pinned();
        let view = plan.view_projection_binding.clone();
        let mut root = plan.horizontal_player_root_binding.clone();
        root.pass_ordinal = 7;
        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        observe(
            &mut extractor,
            130,
            &view,
            &view_projection_bytes([10.0, 20.0, 3.0]),
        )
        .unwrap();
        observe(
            &mut extractor,
            130,
            &root,
            &player_root_bytes([9.9, 20.0, 1.5]),
        )
        .unwrap();
        root.draw_ordinal = 1;
        observe(
            &mut extractor,
            130,
            &root,
            &player_root_bytes([10.2, 20.0, 1.5]),
        )
        .unwrap();
        let Some(Kcd2LivePoseOutcome::Discarded(discarded)) =
            extractor.finish_present(130, 13_000).unwrap()
        else {
            panic!("two distinct near-camera roots must fail closed");
        };
        assert_eq!(discarded.missing, vec!["horizontal_player_root_ambiguous"]);
    }

    #[test]
    fn malformed_or_non_horizontal_root_rotation_fails_closed() {
        let plan = Kcd2LivePosePlan::pinned();
        let root = plan.horizontal_player_root_binding.clone();
        let mut extractor =
            Kcd2LivePoseExtractor::new(plan.clone(), KCD2_EXECUTABLE_SHA256).unwrap();
        let mut scaled = player_root_bytes([1.0, 2.0, 3.0]);
        write_f32(&mut scaled, 0, 2.0);
        assert!(observe(&mut extractor, 30, &root, &scaled).is_err());

        let mut extractor = Kcd2LivePoseExtractor::new(plan, KCD2_EXECUTABLE_SHA256).unwrap();
        let mut pitched = player_root_bytes([1.0, 2.0, 3.0]);
        write_f32(&mut pitched, 6, 0.1);
        assert!(observe(&mut extractor, 31, &root, &pitched).is_err());
    }

    #[test]
    fn mainline_depth_candidate_preserves_facts_and_current_admission_blockers() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/depth-profiles/kcd2-d3d12-mainline-candidate.json");
        let bytes =
            std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let candidate: Kcd2MainlineDepthCandidate = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        candidate.validate().unwrap();
        assert_eq!(candidate.status, Kcd2MainlineDepthCandidate::STATUS);
        assert_eq!(candidate.blockers.len(), 3);
    }
}
