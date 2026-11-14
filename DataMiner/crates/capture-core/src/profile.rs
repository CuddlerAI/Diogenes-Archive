use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::audio_contract::{
    AUDIO_BITS_PER_SAMPLE, AUDIO_CHANNEL_MASK_STEREO, AUDIO_CHANNELS, AUDIO_COMPRESSION,
    AUDIO_SAMPLE_FORMAT, AUDIO_SAMPLE_RATE_HZ,
};

pub const DEV_PROFILE_ID: &str = "synthetic-win32-dev-v1";
pub const NON_TRAINABLE_MIN_SOURCE_WIDTH: u32 = 960;
pub const NON_TRAINABLE_MIN_SOURCE_HEIGHT: u32 = 540;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureProfile {
    pub profile_id: String,
    pub non_trainable: bool,
    pub capture_backend: String,
    pub encoder_backend: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub gop_frames: u32,
    pub segment_seconds: u32,
    pub test_segments: u32,
    pub accepted_encoder_relaxations: Option<AcceptedEncoderRelaxations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<CaptureAudioProfile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureAudioProfile {
    pub mode: AudioCaptureMode,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_format: String,
    pub channel_mask: u32,
    pub compression: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCaptureMode {
    ProcessLoopback,
}

impl CaptureAudioProfile {
    pub fn process_loopback() -> Self {
        Self {
            mode: AudioCaptureMode::ProcessLoopback,
            sample_rate_hz: AUDIO_SAMPLE_RATE_HZ,
            channels: AUDIO_CHANNELS,
            bits_per_sample: AUDIO_BITS_PER_SAMPLE,
            sample_format: AUDIO_SAMPLE_FORMAT.into(),
            channel_mask: AUDIO_CHANNEL_MASK_STEREO,
            compression: AUDIO_COMPRESSION.into(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self == &Self::process_loopback(),
            "audio profile must use process loopback with pcm_s16le/48000/stereo and zstd"
        );
        Ok(())
    }
}

/// 驱动会把编码器控制项归一化（例如 NVENC 把 max bitrate 24M 夹到 15.552M、
/// GOP 120 压到 30）。这里显式声明该 profile 已接受的归一化包络：
/// `encoder_health` 只对超出包络的偏差报警，恒为 true 的告警不携带信息。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedEncoderRelaxations {
    /// 接受的有效 max bitrate 下限（bps）。驱动 CBR 折叠后 max 可低于 mean。
    pub min_effective_max_bitrate_bps: u32,
    /// 除请求值外接受的有效 GOP 值；必须整除请求 GOP，保证 4s chunk 边界仍落在 IDR。
    pub gop_frames: Vec<u32>,
    pub allow_rate_control_normalization: bool,
    pub allow_low_latency_disabled: bool,
    pub allow_unverified_b_frames: bool,
}

/// 一个 profile 对游戏窗口客户区尺寸的要求。
///
/// 拆成枚举而不是返回一对数字，是因为旧名字 `minimum_source_dimensions` 在可训练
/// 分支下返回的**根本不是最小值，是唯一允许值**——这个名字实际误导过人。调用点现在
/// 必须显式面对"到底是下限还是精确值"。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceGeometryRule {
    /// 下限：不低于即可，其余交给缩放。
    AtLeast(u32, u32),
    /// 精确值：必须相等，不缩放。
    Exactly(u32, u32),
}

impl SourceGeometryRule {
    #[must_use]
    pub fn accepts(self, width: u32, height: u32) -> bool {
        match self {
            Self::AtLeast(w, h) => width >= w && height >= h,
            Self::Exactly(w, h) => width == w && height == h,
        }
    }

    /// 规则涉及的那对尺寸（下限或精确值）。写进 session contract 时配合
    /// `capture_policy.scale` 一起读：`scale=false` 即表示这对数是精确值。
    #[must_use]
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::AtLeast(w, h) | Self::Exactly(w, h) => (w, h),
        }
    }

    /// 给人看的一句话，用于拒绝时的诊断。
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::AtLeast(w, h) => format!("至少 {w}x{h}"),
            Self::Exactly(w, h) => format!("必须精确等于 {w}x{h}"),
        }
    }
}

