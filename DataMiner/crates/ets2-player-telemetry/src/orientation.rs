//! SCS 欧拉角(单位:**圈**)→ 世界旋转矩阵。
//!
//! 全部规格取自官方 SDK 1.14 头文件 `include/scssdk_value.h` 的 doc 注释,逐字见 [`SPEC`]。
//! 这一步是 ETS2 线**唯一一处「错了不会崩、只会让整批四元数静默错」**的地方,
//! 所以本模块的每一条约定都钉了测试,且**没有测试锚点的那一条(roll)不给默认值**。

use serde::{Deserialize, Serialize};

/// 官方头文件原文(`scs_sdk_1_14/include/scssdk_value.h`),本模块的全部依据。
///
/// ```text
/// // 坐标系(第 86–87 行)
/// In local space the X points to right, Y up and Z backwards.
/// In world space the X points to east, Y up and Z south.
///
/// // heading(第 111–118 行)
/// Stored in unit range where <0,1) corresponds to <0,360).
/// The angle is measured counterclockwise in horizontal plane when looking
/// from top where 0 corresponds to forward (north), 0.25 to left (west),
/// 0.5 to backward (south) and 0.75 to right (east).
///
/// // pitch(第 122–129 行)
/// Stored in unit range where <-0.25,0.25> corresponds to <-90,90>.
/// The pitch angle is zero when in horizontal direction,
/// with positive values pointing up (0.25 directly to zenith),
/// and negative values pointing down (-0.25 directly to nadir).
///
/// // roll(第 133–139 行)
/// Stored in unit range where <-0.5,0.5> corresponds to <-180,180>.
/// The angle is measured in counterclockwise when looking in direction of
/// the roll axis.
/// ```
pub const SPEC: &str = "scs_sdk_1_14/include/scssdk_value.h";

/// 一圈的弧度数。SCS 的三个角**全部**以「圈」为单位,不是弧度也不是度。
pub const RADIANS_PER_TURN: f64 = std::f64::consts::TAU;

/// roll 的符号约定。**没有默认值,调用方必须显式选。**
///
/// # 为什么这一条必须由调用方决定
///
/// heading 与 pitch 的官方注释都给了**可判定的锚点**——heading 逐个列出了
/// 0/0.25/0.5/0.75 对应北/西/南/东,pitch 写明「正值朝上,0.25 指天顶」。
/// 这两条各自有唯一解,本模块直接钉死并附测试。
///
/// roll 的原文只有一句「counterclockwise when looking **in direction of the roll axis**」,
/// 而「roll 轴的方向」本身有两种都说得通的读法:
///
/// - 读作**局部 +Z(向后)**:视线朝车尾看,此视角下的逆时针 = 标准 `Rz(−ψ)`;
/// - 读作**车头前向**(驾驶员/飞行员的习惯说法,即局部 −Z):视线朝车头看,
///   此视角下的逆时针 = 标准 `Rz(+ψ)`。
///
/// 两者差一个符号,而且**在平地上永远看不出来**(roll≈0),
/// 只在侧倾路面/事故翻车时才显形——正是最容易带着错符号交付一整批的形状。
///
/// 官方文档没有第三句话可以裁定,**所以本模块不猜**。
/// 参照 [98 号 §5.7.3 对 DCS 3×3 基向量的处理](../../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md):
/// 「上机第一件事必须是标定……这一步错了整批四元数会静默错」。
/// 实机标定方法见本 crate README「roll 符号标定」一节;标定前请用
/// [`RollConvention::Unvalidated`] 并把它记进交付元数据,使数据在符号选错时仍可重算。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RollConvention {
    /// 「roll 轴」= 局部 +Z(向后)⇒ `Rz(−ψ)`。
    RollAxisIsLocalZBackward,
    /// 「roll 轴」= 车头前向(局部 −Z)⇒ `Rz(+ψ)`。
    RollAxisIsForward,
    /// 尚未实机标定。等价于 [`Self::RollAxisIsForward`] 的数值,
    /// 但**语义是「这个符号还没被验证」**——交付元数据必须原样记录本取值,
    /// 以便标定结论出来后判断是否需要重算。
    Unvalidated,
}

impl RollConvention {
    /// 该约定下 roll 角的符号因子。
    #[must_use]
    pub const fn sign(self) -> f64 {
        match self {
            Self::RollAxisIsLocalZBackward => -1.0,
            Self::RollAxisIsForward | Self::Unvalidated => 1.0,
        }
    }

    /// 是否已经过实机标定。`false` 时交付层应把它标进 `confidence`。
    #[must_use]
    pub const fn is_validated(self) -> bool {
        !matches!(self, Self::Unvalidated)
    }
}

