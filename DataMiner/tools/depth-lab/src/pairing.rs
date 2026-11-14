//! 逐帧配对契约与七条不可违反的不变式
//! （[77 号 §5.7](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
//!
//! 这是本 crate 里风险最高的一块。68 号 §6.5 把「depth 与 rgb 帧严格对齐」列为单独质检项，
//! 并写明「只要发现一条没对齐，全部打回」；而 DataMiner 的 CFR pacer 会产生
//! `duplicate` 与 `source_superseded` 两种情况，**Present 次数 ≠ 输出帧数**。
//! 不显式处理就会在游戏帧率波动处静默错位，而错位后的 GT-vs-MDE 差异会被误读成「MDE 不准」
//! ——这是整个实验最隐蔽的伪结论通道（风险登记 R-4）。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// 相机来源（v5 §8.5.3）。free-cam 段与玩家视角段的 GT 可信度不同，
/// 不得混入同一条交付数据，也不得在评测时混算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraMode {
    Player,
    Freecam,
}

/// recorder 侧的一个 CFR 输出帧（来自 `frames.jsonl.zst` + 水印解码结果）。
#[derive(Debug, Clone)]
pub struct RgbFrame {
    pub session_frame: u64,
    pub segment_index: u32,
    pub segment_frame: u32,
    pub source_qpc: i64,
    /// WGC 该 slot 无新 source frame，复用了上一 NV12（[01 §3](../../../docs/01-recording-pipeline.md)）。
    pub duplicate: bool,
    pub camera_mode: CameraMode,
    /// 从画面左上角水印解出的 `present_seq`（已展开）。`None` 表示解码失败。
    pub watermark_present_seq: Option<u64>,
}

/// probe 侧的一个 depth 帧（来自 `depth-index.jsonl`）。
#[derive(Debug, Clone)]
pub struct DepthFrame {
    pub present_seq: u64,
    pub dsv_id: String,
}

/// depth 缺失的原因。区分开是为了让缺失是**可解释**的，而不是一个空洞。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbsentReason {
    /// 68 号 §6.5 规定奇数帧无 depth。这是正常状态，不是缺陷。
    OddFrameBySpec,
    /// probe 丢帧（§5.4 的「宁可丢 depth，不可卡游戏」）。
    ProbeDropped,
    /// 水印无法解码（§5.6 的失败模式）。
    WatermarkUndecodable,
    /// 该帧落在 probe 启停窗口之外。
    OutsideProbeWindow,
    /// 该 present_seq 有记录但 DSV 缺失。
    DsvMissing,
}

/// 配对上的 depth 引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDepth {
    pub present_seq: u64,
    /// 必须等于 `depth_<session_frame>`（不变式 3）。
    pub depth_name: String,
    pub dsv_id: String,
    /// 唯一允许的复用：`duplicate` 帧继承前一帧（不变式 5）。
    /// **继承帧不得进对外交付 zip**——它不是一次真实渲染。
    pub inherited: bool,
}

/// 配对表的一行：**每个 RGB 输出帧恰好一行**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePair {
    pub session_frame: u64,
    pub segment_index: u32,
    pub segment_frame: u32,
    pub source_qpc: i64,
    pub duplicate: bool,
    pub camera_mode: CameraMode,
    /// Watermark-decoded source Present for every RGB frame. Odd RGB frames do not
    /// have depth, but the 30 fps action-camera file still needs their exact camera
    /// and player pose. Older pair files may omit this and remain readable for the
    /// 15 fps depth path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<PairedDepth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth_absent_reason: Option<AbsentReason>,
}

/// 配对结果的统计面。覆盖率是硬门（不变式 7）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingReport {
    pub total_frames: u64,
    /// 分母：非 duplicate 的偶数帧总数。
    pub eligible_even_frames: u64,
    /// 分子：非 null 且非 inherited 的偶数帧数。
    pub paired_even_frames: u64,
    pub inherited_frames: u64,
    /// 未被任何 RGB 帧引用的 depth 帧。CFR pacer 丢弃 source frame 时产生（不变式 6）。
    pub unreferenced_depth_frames: u64,
    pub coverage: f64,
}

impl PairingReport {
    /// §5.7.1 不变式 7 的默认门限。
    pub const MIN_COVERAGE: f64 = 0.98;

