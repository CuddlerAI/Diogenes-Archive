//! Exact Fallout 4 build layouts. Admission is separate from layout discovery:
//! a known relocation is not proof that the rendered camera/depth is correct.

use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FalloutCapturePurpose {
    Production,
    /// Only discovery or a managed run already marked permanently non-deliverable.
    ValidationOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FalloutBuild {
    pub version: &'static str,
    pub executable_sha256: &'static str,
    pub camera_singleton_relocation: usize,
    pub player_singleton_relocation: usize,
}

// F4SE 0.7.8, commit cb39721a8155b0ec31640291f2997fa1c43474af.
pub const FALLOUT4_1_11_221: FalloutBuild = FalloutBuild {
    version: "1.11.221.0",
    executable_sha256: "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61",
    camera_singleton_relocation: 0x030D_BDD8,
    player_singleton_relocation: 0x032D_22E0,
};

// F4SE 0.7.9, commit 6f6a7caa9aeaebc957a70d934b6ab6b80eb768b2:
// f4se/GameCamera.cpp, GameReferences.cpp; f4se_common/f4se_version.h pins 1.11.240.
// This is an authoring candidate, NOT a production compatibility approval.
pub const FALLOUT4_1_11_240: FalloutBuild = FalloutBuild {
    version: "1.11.240.0",
    executable_sha256: "sha256:fdcef37ac1230af6d0b0050eb2142b139ef3a867b37b9211fb6edfcc646072f8",
    camera_singleton_relocation: 0x030E_6E58,
    player_singleton_relocation: 0x032D_D370,
};

pub fn resolve_fallout_build(
    actual_executable_sha256: &str,
    purpose: FalloutCapturePurpose,
) -> Result<FalloutBuild, String> {
    match actual_executable_sha256 {
        hash if hash == FALLOUT4_1_11_221.executable_sha256 => Ok(FALLOUT4_1_11_221),
        hash if hash == FALLOUT4_1_11_240.executable_sha256 => match purpose {
            FalloutCapturePurpose::ValidationOnly => Ok(FALLOUT4_1_11_240),
            FalloutCapturePurpose::Production => Err(
                "Fallout4 executable 1.11.240.0 is authoring-only; production remains blocked until camera, player and depth evidence is verified"
                    .into(),
            ),
        },
        _ => Err(format!(
            "Fallout exact-build mismatch: unsupported executable_sha256 {actual_executable_sha256}"
        )),
    }
}

/// Mark candidate managed runs before any sampler/writer starts. The client recovery path
/// recognizes this prefix even after a restart; a failed start must not remove the marker.
pub fn prepare_fallout_capture(
    output_dir: &Path,
    actual_executable_sha256: &str,
    purpose: FalloutCapturePurpose,
) -> Result<(), String> {
    let build = resolve_fallout_build(actual_executable_sha256, purpose)?;
    if build != FALLOUT4_1_11_240 {
        return Ok(());
    }
    let provenance = output_dir.join("provenance");
    std::fs::create_dir_all(&provenance)
        .map_err(|error| format!("create Fallout authoring provenance: {error}"))?;
    let path = provenance.join("performance-validation-only-fallout4-authoring.json");
    let marker = serde_json::to_vec_pretty(&serde_json::json!({
        "schema": "dm-depth-performance-validation-only-v1",
        "deliverable": false,
        "certificationCandidateWritten": false,
        "reason": "unverified-fallout4-build-authoring",
        "executableSha256": build.executable_sha256,
        "gameVersion": build.version,
    }))
    .map_err(|error| format!("serialize Fallout authoring provenance: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create fresh Fallout authoring marker: {error}"))?;
    file.write_all(&marker)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("persist Fallout authoring marker: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_build_keeps_its_layout_in_both_modes() {
        for purpose in [
            FalloutCapturePurpose::Production,
            FalloutCapturePurpose::ValidationOnly,
        ] {
            assert_eq!(
                resolve_fallout_build(FALLOUT4_1_11_221.executable_sha256, purpose),
                Ok(FALLOUT4_1_11_221)
            );
        }
    }

    #[test]
    fn candidate_uses_its_own_addresses_only_in_non_deliverable_mode() {
        assert_eq!(
            resolve_fallout_build(
                FALLOUT4_1_11_240.executable_sha256,
                FalloutCapturePurpose::ValidationOnly
            ),
            Ok(FALLOUT4_1_11_240)
        );
        assert_ne!(
            FALLOUT4_1_11_240.camera_singleton_relocation,
            FALLOUT4_1_11_221.camera_singleton_relocation
        );
        assert_ne!(
            FALLOUT4_1_11_240.player_singleton_relocation,
            FALLOUT4_1_11_221.player_singleton_relocation
        );
        let error = resolve_fallout_build(
            FALLOUT4_1_11_240.executable_sha256,
            FalloutCapturePurpose::Production,
        )
        .unwrap_err();
        assert!(error.contains("authoring-only"));
    }

    #[test]
    fn validation_is_not_an_arbitrary_hash_bypass() {
        for hash in [
            "",
            "1.11.240.0",
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ] {
            for purpose in [
                FalloutCapturePurpose::Production,
                FalloutCapturePurpose::ValidationOnly,
            ] {
                assert!(resolve_fallout_build(hash, purpose).is_err());
            }
        }
    }

    #[test]
    fn candidate_managed_run_is_permanently_marked_before_capture() {
        let root = std::env::temp_dir().join(format!(
            "fallout-authoring-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let hash = FALLOUT4_1_11_240.executable_sha256;
        assert!(prepare_fallout_capture(&root, hash, FalloutCapturePurpose::Production).is_err());
        assert!(!root.exists());
        prepare_fallout_capture(&root, hash, FalloutCapturePurpose::ValidationOnly).unwrap();
        let path = root.join("provenance/performance-validation-only-fallout4-authoring.json");
        let bytes = std::fs::read(&path).unwrap();
        let marker: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(marker["deliverable"], false);
        assert_eq!(marker["executableSha256"], hash);
        assert!(
            prepare_fallout_capture(&root, hash, FalloutCapturePurpose::ValidationOnly).is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
    }
}
