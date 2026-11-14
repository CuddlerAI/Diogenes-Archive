use super::*;

fn hash(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        h.wrapping_mul(0x100_0000_01b3) ^ u64::from(*b)
    });
    format!("sha256:{h:064x}")
}

fn projection(fov: f32) -> Mat4 {
    let x = 1. / (fov.to_radians() / 2.).tan();
    [
        [x, 0., 0., 0.],
        [0., x * 16. / 9., 0., 0.],
        [0., 0., 1.001, 1.],
        [0., 0., -0.1, 0.],
    ]
}

fn pose(present: u64, yaw: f32, local_position: [f32; 3]) -> PoseSample {
    let (s, c) = yaw.sin_cos();
    // Independently construct camera-to-world yaw R and native reflection C.
    // V = R*C, t = -position*V. The production decoder must recover R.
    let r = [[c, 0., s], [0., 1., 0.], [-s, 0., c]];
    let position: [f32; 3] =
        std::array::from_fn(|i| (0..3).map(|j| r[i][j] * local_position[j]).sum());
    let mut view = [[0.; 4]; 4];
    for i in 0..3 {
        for j in 0..3 {
            view[i][j] = r[i][j] * if j == 1 { -1. } else { 1. };
        }
    }
    let t: [f32; 3] =
        std::array::from_fn(|j| -(0..3).map(|i| position[i] * view[i][j]).sum::<f32>());
    view[3] = [t[0], t[1], t[2], 1.];
    PoseSample {
        present,
        view,
        projection: projection(75.),
    }
}

fn input() -> (ReviewInput, Vec<u8>) {
    let profile = hash(b"profile");
    let geometry = serde_json::to_vec(&serde_json::json!({
        "schema":CP2077_EGEO_REPORT_SCHEMA_V1,"non_trainable":true,"local_only":true,"upload_allowed":false,
        "e_geo":{"schema":CAMERA_EGEO_VALIDATION_SCHEMA_V3,"passed":true,"ready_for_independent_repeat":true,
        "production_binding_authorized":false,"failures":[],"raw_depth_profile_sha256":profile}
    })).unwrap();
    let launches = [
        (D3d12AuthoringLaunchRole::A, 0.8_f32, "a"),
        (D3d12AuthoringLaunchRole::B, -1.2, "b"),
    ]
    .map(|(role, yaw, id)| {
        let mut launch = LaunchEvidence {
            role,
            process_launch_id: id.repeat(32),
            depth_profile_sha256: profile.clone(),
            binding: crate::cp2077_bound_camera::tests::binding(),
            source_evidence_sha256: hash(id.as_bytes()),
            authoring_blockers: vec![],
            independent_motion_evidence_sha256: Some(hash(
                b"independently reviewed motion witness",
            )),
            metric_evidence_sha256: Some(hash(b"independent metric")),
            fov_phases: [75., 90., 75.]
                .into_iter()
                .enumerate()
                .map(|(i, fov)| FovPhase {
                    configured_horizontal_fov_degrees: fov as f64,
                    first_present: 1 + 120 * i as u64,
                    last_present: 120 + 120 * i as u64,
                    sample_count: 120,
                    projection: projection(fov),
                })
                .collect(),
            axis_motion: [
                (D3d12ControlledMotion::Forward, [0., 0., 2.]),
                (D3d12ControlledMotion::StrafeRight, [2., 0., 0.]),
                (D3d12ControlledMotion::VerticalUp, [0., 0.5, 0.]),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (motion, p))| AxisMotion {
                motion,
                start: pose(10 + 20 * i as u64, yaw, [0.; 3]),
                end: pose(20 + 20 * i as u64, yaw, p),
            })
            .collect(),
            route: Some(Route {
                start: pose(1, yaw, [0.; 3]),
                end: pose(100, yaw, [0.01, 0., 0.]),
            }),
            geometry: None,
        };
        seal(&mut launch, &geometry);
        launch
    })
    .to_vec();
    (
        ReviewInput {
            schema: INPUT_SCHEMA.into(),
            non_trainable: true,
            launches,
        },
        geometry,
    )
}

fn seal(launch: &mut LaunchEvidence, bytes: &[u8]) {
    let report_sha256 = hash(bytes);
    launch.geometry = Some(GeometryAttestation {
        claims_sha256: geometry_claims_sha256(launch, &report_sha256, hash).unwrap(),
        report_sha256,
    });
}

fn run(input: &ReviewInput, bytes: &[u8]) -> Review {
    review(input, [Some(bytes), Some(bytes)], hash).unwrap()
}
fn has(r: &Review, needle: &str) -> bool {
    r.blockers
        .iter()
        .chain(r.launches.iter().flat_map(|l| &l.blockers))
        .any(|x| x.contains(needle))
}

