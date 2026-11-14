//! P4 第二层：从「一堆候选矩阵」到「哪一个是主相机」。
//!
//! [`crate::camera`] 解决的是**单个矩阵**的问题——这 16 个 float 像不像视图/投影矩阵、
//! 像的话怎么取位姿和内参。但真机上每帧会扫出几十上百个候选：
//! 阴影级联的视图矩阵同样正交、武器视模型的投影矩阵同样是透视、
//! 反射/传送门相机每帧同样在动。**结构判据一个都筛不掉它们。**
//!
//! 本模块做的是跨帧的那一半：累积候选、按 [77 号 §5.11.2 判据 3](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)
//! 的两个真值验证、给出打分。
//!
//! # 设计原则：宁可报「分不清」，不要猜
//!
//! 认错矩阵的后果是**交付一份看起来完全正常、但相机轨迹是别的东西**的
//! `action_camera.json`——阴影相机的轨迹也平滑、也连续、也有合理的位姿。
//! 下游拿去做 SFM 会得到一个自洽但错误的重建。
//!
//! 所以 [`select_view`] / [`select_projection`] 在候选之间**区分度不够时返回 `None`**，
//! 而不是取分最高的那个。这与 [`crate::dsv`] 的取舍一致：先出证据，再由人下结论。
//!
//! # 两个真值从哪来
//!
//! 1. **视图矩阵 ↔ 鼠标**：逐帧相机偏航增量应与 `events.jsonl.zst` 的鼠标 dx 高度相关。
//!    这条不依赖任何游戏知识，纯靠「玩家转鼠标、画面就转」。**决定性判据。**
//! 2. **投影矩阵 ↔ 深度缓冲**：候选给出 `(A, B)`，代入 `z = B/(d − A)`。
//!    用真实深度样本算出的 `z` 若出现负值或远平面荒谬，即候选错误。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::camera::{
    Mat4, MatrixKind, TranslationSlot, correlation, intrinsics_from_projection, pose_from_view,
};
use crate::depth::Projection;

/// 候选在常量缓冲里的位置。
///
/// `buffer_tag` 不必是指针——引擎常用环形的 dynamic buffer，指针每帧都会变。
/// 由 probe 决定用什么做标签（如着色器槽位 + 尺寸），本模块只要求它**跨帧稳定**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SlotKey {
    pub buffer_tag: u64,
    pub offset: u32,
}

/// 一帧里在某个槽位上看到的一个候选。
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub slot: SlotKey,
    pub matrix: Mat4,
    pub kind: MatrixKind,
}

/// 视图候选的跨帧统计。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewCandidate {
    pub slot: SlotKey,
    pub translation: TranslationSlot,
    /// 出现过的帧数。
    pub frames: u32,
    /// 逐帧偏航角（弧度，世界系）。与 `frames` 等长。
    pub yaw: Vec<f32>,
    /// 逐帧俯仰角（弧度）。
    pub pitch: Vec<f32>,
    /// 逐帧相机位置。
    pub position: Vec<[f32; 3]>,
}

/// 投影候选的跨帧统计。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionCandidate {
    pub slot: SlotKey,
    pub frames: u32,
    /// 最后一次看到的矩阵（投影通常整场不变，留一份就够复算内参）。
    pub matrix: Mat4,
    /// 出现过几种不同的矩阵值。主相机投影在无变焦时应当恒定。
    pub distinct_values: u32,
}

/// 跨帧累积器。
///
/// probe 每帧把扫到的候选喂进来；离线阶段再调 [`Self::view_candidates`] /
/// [`Self::projection_candidates`] 取统计。
#[derive(Debug, Default)]
pub struct CandidateTracker {
    frames_seen: u32,
    views: HashMap<SlotKey, ViewCandidate>,
    projections: HashMap<SlotKey, ProjectionCandidate>,
}

