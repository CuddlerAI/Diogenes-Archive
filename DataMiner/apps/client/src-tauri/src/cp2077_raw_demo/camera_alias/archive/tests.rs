use super::*;
use depth_lab::d3d12_camera_alias::CopyCommandIdentity;
use depth_lab::{
    D3d12DescriptorTableBinding, D3d12DrawRelation, D3d12MatrixSourceBinding, MatrixEncoding,
    MatrixLayout, MatrixRole,
};

fn row(present: u64, draw: u32, root: u32, role: MatrixRole) -> Observation {
    Observation {
        sample: Sample {
            present,
            source: D3d12MatrixSourceBinding {
                descriptor_table: D3d12DescriptorTableBinding {
                    root_signature_sha256: format!("sha256:{}", "a".repeat(64)),
                    root_parameter_index: root,
                    descriptor_range_index: 0,
                    descriptor_offset_in_range: 1,
                    descriptor_count: 4,
                    base_shader_register: 0,
                    register_space: 0,
                },
                cbv_resource_offset: 0,
                declared_cbv_bytes: 1024,
                matrix_offset: if role == MatrixRole::View { 0 } else { 128 },
                encoding: MatrixEncoding::F32LittleEndian,
                layout: MatrixLayout::RowMajor,
                transpose: true,
                role,
                draw_relation: D3d12DrawRelation::ExactSelectedDepthDsvDraw,
            },
            matrix: [[present as f64; 4]; 4],
            receipt: Some(CopyReceipt {
                schema: "dm-d3d12-executed-cbv-copy/v2".into(),
                copy_submission: present * 100 + u64::from(draw),
                queue_tag: 1,
                destination_source_tag: 10,
                destination_content_generation: 0,
                destination_offset: 0,
                source_source_tag: 20,
                source_content_generation: 1,
                source_offset: u64::from(draw) * 256,
                descriptor_generation: 1 + u64::from(root),
                copied_bytes: 256,
                bytes_sha256: format!("sha256:{present:064x}"),
                copy_command: Some(CopyCommandIdentity {
                    command_list_tag: 2,
                    command_list_generation: present,
                    copy_ordinal: u64::from(draw),
                }),
            }),
        },
        draw: DrawIdentity {
            present_qpc: present as i64 * 1000,
            queue_tag: 1,
            command_list_tag: 2,
            command_list_generation: present,
            selected_depth_resource_tag: 3,
            selected_depth_resource_epoch: 1,
            selected_depth_descriptor_generation: 1,
            draw_ordinal: draw,
        },
    }
}

fn feed(camera: &mut ArchiveCamera, present: u64, draws: u32) -> anyhow::Result<()> {
    for draw in 1..=draws {
        for root in [0, 18] {
            for role in [MatrixRole::View, MatrixRole::Projection] {
                camera.push(row(present, draw, root, role))?;
            }
        }
    }
    Ok(())
}

fn draw_json(present: u64, ordinal: u32, writer: bool) -> Value {
    let first = row(present, ordinal, 0, MatrixRole::View);
    let mut draw = serde_json::to_value(&first.draw).unwrap();
    draw["present_seq"] = json!(present);
    draw["physical_source"] =
        json!("copy_buffer_region_root_signature_declared_cbv_at_selected_dsv_draw");
    draw["pipeline_depth_enable"] = json!(true);
    draw["pipeline_depth_write"] = json!(writer);
    draw["pass_context"] = json!({"schema":"dm-d3d12-draw-pass/v1","non_trainable":true,"dsv_flags":0,
        "viewport":{"count":1,"single_bits":([0.0f32,0.0,1920.0,1080.0,0.0,1.0].map(f32::to_bits))},
        "pipeline":{"depth_write_mask":u32::from(writer),"dsv_format":20}});
    let mut matrices = Vec::new();
    for root in [0, 18] {
        for role in [MatrixRole::View, MatrixRole::Projection] {
            let mut r = row(present, ordinal, root, role);
            // Non-writers may use another CBV payload/range, but must agree numerically.
            if !writer {
                r.sample.receipt.as_mut().unwrap().bytes_sha256 =
                    format!("sha256:{}", "b".repeat(64));
            }
            matrices.push(json!({"matrix_kind":if role==MatrixRole::View {"view"} else {"projection"},
                "matrix":r.sample.matrix,"stable_source":r.sample.source,"copy_receipt":r.sample.receipt,
                "physical_source":"copy_buffer_region_root_signature_declared_cbv","source_tag":10,
                "root_parameter_index":root,"cbv_resource_offset":0,
                "matrix_offset":if role==MatrixRole::View {0} else {128},"transpose":true,
                "configured_vertical_fov_matches":true,"projection_a":-0.000001,"projection_b":0.02}));
        }
    }
    draw["matrices"] = json!(matrices);
    draw
}

