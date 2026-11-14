//! 主场景 depth-stencil view 的追踪与选择
//! （[77 号 §5.10](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
//!
//! 这是整个方案**最脆的一环**：一帧里会绑定多个 depth target（shadow cascade、depth prepass、
//! 主场景、反射、粒子、UI），选错就是全部数据无效，而且是**静默**无效。
//!
//! 因此本模块的姿态是：
//!
//! 1. **只做候选打分与排序，不做「自动选中」。** 最终选择由 operator 目视 `dump-dsv` 的
//!    PNG 后写进 profile（§5.10「靠启发式自动选中不是可接受的验收标准」）。
//! 2. **MSAA 是可行性前置条件，不是打分项。** `SampleDesc.Count > 1` 时 `CopyResource →
//!    STAGING → Map` 在 API 层面不可用（§5.3.1，官方文档核查 §14-②），必须切 typeless 路线。
//!    本模块把它作为**独立的返回值**报出来，而不是混进分数里。
//! 3. **打分只在平台无关的元信息上做**，因此可以在任意平台测试。

use serde::{Deserialize, Serialize};

/// 一个被观察到的 depth-stencil view 的元信息。probe 侧从 `D3D11_TEXTURE2D_DESC` 填。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsvObservation {
    /// 稳定标识（probe 侧用资源指针或自增 id）。
    pub id: String,
    pub width: u32,
    pub height: u32,
    /// `DXGI_SAMPLE_DESC::Count`。**>1 即 MSAA**，见 §5.3.1。
    pub sample_count: u32,
    /// 本帧内该 DSV 被绑定为深度目标时累计的 draw call 数。
    pub draw_calls: u64,
    /// 本帧内该 DSV 被 `ClearDepthStencilView` 清除的次数。
    pub clears: u32,
    /// 本帧内该 DSV 最后一次被绑定时的顺序号（越大越晚）。
    pub last_bound_order: u64,
}

/// 主视口几何，用于判定「分辨率/宽高比是否匹配」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// 候选打分结果。分数只用于**排序与呈现**，不用于自动选定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DsvCandidate {
    pub observation: DsvObservation,
    pub score: f64,
    /// 打分依据的可读拆解，进 `depth-profile-evidence.json` 供人复核。
    pub reasons: Vec<String>,
    /// `true` 表示这张 DSV 走不了 `CopyResource → STAGING → Map`（§5.3.1）。
    pub requires_typeless_path: bool,
}

/// 排序后的候选列表 + 一条整帧结论。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DsvRanking {
    /// 按分数降序。
    pub candidates: Vec<DsvCandidate>,
    /// 本帧是否**存在**任何 MSAA 深度目标。命中即 P1b 立刻切 typeless 路线（§5.3.1）。
    pub any_msaa: bool,
}

impl DsvRanking {
    /// 得分最高者。**这不是「选中的那张」**——最终选择必须由 operator 目视确认。
    #[must_use]
    pub fn top(&self) -> Option<&DsvCandidate> {
        self.candidates.first()
    }

    /// 首选与次选的分数差。差距小意味着启发式没有把握，**必须人工介入**。
    #[must_use]
    pub fn margin(&self) -> Option<f64> {
        match self.candidates.as_slice() {
            [a, b, ..] => Some(a.score - b.score),
            _ => None,
        }
    }

    /// 启发式是否给出了足够清晰的首选。低于阈值时 `dump-dsv` 应当强制人工挑选。
    pub const CLEAR_MARGIN: f64 = 20.0;

    #[must_use]
    pub fn is_confident(&self) -> bool {
        self.margin().is_none_or(|m| m >= Self::CLEAR_MARGIN)
    }
}