impl CandidateTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn frames_seen(&self) -> u32 {
        self.frames_seen
    }

    /// 喂入一帧的全部候选。
    ///
    /// **同一槽位在一帧里出现多次时只取第一次**——常量缓冲可能在一帧内被多次
    /// `Map`/`Unmap`（引擎给不同 pass 复用同一块），后面的写入属于别的 pass。
    pub fn observe_frame(&mut self, observations: &[Observation]) {
        self.frames_seen += 1;
        let mut seen_this_frame = std::collections::HashSet::new();
        for obs in observations {
            if !seen_this_frame.insert(obs.slot) {
                continue;
            }
            match obs.kind {
                MatrixKind::View { translation } => {
                    let pose = pose_from_view(&obs.matrix, translation);
                    let (yaw, pitch) = yaw_pitch_from_view(&obs.matrix);
                    let e = self.views.entry(obs.slot).or_insert_with(|| ViewCandidate {
                        slot: obs.slot,
                        translation,
                        frames: 0,
                        yaw: Vec::new(),
                        pitch: Vec::new(),
                        position: Vec::new(),
                    });
                    e.frames += 1;
                    e.yaw.push(yaw);
                    e.pitch.push(pitch);
                    e.position.push(pose.position);
                }
                MatrixKind::Projection => {
                    let e =
                        self.projections
                            .entry(obs.slot)
                            .or_insert_with(|| ProjectionCandidate {
                                slot: obs.slot,
                                frames: 0,
                                matrix: obs.matrix,
                                distinct_values: 0,
                            });
                    if e.frames == 0 || e.matrix != obs.matrix {
                        e.distinct_values += 1;
                    }
                    e.frames += 1;
                    e.matrix = obs.matrix;
                }
                MatrixKind::Unknown => {}
            }
        }
    }

    /// 出现率不低于 `min_hit_rate` 的视图候选，按槽位排序（输出稳定，便于 diff）。
    #[must_use]
    pub fn view_candidates(&self, min_hit_rate: f32) -> Vec<&ViewCandidate> {
        let mut v: Vec<_> = self
            .views
            .values()
            .filter(|c| self.hit_rate(c.frames) >= min_hit_rate)
            .collect();
        v.sort_by_key(|c| c.slot);
        v
    }

    #[must_use]
    pub fn projection_candidates(&self, min_hit_rate: f32) -> Vec<&ProjectionCandidate> {
        let mut v: Vec<_> = self
            .projections
            .values()
            .filter(|c| self.hit_rate(c.frames) >= min_hit_rate)
            .collect();
        v.sort_by_key(|c| c.slot);
        v
    }

    fn hit_rate(&self, frames: u32) -> f32 {
        if self.frames_seen == 0 {
            0.0
        } else {
            frames as f32 / self.frames_seen as f32
        }
    }
}

/// 从视图矩阵取世界系的偏航/俯仰。
///
/// 视图矩阵的左上 3×3 是 `Rᵀ`（世界→相机），所以相机在世界中的朝向是它的转置。
/// 取相机前向（相机空间 −Z 或 +Z 取决于手系）在世界系的方向即可——
/// **相关性只关心逐帧增量，手系整体取反不影响 |r|**，所以这里不纠结左右手。
#[must_use]
pub fn yaw_pitch_from_view(m: &Mat4) -> (f32, f32) {
    // R = Rvᵀ 的第三列 = Rv 的第三行 = m[2][0..3]
    let (fx, fy, fz) = (m[2][0], m[2][1], m[2][2]);
    let yaw = fz.atan2(fx);
    let horiz = (fx * fx + fz * fz).sqrt();
    let pitch = fy.atan2(horiz);
    (yaw, pitch)
}

/// 把角度差折回 `(−π, π]`。
///
/// 不折的话偏航从 `+π` 跨到 `−π` 会产生一个 `2π` 的假增量，
/// 而那一帧的假增量足以把整段相关系数拉垮——**看起来就像认错了矩阵**。
#[must_use]
pub fn wrap_pi(mut d: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    while d > PI {
        d -= TAU;
    }
    while d <= -PI {
        d += TAU;
    }
    d
}

/// 角度序列 → 逐帧增量（已折回 `(−π, π]`）。长度为 `series.len() - 1`。
#[must_use]
pub fn angular_deltas(series: &[f32]) -> Vec<f32> {
    series.windows(2).map(|w| wrap_pi(w[1] - w[0])).collect()
}

/// 一个视图候选与鼠标的相关性评分。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewScore {
    pub slot: SlotKey,
    /// 偏航增量 ↔ 鼠标 dx 的相关系数。
    pub yaw_corr: f32,
    /// 俯仰增量 ↔ 鼠标 dy 的相关系数。
    pub pitch_corr: f32,
}

