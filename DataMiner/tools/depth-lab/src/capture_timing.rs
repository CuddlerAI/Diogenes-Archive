//! 「什么时候拷贝深度、拷到的是哪一帧」的状态机。
//!
//! # 为什么拷贝时机需要一个状态机
//!
//! P1c 要把主场景 depth 拷出来，但三个可选时机都有问题：
//!
//! | 时机 | 问题 |
//! |---|---|
//! | `Present` 边界 | **拿不到 DSV**——此刻游戏已切到后处理/UI，深度目标早已解绑（[#425](https://github.com/DiogenesModel/Diogenes/pull/425) 实测） |
//! | `OMSetRenderTargets` 绑定时 | 拿得到 DSV，但**这一帧还没画完**——depth prepass 刚开始就拷，拿到的是残缺深度 |
//! | 每次绑定都拷、最后一次胜出 | 正确但浪费：一帧多次 1920×1080 D24S8 拷贝，每次 ~8 MB，直接压 FPS 门 |
//!
//! 本模块采用第四种：**在下一帧清除深度之前拷**（copy-before-clear）。
//! `ClearDepthStencilView` 被调用的那一刻，缓冲里装的正是**上一帧完整画完**的深度，
//! 而且 DSV 指针就在参数里。一帧只拷一次，时机明确。
//!
//! **代价是一个 off-by-one**：在第 N 帧的 clear 处拷到的深度属于第 **N−1** 帧。
//! 这个偏移必须显式处理——搞错就是整份数据错位一帧，而且看起来完全正常
//! （77 号风险登记 R-4 说的正是这类静默错位）。本模块把它做成状态机并逐条测试。

use serde::{Deserialize, Serialize};

/// 一次「该拷贝」的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureDecision {
    /// 这次拷到的深度属于哪个 `present_seq`。**注意这是上一帧的序号**，
    /// 不是当前正在开始的这一帧。
    pub belongs_to_present_seq: u64,
}

/// 为什么这次没拷。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 还没见过第一次 `Present`，没有「上一帧」可归属。
    NoPreviousFrame,
    /// 上一帧是奇数帧——按 68 号 §6.5 的偶数帧约定不采。
    OddFrameBySpec,
    /// 本帧已经拷过了（同一帧里可能有多次 clear）。
    AlreadyCapturedThisFrame,
    /// 不是 profile 选定的那张 DSV。
    NotTheSelectedDsv,
}

/// copy-before-clear 的时机状态机。
///
/// probe 侧的用法：
///
/// ```text
/// Present   → tick_present()          // 帧号自增
/// Clear(dsv)→ on_clear_depth(是否选中) // 返回 Some 就在调用原 Clear 之前拷
/// ```
#[derive(Debug, Clone, Default)]
pub struct CaptureTiming {
    /// 已经发生过的 `Present` 次数。下一帧的序号就是它。
    presents: u64,
    /// 是否已经有过至少一次 Present（决定「上一帧」是否存在）。
    started: bool,
    /// 当前帧内是否已拷过。
    captured_this_frame: bool,
}

impl CaptureTiming {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// probe 在每次 `Present` 调用（在调用原 `Present` 之前或之后都可以，
    /// 只要全程一致）。
    pub fn tick_present(&mut self) {
        self.presents += 1;
        self.started = true;
        self.captured_this_frame = false;
    }

    /// 当前正在渲染的帧序号（下一次 `Present` 会输出的那一帧）。
    #[must_use]
    pub fn current_present_seq(&self) -> u64 {
        self.presents
    }

    /// 刚刚 `Present` 出去的那一帧的序号——**copy-before-clear 拷到的就是它**。
    #[must_use]
    pub fn previous_present_seq(&self) -> Option<u64> {
        self.presents.checked_sub(1)
    }

    /// `ClearDepthStencilView` 命中时调用。
    ///
    /// `is_selected_dsv` 由调用方按 profile 判定（几何 + 资源指针）。
    /// 返回 `Ok` 表示**应当在调用原 Clear 之前**把该 DSV 的资源拷进 staging。
    pub fn on_clear_depth(&mut self, is_selected_dsv: bool) -> Result<CaptureDecision, SkipReason> {
        if !is_selected_dsv {
            return Err(SkipReason::NotTheSelectedDsv);
        }
        if !self.started {
            return Err(SkipReason::NoPreviousFrame);
        }
        if self.captured_this_frame {
            return Err(SkipReason::AlreadyCapturedThisFrame);
        }
        let prev = self
            .previous_present_seq()
            .ok_or(SkipReason::NoPreviousFrame)?;
        // 68 号 §6.5：偶数帧才有 depth
        if !prev.is_multiple_of(2) {
            return Err(SkipReason::OddFrameBySpec);
        }
        self.captured_this_frame = true;
        Ok(CaptureDecision {
            belongs_to_present_seq: prev,
        })
    }
}

/// 索引 schema 标签（[85 号 §4](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md)）。
/// v1 的行没有这个字段，读取端按「缺失即 v1」处理。
pub const INDEX_SCHEMA_V2: &str = "depth-index/v2";
/// v3 起每行必须带 [`IndexProjection`]。
///
/// 为什么值得升一版而不是悄悄加可选字段：v2 的行**没有**投影参数，
/// 而 v3 的行即使参数未知也会显式写 `"proj_source":"unknown"`。
/// 前者是「这个版本不记」，后者是「这次采集没记到」——
/// 处置完全不同（换工具 vs 补标定），schema 号让它们分得开。
pub const INDEX_SCHEMA_V3: &str = "depth-index/v3";
/// v4 起每行必须带 [`DepthStorage::transform`]。
///
/// 为什么又升一版而不是加个可选字段：v3 的行**没有**这个键，而它们证明是
/// [`StorageTransform::Identity`]（当时代码只写原始 `d`）。若把新字段做成
/// 「缺失即 identity」，那么将来 writer 忘了写，读方也会读成 identity——
/// 与真的是 identity 完全不可区分，而后果是深度整体反过来却看起来正常
/// （[94 号](../../../docs/94-depth-buffer-encoding-decision.md)）。
/// schema 号让「这个版本不记」和「这次忘了记」分得开。
pub const INDEX_SCHEMA_V4: &str = "depth-index/v4";
/// v5 adds mandatory per-frame raw/stored health evidence.
///
/// This is a schema bump rather than an optional v4 field for the same reason v4 exists:
/// a writer that forgets durable health evidence must be distinguishable from an older writer
/// that never promised to produce it.
pub const INDEX_SCHEMA_V5: &str = "depth-index/v5";
/// v6 makes projection temporal validity and the selected DSV pass observable.
///
/// Both fields are mandatory for v6 writers. Older rows remain readable, but a missing
/// projection scope is interpreted fail-closed as a run-level assumption; it is never upgraded
/// to a per-frame observation by a reader default.
pub const INDEX_SCHEMA_V6: &str = "depth-index/v6";

/// 落盘前对缓冲值做的可逆变换。
///
/// # 为什么需要它
///
/// fp16 的精度是**相对**的（尾数 10 位，相对分辨率恒为 2⁻¹¹），所以它保住的是
/// 「存进去的那个量」的相对精度，不是深度的。
///
/// - reversed-Z：`d = b/z` 本身就正比于 `1/z`，fp16(`d`) 的相对精度直接等于
///   `dz/z = 2⁻¹¹`——已经是逆深度，什么都不用做。
/// - 传统 Z：`d = 1 − n/z`，绝大多数像素挤在 `d → 1` 的一小段里。
///   `dz/z = dd/(1−d) ≈ (dd/d)·z/n`，**误差被放大 `z/n` 倍**。
///   Fallout 4 的 `n = 0.214 m`，20 m 处放大 93 倍——实采一帧只剩 185 个唯一值。
///
/// 存 `1 − d ∝ n/z` 把传统 Z 也变成逆深度，相对精度回到 2⁻¹¹，
/// **与 68 号交付档要求的 fp16(`z`) 逐位同阶**。
///
/// # 为什么它必须显式落进索引
///
/// 存了 `1−d` 而读时忘了还原，得到的深度是整体反过来的——近处变远、远处变近，
/// 而画面**看起来仍然完全正常**。这与 `z_convention` 填反是同一类静默失效，
/// 唯一的防线是让每一行自己说清楚做过什么，且读方不许猜
/// （见 [`DepthIndexEntry::resolve_transform`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageTransform {
    /// 盘上就是缓冲原值 `d`。reversed-Z 用这个。
    Identity,
    /// 盘上是 `1 − d`。传统 Z 用这个。
    OneMinusD,
}