/// 实际选中的 DSV 与启发式 ranking 的逐帧对账结果。
///
/// ranking 只做核验、不替 operator 自动选通道。`selected_matches_top=false` 因而是告警证据；
/// 但低 margin 与 MSAA 是结构性不确定/不支持，必须 fail-closed（#577）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DsvSelectionAudit {
    pub selected_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ranked_top_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_margin: Option<f64>,
    pub selected_matches_top: bool,
    pub ranking_confident: bool,
    pub any_msaa: bool,
    /// An audited production profile pinned this unique non-MSAA resource by exact render
    /// identity and format. This may resolve a low heuristic score margin, but never MSAA.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub operator_exact_pin: bool,
}

impl DsvSelectionAudit {
    #[must_use]
    pub fn from_ranking(selected_id: impl Into<String>, ranking: &DsvRanking) -> Self {
        let selected_id = selected_id.into();
        let ranked_top_id = ranking
            .top()
            .map(|candidate| candidate.observation.id.clone());
        Self {
            selected_matches_top: ranked_top_id.as_deref() == Some(selected_id.as_str()),
            selected_id,
            ranked_top_id,
            score_margin: ranking.margin(),
            ranking_confident: ranking.is_confident(),
            any_msaa: ranking.any_msaa,
            operator_exact_pin: false,
        }
    }

    /// Records independent operator evidence for an exact production render identity.
    #[must_use]
    pub fn with_operator_exact_pin(mut self) -> Self {
        self.operator_exact_pin = true;
        self
    }

    /// ranking 自身能否支撑交付。operator 可以有理由选择非 top 候选，所以 mismatch
    /// 只告警；低 margin 与 MSAA 没有可解释的放行依据，必须拒绝。
    #[must_use]
    pub fn is_deliverable(&self) -> bool {
        (self.ranking_confident || self.operator_exact_pin) && !self.any_msaa
    }
}

/// 按 §5.10 的启发式给候选打分并排序。
///
/// 权重设计的意图（而不只是数值）：
///
/// - **分辨率精确匹配是主导项**（+50）。shadow map 通常是方形高分辨率图集，
///   与渲染分辨率不等，这一项就能把它们踢下去。
/// - **宽高比匹配**（+20）作为分辨率的软化版本，覆盖动态分辨率的情形。
/// - **draw call 数取对数**（最多 +25）。取对数是刻意的：主场景与 prepass 的 draw call
///   可能都是几百上千，线性加权会让一个特效 pass 靠刷调用数把主场景挤下去。
/// - **本帧最后绑定**（+10）。主场景通常在后处理之前的最后一个几何 pass。
/// - **完全没被清除**（−15）。主场景每帧必清。
#[must_use]
pub fn rank(observations: &[DsvObservation], viewport: Viewport) -> DsvRanking {
    let mut candidates: Vec<DsvCandidate> = observations
        .iter()
        .map(|o| score_one(o, viewport, observations))
        .collect();

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // 分数相同时按 id 稳定排序，保证同一输入永远给同一输出（P1b 的证据要可复现）
            .then_with(|| a.observation.id.cmp(&b.observation.id))
    });

    let any_msaa = observations.iter().any(|o| o.sample_count > 1);
    DsvRanking {
        candidates,
        any_msaa,
    }
}

