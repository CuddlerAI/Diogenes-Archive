//! Offline sealing for a CP2077 discovery launched directly with `dm-depth-launch`.
//!
//! This contract is intentionally distinct from the client-managed discovery marker. It binds
//! the exact launcher stdout, configuration, executable/probe identities, and both probe reports
//! without claiming that the DataMiner client owned or observed the process lifecycle.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::raw_depth_demo::{
    CP2077_BUILD_20383525_EXE_SHA256, D3D12_DISCOVERY_REPORT_FILE, D3D12_DISCOVERY_REPORT_SCHEMA_V1,
};

pub const LOCAL_DISCOVERY_SEALED_SCHEMA_V1: &str = "dm-local-cp2077-discovery-report-sealed/v1";
pub const MANUAL_LOCAL_SMOKE_PROVENANCE: &str = "manual_local_smoke";
pub const DISCOVERY_CONFIG_SCHEMA_V1: &str = "dm-cp2077-discovery-smoke-config/v1";
pub const POSE_DISCOVERY_REPORT_SCHEMA_V1: &str = "cp2077-pose-runtime-observation/v1";
pub const POSE_DISCOVERY_REPORT_FILE: &str = "cp2077-pose-runtime-observation.json";

pub type Sha256Hex = fn(&[u8]) -> String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDiscoveryAddressAudit {
    pub absolute_process_addresses_recorded: bool,
    pub d3d12_candidate_ids_opaque: bool,
    pub module_relative_rvas_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDiscoverySealedMarker {
    pub schema: String,
    pub provenance_mode: String,
    pub client_managed: bool,
    pub discovery_only: bool,
    pub recording_ready: bool,
    pub process_launch_id: String,
    pub launched_process_id: u32,
    pub executable_sha256: String,
    pub resident_probe_sha256: String,
    pub launcher_stdout_sha256: String,
    pub discovery_config_sha256: String,
    pub reports: BTreeMap<String, String>,
    pub address_audit: LocalDiscoveryAddressAudit,
}

impl LocalDiscoverySealedMarker {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != LOCAL_DISCOVERY_SEALED_SCHEMA_V1
            || self.provenance_mode != MANUAL_LOCAL_SMOKE_PROVENANCE
            || self.client_managed
            || !self.discovery_only
            || self.recording_ready
        {
            return Err("local discovery provenance/safety ceiling is malformed".into());
        }
        if !is_launch_id(&self.process_launch_id)
            || self.launched_process_id == 0
            || self.executable_sha256 != CP2077_BUILD_20383525_EXE_SHA256
            || !is_sha256(&self.resident_probe_sha256)
            || !is_sha256(&self.launcher_stdout_sha256)
            || !is_sha256(&self.discovery_config_sha256)
        {
            return Err("local discovery identity is malformed".into());
        }
        if self.reports.len() != 2
            || !self
                .reports
                .get(D3D12_DISCOVERY_REPORT_FILE)
                .is_some_and(|hash| is_sha256(hash))
            || !self
                .reports
                .get(POSE_DISCOVERY_REPORT_FILE)
                .is_some_and(|hash| is_sha256(hash))
        {
            return Err("local discovery marker must bind exactly both reports".into());
        }
        if self.address_audit.absolute_process_addresses_recorded
            || !self.address_audit.d3d12_candidate_ids_opaque
            || !self.address_audit.module_relative_rvas_allowed
        {
            return Err("local discovery report address audit did not pass".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalDiscoverySealError {
    MalformedConfig(String),
    MalformedLauncherOutput(String),
    MalformedD3d12Report(String),
    MalformedPoseReport(String),
    WrongExecutable(String),
    ProbeHashMismatch { expected: String, actual: String },
    LaunchIdentityMismatch,
    RawAddressAuditFailed(String),
    MalformedMarker(String),
}

impl core::fmt::Display for LocalDiscoverySealError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "local CP2077 discovery seal refused: {self:?}")
    }
}

impl std::error::Error for LocalDiscoverySealError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryConfig {
    schema: String,
    recording_ready: bool,
    output_dir: String,
}

#[derive(Debug, PartialEq, Eq)]
struct LauncherOutput {
    launched_process_id: u32,
    resident_probe_sha256: String,
    launch_id: String,
}

/// Produces a create-new-ready marker for a direct-launch, local-only discovery session.
///
/// `actual_executable_sha256` and `actual_probe_sha256` must be hashes calculated from the local
/// executable and DLL bytes by the caller. The launcher stdout is bound byte-for-byte in the
/// marker, but paths and account information are never persisted.
pub fn seal_cp2077_local_discovery(
    discovery_config_bytes: &[u8],
    launcher_stdout_bytes: &[u8],
    actual_executable_sha256: &str,
    actual_probe_sha256: &str,
    d3d12_report_bytes: &[u8],
    pose_report_bytes: &[u8],
    sha256_hex: Sha256Hex,
) -> Result<LocalDiscoverySealedMarker, LocalDiscoverySealError> {
    let config: DiscoveryConfig = serde_json::from_slice(discovery_config_bytes)
        .map_err(|error| LocalDiscoverySealError::MalformedConfig(error.to_string()))?;
    if config.schema != DISCOVERY_CONFIG_SCHEMA_V1
        || config.recording_ready
        || config.output_dir != "."
    {
        return Err(LocalDiscoverySealError::MalformedConfig(
            "config is not the relative-output, discovery-only v1 contract".into(),
        ));
    }

    if actual_executable_sha256 != CP2077_BUILD_20383525_EXE_SHA256 {
        return Err(LocalDiscoverySealError::WrongExecutable(
            actual_executable_sha256.into(),
        ));
    }
    if !is_sha256(actual_probe_sha256) {
        return Err(LocalDiscoverySealError::ProbeHashMismatch {
            expected: "a lowercase sha256:<64 hex> digest".into(),
            actual: actual_probe_sha256.into(),
        });
    }

    let launcher = parse_launcher_output(launcher_stdout_bytes)?;
    if launcher.resident_probe_sha256 != actual_probe_sha256 {
        return Err(LocalDiscoverySealError::ProbeHashMismatch {
            expected: actual_probe_sha256.into(),
            actual: launcher.resident_probe_sha256,
        });
    }

    let d3d12: serde_json::Value = serde_json::from_slice(d3d12_report_bytes)
        .map_err(|error| LocalDiscoverySealError::MalformedD3d12Report(error.to_string()))?;
    validate_report_identity(
        &d3d12,
        D3D12_DISCOVERY_REPORT_SCHEMA_V1,
        &launcher.launch_id,
    )
    .map_err(LocalDiscoverySealError::MalformedD3d12Report)?;
    audit_d3d12_report(&d3d12)?;

    let pose: serde_json::Value = serde_json::from_slice(pose_report_bytes)
        .map_err(|error| LocalDiscoverySealError::MalformedPoseReport(error.to_string()))?;
    validate_report_identity(&pose, POSE_DISCOVERY_REPORT_SCHEMA_V1, &launcher.launch_id)
        .map_err(LocalDiscoverySealError::MalformedPoseReport)?;
    audit_pose_report(&pose, actual_executable_sha256)?;

    let marker = LocalDiscoverySealedMarker {
        schema: LOCAL_DISCOVERY_SEALED_SCHEMA_V1.into(),
        provenance_mode: MANUAL_LOCAL_SMOKE_PROVENANCE.into(),
        client_managed: false,
        discovery_only: true,
        recording_ready: false,
        process_launch_id: launcher.launch_id,
        launched_process_id: launcher.launched_process_id,
        executable_sha256: actual_executable_sha256.into(),
        resident_probe_sha256: actual_probe_sha256.into(),
        launcher_stdout_sha256: sha256_hex(launcher_stdout_bytes),
        discovery_config_sha256: sha256_hex(discovery_config_bytes),
        reports: BTreeMap::from([
            (
                D3D12_DISCOVERY_REPORT_FILE.into(),
                sha256_hex(d3d12_report_bytes),
            ),
            (
                POSE_DISCOVERY_REPORT_FILE.into(),
                sha256_hex(pose_report_bytes),
            ),
        ]),
        address_audit: LocalDiscoveryAddressAudit {
            absolute_process_addresses_recorded: false,
            d3d12_candidate_ids_opaque: true,
            module_relative_rvas_allowed: true,
        },
    };
    marker
        .validate()
        .map_err(LocalDiscoverySealError::MalformedMarker)?;
    Ok(marker)
}

fn parse_launcher_output(bytes: &[u8]) -> Result<LauncherOutput, LocalDiscoverySealError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        LocalDiscoverySealError::MalformedLauncherOutput("stdout is not UTF-8".into())
    })?;
    let mut values = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line.split_once('=').ok_or_else(|| {
            LocalDiscoverySealError::MalformedLauncherOutput("line has no '=' separator".into())
        })?;
        if !matches!(key, "launched_pid" | "resident_probe_sha256" | "launch_id")
            || value.trim() != value
            || values.insert(key, value).is_some()
        {
            return Err(LocalDiscoverySealError::MalformedLauncherOutput(
                "stdout has an unknown, duplicated, or non-canonical field".into(),
            ));
        }
    }
    if values.len() != 3 {
        return Err(LocalDiscoverySealError::MalformedLauncherOutput(
            "stdout must contain exactly the three launcher status fields".into(),
        ));
    }
    let launched_process_id = values["launched_pid"]
        .parse::<u32>()
        .map_err(|_| LocalDiscoverySealError::MalformedLauncherOutput("PID is invalid".into()))?;
    let resident_probe_sha256 = values["resident_probe_sha256"].to_owned();
    let launch_id = values["launch_id"].to_owned();
    if launched_process_id == 0 || !is_sha256(&resident_probe_sha256) || !is_launch_id(&launch_id) {
        return Err(LocalDiscoverySealError::MalformedLauncherOutput(
            "PID, resident probe digest, or launch identity is malformed".into(),
        ));
    }
    Ok(LauncherOutput {
        launched_process_id,
        resident_probe_sha256,
        launch_id,
    })
}

