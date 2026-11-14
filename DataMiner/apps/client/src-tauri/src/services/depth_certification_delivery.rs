//! Signed delivery and last-good caching for Depth performance certifications.
//!
//! This reuses the catalog delivery trust anchor, but not its API or database. The payload is one
//! small signed R2 object with a distinct signature domain.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use depth_lab::certification::DepthPerfCertificationManifest;
use serde::{Deserialize, Serialize};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::platform::windows::catalog::verify_delivery_payload_signature;

pub const SIGNED_DEPTH_CERTIFICATIONS_SCHEMA_V1: &str = "dm-signed-depth-certifications-1";
pub const DEPTH_CERTIFICATION_SIGNATURE_DOMAIN: &[u8] = b"dm-signed-depth-certifications-1\x00";
const DELIVERY_SIGNING_KEY_ID: &str = "dm-dev-catalog-delivery-v1";
const CENTRAL_MANIFEST_URL: &str =
    "https://releases.diogenesmodel.com/depth/performance-certifications/v3/current.json";
const LAST_GOOD_FILE: &str = "depth-performance-certifications-last-good-v3.json";
const MAX_SIGNED_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
pub const RETIRED_D3D11_V2_FILE: &str = "perf-certifications.json";
pub const RETIRED_D3D12_V3_FILE: &str = "perf-certifications-d3d12-v3.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignedDepthCertificationsEnvelope {
    schema_version: String,
    signing_key_id: String,
    signature: String,
    manifest: String,
}

trait DepthCertificationFetcher {
    fn fetch(&self) -> Result<Vec<u8>, String>;
}

struct ReqwestDepthCertificationFetcher;

impl DepthCertificationFetcher for ReqwestDepthCertificationFetcher {
    fn fetch(&self) -> Result<Vec<u8>, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("build HTTP client: {error}"))?;
        let response = client
            .get(CENTRAL_MANIFEST_URL)
            .send()
            .map_err(|error| format!("fetch central manifest: {error}"))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(format!(
                "central manifest returned HTTP {}",
                response.status()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SIGNED_MANIFEST_BYTES)
        {
            return Err(format!(
                "central manifest exceeds {MAX_SIGNED_MANIFEST_BYTES} bytes"
            ));
        }
        let bytes = response
            .bytes()
            .map_err(|error| format!("read central manifest: {error}"))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SIGNED_MANIFEST_BYTES {
            return Err(format!(
                "central manifest exceeds {MAX_SIGNED_MANIFEST_BYTES} bytes"
            ));
        }
        Ok(bytes.to_vec())
    }
}

pub fn depth_state_directory() -> Result<PathBuf, String> {
    let program_data = std::env::var_os("ProgramData")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "ProgramData is unavailable; cannot locate Depth certification state".to_string()
        })?;
    Ok(PathBuf::from(program_data).join("DataMiner").join("depth"))
}

pub fn certification_candidate_directory() -> Result<PathBuf, String> {
    Ok(depth_state_directory()?.join("certification-candidates-v1"))
}

/// Delete only the two retired local stores. Their bytes are never opened, parsed, migrated, or
/// copied into the new cache.
pub fn remove_retired_certification_stores() -> Result<(), String> {
    remove_retired_certification_stores_from(&depth_state_directory()?)
}

