//! clip-plan —— 相位对齐选帧:从一段录制构造「每个偶数输出帧都有 depth」的最长子序列。
//!
//! # 为什么需要它(docs/102 §3 G2 / §4)
//!
//! 交付契约把 depth 绑在**视频偶数帧**上,而 probe 按时间节拍采样 present。录制链一旦
//! 出现重复帧(WGC 补帧/游戏卡顿),视频帧与 present 的奇偶对应就翻转一位;probe 若
//! 恰好锁在某一奇偶上,翻转后的偶数视频帧需要的 present 根本没被采——0806 的 gen1
//! 正式采集实测 48 处翻转,连续合规段最长只有 39.1 秒(#573)。
//!
//! 本模块把两条 120s 业务核验包(`2026_08_06_FO4Commonwealth_120s_01/_02`)使用的
//! 选帧算法落库:顺序扫描,**只允许两类剔帧**——
//!
//! - [`DropReason::Duplicate`]:与上一保留帧同源(同 `source_qpc` 或同水印 present)。
//!   本来就该剔,剔除后画面更顺;
//! - [`DropReason::Realign`]:该帧落在偶数输出位上,但其 present 没有 depth
//!   (奇偶相位错开或索引空洞)。剔一帧让后续序列整体移一位,重新对齐。
//!
//! 输出选帧清单与逐帧剔除原因,交 `deliver` 前由调用方按清单剪辑视频与 frames。
//!
//! # 采样策略切到全 present(docs/102 裁决 A)之后
//!
//! 深度对两种奇偶都存在时,`Realign` 剔帧应恒为 0——届时本工具从救数工具转为回归断言:
//! 计划报告里 `realign_drops > 0` 即采集回归。
//! [`ClipPlan::realign_drops`] 单列正是为了这个断言好写。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::pipeline::{
    DepthIndexRow, RecorderFrameRow, WatermarkSample, WatermarkStats, decode_present_identities,
};

pub const CLIP_PLAN_SCHEMA_V1: &str = "clip-plan/v1";

/// 连续 `Realign` 剔帧上限。超过即断窗,而不是继续烧帧硬穿索引空洞:
/// 剔 1–3 帧是 ≤100ms 的微跳(两条业务核验包的实测口径),
/// 再往上就是肉眼可见的内容跳变——那不该由选帧器静默吞掉,该让窗口在此终结,
/// 由报告把「空洞在哪」摆到人面前。`Duplicate` 剔帧不受此限:内容本来就是冻结的,
/// 剔多少都不产生跳变。
const MAX_CONSECUTIVE_REALIGN: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// 与上一保留帧同源(同 `source_qpc` 或同水印 present)。
    Duplicate,
    /// 偶数输出位缺 depth,剔帧重对齐(相位翻转或索引空洞)。
    Realign,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedDrop {
    pub session_frame: u64,
    pub reason: DropReason,
}

/// 一次选帧计划。`selected` 是**源** `session_frame` 的有序清单;
/// 调用方按它剪辑视频/frames 后,新序列从 0 重新编号。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipPlan {
    pub schema: String,
    pub target_frames: usize,
    pub achieved_frames: usize,
    pub met_target: bool,
    /// 选中区间的源起止(含端点;区间内未选中的帧都在 `drops` 里,一帧不漏)。
    pub start_session_frame: u64,
    pub end_session_frame: u64,
    pub selected: Vec<u64>,
    pub drops: Vec<PlannedDrop>,
    pub duplicate_drops: usize,
    /// docs/102 裁决 A 落地后的回归断言位:全 present 采集下应恒为 0。
    pub realign_drops: usize,
    pub watermark: WatermarkStats,
}

/// 复用 pairing 管线的身份判定：selected-present 使用 recorder 的 full identity，
/// MP4 水印只验证低 16 位；全旧格式输入才执行 legacy unwrap。
fn decode_watermarks(
    frames: &[RecorderFrameRow],
    samples: &[WatermarkSample],
) -> Result<(Vec<Option<u64>>, WatermarkStats), String> {
    decode_present_identities(frames, samples).map_err(|error| error.to_string())
}