fn validate_report_identity(
    report: &serde_json::Value,
    expected_schema: &str,
    expected_launch_id: &str,
) -> Result<(), String> {
    if report.get("schema").and_then(serde_json::Value::as_str) != Some(expected_schema)
        || report
            .get("process_launch_id")
            .and_then(serde_json::Value::as_str)
            != Some(expected_launch_id)
        || report
            .get("recording_ready")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
    {
        return Err("schema, launch identity, or discovery-only readiness does not match".into());
    }
    Ok(())
}

fn audit_d3d12_report(report: &serde_json::Value) -> Result<(), LocalDiscoverySealError> {
    reject_live_address_keys(report, "$d3d12")?;
    let candidates = report
        .pointer("/observation/depth_candidates")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            LocalDiscoverySealError::RawAddressAuditFailed(
                "D3D12 report has no candidate array".into(),
            )
        })?;
    let mut unique = BTreeSet::new();
    for candidate in candidates {
        let candidate_id = candidate
            .get("candidate_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                LocalDiscoverySealError::RawAddressAuditFailed(
                    "D3D12 candidate has no opaque ID".into(),
                )
            })?;
        if !is_opaque_candidate_id(candidate_id) || !unique.insert(candidate_id) {
            return Err(LocalDiscoverySealError::RawAddressAuditFailed(
                "D3D12 candidate ID is non-opaque or duplicated".into(),
            ));
        }
    }
    Ok(())
}