/// SCS `scs_value_euler_t`,单位是**圈**。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EulerTurns {
    /// `<0,1)`,0 = 北,0.25 = 西,0.5 = 南,0.75 = 东。
    pub heading: f64,
    /// `<-0.25,0.25>`,正值朝上。
    pub pitch: f64,
    /// `<-0.5,0.5>`,符号含义见 [`RollConvention`]。
    pub roll: f64,
}

/// 欧拉角落在官方声明的取值域之外。
///
/// **越界一律拒绝,不做取模也不做钳位。** 越界只可能来自「订阅错了通道」
/// 「把度当成圈传进来」「读到了未初始化内存」这三类,
/// 每一类继续算下去都会得到看着合理的错位姿。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EulerRangeError {
    /// heading 不在 `<0,1)`。度数误当圈数传入时最先撞上这条。
    Heading,
    /// pitch 不在 `<-0.25,0.25>`。
    Pitch,
    /// roll 不在 `<-0.5,0.5>`。
    Roll,
    /// 有非有限值。
    NonFinite,
}

impl EulerTurns {
    /// 按官方注释的取值域校验。
    pub fn validate(&self) -> Result<(), EulerRangeError> {
        if ![self.heading, self.pitch, self.roll]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(EulerRangeError::NonFinite);
        }
        if !(0.0..1.0).contains(&self.heading) {
            return Err(EulerRangeError::Heading);
        }
        if !(-0.25..=0.25).contains(&self.pitch) {
            return Err(EulerRangeError::Pitch);
        }
        if !(-0.5..=0.5).contains(&self.roll) {
            return Err(EulerRangeError::Roll);
        }
        Ok(())
    }

    /// 构造「局部 → SCS 世界」的旋转矩阵(行主序,`world = R · local`)。
    ///
    /// 列即局部基向量在世界系下的像:第 0 列 = 局部 X(右),
    /// 第 1 列 = 局部 Y(上),第 2 列 = 局部 Z(**向后**)。
    ///
    /// 组合顺序是**内旋** `R = Ry(heading) · Rx(pitch) · Rz(roll)`
    /// ——即先在车身系内 roll,再绕**车身**右轴 pitch,最后 heading 转入世界系。
    ///
    /// 这是 heading/pitch/roll 这类姿态数据的通行约定(pitch 绕机体轴而非世界轴),
    /// 官方头文件没有逐字声明,但它**不是** roll 符号那种五五开的猜测:
    /// 换成外旋(pitch 绕世界 X 轴)会在「朝东西向行驶且有俯仰」时给出明显错误的结果
    /// ——车头朝西时抬头会变成完全不抬头。[`pitch_is_about_the_body_axis_not_the_world_axis`]
    /// 就是用这个场景把顺序钉死的。
    pub fn to_world_rotation(
        &self,
        roll_convention: RollConvention,
    ) -> Result<[[f64; 3]; 3], EulerRangeError> {
        self.validate()?;
        let (h, p, r) = (
            self.heading * RADIANS_PER_TURN,
            self.pitch * RADIANS_PER_TURN,
            self.roll * RADIANS_PER_TURN * roll_convention.sign(),
        );
        // Ry(h)：绕世界「上」轴。四个基本方位由测试逐个钉死。
        let yaw = [
            [h.cos(), 0.0, h.sin()],
            [0.0, 1.0, 0.0],
            [-h.sin(), 0.0, h.cos()],
        ];
        // Rx(p)：绕车身「右」轴，正值抬头。
        let pitch = [
            [1.0, 0.0, 0.0],
            [0.0, p.cos(), -p.sin()],
            [0.0, p.sin(), p.cos()],
        ];
        // Rz(r)：绕车身纵轴（局部 Z）。符号已由 roll_convention 吸收。
        let roll = [
            [r.cos(), -r.sin(), 0.0],
            [r.sin(), r.cos(), 0.0],
            [0.0, 0.0, 1.0],
        ];
        Ok(mul3(mul3(yaw, pitch), roll))
    }

    /// 车头前向在 SCS 世界系下的单位向量。
    ///
    /// 局部前向是 **−Z**(头文件:局部 Z 指向**后方**),所以它是旋转矩阵第 2 列取负。
    /// 这个「前向 = −Z 而不是 +Z」是 ETS2 线最容易漏掉的一条。
    pub fn forward_in_world(
        &self,
        roll_convention: RollConvention,
    ) -> Result<[f64; 3], EulerRangeError> {
        let m = self.to_world_rotation(roll_convention)?;
        Ok([-m[0][2], -m[1][2], -m[2][2]])
    }
}