/// 从起点 `s` 贪心构造:返回 (选中下标, 剔除清单)。
///
/// 刻意保持与两条 120s 包的会话算法逐语义一致:
/// - 偶数输出位:present 必须已解码、且在 depth 集合里,否则剔帧(`Realign`);
/// - 任意位:与上一保留帧同源即剔(`Duplicate`)——`source_qpc` 相同,或双方都解码且水印相同;
/// - 奇数输出位不要求 depth,解码失败也保留(交付契约只按偶数帧配对)。
fn build_from(
    frames: &[RecorderFrameRow],
    wm: &[Option<u64>],
    depth_set: &HashSet<u64>,
    start: usize,
    target: usize,
) -> (Vec<usize>, Vec<PlannedDrop>) {
    let mut sel: Vec<usize> = Vec::new();
    let mut drops = Vec::new();
    let mut consecutive_realign = 0usize;
    let mut k = start;
    while k < frames.len() && sel.len() < target {
        let dup_of_prev = frames[k].duplicate
            || sel.last().is_some_and(|&p| {
                frames[k].source_qpc == frames[p].source_qpc || (wm[k].is_some() && wm[k] == wm[p])
            });
        if dup_of_prev {
            drops.push(PlannedDrop {
                session_frame: frames[k].session_frame,
                reason: DropReason::Duplicate,
            });
            k += 1;
            continue;
        }
        if sel.len().is_multiple_of(2) {
            let ok = wm[k].is_some_and(|v| depth_set.contains(&v));
            if !ok {
                consecutive_realign += 1;
                if consecutive_realign > MAX_CONSECUTIVE_REALIGN {
                    // 索引空洞不是相位问题,烧帧硬穿会造出肉眼可见的跳变——断窗。
                    break;
                }
                drops.push(PlannedDrop {
                    session_frame: frames[k].session_frame,
                    reason: DropReason::Realign,
                });
                k += 1;
                continue;
            }
        }
        consecutive_realign = 0;
        sel.push(k);
        k += 1;
    }
    (sel, drops)
}

