//! Depth probe runtime identity and generation-scoped performance certification.
//!
//! This is deliberately a small, policy-free contract:
//!
//! - the packaged runtime manifest binds the exact DLL bytes to one runtime version and two
//!   independently advancing backend generations;
//! - the signed central manifest allow-lists exact runtime identities and performance evidence;
//! - performance evidence matches a backend generation and an explicit GPU model allow-list;
//! - the local adapter LUID and driver are retained only as audit evidence in a candidate.
//!
//! There is intentionally no parser for the retired D3D11 v2 / D3D12 v3 stores here.  Those
//! schemas are not predecessors of this contract; they are incompatible data that the client
//! removes by exact filename during upgrade.

use serde::{Deserialize, Serialize};

use crate::RendererBackend;

pub const DEPTH_PROBE_RUNTIME_MANIFEST_SCHEMA_V1: &str = "dm-depth-probe-runtime-manifest-v1";
pub const DEPTH_PERF_CERTIFICATIONS_SCHEMA_V3: &str = "dm-depth-performance-certifications-v3";
pub const DEPTH_PERF_CERTIFICATIONS_SCHEMA_V2: &str = "dm-depth-performance-certifications-v2";
pub const DEPTH_PERF_CERTIFICATIONS_SCHEMA_V1: &str = "dm-depth-performance-certifications-v1";
pub const DEPTH_PERF_CERTIFICATION_CANDIDATE_SCHEMA_V1: &str =
    "dm-depth-performance-certification-candidate-v1";

/// Exact identity shipped beside `dm_depth_probe.dll`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepthProbeRuntimeManifest {
    pub schema: String,
    pub runtime_version: String,
    pub dll_sha256: String,
    pub d3d11_generation: u64,
    pub d3d12_generation: u64,
}

impl DepthProbeRuntimeManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != DEPTH_PROBE_RUNTIME_MANIFEST_SCHEMA_V1 {
            return Err(format!(
                "unsupported depth probe runtime schema: {}",
                self.schema
            ));
        }
        if self.runtime_version.trim().is_empty() {
            return Err("depth probe runtime version must not be empty".into());
        }
        validate_sha256(&self.dll_sha256, "depth probe DLL")?;
        if self.d3d11_generation == 0 || self.d3d12_generation == 0 {
            return Err("D3D generations must be positive".into());
        }
        Ok(())
    }

    #[must_use]
    pub const fn generation_for(&self, backend: RendererBackend) -> u64 {
        match backend {
            RendererBackend::D3d11 => self.d3d11_generation,
            RendererBackend::D3d12 => self.d3d12_generation,
        }
    }

    #[must_use]
    pub fn matches_runtime(&self, approved: &ApprovedProbeRuntime) -> bool {
        self.runtime_version.trim() == approved.runtime_version.trim()
            && sha256_eq(&self.dll_sha256, &approved.dll_sha256)
            && self.d3d11_generation == approved.d3d11_generation
            && self.d3d12_generation == approved.d3d12_generation
    }
}

/// One exact probe DLL approved by the signed central manifest. Multiple SHA values may carry
/// the same pair of reviewed generations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovedProbeRuntime {
    pub runtime_version: String,
    pub dll_sha256: String,
    pub d3d11_generation: u64,
    pub d3d12_generation: u64,
}

impl ApprovedProbeRuntime {
    fn validate(&self) -> Result<(), String> {
        if self.runtime_version.trim().is_empty() {
            return Err("approved runtime version must not be empty".into());
        }
        validate_sha256(&self.dll_sha256, "approved depth probe DLL")?;
        if self.d3d11_generation == 0 || self.d3d12_generation == 0 {
            return Err("approved D3D generations must be positive".into());
        }
        Ok(())
    }

    #[must_use]
    pub const fn generation_for(&self, backend: RendererBackend) -> u64 {
        match backend {
            RendererBackend::D3d11 => self.d3d11_generation,
            RendererBackend::D3d12 => self.d3d12_generation,
        }
    }
}