impl StorageTransform {
    /// 这个 Z 约定下该用哪个变换。**只有这一个地方决定这件事**——
    /// 散在两处迟早不一致。
    #[must_use]
    pub fn for_convention(conv: crate::decode::ZConvention) -> Self {
        match conv {
            // 已经是逆深度量级，再变一次只会把精度挪到远端
            crate::decode::ZConvention::Reversed => Self::Identity,
            crate::decode::ZConvention::Traditional => Self::OneMinusD,
        }
    }

    /// 落盘方向：缓冲值 → 盘上值。
    ///
    /// NaN 必须原样穿过——68 号用它标无效像素，而 `1.0 - NaN` 恰好也是 NaN，
    /// 所以这里不需要特判，但下面有测试钉住它。
    #[must_use]
    pub fn apply(self, d: f32) -> f32 {
        match self {
            Self::Identity => d,
            Self::OneMinusD => 1.0 - d,
        }
    }

    /// 读盘方向：盘上值 → 缓冲值。`1−d` 是自反的，所以与 [`Self::apply`] 同式，
    /// 但**必须是两个函数**：写成一个的话，调用点读不出方向，
    /// 而方向弄反正是这个字段要防的事。
    #[must_use]
    pub fn invert(self, stored: f32) -> f32 {
        match self {
            Self::Identity => stored,
            Self::OneMinusD => 1.0 - stored,
        }
    }
}

/// 帧字节在盘上是怎么编码的。
///
/// **`domain`、`encoding`、`transform` 必须分开记。** 一份 fp16 EXR 里装的可能是原始缓冲值
/// （探针未标定），也可能是米制 view-space 深度（引擎路径或已标定探针）——
/// 字节形态一样，能不能直接交付却完全相反。靠文件名或目录猜这件事迟早出错。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthStorage {
    /// 容器与数值格式。当前只有 `exr-half-zip` 一种。
    pub encoding: String,
    /// `raw_buffer`（归一化深度缓冲值，未线性化）或 `view_space_meters`。
    ///
    /// **`transform` 不改变 `domain`**：存 `1−d` 仍然是缓冲域的值，
    /// 只是换了个更适合 fp16 的坐标。线性化成米是另一件事，需要投影参数。
    pub domain: String,
    /// EXR 单通道名，68 号 §6.4 要求 `Y` 或 `Z`。
    pub channel: String,
    /// v4 起必填。`None` 只在 v2/v3 的旧行上合法（那些行证明是 identity）——
    /// 判定统一走 [`DepthIndexEntry::resolve_transform`]，不要直接读这个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<StorageTransform>,
}

impl DepthStorage {
    /// 探针路径：fp16 EXR 装原始缓冲值，带显式变换标记。
    #[must_use]
    pub fn raw_buffer_exr(transform: StorageTransform) -> Self {
        Self {
            encoding: "exr-half-zip".into(),
            domain: "raw_buffer".into(),
            channel: crate::EXR_CHANNEL_NAME.into(),
            transform: Some(transform),
        }
    }
}

/// 索引里投影参数的来源。
///
/// 与 [`crate::profile::ProjectionSource`] 的区别：那个描述 profile 里的参数**是怎么
/// 定出来的**，只有三种合法来源；这里多一个 [`Self::Unknown`]，因为索引必须能表达
/// 「这一帧根本没有参数」——那正是 61 GiB 报废的那次采集的状态
/// （[#490](https://github.com/DiogenesModel/Diogenes/issues/490)），
/// 而当时它在索引里**完全不可见**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexProjectionSource {
    /// 引擎已知默认值。
    EngineDefault,
    /// 游戏内已知距离标定。
    Calibrated,
    /// 当帧从渲染常量缓冲读出（P4）。**只有这一种能证明参数逐帧没变。**
    ConstantBuffer,
    /// **参数在场，但没过交叉验证**（[91 号](../../../docs/91-fallout4-depth-projection-evidence.md) 的 Fallout 4 就是这个状态）。
    ///
    /// # 为什么值得单独一个取值，而不是并进 `Unknown`
    ///
    /// 这两种状态的**下一步动作不同**：`Unknown` 要去查引擎默认值或做标定，
    /// `Candidate` 是数字已经有了、只差一次锚点交叉验证。并成一个就分不出
    /// 「还没查」和「查了但没验」，而后者离可交付近得多。
    ///
    /// 更实际的一点：候选参数至少给出了 `reverse_z`，于是收口那道
    /// 「传统 Z 却按 identity 落盘」的门**才有可能生效**——
    /// `Unknown` 时 `reverse_z` 是 `None`，那道门根本不触发。
    ///
    /// # 但它一样不可交付
    ///
    /// [`IndexProjection::is_metric_resolvable`] 对它返回 `false`。
    /// 候选值只用于采集期自检与事后比对，**不许拿去物化交付档**——
    /// 标定错 10% 的数据看起来和对的一模一样。
    Candidate,
    /// 没有参数。这一帧的深度**事后无法线性化**。
    Unknown,
}

/// 投影参数在时间轴上的有效范围。
///
/// `proj_source` 只说明数字从哪里来；它不能回答这个数字是当帧读到的，还是整段 run
/// 重复写入的配置常量。两者混在一起会让“逐帧参数恒定”检查结构上永远通过（#575）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionScope {
    /// 当帧从渲染常量缓冲观测到。只有这档能证明参数在这一帧成立。
    ObservedThisFrame,
    /// 整段 run 假定不变。旧 schema 缺字段时也按这一档处理，绝不脑补为逐帧观测。
    #[default]
    AssumedConstantForRun,
}

impl From<crate::profile::ProjectionSource> for IndexProjectionSource {
    fn from(s: crate::profile::ProjectionSource) -> Self {
        match s {
            crate::profile::ProjectionSource::EngineDefault => Self::EngineDefault,
            crate::profile::ProjectionSource::Calibrated => Self::Calibrated,
            crate::profile::ProjectionSource::ConstantBuffer => Self::ConstantBuffer,
        }
    }
}

/// 逐帧记录的投影参数。**逐帧而不是 run 级**——见下。
///
/// # 为什么必须逐帧
///
/// 「一次 run 一个 `--near`」的前提是「全程不变」。而 [#490](https://github.com/DiogenesModel/Diogenes/issues/490)
/// 的那次 ETS2 采集里，同一次 run 两帧的值域差 12 倍以上、第二帧一个 clear 像素都没有——
/// **到底是投影换了（内/外视角 near plane 不同）还是天空盒画在有限深度上，
/// 从数据本身分不出来**，因为参数没记。
///
/// 所以逐帧记的理由不是「已经证明它会变」，而是**「变没变都无法验证」**。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IndexProjection {
    /// `d = a + b/z` 的两个系数。**落 a/b 而不是 near/far**：
    /// 后者要求读方自己判断 reversed-Z 与否，a/b 没有这个歧义。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proj_a: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proj_b: Option<f64>,
    /// 由 `proj_b` 的符号推出（[`crate::depth::Projection::is_reversed_z`]），
    /// 冗余但便宜，供人核对。参数未知时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverse_z: Option<bool>,
    /// 游戏单位 → 米。
    ///
    /// **未标定时是 `None`，不是 `1.0`。** 落 1.0 会让下游把「没标定」读成
    /// 「这个引擎的单位就是米」——两者在数值上不可区分，而后者是个已被证实
    /// 对某些游戏成立、对另一些不成立的断言。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_scale: Option<f64>,
    /// The viewport depth transform used by the exact draw that produced this frame.
    ///
    /// `proj_a` / `proj_b` describe the final depth-buffer value, after applying
    /// `d = min_depth + (max_depth - min_depth) * d_ndc`. Older v6 rows predate these
    /// fields and remain readable; their coefficients keep their original meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport_min_depth: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport_max_depth: Option<f64>,
    pub proj_source: IndexProjectionSource,
    /// 数字在时间轴上的有效范围。v6 起必填；旧行缺失时 fail-closed 为 run 级假定。
    #[serde(default)]
    pub proj_scope: ProjectionScope,
}

