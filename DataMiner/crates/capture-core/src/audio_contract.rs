use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

// 两代摄取契约的 schema 词表搬去了 `ingest_contract`：它们描述的是"这场采集属于
// 哪一代契约"，与音频无关。断代前它们放在这里，是因为世代曾经**只由**音频这一个
// 事实决定（docs/100）——issue #855 §B 之后不再是了。这里不做 re-export：
// catalog-validator 以 `#[path]` 挂载本文件，一条没人用的 `pub use` 在那边就是
// 一条 `-D warnings` 会拦下的 unused_imports。

pub const AUDIO_ARTIFACT_NAME: &str = "audio.pcm.zst";
pub const AUDIO_SAMPLE_RATE_HZ: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_BITS_PER_SAMPLE: u16 = 16;
pub const AUDIO_CHANNEL_MASK_STEREO: u32 = 0x3;
pub const AUDIO_SAMPLE_FORMAT: &str = "pcm_s16le";
pub const AUDIO_REQUESTED_FORMAT: &str = "pcm_s16le/48000/2";
pub const AUDIO_COMPRESSION: &str = "zstd";
pub const AUDIO_PCM_BYTES_PER_SECOND: u64 =
    AUDIO_SAMPLE_RATE_HZ as u64 * AUDIO_CHANNELS as u64 * AUDIO_BITS_PER_SAMPLE as u64 / 8;
pub const MAX_DEGRADED_SILENCE_MS: u64 = 100;
pub const MAX_DEGRADED_SILENCE_PERCENT: u64 = 1;

