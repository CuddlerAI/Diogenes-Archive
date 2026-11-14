//! Checked-in authoring inputs, not a runtime admission or hardware certification test.
use depth_lab::{CameraBinding, CameraBindingDocument, DepthProfile, Lane};
use serde_json::Value;
use std::path::PathBuf;

const NEW_EXE: &str = "sha256:fdcef37ac1230af6d0b0050eb2142b139ef3a867b37b9211fb6edfcc646072f8";
const OLD_EXE: &str = "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn candidate_json(name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(
            root()
                .join("tools/pr405-fallout4/authoring/1.11.240.0")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn new_candidate_is_exact_build_and_profile_consistent() {
    let binding: CameraBinding =
        serde_json::from_value(candidate_json("camera-binding.json")).unwrap();
    let profile: DepthProfile =
        serde_json::from_value(candidate_json("depth-profile.json")).unwrap();
    let document = CameraBindingDocument::V1(binding.clone());
    document.validate_for_profile(&profile, NEW_EXE).unwrap();
    assert!(document.validate_for_profile(&profile, OLD_EXE).is_err());
    assert_eq!(binding.binding_version, "authoring-trial-1");
    assert_eq!(binding.render_width, 1920);
    assert_eq!(binding.render_height, 1080);
    assert_eq!(profile.lane, Lane::Lane30);
    assert_eq!(profile.depth_hz(), 30);
    assert!((-profile.projection_b / profile.projection_a - 15.0).abs() < 1e-5);
    assert_eq!(profile.metric_scale.value, 0.0142875);
    assert!(root().join(&profile.metric_scale.evidence_path).is_file());
    assert!(root().join(&profile.dsv.evidence_path).is_file());
    let reference = candidate_json("camera-reference.json");
    assert_eq!(
        serde_json::to_value(binding.game_camera.unwrap()).unwrap(),
        reference["game_camera"]
    );
}

#[test]
fn new_candidate_pins_scene_draw_and_preserves_hand_and_projection_conventions() {
    let binding = candidate_json("camera-binding.json");
    assert_eq!(binding["view_sources"].as_array().unwrap().len(), 1);
    assert_eq!(binding["projection_sources"].as_array().unwrap().len(), 1);
    for source in [
        &binding["view_sources"][0],
        &binding["projection_sources"][0],
    ] {
        assert_eq!(source["shader_stage"], "vertex");
        assert_eq!(source["bind_slot"], 12);
        assert_eq!(source["pass_ordinal"], 3);
        assert_eq!(source["draw_ordinal"], 0);
        assert_eq!(source["buffer_bytes"], 752);
        assert_eq!(source["encoding"], "f32_little_endian");
        assert_eq!(source["layout"], "row_major");
    }
    assert_eq!(binding["view_sources"][0]["offset"], 0);
    assert_eq!(binding["view_sources"][0]["transpose"], false);
    assert_eq!(binding["view_sources"][0]["translation"], "row3");
    assert_eq!(binding["projection_sources"][0]["offset"], 64);
    assert_eq!(binding["projection_sources"][0]["transpose"], true);
    assert_eq!(binding["game_camera"]["axis_mapping"], "x_z_y");
    assert_eq!(binding["game_camera"]["metres_per_game_unit"], "0.0142875");
}

#[test]
fn trial_summary_preserves_failures_and_never_claims_production_or_performance() {
    let summary = candidate_json("geometry-evidence.json");
    assert_eq!(summary["production_binding_authorized"], false);
    assert_eq!(summary["performance_certified"], false);
    assert_eq!(summary["deliverable"], false);
    assert_eq!(summary["discovery"]["original_sweep_status"], "Ambiguous");
    assert_eq!(summary["discovery"]["selection"], "explicit_local_trial");
    let trials = summary["passing_trials"].as_array().unwrap();
    assert_eq!(trials.len(), 3);
    for trial in trials {
        assert_eq!(trial["report_schema"], "camera-egeo-trial-validation/v1");
        assert_eq!(trial["passed"], true);
        assert_eq!(trial["production_binding_authorized"], false);
        assert_eq!(trial["eligible_even_frames"], trial["paired_even_frames"]);
        assert!(trial["improved_share"].as_f64().unwrap() >= 0.6);
        assert!(trial["improvement_p50"].as_f64().unwrap() >= 1.05);
        assert!(trial["valid_frac_p10"].as_f64().unwrap() >= 0.5);
        assert!(trial["duplicate_frames"].as_u64().unwrap() > 0);
    }
    let failed = summary["retained_failed_trials"].as_array().unwrap();
    assert_eq!(failed.len(), 3);
    assert!(failed.iter().all(|trial| trial["passed"] == false));
}

#[test]
fn authoring_candidate_does_not_replace_shipped_catalog() {
    let path = root().join("config/catalogs/dev/fallout4-steam-release-test/camera-binding.json");
    let shipped: CameraBinding = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(shipped.executable_sha256, OLD_EXE);
    assert!(shipped.validate_for_executable(NEW_EXE).is_err());
    let summary = candidate_json("geometry-evidence.json");
    assert_ne!(shipped.binding_version, "authoring-trial-1");
    assert_eq!(
        summary["review_files"]["camera_binding"]["semantic_changes_from_measured"],
        serde_json::json!([])
    );
    assert_eq!(
        summary["review_files"]["depth_profile"]["semantic_changes_from_measured"],
        serde_json::json!(["dsv.evidence_path"])
    );
}