#[test]
fn non_writers_remain_checked_witnesses_not_source_members() {
    let mut camera = ArchiveCamera::default();
    for p in 1..=130 {
        camera.push_draw(&draw_json(p, 1, true)).unwrap();
        camera.push_draw(&draw_json(p, 2, false)).unwrap();
    }
    assert_eq!(camera.finish(130).unwrap().retained_views, 520);
    for change in 0..5 {
        let mut camera = ArchiveCamera::default();
        for p in 1..=120 {
            camera.push_draw(&draw_json(p, 1, true)).unwrap();
        }
        let result = (|| {
            camera.push_draw(&draw_json(121, 1, true))?;
            let mut witness = draw_json(121, 2, false);
            match change {
                0 => witness["matrices"][0]["matrix"][0][0] = json!(122.0),
                1 => witness["pipeline_depth_write"] = Value::Null,
                2 => {
                    witness["pass_context"]["viewport"]["single_bits"][5] = json!(0.5f32.to_bits())
                }
                3 => witness["matrices"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|m| m["matrix_kind"] == "view"),
                _ => witness["matrices"][0]["copy_receipt"]["copied_bytes"] = json!(32),
            }
            camera.push_draw(&witness)?;
            camera.finish(121)
        })();
        assert!(result.is_err(), "witness change {change}");
    }
    let mut camera = ArchiveCamera::default();
    camera.push_draw(&draw_json(1, 1, false)).unwrap();
    assert!(camera.finish(1).is_err());
}

#[test]
fn full_recording_exceeds_old_total_bound_without_widening_a_batch() {
    let mut camera = ArchiveCamera::default();
    for p in 1..=960 {
        feed(&mut camera, p, 20).unwrap();
    }
    let checked = camera.finish(960).unwrap();
    assert_eq!(checked.views.len(), 960);
    assert_eq!(checked.projections.len(), 960);
    assert_eq!(checked.retained_views, 38_400);
    assert_eq!(checked.retained_projections, 38_400);
    assert_eq!(checked.view_sources.len(), 2);
    assert_eq!(checked.projection_sources.len(), 2);
    assert_eq!(checked.views[&960], [[960.0; 4]; 4]);
}

#[test]
fn late_corruption_and_source_changes_cannot_hide_after_warmup() {
    for change in 0..6 {
        let mut camera = ArchiveCamera::default();
        for p in 1..=120 {
            feed(&mut camera, p, 1).unwrap();
        }
        let result = (|| {
            let mut view = row(121, 1, 0, MatrixRole::View);
            match change {
                0 => view.sample.source.descriptor_table.root_parameter_index = 19,
                1 => {
                    view.sample
                        .receipt
                        .as_mut()
                        .unwrap()
                        .destination_content_generation += 1
                }
                2 => view.sample.receipt.as_mut().unwrap().copy_submission = 101,
                3 => view.sample.matrix[0][0] = f64::NAN,
                _ => {}
            }
            camera.push(view)?;
            if change != 4 {
                let mut projection = row(121, 1, 0, MatrixRole::Projection);
                if change == 5 {
                    projection.sample.receipt.as_mut().unwrap().bytes_sha256 =
                        format!("sha256:{}", "b".repeat(64));
                }
                camera.push(projection)?;
            }
            camera.finish(121)
        })();
        assert!(result.is_err(), "change {change}");
    }
}

#[test]
fn conflicting_late_draws_are_all_checked() {
    let mut camera = ArchiveCamera::default();
    for p in 1..=121 {
        feed(&mut camera, p, 1).unwrap();
    }
    let mut bad = row(121, 2, 0, MatrixRole::View);
    bad.sample.matrix[0][0] += 1.0;
    camera.push(bad).unwrap();
    camera.push(row(121, 2, 0, MatrixRole::Projection)).unwrap();
    assert!(camera.finish(121).is_err());
}

#[test]
fn insufficient_evidence_regression_and_coverage_still_fail() {
    let mut camera = ArchiveCamera::default();
    for p in 1..120 {
        feed(&mut camera, p, 1).unwrap();
    }
    assert!(camera.finish(119).is_err());
    let mut camera = ArchiveCamera::default();
    feed(&mut camera, 2, 1).unwrap();
    assert!(feed(&mut camera, 1, 1).is_err());
    let mut camera = ArchiveCamera::default();
    for p in 1..=120 {
        feed(&mut camera, p, 1).unwrap();
    }
    assert!(camera.finish(150).is_err());
}