impl IndexProjection {
    /// 「这一帧没有投影参数」。**这是显式状态，不是缺省**。
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            proj_a: None,
            proj_b: None,
            reverse_z: None,
            metric_scale: None,
            viewport_min_depth: None,
            viewport_max_depth: None,
            proj_source: IndexProjectionSource::Unknown,
            proj_scope: ProjectionScope::AssumedConstantForRun,
        }
    }

    /// 由一组投影参数构造。`reverse_z` 在这里**推导**，调用方给不了它——
    /// 见 [`crate::depth::Projection::z_convention`] 里对「为什么不是独立字段」的说明。
    #[must_use]
    pub fn known(
        projection: crate::depth::Projection,
        metric_scale: Option<f64>,
        source: IndexProjectionSource,
    ) -> Self {
        Self {
            proj_a: Some(projection.a),
            proj_b: Some(projection.b),
            reverse_z: Some(projection.is_reversed_z()),
            metric_scale,
            viewport_min_depth: None,
            viewport_max_depth: None,
            proj_source: source,
            proj_scope: ProjectionScope::AssumedConstantForRun,
        }
    }

    /// 由当帧常量缓冲观测到的一组投影参数。
    #[must_use]
    pub fn observed(projection: crate::depth::Projection, metric_scale: Option<f64>) -> Self {
        Self {
            proj_a: Some(projection.a),
            proj_b: Some(projection.b),
            reverse_z: Some(projection.is_reversed_z()),
            metric_scale,
            viewport_min_depth: None,
            viewport_max_depth: None,
            proj_source: IndexProjectionSource::ConstantBuffer,
            proj_scope: ProjectionScope::ObservedThisFrame,
        }
    }

    /// Construct coefficients observed at the exact draw after applying its viewport mapping.
    #[must_use]
    pub fn observed_with_viewport(
        projection: crate::depth::Projection,
        metric_scale: Option<f64>,
        min_depth: f64,
        max_depth: f64,
    ) -> Self {
        Self {
            proj_a: Some(projection.a),
            proj_b: Some(projection.b),
            reverse_z: Some(projection.is_reversed_z()),
            metric_scale,
            viewport_min_depth: Some(min_depth),
            viewport_max_depth: Some(max_depth),
            proj_source: IndexProjectionSource::ConstantBuffer,
            proj_scope: ProjectionScope::ObservedThisFrame,
        }
    }

    /// 这一帧的深度事后能不能线性化成米。
    ///
    /// 三者缺一不可且必须是有效数值：a/b 要有限、b 不能为 0、metric_scale 必须
    /// 有限且为正。
    ///
    /// **来源也算判据**：`unknown` 是没有数字，`candidate` 是数字没过交叉验证——
    /// 两者都不算可解。有数字不等于可交付，标定错 10% 的数据看起来和对的一模一样。
    #[must_use]
    pub fn is_metric_resolvable(&self) -> bool {
        let (Some(a), Some(b), Some(scale)) = (self.proj_a, self.proj_b, self.metric_scale) else {
            return false;
        };
        let viewport_valid = match (self.viewport_min_depth, self.viewport_max_depth) {
            (None, None) => true,
            (Some(min_depth), Some(max_depth)) => {
                min_depth.is_finite()
                    && max_depth.is_finite()
                    && (0.0..=1.0).contains(&min_depth)
                    && (0.0..=1.0).contains(&max_depth)
                    && min_depth < max_depth
            }
            _ => false,
        };
        a.is_finite()
            && b.is_finite()
            && b != 0.0
            && scale.is_finite()
            && scale > 0.0
            && viewport_valid
            && self.proj_source == IndexProjectionSource::ConstantBuffer
            && self.proj_scope == ProjectionScope::ObservedThisFrame
    }
}

/// 本帧实际落盘的 DSV pass 证据（#577）。
///
/// 这三个字段不是选择策略，而是生产事实：本帧一共完成多少次受审 DSV pass、实际快照
/// 取自帧尾倒数第几次，以及该 pass 内执行了多少次 raster draw。画质或渲染管线漂移
/// 因而会直接出现在索引里，不再只藏在运行时日志中。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DsvPassEvidence {
    pub unbind_ordinal_from_end: u32,
    pub unbind_total: u32,
    pub draw_calls: u64,
    pub selection: crate::dsv::DsvSelectionAudit,
}

impl DsvPassEvidence {
    pub fn validate(&self) -> Result<(), String> {
        if self.unbind_ordinal_from_end == 0 {
            return Err("unbind_ordinal_from_end must be at least 1".into());
        }
        if self.unbind_total == 0 {
            return Err("unbind_total must be at least 1".into());
        }
        if self.unbind_ordinal_from_end > self.unbind_total {
            return Err(format!(
                "unbind_ordinal_from_end {} exceeds unbind_total {}",
                self.unbind_ordinal_from_end, self.unbind_total
            ));
        }
        if self.selection.selected_id.trim().is_empty() {
            return Err("selection.selected_id must not be empty".into());
        }
        Ok(())
    }
}

/// 一次成功回读后要落进 `depth-index.jsonl` 的一行。
///
/// `present_seq` **必须**是 [`CaptureDecision::belongs_to_present_seq`]，
/// 不是回读完成时的当前帧号——两者相差至少一帧，混用就是全局错位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepthIndexEntry {
    /// v2 行为 [`INDEX_SCHEMA_V2`]，带逐帧投影参数的 v3 行为 [`INDEX_SCHEMA_V3`]，
    /// 带落盘变换的 v4 行为 [`INDEX_SCHEMA_V4`]，带逐帧健康证据的 v5 行为
    /// [`INDEX_SCHEMA_V5`]，带投影时间范围与 DSV pass 证据的 v6 行为
    /// [`INDEX_SCHEMA_V6`]。
    pub schema: String,
    pub present_seq: u64,
    /// 这一帧深度对应的 RGB 源帧号（85 号 C3）。探针路径上它等于 `present_seq`；
    /// 引擎路径上 depth 低频采样时它指回 30 fps 那条时间线上的帧。
    pub source_frame: u64,
    /// 68 号 §6.5 的交付命名 `depth_<偶数帧>`。
    ///
    /// **探针层写 `None`，这不是偷懒**：交付命名是相对每条切出来的 clip 从 0 重新
    /// 编号的，而探针只知道全局 `present_seq`（一次采集能跑到 35766）。在这里填一个
    /// 看起来像交付名、实际不是的字符串，比留空危险得多。切段时由物化步骤补上。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth_name: Option<String>,
    pub width: u32,
    pub height: u32,
    /// 回读时的 DSV 原始格式。它描述**来源**，不是盘上的编码——后者看 `storage`。
    #[serde(rename = "buffer_format")]
    pub format: crate::decode::DepthFormat,
    pub dsv_id: String,
    pub storage: DepthStorage,
    /// 相对 session 目录的路径。
    pub path: String,
    /// 从 clear 时刻发出拷贝，到 `Map` 成功读出，经过了几个 `Present`。
    pub readback_latency_frames: u64,
    /// 逐帧投影参数（v3 起必填，未知时是显式的 `unknown`）。
    ///
    /// `#[serde(default)]` 让 v2 的旧索引仍能读进来——旧行会得到 `unknown`，
    /// 语义正确：那些行确实没有参数。
    #[serde(default = "IndexProjection::unknown")]
    pub projection: IndexProjection,
    /// v5 起必填。旧索引没有这条证据，仍可读，但不能冒充采集期已监控。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<crate::frame_health::FrameHealthEvidence>,
    /// v6 起必填。旧索引没有 pass 证据，仍可读取取证，但不能冒充生产者记录过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<DsvPassEvidence>,
}

impl DepthIndexEntry {
    /// 这一行的字节要怎么还原成缓冲值 `d`。**读取端唯一允许的取值方式**，
    /// 不要直接读 `storage.transform`。
    ///
    /// 三种情况，处置刻意不同：
    ///
    /// - **有值** → 用它。未知的取值（将来的 writer 写了本版不认识的变换）
    ///   在 serde 那一层就解析失败了，到不了这里。
    /// - **v4 却没值** → 拒。v4 的契约就是必填，缺了说明 writer 有 bug，
    ///   而此时盘上的字节到底做没做过变换**无从判断**——猜 identity 会把
    ///   一份变换过的数据整体读反，且看起来正常。
    /// - **v2/v3 没值** → identity。这不是猜：那两版的 writer 代码只写原始 `d`，
    ///   是可查证的事实，不是默认假设。
    pub fn resolve_transform(&self) -> Result<StorageTransform, String> {
        if let Some(t) = self.storage.transform {
            return Ok(t);
        }
        if matches!(
            self.schema.as_str(),
            INDEX_SCHEMA_V4 | INDEX_SCHEMA_V5 | INDEX_SCHEMA_V6
        ) {
            return Err(format!(
                "present_seq {} 是 {} 却没有 storage.transform——\
                 该版本必填。盘上字节是否做过 1−d 变换无法判断，\
                 猜 identity 会把深度整体读反且看起来正常（94 号），故拒绝",
                self.present_seq, self.schema
            ));
        }
        Ok(StorageTransform::Identity)
    }