#[test]
fn independent_headings_compare_in_camera_coordinates_and_never_publish() {
    let (i, g) = input();
    let r = run(&i, &g);
    assert!(r.ready_for_human_review, "{r:?}");
    assert!(r.non_trainable && r.requires_original_evidence_review);
    assert!(!r.upload_allowed && !r.production_binding_authorized && !r.camera_binding_emitted);
    for l in r.launches {
        assert!((l.motion_in_reference_camera_metres[0][2] - 2.).abs() < 1e-5);
    }
}

#[test]
fn partial_real_world_shape_lists_all_missing_evidence() {
    let (mut i, g) = input();
    i.launches.truncate(1);
    let l = &mut i.launches[0];
    l.fov_phases.pop();
    l.axis_motion.clear();
    l.independent_motion_evidence_sha256 = None;
    l.geometry = None;
    l.authoring_blockers
        .push("manual-stop exceeded 240 seconds".into());
    let r = run(&i, &g);
    for expected in [
        "independent_launch_pair_missing",
        "fov_75_90_75_incomplete",
        "three_independent_motions_missing",
        "independent_motion_evidence_missing",
        "geometry_attestation_missing",
        "source_blocker",
    ] {
        assert!(has(&r, expected), "{r:?}");
    }
    assert!(!r.ready_for_human_review);
}

#[test]
fn stale_or_reordered_fov_and_invented_sample_counts_are_rejected() {
    for mutation in 0..4 {
        let (mut i, g) = input();
        let l = &mut i.launches[0];
        match mutation {
            0 => l.fov_phases[1].projection = projection(75.),
            1 => l.fov_phases[2].projection = projection(78.),
            2 => l.fov_phases[1].first_present = 1,
            _ => l.fov_phases[1].sample_count = 1_000,
        }
        seal(l, &g);
        assert!(!run(&i, &g).ready_for_human_review);
    }
}

#[test]
fn wrong_motion_sign_rotation_and_non_return_do_not_pass() {
    for mutation in 0..4 {
        let (mut i, g) = input();
        let l = &mut i.launches[0];
        match mutation {
            0 => l.axis_motion[0].end = pose(20, 0.8, [0., 0., -2.]),
            1 => l.axis_motion[1].end = pose(40, 1.1, [2., 0., 0.]),
            2 => l.route.as_mut().unwrap().end = pose(100, 0.8, [0.6, 0., 0.]),
            _ => l.axis_motion[2].start.present = 5,
        }
        seal(l, &g);
        assert!(!run(&i, &g).ready_for_human_review);
    }
}

#[test]
fn complete_binding_and_all_evidence_are_in_geometry_claims() {
    for mutation in 0..4 {
        let (mut i, g) = input();
        let l = &mut i.launches[0];
        match mutation {
            0 => {
                l.binding.view_sources[1]
                    .source
                    .descriptor_table
                    .root_parameter_index = 19
            }
            1 => {
                l.binding
                    .holstered_camera
                    .as_mut()
                    .unwrap()
                    .camera_axis_mapping
                    .delivery_x_from = crate::D3d12SignedAxis::NegativeX
            }
            2 => l.metric_evidence_sha256 = Some(hash(b"different metric evidence")),
            _ => l.source_evidence_sha256 = hash(b"different source evidence"),
        }
        let r = run(&i, &g);
        assert!(has(
            &r,
            "geometry_content_or_complete_binding_claims_changed"
        ));
        assert!(!r.ready_for_human_review);
    }
}

#[test]
fn original_geometry_bytes_and_typed_gate_remain_required() {
    let (mut i, g) = input();
    let mut changed = g.clone();
    changed.push(b' ');
    assert!(has(
        &run(&i, &changed),
        "geometry_content_or_complete_binding_claims_changed"
    ));
    let local = br#"{"schema":"dm-cp2077-selected-geometry/v2","geometry_checks_passed":true}"#;
    for l in &mut i.launches {
        seal(l, local);
    }
    assert!(has(&run(&i, local), "typed_geometry_report_invalid"));
    assert!(
        !review(&i, [None, None], hash)
            .unwrap()
            .ready_for_human_review
    );
}

#[test]
fn duplicate_launch_old_binding_and_missing_alias_pair_are_rejected() {
    for mutation in 0..3 {
        let (mut i, g) = input();
        match mutation {
            0 => i.launches[1].process_launch_id = i.launches[0].process_launch_id.clone(),
            1 => i.launches[0].binding.schema = crate::CAMERA_BINDING_SCHEMA_V2.into(),
            _ => {
                i.launches[0].binding.projection_sources.pop();
            }
        }
        assert!(!run(&i, &g).ready_for_human_review);
    }
    let (mut i, g) = input();
    i.non_trainable = false;
    assert!(review(&i, [Some(&g), None], hash).is_err());
}

