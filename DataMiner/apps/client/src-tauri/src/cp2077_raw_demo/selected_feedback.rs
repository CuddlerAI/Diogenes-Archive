//! Local-only bootstrap. Recording owns all Select/Retire/Seal messages after handoff.
use super::*;
use depth_lab::cp2077_raw_feedback as contract;
use depth_lab::selected_present_transport::{ClientTransport, TransportNames};

#[cfg(test)]
pub(super) mod review_tests;

pub(super) struct Prepared {
    pub request: contract::RawSelectedRequest,
    pub transport: ClientTransport,
}

/// Read the actual recorded opt-in; a summary alone cannot change the cadence.
pub(super) fn recorded_even_cfr_request(dir: &Path, launch: &str) -> anyhow::Result<bool> {
    let path = dir.join(ARM_FILE);
    if !path.exists() {
        return Ok(false); // Legacy offline sessions need not have this file.
    }
    let arm: Value = serde_json::from_slice(&read_bounded_file(&path, 128 * 1024)?)?;
    let Some(selected) = arm.get("selected_present").filter(|v| !v.is_null()) else {
        return Ok(false);
    };
    if selected["schema"] == "dm-cp2077-raw-selected-request/v1" {
        return Ok(false);
    }
    let selected: contract::RawSelectedRequest = serde_json::from_value(selected.clone())?;
    selected.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        arm["schema"] == "dm-cp2077-raw-depth-demo-arm-request/v2"
            && arm["process_launch_id"] == launch
            && arm["capture_contract"] == depth_lab::ISSUE735_LOCAL_CONTRACT
            && arm["duration_ms"] == depth_lab::ISSUE735_LOCAL_DURATION_MS
            && arm["target_depth_hz"] == contract::TARGET_DEPTH_HZ
            && arm["manual_stop"].as_bool().is_some(),
        "even-CFR Arm identity or capture policy mismatch"
    );
    Ok(true)
}

pub(super) fn audit_even_cfr(
    dir: &Path,
    frames_path: &Path,
    samples: &std::collections::HashMap<u64, Vec<u8>>,
    summary: &Value,
    report: &mut RawRgbDepthPairingReport,
) -> anyhow::Result<()> {
    let rgb: Vec<contract::EvenCfrRgbFrame> = read_bounded_jsonl(frames_path, true)?;
    let depth_path = dir.join("raw-depth-index.jsonl");
    let depth: Vec<contract::EvenCfrDepthFrame> = read_bounded_jsonl(&depth_path, false)?;
    let launch = summary["process_launch_id"]
        .as_str()
        .context("even-CFR summary lacks launch")?;
    let stream = contract::stream(launch, 1).map_err(anyhow::Error::msg)?;
    let expected_frames = dir
        .join("rgb-recorder")
        .join(stream.identity.session_id())
        .join("runs/r0000/segments/00000/frames.jsonl.zst");
    ensure!(
        fs::canonicalize(frames_path)? == fs::canonicalize(expected_frames)?,
        "even-CFR RGB index belongs to a different run"
    );
    ensure!(
        recorded_even_cfr_request(dir, launch)?,
        "even-CFR recorded opt-in is missing"
    );
    let meta: Value = serde_json::from_slice(&read_bounded_file(
        &frames_path.with_file_name("meta.json"),
        128 * 1024,
    )?)?;
    ensure!(
        meta["fps_num"] == 30
            && meta["fps_den"] == 1
            && meta["width"] == 1920
            && meta["height"] == 1080
            && meta["n_frames"].as_u64() == Some(rgb.len() as u64),
        "even-CFR depth requires the unchanged native 30 FPS RGB profile"
    );
    ensure!(
        summary["schema"] == contract::SUMMARY_SCHEMA
            && summary["depth_selection"] == contract::DEPTH_SELECTION
            && summary["feedback_schema"]
                == depth_lab::selected_present_feedback::FEEDBACK_SCHEMA_V4
            && summary["generation"] == contract::GENERATION
            && summary["resource_epoch"] == 1
            && summary["non_trainable"] == true
            && summary["recording_ready"] == false
            && summary["sealed"] == true
            && summary.get("fault").is_some_and(Value::is_null),
        "even-CFR summary identity, policy or completion state is invalid"
    );
    for (key, expected) in [
        ("selected_rgb_frames", rgb.len()),
        ("required_depth_rgb_frames", rgb.len().div_ceil(2)),
        ("skipped_depth_rgb_frames", rgb.len() / 2),
        ("distinct_selected_depth", depth.len()),
        ("readback_submitted", depth.len()),
        ("readback_completed", depth.len()),
        ("written_depth_frames", depth.len()),
        ("aliases", rgb.len().div_ceil(2).saturating_sub(depth.len())),
        ("unresolved", 0),
        ("decision_queue_depth", 0),
        ("ack_queue_depth", 0),
    ] {
        ensure!(
            summary[key].as_u64() == Some(expected as u64),
            "even-CFR summary count mismatch: {key}"
        );
    }
    let decoded = samples
        .iter()
        .filter_map(|(frame, means)| {
            depth_lab::watermark::decode(means, 32)
                .ok()
                .map(|present| (*frame, present))
        })
        .collect();
    report.schema = contract::PAIRING_SCHEMA;
    match contract::map_even_cfr_depth(&rgb, &depth, &decoded, report.pairing_window_lag_ticks) {
        Ok(mapping) => {
            report.even_cfr = Some(json!({
                "process_launch_id": launch, "session_id": stream.identity.session_id(),
                "run_id": stream.identity.run_id(), "generation": stream.generation,
                "resource_epoch": stream.resource_epoch, "mapping": mapping,
                "rgb_index_sha256": sha256_file(frames_path)?,
                "raw_depth_index_sha256": sha256_file(&depth_path)?,
                "watermark_samples_sha256": sha256_file(&dir.join(WATERMARK_SAMPLES_FILE))?,
                "summary_sha256": sha256_file(&dir.join(contract::SUMMARY_FILE))?,
                "arm_request_sha256": sha256_file(&dir.join(ARM_FILE))?,
                "rgb_meta_sha256": sha256_file(&frames_path.with_file_name("meta.json"))?,
            }));
        }
        Err(error) => {
            report.passed = false;
            report.blockers.push(error);
        }
    }
    Ok(())
}

