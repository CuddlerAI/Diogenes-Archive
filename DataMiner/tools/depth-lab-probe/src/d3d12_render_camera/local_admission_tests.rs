//! Read-only replay uses operator-owned evidence; no local identifiers are embedded in source.
use super::*;
use depth_lab::d3d12_camera_local::{MIN_PRESENTS, admit};
use serde_json::{Value, json};

/// Replays authored stable sources through the normal production resolver, with fresh physical
/// admission for EACH original launch. A caller must supply the explicit candidate binding;
/// this test neither elects a source nor authorizes the candidate for a catalog.
#[test]
#[ignore = "requires CP2077_BOUND_CAMERA_REPLAY with reports, binding and a new output path"]
fn replay_bound_holstered_camera_on_original_reports() {
    use std::io::Write;
    let input = std::env::var("CP2077_BOUND_CAMERA_REPLAY").unwrap();
    let manifest: Value = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let binding_bytes = std::fs::read(manifest["binding"].as_str().unwrap()).unwrap();
    let binding: depth_lab::D3d12CameraBinding = serde_json::from_slice(&binding_bytes).unwrap();
    assert!(binding.holstered_camera.is_some());
    let mut results = Vec::new();
    let reports = manifest["reports"].as_array().unwrap();
    let exports = manifest["pose_outputs"].as_array();
    if let Some(exports) = exports {
        assert_eq!(exports.len(), reports.len());
    }
    for (index, report) in reports.iter().enumerate() {
        let path = report.as_str().unwrap();
        let export_path =
            exports.map(|paths| std::path::PathBuf::from(paths[index].as_str().unwrap()));
        let mut export = export_path.as_ref().map(|p| {
            assert!(p.is_absolute());
            let parent = p.parent().unwrap().canonicalize().unwrap();
            for original in reports {
                let raw = std::path::Path::new(original.as_str().unwrap())
                    .parent()
                    .unwrap()
                    .canonicalize()
                    .unwrap();
                assert!(
                    !parent.starts_with(raw),
                    "pose export must not modify raw evidence"
                );
            }
            std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(p)
                    .unwrap(),
            )
        });
        let original_hash = Sha256::digest(std::fs::read(path).unwrap());
        let (mut snapshot, expected) = load(path);
        let first = snapshot
            .observations
            .iter()
            .find(|d| expected.contains(&d.present_seq))
            .unwrap();
        let selected = DsvIdentity {
            handle: crate::d3d12_abi::DescriptorHandle(1),
            resource: crate::d3d12_abi::ResourceId(first.selected_depth_resource_tag),
            resource_epoch: first.selected_depth_resource_epoch,
            descriptor_generation: first.selected_depth_descriptor_generation,
        };
        let mut by_present = BTreeMap::<_, Vec<_>>::new();
        for draw in std::mem::take(&mut snapshot.observations) {
            by_present.entry(draw.present_seq).or_default().push(draw);
        }
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        tracker.selected_depth = Some(selected);
        tracker
            .configure_production(
                binding.clone(),
                depth_lab::CameraPoseStreamContext {
                    identity: depth_lab::StreamIdentity {
                        session_id: "offline-replay".into(),
                        run_id: "original-launch".into(),
                        generation: 1,
                    },
                    dsv_id: "original-selected-depth".into(),
                },
                (0, 0),
            )
            .unwrap();
        tracker.snapshot = snapshot;
        let mut poses = 0;
        let mut warmup = 0;
        for &present in &expected {
            tracker.snapshot.observations = by_present.remove(&present).unwrap_or_default();
            let original_matrices: Vec<_> = tracker
                .snapshot
                .observations
                .iter()
                .flat_map(|d| d.matrices.iter())
                .map(|m| m.matrix)
                .collect();
            match tracker
                .take_production_pose(present, (0, 0))
                .unwrap_or_else(|e| panic!("Present {present}: {e}"))
            {
                None => warmup += 1,
                Some((row, copied)) => {
                    assert!(copied);
                    depth_lab::validate_camera_pose_row(&row).unwrap();
                    if let Some(output) = export.as_mut() {
                        serde_json::to_writer(&mut *output, &row).unwrap();
                        output.write_all(b"\n").unwrap();
                    }
                    let depth_lab::CameraPoseProvenance::ResolvedD3d12CameraHook {
                        view_matrix,
                        projection_matrix,
                        ..
                    } = row.provenance
                    else {
                        panic!()
                    };
                    assert!(original_matrices.contains(&*view_matrix));
                    assert!(original_matrices.contains(&*projection_matrix));
                    poses += 1;
                }
            }
            assert!(tracker.snapshot.observations.is_empty());
        }
        assert_eq!(warmup, 119);
        assert_eq!(poses + warmup, expected.len());
        assert_eq!(original_hash, Sha256::digest(std::fs::read(path).unwrap()));
        let pose_export = export_path.map(|p| {
            let mut output = export.take().unwrap();
            output.flush().unwrap();
            output.get_ref().sync_all().unwrap();
            let bytes = std::fs::read(&p).unwrap();
            let root: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let profile = std::fs::read(
                std::path::Path::new(path).with_file_name("raw-depth-demo-profile.json"),
            )
            .unwrap();
            json!({"file":p, "sha256":format!("sha256:{:x}",Sha256::digest(&bytes)),
                "bytes":bytes.len(), "rows":poses, "process_launch_id":root["process_launch_id"],
                "raw_depth_profile_sha256":format!("sha256:{:x}",Sha256::digest(profile))})
        });
        results.push(json!({"source_sha256":format!("sha256:{original_hash:x}"), "exact_copied_presents":expected.len(), "warmup_presents":warmup,"resolved_poses":poses,
            "pose_export":pose_export}));
    }
    assert_eq!(
        binding_bytes,
        std::fs::read(manifest["binding"].as_str().unwrap()).unwrap()
    );
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(manifest["output"].as_str().unwrap())
        .unwrap();
    serde_json::to_writer_pretty(output, &json!({"schema":if exports.is_some(){"cp2077-bound-camera-replay/v2"}else{"cp2077-bound-camera-replay/v1"}, "non_trainable":true,
        "acceptance":false,"production_binding_authorized":false,"binding_sha256":format!("sha256:{:x}",Sha256::digest(binding_bytes)),"runs":results})).unwrap();
}