/// 全起点搜索,返回最优计划:优先「达到 target 且剔帧最少」,否则「最长」。
///
/// # Errors
///
/// `frames` 为空时报错——空输入下「最长窗口为 0」是掩盖上游断供的答案,拒绝比沉默好。
pub fn plan(
    frames: &[RecorderFrameRow],
    samples: &[WatermarkSample],
    depth: &[DepthIndexRow],
    target_frames: usize,
) -> Result<ClipPlan, String> {
    if frames.is_empty() {
        return Err("frames 为空:上游没给任何录制帧,不是「没有可用窗口」".into());
    }
    if target_frames == 0 || !target_frames.is_multiple_of(2) {
        return Err(format!(
            "target-frames 必须是正偶数(交付按偶数帧配对),收到 {target_frames}"
        ));
    }
    let (wm, stats) = decode_watermarks(frames, samples)?;
    let depth_set: HashSet<u64> = depth.iter().map(|d| d.present_seq).collect();

    let mut best: Option<(Vec<usize>, Vec<PlannedDrop>)> = None;
    for start in 0..frames.len() {
        let (sel, drops) = build_from(frames, &wm, &depth_set, start, target_frames);
        let better = match &best {
            None => true,
            Some((bsel, bdrops)) => {
                let (a_met, b_met) = (sel.len() >= target_frames, bsel.len() >= target_frames);
                match (a_met, b_met) {
                    (true, false) => true,
                    (false, true) => false,
                    (true, true) => drops.len() < bdrops.len(),
                    (false, false) => sel.len() > bsel.len(),
                }
            }
        };
        if better {
            best = Some((sel, drops));
        }
    }
    let (sel, mut drops) = best.expect("frames 非空则至少有一个候选");
    // 只保留选中区间内的剔除记录:区间外的「剔除」只是搜索的副产品,不是计划的一部分。
    let (lo, hi) = match (sel.first(), sel.last()) {
        (Some(&a), Some(&b)) => (frames[a].session_frame, frames[b].session_frame),
        _ => (0, 0),
    };
    drops.retain(|d| d.session_frame >= lo && d.session_frame <= hi);
    let duplicate_drops = drops
        .iter()
        .filter(|d| d.reason == DropReason::Duplicate)
        .count();
    let realign_drops = drops.len() - duplicate_drops;
    Ok(ClipPlan {
        schema: CLIP_PLAN_SCHEMA_V1.to_string(),
        target_frames,
        achieved_frames: sel.len(),
        met_target: sel.len() >= target_frames,
        start_session_frame: lo,
        end_session_frame: hi,
        selected: sel.iter().map(|&k| frames[k].session_frame).collect(),
        drops,
        duplicate_drops,
        realign_drops,
        watermark: stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watermark;

    fn frame(sf: u64, qpc: i64) -> RecorderFrameRow {
        RecorderFrameRow {
            session_frame: sf,
            segment_index: 0,
            segment_frame: 0,
            source_qpc: qpc,
            duplicate: false,
            present_seq: None,
        }
    }

    fn sample(sf: u64, present: u16) -> WatermarkSample {
        // encode 给出白/黑矩形;判决输入是 24 个块的中心灰度均值——白块 255、黑块 0。
        let rects = watermark::encode(present, 0, 0);
        let means: Vec<u8> = (0..watermark::TOTAL_BLOCKS)
            .map(|i| {
                let left = i * watermark::BLOCK_PX;
                if rects.white.iter().any(|r| r.left == left) {
                    255
                } else {
                    0
                }
            })
            .collect();
        WatermarkSample {
            session_frame: sf,
            block_means: means,
        }
    }

    fn depth_rows(seqs: &[u64]) -> Vec<DepthIndexRow> {
        seqs.iter()
            .map(|&s| DepthIndexRow {
                present_seq: s,
                dsv_id: String::new(),
                width: 0,
                height: 0,
            })
            .collect()
    }

    /// 干净序列:present 与帧一一对应、深度全偶采——满长、零剔帧。
    #[test]
    fn clean_sequence_reaches_target_with_zero_drops() {
        let n = 60u64;
        let frames: Vec<_> = (0..n).map(|k| frame(k, k as i64 * 100)).collect();
        let samples: Vec<_> = (0..n).map(|k| sample(k, 1000 + k as u16)).collect();
        let depth = depth_rows(
            &(0..n)
                .map(|k| 1000 + k)
                .filter(|s| s % 2 == 0)
                .collect::<Vec<_>>(),
        );
        let p = plan(&frames, &samples, &depth, 40).unwrap();
        assert!(p.met_target);
        assert_eq!(p.achieved_frames, 40);
        assert_eq!(p.drops.len(), 0);
        assert_eq!(p.realign_drops, 0);
    }

    #[test]
    fn selected_present_clip_plan_keeps_full_identity_across_low16_wrap() {
        let start = 131_068u64;
        let mut frames: Vec<_> = (0..8)
            .map(|offset| frame(offset, offset as i64 * 100))
            .collect();
        for (offset, frame) in frames.iter_mut().enumerate() {
            frame.present_seq = Some(start + offset as u64);
        }
        let samples: Vec<_> = (0..8)
            .map(|offset| sample(offset, (start + offset) as u16))
            .collect();
        let depth = depth_rows(
            &(0..8)
                .filter(|offset| offset % 2 == 0)
                .map(|offset| start + offset)
                .collect::<Vec<_>>(),
        );
        let plan = plan(&frames, &samples, &depth, 8).unwrap();
        assert!(plan.met_target, "{plan:?}");
        assert_eq!(plan.selected, (0..8).collect::<Vec<_>>());
        assert_eq!(plan.realign_drops, 0);
        assert_eq!(plan.watermark.identity_mismatch, 0);
    }

    #[test]
    fn a_duplicate_cannot_become_the_first_selected_frame() {
        let mut frames: Vec<_> = (0..8).map(|k| frame(k, k as i64 * 100)).collect();
        frames[0].duplicate = true;
        let samples: Vec<_> = (0..8).map(|k| sample(k, 1000 + k as u16)).collect();
        let depth = depth_rows(&(1000..1008).collect::<Vec<_>>());
        let p = plan(&frames, &samples, &depth, 4).unwrap();
        assert!(p.met_target);
        assert!(!p.selected.contains(&0));
        assert!(
            p.selected
                .iter()
                .all(|session_frame| !frames[*session_frame as usize].duplicate)
        );
    }

    /// 卡顿重复(present 停走 + 后续跳一格):重复帧被剔,序列继续,不因相位翻转而断。
    #[test]
    fn stall_duplicate_is_dropped_and_sequence_survives() {
        // present 序列:1000,1001,1002,1002,1004,1005,…(1002 重复一次,1003 被跳过)。
        // 总帧数刚好只比 target 多 1:任何绕开重复帧的起点都凑不够长度,
        // 逼最优解必须穿过重复帧——验证它被剔而不是终结序列。
        let mut presents: Vec<u16> = vec![1000, 1001, 1002, 1002];
        presents.extend(1004..1031);
        let frames: Vec<_> = (0..presents.len() as u64)
            .map(|k| {
                // 重复帧连 source_qpc 都与上一保留帧相同(客户端 mux 了同一源)
                let qpc = if k == 3 {
                    100 + 2 * 100
                } else {
                    100 + k as i64 * 100
                };
                frame(k, qpc)
            })
            .collect();
        let samples: Vec<_> = presents
            .iter()
            .enumerate()
            .map(|(k, &p)| sample(k as u64, p))
            .collect();
        // 深度对两种奇偶都在(全 present 采集,docs/102 裁决 A 目标态)
        let depth = depth_rows(&(1000..1031).collect::<Vec<_>>());
        let p = plan(&frames, &samples, &depth, 30).unwrap();
        assert!(p.met_target, "重复帧不应终结序列:{p:?}");
        assert_eq!(p.duplicate_drops, 1, "{:?}", p.drops);
        assert_eq!(p.realign_drops, 0, "全 present 深度下不允许出现重对齐剔帧");
    }

    /// 偶数锁定的深度 × 跳帧翻转相位:恰好一次 Realign 剔帧后恢复满配。
    #[test]
    fn phase_flip_costs_exactly_one_realign_drop() {
        // present:1000..1010 连续,然后跳过 1010(=偶),1011 起继续 → 偶位帧从此骑上奇数
        let mut presents: Vec<u16> = (1000..1010).collect();
        presents.extend(1011..1051);
        let frames: Vec<_> = (0..presents.len() as u64)
            .map(|k| frame(k, 100 + k as i64 * 100))
            .collect();
        let samples: Vec<_> = presents
            .iter()
            .enumerate()
            .map(|(k, &p)| sample(k as u64, p))
            .collect();
        let depth = depth_rows(&(1000..1051).filter(|s| s % 2 == 0).collect::<Vec<_>>());
        let p = plan(&frames, &samples, &depth, 40).unwrap();
        assert!(p.met_target, "{p:?}");
        assert_eq!(
            p.realign_drops, 1,
            "一次相位翻转应恰好付一帧代价:{:?}",
            p.drops
        );
        // 选中的偶位帧必须全部真的有 depth
        for (n, &sf) in p.selected.iter().enumerate() {
            if n % 2 == 0 {
                let v = 1000 + sf + u64::from(sf >= 10); // 跳过 1010 后 present 比帧号多 1
                assert_eq!(v % 2, 0, "偶位帧 {sf} 骑在奇数 present 上");
            }
        }
    }

    /// 索引大空洞:计划应落在空洞外的最长一侧,而不是硬穿过去。
    #[test]
    fn index_gap_bounds_the_window() {
        let n = 100u64;
        let frames: Vec<_> = (0..n).map(|k| frame(k, k as i64 * 100)).collect();
        let samples: Vec<_> = (0..n).map(|k| sample(k, 1000 + k as u16)).collect();
        // 偶数深度,但 1040..1060 整段缺失
        let depth = depth_rows(
            &(1000..1100)
                .filter(|s| s % 2 == 0 && !(1040..1060).contains(s))
                .collect::<Vec<_>>(),
        );
        let p = plan(&frames, &samples, &depth, 90).unwrap();
        assert!(!p.met_target);
        // 空洞后侧 1060..1100 有 40 帧连续 → 最长可构造 40
        assert_eq!(p.achieved_frames, 40, "{p:?}");
        assert_eq!(p.realign_drops, 0);
    }

    /// 输入校验:空 frames 与奇数 target 都必须拒绝。
    #[test]
    fn rejects_empty_frames_and_odd_target() {
        let s = [sample(0, 1)];
        let d = depth_rows(&[0]);
        assert!(plan(&[], &s, &d, 2).is_err());
        assert!(plan(&[frame(0, 0)], &s, &d, 3).is_err());
    }
}