pub(super) fn prepare(pid: u32, launch: &str) -> anyhow::Result<Prepared> {
    ensure!(
        cfg!(all(feature = "cp2077-diagnostics", debug_assertions)),
        "selected-Present raw diagnostics require a diagnostic build"
    );
    let request = contract::RawSelectedRequest {
        schema: contract::REQUEST_SCHEMA.into(),
        feedback_schema: depth_lab::selected_present_feedback::FEEDBACK_SCHEMA_V4,
        depth_selection: contract::DEPTH_SELECTION.into(),
        holstered: true,
        non_trainable: true,
        bootstrap: depth_lab::SelectedPresentFeedbackBootstrap {
            client_process_id: std::process::id(),
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            resource_epoch: 1,
            queue_capacity: 512,
            ring_capacity: 96,
            max_feedback_lag_presents: 88,
            guard_slots: 8,
            width: 1920,
            height: 1080,
            bytes_per_pixel: 4,
            vram_limit_bytes: 1024 * 1024 * 1024,
            vram_reserve_bytes: 256 * 1024 * 1024,
        },
    };
    request.validate().map_err(anyhow::Error::msg)?;
    let stream =
        contract::stream(launch, request.bootstrap.resource_epoch).map_err(anyhow::Error::msg)?;
    let names = TransportNames::new(
        std::process::id(),
        pid,
        stream.generation,
        &request.bootstrap.nonce,
    )?;
    let transport =
        ClientTransport::create(&names, &stream, request.bootstrap.queue_capacity as usize)?;
    Ok(Prepared { request, transport })
}

pub(super) fn wait_ready(
    dir: &Path,
    launch: &str,
    profile_sha: &str,
) -> anyhow::Result<capture_core::capture::PresentIdentitySession> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        ensure!(
            !dir.join(TELEMETRY_FILE).exists(),
            "raw probe stopped before selected-Present local Ready"
        );
        let path = dir.join(contract::READY_FILE);
        if path.exists() {
            let value: Value = serde_json::from_slice(&read_bounded_file(&path, 128 * 1024)?)?;
            return validate_ready(&value, launch, profile_sha);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    anyhow::bail!(
        "timed out waiting for 120 holstered camera Presents and local selected-Present Ready"
    )
}

fn validate_ready(
    v: &Value,
    launch: &str,
    profile: &str,
) -> anyhow::Result<capture_core::capture::PresentIdentitySession> {
    ensure!(
        v["schema"] == contract::READY_SCHEMA
            && v["feedback_schema"] == depth_lab::selected_present_feedback::FEEDBACK_SCHEMA_V4
            && v["depth_selection"] == contract::DEPTH_SELECTION
            && v["process_launch_id"] == launch
            && v["profile_sha256"] == profile
            && v["non_trainable"] == true
            && v["production_certified"] == false
            && v["recording_ready"] == false
            && v["generation"] == contract::GENERATION
            && v["resource_epoch"] == 1
            && v["candidate_materialized"] == 0
            && v["watermark_source_width"] == 1920
            && v["watermark_source_height"] == 1080,
        "local selected-Present Ready identity/geometry/state mismatch"
    );
    let camera = &v["camera_admission"];
    ensure!(
        camera["schema"] == depth_lab::d3d12_camera_local::POLICY
            && camera["non_trainable"] == true
            && camera["production_camera_binding_emitted"] == false
            && camera["cross_launch_admission"] == false
            && camera["required_presents"]
                .as_u64()
                .is_some_and(|n| n >= 120)
            && camera["exact_presents"].as_u64().is_some_and(|n| n >= 120),
        "local Ready lacks bounded same-launch camera admission"
    );
    let exact = camera["exact_presents"]
        .as_u64()
        .context("missing exact Present count")?;
    let required = camera["required_presents"]
        .as_u64()
        .context("missing required Present count")?;
    ensure!(
        exact <= required && u128::from(exact) * 10 >= u128::from(required) * 9,
        "local Ready camera coverage is inconsistent"
    );
    for (field, role) in [
        ("view_sources", depth_lab::MatrixRole::View),
        ("projection_sources", depth_lab::MatrixRole::Projection),
    ] {
        let sources: Vec<depth_lab::D3d12MatrixSourceBinding> =
            serde_json::from_value(camera[field].clone())?;
        ensure!(
            !sources.is_empty() && sources.len() <= 64,
            "local Ready has an empty/unbounded camera source group"
        );
        let mut unique = std::collections::BTreeSet::new();
        for source in sources {
            source.validate_for_role(role)?;
            ensure!(
                unique.insert(source),
                "duplicate local Ready camera binding"
            );
        }
    }
    let first = v["first_present_seq"]
        .as_u64()
        .filter(|&n| n > 0)
        .context("local Ready lacks exact Present baseline")?;
    Ok(capture_core::capture::PresentIdentitySession {
        generation: contract::GENERATION,
        resource_epoch: 1,
        first_present_seq: first,
        unwrap_accept_budget: 32767,
        watermark_source_width: 1920,
        watermark_source_height: 1080,
    })
}