fn audit_pose_report(
    report: &serde_json::Value,
    executable_sha256: &str,
) -> Result<(), LocalDiscoverySealError> {
    reject_live_address_keys(report, "$pose")?;
    if report
        .get("absolute_process_addresses_recorded")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
        || report
            .get("production_binding_emitted")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        || report
            .pointer("/exact_build/executable_sha256")
            .and_then(serde_json::Value::as_str)
            != Some(executable_sha256)
    {
        return Err(LocalDiscoverySealError::RawAddressAuditFailed(
            "pose report records a live address, emits a production binding, or has the wrong build"
                .into(),
        ));
    }
    Ok(())
}

/// Rejects fields whose names would carry live process/GPU/descriptor addresses. Module-relative
/// `rva` fields and the PE catalog's preferred image base are deliberately allowed: neither is an
/// observed runtime address and both are needed to audit the exact-build ABI gate.
fn reject_live_address_keys(
    value: &serde_json::Value,
    path: &str,
) -> Result<(), LocalDiscoverySealError> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                let forbidden = lower.ends_with("_ptr")
                    || lower.ends_with("_pointer")
                    || matches!(
                        lower.as_str(),
                        "absolute_address"
                            | "process_address"
                            | "object_address"
                            | "instance_address"
                            | "resource_address"
                            | "gpu_virtual_address"
                            | "cpu_descriptor_handle"
                            | "gpu_descriptor_handle"
                            | "native_handle"
                    );
                if forbidden {
                    return Err(LocalDiscoverySealError::RawAddressAuditFailed(format!(
                        "live address field found at {path}.{key}"
                    )));
                }
                reject_live_address_keys(child, &format!("{path}.{key}"))?;
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                reject_live_address_keys(child, &format!("{path}[{index}]"))?;
            }
        }
        serde_json::Value::String(text) if contains_live_address_literal(text) => {
            return Err(LocalDiscoverySealError::RawAddressAuditFailed(format!(
                "live address literal found at {path}"
            )));
        }
        _ => {}
    }
    Ok(())
}

fn contains_live_address_literal(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 2 <= bytes.len() {
        if bytes[index] == b'0' && matches!(bytes[index + 1], b'x' | b'X') {
            let start = index + 2;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if end.saturating_sub(start) >= 12 {
                return true;
            }
            index = end;
        } else {
            index += 1;
        }
    }
    false
}

