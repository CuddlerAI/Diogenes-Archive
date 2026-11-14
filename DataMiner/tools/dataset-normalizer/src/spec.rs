use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::hash::sha256_bytes;

pub const SPEC_SCHEMA: &str = "dm-normalization-spec-1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RationalRate {
    pub fps_num: u32,
    pub fps_den: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationSpec {
    pub schema_version: String,
    pub spec_id: String,
    pub source: RationalRate,
    pub target: TargetSpec,
    pub phase_policy: String,
    pub video_sample_policy: String,
    pub input_policy: String,
    pub pointer_policy: String,
    pub trim_policy: String,
    pub duplicate_policy: DuplicatePolicy,
    pub framepair_policy: FramePairPolicy,
    pub encoder: EncoderPolicy,
    pub trainability: TrainabilityPolicy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSpec {
    pub geometry_policy: String,
    pub fps_num: u32,
    pub fps_den: u32,
    pub chunk_seconds: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DuplicatePolicy {
    pub version: String,
    pub disposition: String,
    pub owner_status: String,
    pub trainable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FramePairPolicy {
    pub version: String,
    pub emit: bool,
    pub owner_status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderPolicy {
    pub implementation: String,
    pub version: String,
    pub codec: String,
    pub settings_id: String,
    pub binary_policy: String,
    pub gop_seconds: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrainabilityPolicy {
    pub version: String,
    pub is_trainable: bool,
    pub production_certified: bool,
}

#[derive(Clone, Debug)]
pub struct LoadedSpec {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub spec: NormalizationSpec,
}

impl LoadedSpec {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read normalization spec {}", path.display()))?;
        Self::from_bytes(path, bytes)
    }

    pub fn from_bytes(path: impl AsRef<Path>, bytes: Vec<u8>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let spec: NormalizationSpec = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid normalization spec {}", path.display()))?;
        spec.validate()?;
        Ok(Self {
            path: path.to_path_buf(),
            sha256: sha256_bytes(&bytes),
            bytes,
            spec,
        })
    }
}

impl NormalizationSpec {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema_version == SPEC_SCHEMA,
            "unsupported normalization spec schema {}",
            self.schema_version
        );
        ensure!(
            self.source
                == (RationalRate {
                    fps_num: 60,
                    fps_den: 1
                }),
            "normalization v1 requires exact source fps 60/1"
        );
        let target = self.target_rate();
        ensure!(
            matches!(
                target,
                RationalRate {
                    fps_num: 30 | 20 | 12,
                    fps_den: 1
                }
            ),
            "normalization v1 only supports target fps 30/1, 20/1, or 12/1"
        );
        ensure!(
            60_u32.is_multiple_of(target.fps_num),
            "normalization v1 target must be an integer divisor of 60"
        );
        ensure!(
            self.spec_id == format!("fps60-to-training{}-v1", target.fps_num),
            "normalization spec id does not match its exact target fps"
        );
        ensure!(
            self.target.geometry_policy == "preserve_source_v1",
            "unsupported target geometry policy"
        );
        ensure!(
            self.target.chunk_seconds == 4,
            "normalization v1 requires four-second chunks"
        );
        for (actual, expected, label) in [
            (
                self.phase_policy.as_str(),
                "per_run_qpc_anchor_half_open_round_v1",
                "phase",
            ),
            (
                self.video_sample_policy.as_str(),
                "last_source_cfr_slot_before_window_end_v1",
                "video sample",
            ),
            (
                self.input_policy.as_str(),
                "capture_core_frame_projector_v1",
                "input",
            ),
            (
                self.pointer_policy.as_str(),
                "sum_raw_i64_then_clamp_and_bin_once_v1",
                "pointer",
            ),
            (
                self.trim_policy.as_str(),
                "trim_incomplete_tail_window_v1",
                "trim",
            ),
        ] {
            ensure!(actual == expected, "unsupported {label} policy {actual}");
        }
        ensure!(
            self.duplicate_policy.version == "preserve_and_mark_v1"
                && self.duplicate_policy.disposition == "preserve_and_mark"
                && self.duplicate_policy.owner_status == "pending"
                && !self.duplicate_policy.trainable,
            "duplicate policy must stay versioned, preserved, and non-trainable pending owner decision"
        );
        ensure!(
            self.framepair_policy.version == "deferred_owner_decision_v1"
                && !self.framepair_policy.emit
                && self.framepair_policy.owner_status == "pending",
            "FramePair emission is not authorized until its owner policy is frozen"
        );
        ensure!(
            self.encoder.implementation == "pinned_offline_ffmpeg"
                && self.encoder.version == "ffmpeg_cli_sha256_pinned_v1"
                && self.encoder.codec == "h264"
                && self.encoder.settings_id == "libx264_cfr_closed_gop_no_bframes_v1"
                && self.encoder.binary_policy == "sha256_exact_v1"
                && self.encoder.gop_seconds == 4,
            "unsupported or unversioned offline encoder policy"
        );
        ensure!(
            self.trainability.version == "pending_owner_decisions_v1"
                && !self.trainability.is_trainable
                && !self.trainability.production_certified,
            "normalization v1 output must remain non-trainable and non-certified"
        );
        Ok(())
    }

    #[must_use]
    pub const fn target_rate(&self) -> RationalRate {
        RationalRate {
            fps_num: self.target.fps_num,
            fps_den: self.target.fps_den,
        }
    }

    #[must_use]
    pub const fn downsample_factor(&self) -> u32 {
        60 / self.target.fps_num
    }

    #[must_use]
    pub const fn gop_frames(&self) -> u32 {
        self.target.fps_num * self.encoder.gop_seconds / self.target.fps_den
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped(name: &str) -> LoadedSpec {
        LoadedSpec::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("specs")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn shipped_specs_freeze_the_supported_integer_downsamples() {
        for (name, fps, factor, gop) in [
            ("fps60-to-training30-v1.json", 30, 2, 120),
            ("fps60-to-training20-v1.json", 20, 3, 80),
            ("fps60-to-training12-v1.json", 12, 5, 48),
        ] {
            let loaded = shipped(name);
            assert_eq!(loaded.spec.target_rate().fps_num, fps);
            assert_eq!(loaded.spec.downsample_factor(), factor);
            assert_eq!(loaded.spec.gop_frames(), gop);
            assert!(loaded.sha256.starts_with("sha256:"));
            assert!(!loaded.spec.framepair_policy.emit);
            assert!(!loaded.spec.trainability.is_trainable);
        }
    }

    #[test]
    fn exact_source_rate_and_unknown_policy_fail_closed() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&shipped("fps60-to-training30-v1.json").bytes).unwrap();
        value["source"] = serde_json::json!({"fps_num": 60000, "fps_den": 1001});
        let error = LoadedSpec::from_bytes("bad.json", serde_json::to_vec(&value).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("exact source fps 60/1"));

        value["source"] = serde_json::json!({"fps_num": 60, "fps_den": 1});
        value["input_policy"] = serde_json::json!("nearly_the_same_v1");
        let error = LoadedSpec::from_bytes("bad.json", serde_json::to_vec(&value).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported input policy"));
    }

    #[test]
    fn unresolved_framepair_or_duplicate_trainability_cannot_be_enabled() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&shipped("fps60-to-training30-v1.json").bytes).unwrap();
        value["framepair_policy"]["emit"] = serde_json::json!(true);
        assert!(LoadedSpec::from_bytes("bad.json", serde_json::to_vec(&value).unwrap()).is_err());
        value["framepair_policy"]["emit"] = serde_json::json!(false);
        value["duplicate_policy"]["trainable"] = serde_json::json!(true);
        assert!(LoadedSpec::from_bytes("bad.json", serde_json::to_vec(&value).unwrap()).is_err());
    }
}