impl ViewScore {
    /// 综合强度：两轴取绝对值后的较小者。
    ///
    /// **取较小者而不是平均**：主相机两个轴都必须跟鼠标走。
    /// 只有一个轴相关的候选（比如只跟着偏航转的阴影相机）不该靠另一个轴的
    /// 高分捞回来——平均会让 `(0.95, 0.05)` 得到 0.5，看着还行，实则一个轴完全对不上。
    #[must_use]
    pub fn strength(self) -> f32 {
        self.yaw_corr.abs().min(self.pitch_corr.abs())
    }
}

/// 相关性最低要求。低于这个值说明这个候选根本不跟鼠标走。
pub const MIN_CORRELATION: f32 = 0.7;
/// 第一名相对第二名的最小领先。差距太小说明**分不清**，此时必须返回 `None`。
pub const MIN_MARGIN: f32 = 0.15;

/// 选择偏差原因。**每一条都要能指导下一步做什么**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "code")]
pub enum SelectionFailure {
    /// 一个候选都没有。
    NoCandidates,
    /// 有候选但样本太短，相关性算不出来。
    NotEnoughFrames { frames: usize },
    /// **鼠标序列本身没有变化**，这一段里相关性对任何候选都无意义。
    ///
    /// 与 [`Self::BelowThreshold`] 分开：那条说「候选不跟鼠标走」，
    /// 这条说「这段样本没法判断」。前者该换候选，后者该换一段有转视角的样本。
    NoMouseVariance,
    /// 最高分也不够高——多半是这一段里玩家几乎没动鼠标。
    BelowThreshold { best: f32, threshold: f32 },
    /// 前两名太接近，**分不清**。附上两者以便人工看。
    Ambiguous {
        best: SlotKey,
        best_score: f32,
        runner_up: SlotKey,
        runner_up_score: f32,
    },
}

/// 用鼠标增量在视图候选里选主相机。
///
/// `mouse_dx` / `mouse_dy` 必须与候选的**增量序列**等长，即比帧数少 1。
///
/// 返回 `Err` 时**不要退而求其次取最高分**——见模块文档。
pub fn select_view(
    candidates: &[&ViewCandidate],
    mouse_dx: &[f32],
    mouse_dy: &[f32],
) -> Result<(ViewScore, Vec<ViewScore>), SelectionFailure> {
    if candidates.is_empty() {
        return Err(SelectionFailure::NoCandidates);
    }
    if !has_variance(mouse_dx) || !has_variance(mouse_dy) {
        return Err(SelectionFailure::NoMouseVariance);
    }
    let mut scores: Vec<ViewScore> = Vec::new();
    for c in candidates {
        let dy_yaw = angular_deltas(&c.yaw);
        let dp = angular_deltas(&c.pitch);
        if dy_yaw.len() != mouse_dx.len() || dp.len() != mouse_dy.len() {
            continue;
        }
        // 候选自己某轴零方差（阴影相机的俯仰恒定）时 `correlation` 返回 None。
        // **记 0 分，不要跳过**——跳过等于把这个候选从证据里抹掉，
        // 而「它一动不动」恰恰是最该被人看到的判据。
        scores.push(ViewScore {
            slot: c.slot,
            yaw_corr: correlation(&dy_yaw, mouse_dx).unwrap_or(0.0),
            pitch_corr: correlation(&dp, mouse_dy).unwrap_or(0.0),
        });
    }
    if scores.is_empty() {
        return Err(SelectionFailure::NotEnoughFrames {
            frames: mouse_dx.len(),
        });
    }
    // 降序；同分时按槽位定序，保证输出可复现
    scores.sort_by(|a, b| {
        b.strength()
            .partial_cmp(&a.strength())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.slot.cmp(&b.slot))
    });
    let best = scores[0];
    if best.strength() < MIN_CORRELATION {
        return Err(SelectionFailure::BelowThreshold {
            best: best.strength(),
            threshold: MIN_CORRELATION,
        });
    }
    if let Some(second) = scores.get(1)
        && best.strength() - second.strength() < MIN_MARGIN
    {
        return Err(SelectionFailure::Ambiguous {
            best: best.slot,
            best_score: best.strength(),
            runner_up: second.slot,
            runner_up_score: second.strength(),
        });
    }
    Ok((best, scores))
}

/// 序列是否有变化。全等的序列方差为 0，相关性无从定义。
#[must_use]
fn has_variance(v: &[f32]) -> bool {
    v.iter().any(|x| (x - v[0]).abs() > f32::EPSILON)
}

