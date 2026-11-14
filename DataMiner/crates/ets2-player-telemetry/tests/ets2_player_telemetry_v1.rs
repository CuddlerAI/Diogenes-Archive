//! 契约级验收:从 SDK 原始读数到交付侧 `player_*` 的端到端换算。
//!
//! 这些用例锁的是**语义**不是实现:每一条都对应一个「错了不会崩、只会静默交出坏数据」
//! 的具体失败形状。

use ets2_player_telemetry::{
    ConversionErrorV1, CoordinateTransformV1, EulerRangeError, EulerTurns, PlayerSampleV1,
    RawTruckSampleV1, RollConvention, SpeedProvenanceV1,
};

fn facing_north_at_origin() -> RawTruckSampleV1 {
    RawTruckSampleV1 {
        world_position_m: [0.0, 0.0, 0.0],
        world_orientation: EulerTurns {
            heading: 0.0,
            pitch: 0.0,
            roll: 0.0,
        },
        local_linear_velocity_mps: Some([0.0, 0.0, -20.0]),
        speedometer_mps: 21.0,
    }
}

/// 位置换算只翻 Z 号:SCS 的 +Z 是**南**,交付侧的 +Z 是**前(北)**。
/// 漏掉这一步,整批轨迹会南北镜像——而地图看上去仍然「像那么回事」。
#[test]
fn position_flips_only_the_south_axis() {
    let sample = RawTruckSampleV1 {
        world_position_m: [1000.0, 50.0, 2000.0], // 东 1000、上 50、南 2000
        ..facing_north_at_origin()
    };
    let out = sample
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    assert_eq!(out.position_m, [1000.0, 50.0, -2000.0]);
}

/// 朝北行驶时,交付侧的前向四元数必须是单位旋转。
///
/// 因为交付世界系的 +Z 就定义为「前」,而 heading=0 的车头正指北 = 交付系 +Z,
/// 两者重合 ⇒ 无旋转。
///
/// ⚠️ **这条只验自洽,不验轴选对了**:heading=0 时源旋转是单位阵,
/// 而 `A · I · Aᵀ = I` 对任何正交 `A` 都成立。真正区分轴矩阵的是
/// [`facing_east_is_a_quarter_turn_about_the_up_axis`],两条要一起看。
#[test]
fn heading_north_maps_to_identity_rotation_in_delivery_space() {
    let out = facing_north_at_origin()
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    let q = out.rotation_quaternion_xyzw;
    assert!(
        q[0].abs() < 1e-9 && q[1].abs() < 1e-9 && q[2].abs() < 1e-9,
        "{q:?}"
    );
    assert!((q[3] - 1.0).abs() < 1e-9, "{q:?}");
}

/// 四元数永远归一化且 `w ≥ 0`(与 `fallout-player-telemetry` 同一约定,
/// 否则两款游戏的交付数据会用两种等价但不同号的表示)。
#[test]
fn quaternion_is_normalised_with_non_negative_w() {
    for heading in [0.0, 0.1, 0.3, 0.55, 0.8, 0.95] {
        let sample = RawTruckSampleV1 {
            world_orientation: EulerTurns {
                heading,
                pitch: 0.07,
                roll: 0.03,
            },
            ..facing_north_at_origin()
        };
        let q = sample
            .to_player_sample(RollConvention::RollAxisIsForward)
            .expect("合法样本")
            .rotation_quaternion_xyzw;
        let norm = q.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-9, "heading={heading} norm={norm}");
        assert!(q[3] >= 0.0, "heading={heading} q={q:?}");
    }
}

/// 速度优先取真实速度向量的模长,而不是仪表读数。
///
/// 本例里两者故意不同(20 vs 21):仪表通常偏乐观,
/// 若实现悄悄取了 `truck.speed`,这条会立刻抓到。
#[test]
fn true_velocity_wins_over_speedometer() {
    let out = facing_north_at_origin()
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    assert!((out.speed_mps - 20.0).abs() < 1e-9);
    assert_eq!(
        out.speed_provenance,
        SpeedProvenanceV1::LocalLinearVelocityMagnitude
    );
}

