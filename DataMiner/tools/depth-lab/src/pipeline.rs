//! E2E 配对管线：把三份输入合成一份 `frame-pairs.jsonl` 和一份可判定的报告。
//!
//! # 三份输入与它们的来源
//!
//! | 输入 | 谁产的 | 平台 |
//! |---|---|---|
//! | `frames.jsonl.zst` | DataMiner recorder（生产链路，一行未改） | Windows |
//! | `depth-index.jsonl` | probe 的 writer | Windows |
//! | `watermark-samples.jsonl` | MP4 水印提取器 | Windows（需 Media Foundation 解 H.264） |
//!
//! **接缝刻意定在「块灰度均值」而不是「像素」**：仓库里没有 H.264 解码器
//! （`mp4` crate 只解容器不解码，FFmpeg 被明令禁止），所以解码只能在 Windows 侧做。
//! 把接缝定在 24 个 `u8` 上，配对逻辑就完全平台无关、可以在任意机器上测。

use serde::{Deserialize, Serialize};

use crate::pairing::{CameraMode, DepthFrame, PairingError, PairingReport, RgbFrame, build};
use crate::watermark::{self, DecodeError};

/// recorder 侧一行 `frames.jsonl`（只取配对需要的字段）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecorderFrameRow {
    pub session_frame: u64,
    #[serde(default)]
    pub segment_index: u32,
    #[serde(default)]
    pub segment_frame: u32,
    #[serde(default)]
    pub source_qpc: i64,
    #[serde(default)]
    pub duplicate: bool,
    /// selected-present 路径由 recorder 在线确认的完整 Present identity。
    /// 旧证据没有该字段时保持 `None`，只能走 legacy 离线 unwrap。
    #[serde(default)]
    pub present_seq: Option<u64>,
}

/// 提取器从 `video.mp4` 的水印矩形量出的一帧。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WatermarkSample {
    pub session_frame: u64,
    /// 24 个块的中心灰度均值，块序与 [`crate::watermark::encode`] 一致。
    pub block_means: Vec<u8>,
}

/// probe 侧 `depth-index.jsonl` 的一行（只取配对需要的字段）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DepthIndexRow {
    pub present_seq: u64,
    #[serde(default)]
    pub dsv_id: String,
    /// 几何。配对本身用不到，但 `materialize` 需要它把 raw 字节还原成二维——
    /// 没有它就只能靠猜或另开一个来源，那是错位的温床。
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// 水印解码的分项统计。**解不出来的原因必须可分**——
/// 「没水印」「被后处理糊了」「压根没这一帧」是三种完全不同的故障。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatermarkStats {
    pub decoded: u64,
    pub sync_mismatch: u64,
    pub ambiguous: u64,
    pub wrong_sample_count: u64,
    pub missing: u64,
    /// MP4 ROI 解出的低 16 位与 recorder 在线确认的完整 identity 不一致。
    #[serde(default)]
    pub identity_mismatch: u64,
}

/// E2E 的一次判定结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E2eReport {
    pub pairing: PairingReport,
    pub watermark: WatermarkStats,
    /// 覆盖率与水印两道门是否都过。
    pub passed: bool,
    /// 没过时的人话原因。
    pub failures: Vec<String>,
}

/// 水印解码率门限。低于它说明水印链路本身有问题，配对覆盖率就没有参考价值了。
pub const MIN_WATERMARK_RATE: f64 = 0.98;
pub(crate) const DECODE_MARGIN: u8 = 32;