/// 一个投影候选在真实深度样本上的表现。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProjectionScore {
    pub slot: SlotKey,
    /// 线性化后落在合理范围内的样本比例。
    ///
    /// **这条只抓「结构性错」**（`A` 不对导致 z 为负、远平面荒谬）。
    /// 它**抓不到纯尺度错**：合理区间跨 5.6 个数量级，10× 的近平面差异
    /// 仍然整体落在区间内。见 [`select_projection`] 的说明。
    pub plausible_ratio: f32,
    /// 线性化后的深度中位数（米），供人工判断量级是否合理。
    pub median_z: f32,
    /// `|fx − fy| / fx`。方形像素下应当为 0；明显不为 0 的多半是 UI/后处理矩阵。
    pub square_pixel_error: f32,
}

/// 深度合理性区间（米）。
///
/// 下界 0.05：比这更近的东西在游戏里通常是视模型或穿模，不该占多数。
/// 上界 20 000：超过 20 km 的「地面场景」不存在；天空盒会命中远平面，
/// 所以这里不要求 100% 落区间，只看比例。
pub const MIN_PLAUSIBLE_Z: f64 = 0.05;
pub const MAX_PLAUSIBLE_Z: f64 = 20_000.0;
/// 合理样本比例的门限。
pub const MIN_PLAUSIBLE_RATIO: f32 = 0.8;

/// 用真实深度缓冲值验证投影候选。
///
/// `depth_samples` 是归一化的深度缓冲值（0..1），直接取自突发层的原生深度。
///
/// # 为什么这条能区分主相机与视模型投影
///
/// 视模型（手/武器）用的是**另一套近远平面**，通常近得多。拿它的 `(A, B)`
/// 去线性化**主场景**的深度，会把整个场景压进几米之内甚至给出负值——
/// 而结构判据对两者一视同仁，光看矩阵分不出来。
pub fn score_projections(
    candidates: &[&ProjectionCandidate],
    depth_samples: &[f32],
) -> Vec<ProjectionScore> {
    let mut out = Vec::new();
    for c in candidates {
        let Some(proj) = projection_from_matrix(&c.matrix) else {
            continue;
        };
        let mut zs: Vec<f64> = Vec::new();
        let mut plausible = 0usize;
        for &d in depth_samples {
            let Some(z) = proj.linearize(f64::from(d)) else {
                continue;
            };
            if (MIN_PLAUSIBLE_Z..=MAX_PLAUSIBLE_Z).contains(&z) {
                plausible += 1;
            }
            zs.push(z);
        }
        // **一个有效样本都没有的候选也要出现在结果里。** 它恰恰是最该被看到的那个
        // ——`A` 错到让全帧 z 为负，正是这条判据要抓的东西。
        // 早先版本在这里 `continue`，等于把最明确的错误证据从报告里删掉。
        zs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_z = if zs.is_empty() {
            f32::NAN
        } else {
            zs[zs.len() / 2] as f32
        };
        // 以 16:9 为参照算方形像素误差。渲染分辨率由调用方给出更准，
        // 但这里只做相对比较，等比缩放不影响 |fx − fy| / fx。
        let intr = intrinsics_from_projection(&c.matrix, 1920.0, 1080.0);
        let square_pixel_error = if intr.fx.abs() > f32::EPSILON {
            (intr.fx - intr.fy).abs() / intr.fx.abs()
        } else {
            f32::INFINITY
        };
        out.push(ProjectionScore {
            slot: c.slot,
            plausible_ratio: plausible as f32 / depth_samples.len() as f32,
            median_z,
            square_pixel_error,
        });
    }
    out.sort_by(|a, b| {
        b.plausible_ratio
            .partial_cmp(&a.plausible_ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.slot.cmp(&b.slot))
    });
    out
}

