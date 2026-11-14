//! Fail-closed promotion of render-camera discovery evidence into an E-geo authoring binding.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::camera_evidence::{
    CameraEvidenceAnalysisMode, FALLOUT_HYBRID_EVIDENCE_SCHEMA_V2, FALLOUT_HYBRID_SWEEP_SCHEMA_V2,
    analyze_camera_evidence_with_mode, normalized_camera_quaternion, selection_from_window_reports,
};
use crate::{
    CAMERA_BINDING_SCHEMA_V1, CAMERA_DISCOVERY_REPORT_SCHEMA_V2, CAMERA_DISCOVERY_REPORT_SCHEMA_V3,
    CAMERA_DISCOVERY_REPORT_SCHEMA_V4, CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1, CameraBinding,
    CameraDiscoveryReport, CameraEvidenceSelection, CameraEvidenceSweepReport, CameraPoseAssembler,
    CameraPoseStreamContext, GameCameraBinding, MatrixRole, MatrixSourceBinding, StreamIdentity,
    ViewSourceBinding, fallout_render_rotation_from_basis,
};

pub const CAMERA_BINDING_AUTHORING_SCHEMA_V1: &str = "camera-binding-authoring/v1";
/// Explicit, non-promotable experiment when several draws carry a valid camera. The original
/// ambiguous sweep is retained, not rewritten into an allegedly unique discovery result.
pub const CAMERA_BINDING_TRIAL_SCHEMA_V1: &str = "camera-binding-authoring-trial/v1";
const MIN_REFERENCE_RESPONSE_RATIO: f64 = 0.5;
const MAX_REFERENCE_RESPONSE_RATIO: f64 = 2.0;
const MAX_RENDER_REFERENCE_ANGLE_RAD: f64 = 0.02;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoringArtifact {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraBindingAuthoringManifest {
    pub schema: String,
    pub candidate_binding: AuthoringArtifact,
    pub sweep_evidence: AuthoringArtifact,
    pub phase_reports: Vec<AuthoringArtifact>,
    pub selected_view: ViewSourceBinding,
    pub selected_projection: MatrixSourceBinding,
    pub position_source: GameCameraBinding,
    pub view_metres_per_unit: String,
    pub requires_e_geo_validation: bool,
    pub production_binding_authorized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_reason: Option<String>,
}

/// Produce a runtime-readable candidate without authorizing it for delivery.
///
/// Fallout 4's render constant buffer carries a camera-relative, left-handed render basis rather
/// than a standalone rigid view matrix. Authoring therefore keeps the exact-build game-camera
/// position source and promotes only a GPU render basis whose repaired orientation agrees with the
/// same-Present reference. The basis and projection must coexist on one exact draw in all three
/// reliable reports.
pub fn author_render_camera_binding(
    sweep: &CameraEvidenceSweepReport,
    reports: [&CameraDiscoveryReport; 3],
    template: &CameraBinding,
) -> Result<CameraBinding, String> {
    author_render_camera_binding_inner(sweep, reports, template, None)
}

/// Validate a specified source for a local E-geo experiment, without claiming that discovery
/// selected it uniquely. The regular authoring path continues to reject ambiguity.
pub fn author_render_camera_trial(
    sweep: &CameraEvidenceSweepReport,
    reports: [&CameraDiscoveryReport; 3],
    template: &CameraBinding,
    source: ViewSourceBinding,
) -> Result<CameraBinding, String> {
    author_render_camera_binding_inner(sweep, reports, template, Some(source))
}

fn author_render_camera_binding_inner(
    sweep: &CameraEvidenceSweepReport,
    reports: [&CameraDiscoveryReport; 3],
    template: &CameraBinding,
    trial: Option<ViewSourceBinding>,
) -> Result<CameraBinding, String> {
    if !sweep.requires_e_geo_validation || sweep.production_binding_authorized {
        return Err("camera sweep is not an unpromoted E-geo authoring result".into());
    }
    if template.executable_sha256 != sweep.executable_sha256 {
        return Err("camera template and sweep executable hashes differ".into());
    }
    let hybrid = sweep.schema == FALLOUT_HYBRID_SWEEP_SCHEMA_V2;
    validate_authoring_evidence(sweep, reports, trial)?;
    let game_camera = template
        .game_camera
        .clone()
        .ok_or("camera template must carry one exact-build game-camera position source")?;
    if hybrid
        && reports
            .iter()
            .any(|report| report.config.comparison_camera_reference.as_ref() != Some(&game_camera))
    {
        return Err("hybrid template position reference differs from captured reference".into());
    }
    if template.view_metres_per_unit.is_some() || !template.view_sources.is_empty() {
        return Err("camera template must be an exact-build game-camera binding".into());
    }
    if template.projection_sources.len() != 1 {
        return Err("camera template must carry exactly one audited projection".into());
    }
    let template_projection = template.projection_sources[0];
    if let Some(source) = trial {
        if source.source.pass_ordinal != template_projection.pass_ordinal
            || source.source.draw_ordinal != template_projection.draw_ordinal
        {
            return Err(
                "trial source and template projection must name the same exact draw".into(),
            );
        }
        for report in reports {
            let view = report
                .view_candidates
                .iter()
                .find(|candidate| candidate.source == source)
                .ok_or("trial source is missing from a raw phase")?;
            report.validate_main_scene_draw(view)?;
            if view.observations.len() != report.comparison_camera_samples.len()
                || !report.projection_candidates.iter().any(|candidate| {
                    candidate.source == template_projection && candidate.frames == view.frames
                })
            {
                return Err("trial requires complete same-Present view/projection/reference coverage in every phase".into());
            }
            let mut viewport = None;
            for row in report.draw_contexts.iter().filter(|row| {
                row.pass_ordinal == source.source.pass_ordinal
                    && row.draw_ordinal == source.source.draw_ordinal
            }) {
                if let Some(expected) = viewport
                    && row.viewport != Some(expected)
                {
                    return Err("trial exact-draw viewport changes within a phase".into());
                }
                viewport = row.viewport;
            }
        }
    }
    let mut scale = None::<String>;
    for report in reports {
        if !report.reliable || !report.blockers.is_empty() {
            return Err(format!(
                "camera phase {} is not reliable: {:?}",
                report.phase_id, report.blockers
            ));
        }
        if report.config.executable_sha256 != sweep.executable_sha256
            || report.render.width != template.render_width
            || report.render.height != template.render_height
        {
            return Err(format!(
                "camera phase {} does not match executable/render identity",
                report.phase_id
            ));
        }
        let report_scale = report
            .config
            .comparison_camera_reference
            .as_ref()
            .ok_or_else(|| format!("camera phase {} has no metric reference", report.phase_id))?
            .metres_per_game_unit
            .clone();
        let parsed = report_scale
            .parse::<f64>()
            .map_err(|_| format!("camera phase {} metric scale is invalid", report.phase_id))?;
        if !parsed.is_finite() || parsed <= 0.0 {
            return Err(format!(
                "camera phase {} metric scale is not positive and finite",
                report.phase_id
            ));
        }
        match &scale {
            Some(previous) if previous != &report_scale => {
                return Err("camera metric scale differs across phases".into());
            }
            None => scale = Some(report_scale),
            _ => {}
        }
    }
    let scale = scale.ok_or("camera metric scale is missing after phase validation")?;
    if scale != game_camera.metres_per_game_unit {
        return Err("camera template and discovery reference metric scales differ".into());
    }

    let (selected_view, selected_projection) =
        select_render_basis(reports, template_projection, hybrid, trial)?;
    if let Some(source) = trial {
        if selected_view != source {
            return Err("trial authoring selected a different source".into());
        }
    } else if hybrid {
        let CameraEvidenceSelection::Selected {
            source,
            equivalent_draw_ordinals,
        } = &sweep.selection
        else {
            return Err("hybrid sweep lost its unique selection".into());
        };
        if without_draw(*source) != without_draw(selected_view)
            || !equivalent_draw_ordinals.contains(&selected_view.source.draw_ordinal)
        {
            return Err("authored hybrid source differs from evidence selection".into());
        }
    }
    let candidate = CameraBinding {
        schema: CAMERA_BINDING_SCHEMA_V1.into(),
        binding_id: format!("{}-render-hybrid-authoring-egeo-v2", template.binding_id),
        // v3 pins the verified camera-to-world convention. v2 repaired Fallout's
        // handedness but accidentally emitted the remaining world-to-camera basis.
        binding_version: if trial.is_some() {
            "authoring-trial-1"
        } else {
            "authoring-egeo-4"
        }
        .into(),
        profile_id: template.profile_id.clone(),
        profile_version: template.profile_version.clone(),
        executable_sha256: template.executable_sha256.clone(),
        render_width: template.render_width,
        render_height: template.render_height,
        view_metres_per_unit: None,
        game_camera: Some(game_camera),
        view_sources: vec![selected_view],
        projection_sources: vec![selected_projection],
    };
    CameraPoseAssembler::new(
        &candidate,
        CameraPoseStreamContext {
            identity: StreamIdentity {
                session_id: "authoring-validation".into(),
                run_id: "authoring-validation".into(),
                generation: 1,
            },
            dsv_id: "authoring-validation".into(),
        },
        &candidate.executable_sha256,
    )
    .map_err(|error| format!("authored CameraBinding is invalid: {error}"))?;
    Ok(candidate)
}

fn validate_authoring_evidence(
    sweep: &CameraEvidenceSweepReport,
    reports: [&CameraDiscoveryReport; 3],
    trial: Option<ViewSourceBinding>,
) -> Result<(), String> {
    let mode = if sweep.schema == FALLOUT_HYBRID_SWEEP_SCHEMA_V2 {
        if sweep.windows.iter().any(|window| {
            window.evidence.schema != FALLOUT_HYBRID_EVIDENCE_SCHEMA_V2
                || window.evidence.production_binding_authorized
                || !window.evidence.requires_e_geo_validation
        }) {
            return Err("hybrid sweep mixes analysis schemas or promotion states".into());
        }
        CameraEvidenceAnalysisMode::FalloutRenderHybrid
    } else if sweep.schema == CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1 {
        CameraEvidenceAnalysisMode::RigidView
    } else {
        return Err(format!(
            "unsupported camera evidence sweep schema {}",
            sweep.schema
        ));
    };
    if reports.iter().any(|report| {
        report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V2
            && report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V3
            && report.schema != CAMERA_DISCOVERY_REPORT_SCHEMA_V4
    }) || reports[1].schema != reports[0].schema
        || reports[2].schema != reports[0].schema
    {
        return Err("camera authoring phase reports must share one supported schema".into());
    }
    let start = reports[0].config.draw_window_start;
    let count = reports[0].config.draw_window_count;
    if reports[1].config.draw_window_start != start
        || reports[2].config.draw_window_start != start
        || reports[1].config.draw_window_count != count
        || reports[2].config.draw_window_count != count
    {
        return Err("camera authoring phase reports do not share one draw window".into());
    }
    let suffix = format!("W{start:03}");
    let expected_phases = [
        format!("POSE_STATIC_{suffix}"),
        format!("POSE_WALK_{suffix}"),
        format!("POSE_TURN_{suffix}"),
    ];
    let recomputed = analyze_camera_evidence_with_mode(
        reports[0],
        reports[1],
        reports[2],
        [
            &expected_phases[0],
            &expected_phases[1],
            &expected_phases[2],
        ],
        mode,
    )?;
    let matching_windows = sweep
        .windows
        .iter()
        .filter(|window| window.draw_window_start == start && window.draw_window_count == count)
        .collect::<Vec<_>>();
    let recomputed_value = serde_json::to_value(&recomputed)
        .map_err(|error| format!("serialize recomputed camera evidence failed: {error}"))?;
    let stored_value = matching_windows
        .first()
        .map(|window| serde_json::to_value(&window.evidence))
        .transpose()
        .map_err(|error| format!("serialize stored camera evidence failed: {error}"))?;
    let difference = stored_value
        .as_ref()
        .and_then(|stored| first_json_difference(stored, &recomputed_value, "$"));
    if matching_windows.len() != 1 || difference.is_some() {
        let difference = difference.unwrap_or_else(|| "window multiplicity differs".into());
        return Err(format!(
            "camera authoring phase reports are not the exact evidence stored in the sweep window: start={start} count={count} matches={} difference={difference}",
            matching_windows.len(),
        ));
    }
    if let Some(source) = trial {
        if mode != CameraEvidenceAnalysisMode::FalloutRenderHybrid
            || sweep.windows.len() != 1
            || sweep.selection != selection_from_window_reports(&sweep.windows)
            || !recomputed.candidates.iter().any(|candidate| {
                candidate.source == source && candidate.controlled_phase_pattern_matches
            })
        {
            return Err("trial source must pass the exact replayed hybrid window; raw sweep ambiguity is not waived or rewritten".into());
        }
        return Ok(());
    }
    let CameraEvidenceSelection::Selected {
        source,
        equivalent_draw_ordinals,
    } = &sweep.selection
    else {
        return Err("camera evidence sweep has no unique selected render basis".into());
    };
    let CameraEvidenceSelection::Selected {
        source: window_source,
        equivalent_draw_ordinals: window_draws,
    } = &recomputed.selection
    else {
        return Err("camera authoring window has no selected sweep source".into());
    };
    if without_draw(*window_source) != without_draw(*source)
        || !equivalent_draw_ordinals.contains(&window_source.source.draw_ordinal)
        || !window_draws.contains(&source.source.draw_ordinal)
    {
        return Err(
            "camera evidence sweep selection is inconsistent with its authoring window".into(),
        );
    }
    Ok(())
}

fn first_json_difference(
    left: &serde_json::Value,
    right: &serde_json::Value,
    path: &str,
) -> Option<String> {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            for key in left.keys().chain(right.keys()) {
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => {
                        if let Some(difference) =
                            first_json_difference(left, right, &format!("{path}.{key}"))
                        {
                            return Some(difference);
                        }
                    }
                    _ => return Some(format!("{path}.{key}: object key differs")),
                }
            }
            None
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Some(format!(
                    "{path}: array length {} != {}",
                    left.len(),
                    right.len()
                ));
            }
            left.iter()
                .zip(right)
                .enumerate()
                .find_map(|(index, (left, right))| {
                    first_json_difference(left, right, &format!("{path}[{index}]"))
                })
        }
        (Value::Number(left), Value::Number(right)) => {
            if (left.is_i64() || left.is_u64()) && (right.is_i64() || right.is_u64()) {
                return (left != right).then(|| format!("{path}: {left} != {right}"));
            }
            let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) else {
                return Some(format!("{path}: {left} != {right}"));
            };
            let tolerance = 1e-12_f64.max(1e-12 * left.abs().max(right.abs()));
            ((left - right).abs() > tolerance).then(|| format!("{path}: {left} != {right}"))
        }
        _ if left == right => None,
        _ => Some(format!("{path}: {left} != {right}")),
    }
}