const BASE_PAYLOAD_NAMES: [&str; 4] = [
    "video.mp4",
    "frames.jsonl.zst",
    "events.jsonl.zst",
    "meta.json",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentAudioState {
    Ready,
    Degraded,
    Absent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSource {
    ProcessLoopback,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioPcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub sample_format: String,
    pub channel_mask: u32,
    pub requested_format: String,
    pub system_src: bool,
}

impl Default for AudioPcmFormat {
    fn default() -> Self {
        Self {
            sample_rate: AUDIO_SAMPLE_RATE_HZ,
            channels: AUDIO_CHANNELS,
            bits: AUDIO_BITS_PER_SAMPLE,
            sample_format: AUDIO_SAMPLE_FORMAT.into(),
            channel_mask: AUDIO_CHANNEL_MASK_STEREO,
            requested_format: AUDIO_REQUESTED_FORMAT.into(),
            system_src: true,
        }
    }
}

impl AudioPcmFormat {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self == &Self::default(),
            "audio PCM format must be pcm_s16le/48000/stereo with system SRC"
        );
        Ok(())
    }

    #[must_use]
    pub fn bytes_per_frame(&self) -> u64 {
        u64::from(self.channels) * u64::from(self.bits) / 8
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSampleSpan {
    pub start_frame: u64,
    pub frames: u64,
}

impl AudioSampleSpan {
    fn validate(&self, total_frames: u64) -> anyhow::Result<()> {
        ensure!(self.frames > 0, "audio health span must not be empty");
        let end = self
            .start_frame
            .checked_add(self.frames)
            .context("audio health span overflow")?;
        ensure!(
            end <= total_frames,
            "audio health span exceeds the segment sample range"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentAudioHealth {
    pub discontinuities: Vec<AudioSampleSpan>,
    pub silence_filled_spans: Vec<AudioSampleSpan>,
    pub queue_overflows: u64,
    pub timestamp_errors: u64,
    pub max_qpc_drift_us: u64,
}

impl SegmentAudioHealth {
    fn validate_spans(&self, total_frames: u64) -> anyhow::Result<()> {
        for span in self
            .discontinuities
            .iter()
            .chain(self.silence_filled_spans.iter())
        {
            span.validate(total_frames)?;
        }
        Ok(())
    }

    fn silence_filled_frames(&self) -> anyhow::Result<u64> {
        self.silence_filled_spans
            .iter()
            .try_fold(0_u64, |sum, span| {
                sum.checked_add(span.frames)
                    .context("audio silence-filled frame count overflow")
            })
    }

    #[must_use]
    pub fn is_pristine(&self) -> bool {
        self.discontinuities.is_empty()
            && self.silence_filled_spans.is_empty()
            && self.queue_overflows == 0
            && self.timestamp_errors == 0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentAudioMetadata {
    pub state: SegmentAudioState,
    pub source: AudioSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub format: AudioPcmFormat,
    pub qpc_frequency: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_sample_qpc: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    pub health: SegmentAudioHealth,
}

impl SegmentAudioMetadata {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.format.validate()?;
        ensure!(
            self.qpc_frequency > 0,
            "audio QPC frequency must be positive"
        );

        match self.state {
            SegmentAudioState::Ready => {
                self.validate_present_artifact()?;
                ensure!(
                    self.failure_code.is_none(),
                    "ready audio must not carry a failure code"
                );
                ensure!(
                    self.health.is_pristine(),
                    "ready audio must have pristine health"
                );
            }
            SegmentAudioState::Degraded => {
                let frames = self.validate_present_artifact()?;
                ensure!(
                    self.failure_code
                        .as_deref()
                        .is_some_and(|code| !code.trim().is_empty()),
                    "degraded audio must carry a failure code"
                );
                ensure!(
                    self.health.queue_overflows == 0 && self.health.timestamp_errors == 0,
                    "audio queue overflow or timestamp error must be absent, not degraded"
                );
                self.health.validate_spans(frames)?;
                let silence_frames = self.health.silence_filled_frames()?;
                ensure!(
                    silence_frames > 0,
                    "degraded audio must record a filled gap"
                );
                let max_by_time = u64::from(AUDIO_SAMPLE_RATE_HZ)
                    .checked_mul(MAX_DEGRADED_SILENCE_MS)
                    .context("audio degraded time threshold overflow")?
                    / 1_000;
                let max_by_percent = frames
                    .checked_mul(MAX_DEGRADED_SILENCE_PERCENT)
                    .context("audio degraded percentage threshold overflow")?
                    / 100;
                ensure!(
                    silence_frames <= max_by_time && silence_frames <= max_by_percent,
                    "degraded audio exceeds the bounded silence-fill threshold"
                );
            }
            SegmentAudioState::Absent => {
                ensure!(
                    self.path.is_none()
                        && self.sha256.is_none()
                        && self.bytes.is_none()
                        && self.first_sample_qpc.is_none()
                        && self.frames.is_none(),
                    "absent audio must not claim an artifact or sample range"
                );
                ensure!(
                    self.failure_code
                        .as_deref()
                        .is_some_and(|code| !code.trim().is_empty()),
                    "absent audio must carry a failure code"
                );
            }
        }
        Ok(())
    }

    fn validate_present_artifact(&self) -> anyhow::Result<u64> {
        ensure!(
            self.path.as_deref() == Some(AUDIO_ARTIFACT_NAME),
            "present audio must use the canonical audio.pcm.zst path"
        );
        let hash = self
            .sha256
            .as_deref()
            .context("present audio is missing its SHA-256")?;
        let digest = hash
            .strip_prefix("sha256:")
            .context("audio SHA-256 must use the sha256:<hex> encoding")?;
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "audio SHA-256 digest must be 64 lowercase hexadecimal characters"
        );
        ensure!(
            self.bytes.is_some_and(|bytes| bytes > 0),
            "present audio must record a positive compressed byte size"
        );
        ensure!(
            self.first_sample_qpc.is_some(),
            "present audio must record first_sample_qpc"
        );
        let frames = self
            .frames
            .context("present audio is missing frame count")?;
        ensure!(frames > 0, "present audio frame count must be positive");
        self.health.validate_spans(frames)?;
        Ok(frames)
    }

    #[must_use]
    pub fn has_artifact(&self) -> bool {
        matches!(
            self.state,
            SegmentAudioState::Ready | SegmentAudioState::Degraded
        )
    }

    #[must_use]
    pub fn is_publicly_publishable(&self) -> bool {
        self.state == SegmentAudioState::Ready && self.health.is_pristine()
    }

    pub fn uncompressed_bytes(&self) -> anyhow::Result<Option<u64>> {
        self.frames
            .map(|frames| {
                frames
                    .checked_mul(self.format.bytes_per_frame())
                    .context("audio PCM byte count overflow")
            })
            .transpose()
    }
}

#[must_use]
pub fn expected_manifest_payloads(audio: Option<&SegmentAudioMetadata>) -> BTreeSet<&'static str> {
    let mut names = BASE_PAYLOAD_NAMES.into_iter().collect::<BTreeSet<_>>();
    if audio.is_some_and(SegmentAudioMetadata::has_artifact) {
        names.insert(AUDIO_ARTIFACT_NAME);
    }
    names
}

// 这里曾有 `recording_schema_version(audio)` / `session_schema_version(audio)` 两个
// 自由函数：世代只由音频这一个事实决定，所以调用方只要交出"有没有音频"即可。
// issue #855 §B 之后不成立了——世代先由**契约**决定，音频只在 legacy 契约内部再分
// 一层。两个函数因此并进 `SessionContract`：一个不带契约就能取到 schema 版本的
// 入口，等于给每个调用点留一条静默落回 legacy 的路，而"这场采集属于哪一代"正是
// 本 issue 要求显式回答的问题。

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> String {
        format!("sha256:{}", "a".repeat(64))
    }

    fn ready() -> SegmentAudioMetadata {
        SegmentAudioMetadata {
            state: SegmentAudioState::Ready,
            source: AudioSource::ProcessLoopback,
            path: Some(AUDIO_ARTIFACT_NAME.into()),
            sha256: Some(hash()),
            bytes: Some(42),
            format: AudioPcmFormat::default(),
            qpc_frequency: 10_000_000,
            first_sample_qpc: Some(123),
            frames: Some(480_000),
            failure_code: None,
            health: SegmentAudioHealth::default(),
        }
    }

    #[test]
    fn ready_audio_requires_a_pristine_canonical_artifact() {
        let metadata = ready();
        let mut measured = metadata.clone();
        measured.health.max_qpc_drift_us = 17;
        measured.validate().unwrap();
        assert!(measured.has_artifact());
        assert!(measured.is_publicly_publishable());
        assert_eq!(measured.uncompressed_bytes().unwrap(), Some(1_920_000));

        let mut unhealthy = metadata.clone();
        unhealthy.health.timestamp_errors = 1;
        assert!(unhealthy.validate().is_err());
    }

    #[test]
    fn bounded_silence_fill_is_degraded_and_never_public() {
        let mut metadata = ready();
        metadata.state = SegmentAudioState::Degraded;
        metadata.failure_code = Some("AUDIO_BOUNDED_GAP_FILLED".into());
        metadata.health.discontinuities.push(AudioSampleSpan {
            start_frame: 10_000,
            frames: 2_400,
        });
        metadata.health.silence_filled_spans.push(AudioSampleSpan {
            start_frame: 10_000,
            frames: 2_400,
        });

        metadata.validate().unwrap();
        assert!(metadata.has_artifact());
        assert!(!metadata.is_publicly_publishable());

        metadata.health.silence_filled_spans[0].frames = 4_801;
        assert!(metadata.validate().is_err());
    }

    #[test]
    fn absent_audio_cannot_claim_a_sidecar() {
        let mut metadata = ready();
        metadata.state = SegmentAudioState::Absent;
        metadata.failure_code = Some("AUDIO_TIMESTAMP_ERROR".into());
        metadata.path = None;
        metadata.sha256 = None;
        metadata.bytes = None;
        metadata.first_sample_qpc = None;
        metadata.frames = None;
        metadata.health.timestamp_errors = 1;
        metadata.validate().unwrap();
        assert!(!metadata.has_artifact());
        assert!(!metadata.is_publicly_publishable());

        metadata.path = Some(AUDIO_ARTIFACT_NAME.into());
        assert!(metadata.validate().is_err());
    }

    #[test]
    fn manifest_payload_set_is_conditional_without_changing_legacy_names() {
        let legacy = expected_manifest_payloads(None);
        assert_eq!(legacy.len(), 4);
        assert!(!legacy.contains(AUDIO_ARTIFACT_NAME));

        let with_audio = expected_manifest_payloads(Some(&ready()));
        assert_eq!(with_audio.len(), 5);
        assert!(with_audio.contains(AUDIO_ARTIFACT_NAME));
    }

    /// 退役的 certified 词表不得从任何一个世代取值里冒出来（docs/100）。服务端
    /// 迁移 0029 的 schema_version 词表以这里的取值为权威。
    #[test]
    fn no_generation_yields_a_retired_certified_word() {
        for contract in [
            crate::ingest_contract::SessionContract::LocalDevV1,
            crate::ingest_contract::SessionContract::IngestV2,
        ] {
            for audio in [false, true] {
                assert!(!contract.session_schema_version(audio).contains("certified"));
                assert!(
                    !contract
                        .recording_schema_version(audio)
                        .contains("certified")
                );
            }
        }
    }
}