/// 速度向量缺失时降级到仪表读数,**但必须显式标注降级**,不能静默。
#[test]
fn missing_velocity_channel_degrades_explicitly() {
    let sample = RawTruckSampleV1 {
        local_linear_velocity_mps: None,
        ..facing_north_at_origin()
    };
    let out = sample
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    assert!((out.speed_mps - 21.0).abs() < 1e-9);
    assert_eq!(out.speed_provenance, SpeedProvenanceV1::SpeedometerAbs);
}

/// 倒车时 `truck.speed` 是负值,而 68 号要的 `player_speed` 是标量速率。
/// 直接把负数交出去会让下游把倒车当成「负速度」——必须取绝对值。
#[test]
fn reversing_speedometer_becomes_non_negative_speed() {
    let sample = RawTruckSampleV1 {
        local_linear_velocity_mps: None,
        speedometer_mps: -3.5,
        ..facing_north_at_origin()
    };
    let out = sample
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    assert!((out.speed_mps - 3.5).abs() < 1e-9);
    assert!(out.speed_mps >= 0.0);
}

/// 速度模长与坐标系无关:车身系直接取模,不需要先转到世界系。
/// 这条保证「省掉一次轴变换」是有依据的省,而不是漏做。
#[test]
fn speed_magnitude_is_frame_invariant() {
    let sample = RawTruckSampleV1 {
        local_linear_velocity_mps: Some([3.0, 0.0, -4.0]),
        ..facing_north_at_origin()
    };
    for heading in [0.0, 0.2, 0.6, 0.9] {
        let rotated = RawTruckSampleV1 {
            world_orientation: EulerTurns {
                heading,
                pitch: 0.0,
                roll: 0.0,
            },
            ..sample
        };
        let out = rotated
            .to_player_sample(RollConvention::Unvalidated)
            .expect("合法样本");
        assert!((out.speed_mps - 5.0).abs() < 1e-9, "heading={heading}");
    }
}

/// 越界欧拉角一路冒泡成错误,不静默钳位。
#[test]
fn out_of_range_euler_fails_the_whole_sample() {
    let sample = RawTruckSampleV1 {
        world_orientation: EulerTurns {
            heading: 180.0, // 把「度」当成「圈」传进来
            pitch: 0.0,
            roll: 0.0,
        },
        ..facing_north_at_origin()
    };
    assert_eq!(
        sample.to_player_sample(RollConvention::Unvalidated),
        Err(ConversionErrorV1::Euler(EulerRangeError::Heading))
    );
}

/// 非有限输入一律拒绝,不产出 NaN 样本。
#[test]
fn non_finite_inputs_are_rejected() {
    let bad_position = RawTruckSampleV1 {
        world_position_m: [f64::NAN, 0.0, 0.0],
        ..facing_north_at_origin()
    };
    assert_eq!(
        bad_position.to_player_sample(RollConvention::Unvalidated),
        Err(ConversionErrorV1::NonFinitePosition)
    );

    let bad_velocity = RawTruckSampleV1 {
        local_linear_velocity_mps: Some([f64::INFINITY, 0.0, 0.0]),
        ..facing_north_at_origin()
    };
    assert_eq!(
        bad_velocity.to_player_sample(RollConvention::Unvalidated),
        Err(ConversionErrorV1::NonFiniteSpeed)
    );
}

/// `metric_scale` 恒为 1.0——1:20 是里程压缩比,不是几何比例(98 号 §4.2 陷阱 2)。
#[test]
fn metric_scale_is_one_not_the_advertised_map_ratio() {
    let transform = CoordinateTransformV1::scs_to_dm_world();
    assert!((transform.metres_per_game_unit - 1.0).abs() < f64::EPSILON);
    assert_eq!(transform.validate_shape(), Ok(()));
}

/// 变换声明必须原样进交付元数据,且可 JSON 往返——
/// 否则「用了哪套轴」这件事在事后不可复核。
#[test]
fn sample_round_trips_through_json_with_its_transform() {
    let out = facing_north_at_origin()
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    let encoded = serde_json::to_string(&out).expect("可序列化");
    let decoded: PlayerSampleV1 = serde_json::from_str(&encoded).expect("可反序列化");
    assert_eq!(decoded, out);
    assert!(encoded.contains("scs-world-x-east-y-up-z-south-rh-m/v1"));
    assert!(encoded.contains("\"roll_convention\":\"unvalidated\""));
}