/// selected-present 路径只把 MP4 水印当作低 16 位一致性证据，完整身份以 recorder
/// 在线确认值为准。全旧格式输入仍保留 legacy 连续 unwrap，供历史证据读取。
pub(crate) fn decode_present_identities(
    frames: &[RecorderFrameRow],
    samples: &[WatermarkSample],
) -> Result<(Vec<Option<u64>>, WatermarkStats), PairingError> {
    use std::collections::HashMap;

    let by_frame: HashMap<u64, &WatermarkSample> = samples
        .iter()
        .map(|sample| (sample.session_frame, sample))
        .collect();
    let selected_present_mode = frames.iter().any(|frame| frame.present_seq.is_some());
    if selected_present_mode
        && let Some(frame) = frames.iter().find(|frame| frame.present_seq.is_none())
    {
        return Err(PairingError::MissingPresentIdentity {
            session_frame: frame.session_frame,
        });
    }
    if selected_present_mode {
        for window in frames.windows(2) {
            let previous = &window[0];
            let current = &window[1];
            let previous_seq = previous
                .present_seq
                .expect("selected-present mode was checked as complete");
            let current_seq = current
                .present_seq
                .expect("selected-present mode was checked as complete");
            let contiguous_alias = current_seq == previous_seq
                && previous.session_frame.checked_add(1) == Some(current.session_frame);
            if current_seq < previous_seq || (current_seq == previous_seq && !contiguous_alias) {
                return Err(PairingError::PresentIdentityNotMonotonic {
                    previous_session_frame: previous.session_frame,
                    previous_present_seq: previous_seq,
                    session_frame: current.session_frame,
                    present_seq: current_seq,
                });
            }
        }
    }

    let mut stats = WatermarkStats::default();
    let mut legacy_previous = 0u64;
    let mut legacy_started = false;
    let identities = frames
        .iter()
        .map(|frame| match by_frame.get(&frame.session_frame) {
            None => {
                stats.missing += 1;
                None
            }
            Some(sample) => match watermark::decode(&sample.block_means, DECODE_MARGIN) {
                Ok(raw) => {
                    if selected_present_mode {
                        let full = frame
                            .present_seq
                            .expect("selected-present mode was checked as complete");
                        if raw == full as u16 {
                            stats.decoded += 1;
                            Some(full)
                        } else {
                            stats.identity_mismatch += 1;
                            None
                        }
                    } else {
                        let full = if legacy_started {
                            watermark::unwrap_seq(legacy_previous, raw)
                        } else {
                            legacy_started = true;
                            u64::from(raw)
                        };
                        legacy_previous = full;
                        stats.decoded += 1;
                        Some(full)
                    }
                }
                Err(DecodeError::IntegrityMismatch) => {
                    stats.sync_mismatch += 1;
                    None
                }
                Err(DecodeError::AmbiguousBlock) => {
                    stats.ambiguous += 1;
                    None
                }
                Err(DecodeError::WrongSampleCount) => {
                    stats.wrong_sample_count += 1;
                    None
                }
            },
        })
        .collect();
    Ok((identities, stats))
}

/// 跑一次完整配对。
///
/// `camera_mode` 由 session 元信息给（free-cam 段与玩家段不得混算，见 77 号 §8.5.3）。
pub fn run(
    frames: &[RecorderFrameRow],
    samples: &[WatermarkSample],
    depth: &[DepthIndexRow],
    camera_mode: CameraMode,
) -> Result<(Vec<crate::pairing::FramePair>, E2eReport), PairingError> {
    let (present_identities, stats) = decode_present_identities(frames, samples)?;

    run_with_present_identities(frames, present_identities, stats, depth, camera_mode)
}

/// Production selected-Present finalization consumes the full identity that
/// the WGC ROI decoder already validated online and attached to every CFR row.
/// MP4 re-decoding is a separate video-robustness diagnostic: scaling the RGB
/// output must not be allowed to replace or invalidate the authoritative join.
pub fn run_selected_present(
    frames: &[RecorderFrameRow],
    depth: &[DepthIndexRow],
    camera_mode: CameraMode,
) -> Result<(Vec<crate::pairing::FramePair>, E2eReport), PairingError> {
    if let Some(frame) = frames.iter().find(|frame| frame.present_seq.is_none()) {
        return Err(PairingError::MissingPresentIdentity {
            session_frame: frame.session_frame,
        });
    }
    for window in frames.windows(2) {
        let previous = &window[0];
        let current = &window[1];
        let previous_seq = previous
            .present_seq
            .expect("selected-present mode was checked as complete");
        let current_seq = current
            .present_seq
            .expect("selected-present mode was checked as complete");
        let contiguous_alias = current_seq == previous_seq
            && previous.session_frame.checked_add(1) == Some(current.session_frame);
        if current_seq < previous_seq || (current_seq == previous_seq && !contiguous_alias) {
            return Err(PairingError::PresentIdentityNotMonotonic {
                previous_session_frame: previous.session_frame,
                previous_present_seq: previous_seq,
                session_frame: current.session_frame,
                present_seq: current_seq,
            });
        }
    }
    let present_identities = frames.iter().map(|frame| frame.present_seq).collect();
    let stats = WatermarkStats {
        decoded: frames.len() as u64,
        ..WatermarkStats::default()
    };
    run_with_present_identities(frames, present_identities, stats, depth, camera_mode)
}