#[test]
fn motion_cannot_use_another_fov_or_overflow_metric_coordinates() {
    let (mut i, g) = input();
    i.launches[0].axis_motion[0].end.projection = projection(90.);
    seal(&mut i.launches[0], &g);
    assert!(has(
        &run(&i, &g),
        "motion_projection_differs_from_fov75_phase"
    ));
    let (mut i, g) = input();
    i.launches[0].binding.view_metres_per_unit = "1e308".into();
    seal(&mut i.launches[0], &g);
    assert!(!run(&i, &g).ready_for_human_review);
}

fn bound_report(i: &ReviewInput) -> serde_json::Value {
    serde_json::json!({
        "schema":bound_geometry::SCHEMA,"non_trainable":true,"local_only":true,
        "upload_allowed":false,"acceptance":false,"production_binding_authorized":false,
        "binding":i.launches[0].binding,"binding_sha256":hash(b"full binding"),
        "native_replay_sha256":hash(b"native poses"),
        "metric_evidence_sha256":i.launches[0].metric_evidence_sha256,
        "geometry":{"all_original_sha_unchanged":true,"geometry_checks_passed":true,"failures":[]},
        "sessions":i.launches.iter().map(|l| serde_json::json!({
            "process_launch_id":l.process_launch_id,"raw_depth_profile_sha256":l.depth_profile_sha256,
            "inventory_sha256":hash(b"inventory"),"camera_report_sha256":hash(b"camera report"),
            "native_pose_sha256":hash(b"native poses"),"selected_depth_frames":518,
            "bound_pose_coverage":518,"required_rgb_frames":901,"exact_rgb_pairs":901,
            "gate":{"geometry_checks_passed":true,"failures":[],"usable_motion_pairs":40,"invalid_metric_pairs":0,
                "sampled_pairs":{"static":10,"translation":20,"rotation":20},
                "metrics":{"improvement_p50":2.,"inverse_improvement_p50":0.8,"valid_frac_p10":0.7,
                    "improved_share":0.9,"moving_baseline_l1_p50":17.,"static_baseline_l1_p50":3.}}
        })).collect::<Vec<_>>()
    })
}

#[test]
fn bound_selected_geometry_enters_review_without_authorizing_production() {
    let (mut i, _) = input();
    let g = serde_json::to_vec(&bound_report(&i)).unwrap();
    for l in &mut i.launches {
        seal(l, &g);
    }
    let r = run(&i, &g);
    assert!(r.ready_for_human_review, "{r:?}");
    assert!(!r.production_binding_authorized && !r.camera_binding_emitted && !r.upload_allowed);
}

#[test]
fn recomputing_claims_cannot_reassign_geometry_to_other_binding_launch_or_metric() {
    for mutation in 0..7 {
        let (mut i, _) = input();
        let mut value = bound_report(&i);
        match mutation {
            0 => {
                value["binding"]["view_sources"][1]["descriptor_table"]["root_parameter_index"] =
                    19.into()
            }
            1 => value["binding"]["axis_mapping"]["delivery_x_from"] = "negative_x".into(),
            2 => {
                value["binding"]["holstered_camera"]["camera_axis_mapping"]["delivery_y_from"] =
                    "positive_y".into()
            }
            3 => value["sessions"][0]["process_launch_id"] = "c".repeat(32).into(),
            4 => value["sessions"][0]["raw_depth_profile_sha256"] = hash(b"other profile").into(),
            5 => value["metric_evidence_sha256"] = hash(b"another metric").into(),
            _ => value["sessions"][0]["bound_pose_coverage"] = 517.into(),
        }
        let g = serde_json::to_vec(&value).unwrap();
        for l in &mut i.launches {
            seal(l, &g);
        }
        assert!(!run(&i, &g).ready_for_human_review, "mutation {mutation}");
    }
}

#[test]
fn aggregate_geometry_success_cannot_supply_missing_launch_populations_or_weak_metrics() {
    for mutation in 0..5 {
        let (mut i, _) = input();
        let mut value = bound_report(&i);
        let gate = &mut value["sessions"][0]["gate"];
        match mutation {
            0 => gate["sampled_pairs"]["rotation"] = 0.into(),
            1 => gate["metrics"]["improvement_p50"] = 1.0.into(),
            2 => gate["metrics"]["inverse_improvement_p50"] = 2.1.into(),
            3 => gate["metrics"]["static_baseline_l1_p50"] = serde_json::Value::Null,
            _ => gate["invalid_metric_pairs"] = 1.into(),
        }
        let g = serde_json::to_vec(&value).unwrap();
        for l in &mut i.launches {
            seal(l, &g);
        }
        let r = run(&i, &g);
        assert!(!r.ready_for_human_review);
        assert!(
            r.launches[0]
                .blockers
                .iter()
                .any(|b| b.contains("bound_geometry_this_launch")),
            "{r:?}"
        );
        assert!(r.launches[1].blockers.is_empty());
    }
}

