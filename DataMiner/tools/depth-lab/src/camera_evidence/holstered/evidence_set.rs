//! Reuse independently recorded, scoped evidence without pretending it was one launch.
//! Each obligation is checked once. Live StartRun admission and production authorization
//! are separate; this offline review never clears either of those gates.
use super::*;
use std::collections::BTreeSet;

pub const INPUT_SCHEMA: &str = "cp2077-holstered-evidence-set-input/v1";
pub const REPORT_SCHEMA: &str = "cp2077-holstered-evidence-set-review/v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    /// A prepared, explicitly scoped summary retaining its real launch/profile identity.
    pub evidence: LaunchEvidence,
    /// Original review is retained, including failures outside the reused scope.
    pub original_review_sha256: String,
    /// Differences from the original whole-session verdict must remain visible.
    pub retained_limitations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSet {
    pub schema: String,
    pub non_trainable: bool,
    pub binding: D3d12CameraBinding,
    pub motion: Contribution,
    pub fov: Contribution,
    pub geometry: Contribution,
}

#[derive(Debug, Serialize)]
pub struct ContributionReview {
    pub purpose: &'static str,
    pub depth_profile_sha256: String,
    pub source_evidence_sha256: String,
    pub original_review_sha256: String,
    pub retained_limitations: Vec<String>,
    pub checks: LaunchReview,
}

#[derive(Debug, Serialize)]
pub struct EvidenceSetReview {
    pub schema: &'static str,
    pub non_trainable: bool,
    pub upload_allowed: bool,
    pub production_binding_authorized: bool,
    pub live_client_verified: bool,
    pub ready_for_local_configuration: bool,
    pub requires_original_evidence_review: bool,
    pub independent_launch_count: usize,
    pub blockers: Vec<String>,
    pub contributions: Vec<ContributionReview>,
}

pub fn review_set(
    input: &EvidenceSet,
    geometry_bytes: Option<&[u8]>,
    sha: Sha256Hex,
) -> Result<EvidenceSetReview, String> {
    if input.schema != INPUT_SCHEMA || !input.non_trainable {
        return Err("unsupported evidence-set schema or trainability".into());
    }
    let binding_valid = input.binding.schema == crate::CAMERA_BINDING_SCHEMA_V3
        && input
            .binding
            .validate_for_executable(&input.binding.executable_sha256)
            .is_ok();
    let mut result = EvidenceSetReview {
        schema: REPORT_SCHEMA,
        non_trainable: true,
        upload_allowed: false,
        production_binding_authorized: false,
        live_client_verified: false,
        ready_for_local_configuration: false,
        requires_original_evidence_review: true,
        independent_launch_count: 0,
        blockers: Vec::new(),
        contributions: Vec::new(),
    };
    if !binding_valid {
        result.blockers.push("invalid_holstered_v3_binding".into());
    }
    let metric = &input.geometry.evidence.metric_evidence_sha256;
    if !metric.as_ref().is_some_and(|h| is_sha256_digest(h)) {
        result
            .blockers
            .push("metric_evidence_missing_or_invalid".into());
    }
    let mut launches = BTreeSet::new();
    for (purpose, c) in [
        ("motion", &input.motion),
        ("fov", &input.fov),
        ("geometry", &input.geometry),
    ] {
        let l = &c.evidence;
        launches.insert(&l.process_launch_id);
        let mut r = LaunchReview {
            role: l.role,
            process_launch_id: l.process_launch_id.clone(),
            blockers: l
                .authoring_blockers
                .iter()
                .map(|b| format!("source_blocker: {b}"))
                .collect(),
            observed_fov_degrees: Vec::new(),
            motion_in_reference_camera_metres: Vec::new(),
            return_position_error_metres: None,
            return_rotation_error_rad: None,
        };
        if !valid_process_launch_id(&l.process_launch_id)
            || [
                &l.depth_profile_sha256,
                &l.source_evidence_sha256,
                &c.original_review_sha256,
            ]
            .iter()
            .any(|h| !is_sha256_digest(h))
        {
            r.blockers
                .push("invalid_contribution_identity_or_hash".into());
        }
        // Profiles keep their actual hashes. What must stay identical is the entire camera
        // definition, including executable, resolution, all aliases, W/C and metric scale.
        if l.binding != input.binding || l.metric_evidence_sha256 != *metric {
            r.blockers
                .push("cross_evidence_binding_or_metric_changed".into());
        }
        let check = match purpose {
            "motion" => {
                if !l
                    .independent_motion_evidence_sha256
                    .as_ref()
                    .is_some_and(|h| is_sha256_digest(h))
                {
                    r.blockers
                        .push("independent_motion_evidence_missing_or_invalid".into());
                }
                // A single, real FOV75 phase is sufficient to anchor motion; do not synthesize
                // FOV90/return phases for this historical launch.
                let phase_result = fov_observations(l, &mut r.observed_fov_degrees);
                if l.fov_phases.len() != 1
                    || r.observed_fov_degrees.len() != 1
                    || phase_result.is_err()
                    || (r.observed_fov_degrees[0] - 75.).abs() > FOV_SETTING_TOLERANCE_DEGREES
                {
                    r.blockers.push("motion_fov75_phase_invalid".into());
                }
                if binding_valid {
                    motion(l, &mut r)
                } else {
                    Ok(())
                }
            }
            "fov" => fov(l, &mut r.observed_fov_degrees),
            "geometry" => {
                let schema_matches = geometry_bytes
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
                    .is_some_and(|r| r["schema"] == bound_geometry::SCHEMA);
                if !schema_matches {
                    Err("evidence_reuse_requires_bound_geometry_report".into())
                } else {
                    geometry(l, geometry_bytes, sha)
                }
            }
            _ => unreachable!(),
        };
        if let Err(error) = check {
            r.blockers.push(error);
        }
        result.contributions.push(ContributionReview {
            purpose,
            depth_profile_sha256: l.depth_profile_sha256.clone(),
            source_evidence_sha256: l.source_evidence_sha256.clone(),
            original_review_sha256: c.original_review_sha256.clone(),
            retained_limitations: c.retained_limitations.clone(),
            checks: r,
        });
    }
    result.independent_launch_count = launches.len();
    if launches.len() < 2 {
        result
            .blockers
            .push("independent_source_repeat_missing".into());
    }
    if let (Some(m), Some(f)) = (
        input.motion.evidence.fov_phases.first(),
        input.fov.evidence.fov_phases.first(),
    ) && [0, 1].iter().any(|&axis| {
        relative_difference(
            f64::from(m.projection[axis][axis]),
            f64::from(f.projection[axis][axis]),
        ) > FOV_RETURN_RELATIVE_TOLERANCE
    }) {
        result
            .blockers
            .push("cross_evidence_fov75_intrinsics_changed".into());
    }
    result.ready_for_local_configuration = result.blockers.is_empty()
        && result
            .contributions
            .iter()
            .all(|r| r.checks.blockers.is_empty());
    Ok(result)
}