    #[must_use]
    pub fn passes_coverage_gate(&self) -> bool {
        self.coverage >= Self::MIN_COVERAGE
    }
}

/// 违反不变式即报错退出，**不产出配对表**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    /// 同一个 `present_seq` 在身份已经前进后再次出现，不能解释为连续 source identity alias。
    DuplicatePresentSeq {
        present_seq: u64,
        first: u64,
        second: u64,
    },
    /// 不变式 5：首帧就是 duplicate，没有可继承的前驱。
    LeadingDuplicate { session_frame: u64 },
    /// recorder 声明 duplicate，却解出了不同于前驱的 source Present。
    DuplicateSourceMismatch {
        session_frame: u64,
        previous_present_seq: u64,
        current_present_seq: u64,
    },
    /// selected-present 帧序列中只有部分行携带在线确认的完整 Present identity。
    MissingPresentIdentity { session_frame: u64 },
    /// 在线确认的完整 Present identity 只能前进，或在相邻 RGB observation 上保持不变。
    PresentIdentityNotMonotonic {
        previous_session_frame: u64,
        previous_present_seq: u64,
        session_frame: u64,
        present_seq: u64,
    },
    /// 输入未按 `session_frame` 单调递增排序。
    NotMonotonic { previous: u64, current: u64 },
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicatePresentSeq {
                present_seq,
                first,
                second,
            } => write!(
                f,
                "present_seq {present_seq} 被 session_frame {first} 与 {second} 同时引用（违反一对一）"
            ),
            Self::LeadingDuplicate { session_frame } => write!(
                f,
                "session_frame {session_frame} 是 duplicate 但没有可继承的前驱帧"
            ),
            Self::DuplicateSourceMismatch {
                session_frame,
                previous_present_seq,
                current_present_seq,
            } => write!(
                f,
                "session_frame {session_frame} 是 duplicate，但 present_seq 从 {previous_present_seq} 变成了 {current_present_seq}"
            ),
            Self::MissingPresentIdentity { session_frame } => write!(
                f,
                "selected-present session_frame {session_frame} 缺少在线确认的完整 present_seq"
            ),
            Self::PresentIdentityNotMonotonic {
                previous_session_frame,
                previous_present_seq,
                session_frame,
                present_seq,
            } => write!(
                f,
                "完整 present identity 非单调/非连续 alias：session_frame {previous_session_frame} seq {previous_present_seq} -> session_frame {session_frame} seq {present_seq}"
            ),
            Self::NotMonotonic { previous, current } => {
                write!(f, "session_frame 非单调：{previous} 之后出现 {current}")
            }
        }
    }
}

impl std::error::Error for PairingError {}

/// depth 帧命名契约（不变式 3）：`depth_<session_frame>`，且 `session_frame` 必须为偶数。
#[must_use]
pub fn depth_name_for(session_frame: u64) -> String {
    format!("depth_{session_frame}")
}