    /// Return durable health evidence when this schema carries it.
    ///
    /// v5 without evidence is a writer contract failure and is rejected. v2-v4 return `None`
    /// because those versions genuinely predate the field.
    pub fn resolve_health(
        &self,
    ) -> Result<Option<&crate::frame_health::FrameHealthEvidence>, String> {
        if let Some(health) = self.health.as_ref() {
            health.validate().map_err(|error| {
                format!(
                    "present_seq {} has invalid frame health evidence: {error}",
                    self.present_seq
                )
            })?;
            return Ok(Some(health));
        }
        if matches!(self.schema.as_str(), INDEX_SCHEMA_V5 | INDEX_SCHEMA_V6) {
            return Err(format!(
                "present_seq {} is {} but has no health evidence",
                self.present_seq, self.schema
            ));
        }
        Ok(None)
    }

    /// Return the selected DSV pass evidence promised by v6.
    pub fn resolve_pass(&self) -> Result<Option<&DsvPassEvidence>, String> {
        if let Some(pass) = self.pass.as_ref() {
            pass.validate().map_err(|error| {
                format!(
                    "present_seq {} has invalid DSV pass evidence: {error}",
                    self.present_seq
                )
            })?;
            return Ok(Some(pass));
        }
        if self.schema == INDEX_SCHEMA_V6 {
            return Err(format!(
                "present_seq {} is {INDEX_SCHEMA_V6} but has no DSV pass evidence",
                self.present_seq
            ));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 空帧与 schema 校验（2026-08-05 实采暴露）---

    /// 空帧的判据是「整帧只有一个值」，不是「等于某个写死的 clear 值」。
    /// 传统 Z 清 1.0、reversed-Z 清 0.0，写死任何一个都会漏掉另一种。
    #[test]
    fn a_blank_frame_is_detected_whatever_the_clear_value_is() {
        assert_eq!(
            classify_frame(&[1.0; 16]),
            FrameContent::Blank {
                clear_value_bits: 1.0f32.to_bits()
            },
            "传统 Z 清成 1.0"
        );
        assert_eq!(
            classify_frame(&[0.0; 16]),
            FrameContent::Blank {
                clear_value_bits: 0.0f32.to_bits()
            },
            "reversed-Z 清成 0.0"
        );
        // 一个像素不同就算有几何——判据必须是「只有一个值」而不是「大部分是一个值」，
        // 否则天空占九成的正常帧会被误判成空帧
        let mut almost = [1.0f32; 16];
        almost[7] = 0.5;
        assert_eq!(classify_frame(&almost), FrameContent::HasGeometry);
    }

    /// **2026-08-05 Fallout 4 实采的形态标定。**
    ///
    /// 652 帧抽样（1/12 系统抽样）里 70 帧空，但 87% 落在长连段里，最长一段
    /// ≈420 实际帧（约 14 秒）。这是读盘/菜单，不是缺陷。
    ///
    /// 这条测试用等价形态：大部分空帧在长段里，另有少量孤立单帧。
    /// 门必须**放行长段、抓住孤立帧**。
    #[test]
    fn long_gaps_are_gameplay_states_not_defects() {
        let mut frames: Vec<(u64, bool)> = (0..2000).map(|i| (i * 2, false)).collect();
        // 一段 420 帧的读盘
        for f in frames.iter_mut().skip(300).take(420) {
            f.1 = true;
        }
        // 再来两段菜单
        for f in frames.iter_mut().skip(1000).take(60) {
            f.1 = true;
        }
        for f in frames.iter_mut().skip(1500).take(120) {
            f.1 = true;
        }
        let r = summarize_blank_frames(&frames);
        assert_eq!(r.blank, 600);
        assert_eq!(r.in_gameplay_gaps, 600, "全部应当归入游戏状态间隙");
        assert_eq!(r.isolated, 0);
        assert_eq!(r.longest_gap, 420);
        assert!(
            r.passes(),
            "30% 的空帧但全在长段里，不该判失败：{}",
            r.summary()
        );
    }

    /// 孤立单帧才是快照时机选错的形态——即使占比很小也要抓住。
    #[test]
    fn scattered_single_blanks_fail_even_at_low_rates() {
        // 1% 的孤立空帧，均匀散布
        let frames: Vec<(u64, bool)> = (0..2000).map(|i| (i * 2, i % 100 == 0)).collect();
        let r = summarize_blank_frames(&frames);
        assert_eq!(r.isolated, 20);
        assert_eq!(r.in_gameplay_gaps, 0);
        assert_eq!(r.longest_gap, 1);
        assert!(
            !r.passes(),
            "散布的孤立空帧必须判失败，哪怕只有 1%：{}",
            r.summary()
        );
        assert!(!r.isolated_seqs.is_empty(), "必须报出是哪些帧");
    }

    /// 两种形态混在一起时，判定只看孤立那部分——否则长段会把缺陷稀释掉。
    #[test]
    fn a_long_gap_does_not_dilute_a_real_defect() {
        let mut frames: Vec<(u64, bool)> = (0..2000).map(|i| (i * 2, false)).collect();
        for f in frames.iter_mut().skip(100).take(400) {
            f.1 = true; // 大段读盘
        }
        for i in (600..2000).step_by(97) {
            frames[i].1 = true; // 散布的孤立帧
        }
        let r = summarize_blank_frames(&frames);
        assert!(r.in_gameplay_gaps >= 400);
        assert!(r.isolated >= 10);
        assert!(!r.passes(), "长段不该把孤立缺陷稀释掉：{}", r.summary());
    }

    /// 整段采集几乎都在菜单里——不算缺陷，但也不够交付时长，同样要拦。
    #[test]
    fn a_capture_that_is_mostly_menus_is_still_refused() {
        let frames: Vec<(u64, bool)> = (0..1000).map(|i| (i * 2, i < 700)).collect();
        let r = summarize_blank_frames(&frames);
        assert_eq!(r.isolated, 0, "700 帧连续，全是间隙");
        assert!(r.ratio() > 0.5);
        assert!(!r.passes(), "70% 的时间在菜单里，交付时长不够");
    }

    /// ETS2 那 61 GiB 的实测：358 帧抽样零空帧。干净的采集必须过。
    #[test]
    fn a_clean_capture_passes() {
        let frames: Vec<(u64, bool)> = (0..358).map(|i| (i * 100, false)).collect();
        let r = summarize_blank_frames(&frames);
        assert_eq!(r.blank, 0);
        assert!(r.passes());
        assert!(r.summary().contains("空帧 0"));
    }

    /// v1 索引不是「旧但能用」，是缺了区分交付与否所必需的字段。
    /// 2026-08-05 那次就是用 v1 构建采完 5 GiB 才发现。
    #[test]
    fn a_v1_index_is_refused_with_a_reason_that_says_what_to_do() {
        let err = index_schema_is_deliverable(None).unwrap_err();
        assert!(err.contains("v1"), "{err}");
        assert!(err.contains("重采"), "错误信息要说清楚下一步做什么：{err}");
        assert!(index_schema_is_deliverable(Some(INDEX_SCHEMA_V2)).is_ok());
        // 每一版都只是加字段，旧行在新版读法下语义不变，所以 v2-v5 都收。
        // 曾经这里只认 v2，而探针那时已经在写 v3——门是关着的，只是没人推过。
        assert!(index_schema_is_deliverable(Some(INDEX_SCHEMA_V3)).is_ok());
        assert!(index_schema_is_deliverable(Some(INDEX_SCHEMA_V4)).is_ok());
        assert!(index_schema_is_deliverable(Some(INDEX_SCHEMA_V5)).is_ok());
        assert!(index_schema_is_deliverable(Some(INDEX_SCHEMA_V6)).is_ok());
        assert!(index_schema_is_deliverable(Some("depth-index/v9")).is_err());
    }

    // --- 落盘变换（94 号裁决）---

    /// fp16 保的是「存进去那个量」的相对精度。传统 Z 的 `d` 挤在 1 附近，
    /// 存原值等于把深度层次扔掉——这个测试用真实参数把损失钉死，
    /// 免得又出现「在 reversed-Z 上验证过就当已验证」那种事。
    #[test]
    fn storing_raw_d_destroys_traditional_z_depth_but_one_minus_d_does_not() {
        // Fallout 4 实测：near = 15 游戏单位 = 0.2143 m（91 号）
        let near_m = 0.2143f64;
        // fp16 在 [0.5,1) 区间的绝对分辨率 = 2⁻¹¹（尾数 10 位）
        let ulp_near_one = 2f64.powi(-11);

        // 20 m 处，传统 Z 的 d = 1 − n/z
        let z = 20.0f64;
        let d = 1.0 - near_m / z;
        assert!(d > 0.98, "传统 Z 在 20 m 处已经贴着 1：d={d}");

        // 存 d：一个 ulp 的 d 误差对应多大的 z 误差
        let dz_identity = (near_m / (1.0 - (d + ulp_near_one)) - z).abs();
        assert!(
            dz_identity > 0.9,
            "存原值时 20 m 处分辨率应劣于 0.9 m，实得 {dz_identity}"
        );

        // 存 1−d：1−d = n/z ≈ 0.0107，fp16 的相对精度恒为 2⁻¹¹
        let dz_inverse = z * 2f64.powi(-11);
        assert!(
            dz_inverse < 0.011,
            "存逆深度时 20 m 处分辨率应优于 1.1 cm，实得 {dz_inverse}"
        );
        // 差两个数量级以上——这就是 94 号裁决的全部理由
        assert!(dz_identity / dz_inverse > 80.0);
    }

    #[test]
    fn the_transform_is_chosen_by_the_z_convention_in_exactly_one_place() {
        use crate::decode::ZConvention;
        // reversed-Z 的 d 本身就是逆深度量级，再变换只会把精度挪到远端
        assert_eq!(
            StorageTransform::for_convention(ZConvention::Reversed),
            StorageTransform::Identity
        );
        assert_eq!(
            StorageTransform::for_convention(ZConvention::Traditional),
            StorageTransform::OneMinusD
        );
    }

    #[test]
    fn every_transform_round_trips_and_lets_nan_through() {
        for t in [StorageTransform::Identity, StorageTransform::OneMinusD] {
            for d in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
                let back = t.invert(t.apply(d));
                assert!((back - d).abs() < 1e-6, "{t:?} 对 {d} 往返不闭合：{back}");
            }
            // 68 号用 NaN 标无效像素，过变换必须还是 NaN——
            // 变成 0 会被读成「贴着镜头的表面」
            assert!(t.apply(f32::NAN).is_nan(), "{t:?} 把 NaN 吃掉了");
            assert!(t.invert(f32::NAN).is_nan());
        }
    }

    /// v4 缺 transform 必须拒。**这是本次改动的核心防线**：
    /// 猜 identity 会把一份变换过的数据整体读反，而画面看起来完全正常。
    #[test]
    fn a_v4_row_without_a_transform_is_refused_rather_than_assumed_identity() {
        let mut e = sample_entry();
        e.storage.transform = None;
        let err = e.resolve_transform().unwrap_err();
        assert!(err.contains("12344"), "错误要指出是哪一帧：{err}");
        assert!(err.contains("transform"), "{err}");
    }

    #[test]
    fn a_v5_row_without_health_is_refused_but_v4_remains_readable() {
        let mut entry = sample_entry();
        assert_eq!(entry.resolve_health().unwrap(), None);
        entry.schema = INDEX_SCHEMA_V5.into();
        let error = entry.resolve_health().unwrap_err();
        assert!(error.contains(INDEX_SCHEMA_V5), "{error}");
        assert!(error.contains("health"), "{error}");
    }

    /// 但 v2/v3 缺 transform 是 identity，**这不是猜**——那两版的 writer
    /// 代码只写原始 `d`，可查证。拒绝它们只会让旧数据连取证都做不了。
    #[test]
    fn a_pre_v4_row_without_a_transform_reads_as_identity() {
        for schema in [INDEX_SCHEMA_V2, INDEX_SCHEMA_V3] {
            let mut e = sample_entry();
            e.schema = schema.into();
            e.storage.transform = None;
            assert_eq!(e.resolve_transform().unwrap(), StorageTransform::Identity);
        }
    }

    /// 不认识的变换名在 serde 层就该失败，而不是落到某个默认分支上。
    #[test]
    fn an_unknown_transform_name_fails_to_parse() {
        let row = r#"{"schema":"depth-index/v4","present_seq":2,"source_frame":2,
            "width":4,"height":4,"buffer_format":"d24s8","dsv_id":"m",
            "storage":{"encoding":"exr-half-zip","domain":"raw_buffer","channel":"Z",
                       "transform":"log_z"},
            "path":"raw/000000000002.exr","readback_latency_frames":2}"#;
        assert!(serde_json::from_str::<DepthIndexEntry>(row).is_err());
    }