/// 在投影候选里选主相机，判据同 [`select_view`]：不够清楚就返回 `Err`。
///
/// # ⚠️ 这个判据抓不到纯尺度错，这是有意留着的
///
/// 视模型（手/武器）的投影常与主相机**同宽高比、只差近平面**。此时：
///
/// - 结构判据一视同仁（两个都是合法透视矩阵）；
/// - 方形像素误差一视同仁（同宽高比）；
/// - `plausible_ratio` 也基本一视同仁——合理区间跨 5.6 个数量级，
///   10× 的尺度差仍整体落在区间内。
///
/// 所以本函数在这种情形下会返回 [`SelectionFailure::Ambiguous`]，**这是正确行为**。
/// 消歧靠的是 [77 号 §5.11.2 第一步](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)：
/// **只在主场景 DSV 绑定期间截获常量缓冲**。那一步不是「缩小搜索面」的性能优化，
/// 它是这里唯一的消歧来源——跳过它，本函数就只能报分不清。
///
/// `median_z` 一并输出，供人工在 Ambiguous 时按场景量级下结论。
pub fn select_projection(scores: &[ProjectionScore]) -> Result<ProjectionScore, SelectionFailure> {
    let Some(&best) = scores.first() else {
        return Err(SelectionFailure::NoCandidates);
    };
    if best.plausible_ratio < MIN_PLAUSIBLE_RATIO {
        return Err(SelectionFailure::BelowThreshold {
            best: best.plausible_ratio,
            threshold: MIN_PLAUSIBLE_RATIO,
        });
    }
    if let Some(&second) = scores.get(1)
        && best.plausible_ratio - second.plausible_ratio < MIN_MARGIN
    {
        return Err(SelectionFailure::Ambiguous {
            best: best.slot,
            best_score: best.plausible_ratio,
            runner_up: second.slot,
            runner_up_score: second.plausible_ratio,
        });
    }
    Ok(best)
}