fn mul3(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| left[i][k] * right[k][j]).sum();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-12;

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    /// **本 crate 最重要的一条测试。**
    ///
    /// 官方注释把 heading 的四个基本方位逐个写死了,它们是整条 ETS2 线上
    /// 唯一不依赖任何解释的锚点。世界系为 X=东 / Y=上 / Z=南,于是:
    /// 北 = −Z、西 = −X、南 = +Z、东 = +X。
    #[test]
    fn heading_matches_the_four_cardinal_points_from_the_official_header() {
        let cases = [
            (0.00, [0.0, 0.0, -1.0], "0 → forward (north) = −Z"),
            (0.25, [-1.0, 0.0, 0.0], "0.25 → left (west) = −X"),
            (0.50, [0.0, 0.0, 1.0], "0.5 → backward (south) = +Z"),
            (0.75, [1.0, 0.0, 0.0], "0.75 → right (east) = +X"),
        ];
        for (heading, expected, what) in cases {
            let euler = EulerTurns {
                heading,
                pitch: 0.0,
                roll: 0.0,
            };
            let forward = euler
                .forward_in_world(RollConvention::Unvalidated)
                .expect("合法取值");
            assert!(close(forward, expected), "{what}: 实得 {forward:?}");
        }
    }

    /// pitch 正值抬头:前向的「上」分量必须为正,且 0.25 圈正好指天顶。
    #[test]
    fn positive_pitch_points_up_and_quarter_turn_reaches_zenith() {
        let up_a_bit = EulerTurns {
            heading: 0.0,
            pitch: 0.05,
            roll: 0.0,
        }
        .forward_in_world(RollConvention::Unvalidated)
        .expect("合法取值");
        assert!(up_a_bit[1] > 0.0, "正 pitch 必须朝上,实得 {up_a_bit:?}");

        let zenith = EulerTurns {
            heading: 0.0,
            pitch: 0.25,
            roll: 0.0,
        }
        .forward_in_world(RollConvention::Unvalidated)
        .expect("合法取值");
        assert!(close(zenith, [0.0, 1.0, 0.0]), "实得 {zenith:?}");
    }

    /// **pitch 绕的是车身右轴,不是世界 X 轴。**
    ///
    /// 这条把欧拉角的组合顺序(内旋 vs 外旋)钉死了,而它只在 heading 与 pitch
    /// **同时非零**时才显形——单独测 heading、单独测 pitch 都发现不了顺序错。
    ///
    /// 判据用车头朝**西**(heading=0.25)且抬头:
    /// - 内旋(正确):pitch 绕车身右轴 ⇒ 前向的「上」分量为正,车确实在抬头;
    /// - 外旋(错误):pitch 绕世界 X 轴(东)⇒ 此时前向正好沿着世界 X 轴,
    ///   绕自己转不动它,**抬头量恰好为 0**——一个安静到极点的错误。
    #[test]
    fn pitch_is_about_the_body_axis_not_the_world_axis() {
        let facing_west_nose_up = EulerTurns {
            heading: 0.25,
            pitch: 0.1,
            roll: 0.0,
        };
        let forward = facing_west_nose_up
            .forward_in_world(RollConvention::Unvalidated)
            .expect("合法取值");
        let expected_up = (0.1 * RADIANS_PER_TURN).sin();
        assert!(
            (forward[1] - expected_up).abs() < 1e-9,
            "朝西抬头时上分量应为 sin(pitch)={expected_up}，实得 {}（为 0 说明用了外旋顺序）",
            forward[1]
        );
        assert!(forward[0] < 0.0, "仍应指向西侧，实得 {forward:?}");
        assert!(
            forward[2].abs() < 1e-9,
            "朝正西时不该有南北分量，实得 {forward:?}"
        );
    }

    /// heading 与 pitch 都不该改变「上」的定义之外的东西:
    /// 任何合法角度产出的都必须是纯旋转(正交且 det = +1)。
    #[test]
    fn every_legal_euler_produces_a_proper_rotation() {
        for h in [0.0, 0.1, 0.37, 0.5, 0.99] {
            for p in [-0.25, -0.1, 0.0, 0.2, 0.25] {
                for r in [-0.5, -0.3, 0.0, 0.25, 0.5] {
                    let m = EulerTurns {
                        heading: h,
                        pitch: p,
                        roll: r,
                    }
                    .to_world_rotation(RollConvention::RollAxisIsForward)
                    .expect("合法取值");
                    assert!(telemetry_geometry::matrix_is_orthonormal(m, 1e-9));
                    assert!((telemetry_geometry::determinant3(m) - 1.0).abs() < 1e-9);
                }
            }
        }
    }

    /// roll 的两种约定只差一个符号,而且**在 roll=0 时完全无法区分**——
    /// 这正是它必须靠实机标定、不能靠平地数据关闭的原因。
    #[test]
    fn roll_conventions_differ_only_when_roll_is_nonzero() {
        let level = EulerTurns {
            heading: 0.3,
            pitch: 0.1,
            roll: 0.0,
        };
        let a = level
            .to_world_rotation(RollConvention::RollAxisIsForward)
            .unwrap();
        let b = level
            .to_world_rotation(RollConvention::RollAxisIsLocalZBackward)
            .unwrap();
        assert_eq!(a, b, "roll=0 时两种约定必须逐位相同");

        let banked = EulerTurns {
            roll: 0.05,
            ..level
        };
        let a = banked
            .to_world_rotation(RollConvention::RollAxisIsForward)
            .unwrap();
        let b = banked
            .to_world_rotation(RollConvention::RollAxisIsLocalZBackward)
            .unwrap();
        assert_ne!(a, b, "roll≠0 时两种约定必须不同,否则这个开关是假的");
    }

    /// `Unvalidated` 与 `RollAxisIsForward` 数值相同但语义不同:
    /// 前者是「还没验」,交付层要能区分。
    #[test]
    fn unvalidated_is_numerically_forward_but_flagged() {
        let e = EulerTurns {
            heading: 0.2,
            pitch: 0.05,
            roll: 0.1,
        };
        assert_eq!(
            e.to_world_rotation(RollConvention::Unvalidated).unwrap(),
            e.to_world_rotation(RollConvention::RollAxisIsForward)
                .unwrap()
        );
        assert!(!RollConvention::Unvalidated.is_validated());
        assert!(RollConvention::RollAxisIsForward.is_validated());
    }

    /// 越界不取模、不钳位,一律拒绝。
    /// 「把度当圈传进来」是最现实的误用,它会以 heading=90 这种形式撞上来。
    #[test]
    fn out_of_range_angles_are_rejected_rather_than_wrapped() {
        let degrees_mistaken_for_turns = EulerTurns {
            heading: 90.0,
            pitch: 0.0,
            roll: 0.0,
        };
        assert_eq!(
            degrees_mistaken_for_turns.validate(),
            Err(EulerRangeError::Heading)
        );
        // heading 是半开区间：1.0 属于越界。
        assert_eq!(
            EulerTurns {
                heading: 1.0,
                pitch: 0.0,
                roll: 0.0
            }
            .validate(),
            Err(EulerRangeError::Heading)
        );
        assert_eq!(
            EulerTurns {
                heading: 0.0,
                pitch: 0.3,
                roll: 0.0
            }
            .validate(),
            Err(EulerRangeError::Pitch)
        );
        assert_eq!(
            EulerTurns {
                heading: 0.0,
                pitch: 0.0,
                roll: 0.6
            }
            .validate(),
            Err(EulerRangeError::Roll)
        );
        assert_eq!(
            EulerTurns {
                heading: f64::NAN,
                pitch: 0.0,
                roll: 0.0
            }
            .validate(),
            Err(EulerRangeError::NonFinite)
        );
    }

    /// 边界值本身是合法的,不能被误拒。
    #[test]
    fn range_endpoints_are_accepted() {
        for e in [
            EulerTurns {
                heading: 0.0,
                pitch: 0.25,
                roll: 0.5,
            },
            EulerTurns {
                heading: 0.999_999,
                pitch: -0.25,
                roll: -0.5,
            },
        ] {
            assert_eq!(e.validate(), Ok(()));
        }
    }

    /// 局部前向是 −Z 而不是 +Z(头文件:局部 Z 指向后方)。
    /// 若哪天有人把它改成 +Z,整批朝向会反过来 180°——本测试就是那道闸。
    #[test]
    fn local_forward_is_negative_z_not_positive_z() {
        let facing_north = EulerTurns {
            heading: 0.0,
            pitch: 0.0,
            roll: 0.0,
        };
        let m = facing_north
            .to_world_rotation(RollConvention::Unvalidated)
            .unwrap();
        // 第 2 列 = 局部 +Z 的像 = 向后 = 南 = +Z_world。
        assert!(close([m[0][2], m[1][2], m[2][2]], [0.0, 0.0, 1.0]));
        // 前向则是它的反向 = 北。
        assert!(close(
            facing_north
                .forward_in_world(RollConvention::Unvalidated)
                .unwrap(),
            [0.0, 0.0, -1.0]
        ));
    }

    /// 一圈的弧度常数别被写成 π(那会让所有角度差一半)。
    /// 用「四分之一圈 = 90°」表述,而不是硬写一个近似字面量。
    #[test]
    fn a_turn_is_a_full_revolution_not_a_half() {
        assert!((RADIANS_PER_TURN / 4.0 - std::f64::consts::FRAC_PI_2).abs() < EPS);
        assert!((RADIANS_PER_TURN / 2.0 - std::f64::consts::PI).abs() < EPS);
    }
}