    /// `transform` 与 `domain` 是两个轴：存 `1−d` 仍然是缓冲域，
    /// 线性化成米是另一件事、需要投影参数。合并这两个轴会让
    /// 「已变换但未标定」这个真实状态没法表达。
    #[test]
    fn the_transform_does_not_change_the_domain() {
        let s = DepthStorage::raw_buffer_exr(StorageTransform::OneMinusD);
        assert_eq!(s.domain, "raw_buffer");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["transform"], "one_minus_d");
    }

    /// 两份真实采集的清除值占比，用来钉住判据的门限。
    /// ETS2（reversed）9.24% 落在 0.0；Fallout 4（traditional）20.6% 落在 1.0。
    #[test]
    fn the_convention_is_inferred_from_the_clear_value() {
        use crate::decode::ZConvention;
        let n = 10_000;
        // reversed-Z：清除值 0.0，其余是 (0,1) 的几何
        let mut rev = vec![0.0f32; n / 10]; // 10% 清除区
        rev.extend((0..n - n / 10).map(|i| 0.1 + (i % 800) as f32 / 1000.0));
        assert_eq!(infer_z_convention(&rev), Some(ZConvention::Reversed));

        // 传统 Z：清除值 1.0
        let mut trad = vec![1.0f32; n / 5]; // 20% 清除区
        trad.extend((0..n - n / 5).map(|i| 0.1 + (i % 800) as f32 / 1000.0));
        assert_eq!(infer_z_convention(&trad), Some(ZConvention::Traditional));
    }

    /// 判不出来时必须返回 None 而不是猜一个——猜错比不知道更糟，
    /// 因为下游会拿它去决定「取最近表面」是 min 还是 max。
    #[test]
    fn an_undecidable_frame_returns_none_rather_than_guessing() {
        // 空帧：整帧都是 1.0，两端都不成立（0.0 那端为空）
        assert_eq!(infer_z_convention(&[1.0f32; 1000]).map(|_| ()), Some(()));
        // 没有清除区的帧：判不了
        let no_clear: Vec<f32> = (0..1000).map(|i| 0.2 + i as f32 / 10_000.0).collect();
        assert_eq!(infer_z_convention(&no_clear), None);
        assert_eq!(infer_z_convention(&[]), None);
        // 两端都有大片：判不了
        let mut both = vec![0.0f32; 300];
        both.extend(vec![1.0f32; 300]);
        both.extend((0..400).map(|i| 0.3 + i as f32 / 10_000.0));
        assert_eq!(infer_z_convention(&both), None);
    }

    /// 配置填错时必须能查出来。这是本函数存在的全部理由——
    /// 填错的表现是深度整体反过来，而画面看起来完全正常。
    #[test]
    fn a_mis_declared_convention_is_caught() {
        use crate::decode::ZConvention;
        let mut trad = vec![1.0f32; 2000];
        trad.extend((0..8000).map(|i| 0.1 + (i % 800) as f32 / 1000.0));
        assert_eq!(
            z_convention_matches(ZConvention::Traditional, &trad),
            Some(true)
        );
        assert_eq!(
            z_convention_matches(ZConvention::Reversed, &trad),
            Some(false),
            "把传统 Z 的数据声明成 reversed 必须被查出来"
        );
    }

    /// 单帧判不出时要靠多帧投票；分歧则一律 None——同一次采集里两种约定
    /// 并存是不可能的，真出现了说明有别的东西错了。
    #[test]
    fn the_convention_vote_needs_agreement_and_enough_frames() {
        use crate::decode::ZConvention;
        let trad: Vec<f32> = {
            let mut v = vec![1.0f32; 2000];
            v.extend((0..8000).map(|i| 0.1 + (i % 800) as f32 / 1000.0));
            v
        };
        let rev: Vec<f32> = {
            let mut v = vec![0.0f32; 2000];
            v.extend((0..8000).map(|i| 0.1 + (i % 800) as f32 / 1000.0));
            v
        };
        let undecidable: Vec<f32> = (0..10_000).map(|i| 0.2 + i as f32 / 1e6).collect();

        // 三帧一致 → 定
        assert_eq!(
            infer_z_convention_over_frames([trad.as_slice(), trad.as_slice(), trad.as_slice()]),
            Some(ZConvention::Traditional)
        );
        // 判得出的只有两帧 → 不够
        assert_eq!(
            infer_z_convention_over_frames([
                trad.as_slice(),
                trad.as_slice(),
                undecidable.as_slice()
            ]),
            None
        );
        // 分歧 → 一律 None
        assert_eq!(
            infer_z_convention_over_frames([
                trad.as_slice(),
                rev.as_slice(),
                trad.as_slice(),
                trad.as_slice()
            ]),
            None
        );
    }

    #[test]
    fn nothing_is_captured_before_the_first_present() {
        let mut t = CaptureTiming::new();
        assert_eq!(t.on_clear_depth(true), Err(SkipReason::NoPreviousFrame));
    }

    #[test]
    fn capture_belongs_to_the_previous_frame_not_the_current_one() {
        // 这是本模块存在的唯一理由：off-by-one 搞错就是整份数据错位一帧，
        // 而且看起来完全正常。
        let mut t = CaptureTiming::new();
        t.tick_present(); // 第 0 帧已输出，当前正在渲染第 1 帧
        assert_eq!(t.current_present_seq(), 1);
        let d = t.on_clear_depth(true).unwrap();
        assert_eq!(
            d.belongs_to_present_seq, 0,
            "在第 1 帧的 clear 处拷到的是第 0 帧的深度"
        );
    }

    #[test]
    fn odd_frames_are_skipped_by_the_even_frame_convention() {
        let mut t = CaptureTiming::new();
        t.tick_present(); // 上一帧 = 0（偶）
        assert!(t.on_clear_depth(true).is_ok());
        t.tick_present(); // 上一帧 = 1（奇）
        assert_eq!(t.on_clear_depth(true), Err(SkipReason::OddFrameBySpec));
        t.tick_present(); // 上一帧 = 2（偶）
        assert!(t.on_clear_depth(true).is_ok());
    }

    #[test]
    fn a_second_clear_in_the_same_frame_does_not_copy_again() {
        // 一帧里可能有多次 clear（不同 DSV、或引擎自己重复清）
        let mut t = CaptureTiming::new();
        t.tick_present();
        assert!(t.on_clear_depth(true).is_ok());
        assert_eq!(
            t.on_clear_depth(true),
            Err(SkipReason::AlreadyCapturedThisFrame)
        );
    }

    #[test]
    fn other_dsvs_never_trigger_a_copy() {
        // shadow cascade 每帧也会 clear 自己的 DSV
        let mut t = CaptureTiming::new();
        t.tick_present();
        assert_eq!(t.on_clear_depth(false), Err(SkipReason::NotTheSelectedDsv));
        // 而且不该把「本帧已拷」标记掉——真正的主场景 clear 随后还要拷
        assert!(t.on_clear_depth(true).is_ok());
    }

    #[test]
    fn a_frame_without_any_clear_simply_yields_no_depth() {
        // 有的帧可能根本不 clear（暂停菜单等）。不该 panic，也不该顺延到下一帧冒名顶替。
        let mut t = CaptureTiming::new();
        t.tick_present(); // 上一帧 0
        t.tick_present(); // 上一帧 1，中间没 clear
        t.tick_present(); // 上一帧 2
        let d = t.on_clear_depth(true).unwrap();
        assert_eq!(
            d.belongs_to_present_seq, 2,
            "必须归属当前的上一帧，不能补第 0 帧"
        );
    }

    #[test]
    fn sequence_over_many_frames_only_yields_even_seqs() {
        let mut t = CaptureTiming::new();
        let mut got = Vec::new();
        for _ in 0..20 {
            t.tick_present();
            if let Ok(d) = t.on_clear_depth(true) {
                got.push(d.belongs_to_present_seq);
            }
        }
        assert_eq!(got, vec![0, 2, 4, 6, 8, 10, 12, 14, 16, 18]);
        assert!(got.iter().all(|s| s.is_multiple_of(2)));
    }

    fn sample_entry() -> DepthIndexEntry {
        DepthIndexEntry {
            schema: INDEX_SCHEMA_V4.into(),
            present_seq: 12344,
            source_frame: 12344,
            depth_name: None,
            width: 1920,
            height: 1080,
            format: crate::decode::DepthFormat::D24S8,
            dsv_id: "main-scene-v1".into(),
            storage: DepthStorage::raw_buffer_exr(StorageTransform::Identity),
            path: "runs/r7/segments/00003/raw/000000012344.exr".into(),
            readback_latency_frames: 2,
            projection: IndexProjection::known(
                crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
                Some(1.0),
                IndexProjectionSource::Calibrated,
            ),
            health: None,
            pass: None,
        }
    }

    #[test]
    fn an_unknown_projection_writes_metric_scale_as_absent_not_one() {
        // 落 1.0 会让下游把「没标定」读成「这个引擎的单位就是米」——
        // 两者数值上不可区分，而后者是个对某些游戏成立、对另一些不成立的断言。
        let u = IndexProjection::unknown();
        assert_eq!(u.metric_scale, None);
        assert!(!u.is_metric_resolvable());
        let json = serde_json::to_string(&u).unwrap();
        assert!(
            !json.contains("metric_scale"),
            "未知时不该写出这个键：{json}"
        );
        assert!(json.contains("\"proj_source\":\"unknown\""), "{json}");
    }

    #[test]
    fn reverse_z_is_derived_and_cannot_contradict_proj_b() {
        // 独立存一个 reverse_z 布尔会让两个真值来源互相矛盾，
        // 而矛盾时哪个说了算没有答案。这里它是算出来的。
        let rev = IndexProjection::known(
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            Some(1.0),
            IndexProjectionSource::ConstantBuffer,
        );
        assert_eq!(rev.reverse_z, Some(true));
        assert!(rev.proj_b.unwrap() > 0.0);

        let trad = IndexProjection::known(
            crate::depth::Projection::traditional_z(0.1, 1000.0, 1.0),
            Some(1.0),
            IndexProjectionSource::EngineDefault,
        );
        assert_eq!(trad.reverse_z, Some(false));
        assert!(trad.proj_b.unwrap() < 0.0);
    }

    #[test]
    fn a_v2_index_row_still_parses_and_reports_unknown() {
        // 61 GiB 那批就是 v2。读进来必须显式变成 unknown，
        // 而不是解析失败（那样连「它缺什么」都看不到）。
        let v2 = r#"{"schema":"depth-index/v2","present_seq":2,"source_frame":2,
            "width":1280,"height":720,"buffer_format":"d24s8","dsv_id":"main-scene-v1",
            "storage":{"encoding":"exr-half-zip","domain":"raw_buffer","channel":"Z"},
            "path":"raw/000000000002.exr","readback_latency_frames":2}"#;
        let row: DepthIndexEntry = serde_json::from_str(v2).unwrap();
        assert_eq!(row.projection.proj_source, IndexProjectionSource::Unknown);
        assert_eq!(
            row.projection.proj_scope,
            ProjectionScope::AssumedConstantForRun
        );
        assert!(!row.projection.is_metric_resolvable());
    }

    /// 候选参数**不是**可交付参数。它换来的是 `reverse_z` 有值
    /// （于是精度门才会触发）和事后可比对，不是交付资格。
    #[test]
    fn candidate_parameters_are_recorded_but_never_deliverable() {
        let c = IndexProjection::known(
            crate::depth::Projection::traditional_z(0.2143, 1000.0, 0.0142875),
            Some(0.0142875),
            IndexProjectionSource::Candidate,
        );
        assert!(
            !c.is_metric_resolvable(),
            "候选值不能冒充已标定——标定错 10% 的数据看起来和对的一模一样"
        );
        // 但数字与 Z 约定都在，这正是它相对 unknown 的全部价值
        assert_eq!(c.reverse_z, Some(false));
        assert!(c.proj_a.is_some() && c.metric_scale.is_some());

        let u = IndexProjection::unknown();
        assert_eq!(u.reverse_z, None, "unknown 时精度门根本不触发，这就是差别");

        // 序列化成 snake_case，与其他取值一致
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v["proj_source"], "candidate");
    }

    #[test]
    fn metric_resolvable_needs_all_three() {
        // 少任何一个，z = b/(d−a)·scale 都算不出米制值。
        let no_scale = IndexProjection::observed(
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            None,
        );
        assert!(
            !no_scale.is_metric_resolvable(),
            "缺 metric_scale 就不该算可解"
        );
        let assumed = IndexProjection::known(
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            Some(0.02),
            IndexProjectionSource::ConstantBuffer,
        );
        assert!(
            !assumed.is_metric_resolvable(),
            "run 级常量即使来源写 constant_buffer 也不能冒充当帧观测（#575）"
        );
        let full = IndexProjection::observed(
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            Some(0.02),
        );
        assert!(full.is_metric_resolvable());

        let with_viewport = IndexProjection::observed_with_viewport(
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            Some(0.02),
            0.0098,
            1.0,
        );
        assert!(with_viewport.is_metric_resolvable());
        let value = serde_json::to_value(with_viewport).unwrap();
        assert_eq!(value["viewport_min_depth"], 0.0098);
        assert_eq!(value["viewport_max_depth"], 1.0);

        let mut incomplete_viewport = with_viewport;
        incomplete_viewport.viewport_max_depth = None;
        assert!(!incomplete_viewport.is_metric_resolvable());

        let mut degenerate_viewport = with_viewport;
        degenerate_viewport.viewport_max_depth = Some(0.0098);
        assert!(!degenerate_viewport.is_metric_resolvable());

        let mut degenerate = full;
        degenerate.proj_b = Some(0.0);
        assert!(!degenerate.is_metric_resolvable(), "b=0 是退化投影");

        let mut negative_scale = full;
        negative_scale.metric_scale = Some(-1.0);
        assert!(
            !negative_scale.is_metric_resolvable(),
            "负 metric_scale 不能冒充米制标定"
        );

        let unknown_source = IndexProjection::known(
            crate::depth::Projection::reversed_z_infinite(0.1, 1.0),
            Some(1.0),
            IndexProjectionSource::Unknown,
        );
        assert!(
            !unknown_source.is_metric_resolvable(),
            "没有参数来源的数字不能冒充已标定"
        );
    }

    #[test]
    fn index_entry_serialises_with_the_owning_seq() {
        let v = serde_json::to_value(sample_entry()).unwrap();
        assert_eq!(v["present_seq"], 12344);
        assert_eq!(v["readback_latency_frames"], 2);
    }

    #[test]
    fn a_v6_row_requires_valid_dsv_pass_evidence() {
        let mut entry = sample_entry();
        entry.schema = INDEX_SCHEMA_V6.into();
        let error = entry.resolve_pass().unwrap_err();
        assert!(error.contains(INDEX_SCHEMA_V6), "{error}");

        entry.pass = Some(DsvPassEvidence {
            unbind_ordinal_from_end: 4,
            unbind_total: 12,
            draw_calls: 3_184,
            selection: crate::dsv::DsvSelectionAudit {
                selected_id: "dsv-0001".into(),
                ranked_top_id: Some("dsv-0001".into()),
                score_margin: Some(42.0),
                selected_matches_top: true,
                ranking_confident: true,
                any_msaa: false,
                operator_exact_pin: false,
            },
        });
        assert_eq!(entry.resolve_pass().unwrap().unwrap().draw_calls, 3_184);

        entry.pass.as_mut().unwrap().unbind_ordinal_from_end = 13;
        assert!(entry.resolve_pass().unwrap_err().contains("unbind_total"));
    }

    /// `buffer_format` 描述**来源** DSV，`storage.encoding` 描述**盘上**的字节。
    /// 两者曾经是同一个 `format` 字段，合并回去就再也分不出「d32 的缓冲被存成了
    /// fp16」这件事——而那正是 85 号改格式之后每一行的实情。
    #[test]
    fn source_format_and_storage_encoding_are_separate_fields() {
        let v = serde_json::to_value(sample_entry()).unwrap();
        assert_eq!(v["buffer_format"], "d24s8");
        assert_eq!(v["storage"]["encoding"], "exr-half-zip");
        assert_eq!(v["storage"]["domain"], "raw_buffer");
        assert_eq!(v["storage"]["channel"], crate::EXR_CHANNEL_NAME);
        assert!(v.get("format").is_none(), "旧的 format 键不该同时存在");
    }

    /// 交付命名是相对每条 clip 从 0 重编号的；探针只有全局 present_seq。
    /// 空着比编一个像交付名的字符串安全，所以它必须是可省略的。
    #[test]
    fn depth_name_is_omitted_rather_than_invented() {
        let v = serde_json::to_value(sample_entry()).unwrap();
        assert!(v.get("depth_name").is_none());
    }

    #[test]
    fn rows_declare_their_schema() {
        let v = serde_json::to_value(sample_entry()).unwrap();
        assert_eq!(v["schema"], "depth-index/v4");
        // v3 起的行即使参数未知也必须显式写 proj_source——
        // 「这个版本不记」与「这次采集没记到」处置完全不同。
        assert!(v["projection"]["proj_source"].is_string());
        // v4 起还必须显式写落盘变换，理由同上：缺失与 identity 不可区分（94 号）。
        assert!(v["storage"]["transform"].is_string());
    }
}