fn select_render_basis(
    reports: [&CameraDiscoveryReport; 3],
    template_projection: MatrixSourceBinding,
    normalize_valid_rotations: bool,
    trial: Option<ViewSourceBinding>,
) -> Result<(ViewSourceBinding, MatrixSourceBinding), String> {
    let mut qualified = BTreeMap::<ViewSourceBinding, Vec<u32>>::new();
    for candidate in &reports[0].view_candidates {
        if trial.is_some_and(|source| source != candidate.source) {
            continue;
        }
        if candidate.frames < 8 || candidate.observations.len() < 8 {
            continue;
        }
        let Some(_) = candidate.observations.first().and_then(|row| {
            fallout_render_rotation_from_basis(&row.matrix, candidate.source.translation)
        }) else {
            continue;
        };
        let semantic = without_draw(candidate.source);
        qualified.entry(semantic).or_default();
    }

    qualified.retain(|semantic, common_draws| {
        let phase_draws = reports.map(|report| {
            qualified_draws(
                report,
                *semantic,
                template_projection,
                normalize_valid_rotations,
            )
        });
        if phase_draws.iter().any(Vec::is_empty) {
            return false;
        }
        *common_draws = phase_draws[0]
            .iter()
            .copied()
            .filter(|draw| phase_draws[1].contains(draw) && phase_draws[2].contains(draw))
            .collect();
        common_draws.retain(|draw| {
            if trial.is_some_and(|source| source.source.draw_ordinal != *draw) {
                return false;
            }
            let mut exact = *semantic;
            exact.source.draw_ordinal = *draw;
            reports.iter().all(|report| {
                validate_render_basis_phase(report, exact, normalize_valid_rotations).is_ok()
            }) && validate_response_ratio(reports[1], exact, "walk", normalize_valid_rotations)
                .is_ok()
                && validate_response_ratio(reports[2], exact, "turn", normalize_valid_rotations)
                    .is_ok()
        });
        !common_draws.is_empty()
    });

    if qualified.len() != 1 {
        return Err(format!(
            "camera authoring found {} exact-build-aligned render-basis identities; expected exactly one",
            qualified.len()
        ));
    }
    let (semantic, draws) = qualified.into_iter().next().expect("one qualified source");
    let draw = if draws.contains(&template_projection.draw_ordinal) {
        template_projection.draw_ordinal
    } else {
        *draws
            .iter()
            .min()
            .expect("qualified source has a common draw")
    };
    let mut selected_view = semantic;
    selected_view.source.draw_ordinal = draw;
    reports.iter().try_for_each(|report| {
        validate_render_basis_phase(report, selected_view, normalize_valid_rotations)
    })?;
    validate_response_ratio(reports[1], selected_view, "walk", normalize_valid_rotations)?;
    validate_response_ratio(reports[2], selected_view, "turn", normalize_valid_rotations)?;
    let mut selected_projection = template_projection;
    selected_projection.pass_ordinal = selected_view.source.pass_ordinal;
    selected_projection.draw_ordinal = draw;
    Ok((selected_view, selected_projection))
}

