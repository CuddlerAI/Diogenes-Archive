//! 把 depth 采集接进 DataMiner 客户端所需的**全部判断**。
//!
//! 客户端侧（`apps/client`）是 Windows-only 且在开发机上无法交叉编译
//! （`zstd-sys` 需要 MSVC 的 C 交叉工具链），所以那边只留最薄的胶水：
//! 读设置 → 调 [`evaluate_gates`] → 按 [`ProbeEnv`] 起进程 → session 结束时停。
//!
//! **所有会导致「不该采却采了」或「该拦没拦」的判断都在这里，并且都有测试。**
//! 这是刻意的：客户端那 5000 行 orchestrator 里加一个没测过的分支，
//! 出问题时既难复现也难归因。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 采集员在设置页看到的开关状态。
///
/// **默认关，且每次启动重置为关**（77 号 §6.3）——
/// depth 采集会注入游戏进程、可能违反 EULA、并带来账号风险，
/// 不该有「上次开着这次就自动开着」这种状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DepthToggle {
    /// 本次运行内采集员是否显式打开过。进程重启即回到 `false`。
    pub enabled_this_run: bool,
    /// 是否已经看过并确认了风险清单（§6.3 的逐条确认框）。
    pub risk_acknowledged: bool,
}

/// 评估一次门控所需的全部事实。由客户端从各处收集后一次性传进来。
#[derive(Debug, Clone, PartialEq)]
pub struct GateFacts {
    /// G1：本次构建是否编入了 depth-lab 能力（cargo feature）。
    ///
    /// # ⚠️ 这一条**不再是**授权边界（2026-08-09 起）
    ///
    /// 它曾经是。2026-08-05 移除 G3「设备持有 experiment grant」之后，「谁能采」
    /// 一度由**分发**回答——depth 是独立安装包口味、手工发放，装到哪台机器上就是
    /// 授权了哪台，而正式安装包里这一条恒为 `false`。
    ///
    /// [docs/109](../../../docs/109-formal-collection-ignition-readiness-review.md) §10 的
    /// 裁决 R-1（采纳「乙」）推翻了那个前提：**All Games DEV 正式包现在显式打开这个
    /// feature**，于是它在采集员机器上恒为 `true`，不再区分任何人。
    ///
    /// 接手授权判定的是本结构里另外两条，**它们都是数据驱动、默认关闭**：
    ///
    /// - [`GateFacts::game_whitelisted`] —— 要有逐游戏的 depth profile 标定；
    /// - [`GateFacts::fps_gate_certified`] —— 实际 DLL 必须在签名中央 allow-list 中，且当前
    ///   后端 generation、profile/binding、运行配置和显式 GPU 型号覆盖全部命中。
    ///
    /// 也就是说：二进制人人都有，但**没做过标定/实测的机器上开关仍然是灰的**。
    /// 这是 R-1 可以接受的实质原因，不是「门被拆了」。
    ///
    /// # 代价：吊销能力比原先更差
    ///
    /// 原先撤销 depth = 让那个人卸载实验包，正式版不受影响。现在**没有对应动作**：
    /// 要么发一个关掉 feature 的新版本（渠道级回退、影响所有人），要么无。
    ///
    /// 将来需要真正的吊销能力时，应当把 G3 加回来，**或**把 depth profile 与性能认证
    /// 改成服务端下发（撤一条记录 = 关掉一台机器），而不是在别处打补丁。
    pub feature_compiled_in: bool,
    /// G2：当前绑定游戏的 id 是否在 depth-lab 白名单内。
    pub game_whitelisted: bool,
    /// G2：`executable_sha256` 是否与 depth-profile 钉的一致。
    pub executable_sha256_matches: bool,
    /// G2：目标进程位数与已构建的 probe DLL 位数是否一致。
    ///
    /// BioShock Infinite 是 32 位（#425 实测），位数不匹配的 DLL 根本注不进去。
    pub probe_bitness_matches: bool,
    /// G4：当前 capture profile 的 fps 档与 depth-profile 声明的车道是否一致。
    pub lane_matches: bool,
    /// G5：采集盘剩余字节。
    pub free_disk_bytes: u64,
    /// G5：本次 session 预计时长（秒）。
    pub planned_seconds: u64,
    /// 每小时预计写入字节。**用 [`depth_bytes_per_hour`] 算，别在调用侧写字面量**——
    /// 它随落盘格式变，写死过一次就再也不会跟着改。
    pub bytes_per_hour: u64,
    /// **性能门是否已在本机型上验收通过**（77 号 §0.7）。
    ///
    /// 由 [`crate::certification::evaluate_perf_certification`] 从签名中央清单算出，
    /// **不是代码里写死的布尔值**。性能认证按 D3D11/D3D12 generation 独立失效；DLL SHA
    /// 负责完整性和审计，驱动/LUID 只保留在候选证据中。
    ///
    /// 未验收时不得让采集员开启——hook 卡顿会被 WGC 录进同一份数据的 RGB 那一半，
    /// 丢 depth 只丢一半，卡顿两半全废（风险登记 R-1）。
    pub fps_gate_certified: bool,
    /// 仅用于为当前完整 certification key 采集性能基线；该 run 必须被客户端持久标记为
    /// non-deliverable，且不得生成或上传 depth archive。它不是认证结果，也不能写回记录。
    pub performance_validation_only: bool,
    /// 用于匹配认证记录的机型串（已归一化）。**只用于诊断**——
    /// 判定本身在 `fps_gate_certified` 里已经做完。
    ///
    /// 带着它是因为这道门最常见的失败不是「真的没测」，而是记录里的字符串
    /// 与本机报出来的对不上，而那种失败原本完全不可见。
    pub gpu_model: String,
    pub probe_version: String,
}