/// roll 尚未标定这件事必须落在交付数据里,而不只是活在文档中。
#[test]
fn unvalidated_roll_is_visible_in_the_delivered_sample() {
    let out = facing_north_at_origin()
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    assert_eq!(out.roll_convention, RollConvention::Unvalidated);
    assert!(!out.roll_convention.is_validated());
}

/// 车头朝**东**时的四元数必须是绕「上」轴 +90° —— 这条才真正验证轴矩阵选对了。
///
/// 上面那条 `heading_north_maps_to_identity_rotation_in_delivery_space` 有个弱点:
/// heading=0 时源旋转是单位阵,而 `A · I · Aᵀ = I` 对**任何**正交 `A` 都成立,
/// 所以它验的是「自洽」不是「轴选对了」。本条用一个非平凡朝向补上。
///
/// 独立推导(不看实现):交付系是 X 右(东)/ Y 上 / Z 前(北)。
/// 车头朝东 ⇒ 局部前向落在交付系 +X 上。把 `ẑ=(0,0,1)` 转到 `x̂=(1,0,0)` 的是绕 Y 轴 +90°,
/// 对应四元数 `(0, sin45°, 0, cos45°) = (0, √2/2, 0, √2/2)`。
/// 若 Z 轴翻号漏了或翻反了,`qy` 会变号——本条就是那道闸。
#[test]
fn facing_east_is_a_quarter_turn_about_the_up_axis() {
    let sample = RawTruckSampleV1 {
        world_orientation: EulerTurns {
            heading: 0.75, // 官方注释:0.75 → right (east)
            pitch: 0.0,
            roll: 0.0,
        },
        ..facing_north_at_origin()
    };
    let q = sample
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本")
        .rotation_quaternion_xyzw;
    let half = std::f64::consts::FRAC_1_SQRT_2;
    assert!(q[0].abs() < 1e-9, "不该有俯仰分量:{q:?}");
    assert!(q[2].abs() < 1e-9, "不该有滚转分量:{q:?}");
    assert!(
        (q[1] - half).abs() < 1e-9,
        "朝东应为绕上轴 +90°(qy=+√2/2),实得 {q:?}——qy 变号说明 Z 轴翻号有误"
    );
    assert!((q[3] - half).abs() < 1e-9, "{q:?}");
}

/// 68 号 §6.3 的 `player_speed` 是 **Vector3**,标量给不出方向。
///
/// 通道给的是**车身系**速度,必须先过车身旋转再换交付轴。顺序反了会在任何
/// 非正北的朝向上指错方向,而**模长仍然正确**——所以只看 `speed_mps` 抓不到。
#[test]
fn the_velocity_vector_follows_the_truck_heading() {
    // 车身系 −Z 是前向,20 m/s 前进。
    let north = facing_north_at_origin()
        .to_player_sample(RollConvention::Unvalidated)
        .expect("合法样本");
    // 车头朝北 ⇒ 交付系前向 = +Z。
    let v = north.velocity_mps.expect("velocity 通道可用时必须有向量");
    assert!((v[2] - 20.0).abs() < 1e-9, "实得 {v:?}");
    assert!(v[0].abs() < 1e-9 && v[1].abs() < 1e-9, "实得 {v:?}");

    let east = RawTruckSampleV1 {
        world_orientation: EulerTurns {
            heading: 0.75, // 东
            pitch: 0.0,
            roll: 0.0,
        },
        ..facing_north_at_origin()
    }
    .to_player_sample(RollConvention::Unvalidated)
    .expect("合法样本");
    let v = east.velocity_mps.expect("有向量");
    assert!((v[0] - 20.0).abs() < 1e-9, "朝东行驶应落到 +X，实得 {v:?}");
    assert!(v[2].abs() < 1e-9, "实得 {v:?}");
}

/// 仪表兜底那一路**不编向量**。
///
/// 「前向 × 仪表读数」在打滑与倒车时会指错方向,而那正是仪表与地速分离的时刻。
/// 留 `None` 让交付层拒绝,比交一个看起来合理的错向量安全。
#[test]
fn the_speedometer_fallback_does_not_fabricate_a_direction() {
    let out = RawTruckSampleV1 {
        local_linear_velocity_mps: None,
        ..facing_north_at_origin()
    }
    .to_player_sample(RollConvention::Unvalidated)
    .expect("合法样本");
    assert_eq!(out.velocity_mps, None);
    assert_eq!(out.speed_provenance, SpeedProvenanceV1::SpeedometerAbs);
}