fn remove_retired_certification_stores_from(directory: &Path) -> Result<(), String> {
    for file_name in [RETIRED_D3D11_V2_FILE, RETIRED_D3D12_V3_FILE] {
        let path = directory.join(file_name);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "removed retired Depth certification store")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot remove retired Depth certification store {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Refresh from the central R2 object. If it is unavailable, use a re-verified last-good cache.
/// A received but invalid signed object still fails closed instead of falling back.
pub fn load_active_manifest() -> Result<DepthPerfCertificationManifest, String> {
    let cache = depth_state_directory()?.join(LAST_GOOD_FILE);
    sync_with_fetcher(&ReqwestDepthCertificationFetcher, &cache)
}

fn sync_with_fetcher(
    fetcher: &dyn DepthCertificationFetcher,
    cache_path: &Path,
) -> Result<DepthPerfCertificationManifest, String> {
    match fetcher.fetch() {
        Ok(remote_bytes) => {
            let remote = verify_envelope(&remote_bytes)?;
            atomic_write(cache_path, &remote_bytes)?;
            Ok(remote)
        }
        Err(error) => {
            let cached_bytes = read_optional_bounded(cache_path)?.ok_or_else(|| {
                format!("central Depth certifications are offline and no last-good cache exists: {error}")
            })?;
            verify_envelope(&cached_bytes).map_err(|cache_error| {
                format!(
                    "central Depth certifications are offline ({error}); cached last-good is invalid: {cache_error}"
                )
            })
        }
    }
}

fn verify_envelope(bytes: &[u8]) -> Result<DepthPerfCertificationManifest, String> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SIGNED_MANIFEST_BYTES {
        return Err(format!(
            "signed Depth certifications exceed {MAX_SIGNED_MANIFEST_BYTES} bytes"
        ));
    }
    let envelope: SignedDepthCertificationsEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse signed Depth certification envelope: {error}"))?;
    if envelope.schema_version != SIGNED_DEPTH_CERTIFICATIONS_SCHEMA_V1 {
        return Err(format!(
            "unsupported signed Depth certification schema: {}",
            envelope.schema_version
        ));
    }
    if envelope.signing_key_id != DELIVERY_SIGNING_KEY_ID {
        return Err(format!(
            "unexpected Depth certification signing key id: {}",
            envelope.signing_key_id
        ));
    }
    let manifest_bytes = base64::engine::general_purpose::STANDARD
        .decode(&envelope.manifest)
        .map_err(|error| format!("decode Depth certification manifest: {error}"))?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&envelope.signature)
        .map_err(|error| format!("decode Depth certification signature: {error}"))?;
    verify_delivery_payload_signature(
        DEPTH_CERTIFICATION_SIGNATURE_DOMAIN,
        &manifest_bytes,
        &signature,
    )
    .map_err(|error| format!("verify Depth certification signature: {error:#}"))?;
    let manifest: DepthPerfCertificationManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("parse Depth certification manifest: {error}"))?;
    manifest.validate()?;
    if manifest.schema != depth_lab::certification::DEPTH_PERF_CERTIFICATIONS_SCHEMA_V3 {
        return Err(
            "expected v3 Depth certification manifest; earlier cadence contracts are not a migration source".into(),
        );
    }
    Ok(manifest)
}

