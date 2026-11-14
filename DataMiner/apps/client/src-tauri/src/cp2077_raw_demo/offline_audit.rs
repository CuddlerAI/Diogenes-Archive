//! Read-only replay of current validators against an already finished local run.
//! The separate report cannot upgrade, re-arm or rewrite the original attempt.
use super::*;

fn component<T: Serialize>(result: anyhow::Result<T>) -> Value {
    match result {
        Ok(value) => json!({"passed":true,"details":value}),
        Err(error) => json!({"passed":false,"error":format!("{error:#}")}),
    }
}

fn recorded_contract(session: &Path, launch: &str) -> anyhow::Result<(CaptureContract, bool)> {
    let mut contract = capture_contract(
        depth_lab::ISSUE735_LOCAL_CONTRACT,
        depth_lab::ISSUE735_LOCAL_DURATION_MS,
    )?;
    let even_cfr = selected_feedback::recorded_even_cfr_request(session, launch)?;
    if even_cfr {
        let arm: Value =
            serde_json::from_slice(&read_bounded_file(&session.join(ARM_FILE), 128 * 1024)?)?;
        ensure!(
            arm["manual_stop"] == false,
            "offline timed replay requires a timed even-CFR Arm"
        );
        contract.target_depth_hz = depth_lab::cp2077_raw_feedback::TARGET_DEPTH_HZ;
        contract.max_depth_frames = 1024;
    }
    Ok((contract, even_cfr))
}