// --- 采集产物的两条形态校验（2026-08-05 实采暴露）---------------------------

/// 一帧深度的「有没有内容」判定。
///
/// # 为什么需要单独一条
///
/// 2026-08-05 的 Fallout 4 实采里，**约 27% 的深度帧整帧都是 clear 值**——
/// 索引里有行、文件大小正确、`sha256` 也对，所有既有校验一路放行。
/// 只有把像素读出来才看得见。
///
/// 成因是 DSV 快照选在了「已清除、尚未绘制」的时刻（`unbind_from_end` 在某些
/// 帧结构下选错），而这类失败**不会报错**，只会安静地产出一批看起来正常的空帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameContent {
    /// 有几何。
    HasGeometry,
    /// 整帧都是同一个值——深度缓冲被清除后没有画任何东西。
    Blank { clear_value_bits: u32 },
}

/// 判一帧是不是空的。
///
/// 判据是「**整帧只有一个值**」而不是「等于某个特定的 clear 值」：
/// 传统 Z 清成 1.0、reversed-Z 清成 0.0，还有游戏用别的值，
/// 写死任何一个都会漏掉另一种。
#[must_use]
pub fn classify_frame(depth: &[f32]) -> FrameContent {
    let Some(&first) = depth.first() else {
        return FrameContent::Blank {
            clear_value_bits: 0,
        };
    };
    if depth.iter().all(|v| v.to_bits() == first.to_bits()) {
        FrameContent::Blank {
            clear_value_bits: first.to_bits(),
        }
    } else {
        FrameContent::HasGeometry
    }
}

