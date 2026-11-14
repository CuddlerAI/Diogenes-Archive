//! `docs/77 §7` 的独立 depth-ingest 服务核心。
//!
//! 本模块刻意不提供 HTTP、数据库或 R2 SDK 实现：这些边界位于
//! `services/depth-ingest`，长期 R2 凭据只能存在于那个独立服务适配器里。核心只向
//! 调用方返回短期、精确绑定 bucket/key/size/hash/expiry 的授权，并要求适配器在
//! multipart 完成后下载回读整个对象计算 SHA-256。
//! 它不依赖生产 API、`artifact_objects` 或客户端 production uploader。
//!
//! 独立 HTTP 适配器应一一映射为：archive create、multipart create、part authorize、
//! object complete/verify、manifest authorize/finalize、status query。除 create 外的每个
//! 请求都携带完整 [`ArchiveScope`]，不能只凭一个可猜的 upload id 找状态。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    ARCHIVE_BUCKET, ARCHIVE_SCHEMA_VERSION, ArchiveLane, ArchiveManifest, ArchiveObject,
    ContractViolation, TargetViolation, manifest_object_key, object_key, valid_identifier,
    valid_sha256, validate_target,
};

pub const DEPTH_INGEST_AUDIENCE: &str = "dataminer-depth-ingest";
pub const DEPTH_INGEST_STATE_SCHEMA_VERSION: &str = "dm-depth-ingest-state-1";
pub const MIN_MULTIPART_PART_SIZE_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_MULTIPART_PARTS: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthPrincipal {
    /// 独立 depth grant 的 subject；不是生产 uploader credential。
    pub subject: String,
    pub audience: String,
    pub expires_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveIdentity {
    pub principal_id: String,
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
    pub lane: ArchiveLane,
    pub non_trainable: bool,
    pub schema_version: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveScope {
    pub archive_id: String,
    #[serde(flatten)]
    pub identity: ArchiveIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateArchiveRequest {
    pub identity: ArchiveIdentity,
    pub target_bucket: String,
    pub retention_days: u16,
    pub manifest_size_bytes: u64,
    pub manifest: ArchiveManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateMultipartRequest {
    pub scope: ArchiveScope,
    pub bucket: String,
    pub object_key: String,
    pub part_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartIntent {
    pub upload_id: String,
    pub bucket: String,
    pub object_key: String,
    pub expected_size_bytes: u64,
    pub expected_sha256: String,
    pub part_size_bytes: u64,
    pub part_count: u32,
    pub already_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizePartRequest {
    pub scope: ArchiveScope,
    pub upload_id: String,
    pub bucket: String,
    pub object_key: String,
    pub part_number: u32,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedGrant {
    pub url: String,
    pub signed_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedPart {
    pub upload_id: String,
    pub bucket: String,
    pub object_key: String,
    pub expected_object_size_bytes: u64,
    pub expected_object_sha256: String,
    pub part_number: u32,
    pub byte_size: u64,
    pub sha256: String,
    pub expires_at_epoch_seconds: u64,
    pub grant: PresignedGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedPart {
    pub part_number: u32,
    pub byte_size: u64,
    pub sha256: String,
    /// 只交给 R2 complete multipart；不能当作内容 SHA-256。
    pub etag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteObjectRequest {
    pub scope: ArchiveScope,
    pub upload_id: String,
    pub bucket: String,
    pub object_key: String,
    pub parts: Vec<CompletedPart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizeManifestRequest {
    pub scope: ArchiveScope,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedManifest {
    pub bucket: String,
    pub object_key: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub expires_at_epoch_seconds: u64,
    pub grant: PresignedGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeManifestRequest {
    pub scope: ArchiveScope,
    pub bucket: String,
    pub object_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivePhase {
    Registered,
    Uploading,
    PayloadVerified,
    ManifestAuthorized,
    Finalized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveStatus {
    pub scope: ArchiveScope,
    pub phase: ArchivePhase,
    pub declared_object_count: usize,
    pub verified_object_count: usize,
    pub active_multipart_count: usize,
    pub aborted_multipart_count: usize,
    pub manifest_object_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    InvalidGrantTtl,
    WrongCredentialAudience,
    PrincipalExpired,
    PrincipalMismatch,
    InvalidIdentity,
    InvalidManifestHash,
    EmptyManifest,
    Target(Vec<TargetViolation>),
    Contract(Vec<ContractViolation>),
    ManifestBindingMismatch,
    ManifestRunMismatch(String),
    ArchiveCollision,
    ArchiveFinalized,
    UnknownArchive,
    ScopeMismatch,
    WrongBucket,
    WrongObjectKey,
    UnknownObject,
    UnknownUpload,
    UploadBindingMismatch,
    UploadAborted,
    InvalidPartSize,
    TooManyParts,
    InvalidPartNumber,
    InvalidPartHash,
    InvalidEtag,
    PartAuthorizationReplay,
    PartNotAuthorized(u32),
    PartAuthorizationExpired(u32),
    DuplicatePart(u32),
    MissingParts,
    PartMismatch(u32),
    CompletionReplayMismatch,
    ObjectSizeMismatch,
    ObjectSha256Mismatch,
    ManifestNotReady,
    ManifestNotAuthorized,
    ManifestAuthorizationExpired,
    ManifestSizeMismatch,
    ManifestSha256Mismatch,
    ManifestContentMismatch,
    InvalidCleanupGrace,
    InvalidStateSnapshot,
    Storage(String),
}

/// 只有独立 depth 服务的 R2 适配器实现此端口。实现不得把任何长期凭据或 provider
/// upload id 作为稳定 API 字段暴露给客户端；S3 multipart presigned URL 必然携带的
/// `uploadId` 只能作为短期 bearer URL 内的 opaque query，不能另行持久化或复用。
pub trait DepthArchiveStorage {
    fn create_multipart(&mut self, target: &StorageObjectTarget) -> Result<String, String>;

    fn authorize_part(&mut self, target: &StoragePartTarget) -> Result<PresignedGrant, String>;

    fn complete_multipart(&mut self, target: &StorageCompleteTarget) -> Result<(), String>;

    fn abort_multipart(
        &mut self,
        bucket: &str,
        object_key: &str,
        provider_upload_id: &str,
    ) -> Result<(), String>;

    /// 必须 GET/流式回读完整对象并计算 SHA-256；HEAD/ETag 不满足此端口。
    fn verify_object(&mut self, bucket: &str, object_key: &str) -> Result<VerifiedObject, String>;

    fn authorize_manifest_put(
        &mut self,
        target: &StorageManifestTarget,
    ) -> Result<PresignedGrant, String>;

    /// 必须下载 manifest 原始字节、计算 SHA-256，再解析为 `ArchiveManifest`。
    fn read_manifest(&mut self, bucket: &str, object_key: &str)
    -> Result<VerifiedManifest, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageObjectTarget {
    pub bucket: String,
    pub object_key: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePartTarget {
    pub bucket: String,
    pub object_key: String,
    pub provider_upload_id: String,
    pub part_number: u32,
    pub byte_size: u64,
    pub sha256: String,
    pub expires_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCompletedPart {
    pub part_number: u32,
    pub etag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCompleteTarget {
    pub bucket: String,
    pub object_key: String,
    pub provider_upload_id: String,
    pub parts: Vec<StorageCompletedPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageManifestTarget {
    pub bucket: String,
    pub object_key: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub expires_at_epoch_seconds: u64,
    /// manifest 是不可变提交标记；适配器必须签入 `If-None-Match: *` 等价约束。
    pub fail_if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedObject {
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    pub size_bytes: u64,
    pub sha256: String,
    pub manifest: ArchiveManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PartAuthorization {
    byte_size: u64,
    sha256: String,
    expires_at_epoch_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MultipartRecord {
    upload_id: String,
    provider_upload_id: String,
    object: ArchiveObject,
    object_key: String,
    part_size_bytes: u64,
    part_count: u32,
    created_at_epoch_seconds: u64,
    last_authorization_expiry: Option<u64>,
    authorizations: BTreeMap<u32, PartAuthorization>,
    completed_parts: Option<Vec<CompletedPart>>,
    provider_completed: bool,
    verified: bool,
    aborted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveRecord {
    scope: ArchiveScope,
    manifest: ArchiveManifest,
    manifest_size_bytes: u64,
    manifest_object_key: String,
    uploads: BTreeMap<String, MultipartRecord>,
    manifest_authorization_expiry: Option<u64>,
    finalized: bool,
}

/// 独立 depth index 的持久化快照。
///
/// 字段保持私有，避免适配层绕过状态机拼装服务状态；数据库/文件 repository 只能把
/// 它当作带版本的不可解释载荷保存，再由 [`DepthIngestService::from_snapshot`] 做完整
/// fail-closed 校验。provider upload id 只存在于该服务端快照，绝不能返回客户端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthIngestSnapshot {
    schema_version: String,
    next_archive_id: u64,
    next_upload_id: u64,
    archives: BTreeMap<String, ArchiveRecord>,
}

pub struct DepthIngestService<S> {
    storage: S,
    grant_ttl_seconds: u64,
    next_archive_id: u64,
    next_upload_id: u64,
    archives: BTreeMap<String, ArchiveRecord>,
}

impl<S: DepthArchiveStorage> DepthIngestService<S> {
    pub fn new(storage: S, grant_ttl_seconds: u64) -> Result<Self, IngestError> {
        if grant_ttl_seconds == 0 {
            return Err(IngestError::InvalidGrantTtl);
        }
        Ok(Self {
            storage,
            grant_ttl_seconds,
            next_archive_id: 1,
            next_upload_id: 1,
            archives: BTreeMap::new(),
        })
    }

    /// 从独立 repository 恢复。任何版本、对象键、scope、manifest 或 multipart
    /// 内部关系不一致都拒绝启动，不能把损坏的 index 当成可继续上传的事实源。
    pub fn from_snapshot(
        storage: S,
        grant_ttl_seconds: u64,
        snapshot: DepthIngestSnapshot,
    ) -> Result<Self, IngestError> {
        if grant_ttl_seconds == 0 {
            return Err(IngestError::InvalidGrantTtl);
        }
        validate_snapshot(&snapshot)?;
        Ok(Self {
            storage,
            grant_ttl_seconds,
            next_archive_id: snapshot.next_archive_id,
            next_upload_id: snapshot.next_upload_id,
            archives: snapshot.archives,
        })
    }

    /// 供独立 depth index repository 原子持久化；不包含任何 R2 access key/secret，
    /// 但包含 provider upload id，因此仍属于服务端敏感状态。
    #[must_use]
    pub fn snapshot(&self) -> DepthIngestSnapshot {
        DepthIngestSnapshot {
            schema_version: DEPTH_INGEST_STATE_SCHEMA_VERSION.to_string(),
            next_archive_id: self.next_archive_id,
            next_upload_id: self.next_upload_id,
            archives: self.archives.clone(),
        }
    }

    pub fn create_archive(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateArchiveRequest,
    ) -> Result<ArchiveScope, IngestError> {
        authenticate(principal, now_epoch_seconds)?;
        if request.identity.principal_id != principal.subject {
            return Err(IngestError::PrincipalMismatch);
        }
        validate_target(&request.target_bucket, request.retention_days)
            .map_err(IngestError::Target)?;
        validate_identity(&request.identity)?;
        if request.manifest_size_bytes == 0 {
            return Err(IngestError::EmptyManifest);
        }
        request.manifest.validate().map_err(IngestError::Contract)?;
        if request.manifest.schema_version != request.identity.schema_version
            || request.manifest.session_id != request.identity.session_id
            || request.manifest.lane != request.identity.lane
            || request.manifest.non_trainable != request.identity.non_trainable
        {
            return Err(IngestError::ManifestBindingMismatch);
        }
        for object in &request.manifest.objects {
            if let Some(run_id) = run_id_for_object(&object.relative_key)
                && run_id != request.identity.run_id
            {
                return Err(IngestError::ManifestRunMismatch(run_id.to_string()));
            }
        }

        for archive in self.archives.values() {
            if archive.scope.identity.session_id == request.identity.session_id {
                if archive.scope.identity == request.identity
                    && archive.manifest == request.manifest
                    && archive.manifest_size_bytes == request.manifest_size_bytes
                {
                    return Ok(archive.scope.clone());
                }
                // session P0 keys are shared, so a second generation/run would overwrite them.
                return Err(IngestError::ArchiveCollision);
            }
        }

        let archive_id = format!("depth-archive-{:016}", self.next_archive_id);
        self.next_archive_id += 1;
        let scope = ArchiveScope {
            archive_id: archive_id.clone(),
            identity: request.identity,
        };
        let manifest_object_key =
            manifest_object_key(scope.identity.lane, &scope.identity.session_id)
                .map_err(|_| IngestError::InvalidIdentity)?;
        self.archives.insert(
            archive_id,
            ArchiveRecord {
                scope: scope.clone(),
                manifest: request.manifest,
                manifest_size_bytes: request.manifest_size_bytes,
                manifest_object_key,
                uploads: BTreeMap::new(),
                manifest_authorization_expiry: None,
                finalized: false,
            },
        );
        Ok(scope)
    }

    pub fn create_multipart(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateMultipartRequest,
    ) -> Result<MultipartIntent, IngestError> {
        self.check_scope(principal, now_epoch_seconds, &request.scope)?;
        if request.bucket != ARCHIVE_BUCKET {
            return Err(IngestError::WrongBucket);
        }
        let object = {
            let archive = self.record(&request.scope)?;
            declared_object(archive, &request.object_key)
                .cloned()
                .ok_or(IngestError::UnknownObject)?
        };
        validate_part_plan(object.size_bytes, request.part_size_bytes)?;

        if let Some(existing) = self
            .record(&request.scope)?
            .uploads
            .get(&request.object_key)
            && !existing.aborted
        {
            if existing.part_size_bytes != request.part_size_bytes {
                return Err(IngestError::UploadBindingMismatch);
            }
            return Ok(intent_for(existing));
        }

        let target = StorageObjectTarget {
            bucket: request.bucket.clone(),
            object_key: request.object_key.clone(),
            size_bytes: object.size_bytes,
            sha256: object.sha256.clone(),
        };
        let provider_upload_id = self
            .storage
            .create_multipart(&target)
            .map_err(IngestError::Storage)?;
        let upload_id = format!("depth-upload-{:016}", self.next_upload_id);
        self.next_upload_id += 1;
        let part_count = expected_part_count(object.size_bytes, request.part_size_bytes)?;
        let upload = MultipartRecord {
            upload_id,
            provider_upload_id,
            object,
            object_key: request.object_key.clone(),
            part_size_bytes: request.part_size_bytes,
            part_count,
            created_at_epoch_seconds: now_epoch_seconds,
            last_authorization_expiry: None,
            authorizations: BTreeMap::new(),
            completed_parts: None,
            provider_completed: false,
            verified: false,
            aborted: false,
        };
        let response = intent_for(&upload);
        self.record_mut(&request.scope)?
            .uploads
            .insert(request.object_key, upload);
        Ok(response)
    }

    pub fn authorize_part(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizePartRequest,
    ) -> Result<AuthorizedPart, IngestError> {
        self.check_scope(principal, now_epoch_seconds, &request.scope)?;
        if request.bucket != ARCHIVE_BUCKET {
            return Err(IngestError::WrongBucket);
        }
        if !valid_sha256(&request.sha256) {
            return Err(IngestError::InvalidPartHash);
        }
        let expires_at_epoch_seconds = now_epoch_seconds
            .saturating_add(self.grant_ttl_seconds)
            .min(principal.expires_at_epoch_seconds);
        let (target, expected_object_size_bytes, expected_object_sha256) = {
            let upload = self.upload(&request.scope, &request.upload_id, &request.object_key)?;
            if upload.aborted {
                return Err(IngestError::UploadAborted);
            }
            if upload.provider_completed || upload.verified {
                return Err(IngestError::CompletionReplayMismatch);
            }
            let expected = expected_part_size(upload, request.part_number)?;
            if request.byte_size != expected {
                return Err(IngestError::InvalidPartSize);
            }
            if let Some(previous) = upload.authorizations.get(&request.part_number)
                && (previous.byte_size != request.byte_size || previous.sha256 != request.sha256)
            {
                return Err(IngestError::PartAuthorizationReplay);
            }
            (
                StoragePartTarget {
                    bucket: request.bucket.clone(),
                    object_key: request.object_key.clone(),
                    provider_upload_id: upload.provider_upload_id.clone(),
                    part_number: request.part_number,
                    byte_size: request.byte_size,
                    sha256: request.sha256.clone(),
                    expires_at_epoch_seconds,
                },
                upload.object.size_bytes,
                upload.object.sha256.clone(),
            )
        };
        let grant = self
            .storage
            .authorize_part(&target)
            .map_err(IngestError::Storage)?;
        let upload = self.upload_mut(&request.scope, &request.upload_id, &request.object_key)?;
        upload.last_authorization_expiry = Some(expires_at_epoch_seconds);
        upload.authorizations.insert(
            request.part_number,
            PartAuthorization {
                byte_size: request.byte_size,
                sha256: request.sha256.clone(),
                expires_at_epoch_seconds,
            },
        );
        Ok(AuthorizedPart {
            upload_id: request.upload_id,
            bucket: request.bucket,
            object_key: request.object_key,
            expected_object_size_bytes,
            expected_object_sha256,
            part_number: request.part_number,
            byte_size: request.byte_size,
            sha256: request.sha256,
            expires_at_epoch_seconds,
            grant,
        })
    }

    pub fn complete_object(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CompleteObjectRequest,
    ) -> Result<ArchiveStatus, IngestError> {
        self.check_scope(principal, now_epoch_seconds, &request.scope)?;
        if request.bucket != ARCHIVE_BUCKET {
            return Err(IngestError::WrongBucket);
        }
        let (
            provider_upload_id,
            expected_size,
            expected_sha256,
            provider_completed,
            already_verified,
            previous_parts,
            authorizations,
            part_count,
        ) = {
            let upload = self.upload(&request.scope, &request.upload_id, &request.object_key)?;
            if upload.aborted {
                return Err(IngestError::UploadAborted);
            }
            (
                upload.provider_upload_id.clone(),
                upload.object.size_bytes,
                upload.object.sha256.clone(),
                upload.provider_completed,
                upload.verified,
                upload.completed_parts.clone(),
                upload.authorizations.clone(),
                upload.part_count,
            )
        };
        if let Some(previous) = previous_parts.as_ref()
            && previous != &request.parts
        {
            return Err(IngestError::CompletionReplayMismatch);
        }
        if already_verified {
            return self.status(principal, now_epoch_seconds, &request.scope);
        }
        if !provider_completed {
            validate_completion_parts(
                &request.parts,
                &authorizations,
                part_count,
                expected_size,
                now_epoch_seconds,
            )?;
            let complete = StorageCompleteTarget {
                bucket: request.bucket.clone(),
                object_key: request.object_key.clone(),
                provider_upload_id,
                parts: request
                    .parts
                    .iter()
                    .map(|part| StorageCompletedPart {
                        part_number: part.part_number,
                        etag: part.etag.clone(),
                    })
                    .collect(),
            };
            self.storage
                .complete_multipart(&complete)
                .map_err(IngestError::Storage)?;
            let upload =
                self.upload_mut(&request.scope, &request.upload_id, &request.object_key)?;
            upload.provider_completed = true;
            upload.completed_parts = Some(request.parts.clone());
        }

        let verified = self
            .storage
            .verify_object(&request.bucket, &request.object_key)
            .map_err(IngestError::Storage)?;
        if verified.size_bytes != expected_size {
            return Err(IngestError::ObjectSizeMismatch);
        }
        if verified.sha256 != expected_sha256 {
            return Err(IngestError::ObjectSha256Mismatch);
        }
        self.upload_mut(&request.scope, &request.upload_id, &request.object_key)?
            .verified = true;
        self.status(principal, now_epoch_seconds, &request.scope)
    }

    pub fn authorize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizeManifestRequest,
    ) -> Result<AuthorizedManifest, IngestError> {
        self.check_scope(principal, now_epoch_seconds, &request.scope)?;
        if request.bucket != ARCHIVE_BUCKET {
            return Err(IngestError::WrongBucket);
        }
        let (manifest_object_key, manifest_size_bytes, manifest_sha256) = {
            let archive = self.record(&request.scope)?;
            if request.object_key != archive.manifest_object_key {
                return Err(IngestError::WrongObjectKey);
            }
            if archive.finalized {
                return Err(IngestError::ArchiveFinalized);
            }
            if !payload_verified(archive) {
                return Err(IngestError::ManifestNotReady);
            }
            (
                archive.manifest_object_key.clone(),
                archive.manifest_size_bytes,
                archive.scope.identity.manifest_sha256.clone(),
            )
        };
        let expires_at_epoch_seconds = now_epoch_seconds
            .saturating_add(self.grant_ttl_seconds)
            .min(principal.expires_at_epoch_seconds);
        let target = StorageManifestTarget {
            bucket: request.bucket.clone(),
            object_key: manifest_object_key.clone(),
            size_bytes: manifest_size_bytes,
            sha256: manifest_sha256.clone(),
            expires_at_epoch_seconds,
            fail_if_exists: true,
        };
        let grant = self
            .storage
            .authorize_manifest_put(&target)
            .map_err(IngestError::Storage)?;
        self.record_mut(&request.scope)?
            .manifest_authorization_expiry = Some(expires_at_epoch_seconds);
        Ok(AuthorizedManifest {
            bucket: request.bucket,
            object_key: manifest_object_key,
            size_bytes: manifest_size_bytes,
            sha256: manifest_sha256,
            expires_at_epoch_seconds,
            grant,
        })
    }

    pub fn finalize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: FinalizeManifestRequest,
    ) -> Result<ArchiveStatus, IngestError> {
        self.check_scope(principal, now_epoch_seconds, &request.scope)?;
        if request.bucket != ARCHIVE_BUCKET {
            return Err(IngestError::WrongBucket);
        }
        let (expected_key, expected_size, expected_sha256, expected_manifest, expiry, finalized) = {
            let archive = self.record(&request.scope)?;
            (
                archive.manifest_object_key.clone(),
                archive.manifest_size_bytes,
                archive.scope.identity.manifest_sha256.clone(),
                archive.manifest.clone(),
                archive.manifest_authorization_expiry,
                archive.finalized,
            )
        };
        if request.object_key != expected_key {
            return Err(IngestError::WrongObjectKey);
        }
        if finalized {
            return self.status(principal, now_epoch_seconds, &request.scope);
        }
        let expiry = expiry.ok_or(IngestError::ManifestNotAuthorized)?;
        if expiry <= now_epoch_seconds {
            return Err(IngestError::ManifestAuthorizationExpired);
        }
        let verified = self
            .storage
            .read_manifest(&request.bucket, &request.object_key)
            .map_err(IngestError::Storage)?;
        if verified.size_bytes != expected_size {
            return Err(IngestError::ManifestSizeMismatch);
        }
        if verified.sha256 != expected_sha256 {
            return Err(IngestError::ManifestSha256Mismatch);
        }
        verified
            .manifest
            .validate()
            .map_err(IngestError::Contract)?;
        if verified.manifest != expected_manifest {
            return Err(IngestError::ManifestContentMismatch);
        }
        self.record_mut(&request.scope)?.finalized = true;
        self.status(principal, now_epoch_seconds, &request.scope)
    }

    pub fn status(
        &self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        scope: &ArchiveScope,
    ) -> Result<ArchiveStatus, IngestError> {
        self.check_scope(principal, now_epoch_seconds, scope)?;
        Ok(status_for(self.record(scope)?, now_epoch_seconds))
    }

    /// 仅清理从未完成 provider complete 的过期 multipart。已完成或已验证对象不在
    /// 此清扫面；R2 lifecycle 和 session 删除是另一个运维边界。
    pub fn abort_expired_multipart(
        &mut self,
        now_epoch_seconds: u64,
        grace_seconds: u64,
    ) -> Result<Vec<String>, IngestError> {
        if grace_seconds == 0 {
            return Err(IngestError::InvalidCleanupGrace);
        }
        let mut stale = Vec::new();
        for (archive_id, archive) in &self.archives {
            for upload in archive.uploads.values() {
                let fence = upload
                    .last_authorization_expiry
                    .unwrap_or(upload.created_at_epoch_seconds)
                    .saturating_add(grace_seconds);
                if !upload.aborted
                    && !upload.provider_completed
                    && !upload.verified
                    && fence <= now_epoch_seconds
                {
                    stale.push((
                        archive_id.clone(),
                        upload.object_key.clone(),
                        upload.provider_upload_id.clone(),
                        upload.upload_id.clone(),
                    ));
                }
            }
        }
        let mut aborted = Vec::new();
        for (archive_id, object_key, provider_upload_id, upload_id) in stale {
            self.storage
                .abort_multipart(ARCHIVE_BUCKET, &object_key, &provider_upload_id)
                .map_err(IngestError::Storage)?;
            let archive = self
                .archives
                .get_mut(&archive_id)
                .ok_or(IngestError::UnknownArchive)?;
            let upload = archive
                .uploads
                .get_mut(&object_key)
                .ok_or(IngestError::UnknownUpload)?;
            upload.aborted = true;
            aborted.push(upload_id);
        }
        Ok(aborted)
    }

    fn check_scope(
        &self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        scope: &ArchiveScope,
    ) -> Result<(), IngestError> {
        authenticate(principal, now_epoch_seconds)?;
        if scope.identity.principal_id != principal.subject {
            return Err(IngestError::PrincipalMismatch);
        }
        let archive = self
            .archives
            .get(&scope.archive_id)
            .ok_or(IngestError::UnknownArchive)?;
        if archive.scope != *scope {
            return Err(IngestError::ScopeMismatch);
        }
        Ok(())
    }

    fn record(&self, scope: &ArchiveScope) -> Result<&ArchiveRecord, IngestError> {
        let archive = self
            .archives
            .get(&scope.archive_id)
            .ok_or(IngestError::UnknownArchive)?;
        if archive.scope != *scope {
            return Err(IngestError::ScopeMismatch);
        }
        Ok(archive)
    }

    fn record_mut(&mut self, scope: &ArchiveScope) -> Result<&mut ArchiveRecord, IngestError> {
        let archive = self
            .archives
            .get_mut(&scope.archive_id)
            .ok_or(IngestError::UnknownArchive)?;
        if archive.scope != *scope {
            return Err(IngestError::ScopeMismatch);
        }
        Ok(archive)
    }

    fn upload(
        &self,
        scope: &ArchiveScope,
        upload_id: &str,
        object_key: &str,
    ) -> Result<&MultipartRecord, IngestError> {
        let upload = self
            .record(scope)?
            .uploads
            .get(object_key)
            .ok_or(IngestError::UnknownUpload)?;
        if upload.upload_id != upload_id || upload.object_key != object_key {
            return Err(IngestError::UploadBindingMismatch);
        }
        Ok(upload)
    }

    fn upload_mut(
        &mut self,
        scope: &ArchiveScope,
        upload_id: &str,
        object_key: &str,
    ) -> Result<&mut MultipartRecord, IngestError> {
        let upload = self
            .record_mut(scope)?
            .uploads
            .get_mut(object_key)
            .ok_or(IngestError::UnknownUpload)?;
        if upload.upload_id != upload_id || upload.object_key != object_key {
            return Err(IngestError::UploadBindingMismatch);
        }
        Ok(upload)
    }
}

fn validate_snapshot(snapshot: &DepthIngestSnapshot) -> Result<(), IngestError> {
    if snapshot.schema_version != DEPTH_INGEST_STATE_SCHEMA_VERSION
        || snapshot.next_archive_id == 0
        || snapshot.next_upload_id == 0
    {
        return Err(IngestError::InvalidStateSnapshot);
    }

    let mut sessions = BTreeSet::new();
    let mut upload_ids = BTreeSet::new();
    for (archive_id, archive) in &snapshot.archives {
        if parse_counter_id(archive_id, "depth-archive-")
            .is_none_or(|counter| counter >= snapshot.next_archive_id)
            || archive.scope.archive_id != *archive_id
            || !sessions.insert(archive.scope.identity.session_id.as_str())
            || validate_identity(&archive.scope.identity).is_err()
            || archive.manifest.validate().is_err()
            || archive.manifest.schema_version != archive.scope.identity.schema_version
            || archive.manifest.session_id != archive.scope.identity.session_id
            || archive.manifest.lane != archive.scope.identity.lane
            || archive.manifest.non_trainable != archive.scope.identity.non_trainable
            || archive.manifest_size_bytes == 0
            || manifest_object_key(
                archive.scope.identity.lane,
                &archive.scope.identity.session_id,
            )
            .as_deref()
                != Ok(archive.manifest_object_key.as_str())
            || (archive.finalized
                && (!payload_verified(archive) || archive.manifest_authorization_expiry.is_none()))
        {
            return Err(IngestError::InvalidStateSnapshot);
        }

        for (object_key_in_map, upload) in &archive.uploads {
            if parse_counter_id(&upload.upload_id, "depth-upload-")
                .is_none_or(|counter| counter >= snapshot.next_upload_id)
                || upload.object_key != *object_key_in_map
                || upload.provider_upload_id.trim().is_empty()
                || !upload_ids.insert(upload.upload_id.as_str())
                || declared_object(archive, object_key_in_map) != Some(&upload.object)
                || validate_part_plan(upload.object.size_bytes, upload.part_size_bytes).is_err()
                || expected_part_count(upload.object.size_bytes, upload.part_size_bytes)
                    != Ok(upload.part_count)
                || (upload.verified && !upload.provider_completed)
                || (upload.aborted && (upload.provider_completed || upload.verified))
                || upload.provider_completed != upload.completed_parts.is_some()
            {
                return Err(IngestError::InvalidStateSnapshot);
            }

            for (part_number, authorization) in &upload.authorizations {
                if expected_part_size(upload, *part_number) != Ok(authorization.byte_size)
                    || !valid_sha256(&authorization.sha256)
                    || authorization.expires_at_epoch_seconds == 0
                {
                    return Err(IngestError::InvalidStateSnapshot);
                }
            }

            if let Some(parts) = &upload.completed_parts {
                let mut seen = BTreeSet::new();
                let mut total = 0_u64;
                for part in parts {
                    let Some(authorization) = upload.authorizations.get(&part.part_number) else {
                        return Err(IngestError::InvalidStateSnapshot);
                    };
                    if !seen.insert(part.part_number)
                        || part.etag.trim().is_empty()
                        || !valid_sha256(&part.sha256)
                        || part.byte_size != authorization.byte_size
                        || part.sha256 != authorization.sha256
                    {
                        return Err(IngestError::InvalidStateSnapshot);
                    }
                    total = total
                        .checked_add(part.byte_size)
                        .ok_or(IngestError::InvalidStateSnapshot)?;
                }
                if seen != (1..=upload.part_count).collect() || total != upload.object.size_bytes {
                    return Err(IngestError::InvalidStateSnapshot);
                }
            }
        }
    }

    Ok(())
}

fn parse_counter_id(value: &str, prefix: &str) -> Option<u64> {
    value
        .strip_prefix(prefix)
        .filter(|digits| digits.len() == 16 && digits.bytes().all(|byte| byte.is_ascii_digit()))?
        .parse()
        .ok()
}

fn authenticate(principal: &DepthPrincipal, now_epoch_seconds: u64) -> Result<(), IngestError> {
    if principal.audience != DEPTH_INGEST_AUDIENCE {
        return Err(IngestError::WrongCredentialAudience);
    }
    if principal.expires_at_epoch_seconds <= now_epoch_seconds {
        return Err(IngestError::PrincipalExpired);
    }
    if !valid_identifier(&principal.subject) {
        return Err(IngestError::InvalidIdentity);
    }
    Ok(())
}

fn validate_identity(identity: &ArchiveIdentity) -> Result<(), IngestError> {
    if !valid_identifier(&identity.principal_id)
        || !valid_identifier(&identity.session_id)
        || !valid_identifier(&identity.run_id)
        || !identity.non_trainable
        || identity.schema_version != ARCHIVE_SCHEMA_VERSION
    {
        return Err(IngestError::InvalidIdentity);
    }
    if !valid_sha256(&identity.manifest_sha256) {
        return Err(IngestError::InvalidManifestHash);
    }
    Ok(())
}

fn run_id_for_object(relative_key: &str) -> Option<&str> {
    let mut parts = relative_key.split('/');
    (parts.next() == Some("runs"))
        .then(|| parts.next())
        .flatten()
}

fn declared_object<'a>(archive: &'a ArchiveRecord, full_key: &str) -> Option<&'a ArchiveObject> {
    archive.manifest.objects.iter().find(|object| {
        object_key(
            archive.scope.identity.lane,
            &archive.scope.identity.session_id,
            &object.relative_key,
        )
        .as_deref()
            == Ok(full_key)
    })
}

fn validate_part_plan(size_bytes: u64, part_size_bytes: u64) -> Result<(), IngestError> {
    if part_size_bytes == 0
        || (size_bytes > part_size_bytes && part_size_bytes < MIN_MULTIPART_PART_SIZE_BYTES)
    {
        return Err(IngestError::InvalidPartSize);
    }
    expected_part_count(size_bytes, part_size_bytes).map(|_| ())
}

fn expected_part_count(size_bytes: u64, part_size_bytes: u64) -> Result<u32, IngestError> {
    let count = size_bytes.div_ceil(part_size_bytes);
    if count == 0 || count > u64::from(MAX_MULTIPART_PARTS) {
        return Err(IngestError::TooManyParts);
    }
    u32::try_from(count).map_err(|_| IngestError::TooManyParts)
}

fn expected_part_size(upload: &MultipartRecord, part_number: u32) -> Result<u64, IngestError> {
    if part_number == 0 || part_number > upload.part_count {
        return Err(IngestError::InvalidPartNumber);
    }
    if part_number < upload.part_count {
        Ok(upload.part_size_bytes)
    } else {
        Ok(upload.object.size_bytes
            - upload.part_size_bytes * u64::from(upload.part_count.saturating_sub(1)))
    }
}

fn validate_completion_parts(
    parts: &[CompletedPart],
    authorizations: &BTreeMap<u32, PartAuthorization>,
    expected_part_count: u32,
    expected_size: u64,
    now_epoch_seconds: u64,
) -> Result<(), IngestError> {
    if parts.len() != expected_part_count as usize {
        return Err(IngestError::MissingParts);
    }
    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    for part in parts {
        if !seen.insert(part.part_number) {
            return Err(IngestError::DuplicatePart(part.part_number));
        }
        if !valid_sha256(&part.sha256) {
            return Err(IngestError::InvalidPartHash);
        }
        if part.etag.trim().is_empty() {
            return Err(IngestError::InvalidEtag);
        }
        let authorization = authorizations
            .get(&part.part_number)
            .ok_or(IngestError::PartNotAuthorized(part.part_number))?;
        if authorization.expires_at_epoch_seconds <= now_epoch_seconds {
            return Err(IngestError::PartAuthorizationExpired(part.part_number));
        }
        if authorization.byte_size != part.byte_size || authorization.sha256 != part.sha256 {
            return Err(IngestError::PartMismatch(part.part_number));
        }
        total = total
            .checked_add(part.byte_size)
            .ok_or(IngestError::ObjectSizeMismatch)?;
    }
    if seen != (1..=expected_part_count).collect() {
        return Err(IngestError::MissingParts);
    }
    if total != expected_size {
        return Err(IngestError::ObjectSizeMismatch);
    }
    Ok(())
}

fn intent_for(upload: &MultipartRecord) -> MultipartIntent {
    MultipartIntent {
        upload_id: upload.upload_id.clone(),
        bucket: ARCHIVE_BUCKET.into(),
        object_key: upload.object_key.clone(),
        expected_size_bytes: upload.object.size_bytes,
        expected_sha256: upload.object.sha256.clone(),
        part_size_bytes: upload.part_size_bytes,
        part_count: upload.part_count,
        already_verified: upload.verified,
    }
}

fn payload_verified(archive: &ArchiveRecord) -> bool {
    archive.uploads.len() == archive.manifest.objects.len()
        && archive.uploads.values().all(|upload| upload.verified)
}

fn status_for(archive: &ArchiveRecord, now_epoch_seconds: u64) -> ArchiveStatus {
    let verified = archive
        .uploads
        .values()
        .filter(|upload| upload.verified)
        .count();
    let active = archive
        .uploads
        .values()
        .filter(|upload| !upload.aborted && !upload.provider_completed)
        .count();
    let aborted = archive
        .uploads
        .values()
        .filter(|upload| upload.aborted)
        .count();
    let phase = if archive.finalized {
        ArchivePhase::Finalized
    } else if archive
        .manifest_authorization_expiry
        .is_some_and(|expiry| expiry > now_epoch_seconds)
    {
        ArchivePhase::ManifestAuthorized
    } else if payload_verified(archive) {
        ArchivePhase::PayloadVerified
    } else if archive.uploads.is_empty() {
        ArchivePhase::Registered
    } else {
        ArchivePhase::Uploading
    };
    ArchiveStatus {
        scope: archive.scope.clone(),
        phase,
        declared_object_count: archive.manifest.objects.len(),
        verified_object_count: verified,
        active_multipart_count: active,
        aborted_multipart_count: aborted,
        manifest_object_key: archive.manifest_object_key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_contract::{ARCHIVE_RETENTION_DAYS, ArchiveObjectKind, ArchivePriority};

    const OBJECT_HASH: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PART_HASH: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const MANIFEST_HASH: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    #[derive(Default)]
    struct FakeStorage {
        next_provider_id: u64,
        verified: BTreeMap<String, VerifiedObject>,
        manifest: Option<VerifiedManifest>,
        completed: Vec<StorageCompleteTarget>,
        aborted: Vec<String>,
    }

    impl DepthArchiveStorage for FakeStorage {
        fn create_multipart(&mut self, _target: &StorageObjectTarget) -> Result<String, String> {
            self.next_provider_id += 1;
            Ok(format!("provider-{}", self.next_provider_id))
        }

        fn authorize_part(&mut self, target: &StoragePartTarget) -> Result<PresignedGrant, String> {
            Ok(PresignedGrant {
                url: format!("https://depth.invalid/part/{}", target.part_number),
                signed_headers: BTreeMap::from([
                    ("content-length".into(), target.byte_size.to_string()),
                    ("x-amz-checksum-sha256".into(), target.sha256.clone()),
                ]),
            })
        }

        fn complete_multipart(&mut self, target: &StorageCompleteTarget) -> Result<(), String> {
            self.completed.push(target.clone());
            Ok(())
        }

        fn abort_multipart(
            &mut self,
            _bucket: &str,
            object_key: &str,
            _provider_upload_id: &str,
        ) -> Result<(), String> {
            self.aborted.push(object_key.into());
            Ok(())
        }

        fn verify_object(
            &mut self,
            _bucket: &str,
            object_key: &str,
        ) -> Result<VerifiedObject, String> {
            self.verified
                .get(object_key)
                .cloned()
                .ok_or_else(|| format!("no readback for {object_key}"))
        }

        fn authorize_manifest_put(
            &mut self,
            target: &StorageManifestTarget,
        ) -> Result<PresignedGrant, String> {
            assert!(target.fail_if_exists);
            Ok(PresignedGrant {
                url: "https://depth.invalid/manifest".into(),
                signed_headers: BTreeMap::from([
                    ("content-length".into(), target.size_bytes.to_string()),
                    ("x-amz-checksum-sha256".into(), target.sha256.clone()),
                    ("if-none-match".into(), "*".into()),
                ]),
            })
        }

        fn read_manifest(
            &mut self,
            _bucket: &str,
            _object_key: &str,
        ) -> Result<VerifiedManifest, String> {
            self.manifest
                .clone()
                .ok_or_else(|| "no manifest readback".into())
        }
    }

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
            sha256: OBJECT_HASH.into(),
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

    fn principal(expiry: u64) -> DepthPrincipal {
        DepthPrincipal {
            subject: "depth-device-1".into(),
            audience: DEPTH_INGEST_AUDIENCE.into(),
            expires_at_epoch_seconds: expiry,
        }
    }

    fn create_request(manifest: ArchiveManifest) -> CreateArchiveRequest {
        CreateArchiveRequest {
            identity: ArchiveIdentity {
                principal_id: "depth-device-1".into(),
                session_id: "depth-session-0001".into(),
                run_id: "r0000".into(),
                generation: 7,
                lane: ArchiveLane::Lane30,
                non_trainable: true,
                schema_version: ARCHIVE_SCHEMA_VERSION.into(),
                manifest_sha256: MANIFEST_HASH.into(),
            },
            target_bucket: ARCHIVE_BUCKET.into(),
            retention_days: ARCHIVE_RETENTION_DAYS,
            manifest_size_bytes: 4_096,
            manifest,
        }
    }

    fn register(service: &mut DepthIngestService<FakeStorage>) -> ArchiveScope {
        service
            .create_archive(&principal(10_000), 100, create_request(valid_manifest()))
            .unwrap()
    }

    fn full_key(scope: &ArchiveScope, relative_key: &str) -> String {
        object_key(
            scope.identity.lane,
            &scope.identity.session_id,
            relative_key,
        )
        .unwrap()
    }

    fn create_upload(
        service: &mut DepthIngestService<FakeStorage>,
        scope: &ArchiveScope,
        relative_key: &str,
        now: u64,
    ) -> MultipartIntent {
        service
            .create_multipart(
                &principal(10_000),
                now,
                CreateMultipartRequest {
                    scope: scope.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: full_key(scope, relative_key),
                    part_size_bytes: 128,
                },
            )
            .unwrap()
    }

    fn authorize_single_part(
        service: &mut DepthIngestService<FakeStorage>,
        scope: &ArchiveScope,
        intent: &MultipartIntent,
        now: u64,
    ) -> CompletedPart {
        service
            .authorize_part(
                &principal(10_000),
                now,
                AuthorizePartRequest {
                    scope: scope.clone(),
                    upload_id: intent.upload_id.clone(),
                    bucket: intent.bucket.clone(),
                    object_key: intent.object_key.clone(),
                    part_number: 1,
                    byte_size: 128,
                    sha256: PART_HASH.into(),
                },
            )
            .unwrap();
        CompletedPart {
            part_number: 1,
            byte_size: 128,
            sha256: PART_HASH.into(),
            etag: "provider-etag-is-not-sha256".into(),
        }
    }

    fn complete_one(
        service: &mut DepthIngestService<FakeStorage>,
        scope: &ArchiveScope,
        relative_key: &str,
        now: u64,
    ) -> (MultipartIntent, CompletedPart) {
        let intent = create_upload(service, scope, relative_key, now);
        let part = authorize_single_part(service, scope, &intent, now);
        service.storage.verified.insert(
            intent.object_key.clone(),
            VerifiedObject {
                size_bytes: intent.expected_size_bytes,
                sha256: intent.expected_sha256.clone(),
            },
        );
        service
            .complete_object(
                &principal(10_000),
                now + 1,
                CompleteObjectRequest {
                    scope: scope.clone(),
                    upload_id: intent.upload_id.clone(),
                    bucket: intent.bucket.clone(),
                    object_key: intent.object_key.clone(),
                    parts: vec![part.clone()],
                },
            )
            .unwrap();
        (intent, part)
    }

    fn complete_all_payloads(
        service: &mut DepthIngestService<FakeStorage>,
        scope: &ArchiveScope,
        manifest: &ArchiveManifest,
    ) {
        for object in &manifest.objects {
            complete_one(service, scope, &object.relative_key, 200);
        }
    }

    #[test]
    fn happy_path_is_manifest_last_and_never_treats_etag_as_sha256() {
        let manifest = valid_manifest();
        let mut service = DepthIngestService::new(FakeStorage::default(), 900).unwrap();
        let scope = register(&mut service);
        let manifest_key =
            manifest_object_key(scope.identity.lane, &scope.identity.session_id).unwrap();

        assert_eq!(
            service
                .authorize_manifest(
                    &principal(10_000),
                    200,
                    AuthorizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap_err(),
            IngestError::ManifestNotReady
        );

        complete_all_payloads(&mut service, &scope, &manifest);
        let authorized = service
            .authorize_manifest(
                &principal(10_000),
                300,
                AuthorizeManifestRequest {
                    scope: scope.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: manifest_key.clone(),
                },
            )
            .unwrap();
        assert_eq!(authorized.sha256, MANIFEST_HASH);
        assert_eq!(authorized.size_bytes, 4_096);
        assert_eq!(authorized.grant.signed_headers["if-none-match"], "*");
        service.storage.manifest = Some(VerifiedManifest {
            size_bytes: 4_096,
            sha256: MANIFEST_HASH.into(),
            manifest: manifest.clone(),
        });
        let status = service
            .finalize_manifest(
                &principal(10_000),
                301,
                FinalizeManifestRequest {
                    scope: scope.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: manifest_key.clone(),
                },
            )
            .unwrap();
        assert_eq!(status.phase, ArchivePhase::Finalized);
        assert_eq!(status.verified_object_count, manifest.objects.len());
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    302,
                    FinalizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap()
                .phase,
            ArchivePhase::Finalized
        );
        assert_eq!(
            service
                .authorize_manifest(
                    &principal(10_000),
                    302,
                    AuthorizeManifestRequest {
                        scope,
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key,
                    },
                )
                .unwrap_err(),
            IngestError::ArchiveFinalized
        );
        assert!(
            service
                .storage
                .completed
                .iter()
                .flat_map(|complete| &complete.parts)
                .all(|part| part.etag == "provider-etag-is-not-sha256")
        );
    }

    #[test]
    fn bucket_key_path_and_manifest_set_are_fail_closed() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 900).unwrap();
        let mut wrong_bucket = create_request(valid_manifest());
        wrong_bucket.target_bucket = "diogenes-model-training".into();
        assert!(matches!(
            service.create_archive(&principal(10_000), 100, wrong_bucket),
            Err(IngestError::Target(_))
        ));
        let mut wrong_retention = create_request(valid_manifest());
        wrong_retention.retention_days = ARCHIVE_RETENTION_DAYS - 1;
        assert!(matches!(
            service.create_archive(&principal(10_000), 100, wrong_retention),
            Err(IngestError::Target(_))
        ));

        let mut traversal = valid_manifest();
        traversal.objects[0].relative_key = "../escape.zip".into();
        assert!(matches!(
            service.create_archive(&principal(10_000), 100, create_request(traversal)),
            Err(IngestError::Contract(_))
        ));

        let mut duplicate = valid_manifest();
        duplicate.objects.push(duplicate.objects[0].clone());
        assert!(matches!(
            service.create_archive(&principal(10_000), 100, create_request(duplicate)),
            Err(IngestError::Contract(_))
        ));

        let mut missing = valid_manifest();
        missing
            .objects
            .retain(|object| object.relative_key != "session/systeminfo.json");
        assert!(matches!(
            service.create_archive(&principal(10_000), 100, create_request(missing)),
            Err(IngestError::Contract(_))
        ));

        let scope = register(&mut service);
        let key = full_key(&scope, "session/systeminfo.json");
        for (bucket, object_key, expected) in [
            (
                "diogenes-model-training",
                key.as_str(),
                IngestError::WrongBucket,
            ),
            (
                ARCHIVE_BUCKET,
                "depth-lab/v1/../escape",
                IngestError::UnknownObject,
            ),
        ] {
            let error = service
                .create_multipart(
                    &principal(10_000),
                    200,
                    CreateMultipartRequest {
                        scope: scope.clone(),
                        bucket: bucket.into(),
                        object_key: object_key.into(),
                        part_size_bytes: 128,
                    },
                )
                .unwrap_err();
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn every_call_is_bound_to_principal_session_run_generation_lane_and_schema() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 900).unwrap();
        let scope = register(&mut service);
        let mut wrong = scope.clone();
        wrong.identity.generation += 1;
        assert_eq!(
            service.status(&principal(10_000), 200, &wrong).unwrap_err(),
            IngestError::ScopeMismatch
        );
        wrong = scope.clone();
        wrong.identity.lane = ArchiveLane::Lane60;
        assert_eq!(
            service.status(&principal(10_000), 200, &wrong).unwrap_err(),
            IngestError::ScopeMismatch
        );
        wrong = scope.clone();
        wrong.identity.run_id = "r9999".into();
        let mut wrong_scopes = vec![wrong];
        wrong = scope.clone();
        wrong.identity.session_id = "depth-session-9999".into();
        wrong_scopes.push(wrong);
        wrong = scope.clone();
        wrong.identity.non_trainable = false;
        wrong_scopes.push(wrong);
        wrong = scope.clone();
        wrong.identity.schema_version = "dm-depth-lab-archive-999".into();
        wrong_scopes.push(wrong);
        wrong = scope.clone();
        wrong.identity.manifest_sha256 = OBJECT_HASH.into();
        wrong_scopes.push(wrong);
        for wrong in wrong_scopes {
            assert_eq!(
                service.status(&principal(10_000), 200, &wrong).unwrap_err(),
                IngestError::ScopeMismatch
            );
        }
        let other = DepthPrincipal {
            subject: "another-device".into(),
            ..principal(10_000)
        };
        assert_eq!(
            service.status(&other, 200, &scope).unwrap_err(),
            IngestError::PrincipalMismatch
        );

        let mut new_generation = create_request(valid_manifest());
        new_generation.identity.generation += 1;
        assert_eq!(
            service
                .create_archive(&principal(10_000), 200, new_generation)
                .unwrap_err(),
            IngestError::ArchiveCollision
        );

        let mut wrong_schema = create_request(valid_manifest());
        wrong_schema.identity.schema_version = "dm-depth-lab-archive-999".into();
        assert_eq!(
            service
                .create_archive(&principal(10_000), 200, wrong_schema)
                .unwrap_err(),
            IngestError::InvalidIdentity
        );
        let mut trainable = create_request(valid_manifest());
        trainable.identity.non_trainable = false;
        trainable.manifest.non_trainable = false;
        assert_eq!(
            service
                .create_archive(&principal(10_000), 200, trainable)
                .unwrap_err(),
            IngestError::InvalidIdentity
        );

        let mut other_run_manifest = valid_manifest();
        for object in &mut other_run_manifest.objects {
            object.relative_key = object.relative_key.replace("runs/r0000/", "runs/r9999/");
        }
        assert_eq!(
            service
                .create_archive(
                    &DepthPrincipal {
                        subject: "depth-device-2".into(),
                        ..principal(10_000)
                    },
                    200,
                    CreateArchiveRequest {
                        identity: ArchiveIdentity {
                            principal_id: "depth-device-2".into(),
                            session_id: "depth-session-0002".into(),
                            run_id: "r0000".into(),
                            generation: 1,
                            lane: ArchiveLane::Lane30,
                            non_trainable: true,
                            schema_version: ARCHIVE_SCHEMA_VERSION.into(),
                            manifest_sha256: MANIFEST_HASH.into(),
                        },
                        manifest: ArchiveManifest {
                            session_id: "depth-session-0002".into(),
                            ..other_run_manifest
                        },
                        target_bucket: ARCHIVE_BUCKET.into(),
                        retention_days: ARCHIVE_RETENTION_DAYS,
                        manifest_size_bytes: 100,
                    },
                )
                .unwrap_err(),
            IngestError::ManifestRunMismatch("r9999".into())
        );
    }

    #[test]
    fn expired_principals_and_upload_grants_are_rejected() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 10).unwrap();
        let mut wrong_audience = principal(1_000);
        wrong_audience.audience = "dataminer-production-api".into();
        assert_eq!(
            service
                .create_archive(&wrong_audience, 100, create_request(valid_manifest()))
                .unwrap_err(),
            IngestError::WrongCredentialAudience
        );
        assert_eq!(
            service
                .create_archive(&principal(100), 100, create_request(valid_manifest()))
                .unwrap_err(),
            IngestError::PrincipalExpired
        );
        let scope = register(&mut service);
        let intent = create_upload(&mut service, &scope, "session/systeminfo.json", 200);
        let part = authorize_single_part(&mut service, &scope, &intent, 200);
        assert_eq!(
            service
                .complete_object(
                    &principal(10_000),
                    210,
                    CompleteObjectRequest {
                        scope,
                        upload_id: intent.upload_id,
                        bucket: intent.bucket,
                        object_key: intent.object_key,
                        parts: vec![part],
                    },
                )
                .unwrap_err(),
            IngestError::PartAuthorizationExpired(1)
        );
    }

    #[test]
    fn size_hash_and_missing_part_mismatches_never_complete() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 900).unwrap();
        let scope = register(&mut service);
        let intent = create_upload(&mut service, &scope, "session/systeminfo.json", 200);
        assert_eq!(
            service
                .authorize_part(
                    &principal(10_000),
                    200,
                    AuthorizePartRequest {
                        scope: scope.clone(),
                        upload_id: intent.upload_id.clone(),
                        bucket: intent.bucket.clone(),
                        object_key: intent.object_key.clone(),
                        part_number: 1,
                        byte_size: 127,
                        sha256: PART_HASH.into(),
                    },
                )
                .unwrap_err(),
            IngestError::InvalidPartSize
        );
        let part = authorize_single_part(&mut service, &scope, &intent, 200);
        assert_eq!(
            service
                .complete_object(
                    &principal(10_000),
                    201,
                    CompleteObjectRequest {
                        scope: scope.clone(),
                        upload_id: intent.upload_id.clone(),
                        bucket: intent.bucket.clone(),
                        object_key: intent.object_key.clone(),
                        parts: vec![],
                    },
                )
                .unwrap_err(),
            IngestError::MissingParts
        );
        service.storage.verified.insert(
            intent.object_key.clone(),
            VerifiedObject {
                size_bytes: 128,
                // ETag-looking evidence is deliberately not accepted as SHA-256.
                sha256: "provider-etag-is-not-sha256".into(),
            },
        );
        let request = CompleteObjectRequest {
            scope: scope.clone(),
            upload_id: intent.upload_id.clone(),
            bucket: intent.bucket.clone(),
            object_key: intent.object_key.clone(),
            parts: vec![part],
        };
        assert_eq!(
            service
                .complete_object(&principal(10_000), 201, request.clone())
                .unwrap_err(),
            IngestError::ObjectSha256Mismatch
        );
        service.storage.verified.insert(
            intent.object_key,
            VerifiedObject {
                size_bytes: 127,
                sha256: OBJECT_HASH.into(),
            },
        );
        assert_eq!(
            service
                .complete_object(&principal(10_000), 202, request)
                .unwrap_err(),
            IngestError::ObjectSizeMismatch
        );
    }

    #[test]
    fn duplicate_parts_are_rejected_even_when_their_total_shape_looks_complete() {
        let mut manifest = valid_manifest();
        let object = manifest
            .objects
            .iter_mut()
            .find(|object| object.relative_key == "session/systeminfo.json")
            .unwrap();
        object.size_bytes = MIN_MULTIPART_PART_SIZE_BYTES * 2;
        let mut service = DepthIngestService::new(FakeStorage::default(), 900).unwrap();
        let scope = service
            .create_archive(&principal(10_000), 100, create_request(manifest))
            .unwrap();
        let key = full_key(&scope, "session/systeminfo.json");
        let intent = service
            .create_multipart(
                &principal(10_000),
                200,
                CreateMultipartRequest {
                    scope: scope.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: key.clone(),
                    part_size_bytes: MIN_MULTIPART_PART_SIZE_BYTES,
                },
            )
            .unwrap();
        for part_number in 1..=2 {
            service
                .authorize_part(
                    &principal(10_000),
                    200,
                    AuthorizePartRequest {
                        scope: scope.clone(),
                        upload_id: intent.upload_id.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: key.clone(),
                        part_number,
                        byte_size: MIN_MULTIPART_PART_SIZE_BYTES,
                        sha256: PART_HASH.into(),
                    },
                )
                .unwrap();
        }
        let duplicated = CompletedPart {
            part_number: 1,
            byte_size: MIN_MULTIPART_PART_SIZE_BYTES,
            sha256: PART_HASH.into(),
            etag: "etag-1".into(),
        };
        assert_eq!(
            service
                .complete_object(
                    &principal(10_000),
                    201,
                    CompleteObjectRequest {
                        scope,
                        upload_id: intent.upload_id,
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: key,
                        parts: vec![duplicated.clone(), duplicated],
                    },
                )
                .unwrap_err(),
            IngestError::DuplicatePart(1)
        );
    }

    #[test]
    fn duplicate_complete_is_idempotent_but_changed_evidence_is_not() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 900).unwrap();
        let scope = register(&mut service);
        let (intent, part) = complete_one(&mut service, &scope, "session/systeminfo.json", 200);
        let request = CompleteObjectRequest {
            scope: scope.clone(),
            upload_id: intent.upload_id.clone(),
            bucket: intent.bucket.clone(),
            object_key: intent.object_key.clone(),
            parts: vec![part.clone()],
        };
        service
            .complete_object(&principal(10_000), 500, request.clone())
            .unwrap();
        assert_eq!(service.storage.completed.len(), 1);
        let mut changed = request;
        changed.parts[0].etag = "different-etag".into();
        assert_eq!(
            service
                .complete_object(&principal(10_000), 500, changed)
                .unwrap_err(),
            IngestError::CompletionReplayMismatch
        );
    }

    #[test]
    fn manifest_finalize_checks_expiry_size_hash_and_exact_content() {
        let manifest = valid_manifest();
        let mut service = DepthIngestService::new(FakeStorage::default(), 10).unwrap();
        let scope = register(&mut service);
        complete_all_payloads(&mut service, &scope, &manifest);
        let manifest_key =
            manifest_object_key(scope.identity.lane, &scope.identity.session_id).unwrap();
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    299,
                    FinalizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap_err(),
            IngestError::ManifestNotAuthorized
        );
        service
            .authorize_manifest(
                &principal(10_000),
                300,
                AuthorizeManifestRequest {
                    scope: scope.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: manifest_key.clone(),
                },
            )
            .unwrap();
        service.storage.manifest = Some(VerifiedManifest {
            size_bytes: 4_096,
            sha256: MANIFEST_HASH.into(),
            manifest: manifest.clone(),
        });
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    310,
                    FinalizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap_err(),
            IngestError::ManifestAuthorizationExpired
        );
        service
            .authorize_manifest(
                &principal(10_000),
                311,
                AuthorizeManifestRequest {
                    scope: scope.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: manifest_key.clone(),
                },
            )
            .unwrap();
        service.storage.manifest.as_mut().unwrap().size_bytes = 4_095;
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    312,
                    FinalizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap_err(),
            IngestError::ManifestSizeMismatch
        );
        service.storage.manifest.as_mut().unwrap().size_bytes = 4_096;
        service.storage.manifest.as_mut().unwrap().sha256 = OBJECT_HASH.into();
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    312,
                    FinalizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap_err(),
            IngestError::ManifestSha256Mismatch
        );
        let mut mismatched_manifest = manifest.clone();
        mismatched_manifest.objects[0].sha256 =
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".into();
        service.storage.manifest = Some(VerifiedManifest {
            size_bytes: 4_096,
            sha256: MANIFEST_HASH.into(),
            manifest: mismatched_manifest,
        });
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    313,
                    FinalizeManifestRequest {
                        scope: scope.clone(),
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key.clone(),
                    },
                )
                .unwrap_err(),
            IngestError::ManifestContentMismatch
        );
        service.storage.manifest = Some(VerifiedManifest {
            size_bytes: 4_096,
            sha256: MANIFEST_HASH.into(),
            manifest,
        });
        assert_eq!(
            service
                .finalize_manifest(
                    &principal(10_000),
                    314,
                    FinalizeManifestRequest {
                        scope,
                        bucket: ARCHIVE_BUCKET.into(),
                        object_key: manifest_key,
                    },
                )
                .unwrap()
                .phase,
            ArchivePhase::Finalized
        );
    }

    #[test]
    fn orphan_cleanup_waits_for_expiry_plus_grace_and_never_aborts_completed() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 10).unwrap();
        let scope = register(&mut service);
        let stale = create_upload(&mut service, &scope, "session/systeminfo.json", 200);
        authorize_single_part(&mut service, &scope, &stale, 200);
        complete_one(&mut service, &scope, "session/dsv-thumbnail.png", 200);
        assert!(service.abort_expired_multipart(219, 10).unwrap().is_empty());
        let aborted = service.abort_expired_multipart(220, 10).unwrap();
        assert_eq!(aborted, vec![stale.upload_id]);
        assert_eq!(service.storage.aborted, vec![stale.object_key]);
    }

    #[test]
    fn durable_snapshot_round_trips_without_replaying_provider_calls() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 10).unwrap();
        let scope = register(&mut service);
        create_upload(&mut service, &scope, "session/systeminfo.json", 200);
        let snapshot = service.snapshot();

        let restored =
            DepthIngestService::from_snapshot(FakeStorage::default(), 10, snapshot).unwrap();
        let status = restored.status(&principal(10_000), 201, &scope).unwrap();
        assert_eq!(status.phase, ArchivePhase::Uploading);
        assert_eq!(status.active_multipart_count, 1);
    }

    #[test]
    fn durable_snapshot_rejects_tampered_scope_binding() {
        let mut service = DepthIngestService::new(FakeStorage::default(), 10).unwrap();
        let scope = register(&mut service);
        let mut value = serde_json::to_value(service.snapshot()).unwrap();
        value["archives"][&scope.archive_id]["scope"]["session_id"] =
            serde_json::Value::String("other-session".into());
        let snapshot = serde_json::from_value(value).unwrap();

        assert!(matches!(
            DepthIngestService::from_snapshot(FakeStorage::default(), 10, snapshot),
            Err(IngestError::InvalidStateSnapshot)
        ));
    }
}