#[test]
fn per_present_and_warmup_limits_remain_bounded() {
    let mut camera = ArchiveCamera::default();
    for _ in 0..MAX_BATCH_OBSERVATIONS {
        camera.push(row(1, 1, 0, MatrixRole::View)).unwrap();
    }
    assert!(camera.push(row(1, 1, 0, MatrixRole::View)).is_err());
    let mut camera = ArchiveCamera::default();
    let result = (|| {
        for p in 1..=120 {
            for _ in 0..300 {
                camera.push(row(p, 1, 0, MatrixRole::View))?;
                camera.push(row(p, 1, 0, MatrixRole::Projection))?;
            }
        }
        camera.finish(120)
    })();
    assert!(result.err().unwrap().to_string().contains("warmup exceeds"));
}

#[test]
#[ignore = "requires an explicit immutable local session and fresh external output"]
fn replay_completed_archived_camera_and_input() {
    fn component<T: Serialize>(value: anyhow::Result<T>) -> Value {
        match value {
            Ok(details) => json!({"passed":true,"details":details}),
            Err(error) => json!({"passed":false,"error":format!("{error:#}")}),
        }
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(std::env::var("CP2077_POST_CAMERA_REPLAY").expect("explicit replay manifest"))
            .unwrap(),
    )
    .unwrap();
    let session = Path::new(manifest["session"].as_str().unwrap());
    let output = Path::new(manifest["output"].as_str().unwrap());
    assert!(output.is_absolute() && !output.exists());
    assert!(
        !output
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .starts_with(session.canonicalize().unwrap())
    );
    let verify = || {
        for row in manifest["files"].as_array().unwrap() {
            assert_eq!(
                sha256_file(&session.join(row["path"].as_str().unwrap())).unwrap(),
                format!("sha256:{}", row["sha256"].as_str().unwrap())
            );
        }
    };
    verify();
    let launch = manifest["launch_id"].as_str().unwrap();
    let segment = session.join(format!(
        "rgb-recorder/raw-rgb-{launch}/runs/r0000/segments/00000"
    ));
    let meta: Value =
        serde_json::from_slice(&fs::read(segment.join("meta.json")).unwrap()).unwrap();
    let start = meta["qpc_start"].as_i64().unwrap();
    let end = meta["qpc_end"].as_i64().unwrap();
    let report = camera_report::read(&session.join(RENDER_CAMERA_REPORT_FILE)).unwrap();
    let depth: Vec<RawDepthPairingRow> =
        read_bounded_jsonl(&session.join("raw-depth-index.jsonl"), false).unwrap();
    let contract = capture_contract(
        depth_lab::ISSUE735_LOCAL_CONTRACT,
        depth_lab::ISSUE735_LOCAL_DURATION_MS,
    )
    .unwrap();
    let camera = component(audit_render_camera(
        &report, launch, start, end, &depth, contract,
    ));
    let markers: Vec<Value> =
        read_bounded_jsonl(&session.join(ACTION_MARKERS_FILE), false).unwrap();
    assert_eq!(markers.len(), contract.steps.len());
    let freq = markers[0]["qpc_frequency"].as_i64().unwrap();
    let cues = markers
        .iter()
        .enumerate()
        .map(|(step_index, row)| {
            assert_eq!(row["process_launch_id"], launch);
            assert_eq!(row["marker_id"], contract.steps[step_index].marker_id);
            ActualActionCue {
                step_index,
                scheduled_qpc: row["scheduled_qpc"].as_i64().unwrap(),
                actual_qpc: row["actual_cue_qpc"].as_i64().unwrap(),
            }
        })
        .collect::<Vec<_>>();
    let input = component(audit_raw_input(
        &segment.join("events.jsonl.zst"),
        &segment.join("meta.json"),
        &segment.join("sha256.json"),
        start,
        end,
        freq,
        meta["n_frames"].as_u64().unwrap(),
        meta["events"]["lines"].as_u64().unwrap(),
        meta["health"]["input_overflow"].as_u64().unwrap(),
        &cues,
        contract,
        false,
        standalone_requires_mouse_movement(contract, false),
    ));
    let pose: Value =
        serde_json::from_slice(&fs::read(session.join(POSE_REPORT_FILE)).unwrap()).unwrap();
    let player = component(audit_player_pose(
        &pose, launch, start, end, freq, &cues, contract, false,
    ));
    verify();
    write_json_create_new(output,&json!({"schema":"cp2077-post-camera-replay/v1","non_trainable":true,
        "acceptance":false,"raw_modified":false,"camera":camera,"input":input,"player":player,
        "replayed_input_policy":FIXED_VIEW_INPUT_POLICY,
        "action_cue_tolerance_ms":FIXED_VIEW_CUE_TOLERANCE_MS,
        "scope":"Existing native component validators on immutable archived data; no new capture or acceptance"})).unwrap();
    assert_eq!(camera["passed"], true, "{camera}");
    if manifest["expect_input_pass"] == true {
        assert_eq!(input["passed"], true, "{input}");
    }
}