/// 构建配对表。
///
/// 算法刻意保持笨：**按 `present_seq` 精确查表，查不到就是 `None`**。
/// 没有任何「就近取一个」的分支——不变式 2 与 6 是靠「代码里根本不存在那条路径」保证的，
/// 而不是靠一个可以被调参放宽的容差。
pub fn build(
    rgb_frames: &[RgbFrame],
    depth_frames: &[DepthFrame],
) -> Result<(Vec<FramePair>, PairingReport), PairingError> {
    let by_seq: HashMap<u64, &DepthFrame> =
        depth_frames.iter().map(|d| (d.present_seq, d)).collect();

    let mut pairs = Vec::with_capacity(rgb_frames.len());
    let mut consumed: HashMap<u64, u64> = HashMap::new();
    let mut referenced: HashSet<u64> = HashSet::new();
    let mut previous_frame: Option<u64> = None;
    let mut previous_present_seq: Option<u64> = None;

    let mut eligible = 0u64;
    let mut paired = 0u64;
    let mut inherited_count = 0u64;

    for frame in rgb_frames {
        let previous_session_frame = previous_frame;
        if let Some(prev) = previous_frame
            && frame.session_frame <= prev
        {
            return Err(PairingError::NotMonotonic {
                previous: prev,
                current: frame.session_frame,
            });
        }
        previous_frame = Some(frame.session_frame);

        let is_even = frame.session_frame % 2 == 0;

        // 不变式 5：duplicate 帧没有新的渲染结果，继承前一帧且显式标记。
        if frame.duplicate {
            let previous_seq = previous_present_seq.ok_or(PairingError::LeadingDuplicate {
                session_frame: frame.session_frame,
            })?;
            if let Some(current_seq) = frame.watermark_present_seq
                && current_seq != previous_seq
            {
                return Err(PairingError::DuplicateSourceMismatch {
                    session_frame: frame.session_frame,
                    previous_present_seq: previous_seq,
                    current_present_seq: current_seq,
                });
            }
            let (depth, reason) = if is_even {
                match by_seq.get(&previous_seq) {
                    None => (None, Some(AbsentReason::ProbeDropped)),
                    Some(d) if d.dsv_id.is_empty() => (None, Some(AbsentReason::DsvMissing)),
                    Some(d) => {
                        referenced.insert(previous_seq);
                        inherited_count += 1;
                        let inherited = PairedDepth {
                            present_seq: previous_seq,
                            depth_name: depth_name_for(frame.session_frame),
                            dsv_id: d.dsv_id.clone(),
                            inherited: true,
                        };
                        (Some(inherited), None)
                    }
                }
            } else {
                (None, Some(AbsentReason::OddFrameBySpec))
            };
            pairs.push(FramePair {
                session_frame: frame.session_frame,
                segment_index: frame.segment_index,
                segment_frame: frame.segment_frame,
                source_qpc: frame.source_qpc,
                duplicate: true,
                camera_mode: frame.camera_mode,
                present_seq: Some(previous_seq),
                depth,
                depth_absent_reason: reason,
            });
            continue;
        }

        if is_even {
            eligible += 1;
        }

        // 68 号 §6.5：奇数帧本就没有 depth。
        if !is_even {
            pairs.push(FramePair {
                session_frame: frame.session_frame,
                segment_index: frame.segment_index,
                segment_frame: frame.segment_frame,
                source_qpc: frame.source_qpc,
                duplicate: false,
                camera_mode: frame.camera_mode,
                present_seq: frame.watermark_present_seq,
                depth: None,
                depth_absent_reason: Some(AbsentReason::OddFrameBySpec),
            });
            previous_present_seq = frame.watermark_present_seq;
            continue;
        }

        let (depth, reason) = match frame.watermark_present_seq {
            None => (None, Some(AbsentReason::WatermarkUndecodable)),
            Some(seq) => match by_seq.get(&seq) {
                None => (None, Some(AbsentReason::ProbeDropped)),
                Some(d) if d.dsv_id.is_empty() => (None, Some(AbsentReason::DsvMissing)),
                Some(d) => {
                    // 连续 RGB observation 可以精确引用同一个 source Present；身份一旦前进，
                    // 旧 seq 再出现仍然是冲突，不能拿“alias”给乱序回放洗白。
                    if let Some(&first) = consumed.get(&seq) {
                        let contiguous_alias = previous_session_frame
                            .and_then(|previous| previous.checked_add(1))
                            == Some(frame.session_frame)
                            && previous_present_seq == Some(seq);
                        if !contiguous_alias {
                            return Err(PairingError::DuplicatePresentSeq {
                                present_seq: seq,
                                first,
                                second: frame.session_frame,
                            });
                        }
                    } else {
                        consumed.insert(seq, frame.session_frame);
                    }
                    referenced.insert(seq);
                    paired += 1;
                    (
                        Some(PairedDepth {
                            present_seq: seq,
                            depth_name: depth_name_for(frame.session_frame),
                            dsv_id: d.dsv_id.clone(),
                            inherited: false,
                        }),
                        None,
                    )
                }
            },
        };

        pairs.push(FramePair {
            session_frame: frame.session_frame,
            segment_index: frame.segment_index,
            segment_frame: frame.segment_frame,
            source_qpc: frame.source_qpc,
            duplicate: false,
            camera_mode: frame.camera_mode,
            present_seq: frame.watermark_present_seq,
            depth,
            depth_absent_reason: reason,
        });
        previous_present_seq = frame.watermark_present_seq;
    }

    let unreferenced = depth_frames
        .iter()
        .filter(|d| !referenced.contains(&d.present_seq))
        .count() as u64;

    let report = PairingReport {
        total_frames: rgb_frames.len() as u64,
        eligible_even_frames: eligible,
        paired_even_frames: paired,
        inherited_frames: inherited_count,
        unreferenced_depth_frames: unreferenced,
        coverage: if eligible == 0 {
            0.0
        } else {
            paired as f64 / eligible as f64
        },
    };

    Ok((pairs, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(session_frame: u64, seq: Option<u64>) -> RgbFrame {
        RgbFrame {
            session_frame,
            segment_index: 0,
            segment_frame: session_frame as u32,
            source_qpc: session_frame as i64 * 1000,
            duplicate: false,
            camera_mode: CameraMode::Player,
            watermark_present_seq: seq,
        }
    }

    fn depth(seq: u64) -> DepthFrame {
        DepthFrame {
            present_seq: seq,
            dsv_id: "main-scene-v1".into(),
        }
    }

    #[test]
    fn odd_frames_are_absent_by_spec_not_by_failure() {
        let rgbs = vec![rgb(0, Some(0)), rgb(1, Some(1)), rgb(2, Some(2))];
        let depths = vec![depth(0), depth(2)];
        let (pairs, report) = build(&rgbs, &depths).unwrap();

        assert_eq!(pairs[1].depth, None);
        assert_eq!(
            pairs[1].depth_absent_reason,
            Some(AbsentReason::OddFrameBySpec)
        );
        // 奇数帧不进分母
        assert_eq!(report.eligible_even_frames, 2);
        assert_eq!(report.paired_even_frames, 2);
        assert!((report.coverage - 1.0).abs() < 1e-12);
    }

    #[test]
    fn naming_contract_is_enforced_by_construction() {
        let rgbs = vec![rgb(12344, Some(20480))];
        let depths = vec![depth(20480)];
        let (pairs, _) = build(&rgbs, &depths).unwrap();
        assert_eq!(pairs[0].depth.as_ref().unwrap().depth_name, "depth_12344");
    }

    #[test]
    fn duplicate_frames_inherit_and_are_flagged() {
        // 不变式 5：唯一允许的复用，且必须显式标记
        let mut dup = rgb(2, None);
        dup.duplicate = true;
        let rgbs = vec![rgb(0, Some(0)), dup];
        let depths = vec![depth(0)];
        let (pairs, report) = build(&rgbs, &depths).unwrap();

        let inherited = pairs[1].depth.as_ref().unwrap();
        assert!(inherited.inherited);
        assert_eq!(inherited.present_seq, 0);
        assert_eq!(report.inherited_frames, 1);
        // duplicate 帧不进分母（也不进分子）
        assert_eq!(report.eligible_even_frames, 1);
    }

    #[test]
    fn leading_duplicate_is_an_error_not_a_guess() {
        let mut dup = rgb(0, None);
        dup.duplicate = true;
        let err = build(&[dup], &[]).unwrap_err();
        assert_eq!(err, PairingError::LeadingDuplicate { session_frame: 0 });
    }

    #[test]
    fn superseded_depth_is_counted_but_never_reassigned() {
        // 不变式 6：CFR slot 丢弃的 source frame 对应的 depth 不得配给邻近帧。
        // present_seq 1 没有任何 RGB 帧引用它。
        let rgbs = vec![rgb(0, Some(0)), rgb(2, Some(2))];
        let depths = vec![depth(0), depth(1), depth(2)];
        let (pairs, report) = build(&rgbs, &depths).unwrap();

        assert_eq!(report.unreferenced_depth_frames, 1);
        // 关键：没有任何一帧偷偷拿了 seq=1
        for p in &pairs {
            if let Some(d) = &p.depth {
                assert_ne!(d.present_seq, 1);
            }
        }
    }

    #[test]
    fn missing_depth_is_null_never_a_neighbour() {
        // 不变式 2：没有精确匹配就是 None，绝不用相邻帧顶替
        let rgbs = vec![rgb(0, Some(0)), rgb(2, Some(2)), rgb(4, Some(4))];
        let depths = vec![depth(0), depth(4)]; // 缺 seq=2
        let (pairs, report) = build(&rgbs, &depths).unwrap();

        assert_eq!(pairs[1].depth, None);
        assert_eq!(
            pairs[1].depth_absent_reason,
            Some(AbsentReason::ProbeDropped)
        );
        assert_eq!(report.paired_even_frames, 2);
        assert_eq!(report.eligible_even_frames, 3);
        assert!((report.coverage - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn one_to_one_violation_aborts_instead_of_producing_a_table() {
        // 不变式 1：两个 session_frame 引用同一个 present_seq
        let rgbs = vec![rgb(0, Some(7)), rgb(2, Some(7))];
        let depths = vec![depth(7)];
        let err = build(&rgbs, &depths).unwrap_err();
        assert_eq!(
            err,
            PairingError::DuplicatePresentSeq {
                present_seq: 7,
                first: 0,
                second: 2
            }
        );
    }

    #[test]
    fn consecutive_source_identity_aliases_share_one_exact_depth() {
        let rgbs = vec![rgb(0, Some(7)), rgb(1, Some(7)), rgb(2, Some(7))];
        let (pairs, report) = build(&rgbs, &[depth(7)]).unwrap();
        assert_eq!(pairs[0].depth.as_ref().unwrap().present_seq, 7);
        assert_eq!(pairs[2].depth.as_ref().unwrap().present_seq, 7);
        assert!(!pairs[2].depth.as_ref().unwrap().inherited);
        assert_eq!(pairs[2].depth.as_ref().unwrap().depth_name, "depth_2");
        assert_eq!(report.paired_even_frames, 2);
        assert_eq!(report.unreferenced_depth_frames, 0);
    }

    #[test]
    fn duplicate_flag_cannot_change_source_identity() {
        let mut duplicate = rgb(1, Some(8));
        duplicate.duplicate = true;
        let error = build(&[rgb(0, Some(7)), duplicate], &[depth(7)]).unwrap_err();
        assert_eq!(
            error,
            PairingError::DuplicateSourceMismatch {
                session_frame: 1,
                previous_present_seq: 7,
                current_present_seq: 8,
            }
        );
    }

    #[test]
    fn undecodable_watermark_is_distinguishable_from_probe_drop() {
        // 两种缺失原因必须能分开，否则排障时无法定位是水印坏了还是 probe 丢帧
        let rgbs = vec![rgb(0, None), rgb(2, Some(2))];
        let depths = vec![depth(2)];
        let (pairs, _) = build(&rgbs, &depths).unwrap();
        assert_eq!(
            pairs[0].depth_absent_reason,
            Some(AbsentReason::WatermarkUndecodable)
        );
    }

    #[test]
    fn non_monotonic_input_is_rejected() {
        let rgbs = vec![rgb(4, Some(4)), rgb(2, Some(2))];
        let err = build(&rgbs, &[depth(4), depth(2)]).unwrap_err();
        assert_eq!(
            err,
            PairingError::NotMonotonic {
                previous: 4,
                current: 2
            }
        );
    }

    #[test]
    fn coverage_gate_rejects_a_bad_session() {
        // 30 个可配对偶数帧里丢了 2 个 → 0.933，低于 0.98 门限
        let rgbs: Vec<RgbFrame> = (0..30).map(|i| rgb(i * 2, Some(i * 2))).collect();
        let depths: Vec<DepthFrame> = (0..28).map(|i| depth(i * 2)).collect();
        let (_, report) = build(&rgbs, &depths).unwrap();
        assert!(report.coverage < PairingReport::MIN_COVERAGE);
        assert!(!report.passes_coverage_gate());
    }

    #[test]
    fn camera_mode_is_carried_through_for_freecam_segregation() {
        // v5 §8.5.3：free-cam 段必须可识别，才能不混入同一条交付数据
        let mut f = rgb(0, Some(0));
        f.camera_mode = CameraMode::Freecam;
        let (pairs, _) = build(&[f], &[depth(0)]).unwrap();
        assert_eq!(pairs[0].camera_mode, CameraMode::Freecam);
    }

    #[test]
    fn serialises_to_the_documented_shape() {
        let (pairs, _) = build(&[rgb(12344, Some(20480))], &[depth(20480)]).unwrap();
        let v = serde_json::to_value(&pairs[0]).unwrap();
        assert_eq!(v["session_frame"], 12344);
        assert_eq!(v["camera_mode"], "player");
        assert_eq!(v["depth"]["depth_name"], "depth_12344");
        assert_eq!(v["depth"]["inherited"], false);
        // 无缺失时不应出现 depth_absent_reason 字段
        assert!(v.get("depth_absent_reason").is_none());
    }
}