fn reusable_set() -> (evidence_set::EvidenceSet, Vec<u8>) {
    use evidence_set::{Contribution, EvidenceSet};
    let (i, _) = input();
    let g = serde_json::to_vec(&bound_report(&i)).unwrap();
    let mut motion = i.launches[0].clone();
    motion.depth_profile_sha256 = hash(b"original motion profile");
    motion.fov_phases.truncate(1);
    motion.geometry = None;
    let mut fov = i.launches[0].clone();
    fov.process_launch_id = "c".repeat(32);
    fov.depth_profile_sha256 = hash(b"original FOV profile");
    fov.axis_motion.clear();
    fov.route = None;
    fov.independent_motion_evidence_sha256 = None;
    fov.geometry = None;
    let mut geometry = i.launches[1].clone();
    geometry.fov_phases.clear();
    geometry.axis_motion.clear();
    geometry.route = None;
    geometry.independent_motion_evidence_sha256 = None;
    seal(&mut geometry, &g);
    let scoped = |evidence| Contribution {
        evidence,
        original_review_sha256: hash(b"immutable original review"),
        retained_limitations: vec![
            "Historical later phase failed; only the intact cited window is reused.".into(),
        ],
    };
    (
        EvidenceSet {
            schema: evidence_set::INPUT_SCHEMA.into(),
            non_trainable: true,
            binding: i.launches[0].binding.clone(),
            motion: scoped(motion),
            fov: scoped(fov),
            geometry: scoped(geometry),
        },
        g,
    )
}

#[test]
fn scoped_reuse_keeps_three_real_launches_and_profiles_without_repeating_each_test() {
    let (i, g) = reusable_set();
    let r = evidence_set::review_set(&i, Some(&g), hash).unwrap();
    assert!(r.ready_for_local_configuration, "{r:?}");
    assert_eq!(r.independent_launch_count, 3);
    assert_eq!(
        r.contributions[0].depth_profile_sha256,
        i.motion.evidence.depth_profile_sha256
    );
    assert_eq!(
        r.contributions[1].checks.process_launch_id,
        i.fov.evidence.process_launch_id
    );
    assert_eq!(
        r.contributions[0].retained_limitations,
        i.motion.retained_limitations
    );
    assert!(!r.live_client_verified && !r.production_binding_authorized && !r.upload_allowed);
}

#[test]
fn reuse_still_rejects_changed_sources_metric_geometry_identity_and_unhealthy_scope() {
    for mutation in 0..7 {
        let (mut i, g) = reusable_set();
        match mutation {
            0 => {
                i.fov.evidence.binding.view_sources[1]
                    .source
                    .descriptor_table
                    .root_parameter_index = 19
            }
            1 => i.motion.evidence.metric_evidence_sha256 = Some(hash(b"different metric")),
            2 => i.geometry.evidence.process_launch_id = "d".repeat(32),
            3 => i
                .motion
                .evidence
                .authoring_blockers
                .push("copy loss inside reused window".into()),
            4 => i.motion.evidence.independent_motion_evidence_sha256 = None,
            5 => i.fov.evidence.fov_phases[2].projection = projection(90.),
            _ => i.motion.evidence.axis_motion[2].end = pose(60, 0.8, [0., -0.5, 0.]),
        }
        // Even freshly sealed claims cannot make another launch's geometry belong here.
        seal(&mut i.geometry.evidence, &g);
        let r = evidence_set::review_set(&i, Some(&g), hash).unwrap();
        assert!(
            !r.ready_for_local_configuration,
            "mutation {mutation}: {r:?}"
        );
    }
}

#[test]
fn reuse_does_not_accept_legacy_unbound_geometry_or_invent_independent_launches() {
    let (mut i, g) = reusable_set();
    let launch = i.geometry.evidence.process_launch_id.clone();
    i.motion.evidence.process_launch_id = launch.clone();
    i.fov.evidence.process_launch_id = launch;
    let r = evidence_set::review_set(&i, Some(&g), hash).unwrap();
    assert!(
        r.blockers
            .iter()
            .any(|b| b == "independent_source_repeat_missing")
    );
    let (mut i, _) = reusable_set();
    let (_, old_geometry) = input();
    seal(&mut i.geometry.evidence, &old_geometry);
    assert!(
        !evidence_set::review_set(&i, Some(&old_geometry), hash)
            .unwrap()
            .ready_for_local_configuration
    );
    i.non_trainable = false;
    assert!(evidence_set::review_set(&i, None, hash).is_err());
}
