use super::*;

pub(crate) fn arm(launch: &str) -> Value {
    json!({"schema":"dm-cp2077-raw-depth-demo-arm-request/v2","process_launch_id":launch,
        "capture_contract":"issue735_local","duration_ms":64000,"target_depth_hz":15,"manual_stop":false,
        "selected_present":{"schema":contract::REQUEST_SCHEMA,"feedback_schema":4,
            "depth_selection":contract::DEPTH_SELECTION,"holstered":true,"non_trainable":true,
            "bootstrap":{"client_process_id":1234,"nonce":"0123456789abcdef0123456789abcdef",
                "resource_epoch":1,"queue_capacity":512,"ring_capacity":96,"max_feedback_lag_presents":88,
                "guard_slots":8,"width":1920,"height":1080,"bytes_per_pixel":4,
                "vram_limit_bytes":1073741824u64,"vram_reserve_bytes":268435456}}})
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn fixture(dir: &Path) -> (PathBuf, Value, Vec<Value>) {
    let launch = "0123456789abcdef0123456789abcdef";
    let segment = dir.join(format!(
        "rgb-recorder/raw-rgb-{launch}/runs/r0000/segments/00000"
    ));
    fs::create_dir_all(&segment).unwrap();
    let frames = [
        json!({"session_frame":0,"present_seq":100,"source_qpc":1100,"duplicate":false}),
        json!({"session_frame":1,"present_seq":101,"source_qpc":1200,"duplicate":false}),
        json!({"session_frame":2,"present_seq":101,"source_qpc":1200,"duplicate":true}),
    ];
    let lines = frames.iter().map(|v| format!("{v}\n")).collect::<String>();
    let frames_path = segment.join("frames.jsonl.zst");
    fs::write(
        &frames_path,
        zstd::stream::encode_all(lines.as_bytes(), 1).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("raw-depth-index.jsonl"),
        "{\"ordinal\":0,\"present_seq\":100,\"present_qpc\":1000}\n{\"ordinal\":1,\"present_seq\":101,\"present_qpc\":1100}\n").unwrap();
    write_json(&dir.join(ARM_FILE), &arm(launch));
    write_json(
        &segment.join("meta.json"),
        &json!({"fps_num":30,"fps_den":1,"width":1920,"height":1080,"n_frames":3}),
    );
    let summary = json!({"schema":contract::SUMMARY_SCHEMA,"process_launch_id":launch,
        "depth_selection":contract::DEPTH_SELECTION,"feedback_schema":4,"generation":1,"resource_epoch":1,
        "non_trainable":true,"recording_ready":false,"sealed":true,"fault":null,
        "selected_rgb_frames":3,"required_depth_rgb_frames":2,"skipped_depth_rgb_frames":1,
        "distinct_selected_depth":2,"readback_submitted":2,"readback_completed":2,"written_depth_frames":2,
        "aliases":0,"unresolved":0,"decision_queue_depth":0,"ack_queue_depth":0});
    write_json(&dir.join(contract::SUMMARY_FILE), &summary);
    let samples = [100, 101, 101]
        .iter()
        .enumerate()
        .map(|(i, &seq)| {
            let encoded = depth_lab::watermark::encode(seq, 0, 0);
            let means: Vec<u8> = (0..depth_lab::watermark::TOTAL_BLOCKS)
                .map(|block| {
                    let left = block * depth_lab::watermark::BLOCK_PX;
                    u8::from(encoded.white.iter().any(|r| r.left == left)) * 255
                })
                .collect();
            json!({"session_frame":i,"block_means":means})
        })
        .collect::<Vec<_>>();
    write_samples(dir, &samples);
    (frames_path, summary, samples)
}

fn write_samples(dir: &Path, samples: &[Value]) {
    fs::write(
        dir.join(WATERMARK_SAMPLES_FILE),
        samples.iter().map(|s| format!("{s}\n")).collect::<String>(),
    )
    .unwrap();
}

#[test]
fn native_pairing_accepts_odd_inheritance_and_binds_recorded_policy() {
    let dir = tempfile::tempdir().unwrap();
    let (frames, _, _) = fixture(dir.path());
    let report = validate_rgb_depth_pairing(dir.path(), &frames, 1_000_000).unwrap();
    assert!(report.passed);
    assert_eq!(report.schema, contract::PAIRING_SCHEMA);
    let map = report.even_cfr.unwrap();
    assert_eq!(map["mapping"]["bindings"][1]["session_frame"], 2);
    assert_eq!(map["mapping"]["bindings"][1]["present_seq"], 101);
    assert_eq!(
        map["arm_request_sha256"],
        sha256_file(&dir.path().join(ARM_FILE)).unwrap()
    );
    for (key, value) in [
        ("schema", json!(ARM_SCHEMA)),
        ("target_depth_hz", json!(16)),
        (
            "process_launch_id",
            json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ),
    ] {
        let mut bad = arm("0123456789abcdef0123456789abcdef");
        bad[key] = value;
        write_json(&dir.path().join(ARM_FILE), &bad);
        assert!(
            validate_rgb_depth_pairing(dir.path(), &frames, 1_000_000).is_err(),
            "{key}"
        );
    }
}

#[test]
fn native_pairing_rejects_missing_completion_state_and_ambiguous_watermark_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (frames, mut summary, samples) = fixture(dir.path());
    summary.as_object_mut().unwrap().remove("fault");
    write_json(&dir.path().join(contract::SUMMARY_FILE), &summary);
    assert!(validate_rgb_depth_pairing(dir.path(), &frames, 1_000_000).is_err());
    summary["fault"] = Value::Null;
    write_json(&dir.path().join(contract::SUMMARY_FILE), &summary);
    let mut duplicate = samples.clone();
    duplicate.insert(0, json!({"session_frame":0,"block_means":[]}));
    write_samples(dir.path(), &duplicate);
    assert!(validate_rgb_depth_pairing(dir.path(), &frames, 1_000_000).is_err());
    write_samples(dir.path(), &samples[..2]);
    let report = validate_rgb_depth_pairing(dir.path(), &frames, 1_000_000).unwrap();
    assert!(!report.passed && report.even_cfr.is_none());
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("required watermark missing"))
    );
}