/// 门控失败的原因。**每一条都要能直接显示给采集员看懂**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "code", content = "detail")]
pub enum GateDenial {
    /// 本次构建不含 depth 能力（正式安装包的正常状态）。
    NotCompiledIn,
    ToggleOff,
    RiskNotAcknowledged,
    GameNotWhitelisted,
    ExecutableMismatch,
    ProbeBitnessMismatch,
    LaneMismatch,
    /// 磁盘不够，附「需要 / 剩余」字节。
    NotEnoughDisk {
        needed: u64,
        free: u64,
    },
    /// 本机型的 FPS 门尚未验收。
    ///
    /// 带上**实际用于匹配的机型串**：这道门最常见的失败不是「真的没测」，
    /// 而是记录里的字符串与本机报出来的对不上，而那种失败原本完全不可见。
    FpsGateNotCertified {
        /// 本机归一化后的 GPU 型号。审核硬件覆盖名单时照抄这个串。
        gpu_model: String,
        probe_version: String,
    },
}

impl GateDenial {
    /// 给**工程侧**看的补充信息，与 [`Self::annotator_message`] 分开。
    ///
    /// 采集员文案有一条硬规则：不出现内部术语（有测试盯着）。但
    /// [`Self::FpsGateNotCertified`] 最常见的失败根本不是「真的没测」，
    /// 而是认证记录里的字符串与本机报出来的对不上——**那种失败不给出串就查不动**。
    ///
    /// 所以诊断走这一路：日志、诊断包、`--verbose` 里出，UI 上不出。
    #[must_use]
    pub fn diagnostic(&self) -> Option<String> {
        match self {
            Self::FpsGateNotCertified {
                gpu_model,
                probe_version,
            } => Some(format!(
                "匹配签名中央认证清单用的机型串是「{gpu_model}」，probe runtime 版本 \
                 {probe_version}。审核 coveredGpuModels 时须与这个串逐字一致；未知机型保持关闭。"
            )),
            _ => None,
        }
    }

    /// 给采集员看的一句话。**不出现「G3」「R-1」这类内部编号。**
    #[must_use]
    pub fn annotator_message(&self) -> String {
        match self {
            Self::NotCompiledIn => "此版本不含深度采集功能。".into(),
            Self::ToggleOff => "深度采集开关未打开。".into(),
            Self::RiskNotAcknowledged => "请先阅读并确认深度采集的风险说明。".into(),
            Self::GameNotWhitelisted => "当前游戏不在深度采集的许可名单内。".into(),
            Self::ExecutableMismatch => {
                "游戏版本与已校准的版本不一致（可能刚更新过）。深度采集需要重新校准后才能使用。"
                    .into()
            }
            Self::ProbeBitnessMismatch => "深度采集组件与该游戏的位数不匹配，请联系工程。".into(),
            Self::LaneMismatch => "当前录制帧率档与深度采集配置不一致。".into(),
            Self::NotEnoughDisk { needed, free } => format!(
                "磁盘空间不足：本次预计需要 {:.1} GB，当前剩余 {:.1} GB。",
                *needed as f64 / 1e9,
                *free as f64 / 1e9
            ),
            Self::FpsGateNotCertified { .. } => {
                "本机型的深度采集性能验收尚未完成，暂不能开启。".into()
            }
        }
    }
}

