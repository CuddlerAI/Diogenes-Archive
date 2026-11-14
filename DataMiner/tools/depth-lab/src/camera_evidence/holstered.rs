//! Review prepared evidence, never discover sources, choose axes, or publish a binding.
//! Raw-source admission and independent action/metric witnesses remain review prerequisites.
use super::*;
use crate::camera::Mat4;
use crate::d3d12_camera::resolve_pose;

pub const INPUT_SCHEMA: &str = "cp2077-holstered-authoring-review-input/v1";
pub const REPORT_SCHEMA: &str = "cp2077-holstered-authoring-review/v1";
pub const CLAIMS_SCHEMA: &str = "cp2077-holstered-authoring-claims/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewInput {
    pub schema: String,
    pub non_trainable: bool,
    /// Zero/one launch is a useful incomplete review, never an implicit A/B success.
    pub launches: Vec<LaunchEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchEvidence {
    pub role: D3d12AuthoringLaunchRole,
    pub process_launch_id: String,
    pub depth_profile_sha256: String,
    /// The entire proposed v3 binding, including every alias and both W and C.
    pub binding: D3d12CameraBinding,
    /// Immutable raw-source inventory/analysis reference; a digest is not a physical proof.
    pub source_evidence_sha256: String,
    pub authoring_blockers: Vec<String>,
    /// Missing independent witnesses remain missing. Planned timings are not witnesses.
    pub independent_motion_evidence_sha256: Option<String>,
    pub metric_evidence_sha256: Option<String>,
    pub fov_phases: Vec<FovPhase>,
    pub axis_motion: Vec<AxisMotion>,
    pub route: Option<Route>,
    pub geometry: Option<GeometryAttestation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FovPhase {
    pub configured_horizontal_fov_degrees: f64,
    pub first_present: u64,
    pub last_present: u64,
    pub sample_count: u64,
    /// Original decoded representative matrix from the cited phase, never fitted intrinsics.
    pub projection: Mat4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoseSample {
    pub present: u64,
    pub view: Mat4,
    pub projection: Mat4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisMotion {
    pub motion: D3d12ControlledMotion,
    pub start: PoseSample,
    pub end: PoseSample,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Fixed camera reference for all three motions and the return check.
    pub start: PoseSample,
    pub end: PoseSample,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeometryAttestation {
    pub report_sha256: String,
    /// SHA of compact serde JSON in Claims field order, UTF-8, no trailing LF.
    pub claims_sha256: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    schema: &'static str,
    launch: &'a LaunchEvidence,
    report_sha256: &'a str,
}

/// Binds all evidence fields, not just the first alias. Geometry is omitted to avoid a hash cycle.
pub fn geometry_claims_sha256(
    launch: &LaunchEvidence,
    report_sha256: &str,
    sha: Sha256Hex,
) -> Result<String, String> {
    let mut claim = launch.clone();
    claim.geometry = None;
    let bytes = serde_json::to_vec(&Claims {
        schema: CLAIMS_SCHEMA,
        launch: &claim,
        report_sha256,
    })
    .map_err(|e| e.to_string())?;
    let digest = sha(&bytes);
    if !is_sha256_digest(&digest) || !is_sha256_digest(report_sha256) {
        return Err("invalid geometry claims/report SHA-256".into());
    }
    Ok(digest)
}

#[derive(Debug, Serialize)]
pub struct Review {
    pub schema: &'static str,
    pub non_trainable: bool,
    pub upload_allowed: bool,
    pub production_binding_authorized: bool,
    pub camera_binding_emitted: bool,
    pub ready_for_human_review: bool,
    /// Human review must inspect the cited raw source, action and metric evidence.
    pub requires_original_evidence_review: bool,
    pub blockers: Vec<String>,
    pub launches: Vec<LaunchReview>,
}

#[derive(Debug, Serialize)]
pub struct LaunchReview {
    pub role: D3d12AuthoringLaunchRole,
    pub process_launch_id: String,
    pub blockers: Vec<String>,
    pub observed_fov_degrees: Vec<f64>,
    pub motion_in_reference_camera_metres: Vec<[f64; 3]>,
    pub return_position_error_metres: Option<f64>,
    pub return_rotation_error_rad: Option<f64>,
}

/// Reviews supplied summaries. It neither fabricates absent phases nor blesses an input hash
/// as proof that a human performed an action. Even a complete pair only reaches human review.
pub fn review(
    input: &ReviewInput,
    geometry_reports: [Option<&[u8]>; 2],
    sha: Sha256Hex,
) -> Result<Review, String> {
    if input.schema != INPUT_SCHEMA || !input.non_trainable || input.launches.len() > 2 {
        return Err("unsupported holstered review input or non-trainable/count constraint".into());
    }
    let mut result = Review {
        schema: REPORT_SCHEMA,
        non_trainable: true,
        upload_allowed: false,
        production_binding_authorized: false,
        camera_binding_emitted: false,
        ready_for_human_review: false,
        requires_original_evidence_review: true,
        blockers: Vec::new(),
        launches: Vec::new(),
    };
    let a = input
        .launches
        .iter()
        .find(|l| l.role == D3d12AuthoringLaunchRole::A);
    let b = input
        .launches
        .iter()
        .find(|l| l.role == D3d12AuthoringLaunchRole::B);
    if a.is_none() || b.is_none() {
        result
            .blockers
            .push("independent_launch_pair_missing".into());
    }
    for launch in &input.launches {
        let index = usize::from(launch.role == D3d12AuthoringLaunchRole::B);
        result
            .launches
            .push(review_launch(launch, geometry_reports[index], sha));
    }
    if let (Some(a), Some(b)) = (a, b) {
        if a.process_launch_id == b.process_launch_id {
            result
                .blockers
                .push("independent_process_launch_required".into());
        }
        if a.depth_profile_sha256 != b.depth_profile_sha256 || a.binding != b.binding {
            result
                .blockers
                .push("cross_launch_binding_or_profile_changed".into());
        }
        let ar = result
            .launches
            .iter()
            .find(|r| r.role == D3d12AuthoringLaunchRole::A)
            .unwrap();
        let br = result
            .launches
            .iter()
            .find(|r| r.role == D3d12AuthoringLaunchRole::B)
            .unwrap();
        if ar.motion_in_reference_camera_metres.len() == 3
            && br.motion_in_reference_camera_metres.len() == 3
            && ar
                .motion_in_reference_camera_metres
                .iter()
                .zip(&br.motion_in_reference_camera_metres)
                .any(|(a, b)| cosine(*a, *b) < CROSS_LAUNCH_AXIS_MIN_COSINE)
        {
            result
                .blockers
                .push("cross_launch_local_motion_direction_changed".into());
        }
        if let (Some(a), Some(b)) = (a.fov_phases.first(), b.fov_phases.first()) {
            for axis in [0, 1] {
                if relative_difference(
                    f64::from(a.projection[axis][axis]),
                    f64::from(b.projection[axis][axis]),
                ) > FOV_RETURN_RELATIVE_TOLERANCE
                {
                    result
                        .blockers
                        .push("cross_launch_fov75_intrinsics_changed".into());
                    break;
                }
            }
        }
    }
    result.ready_for_human_review =
        result.blockers.is_empty() && result.launches.iter().all(|r| r.blockers.is_empty());
    Ok(result)
}

fn review_launch(
    l: &LaunchEvidence,
    geometry_bytes: Option<&[u8]>,
    sha: Sha256Hex,
) -> LaunchReview {
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
        || !is_sha256_digest(&l.depth_profile_sha256)
        || !is_sha256_digest(&l.source_evidence_sha256)
    {
        r.blockers
            .push("invalid_launch_or_evidence_identity".into());
    }
    for (name, hash) in [
        (
            "independent_motion_evidence",
            &l.independent_motion_evidence_sha256,
        ),
        ("metric_evidence", &l.metric_evidence_sha256),
    ] {
        if !hash.as_ref().is_some_and(|h| is_sha256_digest(h)) {
            r.blockers.push(format!("{name}_missing_or_invalid"));
        }
    }
    let binding_valid = l.binding.schema == crate::CAMERA_BINDING_SCHEMA_V3
        && l.binding
            .validate_for_executable(&l.binding.executable_sha256)
            .is_ok();
    if !binding_valid {
        r.blockers.push("invalid_holstered_v3_binding".into());
    }
    if let Err(e) = fov(l, &mut r.observed_fov_degrees) {
        r.blockers.push(e);
    }
    if binding_valid && let Err(e) = motion(l, &mut r) {
        r.blockers.push(e);
    }
    if let Err(e) = geometry(l, geometry_bytes, sha) {
        r.blockers.push(e);
    }
    r
}

fn fov(l: &LaunchEvidence, observed: &mut Vec<f64>) -> Result<(), String> {
    let phases: [_; 3] = fov_observations(l, observed)?
        .try_into()
        .map_err(|_| "fov_75_90_75_incomplete".to_string())?;
    validate_fov_75_90_75(&phases)
}

fn fov_observations(
    l: &LaunchEvidence,
    observed: &mut Vec<f64>,
) -> Result<Vec<D3d12FovPhaseObservation>, String> {
    if l.fov_phases.len() > 3 {
        return Err("too_many_fov_phases".into());
    }
    let mut phases = Vec::new();
    let mut previous = 0;
    for (i, p) in l.fov_phases.iter().enumerate() {
        if p.configured_horizontal_fov_degrees != [75., 90., 75.][i]
            || p.first_present <= previous
            || p.last_present < p.first_present
            || p.sample_count < MIN_PHASE_SAMPLES as u64
            || p.sample_count - 1 > p.last_present - p.first_present
            || p.projection.iter().flatten().any(|v| !v.is_finite())
            || crate::camera_track::projection_from_matrix(&p.projection).is_none()
            || p.projection[0][0] <= 0.
            || p.projection[1][1] <= 0.
        {
            return Err("invalid_fov_phase_order_or_physical_projection".into());
        }
        previous = p.last_present;
        let degrees = (2. * (1. / f64::from(p.projection[0][0])).atan()).to_degrees();
        observed.push(degrees);
        phases.push(D3d12FovPhaseObservation {
            configured_horizontal_fov_degrees: p.configured_horizontal_fov_degrees,
            observed_horizontal_fov_degrees: degrees,
            fx_pixels: f64::from(p.projection[0][0]) * f64::from(l.binding.render_width) / 2.,
            fy_pixels: f64::from(p.projection[1][1]) * f64::from(l.binding.render_height) / 2.,
        });
    }
    Ok(phases)
}

fn motion(l: &LaunchEvidence, r: &mut LaunchReview) -> Result<(), String> {
    let route = l.route.as_ref().ok_or("route_reference_missing")?;
    let phase = l.fov_phases.first().ok_or("motion_fov75_phase_missing")?;
    if route.start.present < phase.first_present
        || route.end.present > phase.last_present
        || route.start.present >= route.end.present
    {
        return Err("invalid_route_present_range".into());
    }
    let decode = |p: &PoseSample| {
        if [0, 1].iter().any(|&axis| {
            relative_difference(
                f64::from(p.projection[axis][axis]),
                f64::from(phase.projection[axis][axis]),
            ) > FOV_RETURN_RELATIVE_TOLERANCE
        }) {
            return Err("motion_projection_differs_from_fov75_phase".to_string());
        }
        let result = resolve_pose(&l.binding, &p.view, &p.projection).map_err(str::to_string)?;
        if result.0.iter().chain(&result.1).any(|v| !v.is_finite()) {
            return Err("non_finite_decoded_motion_pose".to_string());
        }
        Ok(result)
    };
    let (start, reference, _) = decode(&route.start)?;
    let (end, end_rotation, _) = decode(&route.end)?;
    let gap = distance(start, end);
    let angle = quaternion_angle(reference, end_rotation);
    r.return_position_error_metres = Some(gap);
    r.return_rotation_error_rad = Some(angle);
    if !gap.is_finite()
        || !angle.is_finite()
        || gap > MAX_RETURN_POSITION_ERROR_METRES
        || angle > MAX_RETURN_ROTATION_ERROR_RAD
    {
        r.blockers.push("route_did_not_return".into());
    }
    if l.axis_motion.len() != 3 {
        return Err("three_independent_motions_missing".into());
    }
    let mut previous = route.start.present;
    for (m, (expected, axis)) in l.axis_motion.iter().zip([
        (D3d12ControlledMotion::Forward, 2),
        (D3d12ControlledMotion::StrafeRight, 0),
        (D3d12ControlledMotion::VerticalUp, 1),
    ]) {
        if m.motion != expected
            || m.start.present < previous
            || m.end.present <= m.start.present
            || m.end.present > route.end.present
        {
            return Err("invalid_controlled_motion_order".into());
        }
        previous = m.end.present;
        let (a, qa, _) = decode(&m.start)?;
        let (b, qb, _) = decode(&m.end)?;
        if quaternion_angle(reference, qa) > MAX_RETURN_ROTATION_ERROR_RAD
            || quaternion_angle(reference, qb) > MAX_RETURN_ROTATION_ERROR_RAD
        {
            return Err("controlled_motion_reference_rotated".into());
        }
        let local = inverse_rotate(reference, std::array::from_fn(|i| b[i] - a[i]));
        let length = vector_magnitude(local);
        if !length.is_finite()
            || length < MIN_AXIS_MOTION_METRES
            || local[axis] / length < AXIS_DIRECTION_MIN_COSINE
        {
            return Err("controlled_motion_wrong_camera_direction_or_sign".into());
        }
        r.motion_in_reference_camera_metres.push(local);
    }
    if independent_axis_rank(&r.motion_in_reference_camera_metres) != 3 {
        return Err("motion_axes_not_independent".into());
    }
    Ok(())
}

// q is the normalized camera-to-world quaternion produced by the production decoder.
fn inverse_rotate(q: [f64; 4], v: [f64; 3]) -> [f64; 3] {
    let u = [-q[0], -q[1], -q[2]];
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let t = cross(u, v).map(|x| 2. * x);
    let c = cross(u, t);
    std::array::from_fn(|i| v[i] + q[3] * t[i] + c[i])
}

fn cosine(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).map(|(a, b)| a * b).sum::<f64>() / (vector_magnitude(a) * vector_magnitude(b))
}

fn geometry(l: &LaunchEvidence, bytes: Option<&[u8]>, sha: Sha256Hex) -> Result<(), String> {
    let attestation = l.geometry.as_ref().ok_or("geometry_attestation_missing")?;
    let bytes = bytes.ok_or("geometry_report_bytes_missing")?;
    let digest = sha(bytes);
    if !is_sha256_digest(&digest)
        || digest != attestation.report_sha256
        || geometry_claims_sha256(l, &digest, sha)? != attestation.claims_sha256
    {
        return Err("geometry_content_or_complete_binding_claims_changed".into());
    }
    let header: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("typed_geometry_report_invalid: {e}"))?;
    if header["schema"] == bound_geometry::SCHEMA {
        return bound_geometry::validate(l, bytes);
    }
    let report: Cp2077TypedEgeoReport =
        serde_json::from_slice(bytes).map_err(|e| format!("typed_geometry_report_invalid: {e}"))?;
    if report.schema != CP2077_EGEO_REPORT_SCHEMA_V1
        || !report.non_trainable
        || !report.local_only
        || report.upload_allowed
        || report.e_geo.schema != CAMERA_EGEO_VALIDATION_SCHEMA_V3
        || !report.e_geo.passed
        || !report.e_geo.ready_for_independent_repeat
        || report.e_geo.production_binding_authorized
        || !report.e_geo.failures.is_empty()
        || report.e_geo.raw_depth_profile_sha256 != l.depth_profile_sha256
    {
        return Err("typed_geometry_report_not_eligible_for_this_profile".into());
    }
    Ok(())
}

mod bound_geometry;
pub mod evidence_set;

#[cfg(test)]
mod tests;