fn run_with_present_identities(
    frames: &[RecorderFrameRow],
    present_identities: Vec<Option<u64>>,
    stats: WatermarkStats,
    depth: &[DepthIndexRow],
    camera_mode: CameraMode,
) -> Result<(Vec<crate::pairing::FramePair>, E2eReport), PairingError> {
    let rgb: Vec<RgbFrame> = frames
        .iter()
        .zip(present_identities)
        .map(|(f, seq)| RgbFrame {
            session_frame: f.session_frame,
            segment_index: f.segment_index,
            segment_frame: f.segment_frame,
            source_qpc: f.source_qpc,
            duplicate: f.duplicate,
            camera_mode,
            watermark_present_seq: seq,
        })
        .collect();

    let depth_frames: Vec<DepthFrame> = depth
        .iter()
        .map(|d| DepthFrame {
            present_seq: d.present_seq,
            dsv_id: if d.dsv_id.is_empty() {
                "unknown".to_string()
            } else {
                d.dsv_id.clone()
            },
        })
        .collect();

    let (pairs, pairing) = build(&rgb, &depth_frames)?;

    let total_samples = frames.len() as f64;
    let wm_rate = if total_samples == 0.0 {
        0.0
    } else {
        stats.decoded as f64 / total_samples
    };

    let mut failures = Vec::new();
    if wm_rate < MIN_WATERMARK_RATE {
        failures.push(format!(
            "水印解码率 {:.4} 低于门限 {MIN_WATERMARK_RATE}（成功 {} / 总帧 {}；\
             同步字不符 {}、模糊 {}、缺样本 {}、在线身份不一致 {}）——先查水印链路，覆盖率此时没有参考价值",
            wm_rate,
            stats.decoded,
            frames.len(),
            stats.sync_mismatch,
            stats.ambiguous,
            stats.missing,
            stats.identity_mismatch
        ));
    }
    if !pairing.passes_coverage_gate() {
        failures.push(format!(
            "配对覆盖率 {:.4} 低于门限 {}（可配对偶数帧 {}，实际配上 {}）",
            pairing.coverage,
            PairingReport::MIN_COVERAGE,
            pairing.eligible_even_frames,
            pairing.paired_even_frames
        ));
    }

    let report = E2eReport {
        pairing,
        watermark: stats,
        passed: failures.is_empty(),
        failures,
    };
    Ok((pairs, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn means_for(seq: u16) -> Vec<u8> {
        let w = watermark::encode(seq, 0, 0);
        let mut m = vec![0u8; watermark::TOTAL_BLOCKS as usize];
        for r in &w.white {
            m[(r.left / watermark::BLOCK_PX) as usize] = 255;
        }
        m
    }

    /// N 帧，偶数帧有 depth；`present_seq` 与 `session_frame` 同步递增。
    fn scenario(
        n: u64,
    ) -> (
        Vec<RecorderFrameRow>,
        Vec<WatermarkSample>,
        Vec<DepthIndexRow>,
    ) {
        let frames: Vec<RecorderFrameRow> = (0..n)
            .map(|i| RecorderFrameRow {
                session_frame: i,
                segment_index: 0,
                segment_frame: i as u32,
                source_qpc: i as i64 * 1000,
                duplicate: false,
                present_seq: None,
            })
            .collect();
        let samples: Vec<WatermarkSample> = (0..n)
            .map(|i| WatermarkSample {
                session_frame: i,
                block_means: means_for(i as u16),
            })
            .collect();
        let depth: Vec<DepthIndexRow> = (0..n)
            .filter(|i| i.is_multiple_of(2))
            .map(|i| DepthIndexRow {
                present_seq: i,
                dsv_id: "main-scene-v1".into(),
                width: 1280,
                height: 720,
            })
            .collect();
        (frames, samples, depth)
    }

    #[test]
    fn a_clean_run_passes_both_gates() {
        let (f, s, d) = scenario(100);
        let (pairs, rep) = run(&f, &s, &d, CameraMode::Player).unwrap();
        assert_eq!(pairs.len(), 100);
        assert!(rep.passed, "失败原因: {:?}", rep.failures);
        assert_eq!(rep.watermark.decoded, 100);
        assert!((rep.pairing.coverage - 1.0).abs() < 1e-12);
    }

    #[test]
    fn selected_present_finalization_uses_online_full_identity_without_mp4_samples() {
        let (mut frames, _samples, depth) = scenario(100);
        for frame in &mut frames {
            frame.present_seq = Some(frame.session_frame);
        }
        let (pairs, report) = run_selected_present(&frames, &depth, CameraMode::Player).unwrap();

        assert!(report.passed, "{:?}", report.failures);
        assert_eq!(report.watermark.decoded, 100);
        assert_eq!(report.pairing.paired_even_frames, 50);
        assert_eq!(pairs[0].present_seq, Some(0));
    }

    #[test]
    fn a_broken_watermark_is_reported_before_coverage() {
        // 水印全糊 → 覆盖率必然也垮，但报告要先指向水印，
        // 否则会去查 probe 丢帧而不是查画面。
        let (f, mut s, d) = scenario(100);
        for x in &mut s {
            x.block_means = vec![128u8; watermark::TOTAL_BLOCKS as usize];
        }
        let (_, rep) = run(&f, &s, &d, CameraMode::Player).unwrap();
        assert!(!rep.passed);
        assert_eq!(rep.watermark.ambiguous, 100);
        assert!(rep.failures[0].contains("水印解码率"), "{:?}", rep.failures);
    }

    #[test]
    fn watermark_failure_modes_are_distinguishable() {
        // 「没水印」「被糊了」「压根没这帧」是三种不同故障，不能混成一个数
        let (f, mut s, d) = scenario(10);
        s[0].block_means = vec![0u8; watermark::TOTAL_BLOCKS as usize]; // 同步字不符
        s[1].block_means = vec![128u8; watermark::TOTAL_BLOCKS as usize]; // 模糊
        s[2].block_means = vec![255u8; 3]; // 样本数不对
        s.remove(3); // 缺帧
        let (_, rep) = run(&f, &s, &d, CameraMode::Player).unwrap();
        assert_eq!(rep.watermark.sync_mismatch, 1);
        assert_eq!(rep.watermark.ambiguous, 1);
        assert_eq!(rep.watermark.wrong_sample_count, 1);
        assert_eq!(rep.watermark.missing, 1);
    }

    #[test]
    fn probe_dropping_frames_shows_up_as_coverage_not_as_watermark() {
        // 反过来：水印好好的，是 probe 丢了帧 → 只应报覆盖率
        let (f, s, mut d) = scenario(100);
        d.truncate(40); // 50 个偶数帧只有 40 个有 depth
        let (_, rep) = run(&f, &s, &d, CameraMode::Player).unwrap();
        assert!(!rep.passed);
        assert_eq!(rep.watermark.decoded, 100, "水印本身没问题");
        assert!(rep.failures.iter().all(|x| x.contains("覆盖率")));
        assert!((rep.pairing.coverage - 0.8).abs() < 1e-9);
    }

    #[test]
    fn survives_the_16bit_wraparound() {
        // present_seq 是 u16，60fps 下约 18 分钟绕一圈。
        // 跨绕回时展开必须单调，否则配对表会在那一刻整段错位。
        let n = 70_000u64;
        let frames: Vec<RecorderFrameRow> = (0..n)
            .map(|i| RecorderFrameRow {
                session_frame: i,
                segment_index: 0,
                segment_frame: 0,
                source_qpc: 0,
                duplicate: false,
                present_seq: None,
            })
            .collect();
        let samples: Vec<WatermarkSample> = (0..n)
            .map(|i| WatermarkSample {
                session_frame: i,
                block_means: means_for((i % 65536) as u16),
            })
            .collect();
        let depth: Vec<DepthIndexRow> = (0..n)
            .filter(|i| i.is_multiple_of(2))
            .map(|i| DepthIndexRow {
                present_seq: i,
                dsv_id: "m".into(),
                width: 1280,
                height: 720,
            })
            .collect();
        let (_, rep) = run(&frames, &samples, &depth, CameraMode::Player).unwrap();
        assert!(rep.passed, "跨绕回失败: {:?}", rep.failures);
    }

    #[test]
    fn selected_present_uses_full_online_identity_across_high_epoch_wrap_and_alias() {
        let full = [131_070, 131_071, 131_071, 131_072, 131_073, 131_074];
        let frames: Vec<_> = full
            .iter()
            .enumerate()
            .map(|(session_frame, &present_seq)| RecorderFrameRow {
                session_frame: session_frame as u64,
                segment_index: 0,
                segment_frame: session_frame as u32,
                source_qpc: session_frame as i64 * 1_000,
                duplicate: false,
                present_seq: Some(present_seq),
            })
            .collect();
        let samples: Vec<_> = full
            .iter()
            .enumerate()
            .map(|(session_frame, &present_seq)| WatermarkSample {
                session_frame: session_frame as u64,
                block_means: means_for(present_seq as u16),
            })
            .collect();
        let depth = [131_070, 131_071, 131_073]
            .into_iter()
            .map(|present_seq| DepthIndexRow {
                present_seq,
                dsv_id: "main-scene-v1".into(),
                width: 1280,
                height: 720,
            })
            .collect::<Vec<_>>();
        let (pairs, report) = run(&frames, &samples, &depth, CameraMode::Player).unwrap();
        assert!(report.passed, "{:?}", report.failures);
        assert_eq!(pairs[0].depth.as_ref().unwrap().present_seq, 131_070);
        assert_eq!(pairs[2].depth.as_ref().unwrap().present_seq, 131_071);
        assert_eq!(pairs[4].depth.as_ref().unwrap().present_seq, 131_073);
        assert_eq!(report.watermark.identity_mismatch, 0);
    }

    #[test]
    fn selected_present_rejects_mixed_full_identity_and_low16_mismatch() {
        let (mut frames, mut samples, depth) = scenario(4);
        frames[0].present_seq = Some(70_000);
        assert!(matches!(
            run(&frames, &samples, &depth, CameraMode::Player),
            Err(PairingError::MissingPresentIdentity { session_frame: 1 })
        ));

        for (index, frame) in frames.iter_mut().enumerate() {
            frame.present_seq = Some(70_000 + index as u64);
            samples[index].block_means = means_for((70_000 + index as u64) as u16);
        }
        samples[2].block_means = means_for(123);
        let matching_depth = [70_000, 70_002]
            .into_iter()
            .map(|present_seq| DepthIndexRow {
                present_seq,
                dsv_id: "main-scene-v1".into(),
                width: 1280,
                height: 720,
            })
            .collect::<Vec<_>>();
        let (_, report) = run(&frames, &samples, &matching_depth, CameraMode::Player).unwrap();
        assert!(!report.passed);
        assert_eq!(report.watermark.identity_mismatch, 1);
    }

    #[test]
    fn selected_present_rejects_a_full_identity_regression_before_pairing() {
        let (mut frames, mut samples, depth) = scenario(4);
        let full = [131_070, 131_071, 131_069, 131_072];
        for (index, present_seq) in full.into_iter().enumerate() {
            frames[index].present_seq = Some(present_seq);
            samples[index].block_means = means_for(present_seq as u16);
        }
        assert!(matches!(
            run(&frames, &samples, &depth, CameraMode::Player),
            Err(PairingError::PresentIdentityNotMonotonic {
                previous_session_frame: 1,
                previous_present_seq: 131_071,
                session_frame: 2,
                present_seq: 131_069,
            })
        ));
    }

    #[test]
    fn duplicate_frames_do_not_count_against_coverage() {
        // WGC duplicate 帧没有新渲染，按 §5.7.1 不变式 5 继承且不进分母
        let (mut f, mut s, mut d) = scenario(20);
        f[4].duplicate = true;
        f[6].duplicate = true;
        s[4].block_means = means_for(3);
        s[6].block_means = means_for(5);
        d.push(DepthIndexRow {
            present_seq: 3,
            dsv_id: "main-scene-v1".into(),
            width: 1280,
            height: 720,
        });
        d.push(DepthIndexRow {
            present_seq: 5,
            dsv_id: "main-scene-v1".into(),
            width: 1280,
            height: 720,
        });
        let (pairs, rep) = run(&f, &s, &d, CameraMode::Player).unwrap();
        assert!(rep.passed, "{:?}", rep.failures);
        assert!(pairs[4].depth.as_ref().unwrap().inherited);
        assert_eq!(rep.pairing.inherited_frames, 2);
    }

    #[test]
    fn freecam_mode_is_carried_into_every_pair() {
        let (f, s, d) = scenario(10);
        let (pairs, _) = run(&f, &s, &d, CameraMode::Freecam).unwrap();
        assert!(pairs.iter().all(|p| p.camera_mode == CameraMode::Freecam));
    }

    #[test]
    fn report_serialises_for_the_manifest() {
        let (f, s, d) = scenario(10);
        let (_, rep) = run(&f, &s, &d, CameraMode::Player).unwrap();
        let v = serde_json::to_value(&rep).unwrap();
        assert_eq!(v["passed"], true);
        assert!(v["pairing"]["coverage"].is_number());
        assert!(v["watermark"]["decoded"].is_number());
    }
}