fn load(path: &str) -> (D3d12RenderCameraSnapshot, BTreeSet<u64>) {
    let root = read_archive_report(path);
    let s = &root["observation"];
    assert_eq!(root["non_trainable"], true);
    let mut snapshot = D3d12RenderCameraSnapshot {
        selected_depth_armed: s["selected_depth_armed"].as_bool().unwrap(),
        blockers: serde_json::from_value(s["blockers"].clone()).unwrap(),
        ..Default::default()
    };
    macro_rules! copy_counts {($($field:ident),*)=>{$(snapshot.$field=s[stringify!($field)].as_u64().unwrap();)*};}
    copy_counts!(
        matrix_candidate_overflow,
        pending_draw_overflow,
        draw_scan_budget_exhausted,
        present_observation_overflow,
        tracker_contention_drops,
        hot_state_drops,
        upload_unmap_snapshot_budget_exhausted,
        exact_cbv_copy_span_budget_exhausted,
        exact_cbv_copy_request_budget_exhausted,
        exact_cbv_copy_shadow_budget_exhausted
    );
    for d in s["observations"].as_array().unwrap() {
        let mut draw: D3d12RenderDrawObservation = serde_json::from_value(d.clone()).unwrap();
        if !d["pass_context"].is_null() {
            let c = &d["pass_context"];
            assert_eq!(c["schema"], "dm-d3d12-draw-pass/v1");
            assert_eq!(c["non_trainable"], true);
            draw.pass_context = Some(draw_pass::DrawPassContext {
                schema: "dm-d3d12-draw-pass/v1",
                non_trainable: true,
                pipeline: serde_json::from_value(c["pipeline"].clone()).unwrap(),
                scissor: serde_json::from_value(c["scissor"].clone()).unwrap(),
                dsv_flags: serde_json::from_value(c["dsv_flags"].clone()).unwrap(),
                viewport: if c["viewport"].is_null() {
                    None
                } else {
                    Some(draw_pass::ViewportObservation {
                        count: c["viewport"]["count"].as_u64().unwrap().try_into().unwrap(),
                        single_bits: serde_json::from_value(c["viewport"]["single_bits"].clone())
                            .unwrap(),
                    })
                },
            });
        }
        snapshot.observations.push(draw);
    }
    let archive = &s["local_archive"];
    assert_eq!(archive.is_null(), root["observation_archive"].is_null());
    if !archive.is_null() {
        assert_eq!(archive["schema"], "dm-cp2077-camera-active-window/v1");
        let count = root["observation_archive"]["archived_draws"]
            .as_u64()
            .unwrap();
        assert_eq!(archive["enqueued_draws"], count);
        assert_eq!(archive["frozen"], true);
        snapshot.local_archive = Some(observation_archive::ArchiveState {
            schema: "dm-cp2077-camera-active-window/v1",
            enqueued_draws: count,
            enqueued_batches: archive["enqueued_batches"].as_u64().unwrap(),
            enqueue_failures: archive["enqueue_failures"].as_u64().unwrap(),
            retired_before_present: archive["retired_before_present"].as_u64().unwrap(),
            frozen: true,
        });
    }
    (
        snapshot,
        copied_presents(&root).expect("complete independent copy evidence is required"),
    )
}