/// 一次采集里空帧占比的判定门。
///
/// **不是 0 容忍**：加载画面、过场、菜单本来就没有主场景几何，开头结尾出现
/// 少量空帧是正常的。门设在比例上，并要求报出**分布**——散在全程说明是
/// 快照时机选错，聚在两头说明只是没进游戏。
#[derive(Debug, Clone, PartialEq)]
pub struct BlankFrameReport {
    pub total: usize,
    /// 空帧总数（含正常的游戏状态间隙）。
    pub blank: usize,
    /// 落在**长连段**里的空帧。读盘、菜单、过场、快速旅行都会让主场景 DSV
    /// 整段不被绘制——这些不是缺陷，但要报出来（它们不是可交付的采集时长）。
    pub in_gameplay_gaps: usize,
    /// **孤立或极短的空帧**。这才是快照时机选错的形态。
    pub isolated: usize,
    /// 最长的一段连续空帧（帧数）。
    pub longest_gap: usize,
    /// 孤立空帧的 `present_seq`，最多留前 N 个用于定位。
    pub isolated_seqs: Vec<u64>,
}

impl BlankFrameReport {
    /// 多长的连续空帧算「游戏状态间隙」而不是缺陷。
    ///
    /// # 这个界怎么定的
    ///
    /// 2026-08-05 的 Fallout 4 实采（652 帧抽样，1/12 系统抽样）给出了两种截然
    /// 不同的形态：
    ///
    /// | 连段长度 | 段数 | ≈ 连续实际帧 |
    /// |---|---|---|
    /// | 1–2 抽样点 | 7 | 12–24 |
    /// | 5–10 抽样点 | 4 | 60–120 |
    /// | 35 抽样点 | 1 | **≈420（约 14 秒）** |
    ///
    /// **87% 的空帧落在 ≥60 帧的长段里。** 连续 14 秒没有主场景几何不是「快照选错
    /// 时机」该有的样子——选错时机产生的是**随机散布的单帧**；14 秒是读盘、开
    /// Pip-Boy、过场这类游戏状态。
    ///
    /// 界设在 **15 帧（0.5 秒 @30fps）**：任何菜单/读盘都不止半秒，而快照时机
    /// 失败几乎不会连续半秒都失败。
    pub const MIN_GAMEPLAY_GAP_FRAMES: usize = 15;