pub fn audit_existing_session(session: &Path, output: &Path) -> anyhow::Result<()> {
    let session = validate_session_dir(session)?;
    ensure!(
        output.is_absolute() && !output.exists(),
        "offline audit output must be new and absolute"
    );
    let output_parent = output
        .parent()
        .context("offline output has no parent")?
        .canonicalize()?;
    ensure!(
        !output_parent
            .to_string_lossy()
            .to_lowercase()
            .starts_with(&session.to_string_lossy().to_lowercase()),
        "offline audit must be written outside its source session"
    );
    ensure!(
        session.join(STOP_FILE).is_file() && session.join("coordinator-exit.json").is_file(),
        "offline audit requires a completed coordinated attempt"
    );
    let plan: Value = serde_json::from_slice(&read_bounded_file(
        &session.join(ACTION_PLAN_FILE),
        MAX_JSONL_BYTES,
    )?)?;
    ensure!(
        matches!(
            plan["schema"].as_str(),
            Some("dm-cp2077-raw-demo-action-plan/v1") | Some(ACTION_PLAN_SCHEMA)
        ),
        "unknown source action-plan version"
    );
    ensure!(
        plan["duration_ms"].as_u64() == Some(depth_lab::ISSUE735_LOCAL_DURATION_MS)
            && plan["input_provenance"].as_str() == Some(MANUAL_INPUT_PROVENANCE)
            && plan["route"].as_str() == Some("straight_out_and_back_with_terminal_correction"),
        "offline replay only supports the timed hardware-input 64-second walk"
    );
    if plan["schema"].as_str() == Some(ACTION_PLAN_SCHEMA) {
        ensure!(
            matches!(
                plan["input_validation_policy"].as_str(),
                Some("fixed_view_ws/v2") | Some(FIXED_VIEW_INPUT_POLICY)
            ),
            "source input policy mismatch"
        );
    }
    let launch = plan["process_launch_id"]
        .as_str()
        .context("source plan has no launch identity")?;
    validate_launch_id(launch)?;
    let (contract, even_cfr) = recorded_contract(&session, launch)?;
    let telemetry_bytes = read_bounded_file(&session.join(TELEMETRY_FILE), MAX_JSONL_BYTES)?;
    validate_depth_telemetry(&telemetry_bytes, launch, contract)?;
    let segment = session
        .join("rgb-recorder")
        .join(format!("raw-rgb-{launch}"))
        .join("runs/r0000/segments/00000");
    let meta_path = segment.join("meta.json");
    let meta: Value = serde_json::from_slice(&read_bounded_file(&meta_path, MAX_JSONL_BYTES)?)?;
    ensure!(
        meta["width"] == 1920
            && meta["height"] == 1080
            && meta["fps_num"] == 30
            && meta["fps_den"] == 1,
        "offline projection policy requires native 1920x1080@30"
    );
    let start = meta["qpc_start"].as_i64().context("missing start")?;
    let end = meta["qpc_end"].as_i64().context("missing end")?;
    let marker_rows: Vec<Value> = read_bounded_jsonl(&session.join(ACTION_MARKERS_FILE), false)?;
    ensure!(
        marker_rows.len() == contract.steps.len(),
        "incomplete action markers"
    );
    let frequency = marker_rows[0]["qpc_frequency"]
        .as_i64()
        .context("missing marker frequency")?;
    let anchor = marker_rows[0]["scheduled_qpc"]
        .as_i64()
        .context("missing anchor")?;
    let mut cues = Vec::new();
    for (index, (row, step)) in marker_rows.iter().zip(contract.steps).enumerate() {
        let scheduled = scheduled_action_qpc(anchor, frequency, step.offset_ms)?;
        let actual = row["actual_cue_qpc"]
            .as_i64()
            .context("missing actual cue")?;
        ensure!(
            row["schema"].as_str() == Some(ACTION_MARKER_SCHEMA)
                && row["process_launch_id"].as_str() == Some(launch)
                && row["marker_id"].as_str() == Some(step.marker_id)
                && row["offset_ms"].as_u64() == Some(step.offset_ms)
                && row["qpc_frequency"].as_i64() == Some(frequency)
                && row["scheduled_qpc"].as_i64() == Some(scheduled)
                && row["input_provenance"].as_str() == Some(MANUAL_INPUT_PROVENANCE)
                && actual >= scheduled
                && duration_ms(scheduled, actual, frequency)? <= MAX_ACTION_CUE_DELAY_MS,
            "invalid source action marker {index}"
        );
        cues.push(ActualActionCue {
            step_index: index,
            scheduled_qpc: scheduled,
            actual_qpc: actual,
        });
    }
    let pose: Value = serde_json::from_slice(&read_bounded_file(
        &session.join(POSE_REPORT_FILE),
        MAX_JSONL_BYTES,
    )?)?;
    let render = camera_report::read(&session.join(RENDER_CAMERA_REPORT_FILE))?;
    let depth: Vec<RawDepthPairingRow> =
        read_bounded_jsonl(&session.join("raw-depth-index.jsonl"), false)?;
    let events_path = segment.join("events.jsonl.zst");
    let events: Vec<RawInputEvent> = read_bounded_jsonl(&events_path, true)?;
    let player = component(audit_player_pose(
        &pose, launch, start, end, frequency, &cues, contract, false,
    ));
    let camera = component(audit_render_camera(
        &render, launch, start, end, &depth, contract,
    ));
    let input = component(audit_raw_input(
        &events_path,
        &meta_path,
        &segment.join("sha256.json"),
        start,
        end,
        frequency,
        required_u64(&meta, "n_frames")?,
        meta["events"]["lines"]
            .as_u64()
            .context("missing event count")?,
        meta["health"]["input_overflow"]
            .as_u64()
            .context("missing input health")?,
        &cues,
        contract,
        false,
        standalone_requires_mouse_movement(contract, false),
    ));
    let outbound = component(action_hold_ms(
        &events,
        87,
        cues[1].actual_qpc,
        cues[2].actual_qpc,
        frequency,
        contract,
        FIXED_VIEW_CUE_TOLERANCE_MS,
    ));
    let returning = component(action_hold_ms(
        &events,
        83,
        cues[3].actual_qpc,
        cues[4].actual_qpc,
        frequency,
        contract,
        FIXED_VIEW_CUE_TOLERANCE_MS,
    ));
    let pairing = even_cfr.then(|| {
        component((|| {
            let report =
                validate_rgb_depth_pairing(&session, &segment.join("frames.jsonl.zst"), frequency)?;
            ensure!(
                report.schema == depth_lab::cp2077_raw_feedback::PAIRING_SCHEMA
                    && report.even_cfr.is_some()
                    && report.passed,
                "even-CFR exact pairing failed: {}",
                report.blockers.join(", ")
            );
            Ok(report)
        })())
    });
    let mut hashes = std::collections::BTreeMap::new();
    for path in [
        session.join(ACTION_PLAN_FILE),
        session.join(ACTION_MARKERS_FILE),
        session.join(POSE_REPORT_FILE),
        session.join(RENDER_CAMERA_REPORT_FILE),
        session.join("raw-depth-index.jsonl"),
        session.join(TELEMETRY_FILE),
        session.join("coordinator-exit.json"),
        meta_path,
        events_path,
        segment.join("sha256.json"),
    ] {
        hashes.insert(relative_path(&session, &path)?, sha256_file(&path)?);
    }
    if even_cfr {
        for path in [
            session.join(ARM_FILE),
            session.join(depth_lab::cp2077_raw_feedback::SUMMARY_FILE),
            session.join(WATERMARK_SAMPLES_FILE),
            segment.join("frames.jsonl.zst"),
        ] {
            hashes.insert(relative_path(&session, &path)?, sha256_file(&path)?);
        }
    }
    let mut report = json!({
        "schema": if even_cfr { "dm-cp2077-offline-validation-replay/v2" }
                  else { "dm-cp2077-offline-validation-replay/v1" }, "process_launch_id":launch,
        "source_action_plan_schema":plan["schema"],
        "source_input_policy":plan["input_validation_policy"],
        "replayed_input_policy":FIXED_VIEW_INPUT_POLICY,
        "action_cue_tolerance_ms":FIXED_VIEW_CUE_TOLERANCE_MS,
        "camera":camera,"player":player,"input":input,"outbound_hold":outbound,"return_hold":returning,
        "source_sha256":hashes,
        "scope":"Current local camera/player/input validators, plus required-slot pairing for explicit even-CFR; not a new capture, pixel audit or delivery approval",
        "source_result_upgraded":false,"source_modified":false,"recording_ready":false,"non_trainable":true
    });
    if let Some(pairing) = pairing {
        report["even_cfr_pairing"] = pairing;
    }
    write_json_create_new(output, &report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_even_cfr_replay_uses_recorded_15_hz_without_weakening_legacy_contract() {
        let dir = tempfile::tempdir().unwrap();
        let launch = "0123456789abcdef0123456789abcdef";
        let (old, selected) = recorded_contract(dir.path(), launch).unwrap();
        assert!(!selected);
        assert_eq!(old.target_depth_hz, 16);
        let telemetry = json!({"schema":TELEMETRY_SCHEMA,"process_launch_id":launch,
            "capture_contract":"issue735_local","duration_ms":64000,"manual_stop":false,
            "target_depth_hz":15,"submitted_frames":960,"written_frames":960,
            "writer_queue_dropped":0,"present_stamp_dropped":0,"capture_pool_dropped":0,
            "fatal_error":null,"non_trainable":true,"production_certified":false,
            "upload_allowed":false,"deliverable":false,"recording_ready":false,"formal_start_run_used":false});
        let bytes = serde_json::to_vec(&telemetry).unwrap();
        assert!(validate_depth_telemetry(&bytes, launch, old).is_err());
        let arm = selected_feedback::review_tests::arm(launch);
        fs::write(dir.path().join(ARM_FILE), serde_json::to_vec(&arm).unwrap()).unwrap();
        let (new, selected) = recorded_contract(dir.path(), launch).unwrap();
        assert!(selected);
        assert_eq!(
            (
                new.target_depth_hz,
                new.min_depth_frames,
                new.max_depth_frames
            ),
            (15, 900, 1024)
        );
        assert_eq!(
            (new.duration_ms, new.min_duration_ms, new.max_duration_ms),
            (64000, 63000, 65000)
        );
        validate_depth_telemetry(&bytes, launch, new).unwrap();
        let mut manual = arm;
        manual["manual_stop"] = json!(true);
        fs::write(
            dir.path().join(ARM_FILE),
            serde_json::to_vec(&manual).unwrap(),
        )
        .unwrap();
        assert!(recorded_contract(dir.path(), launch).is_err());
    }
}