fn qualified_draws(
    report: &CameraDiscoveryReport,
    semantic: ViewSourceBinding,
    template_projection: MatrixSourceBinding,
    require_scene_viewport: bool,
) -> Vec<u32> {
    report
        .view_candidates
        .iter()
        .filter(|candidate| {
            candidate.frames >= 8
                && candidate.observations.len() >= 8
                && (!require_scene_viewport || report.validate_main_scene_draw(candidate).is_ok())
                && without_draw(candidate.source) == semantic
                && report.projection_candidates.iter().any(|projection| {
                    projection.frames >= 8
                        && projection.source.pass_ordinal == candidate.source.source.pass_ordinal
                        && projection.source.draw_ordinal == candidate.source.source.draw_ordinal
                        && same_projection_contract(projection.source, template_projection)
                })
        })
        .map(|candidate| candidate.source.source.draw_ordinal)
        .collect()
}

fn validate_render_basis_phase(
    report: &CameraDiscoveryReport,
    selected: ViewSourceBinding,
    normalize_valid_rotations: bool,
) -> Result<(), String> {
    let candidate = report
        .view_candidates
        .iter()
        .find(|candidate| candidate.source == selected)
        .ok_or_else(|| format!("camera phase {} is missing render basis", report.phase_id))?;
    let reference = report
        .comparison_camera_samples
        .iter()
        .map(|row| {
            Ok((
                row.present_seq,
                authoring_reference_rotation(&report.schema, row.camera_rotation_quaternion)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut errors = Vec::new();
    for observation in &candidate.observations {
        let rotation =
            fallout_render_rotation_from_basis(&observation.matrix, candidate.source.translation)
                .ok_or_else(|| {
                format!(
                    "camera phase {} contains an invalid Fallout render basis at Present {}",
                    report.phase_id, observation.present_seq
                )
            })?;
        let reference = reference.get(&observation.present_seq).ok_or_else(|| {
            format!(
                "camera phase {} render basis Present {} has no exact-build reference",
                report.phase_id, observation.present_seq
            )
        })?;
        errors.push(authoring_quaternion_angle(
            rotation.map(f64::from),
            *reference,
            normalize_valid_rotations,
        )?);
    }
    if errors.len() < 8 {
        return Err(format!(
            "camera phase {} has only {} exact render/reference joins",
            report.phase_id,
            errors.len()
        ));
    }
    let maximum = errors.iter().copied().fold(0.0_f64, f64::max);
    if maximum > MAX_RENDER_REFERENCE_ANGLE_RAD {
        return Err(format!(
            "camera phase {} render/reference orientation max {:.6} rad exceeds {:.6}",
            report.phase_id, maximum, MAX_RENDER_REFERENCE_ANGLE_RAD
        ));
    }
    Ok(())
}

fn validate_response_ratio(
    report: &CameraDiscoveryReport,
    selected: ViewSourceBinding,
    label: &str,
    normalize_valid_rotations: bool,
) -> Result<(), String> {
    let candidate = report
        .view_candidates
        .iter()
        .find(|candidate| candidate.source == selected)
        .ok_or_else(|| format!("camera phase {} is missing render basis", report.phase_id))?;
    let reference = report
        .comparison_camera_samples
        .iter()
        .map(|row| {
            Ok((
                row.present_seq,
                authoring_reference_rotation(&report.schema, row.camera_rotation_quaternion)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let joined = candidate
        .observations
        .iter()
        .map(|observation| {
            let rotation = fallout_render_rotation_from_basis(
                &observation.matrix,
                candidate.source.translation,
            )?;
            Some((
                observation.present_seq,
                rotation.map(f64::from),
                reference.get(&observation.present_seq)?.to_owned(),
            ))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            format!(
                "camera phase {} has an incomplete render/reference join",
                report.phase_id
            )
        })?;
    let mut candidate_steps = joined
        .windows(2)
        .filter(|pair| pair[1].0 > pair[0].0)
        .map(|pair| authoring_quaternion_angle(pair[0].1, pair[1].1, normalize_valid_rotations))
        .collect::<Result<Vec<_>, _>>()?;
    let mut reference_steps = joined
        .windows(2)
        .filter(|pair| pair[1].0 > pair[0].0)
        .map(|pair| authoring_quaternion_angle(pair[0].2, pair[1].2, normalize_valid_rotations))
        .collect::<Result<Vec<_>, _>>()?;
    let candidate_p95 = quantile(&mut candidate_steps, 0.95);
    let reference_p95 = quantile(&mut reference_steps, 0.95);
    require_response_ratio(label, candidate_p95, reference_p95)
}

fn authoring_reference_rotation(schema: &str, rotation: [f64; 4]) -> Result<[f64; 4], String> {
    match schema {
        // v2 exact-build rows exposed the engine's world-to-camera basis.  Quaternion
        // conjugation is the exact inverse needed to compare them with the corrected
        // authoring candidate; the raw discovery observations remain untouched.
        CAMERA_DISCOVERY_REPORT_SCHEMA_V2 => {
            Ok([-rotation[0], -rotation[1], -rotation[2], rotation[3]])
        }
        CAMERA_DISCOVERY_REPORT_SCHEMA_V3 | CAMERA_DISCOVERY_REPORT_SCHEMA_V4 => Ok(rotation),
        other => Err(format!(
            "camera phase report schema {other} has no authoring orientation convention"
        )),
    }
}

fn require_response_ratio(label: &str, candidate: f64, reference: f64) -> Result<(), String> {
    if !candidate.is_finite() || !reference.is_finite() || reference <= 0.0 {
        return Err(format!(
            "{label} response cannot be compared to a positive finite reference"
        ));
    }
    let ratio = candidate / reference;
    if !(MIN_REFERENCE_RESPONSE_RATIO..=MAX_REFERENCE_RESPONSE_RATIO).contains(&ratio) {
        return Err(format!(
            "selected view {label} p95 ratio {ratio:.6} is outside [{MIN_REFERENCE_RESPONSE_RATIO}, {MAX_REFERENCE_RESPONSE_RATIO}]"
        ));
    }
    Ok(())
}

fn without_draw(mut source: ViewSourceBinding) -> ViewSourceBinding {
    source.source.draw_ordinal = 0;
    source
}

fn authoring_quaternion_angle(a: [f64; 4], b: [f64; 4], normalize: bool) -> Result<f64, String> {
    let (a, b) = if normalize {
        (
            normalized_camera_quaternion(a)?,
            normalized_camera_quaternion(b)?,
        )
    } else {
        (a, b)
    };
    let dot = (0..4)
        .map(|axis| a[axis] * b[axis])
        .sum::<f64>()
        .abs()
        .clamp(0.0, 1.0);
    Ok(2.0 * dot.acos())
}

fn quantile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let index = ((values.len() - 1) as f64 * q).round() as usize;
    values[index]
}

fn same_projection_contract(candidate: MatrixSourceBinding, template: MatrixSourceBinding) -> bool {
    candidate.shader_stage == template.shader_stage
        && candidate.bind_slot == template.bind_slot
        && candidate.buffer_bytes == template.buffer_bytes
        && candidate.offset == template.offset
        && candidate.encoding == template.encoding
        && candidate.layout == template.layout
        && candidate.transpose == template.transpose
        && candidate.role == MatrixRole::Projection
        && template.role == MatrixRole::Projection
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_v2_reference_is_explicitly_inverted_but_v3_is_not() {
        let world_to_camera = [0.1, -0.2, 0.3, 0.9];
        assert_eq!(
            authoring_reference_rotation(CAMERA_DISCOVERY_REPORT_SCHEMA_V2, world_to_camera)
                .unwrap(),
            [-0.1, 0.2, -0.3, 0.9]
        );
        assert_eq!(
            authoring_reference_rotation(CAMERA_DISCOVERY_REPORT_SCHEMA_V3, world_to_camera)
                .unwrap(),
            world_to_camera
        );
        assert!(
            authoring_reference_rotation("camera-discovery-report/v99", world_to_camera).is_err()
        );
    }

    #[test]
    fn stored_evidence_allows_only_json_float_round_trip_noise() {
        let stored = serde_json::json!({"metric": 198297.52464081263, "identity": 7});
        let recomputed = serde_json::json!({"metric": 198297.52464081265, "identity": 7});
        assert!(first_json_difference(&stored, &recomputed, "$").is_none());

        let changed = serde_json::json!({"metric": 198298.0, "identity": 7});
        let difference = first_json_difference(&stored, &changed, "$").unwrap();
        assert!(difference.contains("$.metric"), "{difference}");

        let changed_identity = serde_json::json!({"metric": 198297.52464081263, "identity": 8});
        let difference = first_json_difference(&stored, &changed_identity, "$").unwrap();
        assert!(difference.contains("$.identity"), "{difference}");
    }
}
