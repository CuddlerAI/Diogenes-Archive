//! scale-shift 不变对齐与深度评测指标
//! （[77 号 §9.3](../../../docs/77-depth-ground-truth-hook-experiment-plan.md) A 组）。
//!
//! # 本模块存在的主要理由：证伪一条我们自己写下的结论
//!
//! 77 号 §5.5.3 曾断言「即使 `(A,B)` 标定完全错，A 组指标数值一模一样」。
//! 把它写成测试之后发现**这句话过宽**。真实情况分三档（见本模块测试）：
//!
//! | 情形 | 归一化对齐残差 | AbsRel / δ（深度空间） |
//! |---|---|---|
//! | GT 视差被任意仿射变换 `α + β·u` | **严格不变** | **不一定** |
//! | 只有 `B`（尺度）错、`A` 正确 | 严格不变 | **严格不变** |
//! | `A` 也错（远平面估错） | 严格不变 | **会变** |
//!
//! 好在实践上落在第二档：reversed-Z + 无穷远平面时 `A ≡ 0` 是**结构性**的、不是标定量，
//! 只有 `B = near` 需要标定 → 纯尺度误差 → AbsRel/δ 仍然不变。
//! 传统 Z 下 `A = f/(f-n)`，`far ≫ near` 时 `A ≈ 1`，偏差很小但**不为零**。
//!
//! 结论：**模型之间的排序在任何情况下都可靠**（这正是本实验要回答的），
//! 但 metric 绝对值在 `A` 估错时会漂移，报告里必须连同标定来源一起给。

/// 最小二乘拟合 `s·pred + t ≈ gt`（MiDaS 协议的对齐步骤，在视差空间做）。
///
/// 返回 `None` 表示 `pred` 退化（全为同一值），无法定尺度。
#[must_use]
pub fn align_scale_shift(pred: &[f64], gt: &[f64]) -> Option<(f64, f64)> {
    let n = pred.len();
    if n < 2 || n != gt.len() {
        return None;
    }
    let nf = n as f64;
    let sum_p: f64 = pred.iter().sum();
    let sum_g: f64 = gt.iter().sum();
    let sum_pp: f64 = pred.iter().map(|p| p * p).sum();
    let sum_pg: f64 = pred.iter().zip(gt).map(|(p, g)| p * g).sum();

    let denom = nf * sum_pp - sum_p * sum_p;
    if denom.abs() < f64::EPSILON {
        return None;
    }
    let s = (nf * sum_pg - sum_p * sum_g) / denom;
    let t = (sum_g - s * sum_p) / nf;
    Some((s, t))
}

/// 归一化的对齐残差：`RMSE(s·pred + t − gt) / std(gt)`。
///
/// **这是本模块里唯一在 GT 任意仿射变换下严格不变的量**，
/// 因此也是模型间比较最稳的依据。
#[must_use]
pub fn normalized_aligned_rmse(pred: &[f64], gt: &[f64]) -> Option<f64> {
    let (s, t) = align_scale_shift(pred, gt)?;
    let n = gt.len() as f64;
    let mean_g = gt.iter().sum::<f64>() / n;
    let var_g = gt.iter().map(|g| (g - mean_g).powi(2)).sum::<f64>() / n;
    if var_g <= 0.0 {
        return None;
    }
    let mse = pred
        .iter()
        .zip(gt)
        .map(|(p, g)| (s * p + t - g).powi(2))
        .sum::<f64>()
        / n;
    Some(mse.sqrt() / var_g.sqrt())
}

/// 视差 → 深度。非正视差视为无效（返回 `None`）。
#[must_use]
pub fn disparity_to_depth(u: f64) -> Option<f64> {
    if u > 0.0 && u.is_finite() {
        Some(1.0 / u)
    } else {
        None
    }
}