fn read_optional_bounded(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "Depth certification cache is not a regular file: {}",
                path.display()
            ));
        }
        Ok(metadata) if metadata.len() > MAX_SIGNED_MANIFEST_BYTES => {
            return Err(format!(
                "Depth certification cache exceeds {MAX_SIGNED_MANIFEST_BYTES} bytes"
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "inspect Depth certification cache {}: {error}",
                path.display()
            ));
        }
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|error| format!("read Depth certification cache {}: {error}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "Depth certification cache has no parent: {}",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create Depth certification cache directory {}: {error}",
            parent.display()
        )
    })?;
    let temporary = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            format!(
                "create temporary Depth certification cache {}: {error}",
                temporary.display()
            )
        })?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "write temporary Depth certification cache {}: {error}",
            temporary.display()
        ));
    }
    drop(file);
    move_replace(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

fn move_replace(source: &Path, destination: &Path) -> Result<(), String> {
    fn wide(path: &OsStr) -> Vec<u16> {
        path.encode_wide().chain(Some(0)).collect()
    }
    let source = wide(source.as_os_str());
    let destination = wide(destination.as_os_str());
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| format!("publish Depth certification cache atomically: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth_lab::certification::{ApprovedProbeRuntime, DEPTH_PERF_CERTIFICATIONS_SCHEMA_V3};
    use ed25519_dalek::{Signer as _, SigningKey};

    struct StaticFetcher(Result<Vec<u8>, String>);

    impl DepthCertificationFetcher for StaticFetcher {
        fn fetch(&self) -> Result<Vec<u8>, String> {
            match &self.0 {
                Ok(bytes) => Ok(bytes.clone()),
                Err(error) => Err(error.clone()),
            }
        }
    }

    fn signed() -> Vec<u8> {
        signed_with_schema(DEPTH_PERF_CERTIFICATIONS_SCHEMA_V3)
    }

    fn signed_with_schema(schema: &str) -> Vec<u8> {
        let manifest = DepthPerfCertificationManifest {
            schema: schema.into(),
            approved_runtimes: vec![ApprovedProbeRuntime {
                runtime_version: "depth-2026.08.1".into(),
                dll_sha256: format!("sha256:{}", "a".repeat(64)),
                d3d11_generation: 4,
                d3d12_generation: 9,
            }],
            certifications: Vec::new(),
            device_profile_grants: Vec::new(),
        };
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let seed = *b"dm-dev-catalog-delivery-seed-v1\0";
        let key = SigningKey::from_bytes(&seed);
        let mut input = DEPTH_CERTIFICATION_SIGNATURE_DOMAIN.to_vec();
        input.extend_from_slice(&manifest);
        let signature = key.sign(&input).to_bytes();
        serde_json::to_vec(&SignedDepthCertificationsEnvelope {
            schema_version: SIGNED_DEPTH_CERTIFICATIONS_SCHEMA_V1.into(),
            signing_key_id: DELIVERY_SIGNING_KEY_ID.into(),
            signature: base64::engine::general_purpose::STANDARD.encode(signature),
            manifest: base64::engine::general_purpose::STANDARD.encode(manifest),
        })
        .unwrap()
    }

    #[test]
    fn v3_does_not_accept_legacy_endpoint_or_legacy_cache_as_reuse_authority() {
        let temp = tempfile::tempdir().unwrap();
        for version in ["v1", "v2"] {
            let old = signed_with_schema(&format!("dm-depth-performance-certifications-{version}"));
            std::fs::write(
                temp.path().join(format!(
                    "depth-performance-certifications-last-good-{version}.json"
                )),
                &old,
            )
            .unwrap();
            let cache = temp.path().join(LAST_GOOD_FILE);
            assert!(sync_with_fetcher(&StaticFetcher(Err("offline".into())), &cache).is_err());
            assert!(sync_with_fetcher(&StaticFetcher(Ok(old)), &cache).is_err());
            assert!(!cache.exists());
        }
    }

    #[test]
    fn signed_remote_becomes_reverified_offline_last_good() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(LAST_GOOD_FILE);
        let online = sync_with_fetcher(&StaticFetcher(Ok(signed())), &cache).unwrap();
        let offline =
            sync_with_fetcher(&StaticFetcher(Err("airplane mode".into())), &cache).unwrap();
        assert_eq!(offline, online);
    }

    #[test]
    fn first_offline_start_and_tampered_cache_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(LAST_GOOD_FILE);
        assert!(sync_with_fetcher(&StaticFetcher(Err("offline".into())), &cache).is_err());

        sync_with_fetcher(&StaticFetcher(Ok(signed())), &cache).unwrap();

        let mut tampered = std::fs::read(&cache).unwrap();
        let index = tampered.iter().position(|byte| *byte == b'a').unwrap();
        tampered[index] = b'b';
        std::fs::write(&cache, tampered).unwrap();
        assert!(sync_with_fetcher(&StaticFetcher(Err("offline".into())), &cache).is_err());
    }

    #[test]
    fn invalid_received_response_never_falls_back_to_cache() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(LAST_GOOD_FILE);
        sync_with_fetcher(&StaticFetcher(Ok(signed())), &cache).unwrap();
        assert!(sync_with_fetcher(&StaticFetcher(Ok(b"{}".to_vec())), &cache).is_err());
    }

    #[test]
    fn upgrade_cleanup_removes_only_the_two_named_retired_stores() {
        let temp = tempfile::tempdir().unwrap();
        let d3d11 = temp.path().join(RETIRED_D3D11_V2_FILE);
        let d3d12 = temp.path().join(RETIRED_D3D12_V3_FILE);
        let unrelated = temp.path().join("keep-me.json");
        std::fs::write(&d3d11, b"must not be parsed").unwrap();
        std::fs::write(&d3d12, b"must not be parsed either").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        remove_retired_certification_stores_from(temp.path()).unwrap();

        assert!(!d3d11.exists());
        assert!(!d3d12.exists());
        assert!(unrelated.exists());
    }
}