/// Fields that can materially change the measured capture cost. DLL SHA and host identity do not
/// belong here: the former is an integrity/audit identity and the latter is handled by an explicit
/// reviewed hardware coverage list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerfCertificationKey {
    pub game_id: String,
    pub renderer_backend: RendererBackend,
    pub backend_generation: u64,
    pub depth_profile_sha256: String,
    pub camera_binding_sha256: String,
    pub selected_present_mode: String,
    pub unbind_from_end: u32,
    pub feedback_schema: u16,
    pub queue_capacity: u32,
    pub ring_capacity: u32,
    pub max_feedback_lag_presents: u32,
    pub guard_slots: u32,
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
    pub vram_limit_bytes: u64,
    pub vram_reserve_bytes: u64,
}

impl PerfCertificationKey {
    pub fn validate(&self) -> Result<(), String> {
        if self.game_id.trim().is_empty() {
            return Err("performance certification game id must not be empty".into());
        }
        if self.backend_generation == 0 {
            return Err("performance certification backend generation must be positive".into());
        }
        validate_sha256(&self.depth_profile_sha256, "depth profile")?;
        validate_sha256(&self.camera_binding_sha256, "camera binding")?;
        if self.selected_present_mode.trim().is_empty() {
            return Err("selected present mode must not be empty".into());
        }
        if self.feedback_schema == 0
            || self.queue_capacity == 0
            || self.ring_capacity == 0
            || self.width == 0
            || self.height == 0
            || self.bytes_per_pixel == 0
            || self.vram_limit_bytes == 0
        {
            return Err(
                "performance certification dimensions and capacities must be positive".into(),
            );
        }
        if self.vram_reserve_bytes >= self.vram_limit_bytes {
            return Err("performance certification VRAM reserve must be below the limit".into());
        }
        Ok(())
    }

    #[must_use]
    pub fn matches(&self, actual: &Self) -> bool {
        self.game_id == actual.game_id
            && self.renderer_backend == actual.renderer_backend
            && self.backend_generation == actual.backend_generation
            && sha256_eq(&self.depth_profile_sha256, &actual.depth_profile_sha256)
            && sha256_eq(&self.camera_binding_sha256, &actual.camera_binding_sha256)
            && self.selected_present_mode == actual.selected_present_mode
            && self.unbind_from_end == actual.unbind_from_end
            && self.feedback_schema == actual.feedback_schema
            && self.queue_capacity == actual.queue_capacity
            && self.ring_capacity == actual.ring_capacity
            && self.max_feedback_lag_presents == actual.max_feedback_lag_presents
            && self.guard_slots == actual.guard_slots
            && self.width == actual.width
            && self.height == actual.height
            && self.bytes_per_pixel == actual.bytes_per_pixel
            && self.vram_limit_bytes == actual.vram_limit_bytes
            && self.vram_reserve_bytes == actual.vram_reserve_bytes
    }
}

/// Exact host evidence retained for audit and candidate authoring. It is intentionally not a
/// reusable certification key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerfHardwareIdentity {
    pub gpu_model: String,
    pub gpu_driver_version: String,
    pub gpu_adapter_luid_high: i32,
    pub gpu_adapter_luid_low: u32,
}