#[test]
fn legacy_pairing_keeps_nonzero_session_frame_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (frames, _, mut samples) = fixture(dir.path());
    fs::remove_file(dir.path().join(contract::SUMMARY_FILE)).unwrap();
    let mut rgb: Vec<Value> = read_bounded_jsonl(&frames, true).unwrap();
    for row in rgb.iter_mut().chain(samples.iter_mut()) {
        row["session_frame"] = json!(row["session_frame"].as_u64().unwrap() + 120);
    }
    let lines = rgb.iter().map(|r| format!("{r}\n")).collect::<String>();
    fs::write(
        &frames,
        zstd::stream::encode_all(lines.as_bytes(), 1).unwrap(),
    )
    .unwrap();
    write_samples(dir.path(), &samples);
    let report = validate_rgb_depth_pairing(dir.path(), &frames, 1_000_000).unwrap();
    assert!(report.passed && report.even_cfr.is_none());
    assert_eq!(report.schema, PAIRING_SCHEMA);
}

#[test]
fn native_pairing_full_64s_shape_keeps_aliases_and_rejects_one_missing_even_watermark() {
    // Synthetic index/ROI evidence only. No game, MP4, depth pixels, clock modification,
    // or acceptance claim. Exercise native compressed-index reading and v3 pairing
    // at 1,920 RGB slots, including low16 wrap and odd -> even inheritance.
    let dir = tempfile::tempdir().unwrap();
    let (frames_path, mut summary, _) = fixture(dir.path());
    let mut present = 65_000u64;
    let mut required = Vec::new();
    let mut depth = Vec::new();
    for pair in 0..960u64 {
        if pair % 16 != 15 {
            present += 1;
            let qpc = 10_000_000 + pair.saturating_mul(2).saturating_sub(1) * 33_333;
            depth.push(json!({"ordinal":depth.len(),"present_seq":present,"present_qpc":qpc}));
        }
        required.push(depth.last().unwrap().clone());
    }
    let mut rgb = Vec::new();
    let mut samples = Vec::new();
    for frame in 0..1920usize {
        let source = &required[(frame.div_ceil(2)).min(959)];
        let seq = source["present_seq"].as_u64().unwrap();
        let qpc = source["present_qpc"].as_u64().unwrap() + 10;
        let duplicate = rgb
            .last()
            .is_some_and(|row: &Value| row["present_seq"] == seq);
        rgb.push(
            json!({"session_frame":frame,"present_seq":seq,"source_qpc":qpc,"duplicate":duplicate}),
        );
        let encoded = depth_lab::watermark::encode(seq as u16, 0, 0);
        let means: Vec<u8> = (0..depth_lab::watermark::TOTAL_BLOCKS)
            .map(|block| {
                u8::from(
                    encoded
                        .white
                        .iter()
                        .any(|r| r.left == block * depth_lab::watermark::BLOCK_PX),
                ) * 255
            })
            .collect();
        samples.push(json!({"session_frame":frame,"block_means":means}));
    }
    fs::write(
        &frames_path,
        zstd::stream::encode_all(
            rgb.iter()
                .map(|r| format!("{r}\n"))
                .collect::<String>()
                .as_bytes(),
            1,
        )
        .unwrap(),
    )
    .unwrap();
    fs::write(
        dir.path().join("raw-depth-index.jsonl"),
        depth.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    write_samples(dir.path(), &samples);
    write_json(
        &frames_path.with_file_name("meta.json"),
        &json!({"fps_num":30,"fps_den":1,"width":1920,"height":1080,"n_frames":1920}),
    );
    for (key, value) in [
        ("selected_rgb_frames", 1920),
        ("required_depth_rgb_frames", 960),
        ("skipped_depth_rgb_frames", 960),
        ("distinct_selected_depth", 900),
        ("readback_submitted", 900),
        ("readback_completed", 900),
        ("written_depth_frames", 900),
        ("aliases", 60),
    ] {
        summary[key] = json!(value);
    }
    write_json(&dir.path().join(contract::SUMMARY_FILE), &summary);
    let report = validate_rgb_depth_pairing(dir.path(), &frames_path, 1_000_000).unwrap();
    assert!(report.passed, "{:?}", report.blockers);
    assert_eq!(report.schema, contract::PAIRING_SCHEMA);
    let mapping = report.even_cfr.unwrap();
    assert_eq!(mapping["mapping"]["distinct_depth_frames"], 900);
    assert_eq!(
        mapping["mapping"]["bindings"].as_array().unwrap().len(),
        960
    );
    assert!(required.last().unwrap()["present_seq"].as_u64().unwrap() > 65_536);
    assert_eq!(
        mapping["mapping"]["bindings"][15]["depth_ordinal"],
        mapping["mapping"]["bindings"][14]["depth_ordinal"]
    );
    // The same depth identity is still visible in an adjacent RGB slot, so overall
    // depth-row coverage alone would hide the lost REQUIRED even-slot evidence.
    samples.remove(1534);
    write_samples(dir.path(), &samples);
    let rejected = validate_rgb_depth_pairing(dir.path(), &frames_path, 1_000_000).unwrap();
    assert!(!rejected.passed && rejected.even_cfr.is_none());
    assert!(
        rejected
            .blockers
            .iter()
            .any(|b| b.contains("required watermark missing/conflicting at frame 1534"))
    );
}