fn is_opaque_candidate_id(value: &str) -> bool {
    value.len() <= 64
        && value.starts_with("dsv-r")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.contains("0x")
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(crate) fn is_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCH: &str = "0123456789abcdef0123456789abcdef";
    const PROBE: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn fake_sha(bytes: &[u8]) -> String {
        format!("sha256:{:064x}", bytes.len())
    }

    fn config() -> Vec<u8> {
        br#"{"schema":"dm-cp2077-discovery-smoke-config/v1","recording_ready":false,"output_dir":"."}"#.to_vec()
    }

    fn launcher() -> Vec<u8> {
        format!("launched_pid=735\nresident_probe_sha256={PROBE}\nlaunch_id={LAUNCH}\n")
            .into_bytes()
    }

    fn d3d12() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": D3D12_DISCOVERY_REPORT_SCHEMA_V1,
            "process_launch_id": LAUNCH,
            "recording_ready": false,
            "observation": { "depth_candidates": [{ "candidate_id": "dsv-r31-g1-e1" }] }
        }))
        .unwrap()
    }

    fn pose() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": POSE_DISCOVERY_REPORT_SCHEMA_V1,
            "process_launch_id": LAUNCH,
            "recording_ready": false,
            "exact_build": { "executable_sha256": CP2077_BUILD_20383525_EXE_SHA256 },
            "address_catalog": {
                "preferred_load_address": "0000000140000000",
                "retained_symbols": [{ "rva": 10569004 }]
            },
            "absolute_process_addresses_recorded": false,
            "production_binding_emitted": false
        }))
        .unwrap()
    }

    #[test]
    fn direct_launcher_evidence_is_sealed_without_claiming_client_ownership() {
        let marker = seal_cp2077_local_discovery(
            &config(),
            &launcher(),
            CP2077_BUILD_20383525_EXE_SHA256,
            PROBE,
            &d3d12(),
            &pose(),
            fake_sha,
        )
        .unwrap();
        marker.validate().unwrap();
        assert_eq!(marker.provenance_mode, MANUAL_LOCAL_SMOKE_PROVENANCE);
        assert!(!marker.client_managed);
        assert!(!marker.recording_ready);
        assert_eq!(marker.reports.len(), 2);
    }

    #[test]
    fn launcher_probe_hash_must_match_the_actual_dll_bytes_hash() {
        let error = seal_cp2077_local_discovery(
            &config(),
            &launcher(),
            CP2077_BUILD_20383525_EXE_SHA256,
            &format!("sha256:{}", "2".repeat(64)),
            &d3d12(),
            &pose(),
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LocalDiscoverySealError::ProbeHashMismatch { .. }
        ));
    }

    #[test]
    fn cross_launch_report_is_refused() {
        let mut report: serde_json::Value = serde_json::from_slice(&d3d12()).unwrap();
        report["process_launch_id"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        let error = seal_cp2077_local_discovery(
            &config(),
            &launcher(),
            CP2077_BUILD_20383525_EXE_SHA256,
            PROBE,
            &serde_json::to_vec(&report).unwrap(),
            &pose(),
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LocalDiscoverySealError::MalformedD3d12Report(_)
        ));
    }

    #[test]
    fn pose_report_with_absolute_process_address_is_refused() {
        let mut report: serde_json::Value = serde_json::from_slice(&pose()).unwrap();
        report["absolute_process_addresses_recorded"] = true.into();
        let error = seal_cp2077_local_discovery(
            &config(),
            &launcher(),
            CP2077_BUILD_20383525_EXE_SHA256,
            PROBE,
            &d3d12(),
            &serde_json::to_vec(&report).unwrap(),
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LocalDiscoverySealError::RawAddressAuditFailed(_)
        ));
    }

    #[test]
    fn a_live_pointer_field_is_refused_even_when_the_summary_flag_is_false() {
        let mut report: serde_json::Value = serde_json::from_slice(&pose()).unwrap();
        report["player_ptr"] = "0x00000123456789ab".into();
        let error = seal_cp2077_local_discovery(
            &config(),
            &launcher(),
            CP2077_BUILD_20383525_EXE_SHA256,
            PROBE,
            &d3d12(),
            &serde_json::to_vec(&report).unwrap(),
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LocalDiscoverySealError::RawAddressAuditFailed(_)
        ));
    }

    #[test]
    fn a_live_address_embedded_in_diagnostic_text_is_refused() {
        let mut report: serde_json::Value = serde_json::from_slice(&pose()).unwrap();
        report["blocker"] = "resource 0x00007ffdeadc0ffe could not be read".into();
        let error = seal_cp2077_local_discovery(
            &config(),
            &launcher(),
            CP2077_BUILD_20383525_EXE_SHA256,
            PROBE,
            &d3d12(),
            &serde_json::to_vec(&report).unwrap(),
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            LocalDiscoverySealError::RawAddressAuditFailed(_)
        ));
    }
}