impl CaptureProfile {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read capture profile {}", path.display()))?;
        Self::from_slice(&bytes)
            .with_context(|| format!("invalid capture profile {}", path.display()))
    }

    pub fn from_slice(bytes: &[u8]) -> anyhow::Result<Self> {
        let profile: Self =
            serde_json::from_slice(bytes).context("invalid capture profile JSON")?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.capture_backend == "native_wgc_window_v1",
            "unsupported capture backend"
        );
        ensure!(
            self.encoder_backend == "mf_nvidia_hardware_v1",
            "unsupported encoder backend"
        );
        ensure!(
            self.width >= 1920 && self.height >= 1080,
            "profile is below the 1920x1080 floor"
        );
        ensure!(
            self.width.is_multiple_of(2) && self.height.is_multiple_of(2),
            "NV12 dimensions must be even"
        );
        ensure!(
            self.fps_num > 0 && self.fps_den > 0,
            "frame rate must be positive"
        );
        ensure!(
            u64::from(self.fps_num) >= 30 * u64::from(self.fps_den),
            "profile is below the 30 fps floor"
        );
        ensure!(
            self.bitrate_bps > 0 && self.max_bitrate_bps >= self.bitrate_bps,
            "invalid bitrate range"
        );
        ensure!(
            self.gop_frames == self.required_gop_frames(),
            "gop_frames must equal round(fps * 4s): expected {}, got {}",
            self.required_gop_frames(),
            self.gop_frames
        );
        ensure!(
            self.segment_seconds > 0 && self.test_segments > 0,
            "segment test values must be positive"
        );
        if let Some(accepted) = &self.accepted_encoder_relaxations {
            ensure!(
                accepted.min_effective_max_bitrate_bps > 0
                    && accepted.min_effective_max_bitrate_bps <= self.max_bitrate_bps,
                "accepted effective max bitrate floor must be positive and not exceed the requested max"
            );
            ensure!(
                !accepted.gop_frames.is_empty(),
                "accepted effective GOP list must not be empty"
            );
            for gop in &accepted.gop_frames {
                ensure!(
                    *gop > 0 && self.gop_frames.is_multiple_of(*gop),
                    "accepted effective GOP {gop} must divide the requested GOP {} so chunk boundaries stay on IDR",
                    self.gop_frames
                );
            }
        }
        if let Some(audio) = &self.audio {
            audio.validate()?;
        }
        Ok(())
    }

    /// 有效 max bitrate 是否落在已接受包络内（等于请求值，或不低于声明的下限）。
    #[must_use]
    pub fn accepts_effective_max_bitrate(&self, effective_max_bitrate_bps: u32) -> bool {
        effective_max_bitrate_bps >= self.max_bitrate_bps
            || self
                .accepted_encoder_relaxations
                .as_ref()
                .is_some_and(|accepted| {
                    effective_max_bitrate_bps >= accepted.min_effective_max_bitrate_bps
                })
    }

    /// 有效 GOP 是否落在已接受包络内（等于请求值，或在声明的接受列表里）。
    #[must_use]
    pub fn accepts_effective_gop(&self, effective_gop_frames: u32) -> bool {
        effective_gop_frames == self.gop_frames
            || self
                .accepted_encoder_relaxations
                .as_ref()
                .is_some_and(|accepted| accepted.gop_frames.contains(&effective_gop_frames))
    }

    /// 源几何规则：这个 profile 收什么尺寸的游戏窗口客户区。
    ///
    /// **两条车道的规则不是同一种，这一点曾经把人绕进去**：
    ///
    /// * dev（`non_trainable`）——`AtLeast`，只要不低于 960×540 就收，超出的部分
    ///   由 GPU 缩放到 profile 几何。为的是能在笔记本上开个小窗口调试。
    /// * 可训练——`Exactly`，客户区必须**精确等于** profile 几何。
    ///
    /// 为什么可训练这条这么严：**缩放发生在写盘之前，不可逆**。`BgraToNv12` 把
    /// source rect（窗口客户区）blt 到 dest rect（profile 几何），两者尺寸不同时
    /// 硬件就重采样，RAW MP4 里存的从此只有重采样后的像素。没有"后面再转"这一步
    /// ——data-demo-converter 不改几何，MP4 是什么就是什么。
    ///
    /// 真正承重的不变量是**可训练数据不许被凭空补充信息**（960×540 上采样到
    /// 1080p 就是在造像素）。`Exactly` 顺带也禁掉了无害的降采样，那一半是保守：
    /// 重采样对世界模型的影响没人测过，而合规成本几乎为零（把桌面调成 1080p），
    /// 不对称的时候往不可逆的那边站（owner 裁决 2026-07-29，RAW 维持固定 1080p）。
    #[must_use]
    pub fn source_geometry_rule(&self) -> SourceGeometryRule {
        if self.non_trainable {
            SourceGeometryRule::AtLeast(
                NON_TRAINABLE_MIN_SOURCE_WIDTH,
                NON_TRAINABLE_MIN_SOURCE_HEIGHT,
            )
        } else {
            SourceGeometryRule::Exactly(self.width, self.height)
        }
    }

    #[must_use]
    pub fn accepts_source_dimensions(&self, width: u32, height: u32) -> bool {
        self.source_geometry_rule().accepts(width, height)
    }

    /// 是否允许把源几何缩放到 profile 几何。与 [`Self::source_geometry_rule`]
    /// 是同一条规则的两面：`Exactly` 意味着源尺寸已等于输出，无需也不许缩放。
    #[must_use]
    pub fn allows_source_scaling(&self) -> bool {
        self.non_trainable
    }

    #[must_use]
    pub fn expected_frames_per_segment(&self) -> u64 {
        self.expected_frames_for_seconds(u64::from(self.segment_seconds))
    }

    #[must_use]
    pub fn expected_frames_for_test(&self) -> u64 {
        self.expected_frames_for_seconds(
            u64::from(self.segment_seconds) * u64::from(self.test_segments),
        )
    }

    #[must_use]
    pub fn required_gop_frames(&self) -> u32 {
        let frames =
            (u64::from(self.fps_num) * 4 + u64::from(self.fps_den) / 2) / u64::from(self.fps_den);
        u32::try_from(frames).unwrap_or(u32::MAX)
    }

    fn expected_frames_for_seconds(&self, seconds: u64) -> u64 {
        (seconds * u64::from(self.fps_num)).div_ceil(u64::from(self.fps_den))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> CaptureProfile {
        CaptureProfile {
            profile_id: DEV_PROFILE_ID.into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 60_000,
            fps_den: 1001,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 18_000_000,
            gop_frames: 240,
            segment_seconds: 10,
            test_segments: 30,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    fn accepted() -> AcceptedEncoderRelaxations {
        AcceptedEncoderRelaxations {
            min_effective_max_bitrate_bps: 15_552_000,
            gop_frames: vec![30],
            allow_rate_control_normalization: true,
            allow_low_latency_disabled: true,
            allow_unverified_b_frames: true,
        }
    }

    #[test]
    fn accepts_rational_native_rate() {
        let profile = valid();
        profile.validate().unwrap();
        assert_eq!(profile.expected_frames_per_segment(), 600);
        assert_eq!(profile.expected_frames_for_test(), 17_983);
        assert_eq!(profile.required_gop_frames(), 240);
    }

    #[test]
    fn rejects_implicit_degradation() {
        let mut profile = valid();
        profile.fps_num = 29;
        profile.fps_den = 1;
        assert!(profile.validate().is_err());
    }

    #[test]
    fn non_trainable_profile_accepts_sources_at_or_above_the_development_floor() {
        let profile = valid();
        assert_eq!(
            profile.source_geometry_rule(),
            SourceGeometryRule::AtLeast(960, 540)
        );
        assert!(profile.accepts_source_dimensions(960, 540));
        assert!(profile.accepts_source_dimensions(1273, 781));
        assert!(!profile.accepts_source_dimensions(959, 540));
        assert!(!profile.accepts_source_dimensions(960, 539));
        assert!(profile.allows_source_scaling());
    }

    #[test]
    fn without_accepted_relaxations_any_normalization_is_off_envelope() {
        let profile = valid();
        assert!(profile.accepts_effective_max_bitrate(18_000_000));
        assert!(!profile.accepts_effective_max_bitrate(15_552_000));
        assert!(profile.accepts_effective_gop(240));
        assert!(!profile.accepts_effective_gop(30));
    }

    #[test]
    fn accepted_relaxations_extend_the_envelope_without_hiding_worse_drivers() {
        let mut profile = valid();
        profile.accepted_encoder_relaxations = Some(accepted());
        profile.validate().unwrap();
        assert!(profile.accepts_effective_max_bitrate(15_552_000));
        assert!(!profile.accepts_effective_max_bitrate(12_000_000));
        assert!(profile.accepts_effective_gop(30));
        assert!(!profile.accepts_effective_gop(24));
    }

    #[test]
    fn accepted_relaxations_are_validated() {
        let mut profile = valid();
        let mut bad = accepted();
        bad.min_effective_max_bitrate_bps = 0;
        profile.accepted_encoder_relaxations = Some(bad);
        assert!(profile.validate().is_err());

        let mut floor_above_request = accepted();
        floor_above_request.min_effective_max_bitrate_bps = 19_000_000;
        profile.accepted_encoder_relaxations = Some(floor_above_request);
        assert!(profile.validate().is_err());

        let mut non_divisor_gop = accepted();
        non_divisor_gop.gop_frames = vec![50];
        profile.accepted_encoder_relaxations = Some(non_divisor_gop);
        assert!(profile.validate().is_err());

        let mut empty_gop = accepted();
        empty_gop.gop_frames = Vec::new();
        profile.accepted_encoder_relaxations = Some(empty_gop);
        assert!(profile.validate().is_err());
    }

    #[test]
    fn production_profile_keeps_exact_native_geometry() {
        let mut profile = valid();
        profile.non_trainable = false;
        // 规则种类本身要被钉住，不只是那对数字——`AtLeast(1920,1080)` 和
        // `Exactly(1920,1080)` 的 dimensions() 相同，但含义完全相反。
        assert_eq!(
            profile.source_geometry_rule(),
            SourceGeometryRule::Exactly(1920, 1080)
        );
        assert!(profile.accepts_source_dimensions(1920, 1080));
        // 1440p 是**降采样**，不造像素，却同样被拒——这一半是保守，理由见
        // `source_geometry_rule` 的文档注释（owner 裁决 2026-07-29）。
        assert!(!profile.accepts_source_dimensions(2560, 1440));
        assert!(!profile.allows_source_scaling());
    }

    #[test]
    fn old_profile_without_audio_remains_disabled() {
        let bytes = serde_json::to_vec(&valid()).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("audio").is_none());

        let parsed = CaptureProfile::from_slice(&bytes).unwrap();
        assert!(parsed.audio.is_none());
    }

    #[test]
    fn process_loopback_profile_is_fixed_and_explicit() {
        let mut profile = valid();
        profile.audio = Some(CaptureAudioProfile::process_loopback());
        profile.validate().unwrap();

        profile.audio.as_mut().unwrap().sample_rate_hz = 44_100;
        assert!(profile.validate().is_err());
    }
}
