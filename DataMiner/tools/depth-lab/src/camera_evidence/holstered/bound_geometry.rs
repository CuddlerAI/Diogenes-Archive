//! Selected-Present geometry is eligible only for the exact full binding and original launch.
//! An aggregate across two incomplete scenes cannot supply either scene's missing population.
use super::*;
use std::collections::BTreeSet;

pub(super) const SCHEMA: &str = "dm-cp2077-bound-selected-geometry/v1";

#[derive(Deserialize)]
struct Report {
    schema: String,
    non_trainable: bool,
    local_only: bool,
    upload_allowed: bool,
    acceptance: bool,
    production_binding_authorized: bool,
    binding: D3d12CameraBinding,
    binding_sha256: String,
    native_replay_sha256: String,
    metric_evidence_sha256: String,
    sessions: Vec<Session>,
    geometry: OriginalGeometry,
}

#[derive(Deserialize)]
struct OriginalGeometry {
    all_original_sha_unchanged: bool,
    geometry_checks_passed: bool,
    failures: Vec<String>,
}

#[derive(Deserialize)]
struct Session {
    process_launch_id: String,
    raw_depth_profile_sha256: String,
    inventory_sha256: String,
    camera_report_sha256: String,
    native_pose_sha256: String,
    selected_depth_frames: u64,
    bound_pose_coverage: u64,
    required_rgb_frames: u64,
    exact_rgb_pairs: u64,
    gate: Gate,
}

#[derive(Deserialize)]
struct Gate {
    geometry_checks_passed: bool,
    failures: Vec<String>,
    usable_motion_pairs: u64,
    invalid_metric_pairs: u64,
    sampled_pairs: Populations,
    metrics: Metrics,
}

#[derive(Deserialize)]
struct Populations {
    #[serde(rename = "static")]
    stationary: u64,
    translation: u64,
    rotation: u64,
}

#[derive(Deserialize)]
struct Metrics {
    improvement_p50: Option<f64>,
    inverse_improvement_p50: Option<f64>,
    valid_frac_p10: Option<f64>,
    improved_share: Option<f64>,
    moving_baseline_l1_p50: Option<f64>,
    static_baseline_l1_p50: Option<f64>,
}

pub(super) fn validate(launch: &LaunchEvidence, bytes: &[u8]) -> Result<(), String> {
    let r: Report =
        serde_json::from_slice(bytes).map_err(|e| format!("bound_geometry_report_invalid: {e}"))?;
    if r.schema != SCHEMA
        || !r.non_trainable
        || !r.local_only
        || r.upload_allowed
        || r.acceptance
        || r.production_binding_authorized
        || r.binding != launch.binding
        || !is_sha256_digest(&r.binding_sha256)
        || !is_sha256_digest(&r.native_replay_sha256)
        || !is_sha256_digest(&r.metric_evidence_sha256)
        || launch.metric_evidence_sha256.as_ref() != Some(&r.metric_evidence_sha256)
        || !r.geometry.all_original_sha_unchanged
        || !r.geometry.geometry_checks_passed
        || !r.geometry.failures.is_empty()
        || r.sessions.is_empty()
        || r.sessions.len() > 3
    {
        return Err("bound_geometry_binding_metric_or_integrity_mismatch".into());
    }
    let mut launches = BTreeSet::new();
    for s in &r.sessions {
        if !valid_process_launch_id(&s.process_launch_id)
            || !launches.insert(&s.process_launch_id)
            || [
                &s.raw_depth_profile_sha256,
                &s.inventory_sha256,
                &s.camera_report_sha256,
                &s.native_pose_sha256,
            ]
            .iter()
            .any(|h| !is_sha256_digest(h))
            || s.selected_depth_frames == 0
            || s.selected_depth_frames > 4096
            || s.bound_pose_coverage != s.selected_depth_frames
            || s.required_rgb_frames < s.selected_depth_frames
            || s.required_rgb_frames != s.exact_rgb_pairs
        {
            return Err("bound_geometry_launch_or_exact_coverage_invalid".into());
        }
    }
    let s = r
        .sessions
        .iter()
        .find(|s| s.process_launch_id == launch.process_launch_id)
        .ok_or("bound_geometry_original_launch_missing")?;
    if s.raw_depth_profile_sha256 != launch.depth_profile_sha256 {
        return Err("bound_geometry_original_profile_mismatch".into());
    }
    let g = &s.gate;
    let m = &g.metrics;
    let numbers = [
        m.improvement_p50,
        m.inverse_improvement_p50,
        m.valid_frac_p10,
        m.improved_share,
        m.moving_baseline_l1_p50,
        m.static_baseline_l1_p50,
    ];
    let [
        Some(improvement),
        Some(inverse),
        Some(valid),
        Some(share),
        Some(moving),
        Some(still),
    ] = numbers
    else {
        return Err("bound_geometry_this_launch_metrics_incomplete".into());
    };
    // Same fixed selected-geometry thresholds; authoring accepts no caller overrides.
    if !g.geometry_checks_passed
        || !g.failures.is_empty()
        || g.usable_motion_pairs < 30
        || g.invalid_metric_pairs != 0
        || g.sampled_pairs.stationary < 5
        || g.sampled_pairs.translation < 5
        || g.sampled_pairs.rotation < 5
        || numbers.iter().flatten().any(|v| !v.is_finite())
        || improvement < 1.05
        || inverse <= 0.
        || improvement < inverse * 1.05
        || !(0.5..=1.).contains(&valid)
        || !(0.6..=1.).contains(&share)
        || moving < 0.5
        || still < 0.
        || still >= moving
    {
        return Err("bound_geometry_this_launch_gate_failed".into());
    }
    Ok(())
}