/// 磁盘预留系数：预算之外再留 20%，免得刚好写满把录制也拖垮。
pub const DISK_HEADROOM: f64 = 1.2;

/// 性能验收门限（77 号 §0.7）。平均值沿用 [01 §8](../../../docs/01-recording-pipeline.md)。
pub const MAX_AVG_FPS_DROP: f64 = 0.02;
/// 1% low 的门限比平均值松——低百分位只由最慢的那几帧决定，同场景重复测量自身就有波动。
/// 2026-09-18 owner 裁决由 01 §8 的 3% 放宽到 5%；对所有游戏生效，不是单个游戏的特判。
pub const MAX_LOW1_FPS_DROP: f64 = 0.05;
/// 反向改善超过正常门限的两倍，说明四档场景/边界抖动已经淹没被测开销。
pub const MAX_AVG_FPS_GAIN: f64 = MAX_AVG_FPS_DROP * 2.0;
pub const MAX_LOW1_FPS_GAIN: f64 = MAX_LOW1_FPS_DROP * 2.0;
/// 评估五重门。返回 `Ok(())` 才允许启动 depth 采集。
///
/// **顺序是刻意的**：先报「结构性不可能」（没编进来、游戏不对），
/// 再报「采集员能自己解决」（开关、确认、磁盘）。
/// 反过来会让采集员在一个根本不支持的版本上去清磁盘。
pub fn evaluate_gates(toggle: DepthToggle, facts: &GateFacts) -> Result<(), GateDenial> {
    if !facts.feature_compiled_in {
        return Err(GateDenial::NotCompiledIn);
    }
    if !facts.game_whitelisted {
        return Err(GateDenial::GameNotWhitelisted);
    }
    if !facts.executable_sha256_matches {
        return Err(GateDenial::ExecutableMismatch);
    }
    if !facts.probe_bitness_matches {
        return Err(GateDenial::ProbeBitnessMismatch);
    }
    if !facts.lane_matches {
        return Err(GateDenial::LaneMismatch);
    }
    if !facts.fps_gate_certified && !facts.performance_validation_only {
        return Err(GateDenial::FpsGateNotCertified {
            gpu_model: facts.gpu_model.clone(),
            probe_version: facts.probe_version.clone(),
        });
    }
    if !toggle.enabled_this_run {
        return Err(GateDenial::ToggleOff);
    }
    if !toggle.risk_acknowledged {
        return Err(GateDenial::RiskNotAcknowledged);
    }

    let needed = (facts.bytes_per_hour as f64 * facts.planned_seconds as f64 / 3600.0
        * DISK_HEADROOM) as u64;
    if facts.free_disk_bytes < needed {
        return Err(GateDenial::NotEnoughDisk {
            needed,
            free: facts.free_disk_bytes,
        });
    }
    Ok(())
}

