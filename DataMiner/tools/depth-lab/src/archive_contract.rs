//! Depth-lab 独立 R2 归档的纯契约（docs/77 §7）。
//!
//! 本模块只回答三件事：目标是否仍是隔离桶、对象键是否属于正式布局、
//! 远端证据是否与不可变 manifest 完全一致。它不读环境变量、不碰文件系统、
//! 不发网络请求，也不依赖生产 API / `artifact_objects`。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 独立 depth-ingest 服务的授权、multipart、回读验证与 manifest-last 状态机。
pub mod ingest;

pub const ARCHIVE_SCHEMA_VERSION: &str = "dm-depth-lab-archive-1";
pub const ARCHIVE_BUCKET: &str = "dataminer-depth-lab";
pub const ARCHIVE_PREFIX: &str = "depth-lab/v1";
pub const ARCHIVE_RETENTION_DAYS: u16 = 180;

const REQUIRED_SESSION_OBJECTS: &[(&str, ArchiveObjectKind)] = &[
    (
        "session/depth-profile-evidence.json",
        ArchiveObjectKind::DepthProfileEvidence,
    ),
    ("session/dsv-thumbnail.png", ArchiveObjectKind::DsvThumbnail),
    (
        "session/frame-pairs.jsonl.zst",
        ArchiveObjectKind::FramePairs,
    ),
    (
        "session/action_camera_15fps.json",
        ArchiveObjectKind::ActionCamera,
    ),
    ("session/systeminfo.json", ArchiveObjectKind::SystemInfo),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveLane {
    #[serde(rename = "lane-30")]
    Lane30,
    #[serde(rename = "lane-60")]
    Lane60,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePriority {
    P0,
    P1,
    P2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveObjectKind {
    DepthProfileEvidence,
    DsvThumbnail,
    FramePairs,
    ActionCamera,
    SystemInfo,
    VariantRecipe,
    DepthIndex,
    VariantRArchive,
    Burst,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveObject {
    /// 相对 session 前缀的键；完整键由 [`object_key`] 唯一生成。
    pub relative_key: String,
    pub kind: ArchiveObjectKind,
    pub priority: ArchivePriority,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveManifest {
    pub schema_version: String,
    pub session_id: String,
    pub lane: ArchiveLane,
    /// docs/77 的硬边界。不能由调用方借 manifest 把实验数据提成可训练。
    pub non_trainable: bool,
    /// 不包含 `depth-manifest.json` 自己；调用方必须先校验/上传这些对象，
    /// 再把 manifest 作为同一 session 的最后一个提交对象上传。
    pub objects: Vec<ArchiveObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractViolation {
    SchemaVersion,
    TrainabilityBoundary,
    InvalidSessionId,
    EmptyObjectSet,
    InvalidObjectKey(String),
    DuplicateObjectKey(String),
    EmptyObject(String),
    InvalidSha256(String),
    UnsupportedObjectKey(String),
    KindMismatch {
        key: String,
        declared: ArchiveObjectKind,
        expected: ArchiveObjectKind,
    },
    PriorityMismatch {
        key: String,
        declared: ArchivePriority,
        expected: ArchivePriority,
    },
    MissingRequiredObject(String),
    MissingRunArtifacts,
    MissingRunRecipe(String),
    MissingRunIndex(String),
    MissingRunDepthArchive(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetViolation {
    WrongBucket,
    WrongLifecycle,
}

/// 启动上传通道前先钉住物理隔离与生命周期；调用方不得把桶作为自由参数透传。
pub fn validate_target(bucket: &str, lifecycle_days: u16) -> Result<(), Vec<TargetViolation>> {
    let mut violations = Vec::new();
    if bucket != ARCHIVE_BUCKET {
        violations.push(TargetViolation::WrongBucket);
    }
    if lifecycle_days != ARCHIVE_RETENTION_DAYS {
        violations.push(TargetViolation::WrongLifecycle);
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

impl ArchiveManifest {
    pub fn validate(&self) -> Result<(), Vec<ContractViolation>> {
        let mut violations = Vec::new();
        if self.schema_version != ARCHIVE_SCHEMA_VERSION {
            violations.push(ContractViolation::SchemaVersion);
        }
        if !self.non_trainable {
            violations.push(ContractViolation::TrainabilityBoundary);
        }
        if !valid_identifier(&self.session_id) {
            violations.push(ContractViolation::InvalidSessionId);
        }
        if self.objects.is_empty() {
            violations.push(ContractViolation::EmptyObjectSet);
        }

        let mut keys = BTreeSet::new();
        let mut runs: BTreeMap<&str, RunPresence> = BTreeMap::new();
        for object in &self.objects {
            if !valid_relative_key(&object.relative_key) {
                violations.push(ContractViolation::InvalidObjectKey(
                    object.relative_key.clone(),
                ));
                continue;
            }
            if !keys.insert(object.relative_key.as_str()) {
                violations.push(ContractViolation::DuplicateObjectKey(
                    object.relative_key.clone(),
                ));
            }
            if object.size_bytes == 0 {
                violations.push(ContractViolation::EmptyObject(object.relative_key.clone()));
            }
            if !valid_sha256(&object.sha256) {
                violations.push(ContractViolation::InvalidSha256(
                    object.relative_key.clone(),
                ));
            }

            let Some((expected_kind, expected_priority)) = classify_key(&object.relative_key)
            else {
                violations.push(ContractViolation::UnsupportedObjectKey(
                    object.relative_key.clone(),
                ));
                continue;
            };
            if object.kind != expected_kind {
                violations.push(ContractViolation::KindMismatch {
                    key: object.relative_key.clone(),
                    declared: object.kind,
                    expected: expected_kind,
                });
            }
            if object.priority != expected_priority {
                violations.push(ContractViolation::PriorityMismatch {
                    key: object.relative_key.clone(),
                    declared: object.priority,
                    expected: expected_priority,
                });
            }

            if let Some((run_id, member)) = run_member(&object.relative_key) {
                let presence = runs.entry(run_id).or_default();
                match member {
                    RunMember::Recipe => presence.recipe = true,
                    RunMember::Index => presence.index = true,
                    RunMember::DepthArchive => presence.depth_archive = true,
                    RunMember::Burst => {}
                }
            }
        }

        for (key, _) in REQUIRED_SESSION_OBJECTS {
            if !keys.contains(key) {
                violations.push(ContractViolation::MissingRequiredObject((*key).to_string()));
            }
        }
        if runs.is_empty() {
            violations.push(ContractViolation::MissingRunArtifacts);
        }
        for (run_id, presence) in runs {
            if !presence.recipe {
                violations.push(ContractViolation::MissingRunRecipe(run_id.to_string()));
            }
            if !presence.index {
                violations.push(ContractViolation::MissingRunIndex(run_id.to_string()));
            }
            if !presence.depth_archive {
                violations.push(ContractViolation::MissingRunDepthArchive(
                    run_id.to_string(),
                ));
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// `depth-manifest.json` 固定在 session 的 P0 目录，并由上传器最后提交。
pub fn manifest_object_key(
    lane: ArchiveLane,
    session_id: &str,
) -> Result<String, ContractViolation> {
    Ok(format!(
        "{}/session/depth-manifest.json",
        session_prefix(lane, session_id)?
    ))
}

/// 把 manifest 内的相对键绑定到车道与 session；不接受绝对路径或 `..`。
pub fn object_key(
    lane: ArchiveLane,
    session_id: &str,
    relative_key: &str,
) -> Result<String, ContractViolation> {
    if !valid_relative_key(relative_key) {
        return Err(ContractViolation::InvalidObjectKey(
            relative_key.to_string(),
        ));
    }
    if classify_key(relative_key).is_none() {
        return Err(ContractViolation::UnsupportedObjectKey(
            relative_key.to_string(),
        ));
    }
    Ok(format!(
        "{}/{}",
        session_prefix(lane, session_id)?,
        relative_key
    ))
}

fn session_prefix(lane: ArchiveLane, session_id: &str) -> Result<String, ContractViolation> {
    if !valid_identifier(session_id) {
        return Err(ContractViolation::InvalidSessionId);
    }
    let lane = match lane {
        ArchiveLane::Lane30 => "lane-30",
        ArchiveLane::Lane60 => "lane-60",
    };
    Ok(format!("{ARCHIVE_PREFIX}/{lane}/sessions/{session_id}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObjectEvidence {
    pub object_key: String,
    pub size_bytes: u64,
    /// 必须来自下载回读后的全对象 SHA-256；multipart ETag 或仅 HEAD 不够。
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArchiveVerification {
    pub missing: Vec<String>,
    pub size_mismatch: Vec<String>,
    pub missing_sha256: Vec<String>,
    pub sha256_mismatch: Vec<String>,
    pub unexpected: Vec<String>,
    pub duplicate_evidence: Vec<String>,
}

impl ArchiveVerification {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
            && self.size_mismatch.is_empty()
            && self.missing_sha256.is_empty()
            && self.sha256_mismatch.is_empty()
            && self.unexpected.is_empty()
            && self.duplicate_evidence.is_empty()
    }
}

/// 对账 manifest 声明的 payload。manifest 本身应在本函数通过后上传，并由独立
/// depth archive journal 记录其字节数与 SHA-256；不能写进生产 upload journal。
pub fn verify_remote(
    manifest: &ArchiveManifest,
    remote: &[RemoteObjectEvidence],
) -> Result<ArchiveVerification, Vec<ContractViolation>> {
    manifest.validate()?;
    let expected: BTreeMap<String, &ArchiveObject> = manifest
        .objects
        .iter()
        .map(|object| {
            (
                object_key(manifest.lane, &manifest.session_id, &object.relative_key)
                    .expect("validated manifest keys must build"),
                object,
            )
        })
        .collect();
    let mut report = ArchiveVerification::default();
    let mut observed = BTreeMap::new();
    for evidence in remote {
        if observed
            .insert(evidence.object_key.as_str(), evidence)
            .is_some()
        {
            report.duplicate_evidence.push(evidence.object_key.clone());
        }
        if !expected.contains_key(&evidence.object_key) {
            report.unexpected.push(evidence.object_key.clone());
        }
    }
    for (key, object) in expected {
        let Some(evidence) = observed.get(key.as_str()) else {
            report.missing.push(key);
            continue;
        };
        if evidence.size_bytes != object.size_bytes {
            report.size_mismatch.push(key.clone());
        }
        match evidence.sha256.as_deref() {
            None => report.missing_sha256.push(key),
            Some(sha256) if sha256 != object.sha256 => report.sha256_mismatch.push(key),
            Some(_) => {}
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
struct RunPresence {
    recipe: bool,
    index: bool,
    depth_archive: bool,
}

#[derive(Debug, Clone, Copy)]
enum RunMember {
    Recipe,
    Index,
    DepthArchive,
    Burst,
}

fn run_member(key: &str) -> Option<(&str, RunMember)> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() < 3 || parts[0] != "runs" || !valid_identifier(parts[1]) {
        return None;
    }
    let member = match parts.as_slice() {
        ["runs", _, "variant-s-recipe.json"] => RunMember::Recipe,
        ["runs", _, "depth-index.jsonl.zst"] => RunMember::Index,
        ["runs", _, "variant-r", name] if numbered_file(name, "depth_", ".zip") => {
            RunMember::DepthArchive
        }
        ["runs", _, "burst", name] if numbered_file(name, "", ".depth.zst") => RunMember::Burst,
        _ => return None,
    };
    Some((parts[1], member))
}

fn classify_key(key: &str) -> Option<(ArchiveObjectKind, ArchivePriority)> {
    match key {
        "session/depth-profile-evidence.json" => {
            Some((ArchiveObjectKind::DepthProfileEvidence, ArchivePriority::P0))
        }
        "session/dsv-thumbnail.png" => Some((ArchiveObjectKind::DsvThumbnail, ArchivePriority::P0)),
        "session/frame-pairs.jsonl.zst" => {
            Some((ArchiveObjectKind::FramePairs, ArchivePriority::P0))
        }
        "session/action_camera_15fps.json" => {
            Some((ArchiveObjectKind::ActionCamera, ArchivePriority::P0))
        }
        "session/systeminfo.json" => Some((ArchiveObjectKind::SystemInfo, ArchivePriority::P0)),
        _ => match run_member(key)?.1 {
            RunMember::Recipe => Some((ArchiveObjectKind::VariantRecipe, ArchivePriority::P0)),
            RunMember::Index => Some((ArchiveObjectKind::DepthIndex, ArchivePriority::P0)),
            RunMember::DepthArchive => {
                Some((ArchiveObjectKind::VariantRArchive, ArchivePriority::P2))
            }
            RunMember::Burst => Some((ArchiveObjectKind::Burst, ArchivePriority::P1)),
        },
    }
}

fn valid_identifier(value: &str) -> bool {
    value.len() <= 160
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_relative_key(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| part != "." && part != ".." && valid_identifier(part))
}

fn numbered_file(value: &str, prefix: &str, suffix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn object(
        relative_key: &str,
        kind: ArchiveObjectKind,
        priority: ArchivePriority,
    ) -> ArchiveObject {
        ArchiveObject {
            relative_key: relative_key.into(),
            kind,
            priority,
            size_bytes: 128,
            sha256: HASH.into(),
        }
    }

    fn valid_manifest() -> ArchiveManifest {
        ArchiveManifest {
            schema_version: ARCHIVE_SCHEMA_VERSION.into(),
            session_id: "depth-session-0001".into(),
            lane: ArchiveLane::Lane30,
            non_trainable: true,
            objects: vec![
                object(
                    "session/depth-profile-evidence.json",
                    ArchiveObjectKind::DepthProfileEvidence,
                    ArchivePriority::P0,
                ),
                object(
                    "session/dsv-thumbnail.png",
                    ArchiveObjectKind::DsvThumbnail,
                    ArchivePriority::P0,
                ),
                object(
                    "session/frame-pairs.jsonl.zst",
                    ArchiveObjectKind::FramePairs,
                    ArchivePriority::P0,
                ),
                object(
                    "session/action_camera_15fps.json",
                    ArchiveObjectKind::ActionCamera,
                    ArchivePriority::P0,
                ),
                object(
                    "session/systeminfo.json",
                    ArchiveObjectKind::SystemInfo,
                    ArchivePriority::P0,
                ),
                object(
                    "runs/r0000/variant-s-recipe.json",
                    ArchiveObjectKind::VariantRecipe,
                    ArchivePriority::P0,
                ),
                object(
                    "runs/r0000/depth-index.jsonl.zst",
                    ArchiveObjectKind::DepthIndex,
                    ArchivePriority::P0,
                ),
                object(
                    "runs/r0000/variant-r/depth_0000.zip",
                    ArchiveObjectKind::VariantRArchive,
                    ArchivePriority::P2,
                ),
            ],
        }
    }

    #[test]
    fn formal_target_and_keys_are_pinned() {
        assert!(validate_target("dataminer-depth-lab", 180).is_ok());
        assert_eq!(
            manifest_object_key(ArchiveLane::Lane30, "depth-session-0001").unwrap(),
            "depth-lab/v1/lane-30/sessions/depth-session-0001/session/depth-manifest.json"
        );
        assert_eq!(
            object_key(
                ArchiveLane::Lane60,
                "depth-session-0001",
                "runs/r0000/variant-r/depth_0000.zip"
            )
            .unwrap(),
            "depth-lab/v1/lane-60/sessions/depth-session-0001/runs/r0000/variant-r/depth_0000.zip"
        );
        assert_eq!(
            validate_target("diogenes-model-training", 365),
            Err(vec![
                TargetViolation::WrongBucket,
                TargetViolation::WrongLifecycle
            ])
        );
        assert!(object_key(ArchiveLane::Lane30, "depth-session-0001", "../video.mp4").is_err());
    }

    #[test]
    fn a_complete_non_trainable_manifest_is_accepted() {
        let manifest = valid_manifest();
        assert!(manifest.validate().is_ok());
        assert_eq!(serde_json::to_value(manifest).unwrap()["lane"], "lane-30");
    }

    #[test]
    fn missing_or_inconsistent_depth_declaration_fails_closed() {
        let mut manifest = valid_manifest();
        manifest
            .objects
            .retain(|object| object.relative_key != "runs/r0000/variant-r/depth_0000.zip");
        let violations = manifest.validate().unwrap_err();
        assert!(violations.contains(&ContractViolation::MissingRunDepthArchive("r0000".into())));

        let mut manifest = valid_manifest();
        manifest.objects[7].sha256 = "not-a-sha256".into();
        manifest.objects[7].size_bytes = 0;
        manifest.objects[7].kind = ArchiveObjectKind::Burst;
        let violations = manifest.validate().unwrap_err();
        assert!(violations.iter().any(|violation| matches!(
            violation,
            ContractViolation::InvalidSha256(key)
                if key == "runs/r0000/variant-r/depth_0000.zip"
        )));
        assert!(violations.iter().any(|violation| matches!(
            violation,
            ContractViolation::EmptyObject(key)
                if key == "runs/r0000/variant-r/depth_0000.zip"
        )));
        assert!(violations.iter().any(|violation| matches!(
            violation,
            ContractViolation::KindMismatch { key, .. }
                if key == "runs/r0000/variant-r/depth_0000.zip"
        )));
    }

    #[test]
    fn trainable_or_partial_session_manifests_are_rejected() {
        let mut manifest = valid_manifest();
        manifest.non_trainable = false;
        manifest
            .objects
            .retain(|object| object.relative_key != "session/frame-pairs.jsonl.zst");
        let violations = manifest.validate().unwrap_err();
        assert!(violations.contains(&ContractViolation::TrainabilityBoundary));
        assert!(
            violations.contains(&ContractViolation::MissingRequiredObject(
                "session/frame-pairs.jsonl.zst".into()
            ))
        );
    }

    #[test]
    fn remote_completion_requires_exact_keys_sizes_and_full_sha256() {
        let manifest = valid_manifest();
        let remote: Vec<RemoteObjectEvidence> = manifest
            .objects
            .iter()
            .map(|object| RemoteObjectEvidence {
                object_key: object_key(manifest.lane, &manifest.session_id, &object.relative_key)
                    .unwrap(),
                size_bytes: object.size_bytes,
                sha256: Some(object.sha256.clone()),
            })
            .collect();
        assert!(verify_remote(&manifest, &remote).unwrap().is_complete());

        let mut corrupt = remote;
        corrupt.remove(0);
        corrupt[0].size_bytes += 1;
        corrupt[1].sha256 = None;
        corrupt[2].sha256 = Some(format!("sha256:{}", "b".repeat(64)));
        corrupt.push(RemoteObjectEvidence {
            object_key: "depth-lab/v1/lane-30/sessions/depth-session-0001/video.mp4".into(),
            size_bytes: 1,
            sha256: Some(HASH.into()),
        });
        let report = verify_remote(&manifest, &corrupt).unwrap();
        assert!(!report.is_complete());
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.size_mismatch.len(), 1);
        assert_eq!(report.missing_sha256.len(), 1);
        assert_eq!(report.sha256_mismatch.len(), 1);
        assert_eq!(report.unexpected.len(), 1);
    }
}