fn score_one(o: &DsvObservation, vp: Viewport, all: &[DsvObservation]) -> DsvCandidate {
    let mut score = 0.0f64;
    let mut reasons = Vec::new();

    if o.width == vp.width && o.height == vp.height {
        score += 50.0;
        reasons.push(format!(
            "分辨率精确匹配主视口 {}×{} (+50)",
            vp.width, vp.height
        ));
    } else {
        let a_dsv = f64::from(o.width) / f64::from(o.height.max(1));
        let a_vp = f64::from(vp.width) / f64::from(vp.height.max(1));
        if (a_dsv - a_vp).abs() < 0.01 {
            score += 20.0;
            reasons.push(format!(
                "分辨率不等但宽高比匹配（{}×{}，可能是动态分辨率/上采样）(+20)",
                o.width, o.height
            ));
        } else {
            reasons.push(format!(
                "宽高比不匹配（{}×{} vs 视口 {}×{}），大概率是 shadow map 或立方体贴图 (+0)",
                o.width, o.height, vp.width, vp.height
            ));
        }
    }

    if o.draw_calls > 0 {
        let max_draws = all.iter().map(|x| x.draw_calls).max().unwrap_or(1).max(1);
        // 对数归一：避免一个高频小 pass 靠刷调用数压过主场景
        let ratio = (o.draw_calls as f64).ln_1p() / (max_draws as f64).ln_1p();
        let pts = 25.0 * ratio;
        score += pts;
        reasons.push(format!(
            "draw call {} / 本帧最多 {}（对数归一）(+{pts:.1})",
            o.draw_calls, max_draws
        ));
    }

    let latest = all.iter().map(|x| x.last_bound_order).max().unwrap_or(0);
    if o.last_bound_order == latest && latest > 0 {
        score += 10.0;
        reasons.push("本帧最后一个被绑定的深度目标 (+10)".into());
    }

    if o.clears == 0 {
        score -= 15.0;
        reasons.push("本帧从未被清除，主场景深度通常每帧必清 (−15)".into());
    }

    let requires_typeless_path = o.sample_count > 1;
    if requires_typeless_path {
        reasons.push(format!(
            "⚠️ MSAA ×{}：CopyResource→STAGING→Map 在 API 层面不可用，必须走 typeless + Texture2DMS shader（§5.3.1）",
            o.sample_count
        ));
    }

    DsvCandidate {
        observation: o.clone(),
        score,
        reasons,
        requires_typeless_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(id: &str, w: u32, h: u32, draws: u64, clears: u32, order: u64) -> DsvObservation {
        DsvObservation {
            id: id.into(),
            width: w,
            height: h,
            sample_count: 1,
            draw_calls: draws,
            clears,
            last_bound_order: order,
        }
    }

    /// 一个贴近真实的一帧：主场景 + 两级 shadow cascade + 反射 + UI。
    fn realistic_frame() -> Vec<DsvObservation> {
        vec![
            obs("main", 1920, 1080, 3200, 1, 900),
            obs("shadow_c0", 2048, 2048, 1400, 1, 100),
            obs("shadow_c1", 2048, 2048, 900, 1, 200),
            obs("reflection", 960, 540, 400, 1, 500),
            obs("ui", 1920, 1080, 12, 0, 950),
        ]
    }

    #[test]
    fn picks_the_main_scene_on_a_realistic_frame() {
        let r = rank(
            &realistic_frame(),
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert_eq!(r.top().unwrap().observation.id, "main");
    }

    #[test]
    fn shadow_maps_lose_on_aspect_ratio() {
        let r = rank(
            &realistic_frame(),
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        let shadow = r
            .candidates
            .iter()
            .find(|c| c.observation.id == "shadow_c0")
            .unwrap();
        assert!(
            shadow.reasons.iter().any(|s| s.contains("宽高比不匹配")),
            "shadow map 应因宽高比出局，实际理由：{:?}",
            shadow.reasons
        );
    }

    #[test]
    fn a_ui_pass_cannot_win_by_being_last() {
        // UI 层分辨率一样、绑定最晚，但 draw call 少且从不清除
        let r = rank(
            &realistic_frame(),
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        let ui = r
            .candidates
            .iter()
            .find(|c| c.observation.id == "ui")
            .unwrap();
        let main = r
            .candidates
            .iter()
            .find(|c| c.observation.id == "main")
            .unwrap();
        assert!(ui.score < main.score);
    }

    #[test]
    fn draw_call_weight_is_logarithmic_so_a_spam_pass_cannot_hijack() {
        // 一个特效 pass 刷了 10 倍于主场景的 draw call，但分辨率不匹配
        let mut frame = realistic_frame();
        frame.push(obs("particles", 480, 270, 32_000, 1, 800));
        let r = rank(
            &frame,
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert_eq!(
            r.top().unwrap().observation.id,
            "main",
            "高频小 pass 不应靠 draw call 数压过主场景"
        );
    }

    #[test]
    fn dynamic_resolution_still_scores_via_aspect_ratio() {
        // DLSS/动态分辨率下 depth 在内部渲染分辨率（§5.9）
        let frame = vec![
            obs("main_scaled", 1280, 720, 3200, 1, 900),
            obs("shadow", 2048, 2048, 1400, 1, 100),
        ];
        let r = rank(
            &frame,
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert_eq!(r.top().unwrap().observation.id, "main_scaled");
        assert!(
            r.top()
                .unwrap()
                .reasons
                .iter()
                .any(|s| s.contains("宽高比匹配")),
            "应当靠宽高比而非精确分辨率胜出"
        );
    }

    #[test]
    fn msaa_is_reported_separately_not_folded_into_the_score() {
        // §5.3.1：MSAA 是可行性前置条件，不是打分项
        let mut frame = realistic_frame();
        frame[0].sample_count = 4;
        let r = rank(
            &frame,
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert!(r.any_msaa);
        let top = r.top().unwrap();
        assert_eq!(top.observation.id, "main", "MSAA 不应改变排序");
        assert!(top.requires_typeless_path);
        assert!(top.reasons.iter().any(|s| s.contains("MSAA")));
    }

    #[test]
    fn no_msaa_frame_reports_false() {
        let r = rank(
            &realistic_frame(),
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert!(!r.any_msaa);
        assert!(r.candidates.iter().all(|c| !c.requires_typeless_path));
    }

    #[test]
    fn ambiguous_frames_are_flagged_for_manual_selection() {
        // 两张几何完全相同的 DSV（例如 depth prepass 与主场景）→ 启发式没把握
        let frame = vec![
            obs("prepass", 1920, 1080, 3000, 1, 400),
            obs("main", 1920, 1080, 3100, 1, 410),
        ];
        let r = rank(
            &frame,
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert!(
            !r.is_confident(),
            "首选与次选差距 {:?} 应被判为不自信，必须人工选",
            r.margin()
        );
    }

    #[test]
    fn ranking_is_deterministic_for_reproducible_evidence() {
        // P1b 的证据要可复现：同一输入永远给同一排序
        let frame = realistic_frame();
        let vp = Viewport {
            width: 1920,
            height: 1080,
        };
        let a = rank(&frame, vp);
        let b = rank(&frame, vp);
        assert_eq!(a, b);
    }

    #[test]
    fn empty_frame_is_not_a_panic() {
        let r = rank(
            &[],
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        assert!(r.top().is_none());
        assert!(r.is_confident()); // 无候选时不该报「不自信」而阻塞调用方
        assert!(!r.any_msaa);
    }

    #[test]
    fn an_operator_selection_that_is_not_top_is_recorded_but_not_auto_overridden() {
        let ranking = rank(
            &realistic_frame(),
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        let audit = DsvSelectionAudit::from_ranking("shadow_c0", &ranking);
        assert!(!audit.selected_matches_top);
        assert_eq!(audit.ranked_top_id.as_deref(), Some("main"));
        assert!(
            audit.is_deliverable(),
            "operator 可以有目视证据选择非 top，ranking 不得擅自替换"
        );
    }

    #[test]
    fn a_low_margin_or_any_msaa_is_fail_closed() {
        let ambiguous = rank(
            &[
                obs("prepass", 1920, 1080, 3000, 1, 400),
                obs("main", 1920, 1080, 3100, 1, 410),
            ],
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        let audit = DsvSelectionAudit::from_ranking("main", &ambiguous);
        assert!(!audit.ranking_confident);
        assert!(!audit.is_deliverable());
        assert!(audit.clone().with_operator_exact_pin().is_deliverable());

        let mut msaa = realistic_frame();
        msaa[0].sample_count = 4;
        let ranking = rank(
            &msaa,
            Viewport {
                width: 1920,
                height: 1080,
            },
        );
        let audit = DsvSelectionAudit::from_ranking("main", &ranking);
        assert!(audit.any_msaa);
        assert!(!audit.is_deliverable());
        assert!(!audit.with_operator_exact_pin().is_deliverable());
    }
}