/// An external diagnostic controller requests RGB stop; raw Stop is published only after Seal ACK.
pub(super) fn rgb_stop_requested(dir: &Path, launch: &str) -> anyhow::Result<bool> {
    let path = dir.join(contract::RGB_STOP_FILE);
    if !path.exists() {
        return Ok(false);
    }
    let v: Value = serde_json::from_slice(&read_bounded_file(&path, 4096)?)?;
    ensure!(
        v["schema"] == "dm-cp2077-raw-selected-rgb-stop/v1" && v["process_launch_id"] == launch,
        "raw selected RGB stop identity/schema mismatch"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> Value {
        let source = |role: &str, offset: u32| {
            json!({"descriptor_table":{"root_signature_sha256":format!("sha256:{}","a".repeat(64)),
            "root_parameter_index":0,"descriptor_range_index":0,"descriptor_offset_in_range":1,"descriptor_count":4,
            "base_shader_register":0,"register_space":0},"cbv_resource_offset":0,"declared_cbv_bytes":1024,
            "matrix_offset":offset,"encoding":"f32_little_endian","layout":"row_major","transpose":true,
            "role":role,"draw_relation":"exact_selected_depth_dsv_draw"})
        };
        json!({"schema":contract::READY_SCHEMA,"process_launch_id":"launch","profile_sha256":"profile",
        "feedback_schema":depth_lab::selected_present_feedback::FEEDBACK_SCHEMA_V4,"depth_selection":contract::DEPTH_SELECTION,
        "non_trainable":true,"production_certified":false,"recording_ready":false,"generation":1,"resource_epoch":1,
        "candidate_materialized":0,"watermark_source_width":1920,"watermark_source_height":1080,"first_present_seq":1234,
        "camera_admission":{"schema":depth_lab::d3d12_camera_local::POLICY,"non_trainable":true,
            "production_camera_binding_emitted":false,"cross_launch_admission":false,"required_presents":120,"exact_presents":120,
            "view_sources":[source("view",0)],"projection_sources":[source("projection",128)]}})
    }
    #[test]
    fn local_ready_requires_exact_new_launch_profile_camera_and_prearm_state() {
        let v = ready();
        assert_eq!(
            validate_ready(&v, "launch", "profile")
                .unwrap()
                .first_present_seq,
            1234
        );
        for (field, value) in [
            ("process_launch_id", json!("old")),
            ("feedback_schema", json!(3)),
            ("depth_selection", json!("every_rgb_frame")),
            ("profile_sha256", json!("old")),
            ("candidate_materialized", json!(1)),
            ("recording_ready", json!(true)),
            ("generation", json!(2)),
            ("first_present_seq", json!(null)),
            ("watermark_source_height", json!(720)),
        ] {
            let mut bad = v.clone();
            bad[field] = value;
            assert!(
                validate_ready(&bad, "launch", "profile").is_err(),
                "{field}"
            );
        }
        let mut bad = v;
        bad["camera_admission"]["exact_presents"] = json!(119);
        assert!(validate_ready(&bad, "launch", "profile").is_err());
        let mut bad = ready();
        bad["camera_admission"]["view_sources"] = json!([]);
        assert!(validate_ready(&bad, "launch", "profile").is_err());
    }
    #[test]
    fn rgb_stop_is_separate_bounded_and_launch_bound() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!rgb_stop_requested(dir.path(), "launch").unwrap());
        write_stop_request(dir.path(), "launch").unwrap();
        assert!(
            !rgb_stop_requested(dir.path(), "launch").unwrap(),
            "raw Stop is never reused as RGB Seal control"
        );
        write_request_atomic(
            dir.path(),
            contract::RGB_STOP_FILE,
            &json!({"schema":"dm-cp2077-raw-selected-rgb-stop/v1","process_launch_id":"launch"}),
        )
        .unwrap();
        assert!(rgb_stop_requested(dir.path(), "launch").unwrap());
        assert!(rgb_stop_requested(dir.path(), "other").is_err());
    }
}