/// 一帧 720p 深度落盘后的字节数（[85 号 §1.0](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md)）。
///
/// 实测：fp16 EXR ZIP 装**逆深度**（94 号裁决）。**这个数会随场景变**——
/// 深度不连续越多压得越差，所以 [`evaluate_gates`] 还会再乘 [`DISK_HEADROOM`]。
///
/// # 当前值 274 KiB 的来源（2026-08-08 重标）
///
/// 取自**生产摄取面上唯一一件真实 depth 产物**——`clip d4568aaa` 的
/// `depth.zip`，Fallout 4 Commonwealth 120s 采集，服务端已 `verified`：
///
/// ```text
/// 503,648,296 B ÷ 1800 帧 = 279,697 B/帧 = 273.1 KiB/帧
/// ```
///
/// 扣掉 ZIP 条目开销之后的净值。**这个换算成立的前提是 variant-R 归档用
/// `CompressionMethod::Stored`**（`client_archive.rs`）——不压缩，所以包大小就是
/// 落盘大小，开销只有约 108 B/条目、合计 0.039%。哪天归档改成压缩存，这条推导
/// 立刻失效，必须换成直接量 EXR 目录。
///
/// # 为什么必须改：20% 余量已经被吃光，还倒欠 8.4%
///
/// 旧值 210 KiB 也标称来自 Fallout 4（户外一帧 209.2 KiB），但对着上面这条真实
/// 采集**低估 30.0%**。而 [`DISK_HEADROOM`] 的 20% 本是留给「场景差异」的：
///
/// ```text
/// 210 × 1.2 = 252.0 KiB/帧  <  273.1 KiB/帧（实测）   → 仍短 8.4%，且余量归零
/// ```
///
/// 也就是说磁盘门在**这一条已知能采出来的数据**上就已经不够用了，余量一点不剩。
/// 一小时 30fps 采集：门以为需要 13.0 GiB，实际至少 14.1 GiB。而这种失败的发生
/// 时刻是采集员采了一小时之后——正是原注释警告过的那一种。
///
/// 273.1 按既有惯例向上取整到 **274 KiB**（旧值也是 209.2 → 210 这么取的），
/// 让 `DISK_HEADROOM` 重新表示它字面的意思。
///
/// ⚠️ **n = 1，且只有 Fallout 4 一款。** 同一款游戏上两次测量差 30%，原因未查明
/// （可能是场景/段落不同，也可能是编码链在两次之间变过）——所以这不是「210 错了、
/// 274 对了」，而是「274 有一条可复算的实证背书，210 没有」。逐游戏实测挂在
/// #655 W-C；那之前请**只向上调**这个值。
///
/// # 历史：为什么从 100 KiB 改成 210 KiB
///
/// 更早的值量的是 ETS2 的 fp16(`d`)：99.97 KiB/帧。那个数字**小得不真实**——
/// 传统 Z 的 `d` 挤在 1 附近，fp16 之后一帧只剩 185 个唯一值，压得好是因为
/// 数据被量化没了。改存逆深度之后信息真的变多，体积随之翻倍，
/// 这是恢复精度的代价，不是编码变差。
///
/// 改落盘格式就必须改这里：v1 的裸 f32 是 3600 KiB/帧。
pub const DEPTH_BYTES_PER_FRAME: u64 = 274 * 1024;

/// 每小时预计写入字节。`depth_sample_fps` 必须来自选中 profile 的
/// [`crate::profile::DepthProfile::depth_hz`]，这样旧定频 profile 与 30Hz 全采 profile
/// 都使用各自真实口径。
#[must_use]
pub const fn depth_bytes_per_hour(depth_sample_fps: u64) -> u64 {
    DEPTH_BYTES_PER_FRAME * depth_sample_fps * 3600
}

/// 注入 probe 时要设的环境变量。
///
/// probe 的全部行为都由环境变量驱动（这是 #437 定下的接口），
/// 所以客户端不需要和 probe 有任何 IPC——**起进程时把 env 摆对就行**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeEnv(pub BTreeMap<String, String>);

/// 构造 probe 环境。
///
/// `output_dir` 应当落在**本次 session 目录之下**，这样 depth 产物与五件套同源可追溯，
/// 而不是散在 `%TEMP%` 里（#437 的默认值是临时目录，接进产品后必须改掉）。
#[must_use]
pub fn probe_env(output_dir: &str, unbind_from_end: usize, max_frames: Option<u64>) -> ProbeEnv {
    let mut m = BTreeMap::new();
    // #437 目前用 DM_DEPTH_PROBE_DSV_AUDIT 兼作总开关。名字说的是「审计」、
    // 干的是「开启采集」——接进产品前应改名为 DM_DEPTH_PROBE_ENABLE。
    // 这里两个都设，等 probe 侧改名后删掉旧的即可，中间不会有断档。
    m.insert("DM_DEPTH_PROBE_ENABLE".into(), "1".into());
    m.insert("DM_DEPTH_PROBE_DSV_AUDIT".into(), "1".into());
    m.insert("DM_DEPTH_PROBE_OUTPUT_DIR".into(), output_dir.into());
    m.insert(
        "DM_DEPTH_PROBE_UNBIND_FROM_END".into(),
        unbind_from_end.to_string(),
    );
    if let Some(n) = max_frames {
        m.insert("DM_DEPTH_PROBE_MAX_FRAMES".into(), n.to_string());
    }
    ProbeEnv(m)
}