    /// 孤立空帧的容忍上限。**这个数应当接近 0**——它代表的是「本该有几何却没有」。
    pub const MAX_ISOLATED_RATIO: f64 = 0.005;

    /// 空帧总量的上限。超过一半说明这次采集大部分时间在菜单里，
    /// 无论是不是缺陷都不够交付时长。
    pub const MAX_TOTAL_BLANK_RATIO: f64 = 0.5;

    /// 空帧总占比（含游戏状态间隙）。
    #[must_use]
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.blank as f64 / self.total as f64
        }
    }

    /// **可疑**空帧占比——判缺陷用的就是这个。
    #[must_use]
    pub fn isolated_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.isolated as f64 / self.total as f64
        }
    }

    #[must_use]
    pub fn passes(&self) -> bool {
        self.isolated_ratio() <= Self::MAX_ISOLATED_RATIO
            && self.ratio() <= Self::MAX_TOTAL_BLANK_RATIO
    }

    /// 给人看的一句话。**要分开说「间隙」和「缺陷」**——把两者混成一个百分比
    /// 会让「玩家开了菜单」和「探针选错快照」看起来是同一件事。
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} 帧中空帧 {}（{:.1}%）：其中 {} 帧属于 ≥{} 帧的游戏状态间隙（最长 {} 帧），\
             孤立空帧 {} 帧（{:.2}%）",
            self.total,
            self.blank,
            self.ratio() * 100.0,
            self.in_gameplay_gaps,
            Self::MIN_GAMEPLAY_GAP_FRAMES,
            self.longest_gap,
            self.isolated,
            self.isolated_ratio() * 100.0
        )
    }
}

/// 汇总一次采集的空帧情况。输入是 `(present_seq, 是否空帧)`，按 seq 升序。
///
/// **按连段长度分类，不是只数总量。** 读盘与菜单会产出成段的空帧，而快照时机
/// 失败产出的是散布的单帧——把两者算进同一个百分比，会让正常采集被判失败、
/// 或者让真实缺陷被大段间隙稀释掉。
#[must_use]
pub fn summarize_blank_frames(frames: &[(u64, bool)]) -> BlankFrameReport {
    let total = frames.len();
    let (mut in_gaps, mut isolated, mut longest) = (0usize, 0usize, 0usize);
    let mut isolated_seqs = Vec::new();

    let mut i = 0;
    while i < frames.len() {
        if !frames[i].1 {
            i += 1;
            continue;
        }
        let start = i;
        while i < frames.len() && frames[i].1 {
            i += 1;
        }
        let len = i - start;
        longest = longest.max(len);
        if len >= BlankFrameReport::MIN_GAMEPLAY_GAP_FRAMES {
            in_gaps += len;
        } else {
            isolated += len;
            for (seq, _) in &frames[start..i] {
                if isolated_seqs.len() < 20 {
                    isolated_seqs.push(*seq);
                }
            }
        }
    }

    BlankFrameReport {
        total,
        blank: in_gaps + isolated,
        in_gameplay_gaps: in_gaps,
        isolated,
        longest_gap: longest,
        isolated_seqs,
    }
}

/// 索引 schema 是否够交付用。
///
/// # 为什么这要挡在采集这一侧
///
/// 2026-08-05 的实采用了 PR #469 之前的构建：索引是 v1、帧是裸 f32。
/// 那批数据**无法判断值域语义**（是原始缓冲值还是米制），因此无论后续怎么处理
/// 都交不出去；而这一点直到离线核查才被发现，5 GiB 已经传完。
///
/// v1 不是「旧但能用」，是「缺了区分交付与否所必需的字段」。
///
/// v2 及以上都收：每一版都是在前一版之上**加**字段，没有删过，
/// 所以旧行的语义在新版读法下仍然成立（v2/v3 缺 `transform` ⇒ identity，
/// 见 [`DepthIndexEntry::resolve_transform`]）。这里挡的只有 v1 与不认识的版本。
pub fn index_schema_is_deliverable(schema: Option<&str>) -> Result<(), String> {
    match schema {
        Some(
            INDEX_SCHEMA_V2 | INDEX_SCHEMA_V3 | INDEX_SCHEMA_V4 | INDEX_SCHEMA_V5 | INDEX_SCHEMA_V6,
        ) => Ok(()),
        Some(other) => Err(format!(
            "索引 schema 是 `{other}`，本工具只认 \
             `{INDEX_SCHEMA_V2}`/`{INDEX_SCHEMA_V3}`/`{INDEX_SCHEMA_V4}`/`{INDEX_SCHEMA_V5}`/`{INDEX_SCHEMA_V6}`"
        )),
        None => Err(format!(
            "索引没有 schema 字段——这是 v1（裸 f32、无 storage/domain）。\
             v1 分不出值域是原始缓冲值还是米制，交付判定无从做起。\
             请换用含 `{INDEX_SCHEMA_V6}` 的构建重采"
        )),
    }
}

/// 从一帧深度反推 Z 约定。
///
/// # 为什么这条必须能自动判
///
/// `z_convention` 今天是配置里人填的，**没有任何东西验证它与数据是否一致**。
/// 填错的后果不是报错，是：
///
/// - 降采样的「取最近表面」取反，边缘处静默取到远表面；
/// - 线性化得到的深度整体反过来——近处变远、远处变近，**画面看起来仍然完全正常**。
///
/// 判据是**清除值**：深度缓冲每帧都会被清成远平面，所以「恰好等于 0.0 或 1.0 的
/// 像素占了一大块」的那一端就是远端。传统 Z 远端是 1.0，reversed-Z 远端是 0.0。
///
/// 两份真实采集印证：2026-08-04 的 ETS2 有 9.24% 的像素恰好是 0.0（reversed），
/// 2026-08-05 的 Fallout 4 有 20.6% 恰好是 1.0（traditional）。
#[must_use]
pub fn infer_z_convention(depth: &[f32]) -> Option<crate::decode::ZConvention> {
    if depth.is_empty() {
        return None;
    }
    let mut at_zero = 0usize;
    let mut at_one = 0usize;
    for &v in depth {
        if v == 0.0 {
            at_zero += 1;
        } else if v == 1.0 {
            at_one += 1;
        }
    }
    // 门限取 1%：清除值必然占一大片（天空/未绘制区），而正常几何恰好落在
    // 端点上的概率极低。两端都不够多说明这一帧没有可判的清除区——
    // **返回 None 而不是猜一个**，猜错比不知道更糟。
    let threshold = depth.len() / 100;
    match (at_zero > threshold, at_one > threshold) {
        (true, false) => Some(crate::decode::ZConvention::Reversed),
        (false, true) => Some(crate::decode::ZConvention::Traditional),
        // 两端都有大片：多半是空帧（整帧一个值）或异常，判不了
        _ => None,
    }
}

/// 对多帧投票得出 Z 约定。
///
/// **单帧不够。** 室内近景、贴脸的墙、全屏 UI 都会让整帧几乎没有远平面像素，
/// 此时 [`infer_z_convention`] 正确地返回 `None`——2026-08-05 的 Fallout 4 采集里
/// 就有这样一帧（只有 0.35% 落在 1.0）。
///
/// 要求：至少 [`MIN_CONCLUSIVE_FRAMES`] 帧判得出来，且**全部一致**。
/// 出现分歧直接返回 `None`——同一次采集里两种约定并存是不可能的，
/// 真出现了说明有别的东西错了（选错 DSV、格式解错），此时给出任何一个答案都是错的。
#[must_use]
pub fn infer_z_convention_over_frames<'a>(
    frames: impl IntoIterator<Item = &'a [f32]>,
) -> Option<crate::decode::ZConvention> {
    let mut decided: Option<crate::decode::ZConvention> = None;
    let mut count = 0usize;
    for frame in frames {
        let Some(c) = infer_z_convention(frame) else {
            continue;
        };
        match decided {
            None => decided = Some(c),
            Some(prev) if prev == c => {}
            // 分歧
            Some(_) => return None,
        }
        count += 1;
    }
    if count >= MIN_CONCLUSIVE_FRAMES {
        decided
    } else {
        None
    }
}

/// 投票判定所需的最少「判得出」帧数。
pub const MIN_CONCLUSIVE_FRAMES: usize = 3;

/// 配置声明的 Z 约定与数据是否一致。
///
/// `None` 表示这一帧判不出来（比如空帧），**不算失败**——调用方应当换一帧再问，
/// 而不是据此放行或拒绝。
#[must_use]
pub fn z_convention_matches(declared: crate::decode::ZConvention, depth: &[f32]) -> Option<bool> {
    infer_z_convention(depth).map(|observed| observed == declared)
}