/// 深度空间的 AbsRel：`mean(|pred − gt| / gt)`。
#[must_use]
pub fn abs_rel(pred_depth: &[f64], gt_depth: &[f64]) -> Option<f64> {
    if pred_depth.len() != gt_depth.len() || pred_depth.is_empty() {
        return None;
    }
    let mut sum = 0.0;
    let mut n = 0u64;
    for (p, g) in pred_depth.iter().zip(gt_depth) {
        if *g > 0.0 && g.is_finite() && p.is_finite() {
            sum += (p - g).abs() / g;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f64)
}

/// δ_k：`max(pred/gt, gt/pred) < 1.25^k` 的像素占比。
#[must_use]
pub fn delta_k(pred_depth: &[f64], gt_depth: &[f64], k: u32) -> Option<f64> {
    if pred_depth.len() != gt_depth.len() || pred_depth.is_empty() {
        return None;
    }
    let threshold = 1.25f64.powi(k as i32);
    let mut hit = 0u64;
    let mut n = 0u64;
    for (p, g) in pred_depth.iter().zip(gt_depth) {
        if *g > 0.0 && *p > 0.0 && g.is_finite() && p.is_finite() {
            n += 1;
            if (p / g).max(g / p) < threshold {
                hit += 1;
            }
        }
    }
    (n > 0).then(|| hit as f64 / n as f64)
}

/// 把预测视差按 GT 对齐后转成深度，用于 A 组指标。
#[must_use]
pub fn align_then_to_depth(pred_disp: &[f64], gt_disp: &[f64]) -> Option<Vec<f64>> {
    let (s, t) = align_scale_shift(pred_disp, gt_disp)?;
    Some(
        pred_disp
            .iter()
            .map(|p| disparity_to_depth(s * p + t).unwrap_or(f64::NAN))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一组「真值视差」与一个带噪的「预测视差」。
    fn sample() -> (Vec<f64>, Vec<f64>) {
        let gt: Vec<f64> = (1..=200).map(|i| 1.0 / (f64::from(i) * 0.25)).collect();
        // 预测 = 真值的仿射变形 + 一点结构化误差（模拟 MDE 的尺度/偏移未知 + 噪声）
        let pred: Vec<f64> = gt
            .iter()
            .enumerate()
            .map(|(i, u)| 3.7 * u + 0.42 + 0.01 * ((i % 7) as f64 - 3.0))
            .collect();
        (pred, gt)
    }

    #[test]
    fn alignment_recovers_an_exact_affine_relation() {
        let gt: Vec<f64> = (1..=50).map(f64::from).collect();
        let pred: Vec<f64> = gt.iter().map(|g| (g - 4.0) / 3.0).collect();
        let (s, t) = align_scale_shift(&pred, &gt).unwrap();
        assert!((s - 3.0).abs() < 1e-9, "s={s}");
        assert!((t - 4.0).abs() < 1e-9, "t={t}");
    }

    #[test]
    fn normalized_residual_is_invariant_under_any_affine_gt_error() {
        // 这是 §5.5.3 真正成立的那部分：GT 视差被任意 α + β·u 变换，
        // 归一化对齐残差严格不变 —— 因此模型排序永远可靠。
        let (pred, gt) = sample();
        let base = normalized_aligned_rmse(&pred, &gt).unwrap();

        for (alpha, beta) in [(0.0, 2.0), (0.37, 1.0), (-1.5, 0.8), (12.0, 41.0)] {
            let gt2: Vec<f64> = gt.iter().map(|u| alpha + beta * u).collect();
            let got = normalized_aligned_rmse(&pred, &gt2).unwrap();
            assert!(
                (got - base).abs() < 1e-9,
                "α={alpha} β={beta} 下残差应不变：{got} vs {base}"
            );
        }
    }

    #[test]
    fn depth_metrics_survive_a_pure_scale_error_in_b() {
        // 第二档：只有 B（近平面/尺度）估错，A 正确。
        // reversed-Z + 无穷远时 A ≡ 0 是结构性的，这正是实践中的情形。
        let (pred, gt) = sample();
        let gt_depth: Vec<f64> = gt.iter().map(|u| 1.0 / u).collect();
        let pred_depth = align_then_to_depth(&pred, &gt).unwrap();
        let base_absrel = abs_rel(&pred_depth, &gt_depth).unwrap();
        let base_d1 = delta_k(&pred_depth, &gt_depth, 1).unwrap();

        // B 估错 3.3 倍 → 视差整体乘 1/3.3 → 深度整体乘 3.3
        let beta = 1.0 / 3.3;
        let gt_wrong: Vec<f64> = gt.iter().map(|u| beta * u).collect();
        let gt_depth_wrong: Vec<f64> = gt_wrong.iter().map(|u| 1.0 / u).collect();
        let pred_depth_wrong = align_then_to_depth(&pred, &gt_wrong).unwrap();

        let absrel = abs_rel(&pred_depth_wrong, &gt_depth_wrong).unwrap();
        let d1 = delta_k(&pred_depth_wrong, &gt_depth_wrong, 1).unwrap();

        assert!(
            (absrel - base_absrel).abs() < 1e-9,
            "纯尺度误差下 AbsRel 应不变：{absrel} vs {base_absrel}"
        );
        assert!((d1 - base_d1).abs() < 1e-12, "纯尺度误差下 δ1 应不变");
    }

    #[test]
    fn depth_metrics_do_shift_when_a_is_wrong() {
        // 第三档，也是本模块最重要的一条：`A` 估错时深度空间指标**会变**。
        // 这条证伪了 77 号 §5.5.3 初稿「A 组指标一模一样」的宽泛说法。
        let (pred, gt) = sample();
        let gt_depth: Vec<f64> = gt.iter().map(|u| 1.0 / u).collect();
        let pred_depth = align_then_to_depth(&pred, &gt).unwrap();
        let base_absrel = abs_rel(&pred_depth, &gt_depth).unwrap();

        // A 估错 → GT 视差多了一个非零平移
        let alpha = 0.35;
        let gt_wrong: Vec<f64> = gt.iter().map(|u| alpha + u).collect();
        let gt_depth_wrong: Vec<f64> = gt_wrong.iter().map(|u| 1.0 / u).collect();
        let pred_depth_wrong = align_then_to_depth(&pred, &gt_wrong).unwrap();
        let absrel = abs_rel(&pred_depth_wrong, &gt_depth_wrong).unwrap();

        assert!(
            (absrel - base_absrel).abs() > 1e-6,
            "A 估错时 AbsRel 应当发生变化，实测 {absrel} vs {base_absrel}"
        );

        // 而归一化对齐残差在同一情形下仍然纹丝不动
        let r0 = normalized_aligned_rmse(&pred, &gt).unwrap();
        let r1 = normalized_aligned_rmse(&pred, &gt_wrong).unwrap();
        assert!((r0 - r1).abs() < 1e-9);
    }

    #[test]
    fn model_ranking_is_preserved_under_affine_gt_error() {
        // 实验真正要回答的是「哪个模型更好」，这条保证了答案不受标定影响。
        let (_, gt) = sample();
        let good: Vec<f64> = gt.iter().map(|u| 2.0 * u + 0.1).collect();
        let bad: Vec<f64> = gt
            .iter()
            .enumerate()
            .map(|(i, u)| 2.0 * u + 0.1 + 0.05 * ((i % 5) as f64 - 2.0))
            .collect();

        let rank = |g: &[f64]| {
            (
                normalized_aligned_rmse(&good, g).unwrap(),
                normalized_aligned_rmse(&bad, g).unwrap(),
            )
        };
        let (g0, b0) = rank(&gt);
        assert!(g0 < b0, "构造上 good 应优于 bad");

        for (alpha, beta) in [(0.0, 5.0), (0.9, 0.3), (-2.0, 7.5)] {
            let gt2: Vec<f64> = gt.iter().map(|u| alpha + beta * u).collect();
            let (g1, b1) = rank(&gt2);
            assert!(g1 < b1, "α={alpha} β={beta} 下排序应保持");
        }
    }

    #[test]
    fn delta_thresholds_behave() {
        let gt = vec![10.0, 10.0, 10.0, 10.0];
        // 比值分别为 1.0 / 1.2 / 1.3 / 2.0
        let pred = vec![10.0, 12.0, 13.0, 20.0];
        assert!((delta_k(&pred, &gt, 1).unwrap() - 0.5).abs() < 1e-12); // <1.25 的有 2 个
        assert!((delta_k(&pred, &gt, 2).unwrap() - 0.75).abs() < 1e-12); // <1.5625 的有 3 个
    }

    #[test]
    fn invalid_pixels_are_excluded_not_counted_as_zero() {
        // 68 号用 NaN 标记无效像素（天空/透明面/HUD），它们必须被排除而不是拉低分母
        let gt = vec![10.0, f64::NAN, 20.0];
        let pred = vec![10.0, 999.0, 20.0];
        assert!((abs_rel(&pred, &gt).unwrap() - 0.0).abs() < 1e-12);
        assert!((delta_k(&pred, &gt, 1).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn degenerate_input_returns_none_instead_of_nan() {
        let flat = vec![1.0; 10];
        assert!(align_scale_shift(&flat, &flat).is_none());
        assert!(normalized_aligned_rmse(&flat, &flat).is_none());
        assert!(abs_rel(&[], &[]).is_none());
    }
}