/// 写进 session `meta.json` 的 `depth_lab` 块。
///
/// **`non_trainable` 恒为 `true`**：注入采集的数据来源合法性与现行同意书均不覆盖它
/// （77 号 §1.3）。这个字段不接受外部传入，就是为了让它不可能被写成别的值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DepthLabMeta {
    pub enabled: bool,
    pub lane: u32,
    pub probe_version: String,
    pub dsv_profile_id: String,
    pub unbind_from_end: usize,
    pub non_trainable: bool,
    pub captured_frames: u64,
    pub pool_full_drops: u64,
    pub timed_out_drops: u64,
}

impl DepthLabMeta {
    #[must_use]
    pub fn new(
        lane: u32,
        probe_version: impl Into<String>,
        dsv_profile_id: impl Into<String>,
        unbind_from_end: usize,
    ) -> Self {
        Self {
            enabled: true,
            lane,
            probe_version: probe_version.into(),
            dsv_profile_id: dsv_profile_id.into(),
            unbind_from_end,
            // 不接受传入 —— 见类型文档
            non_trainable: true,
            captured_frames: 0,
            pool_full_drops: 0,
            timed_out_drops: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_facts() -> GateFacts {
        GateFacts {
            feature_compiled_in: true,
            game_whitelisted: true,
            executable_sha256_matches: true,
            probe_bitness_matches: true,
            lane_matches: true,
            free_disk_bytes: 500_000_000_000,
            planned_seconds: 3600,
            bytes_per_hour: depth_bytes_per_hour(30),
            fps_gate_certified: true,
            performance_validation_only: false,
            gpu_model: "NVIDIA GeForce RTX 4060".into(),
            probe_version: "0.7.1".into(),
        }
    }

    fn on() -> DepthToggle {
        DepthToggle {
            enabled_this_run: true,
            risk_acknowledged: true,
        }
    }

    #[test]
    fn a_build_without_the_feature_can_never_enable_depth() {
        // 没编进来的构建，任何设置都打不开。
        //
        // ⚠️ 这条测试**原名** `a_release_build_can_never_enable_depth`，注释写的是
        // 「正式安装包不含该能力」。docs/109 §10 裁决 R-1 之后那句话不再成立——
        // All Games DEV 以及在线 Delivered/production 口味现在都显式打开这个
        // feature。断言的行为没变，变的是它**不再等于**「正式包一定拒绝」。
        // 改名以免它继续宣称一件假事。
        let mut f = ok_facts();
        f.feature_compiled_in = false;
        assert_eq!(
            evaluate_gates(on(), &f),
            Err(GateDenial::NotCompiledIn),
            "未编入该 feature 的构建必须无条件拒绝"
        );
    }

    /// R-1 之后，授权边界落在**两道数据驱动的门**上，而不是构建口味。
    ///
    /// 这条测试就是那句话的可执行版本：**feature 编进来了、开关开了、风险也确认了，
    /// 只要没做过逐游戏标定或逐机型性能认证，仍然打不开。** 它是 R-1「正式包带开关
    /// 但开关默认是灰的」这一论证的落点——论证若不成立，这里会红。
    #[test]
    fn with_the_feature_shipped_the_data_driven_gates_are_the_boundary() {
        // 未做逐游戏 depth profile 标定
        let mut ungated_game = ok_facts();
        ungated_game.feature_compiled_in = true;
        ungated_game.game_whitelisted = false;
        assert_eq!(
            evaluate_gates(on(), &ungated_game),
            Err(GateDenial::GameNotWhitelisted),
            "feature 进了正式包，也不该让未标定的游戏采 depth"
        );

        // 未做本机型 + 本 probe 版本的性能认证（认证记录为空时的常态）
        let mut uncertified = ok_facts();
        uncertified.feature_compiled_in = true;
        uncertified.fps_gate_certified = false;
        assert!(
            matches!(
                evaluate_gates(on(), &uncertified),
                Err(GateDenial::FpsGateNotCertified { .. })
            ),
            "feature 进了正式包，也不该让未做性能验收的机型采 depth"
        );
    }

    #[test]
    fn the_toggle_defaults_to_off() {
        // §6.3：默认关，且每次启动重置
        let t = DepthToggle::default();
        assert!(!t.enabled_this_run);
        assert!(!t.risk_acknowledged);
        assert_eq!(evaluate_gates(t, &ok_facts()), Err(GateDenial::ToggleOff));
    }

    #[test]
    fn acknowledging_risk_is_required_even_when_toggled_on() {
        let t = DepthToggle {
            enabled_this_run: true,
            risk_acknowledged: false,
        };
        assert_eq!(
            evaluate_gates(t, &ok_facts()),
            Err(GateDenial::RiskNotAcknowledged)
        );
    }

    #[test]
    fn an_uncertified_fps_gate_blocks_everything() {
        // R-1：卡顿会把配对数据的两半一起毁掉，所以性能未验收就不许开
        let mut f = ok_facts();
        f.fps_gate_certified = false;
        assert!(matches!(
            evaluate_gates(on(), &f),
            Err(GateDenial::FpsGateNotCertified { .. })
        ));
    }

    #[test]
    fn validation_only_bypasses_only_the_performance_certificate_gate() {
        let mut facts = ok_facts();
        facts.fps_gate_certified = false;
        facts.performance_validation_only = true;
        assert_eq!(evaluate_gates(on(), &facts), Ok(()));

        facts.executable_sha256_matches = false;
        assert_eq!(
            evaluate_gates(on(), &facts),
            Err(GateDenial::ExecutableMismatch)
        );
    }

    #[test]
    fn the_denial_tells_the_operator_which_string_it_matched_on() {
        // 这道门最常见的失败不是「真的没测」，而是记录里的字符串与本机
        // 报出来的对不上——原本那种失败完全不可见，只说「尚未完成」。
        let mut f = ok_facts();
        f.fps_gate_certified = false;
        f.gpu_model = "NVIDIA GeForce RTX 4060 Laptop GPU".into();
        let denial = evaluate_gates(on(), &f).unwrap_err();
        // 采集员文案保持干净（有另一条测试盯着不出现内部术语）
        assert!(!denial.annotator_message().contains("RTX"));
        // 诊断这一路必须把串摆出来，否则这类失败查不动
        let diag = denial.diagnostic().expect("这条拒绝必须带诊断");
        assert!(
            diag.contains("RTX 4060 Laptop GPU"),
            "应当把机型串摆出来：{diag}"
        );
        assert!(
            diag.contains("coveredGpuModels"),
            "应当指出中央清单的人工覆盖字段：{diag}"
        );
    }

    #[test]
    fn a_game_update_blocks_capture_instead_of_producing_garbage() {
        // DSV 选择是按 build 校准的；游戏一更新启发式就可能静默选错
        let mut f = ok_facts();
        f.executable_sha256_matches = false;
        assert_eq!(
            evaluate_gates(on(), &f),
            Err(GateDenial::ExecutableMismatch)
        );
    }

    #[test]
    fn bitness_mismatch_is_caught_before_injection() {
        let mut f = ok_facts();
        f.probe_bitness_matches = false;
        assert_eq!(
            evaluate_gates(on(), &f),
            Err(GateDenial::ProbeBitnessMismatch)
        );
    }

    #[test]
    fn disk_budget_includes_headroom() {
        let budget = (depth_bytes_per_hour(30) as f64 * DISK_HEADROOM) as u64;
        let mut f = ok_facts(); // planned_seconds = 3600
        f.free_disk_bytes = budget - 1;
        match evaluate_gates(on(), &f) {
            Err(GateDenial::NotEnoughDisk { needed, free }) => {
                assert_eq!(needed, budget);
                assert_eq!(free, budget - 1);
            }
            other => panic!("应当因磁盘不足被拦，实得 {other:?}"),
        }
        // 刚好够就放行
        f.free_disk_bytes = budget;
        assert!(evaluate_gates(on(), &f).is_ok());
    }

    /// 磁盘门直接决定采集员能不能开始，而它依赖落盘格式。85 号把格式从裸 f32 换成
    /// fp16 EXR 之后预算小了 17 倍——**用旧数会把完全采得动的 session 拒掉**，
    /// 而那种拒绝在采集员那边看起来就是「功能坏了」。
    ///
    /// 反过来也一样致命：94 号改存逆深度之后一帧从 100 KiB 涨到 210 KiB，
    /// **旧数低估两倍**，会放行一份采到一半就写满盘的 session——
    /// 而那种失败发生在采集员采了一小时之后。这条测试把量级钉住。
    #[test]
    fn the_hourly_budget_tracks_the_current_on_disk_format() {
        // 两条 lane 各有一条闸，都要守：
        //   · 默认 lane（15 Hz）——94 号裁决付出的那个代价，2 小时 ≈ 28 GiB；
        //   · selected RGB source 上限（30 Hz）——117 号链路的保守落盘门，5 分钟 ≈ 2.4 GiB。
        // 用同一个 274 KiB/帧（#656 按生产实测重标，此前是 210 KiB 的规划值）。

        // 274 KiB/帧 × 15 fps × 3600 s ≈ 15.2 GB/h
        assert_eq!(depth_bytes_per_hour(15), 274 * 1024 * 15 * 3600);
        let gb_per_hour = depth_bytes_per_hour(15) as f64 / 1e9;
        assert!(
            (14.0..16.0).contains(&gb_per_hour),
            "每小时 {gb_per_hour:.1} GB 不在 fp16 EXR 逆深度的量级上——落盘格式换了却没改这里？"
        );
        // 2 小时样本必须落在 27-29 GiB，这是 94 号裁决付出的那个代价
        let two_hours_gib = depth_bytes_per_hour(15) as f64 * 2.0 / (1u64 << 30) as f64;
        assert!(
            (27.0..29.0).contains(&two_hours_gib),
            "2h 预计 {two_hours_gib:.1} GiB"
        );

        // selected source 保守上限：274 KiB/帧 × 30 fps × 3600 s ≈ 30.3 GB/h。
        // **这条不能省成「15Hz 的两倍」**——它守的是 `depth_bytes_per_hour` 真的按
        // 传入 fps 算，而不是内部写死某个常量；全采模式静默退回 15Hz 预算就会
        // 低估一半，正好是磁盘门最不该出错的方向。这里的 30 是 CFR 唯一来源上限，
        // 不是要求游戏 Present 率约等于 30Hz。
        assert_eq!(depth_bytes_per_hour(30), 274 * 1024 * 30 * 3600);
        assert_eq!(depth_bytes_per_hour(30), depth_bytes_per_hour(15) * 2);
        // 5 分钟全采约 2.35 GiB；这正是 102 号裁决的磁盘门口径
        // （#656 把规划值从 210 重标到 274 之后，该口径由 1.8 GiB 变成 2.35 GiB）。
        let five_minutes_gib = depth_bytes_per_hour(30) as f64 / 12.0 / (1u64 << 30) as f64;
        assert!(
            (2.2..2.5).contains(&five_minutes_gib),
            "5min 预计 {five_minutes_gib:.1} GiB"
        );
    }

    /// 规划值必须覆盖**已经采出来过**的那一帧，余量才是真余量。
    ///
    /// 这条钉的不是某个具体数字，而是一条不变式：**`DEPTH_BYTES_PER_FRAME` 本身
    /// 就要 ≥ 实测**，不能靠 [`DISK_HEADROOM`] 去补规划值的窟窿。
    ///
    /// 旧值 210 KiB 正好违反它：对着生产上唯一一件真实 depth 产物
    /// （`clip d4568aaa`，FO4 Commonwealth 120s，503,648,296 B ÷ 1800 帧 = 273.1 KiB）
    /// 低估 30%，而 20% 的余量连这个缺口都填不平——`210 × 1.2 = 252 < 273.1`，
    /// 仍短 8.4% 且余量归零。余量本该留给场景差异，不是留给规划值算错。
    #[test]
    fn the_planning_value_covers_the_only_real_capture_we_have() {
        // 生产实测：clip d4568aaa 的 depth.zip（variant-R，CompressionMethod::Stored，
        // 所以包大小≈落盘大小；条目开销约 108 B × 1801 ≈ 0.039%）
        const OBSERVED_ZIP_BYTES: u64 = 503_648_296;
        const OBSERVED_DEPTH_FRAMES: u64 = 1_800;
        const ZIP_ENTRY_OVERHEAD: u64 = 108 * (OBSERVED_DEPTH_FRAMES + 1);
        let observed_per_frame = (OBSERVED_ZIP_BYTES - ZIP_ENTRY_OVERHEAD) / OBSERVED_DEPTH_FRAMES;

        assert!(
            DEPTH_BYTES_PER_FRAME >= observed_per_frame,
            "规划值 {} B/帧 < 实测 {observed_per_frame} B/帧：磁盘门会放行一份写得满盘的 session，\
             而那种失败发生在采集员采了一小时之后。往上调 DEPTH_BYTES_PER_FRAME，别指望 DISK_HEADROOM 兜。",
            DEPTH_BYTES_PER_FRAME
        );

        // 余量必须仍然是余量：规划值盖住实测之后，headroom 还得剩下可观的一截。
        let with_headroom = DEPTH_BYTES_PER_FRAME as f64 * DISK_HEADROOM;
        assert!(
            with_headroom >= observed_per_frame as f64 * 1.15,
            "含余量后仅 {with_headroom:.0} B/帧，相对实测 {observed_per_frame} B/帧不足 15%——\
             DISK_HEADROOM 已被规划值误差吃掉，场景差异就没有保护了"
        );
    }

    #[test]
    fn structural_denials_come_before_things_the_annotator_can_fix() {
        // 顺序有意义：别让采集员在一个根本不支持的版本上去清磁盘
        let mut f = ok_facts();
        f.feature_compiled_in = false;
        f.free_disk_bytes = 0;
        assert_eq!(evaluate_gates(on(), &f), Err(GateDenial::NotCompiledIn));
    }

    #[test]
    fn every_denial_has_a_message_without_internal_jargon() {
        let all = [
            GateDenial::NotCompiledIn,
            GateDenial::ToggleOff,
            GateDenial::RiskNotAcknowledged,
            GateDenial::GameNotWhitelisted,
            GateDenial::ExecutableMismatch,
            GateDenial::ProbeBitnessMismatch,
            GateDenial::LaneMismatch,
            GateDenial::NotEnoughDisk { needed: 1, free: 0 },
            GateDenial::FpsGateNotCertified {
                gpu_model: "NVIDIA GeForce RTX 4060".into(),
                probe_version: "0.7.1".into(),
            },
        ];
        for d in all {
            let m = d.annotator_message();
            assert!(!m.is_empty());
            for jargon in ["G1", "G2", "G3", "G4", "G5", "R-1", "DSV", "probe"] {
                assert!(
                    !m.contains(jargon),
                    "给采集员的文案不该出现内部术语 `{jargon}`：{m}"
                );
            }
        }
    }

    #[test]
    fn probe_env_points_output_into_the_session_dir() {
        // #437 默认落 %TEMP%；接进产品必须改到 session 目录下，产物才可追溯
        let e = probe_env("D:\\dm\\sessions\\s1\\depth-lab\\lane-30", 4, None);
        assert_eq!(
            e.0.get("DM_DEPTH_PROBE_OUTPUT_DIR").unwrap(),
            "D:\\dm\\sessions\\s1\\depth-lab\\lane-30"
        );
        assert_eq!(e.0.get("DM_DEPTH_PROBE_UNBIND_FROM_END").unwrap(), "4");
        assert!(!e.0.contains_key("DM_DEPTH_PROBE_MAX_FRAMES"));
    }

    #[test]
    fn probe_env_sets_both_the_new_and_legacy_switch() {
        // probe 侧改名期间两个都设，避免出现一版都不认的断档
        let e = probe_env("x", 4, Some(60));
        assert_eq!(e.0.get("DM_DEPTH_PROBE_ENABLE").unwrap(), "1");
        assert_eq!(e.0.get("DM_DEPTH_PROBE_DSV_AUDIT").unwrap(), "1");
        assert_eq!(e.0.get("DM_DEPTH_PROBE_MAX_FRAMES").unwrap(), "60");
    }

    #[test]
    fn meta_forces_non_trainable() {
        // 构造函数不接受这个字段，就是为了让它不可能被写成 false
        let m = DepthLabMeta::new(30, "0.7.0", "bioshock-main-v1", 4);
        assert!(m.non_trainable);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["non_trainable"], true);
        assert_eq!(v["lane"], 30);
        assert_eq!(v["unbind_from_end"], 4);
    }
}