impl PerfHardwareIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if normalize_gpu_model(&self.gpu_model).is_empty() {
            return Err("GPU model must not be empty".into());
        }
        if self.gpu_driver_version.trim().is_empty() {
            return Err("GPU driver version must not be empty".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerfCertification {
    pub certification_id: String,
    pub key: PerfCertificationKey,
    /// Exact runtime used for the baseline measurement. This is audit provenance, not part of
    /// applicability: another approved SHA may reuse the evidence when its backend generation is
    /// unchanged.
    pub measured_runtime: ApprovedProbeRuntime,
    pub baseline_hardware: PerfHardwareIdentity,
    /// Exact, manually reviewed DXGI model strings. No ranking or fuzzy family matching.
    pub covered_gpu_models: Vec<String>,
    pub avg_fps_drop: f64,
    pub low1_fps_drop: f64,
    pub measured_on: String,
}

impl PerfCertification {
    #[must_use]
    pub fn passes_thresholds(&self) -> bool {
        -super::integration::MAX_AVG_FPS_GAIN <= self.avg_fps_drop
            && self.avg_fps_drop <= super::integration::MAX_AVG_FPS_DROP
            && -super::integration::MAX_LOW1_FPS_GAIN <= self.low1_fps_drop
            && self.low1_fps_drop <= super::integration::MAX_LOW1_FPS_DROP
    }

    fn validate(&self) -> Result<(), String> {
        if self.certification_id.trim().is_empty() {
            return Err("performance certification id must not be empty".into());
        }
        self.key.validate()?;
        self.measured_runtime.validate()?;
        if self.key.backend_generation
            != self
                .measured_runtime
                .generation_for(self.key.renderer_backend)
        {
            return Err(
                "measured runtime generation does not match the certification backend key".into(),
            );
        }
        self.baseline_hardware.validate()?;
        if self.measured_on.trim().is_empty() {
            return Err("performance certification measured_on must not be empty".into());
        }
        if !self.avg_fps_drop.is_finite() || !self.low1_fps_drop.is_finite() {
            return Err("performance certification measurements must be finite".into());
        }
        if !self.passes_thresholds() {
            return Err(format!(
                "performance certification {} does not pass the FPS thresholds",
                self.certification_id
            ));
        }
        let normalized = self
            .covered_gpu_models
            .iter()
            .map(|model| normalize_gpu_model(model))
            .collect::<Vec<_>>();
        if normalized.is_empty() || normalized.iter().any(String::is_empty) {
            return Err("hardware coverage list must contain explicit non-empty GPU models".into());
        }
        let mut unique = std::collections::BTreeSet::new();
        if normalized.iter().any(|model| !unique.insert(model)) {
            return Err("hardware coverage list contains duplicate normalized GPU models".into());
        }
        let baseline = normalize_gpu_model(&self.baseline_hardware.gpu_model);
        if !unique.contains(&baseline) {
            return Err(
                "baseline GPU must be present in the explicit hardware coverage list".into(),
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn covers(&self, key: &PerfCertificationKey, gpu_model: &str) -> bool {
        self.key.matches(key)
            && self
                .covered_gpu_models
                .iter()
                .any(|covered| normalize_gpu_model(covered) == normalize_gpu_model(gpu_model))
            && self.passes_thresholds()
    }
}

/// Explicit signed reuse authorization. Hashes bind the exact catalog requirements;
/// no GPU name ranking or machine identifier participates in this decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceProfileGrant {
    pub certification_id: String,
    pub device_profile_sha256: String,
    pub recording_profile_sha256: String,
    pub required_present_fps: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct DeviceProfileContext<'a> {
    pub device_profile_sha256: &'a str,
    pub recording_profile_sha256: &'a str,
}

pub struct MatchedCertification<'a> {
    pub certification: &'a PerfCertification,
    pub device_profile_grant: Option<&'a DeviceProfileGrant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepthPerfCertificationManifest {
    pub schema: String,
    pub approved_runtimes: Vec<ApprovedProbeRuntime>,
    pub certifications: Vec<PerfCertification>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_profile_grants: Vec<DeviceProfileGrant>,
}

impl DepthPerfCertificationManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != DEPTH_PERF_CERTIFICATIONS_SCHEMA_V1
            && self.schema != DEPTH_PERF_CERTIFICATIONS_SCHEMA_V2
            && self.schema != DEPTH_PERF_CERTIFICATIONS_SCHEMA_V3
        {
            return Err(format!(
                "unsupported depth performance certification schema: {}",
                self.schema
            ));
        }
        if self.schema == DEPTH_PERF_CERTIFICATIONS_SCHEMA_V1
            && !self.device_profile_grants.is_empty()
        {
            return Err("v1 manifest cannot authorize device-profile reuse".into());
        }
        let mut grants = std::collections::BTreeSet::new();
        for grant in &self.device_profile_grants {
            validate_sha256(&grant.device_profile_sha256, "device profile")?;
            validate_sha256(&grant.recording_profile_sha256, "recording profile")?;
            if !matches!(grant.required_present_fps, 30 | 60) {
                return Err(
                    "device-profile reuse requires an explicit 30/60 Present FPS contract".into(),
                );
            }
            if !self
                .certifications
                .iter()
                .any(|c| c.certification_id == grant.certification_id)
            {
                return Err(
                    "device-profile grant references a missing baseline certification".into(),
                );
            }
            if !grants.insert((
                &grant.certification_id,
                &grant.device_profile_sha256,
                &grant.recording_profile_sha256,
            )) {
                return Err("duplicate device-profile grant".into());
            }
        }
        let mut runtime_sha = std::collections::BTreeSet::new();
        for runtime in &self.approved_runtimes {
            runtime.validate()?;
            let sha = runtime.dll_sha256.trim().to_ascii_lowercase();
            if !runtime_sha.insert(sha) {
                return Err("approved runtime list contains a duplicate DLL SHA-256".into());
            }
        }
        let mut certification_ids = std::collections::BTreeSet::new();
        for certification in &self.certifications {
            certification.validate()?;
            if !certification_ids.insert(certification.certification_id.trim()) {
                return Err("performance certification ids must be unique".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerfCertificationCandidate {
    pub schema: String,
    pub candidate_id: String,
    pub runtime: DepthProbeRuntimeManifest,
    pub key: PerfCertificationKey,
    pub hardware: PerfHardwareIdentity,
    pub avg_fps_drop: f64,
    pub low1_fps_drop: f64,
    pub measured_on: String,
}

impl PerfCertificationCandidate {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != DEPTH_PERF_CERTIFICATION_CANDIDATE_SCHEMA_V1 {
            return Err(format!(
                "unsupported certification candidate schema: {}",
                self.schema
            ));
        }
        if self.candidate_id.trim().is_empty() {
            return Err("performance certification candidate id must not be empty".into());
        }
        if !self
            .candidate_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("performance certification candidate id contains unsafe characters".into());
        }
        self.runtime.validate()?;
        self.key.validate()?;
        self.hardware.validate()?;
        if self.key.backend_generation != self.runtime.generation_for(self.key.renderer_backend) {
            return Err("candidate backend generation does not match its runtime manifest".into());
        }
        if !self.avg_fps_drop.is_finite() || !self.low1_fps_drop.is_finite() {
            return Err("candidate measurements must be finite".into());
        }
        if self.measured_on.trim().is_empty() {
            return Err("candidate measured_on must not be empty".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerfCertificationDenial {
    InvalidRuntime(String),
    DllHashMismatch,
    InvalidCentralManifest(String),
    RuntimeNotApproved,
    BackendGenerationMismatch,
    HardwareNotCovered,
    CertificationNotFound,
}

/// Evaluate the complete generation-based gate. SHA validates bytes and signed allow-list
/// membership; only the selected backend generation invalidates performance evidence.
pub fn evaluate_perf_certification<'a>(
    manifest: &'a DepthPerfCertificationManifest,
    runtime: &DepthProbeRuntimeManifest,
    actual_dll_sha256: &str,
    key: &PerfCertificationKey,
    hardware: &PerfHardwareIdentity,
) -> Result<&'a PerfCertification, PerfCertificationDenial> {
    runtime
        .validate()
        .map_err(PerfCertificationDenial::InvalidRuntime)?;
    if !sha256_eq(&runtime.dll_sha256, actual_dll_sha256) {
        return Err(PerfCertificationDenial::DllHashMismatch);
    }
    manifest
        .validate()
        .map_err(PerfCertificationDenial::InvalidCentralManifest)?;
    if !manifest
        .approved_runtimes
        .iter()
        .any(|approved| runtime.matches_runtime(approved))
    {
        return Err(PerfCertificationDenial::RuntimeNotApproved);
    }
    if key.backend_generation != runtime.generation_for(key.renderer_backend) {
        return Err(PerfCertificationDenial::BackendGenerationMismatch);
    }
    let mut matching_key_found = false;
    for certification in &manifest.certifications {
        if certification.key.matches(key) {
            matching_key_found = true;
            if certification.covers(key, &hardware.gpu_model) {
                return Ok(certification);
            }
        }
    }
    if matching_key_found {
        return Err(PerfCertificationDenial::HardwareNotCovered);
    }
    Err(PerfCertificationDenial::CertificationNotFound)
}

/// The caller must validate the referenced signed device requirements against fresh hardware
/// evidence and enforce the returned Present-rate contract before recording begins.
pub fn evaluate_device_profile_certification<'a>(
    manifest: &'a DepthPerfCertificationManifest,
    runtime: &DepthProbeRuntimeManifest,
    actual_dll_sha256: &str,
    key: &PerfCertificationKey,
    hardware: &PerfHardwareIdentity,
    context: DeviceProfileContext<'_>,
) -> Result<MatchedCertification<'a>, PerfCertificationDenial> {
    let legacy = evaluate_perf_certification(manifest, runtime, actual_dll_sha256, key, hardware);
    match &legacy {
        Ok(_) | Err(PerfCertificationDenial::HardwareNotCovered) => {}
        Err(error) => return Err(error.clone()),
    }
    // Prefer explicit profile reuse even on the baseline GPU: its runtime conditions
    // apply uniformly and cannot be bypassed by also appearing in the legacy list.
    for grant in &manifest.device_profile_grants {
        if sha256_eq(&grant.device_profile_sha256, context.device_profile_sha256)
            && sha256_eq(
                &grant.recording_profile_sha256,
                context.recording_profile_sha256,
            )
            && let Some(certification) = manifest
                .certifications
                .iter()
                .find(|c| c.certification_id == grant.certification_id && c.key.matches(key))
        {
            return Ok(MatchedCertification {
                certification,
                device_profile_grant: Some(grant),
            });
        }
    }
    legacy.map(|certification| MatchedCertification {
        certification,
        device_profile_grant: None,
    })
}

#[must_use]
pub fn normalize_gpu_model(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_sha256(value: &str, label: &str) -> Result<(), String> {
    let Some(hex) = value.trim().strip_prefix("sha256:") else {
        return Err(format!(
            "{label} SHA-256 must use sha256:<64 lowercase hex>"
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(format!(
            "{label} SHA-256 must use sha256:<64 lowercase hex>"
        ));
    }
    Ok(())
}

fn sha256_eq(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_grant_reuses_baseline_without_gpu_name_or_machine_identity() {
        let active = runtime(3, 7, 'a');
        let key = key(RendererBackend::D3d12, 7);
        let baseline = certification(key.clone(), &active);
        let id = baseline.certification_id.clone();
        let mut central = manifest(&active, vec![baseline]);
        central.schema = DEPTH_PERF_CERTIFICATIONS_SCHEMA_V2.into();
        let device = sha('d');
        let recording = sha('e');
        central.device_profile_grants.push(DeviceProfileGrant {
            certification_id: id,
            device_profile_sha256: device.clone(),
            recording_profile_sha256: recording.clone(),
            required_present_fps: 30,
        });
        let context = DeviceProfileContext {
            device_profile_sha256: &device,
            recording_profile_sha256: &recording,
        };
        let gpu = hardware("A new independently checked GPU");
        assert_eq!(
            evaluate_perf_certification(&central, &active, &active.dll_sha256, &key, &gpu),
            Err(PerfCertificationDenial::HardwareNotCovered)
        );
        let matched = evaluate_device_profile_certification(
            &central,
            &active,
            &active.dll_sha256,
            &key,
            &gpu,
            context,
        )
        .unwrap();
        assert_eq!(
            matched.device_profile_grant.unwrap().required_present_fps,
            30
        );
        // Baseline hardware also receives the required local check, never an exact-list bypass.
        let baseline_gpu = &central.certifications[0].baseline_hardware;
        assert!(
            evaluate_device_profile_certification(
                &central,
                &active,
                &active.dll_sha256,
                &key,
                baseline_gpu,
                context
            )
            .unwrap()
            .device_profile_grant
            .is_some()
        );
        let changed = sha('f');
        assert!(
            evaluate_device_profile_certification(
                &central,
                &active,
                &active.dll_sha256,
                &key,
                &gpu,
                DeviceProfileContext {
                    device_profile_sha256: &changed,
                    ..context
                }
            )
            .is_err()
        );
        assert!(
            evaluate_device_profile_certification(
                &central,
                &active,
                &active.dll_sha256,
                &key,
                &gpu,
                DeviceProfileContext {
                    recording_profile_sha256: &changed,
                    ..context
                }
            )
            .is_err()
        );
        let mut wrong_key = key.clone();
        wrong_key.backend_generation = 8;
        assert!(
            evaluate_device_profile_certification(
                &central,
                &active,
                &active.dll_sha256,
                &wrong_key,
                &gpu,
                context
            )
            .is_err()
        );
        assert!(
            evaluate_device_profile_certification(&central, &active, &changed, &key, &gpu, context)
                .is_err()
        );
        central.schema = DEPTH_PERF_CERTIFICATIONS_SCHEMA_V1.into();
        assert!(central.validate().is_err());
        central.schema = DEPTH_PERF_CERTIFICATIONS_SCHEMA_V2.into();
        central.device_profile_grants[0].certification_id = "missing".into();
        assert!(central.validate().is_err());
    }

    #[test]
    fn legacy_manifest_keeps_its_original_wire_shape_and_coverage() {
        let active = runtime(3, 7, 'a');
        let key = key(RendererBackend::D3d12, 7);
        let central = manifest(&active, vec![certification(key.clone(), &active)]);
        let bytes = serde_json::to_string(&central).unwrap();
        assert!(!bytes.contains("deviceProfileGrants"));
        let parsed: DepthPerfCertificationManifest = serde_json::from_str(&bytes).unwrap();
        assert_eq!(parsed, central);
        assert!(parsed.validate().is_ok());
        assert!(
            evaluate_perf_certification(
                &parsed,
                &active,
                &active.dll_sha256,
                &key,
                &hardware("Unknown GPU")
            )
            .is_err()
        );
    }

    fn sha(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn runtime(d3d11: u64, d3d12: u64, dll: char) -> DepthProbeRuntimeManifest {
        DepthProbeRuntimeManifest {
            schema: DEPTH_PROBE_RUNTIME_MANIFEST_SCHEMA_V1.into(),
            runtime_version: "depth-2026.08.1".into(),
            dll_sha256: sha(dll),
            d3d11_generation: d3d11,
            d3d12_generation: d3d12,
        }
    }

    fn key(backend: RendererBackend, generation: u64) -> PerfCertificationKey {
        PerfCertificationKey {
            game_id: "fallout4-steam-release-test".into(),
            renderer_backend: backend,
            backend_generation: generation,
            depth_profile_sha256: sha('b'),
            camera_binding_sha256: sha('c'),
            selected_present_mode: "wgc_selected_present_gpu_ring_v1".into(),
            unbind_from_end: 4,
            feedback_schema: 3,
            queue_capacity: 256,
            ring_capacity: 96,
            max_feedback_lag_presents: 88,
            guard_slots: 8,
            width: 1280,
            height: 720,
            bytes_per_pixel: 4,
            vram_limit_bytes: 512 * 1024 * 1024,
            vram_reserve_bytes: 256 * 1024 * 1024,
        }
    }

    fn hardware(model: &str) -> PerfHardwareIdentity {
        PerfHardwareIdentity {
            gpu_model: model.into(),
            gpu_driver_version: "32.0.15.1234".into(),
            gpu_adapter_luid_high: 7,
            gpu_adapter_luid_low: 42,
        }
    }

    fn manifest(
        runtime: &DepthProbeRuntimeManifest,
        certifications: Vec<PerfCertification>,
    ) -> DepthPerfCertificationManifest {
        DepthPerfCertificationManifest {
            schema: DEPTH_PERF_CERTIFICATIONS_SCHEMA_V1.into(),
            approved_runtimes: vec![ApprovedProbeRuntime {
                runtime_version: runtime.runtime_version.clone(),
                dll_sha256: runtime.dll_sha256.clone(),
                d3d11_generation: runtime.d3d11_generation,
                d3d12_generation: runtime.d3d12_generation,
            }],
            certifications,
            device_profile_grants: Vec::new(),
        }
    }

    fn certification(
        key: PerfCertificationKey,
        measured_runtime: &DepthProbeRuntimeManifest,
    ) -> PerfCertification {
        PerfCertification {
            certification_id: "fo4-d3d11-g4-low-spec-20260828".into(),
            key,
            measured_runtime: ApprovedProbeRuntime {
                runtime_version: measured_runtime.runtime_version.clone(),
                dll_sha256: measured_runtime.dll_sha256.clone(),
                d3d11_generation: measured_runtime.d3d11_generation,
                d3d12_generation: measured_runtime.d3d12_generation,
            },
            baseline_hardware: hardware("NVIDIA GeForce GTX 1060 6GB"),
            covered_gpu_models: vec![
                "NVIDIA GeForce GTX 1060 6GB".into(),
                "NVIDIA GeForce RTX 2060".into(),
            ],
            avg_fps_drop: 0.01,
            low1_fps_drop: 0.02,
            measured_on: "2026-08-28".into(),
        }
    }

    #[test]
    fn another_approved_sha_with_the_same_generations_reuses_certification() {
        let first = runtime(4, 9, 'a');
        let second = runtime(4, 9, 'd');
        let key = key(RendererBackend::D3d11, 4);
        let mut central = manifest(&first, vec![certification(key.clone(), &first)]);
        central.approved_runtimes.push(ApprovedProbeRuntime {
            runtime_version: second.runtime_version.clone(),
            dll_sha256: second.dll_sha256.clone(),
            d3d11_generation: 4,
            d3d12_generation: 9,
        });

        assert!(
            evaluate_perf_certification(
                &central,
                &second,
                &second.dll_sha256,
                &key,
                &hardware("NVIDIA GeForce RTX 2060"),
            )
            .is_ok()
        );
    }

    #[test]
    fn retired_measured_runtime_remains_audit_provenance_without_staying_approved() {
        let measured = runtime(4, 9, 'a');
        let active = runtime(4, 9, 'd');
        let key = key(RendererBackend::D3d11, 4);
        let gpu = hardware("NVIDIA GeForce GTX 1060 6GB");
        let central = manifest(&active, vec![certification(key.clone(), &measured)]);

        assert!(central.validate().is_ok());
        assert!(
            evaluate_perf_certification(&central, &active, &active.dll_sha256, &key, &gpu).is_ok()
        );
        assert_eq!(
            evaluate_perf_certification(&central, &measured, &measured.dll_sha256, &key, &gpu,),
            Err(PerfCertificationDenial::RuntimeNotApproved)
        );
    }

    #[test]
    fn d3d12_generation_bump_does_not_invalidate_d3d11() {
        let old = runtime(4, 9, 'a');
        let next = runtime(4, 10, 'd');
        let d3d11 = key(RendererBackend::D3d11, 4);
        let d3d12 = key(RendererBackend::D3d12, 9);
        let mut central = manifest(
            &old,
            vec![certification(d3d11.clone(), &old), {
                let mut record = certification(d3d12.clone(), &old);
                record.certification_id = "cp2077-d3d12-g9".into();
                record
            }],
        );
        central.approved_runtimes.push(ApprovedProbeRuntime {
            runtime_version: next.runtime_version.clone(),
            dll_sha256: next.dll_sha256.clone(),
            d3d11_generation: 4,
            d3d12_generation: 10,
        });
        let gpu = hardware("NVIDIA GeForce GTX 1060 6GB");

        assert!(
            evaluate_perf_certification(&central, &next, &next.dll_sha256, &d3d11, &gpu).is_ok()
        );
        assert_eq!(
            evaluate_perf_certification(&central, &next, &next.dll_sha256, &d3d12, &gpu),
            Err(PerfCertificationDenial::BackendGenerationMismatch)
        );
    }

    #[test]
    fn unknown_hardware_unregistered_runtime_and_tampered_dll_fail_closed() {
        let active_runtime = runtime(4, 9, 'a');
        let key = key(RendererBackend::D3d11, 4);
        let central = manifest(
            &active_runtime,
            vec![certification(key.clone(), &active_runtime)],
        );

        assert_eq!(
            evaluate_perf_certification(
                &central,
                &active_runtime,
                &active_runtime.dll_sha256,
                &key,
                &hardware("Unknown GPU")
            ),
            Err(PerfCertificationDenial::HardwareNotCovered)
        );
        let unregistered = runtime(4, 9, 'd');
        assert_eq!(
            evaluate_perf_certification(
                &central,
                &unregistered,
                &unregistered.dll_sha256,
                &key,
                &hardware("NVIDIA GeForce GTX 1060 6GB")
            ),
            Err(PerfCertificationDenial::RuntimeNotApproved)
        );
        assert_eq!(
            evaluate_perf_certification(
                &central,
                &active_runtime,
                &sha('e'),
                &key,
                &hardware("NVIDIA GeForce GTX 1060 6GB")
            ),
            Err(PerfCertificationDenial::DllHashMismatch)
        );
    }

    #[test]
    fn coverage_is_exact_after_whitespace_normalization_and_luid_is_audit_only() {
        let runtime = runtime(4, 9, 'a');
        let key = key(RendererBackend::D3d11, 4);
        let central = manifest(&runtime, vec![certification(key.clone(), &runtime)]);
        let mut covered = hardware("  NVIDIA  GeForce RTX 2060  ");
        covered.gpu_adapter_luid_high = -100;
        covered.gpu_adapter_luid_low = 999;
        covered.gpu_driver_version = "different-but-audited".into();

        assert!(
            evaluate_perf_certification(&central, &runtime, &runtime.dll_sha256, &key, &covered)
                .is_ok()
        );
        assert_eq!(
            evaluate_perf_certification(
                &central,
                &runtime,
                &runtime.dll_sha256,
                &key,
                &hardware("NVIDIA GeForce RTX 2060 Laptop GPU")
            ),
            Err(PerfCertificationDenial::HardwareNotCovered)
        );
    }

    #[test]
    fn retired_v2_and_v3_payloads_are_not_deserializable_as_the_new_manifest() {
        let legacy = r#"[{"schema":"dm-depth-perf-certification-v2","key":{}}]"#;
        assert!(serde_json::from_str::<DepthPerfCertificationManifest>(legacy).is_err());
        let legacy = r#"[{"schema":"dm-depth-perf-certification-v3","key":{}}]"#;
        assert!(serde_json::from_str::<DepthPerfCertificationManifest>(legacy).is_err());
    }
}