/// 从投影矩阵还原 `d = A + B/z` 的 `(A, B)`。
///
/// D3D 的透视投影里 `d = m[2][2] + m[3][2]/z`（行向量约定），
/// 即 `A = m[2][2]`、`B = m[3][2]`。
///
/// `B == 0` 说明这不是一个能线性化的投影（正交投影或退化矩阵），返回 `None`。
#[must_use]
pub fn projection_from_matrix(m: &Mat4) -> Option<Projection> {
    let (a, b) = (f64::from(m[2][2]), f64::from(m[3][2]));
    if b == 0.0 || !a.is_finite() || !b.is_finite() {
        return None;
    }
    Some(Projection {
        a,
        b,
        // 单位由 metric_scale 另行标定（80 号实测需求方样本恒为 1，即引擎单位就是米）。
        // 这里只做候选筛选，缩放不影响「z 是否为正、量级是否合理」的判断。
        metric_scale: 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::classify;

    /// 造一个视图矩阵：绕 Y 转 `yaw`、绕 X 转 `pitch`，相机在 `pos`。
    ///
    /// 视图矩阵 `V = [Rᵀ | −Rᵀp]`（行向量约定，平移在 m[3]）。
    fn view_matrix(yaw: f32, pitch: f32, pos: [f32; 3]) -> Mat4 {
        let (cy, sy) = (yaw.cos(), yaw.sin());
        let (cp, sp) = (pitch.cos(), pitch.sin());
        // R = Ry(yaw) · Rx(pitch)，列为相机的 right/up/forward
        let r = [
            [cy, 0.0, -sy],
            [sy * sp, cp, cy * sp],
            [sy * cp, -sp, cy * cp],
        ];
        // Rᵀ 放进左上 3×3（行向量约定下 V 的左上就是 Rᵀ 的转置排布）
        let mut m: Mat4 = [[0.0; 4]; 4];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = r[j][i];
            }
        }
        // t = −Rᵀ·p
        for j in 0..3 {
            m[3][j] = -(r[0][j] * pos[0] + r[1][j] * pos[1] + r[2][j] * pos[2]);
        }
        m[3][3] = 1.0;
        m
    }

    /// reversed-Z + infinite far 的投影矩阵：d = near/z
    fn projection_matrix(near: f32, aspect_x: f32, aspect_y: f32) -> Mat4 {
        let mut m: Mat4 = [[0.0; 4]; 4];
        m[0][0] = aspect_x;
        m[1][1] = aspect_y;
        m[2][2] = 0.0; // A = 0
        m[2][3] = 1.0;
        m[3][2] = near; // B = near
        m[3][3] = 0.0;
        m
    }

    /// 确定性伪随机的鼠标序列。
    ///
    /// **不能用匀速转动做夹具**：恒定序列方差为 0，相关系数无从定义，
    /// 算出来是浮点噪声（实测 2e-7），看着像「完全不相关」。
    /// 真实鼠标输入本来就是变化的，夹具必须反映这一点。
    fn jitter(n: usize, scale: f32, seed: u64) -> Vec<f32> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x % 2000) as f32 / 1000.0 - 1.0) * scale
            })
            .collect()
    }

    fn slot(tag: u64, off: u32) -> SlotKey {
        SlotKey {
            buffer_tag: tag,
            offset: off,
        }
    }

    #[test]
    fn a_synthetic_view_matrix_is_classified_as_a_view() {
        // 先确认测试夹具本身是合法的视图矩阵，否则后面全是空转
        let m = view_matrix(0.3, -0.2, [1.0, 2.0, 3.0]);
        assert!(
            matches!(classify(&m), MatrixKind::View { .. }),
            "夹具不是视图矩阵"
        );
    }

    #[test]
    fn yaw_recovered_from_a_view_matrix_tracks_the_input() {
        let a = yaw_pitch_from_view(&view_matrix(0.0, 0.0, [0.0; 3])).0;
        let b = yaw_pitch_from_view(&view_matrix(0.5, 0.0, [0.0; 3])).0;
        assert!(
            (wrap_pi(b - a).abs() - 0.5).abs() < 1e-3,
            "偏航增量应为 0.5，实得 {}",
            wrap_pi(b - a)
        );
    }

    #[test]
    fn wrapping_prevents_a_fake_two_pi_spike() {
        use std::f32::consts::PI;
        // 从 +179° 转到 −179°，真实增量是 +2°，不折会算成 −358°
        let series = vec![PI - 0.017, -PI + 0.017];
        let d = angular_deltas(&series);
        assert!(d[0].abs() < 0.1, "跨 ±π 处产生了假增量 {}", d[0]);
    }

    #[test]
    fn the_main_camera_wins_over_a_shadow_camera_that_only_follows_yaw() {
        // 阴影相机常跟着主相机偏航转，但俯仰不跟（阴影投影方向由光源定）。
        // strength() 取两轴较小者，正是为了让这种候选掉下去。
        let n = 120;
        let dx = jitter(n - 1, 0.02, 0x9E37);
        let dy = jitter(n - 1, 0.01, 0x85EB);
        let mut t = CandidateTracker::new();
        let (mut yaw, mut pitch) = (0.0f32, 0.0f32);
        for i in 0..n {
            if i > 0 {
                yaw += dx[i - 1];
                pitch += dy[i - 1];
            }
            let main = view_matrix(yaw, pitch, [i as f32 * 0.1, 1.7, 0.0]);
            // 阴影：跟偏航，俯仰恒定
            let shadow = view_matrix(yaw, 0.0, [i as f32 * 0.1, 40.0, 0.0]);
            t.observe_frame(&[
                Observation {
                    slot: slot(1, 0),
                    matrix: main,
                    kind: classify(&main),
                },
                Observation {
                    slot: slot(2, 64),
                    matrix: shadow,
                    kind: classify(&shadow),
                },
            ]);
        }
        let cands = t.view_candidates(0.9);
        assert_eq!(cands.len(), 2, "两个候选都应通过出现率");
        let (best, all) = select_view(&cands, &dx, &dy).expect("应当选得出来");
        assert_eq!(
            best.slot,
            slot(1, 0),
            "选中的应是主相机，实得 {:?}",
            best.slot
        );
        let shadow = all.iter().find(|s| s.slot == slot(2, 64)).unwrap();
        assert!(
            shadow.strength() < MIN_CORRELATION,
            "只跟偏航的候选强度应当很低，实得 {}",
            shadow.strength()
        );
    }

    #[test]
    fn two_indistinguishable_cameras_are_refused_not_guessed() {
        // 认错矩阵的后果是交付一份看起来正常但轨迹是别的东西的数据。
        // 分不清时必须报 Ambiguous，不能取最高分。
        let n = 120;
        let dx = jitter(n - 1, 0.02, 0x1234);
        let dy = jitter(n - 1, 0.01, 0x5678);
        let mut t = CandidateTracker::new();
        let (mut yaw, mut pitch) = (0.0f32, 0.0f32);
        for i in 0..n {
            if i > 0 {
                yaw += dx[i - 1];
                pitch += dy[i - 1];
            }
            let m = view_matrix(yaw, pitch, [i as f32 * 0.1, 1.7, 0.0]);
            // 两个槽位放同一个相机（引擎给不同 pass 各上传一份，真机上很常见）
            t.observe_frame(&[
                Observation {
                    slot: slot(1, 0),
                    matrix: m,
                    kind: classify(&m),
                },
                Observation {
                    slot: slot(9, 128),
                    matrix: m,
                    kind: classify(&m),
                },
            ]);
        }
        let cands = t.view_candidates(0.9);
        let err = select_view(&cands, &dx, &dy).unwrap_err();
        assert!(
            matches!(err, SelectionFailure::Ambiguous { .. }),
            "两个完全相同的候选应报 Ambiguous，实得 {err:?}"
        );
    }

    #[test]
    fn a_still_mouse_reports_its_own_failure_not_a_wrong_pick() {
        // 玩家没动鼠标的片段里，任何候选的相关性都无意义。
        // 这时必须说「这段样本没法判断」，而不是随便挑一个——
        // 而且要与「候选不跟鼠标走」分开：前者换样本，后者换候选。
        let mut t = CandidateTracker::new();
        let n = 40;
        for i in 0..n {
            let m = view_matrix(0.0, 0.0, [i as f32 * 0.05, 1.7, 0.0]); // 只平移
            t.observe_frame(&[Observation {
                slot: slot(1, 0),
                matrix: m,
                kind: classify(&m),
            }]);
        }
        let cands = t.view_candidates(0.9);
        let err = select_view(&cands, &vec![0.0; n - 1], &vec![0.0; n - 1]).unwrap_err();
        assert_eq!(err, SelectionFailure::NoMouseVariance, "实得 {err:?}");
    }

    #[test]
    fn a_viewmodel_projection_is_reported_as_ambiguous_not_silently_resolved() {
        // 视模型（手/武器）投影与主相机同宽高比、只差近平面。**三条判据都分不开它**：
        // 结构判据一视同仁、方形像素误差一视同仁、plausible_ratio 也几乎一视同仁
        // （合理区间跨 5.6 个数量级，10× 尺度差仍整体落在区间内）。
        //
        // 所以正确行为是报 Ambiguous 并把 median_z 交出去，**不是**挑一个。
        // 真正的消歧在 77 号 §5.11.2 第一步：只在主场景 DSV 绑定期间截获常量缓冲。
        // 这个测试就是那一步不可省略的证据。
        let main = projection_matrix(0.1, 1.3, 2.3); // near = 0.1 m
        let viewmodel = projection_matrix(0.001, 3.0, 5.3); // near = 1 mm
        assert_eq!(classify(&main), MatrixKind::Projection);
        assert_eq!(classify(&viewmodel), MatrixKind::Projection);

        let mut t = CandidateTracker::new();
        for _ in 0..30 {
            t.observe_frame(&[
                Observation {
                    slot: slot(1, 0),
                    matrix: main,
                    kind: MatrixKind::Projection,
                },
                Observation {
                    slot: slot(1, 64),
                    matrix: viewmodel,
                    kind: MatrixKind::Projection,
                },
            ]);
        }
        // 主场景深度：z 从 1 m 到 60 m，d = near/z
        let samples: Vec<f32> = (1..=60).map(|z| 0.1 / z as f32).collect();
        let cands = t.projection_candidates(0.9);
        let scores = score_projections(&cands, &samples);

        let err = select_projection(&scores).unwrap_err();
        assert!(
            matches!(err, SelectionFailure::Ambiguous { .. }),
            "同宽高比、只差尺度的两个投影应报 Ambiguous，实得 {err:?}"
        );

        // 但 median_z 必须把差别摆出来，供人工在 Ambiguous 时下结论
        let m = scores.iter().find(|s| s.slot == slot(1, 0)).unwrap();
        let vm = scores.iter().find(|s| s.slot == slot(1, 64)).unwrap();
        assert!(
            m.median_z > 5.0,
            "主投影中位深应是场景量级，实得 {}",
            m.median_z
        );
        assert!(
            vm.median_z < 1.0,
            "视模型投影应把场景压到 1 m 以内，实得中位 {} m",
            vm.median_z
        );
        assert!(
            (m.median_z / vm.median_z - 100.0).abs() < 1.0,
            "两者中位深之比应等于近平面之比 100×，实得 {}",
            m.median_z / vm.median_z
        );
    }

    #[test]
    fn a_projection_with_a_wrong_a_term_is_caught_by_the_plausibility_ratio() {
        // plausible_ratio 真正抓得住的是**结构性错**：A 不对会让 z 变负。
        // 上面那条测试说明它抓不到纯尺度错，这条说明它并非没用。
        let good = projection_matrix(0.1, 1.3, 2.3); // A = 0
        let mut bad = good;
        bad[2][2] = 0.5; // A = 0.5：d < 0.5 的像素全部给出负 z
        let mut t = CandidateTracker::new();
        for _ in 0..30 {
            t.observe_frame(&[
                Observation {
                    slot: slot(1, 0),
                    matrix: good,
                    kind: MatrixKind::Projection,
                },
                Observation {
                    slot: slot(1, 64),
                    matrix: bad,
                    kind: MatrixKind::Projection,
                },
            ]);
        }
        let samples: Vec<f32> = (1..=60).map(|z| 0.1 / z as f32).collect();
        let scores = score_projections(&t.projection_candidates(0.9), &samples);
        let best = select_projection(&scores).expect("A 错的候选应当被判据剔除");
        assert_eq!(best.slot, slot(1, 0));
        let b = scores.iter().find(|s| s.slot == slot(1, 64)).unwrap();
        assert_eq!(b.plausible_ratio, 0.0, "A 错到全帧 z 为负，合理比例应为 0");
        assert!(b.median_z.is_nan(), "一个有效样本都没有时中位深应为 NaN");
    }

    #[test]
    fn a_projection_that_never_changes_is_counted_as_one_distinct_value() {
        // 无变焦时主相机投影恒定；distinct_values 变大是变焦或认错了槽位的信号。
        let m = projection_matrix(0.1, 1.3, 2.3);
        let mut t = CandidateTracker::new();
        for _ in 0..20 {
            t.observe_frame(&[Observation {
                slot: slot(3, 32),
                matrix: m,
                kind: MatrixKind::Projection,
            }]);
        }
        let c = t.projection_candidates(0.9);
        assert_eq!(c[0].distinct_values, 1);
        assert_eq!(c[0].frames, 20);
    }

    #[test]
    fn a_slot_written_twice_in_one_frame_counts_once() {
        // 引擎会把同一块常量缓冲在一帧里给多个 pass 复用。
        // 第二次写入属于别的 pass，不能当成同一相机的第二帧样本——
        // 否则序列长度与鼠标序列对不上，相关性直接算不出来。
        let m = view_matrix(0.1, 0.0, [0.0; 3]);
        let m2 = view_matrix(2.0, 0.5, [10.0, 0.0, 0.0]);
        let mut t = CandidateTracker::new();
        t.observe_frame(&[
            Observation {
                slot: slot(1, 0),
                matrix: m,
                kind: classify(&m),
            },
            Observation {
                slot: slot(1, 0),
                matrix: m2,
                kind: classify(&m2),
            },
        ]);
        let c = t.view_candidates(0.5);
        assert_eq!(c[0].frames, 1, "一帧里同槽位多次写入应只记一次");
        assert_eq!(c[0].yaw.len(), 1);
    }

    #[test]
    fn hit_rate_filters_out_matrices_that_only_appear_occasionally() {
        // 只在某些帧出现的候选（过场动画相机、偶发的反射 pass）不该进入选择。
        let m = view_matrix(0.1, 0.0, [0.0; 3]);
        let mut t = CandidateTracker::new();
        for i in 0..100 {
            let mut obs = vec![Observation {
                slot: slot(1, 0),
                matrix: m,
                kind: classify(&m),
            }];
            if i % 10 == 0 {
                obs.push(Observation {
                    slot: slot(7, 0),
                    matrix: m,
                    kind: classify(&m),
                });
            }
            t.observe_frame(&obs);
        }
        let c = t.view_candidates(0.9);
        assert_eq!(c.len(), 1, "出现率 10% 的候选不该通过");
        assert_eq!(c[0].slot, slot(1, 0));
    }

    #[test]
    fn an_orthographic_projection_has_no_linearization() {
        // 阴影级联多用正交投影，B = m[3][2] 在那里不是 near，
        // 硬套 d = A + B/z 会给出无意义的 z。返回 None 而不是算个数出来。
        let mut ortho: Mat4 = [[0.0; 4]; 4];
        ortho[0][0] = 0.01;
        ortho[1][1] = 0.01;
        ortho[2][2] = 0.001;
        ortho[3][2] = 0.0;
        ortho[3][3] = 1.0;
        assert!(projection_from_matrix(&ortho).is_none());
    }

    #[test]
    fn no_candidates_is_its_own_failure_not_a_threshold_miss() {
        // 「一个都没扫到」与「扫到了但分不清」要指向不同的下一步：
        // 前者是 hook 没抓到东西，后者是判据不够。
        assert_eq!(
            select_view(&[], &[0.0], &[0.0]).unwrap_err(),
            SelectionFailure::NoCandidates
        );
        assert_eq!(
            select_projection(&[]).unwrap_err(),
            SelectionFailure::NoCandidates
        );
    }
}