// Keep the admission replay usable with both physical archive formats. This only resolves
// storage references; production source admission still executes above for the original launch.
fn read_archive_report(path: &str) -> Value {
    use depth_lab::cp2077_camera_report::{
        ARCHIVE_BYTES, LEGACY_REPORT_BYTES, REPORT_BYTES, archive, compact,
    };
    let bytes = std::fs::read(path).unwrap();
    assert!(bytes.len() as u64 <= REPORT_BYTES);
    let mut root: Value = serde_json::from_slice(&bytes).unwrap();
    let is_compact = root["schema"] == compact::REPORT_SCHEMA;
    let a = &root["observation_archive"];
    if a.is_null() {
        assert!(!is_compact);
        return root;
    }
    assert_eq!(
        a["schema"],
        if is_compact {
            compact::ARCHIVE_SCHEMA
        } else {
            "dm-cp2077-camera-archive/v1"
        }
    );
    assert_eq!(a["non_trainable"], true);
    assert_eq!(a["durable"], true);
    assert_eq!(a["final_observations_include_archive"], !is_compact);
    assert_eq!(a["file"], "render-camera-draws.jsonl");
    assert_eq!(a["disk_budget_bytes"], ARCHIVE_BYTES);
    assert_eq!(a["active_draw_capacity"], 4096);
    let path = std::path::Path::new(path).with_file_name("render-camera-draws.jsonl");
    assert!(std::fs::metadata(&path).unwrap().len() <= ARCHIVE_BYTES);
    let mut hash = Sha256::new();
    let mut expanded_hash = Sha256::new();
    let mut rows = Vec::new();
    let counts = archive::visit(
        std::io::BufReader::new(std::fs::File::open(path).unwrap()),
        is_compact,
        |line| hash.update(line),
        |raw| {
            expanded_hash.update(raw.get().as_bytes());
            expanded_hash.update(b"\n");
            rows.push(serde_json::from_str::<Value>(raw.get()).map_err(|e| e.to_string())?);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(a["bytes"], counts.bytes);
    assert_eq!(a["archived_draws"], counts.rows);
    assert_eq!(a["sha256"], format!("sha256:{:x}", hash.finalize()));
    if is_compact {
        assert!(
            bytes.len() as u64 <= LEGACY_REPORT_BYTES
                && bytes.len() as u64 + counts.expanded_bytes <= REPORT_BYTES
        );
        assert_eq!(a["encoding"], compact::BLOCK_SCHEMA);
        assert_eq!(a["blocks"], counts.blocks);
        assert_eq!(a["expanded_bytes"], counts.expanded_bytes);
        assert_eq!(
            a["expanded_sha256"],
            format!("sha256:{:x}", expanded_hash.finalize())
        );
        let tail = root["observation"]["observations"].as_array_mut().unwrap();
        assert!(tail.len() <= 4096);
        rows.append(tail);
        root["observation"]["observations"] = Value::Array(rows);
    } else {
        let full = root["observation"]["observations"].as_array().unwrap();
        assert!(full.len() >= rows.len() && full.len() - rows.len() <= 4096);
        assert_eq!(&full[..rows.len()], rows.as_slice());
    }
    root
}

/// Never derive the expected frames from camera rows: an absent camera would erase its own test.
fn copied_presents(root: &Value) -> Result<BTreeSet<u64>, String> {
    let order = &root["observation"]["depth_order"];
    if order["schema"] != "dm-d3d12-depth-order/v1"
        || order["enabled"] != true
        || order["non_trainable"] != true
    {
        return Err("missing independent depth-copy evidence".into());
    }
    for field in [
        "dropped_recorded",
        "dropped_submitted",
        "unknown_list",
        "unknown_order",
        "discarded_unsubmitted",
    ] {
        if order[field].as_u64() != Some(0) {
            return Err(format!("incomplete depth-copy evidence: {field}"));
        }
    }
    let mut expected = BTreeSet::new();
    let mut capture_ids = BTreeSet::new();
    for b in order["boundaries"]
        .as_array()
        .ok_or("missing submitted boundaries")?
    {
        let e = &b["event"];
        if e["kind"] != "copy_candidate" || e["result"] != "recorded" {
            continue;
        }
        let present = e["interval_present_seq"]
            .as_u64()
            .filter(|v| *v != 0)
            .ok_or("recorded copy has no exact Present")?;
        let capture_id = e["capture_id"]
            .as_u64()
            .filter(|v| *v != 0)
            .ok_or("recorded copy has no capture identity")?;
        if !capture_ids.insert(capture_id) || !expected.insert(present) {
            return Err("duplicate capture identity or copied Present".into());
        }
    }
    if expected.is_empty() {
        return Err("no recorded depth copies".into());
    }
    Ok(expected)
}

fn replay(snapshot: &D3d12RenderCameraSnapshot, presents: &BTreeSet<u64>) -> Result<Value, String> {
    let mut views = vec![];
    let mut projections = vec![];
    let mut warm = BTreeSet::new();
    let mut camera = None;
    let mut continuous = 0;
    for present in presents {
        let (v, p) = snapshot
            .local_camera_samples(*present, 1920, 1080)
            .map_err(|e| format!("Present {present}: {e}"))?;
        if let Some(camera) = camera.as_mut() {
            depth_lab::d3d12_camera_local::LocalCamera::observe_present(camera, v, p)
                .map_err(|e| e.to_string())?;
            continuous += 1;
        } else {
            views.extend(v);
            projections.extend(p);
            warm.insert(*present);
            if warm.len() == MIN_PRESENTS {
                camera = Some(
                    admit(
                        std::mem::take(&mut views),
                        std::mem::take(&mut projections),
                        &warm,
                    )
                    .map_err(|e| e.to_string())?,
                );
            }
        }
    }
    let camera = camera.ok_or("fewer than 120 Presents")?;
    Ok(
        json!({"warmup":camera.summary(),"continued_presents":continuous,"total_presents":presents.len()}),
    )
}

#[test]
#[ignore = "requires immutable failed session via CP2077_LOCAL_ADMISSION_REPLAY manifest"]
fn replay_failed_copied_present_must_not_disappear() {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(std::env::var("CP2077_LOCAL_ADMISSION_REPLAY").unwrap()).unwrap(),
    )
    .unwrap();
    let (snapshot, expected) = load(manifest["failed"].as_str().unwrap());
    let error = replay(&snapshot, &expected)
        .expect_err("a copied Present without local camera must fail replay");
    // A stricter source policy may now fail earlier. Independently recheck EVERY
    // copied Present so that the original missing camera cannot disappear again.
    let missing: Vec<_> = expected
        .iter()
        .filter_map(|p| {
            snapshot
                .local_camera_samples(*p, 1920, 1080)
                .err()
                .filter(|e| e.contains("local camera is absent"))
                .map(|e| json!({"present":p,"error":e}))
        })
        .collect();
    assert!(
        !missing.is_empty(),
        "the failed missing-camera copy must remain visible: {error}"
    );
    let report = json!({"schema":"cp2077-local-admission-replay/v2", "non_trainable":true,
        "acceptance":false,"expected_source":"recorded_depth_copies", "expected_presents":expected.len(),
        "rejected":error,"independently_rejected_missing_cameras":missing});
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(manifest["output"].as_str().unwrap())
        .unwrap();
    serde_json::to_writer_pretty(out, &report).unwrap();
}

#[test]
#[ignore = "requires CP2077_LOCAL_ADMISSION_REPLAY manifest with immutable holstered and weapon report paths"]
fn replay_real_holstered_and_weapon_camera_admission() {
    let path = std::env::var("CP2077_LOCAL_ADMISSION_REPLAY").unwrap();
    let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let (good, expected) = load(manifest["holstered"].as_str().unwrap());
    let result =
        replay(&good, &expected).expect("holstered source admission/continuation must pass");
    let (bad, bad_expected) = load(manifest["weapon"].as_str().unwrap());
    let rejected =
        replay(&bad, &bad_expected).expect_err("weapon mixed-depth viewport must fail closed");
    let present = good
        .observations
        .iter()
        .find(|d| d.selected_present_identity_passed && d.copy_before_clear_submitted)
        .unwrap()
        .present_seq;
    for mutation in 0..5 {
        let mut changed = good.clone();
        let draw = changed
            .observations
            .iter_mut()
            .find(|d| {
                d.present_seq == present
                    && d.selected_present_identity_passed
                    && d.copy_before_clear_submitted
            })
            .unwrap();
        match mutation {
            0 => draw.pass_context = None,
            1 => {
                draw.pass_context
                    .as_mut()
                    .unwrap()
                    .viewport
                    .as_mut()
                    .unwrap()
                    .single_bits
                    .as_mut()
                    .unwrap()[4] = 0.9f32.to_bits()
            }
            2 => draw.pass_context.as_mut().unwrap().dsv_flags = None,
            3 => draw.matrices.iter_mut().for_each(|m| m.copy_receipt = None),
            _ => changed.tracker_contention_drops = 1,
        }
        assert!(
            changed.local_camera_samples(present, 1920, 1080).is_err(),
            "mutation {mutation}"
        );
    }
    let report = json!({"schema":"cp2077-local-admission-replay/v2","non_trainable":true,"acceptance":false,
        "expected_source":"recorded_depth_copies",
        "holstered":result,"weapon_rejected":rejected,"negative_mutations_rejected":5});
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(manifest["output"].as_str().unwrap())
        .unwrap();
    serde_json::to_writer_pretty(&mut out, &report).unwrap();
}

#[test]
fn replay_missing_copy_camera_and_incomplete_expected_evidence_fail_closed() {
    let mut root = json!({"observation":{"depth_order":{
        "schema":"dm-d3d12-depth-order/v1", "enabled":true,"non_trainable":true,
        "dropped_recorded":0,"dropped_submitted":0,"unknown_list":0,"unknown_order":0,
        "discarded_unsubmitted":0,"boundaries":[{"event":{"kind":"copy_candidate",
            "result":"recorded","capture_id":1,"interval_present_seq":42}}]}}});
    let expected = copied_presents(&root).unwrap();
    let snapshot = D3d12RenderCameraSnapshot {
        selected_depth_armed: true,
        ..Default::default()
    };
    assert_eq!(
        replay(&snapshot, &expected).unwrap_err(),
        "Present 42: local camera is absent at the exact copied Present"
    );
    root["observation"]["depth_order"]["unknown_order"] = json!(1);
    assert!(copied_presents(&root).is_err());
    root["observation"]["depth_order"]["unknown_order"] = json!(0);
    let first = root["observation"]["depth_order"]["boundaries"][0].clone();
    root["observation"]["depth_order"]["boundaries"] = json!([first.clone(), first]);
    assert!(copied_presents(&root).is_err());
    root["observation"]["depth_order"]["boundaries"][1]["event"]["interval_present_seq"] =
        json!(43);
    assert!(copied_presents(&root).is_err()); // duplicate capture ID across different Presents
    root["observation"]["depth_order"]["boundaries"][1]["event"]["capture_id"] = json!(2);
    assert_eq!(copied_presents(&root).unwrap(), BTreeSet::from([42, 43]));
    root["observation"]["depth_order"]["boundaries"] = json!([]);
    assert!(copied_presents(&root).is_err());
    assert!(copied_presents(&json!({})).is_err());
}

#[test]
#[ignore = "requires immutable warmup_conflict report via CP2077_LOCAL_ADMISSION_REPLAY"]
fn replay_depth_writer_policy_on_raw_warmup_conflict() {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(std::env::var("CP2077_LOCAL_ADMISSION_REPLAY").unwrap()).unwrap(),
    )
    .unwrap();
    let (snapshot, expected) = load(manifest["warmup_conflict"].as_str().unwrap());
    // Never erase the final report's real overflow, even though its first fatal was earlier.
    assert!(snapshot.present_observation_overflow > 0);
    assert!(
        replay(&snapshot, &expected)
            .unwrap_err()
            .contains("lost bounded selected-draw evidence")
    );
    let mut views = vec![];
    let mut projections = vec![];
    for p in &expected {
        let (v, pr) = snapshot.local_camera_sources(*p, 1920, 1080).unwrap();
        views.extend(v);
        projections.extend(pr);
    }
    let camera = admit(views, projections, &expected).unwrap();
    let report = json!({"schema":"cp2077-depth-writer-policy-replay/v1","non_trainable":true,"acceptance":false,
        "scope":"source-policy layer only; complete snapshot remains rejected for original overflow",
        "expected_presents":expected.len(),"original_overflow":snapshot.present_observation_overflow,
        "full_snapshot_rejected":true,"source_layer":camera.summary()});
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(manifest["output"].as_str().unwrap())
        .unwrap();
    serde_json::to_writer_pretty(out, &report).unwrap();
}
