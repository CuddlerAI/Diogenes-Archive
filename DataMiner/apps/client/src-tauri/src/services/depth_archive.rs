//! U2：ConfirmedProbeFlush 后的 evidence helper 与独立 depth archive worker。
//!
//! 本模块只在 `depth-lab` feature 编译。它不读取 `credentials`、不调用 production
//! uploader/API，也不注册 Tauri command；#541 HTTP 适配器通过 [`DepthIngestClient`]
//! 接入，测试仍可用同一端口驱动真实 #535 状态机。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use depth_lab::archive_contract::ingest::{
    ArchiveIdentity, ArchivePhase, ArchiveScope, ArchiveStatus, AuthorizeManifestRequest,
    AuthorizePartRequest, AuthorizedManifest, AuthorizedPart, CompleteObjectRequest, CompletedPart,
    CreateArchiveRequest, CreateMultipartRequest, DEPTH_INGEST_AUDIENCE, DepthPrincipal,
    FinalizeManifestRequest, MultipartIntent,
};
use depth_lab::archive_contract::{
    ARCHIVE_BUCKET, ARCHIVE_RETENTION_DAYS, ARCHIVE_SCHEMA_VERSION, ArchivePriority,
    manifest_object_key, object_key,
};
use depth_lab::client_archive::{
    ArchiveBuildRequest, ArchiveSupplementFiles, FinalizeEvidencePaths, MaterializedArchive,
    SegmentCandidateSource, load_finalize_evidence, materialize_archive,
    verify_materialized_archive,
};
use depth_lab::segment_finalize::{StreamIdentity, VerifiedPairingEvidence};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::services::depth_lab::{
    ConfirmedProbeFlush, FinalizeDepthRunRequest, finalize_run_segments,
};
use crate::services::journal::{
    DepthArchiveJob, DepthArchiveJobState, DepthArchiveLease, SessionJournal,
};

const UPLOAD_PROGRESS_SCHEMA: &str = "dm-depth-upload-progress-1";
const DEFAULT_RETRY_MAX_MS: u64 = 5 * 60 * 1_000;

pub struct FinalizeConfirmedEvidenceRequest<'a> {
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub generation: u64,
    pub probe_root: &'a Path,
    pub run_directory: &'a Path,
    pub flushed: &'a ConfirmedProbeFlush,
    pub evidence: &'a FinalizeEvidencePaths,
}

/// 只有拿到 #536 `ConfirmedProbeFlush` 才能进入；缺任一 evidence 文件或其内部
/// report/state/coverage 不一致时，#529 finalizer fail-closed，不产生 queue。
pub fn finalize_confirmed_evidence(
    journal: &SessionJournal,
    request: &FinalizeConfirmedEvidenceRequest<'_>,
) -> Result<Vec<depth_lab::segment_finalize::PublishedDepthSegment>, String> {
    let evidence = load_finalize_evidence(request.evidence).map_err(|error| error.to_string())?;
    finalize_run_segments(
        journal,
        &FinalizeDepthRunRequest {
            session_id: request.session_id,
            run_id: request.run_id,
            generation: request.generation,
            probe_root: request.probe_root,
            run_directory: request.run_directory,
            flushed: request.flushed,
            frame_states: &evidence.frame_states,
            pairs: &evidence.pairs,
            report: &evidence.report,
            raw_evidence: None,
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepthIngestClientError {
    PrincipalExpired,
    GrantExpired,
    AlreadyExists,
    Remote(String),
}

impl std::fmt::Display for DepthIngestClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrincipalExpired => write!(formatter, "depth principal expired"),
            Self::GrantExpired => write!(formatter, "depth short-lived grant expired"),
            Self::AlreadyExists => write!(formatter, "immutable remote object already exists"),
            Self::Remote(message) => write!(formatter, "depth ingest remote error: {message}"),
        }
    }
}

impl std::error::Error for DepthIngestClientError {}

/// 未来 HTTP adapter 的唯一端口。参数直接复用 #535 纯协议类型；`put_*` 只消费
/// 该协议返回的短期 grant。实现不得读取生产 uploader credential。
pub trait DepthIngestClient {
    fn create_archive(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateArchiveRequest,
    ) -> Result<ArchiveScope, DepthIngestClientError>;

    fn create_multipart(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateMultipartRequest,
    ) -> Result<MultipartIntent, DepthIngestClientError>;

    fn authorize_part(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizePartRequest,
    ) -> Result<AuthorizedPart, DepthIngestClientError>;

    fn put_part(
        &mut self,
        authorization: &AuthorizedPart,
        bytes: &[u8],
    ) -> Result<String, DepthIngestClientError>;

    /// #535 `complete_object` 内部必须执行全对象下载回读 SHA-256；不是 HEAD/ETag。
    fn complete_object(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CompleteObjectRequest,
    ) -> Result<ArchiveStatus, DepthIngestClientError>;

    fn authorize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizeManifestRequest,
    ) -> Result<AuthorizedManifest, DepthIngestClientError>;

    fn put_manifest(
        &mut self,
        authorization: &AuthorizedManifest,
        bytes: &[u8],
    ) -> Result<(), DepthIngestClientError>;

    fn finalize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: FinalizeManifestRequest,
    ) -> Result<ArchiveStatus, DepthIngestClientError>;

    fn status(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        scope: &ArchiveScope,
    ) -> Result<ArchiveStatus, DepthIngestClientError>;
}

#[derive(Debug, Clone)]
pub struct DepthArchiveLocalInputs {
    pub run_directory: PathBuf,
    pub output_root: PathBuf,
    pub evidence: FinalizeEvidencePaths,
    pub supplements: ArchiveSupplementFiles,
}

pub trait DepthArchiveInputResolver {
    fn resolve(&self, job: &DepthArchiveJob) -> Result<DepthArchiveLocalInputs, String>;
}

/// U2 durable state port。生产实现只委托独立 journal 表；测试可在不伪造
/// `ConfirmedProbeFlush`、不触碰 production upload_jobs 的情况下验证远端状态机。
pub trait DepthArchiveStateStore {
    fn claim_next(
        &self,
        now_ms: u64,
        lease_owner: &str,
        lease_duration_ms: u64,
    ) -> anyhow::Result<Option<DepthArchiveLease>>;
    fn job(&self, job_id: &str) -> anyhow::Result<Option<DepthArchiveJob>>;
    fn renew(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        lease_duration_ms: u64,
    ) -> anyhow::Result<()>;
    fn save_materialization(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        materialized_json: &str,
        progress_json: &str,
    ) -> anyhow::Result<()>;
    fn save_remote_scope(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        remote_scope_json: &str,
    ) -> anyhow::Result<()>;
    fn save_progress(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        state: DepthArchiveJobState,
        progress_json: &str,
    ) -> anyhow::Result<()>;
    fn retry(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        delay_ms: u64,
        error_code: &str,
        error_message: &str,
    ) -> anyhow::Result<()>;
    fn complete(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        remote_phase: &str,
    ) -> anyhow::Result<()>;
}

impl DepthArchiveStateStore for SessionJournal {
    fn claim_next(
        &self,
        now_ms: u64,
        lease_owner: &str,
        lease_duration_ms: u64,
    ) -> anyhow::Result<Option<DepthArchiveLease>> {
        self.claim_next_depth_archive_job(now_ms, lease_owner, lease_duration_ms)
    }

    fn job(&self, job_id: &str) -> anyhow::Result<Option<DepthArchiveJob>> {
        self.depth_archive_job(job_id)
    }

    fn renew(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        lease_duration_ms: u64,
    ) -> anyhow::Result<()> {
        self.renew_depth_archive_lease(job_id, lease_owner, now_ms, lease_duration_ms)
    }

    fn save_materialization(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        materialized_json: &str,
        progress_json: &str,
    ) -> anyhow::Result<()> {
        self.save_depth_archive_materialization(
            job_id,
            lease_owner,
            now_ms,
            materialized_json,
            progress_json,
        )
    }

    fn save_remote_scope(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        remote_scope_json: &str,
    ) -> anyhow::Result<()> {
        self.save_depth_archive_remote_scope(job_id, lease_owner, now_ms, remote_scope_json)
    }

    fn save_progress(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        state: DepthArchiveJobState,
        progress_json: &str,
    ) -> anyhow::Result<()> {
        self.save_depth_archive_progress(job_id, lease_owner, now_ms, state, progress_json)
    }

    fn retry(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        delay_ms: u64,
        error_code: &str,
        error_message: &str,
    ) -> anyhow::Result<()> {
        self.retry_depth_archive_job(
            job_id,
            lease_owner,
            now_ms,
            delay_ms,
            error_code,
            error_message,
        )
    }

    fn complete(
        &self,
        job_id: &str,
        lease_owner: &str,
        now_ms: u64,
        remote_phase: &str,
    ) -> anyhow::Result<()> {
        self.complete_depth_archive_job(job_id, lease_owner, now_ms, remote_phase)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepthArchiveWorkerConfig {
    pub lease_owner: String,
    pub lease_duration_ms: u64,
    pub part_size_bytes: u64,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
}

impl Default for DepthArchiveWorkerConfig {
    fn default() -> Self {
        Self {
            lease_owner: "depth-helper".into(),
            lease_duration_ms: 120_000,
            part_size_bytes: 8 * 1024 * 1024,
            retry_base_ms: 1_000,
            retry_max_ms: DEFAULT_RETRY_MAX_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepthArchiveWorkerOutcome {
    Idle,
    Finalized {
        job_id: String,
    },
    RetryScheduled {
        job_id: String,
        error_code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadProgress {
    schema: String,
    objects: BTreeMap<String, ObjectProgress>,
    manifest_uploaded: bool,
}

impl UploadProgress {
    fn new(archive: &MaterializedArchive) -> Self {
        Self {
            schema: UPLOAD_PROGRESS_SCHEMA.into(),
            objects: archive
                .objects
                .iter()
                .map(|object| (object.relative_key.clone(), ObjectProgress::default()))
                .collect(),
            manifest_uploaded: false,
        }
    }

    fn validate_for(&self, archive: &MaterializedArchive) -> Result<(), String> {
        if self.schema != UPLOAD_PROGRESS_SCHEMA
            || self.objects.len() != archive.objects.len()
            || !archive
                .objects
                .iter()
                .all(|object| self.objects.contains_key(&object.relative_key))
        {
            return Err("depth upload progress does not match materialized objects".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectProgress {
    upload_id: Option<String>,
    part_size_bytes: Option<u64>,
    completed_parts: Vec<CompletedPart>,
    verified: bool,
}

struct WorkerTimes {
    now_ms: u64,
    now_epoch_seconds: u64,
}

pub fn process_next_depth_archive<C: DepthIngestClient, R: DepthArchiveInputResolver>(
    journal: &dyn DepthArchiveStateStore,
    client: &mut C,
    resolver: &R,
    principal: &DepthPrincipal,
    config: &DepthArchiveWorkerConfig,
    now_ms: u64,
    now_epoch_seconds: u64,
) -> Result<DepthArchiveWorkerOutcome, String> {
    validate_worker_inputs(principal, config, now_epoch_seconds)?;
    let Some(lease) = journal
        .claim_next(now_ms, &config.lease_owner, config.lease_duration_ms)
        .map_err(|error| format!("领取 depth archive queue 失败: {error:#}"))?
    else {
        return Ok(DepthArchiveWorkerOutcome::Idle);
    };
    let job_id = lease.job.job_id.clone();
    let times = WorkerTimes {
        now_ms,
        now_epoch_seconds,
    };
    match process_leased_job(journal, client, resolver, principal, config, &times, lease) {
        Ok(()) => Ok(DepthArchiveWorkerOutcome::Finalized { job_id }),
        Err(error) => {
            let error_code = error.code().to_string();
            let message = error.to_string();
            let attempt = journal
                .job(&job_id)
                .map_err(|load| format!("读取 depth archive retry 状态失败: {load:#}"))?
                .map(|job| job.attempt_count)
                .unwrap_or(1);
            let delay = retry_delay(config, attempt);
            journal
                .retry(
                    &job_id,
                    &config.lease_owner,
                    now_ms,
                    delay,
                    &error_code,
                    &message,
                )
                .map_err(|persist| {
                    format!("depth archive 失败且 retry 状态落盘失败: {message}; {persist:#}")
                })?;
            Ok(DepthArchiveWorkerOutcome::RetryScheduled {
                job_id,
                error_code,
                message,
            })
        }
    }
}

#[derive(Debug)]
enum WorkerError {
    Principal(String),
    Local(String),
    Remote(DepthIngestClientError),
}

impl WorkerError {
    fn code(&self) -> &'static str {
        match self {
            Self::Principal(_) => "DEPTH_PRINCIPAL_INVALID",
            Self::Local(_) => "DEPTH_LOCAL_INTEGRITY",
            Self::Remote(DepthIngestClientError::PrincipalExpired) => "DEPTH_PRINCIPAL_EXPIRED",
            Self::Remote(DepthIngestClientError::GrantExpired) => "DEPTH_GRANT_EXPIRED",
            Self::Remote(DepthIngestClientError::AlreadyExists) => "DEPTH_REMOTE_EXISTS",
            Self::Remote(DepthIngestClientError::Remote(_)) => "DEPTH_REMOTE_FAILED",
        }
    }
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Principal(message) | Self::Local(message) => formatter.write_str(message),
            Self::Remote(error) => write!(formatter, "{error}"),
        }
    }
}

fn process_leased_job<C: DepthIngestClient, R: DepthArchiveInputResolver>(
    journal: &dyn DepthArchiveStateStore,
    client: &mut C,
    resolver: &R,
    principal: &DepthPrincipal,
    config: &DepthArchiveWorkerConfig,
    times: &WorkerTimes,
    lease: DepthArchiveLease,
) -> Result<(), WorkerError> {
    let job_id = lease.job.job_id.clone();
    let (archive, mut progress) = load_or_materialize(journal, resolver, config, times, &lease)?;
    verify_materialized_archive(&archive).map_err(|error| WorkerError::Local(error.to_string()))?;
    let mut scope = match lease.job.remote_scope_json.as_deref() {
        Some(json) => serde_json::from_str::<ArchiveScope>(json)
            .map_err(|error| WorkerError::Local(format!("解析远端 scope 失败: {error}")))?,
        None => {
            renew(journal, &job_id, config, times.now_ms)?;
            let identity = ArchiveIdentity {
                principal_id: principal.subject.clone(),
                session_id: archive.identity.session_id.clone(),
                run_id: archive.identity.run_id.clone(),
                generation: archive.identity.generation,
                lane: archive.lane,
                non_trainable: true,
                schema_version: ARCHIVE_SCHEMA_VERSION.into(),
                manifest_sha256: archive.manifest_sha256.clone(),
            };
            let scope = client
                .create_archive(
                    principal,
                    times.now_epoch_seconds,
                    CreateArchiveRequest {
                        identity,
                        target_bucket: ARCHIVE_BUCKET.into(),
                        retention_days: ARCHIVE_RETENTION_DAYS,
                        manifest_size_bytes: archive.manifest_size_bytes,
                        manifest: archive.manifest.clone(),
                    },
                )
                .map_err(WorkerError::Remote)?;
            let json = serde_json::to_string(&scope)
                .map_err(|error| WorkerError::Local(format!("序列化远端 scope 失败: {error}")))?;
            journal
                .save_remote_scope(&job_id, &config.lease_owner, times.now_ms, &json)
                .map_err(|error| WorkerError::Local(format!("保存远端 scope 失败: {error:#}")))?;
            scope
        }
    };
    validate_scope(&scope, &archive, principal)?;

    let status = client
        .status(principal, times.now_epoch_seconds, &scope)
        .map_err(WorkerError::Remote)?;
    validate_scope(&status.scope, &archive, principal)?;
    if status.phase == ArchivePhase::Finalized {
        persist_progress(
            journal,
            &job_id,
            config,
            times.now_ms,
            DepthArchiveJobState::Uploading,
            &progress,
        )?;
        return complete_local(journal, &job_id, config, times.now_ms);
    }

    let mut objects = archive.objects.clone();
    objects.sort_by_key(|object| match object.priority {
        ArchivePriority::P0 => (0_u8, object.relative_key.clone()),
        ArchivePriority::P1 => (1_u8, object.relative_key.clone()),
        ArchivePriority::P2 => (2_u8, object.relative_key.clone()),
    });
    for object in &objects {
        if progress
            .objects
            .get(&object.relative_key)
            .is_some_and(|state| state.verified)
        {
            continue;
        }
        renew(journal, &job_id, config, times.now_ms)?;
        upload_object(
            client,
            &UploadContext {
                journal,
                principal,
                config,
                times,
                job_id: &job_id,
                scope: &scope,
            },
            object,
            &mut progress,
        )?;
    }

    let status = client
        .status(principal, times.now_epoch_seconds, &scope)
        .map_err(WorkerError::Remote)?;
    validate_scope(&status.scope, &archive, principal)?;
    if status.phase != ArchivePhase::PayloadVerified
        && status.phase != ArchivePhase::ManifestAuthorized
    {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            format!(
                "payload upload ended in unexpected phase {:?}",
                status.phase
            ),
        )));
    }
    let manifest_key = manifest_object_key(archive.lane, &archive.identity.session_id)
        .map_err(|error| WorkerError::Local(format!("构造 manifest key 失败: {error:?}")))?;
    let authorization = client
        .authorize_manifest(
            principal,
            times.now_epoch_seconds,
            AuthorizeManifestRequest {
                scope: scope.clone(),
                bucket: ARCHIVE_BUCKET.into(),
                object_key: manifest_key.clone(),
            },
        )
        .map_err(WorkerError::Remote)?;
    ensure_live_manifest_grant(
        &authorization,
        &archive,
        &manifest_key,
        times.now_epoch_seconds,
    )?;
    persist_progress(
        journal,
        &job_id,
        config,
        times.now_ms,
        DepthArchiveJobState::ManifestAuthorized,
        &progress,
    )?;
    let manifest_bytes = std::fs::read(&archive.manifest_path)
        .map_err(|error| WorkerError::Local(format!("读取本地 manifest 失败: {error}")))?;
    match client.put_manifest(&authorization, &manifest_bytes) {
        Ok(()) | Err(DepthIngestClientError::AlreadyExists) => {
            progress.manifest_uploaded = true;
            persist_progress(
                journal,
                &job_id,
                config,
                times.now_ms,
                DepthArchiveJobState::ManifestAuthorized,
                &progress,
            )?;
        }
        Err(error) => return Err(WorkerError::Remote(error)),
    }
    let finalized = client
        .finalize_manifest(
            principal,
            times.now_epoch_seconds,
            FinalizeManifestRequest {
                scope: scope.clone(),
                bucket: ARCHIVE_BUCKET.into(),
                object_key: manifest_key,
            },
        )
        .map_err(WorkerError::Remote)?;
    if finalized.phase != ArchivePhase::Finalized {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            format!("manifest finalize returned {:?}", finalized.phase),
        )));
    }
    scope = finalized.scope;
    validate_scope(&scope, &archive, principal)?;
    complete_local(journal, &job_id, config, times.now_ms)
}

fn load_or_materialize<R: DepthArchiveInputResolver>(
    journal: &dyn DepthArchiveStateStore,
    resolver: &R,
    config: &DepthArchiveWorkerConfig,
    times: &WorkerTimes,
    lease: &DepthArchiveLease,
) -> Result<(MaterializedArchive, UploadProgress), WorkerError> {
    match lease.job.materialized_json.as_deref() {
        Some(json) => {
            let archive = serde_json::from_str::<MaterializedArchive>(json).map_err(|error| {
                WorkerError::Local(format!("解析本地 archive 状态失败: {error}"))
            })?;
            let progress = lease
                .job
                .progress_json
                .as_deref()
                .ok_or_else(|| WorkerError::Local("materialized job 缺 upload progress".into()))
                .and_then(|json| {
                    serde_json::from_str::<UploadProgress>(json).map_err(|error| {
                        WorkerError::Local(format!("解析 upload progress 失败: {error}"))
                    })
                })?;
            progress
                .validate_for(&archive)
                .map_err(WorkerError::Local)?;
            Ok((archive, progress))
        }
        None => {
            let inputs = resolver.resolve(&lease.job).map_err(WorkerError::Local)?;
            let evidence = load_finalize_evidence(&inputs.evidence)
                .map_err(|error| WorkerError::Local(error.to_string()))?;
            let identity = StreamIdentity::new(
                &lease.job.session_id,
                &lease.job.run_id,
                lease.job.generation,
            )
            .map_err(|error| WorkerError::Local(error.to_string()))?;
            evidence
                .frame_states
                .validate_for(&identity)
                .map_err(|error| WorkerError::Local(error.to_string()))?;
            VerifiedPairingEvidence::new(identity.clone(), evidence.pairs.clone(), evidence.report)
                .map_err(|error| WorkerError::Local(error.to_string()))?;
            let candidates = resolve_candidate_sources(&inputs.run_directory, lease)?;
            let lane = candidate_lane(&lease.job, &inputs, &candidates)?;
            let archive = materialize_archive(&ArchiveBuildRequest {
                identity: &identity,
                lane,
                segments: &candidates,
                frame_pairs: &evidence.pairs,
                supplements: &inputs.supplements,
                output_root: &inputs.output_root,
            })
            .map_err(|error| WorkerError::Local(error.to_string()))?;
            let progress = UploadProgress::new(&archive);
            let archive_json = serde_json::to_string(&archive).map_err(|error| {
                WorkerError::Local(format!("序列化 materialized archive 失败: {error}"))
            })?;
            let progress_json = serde_json::to_string(&progress).map_err(|error| {
                WorkerError::Local(format!("序列化 upload progress 失败: {error}"))
            })?;
            journal
                .save_materialization(
                    &lease.job.job_id,
                    &config.lease_owner,
                    times.now_ms,
                    &archive_json,
                    &progress_json,
                )
                .map_err(|error| {
                    WorkerError::Local(format!("保存 archive 物化状态失败: {error:#}"))
                })?;
            Ok((archive, progress))
        }
    }
}

fn resolve_candidate_sources(
    run_directory: &Path,
    lease: &DepthArchiveLease,
) -> Result<Vec<SegmentCandidateSource>, WorkerError> {
    if !run_directory.is_absolute() {
        return Err(WorkerError::Local("run directory 必须是绝对路径".into()));
    }
    lease
        .candidates
        .iter()
        .map(|candidate| {
            let relative = Path::new(&candidate.manifest_relative_path);
            if relative.is_absolute()
                || !relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                return Err(WorkerError::Local(format!(
                    "queue manifest 路径不安全: {}",
                    candidate.manifest_relative_path
                )));
            }
            Ok(SegmentCandidateSource {
                segment_index: candidate.segment_index,
                manifest_path: run_directory.join(relative),
                manifest_sha256: candidate.manifest_sha256.clone(),
            })
        })
        .collect()
}

fn candidate_lane(
    job: &DepthArchiveJob,
    inputs: &DepthArchiveLocalInputs,
    candidates: &[SegmentCandidateSource],
) -> Result<depth_lab::archive_contract::ArchiveLane, WorkerError> {
    let first = candidates
        .first()
        .ok_or_else(|| WorkerError::Local("depth archive 没有 candidate".into()))?;
    // 只为取得 adapter lane；完整对象复核仍由 materialize_archive 逐段执行。
    let text = std::fs::read_to_string(&first.manifest_path)
        .map_err(|error| WorkerError::Local(format!("读取 segment manifest 失败: {error}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| WorkerError::Local(format!("解析 segment manifest 失败: {error}")))?;
    let lane = value
        .pointer("/archive_adapter/lane")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WorkerError::Local("candidate 缺 archive lane".into()))?;
    let lane = match lane {
        "lane-30" => depth_lab::archive_contract::ArchiveLane::Lane30,
        "lane-60" => depth_lab::archive_contract::ArchiveLane::Lane60,
        _ => return Err(WorkerError::Local(format!("未知 archive lane: {lane}"))),
    };
    if inputs
        .output_root
        .starts_with(inputs.run_directory.join("depth-segments"))
    {
        return Err(WorkerError::Local(
            "最终 archive 不能写进 #529 segment candidate 目录".into(),
        ));
    }
    if job.run_id.trim().is_empty() {
        return Err(WorkerError::Local("depth archive run identity 为空".into()));
    }
    Ok(lane)
}

struct UploadContext<'a> {
    journal: &'a dyn DepthArchiveStateStore,
    principal: &'a DepthPrincipal,
    config: &'a DepthArchiveWorkerConfig,
    times: &'a WorkerTimes,
    job_id: &'a str,
    scope: &'a ArchiveScope,
}

fn upload_object<C: DepthIngestClient>(
    client: &mut C,
    context: &UploadContext<'_>,
    object: &depth_lab::client_archive::LocalArchiveObject,
    progress: &mut UploadProgress,
) -> Result<(), WorkerError> {
    let key = object_key(
        context.scope.identity.lane,
        &context.scope.identity.session_id,
        &object.relative_key,
    )
    .map_err(|error| WorkerError::Local(format!("构造 object key 失败: {error:?}")))?;
    let intent = client
        .create_multipart(
            context.principal,
            context.times.now_epoch_seconds,
            CreateMultipartRequest {
                scope: context.scope.clone(),
                bucket: ARCHIVE_BUCKET.into(),
                object_key: key.clone(),
                part_size_bytes: context.config.part_size_bytes,
            },
        )
        .map_err(WorkerError::Remote)?;
    validate_intent(&intent, object, &key, context.config.part_size_bytes)?;
    let state = progress
        .objects
        .get_mut(&object.relative_key)
        .expect("progress shape validated");
    if intent.already_verified {
        state.upload_id = Some(intent.upload_id);
        state.part_size_bytes = Some(intent.part_size_bytes);
        state.verified = true;
        return persist_progress(
            context.journal,
            context.job_id,
            context.config,
            context.times.now_ms,
            DepthArchiveJobState::Uploading,
            progress,
        );
    }
    if state
        .upload_id
        .as_ref()
        .is_some_and(|upload_id| upload_id != &intent.upload_id)
        || state
            .part_size_bytes
            .is_some_and(|part_size| part_size != intent.part_size_bytes)
    {
        return Err(WorkerError::Local(format!(
            "远端 multipart identity 改变: {}",
            object.relative_key
        )));
    }
    state.upload_id = Some(intent.upload_id.clone());
    state.part_size_bytes = Some(intent.part_size_bytes);
    let existing: BTreeMap<u32, CompletedPart> = state
        .completed_parts
        .iter()
        .cloned()
        .map(|part| (part.part_number, part))
        .collect();
    if existing.len() != state.completed_parts.len() {
        return Err(WorkerError::Local("durable multipart 含重复 part".into()));
    }
    for part_number in 1..=intent.part_count {
        if existing.contains_key(&part_number) {
            continue;
        }
        renew(
            context.journal,
            context.job_id,
            context.config,
            context.times.now_ms,
        )?;
        let offset = u64::from(part_number - 1) * intent.part_size_bytes;
        let byte_size = if part_number < intent.part_count {
            intent.part_size_bytes
        } else {
            object.size_bytes - offset
        };
        let bytes = read_part(&object.local_path, offset, byte_size)?;
        let sha256 = sha256_bytes(&bytes);
        let authorization = client
            .authorize_part(
                context.principal,
                context.times.now_epoch_seconds,
                AuthorizePartRequest {
                    scope: context.scope.clone(),
                    upload_id: intent.upload_id.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: key.clone(),
                    part_number,
                    byte_size,
                    sha256: sha256.clone(),
                },
            )
            .map_err(WorkerError::Remote)?;
        validate_part_authorization(
            &authorization,
            &intent,
            part_number,
            byte_size,
            &sha256,
            context.times.now_epoch_seconds,
        )?;
        let etag = client
            .put_part(&authorization, &bytes)
            .map_err(WorkerError::Remote)?;
        if etag.trim().is_empty() {
            return Err(WorkerError::Remote(DepthIngestClientError::Remote(
                "part PUT returned an empty ETag".into(),
            )));
        }
        progress
            .objects
            .get_mut(&object.relative_key)
            .expect("progress shape validated")
            .completed_parts
            .push(CompletedPart {
                part_number,
                byte_size,
                sha256,
                etag,
            });
        persist_progress(
            context.journal,
            context.job_id,
            context.config,
            context.times.now_ms,
            DepthArchiveJobState::Uploading,
            progress,
        )?;
    }
    // #535 在 complete 时要求每个 authorization 仍有效。长上传/重启后重新授权已上传
    // part（不重复 PUT），刷新最短期 grant 的时钟边界。
    let completed = progress
        .objects
        .get(&object.relative_key)
        .expect("progress shape validated")
        .completed_parts
        .clone();
    for part in &completed {
        let authorization = client
            .authorize_part(
                context.principal,
                context.times.now_epoch_seconds,
                AuthorizePartRequest {
                    scope: context.scope.clone(),
                    upload_id: intent.upload_id.clone(),
                    bucket: ARCHIVE_BUCKET.into(),
                    object_key: key.clone(),
                    part_number: part.part_number,
                    byte_size: part.byte_size,
                    sha256: part.sha256.clone(),
                },
            )
            .map_err(WorkerError::Remote)?;
        validate_part_authorization(
            &authorization,
            &intent,
            part.part_number,
            part.byte_size,
            &part.sha256,
            context.times.now_epoch_seconds,
        )?;
    }
    let status = client
        .complete_object(
            context.principal,
            context.times.now_epoch_seconds,
            CompleteObjectRequest {
                scope: context.scope.clone(),
                upload_id: intent.upload_id,
                bucket: ARCHIVE_BUCKET.into(),
                object_key: key,
                parts: completed,
            },
        )
        .map_err(WorkerError::Remote)?;
    if status.scope != *context.scope {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            "object verify returned a mismatched scope".into(),
        )));
    }
    if !matches!(
        status.phase,
        ArchivePhase::Uploading | ArchivePhase::PayloadVerified
    ) {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            format!("object verify returned unexpected phase {:?}", status.phase),
        )));
    }
    progress
        .objects
        .get_mut(&object.relative_key)
        .expect("progress shape validated")
        .verified = true;
    persist_progress(
        context.journal,
        context.job_id,
        context.config,
        context.times.now_ms,
        DepthArchiveJobState::Uploading,
        progress,
    )
}

fn persist_progress(
    journal: &dyn DepthArchiveStateStore,
    job_id: &str,
    config: &DepthArchiveWorkerConfig,
    now_ms: u64,
    state: DepthArchiveJobState,
    progress: &UploadProgress,
) -> Result<(), WorkerError> {
    let json = serde_json::to_string(progress)
        .map_err(|error| WorkerError::Local(format!("序列化 upload progress 失败: {error}")))?;
    journal
        .save_progress(job_id, &config.lease_owner, now_ms, state, &json)
        .map_err(|error| WorkerError::Local(format!("持久化 upload progress 失败: {error:#}")))
}

fn complete_local(
    journal: &dyn DepthArchiveStateStore,
    job_id: &str,
    config: &DepthArchiveWorkerConfig,
    now_ms: u64,
) -> Result<(), WorkerError> {
    journal
        .complete(job_id, &config.lease_owner, now_ms, "finalized")
        .map_err(|error| WorkerError::Local(format!("提交 depth archive 完成状态失败: {error:#}")))
}

fn renew(
    journal: &dyn DepthArchiveStateStore,
    job_id: &str,
    config: &DepthArchiveWorkerConfig,
    now_ms: u64,
) -> Result<(), WorkerError> {
    journal
        .renew(
            job_id,
            &config.lease_owner,
            now_ms,
            config.lease_duration_ms,
        )
        .map_err(|error| WorkerError::Local(format!("续租 depth archive lease 失败: {error:#}")))
}

fn validate_worker_inputs(
    principal: &DepthPrincipal,
    config: &DepthArchiveWorkerConfig,
    now_epoch_seconds: u64,
) -> Result<(), String> {
    if principal.audience != DEPTH_INGEST_AUDIENCE
        || principal.subject.trim().is_empty()
        || principal.expires_at_epoch_seconds <= now_epoch_seconds
    {
        return Err("depth worker 只接受有效的独立 depth principal".into());
    }
    if config.lease_owner.trim().is_empty()
        || config.lease_duration_ms == 0
        || config.part_size_bytes
            < depth_lab::archive_contract::ingest::MIN_MULTIPART_PART_SIZE_BYTES
        || config.retry_base_ms == 0
        || config.retry_max_ms < config.retry_base_ms
    {
        return Err("depth archive worker 配置无效".into());
    }
    Ok(())
}

fn validate_scope(
    scope: &ArchiveScope,
    archive: &MaterializedArchive,
    principal: &DepthPrincipal,
) -> Result<(), WorkerError> {
    if scope.identity.principal_id != principal.subject
        || scope.identity.session_id != archive.identity.session_id
        || scope.identity.run_id != archive.identity.run_id
        || scope.identity.generation != archive.identity.generation
        || scope.identity.lane != archive.lane
        || !scope.identity.non_trainable
        || scope.identity.schema_version != ARCHIVE_SCHEMA_VERSION
        || scope.identity.manifest_sha256 != archive.manifest_sha256
    {
        return Err(WorkerError::Principal(
            "remote scope 与独立 depth principal/local archive 身份不匹配".into(),
        ));
    }
    Ok(())
}

fn validate_intent(
    intent: &MultipartIntent,
    object: &depth_lab::client_archive::LocalArchiveObject,
    key: &str,
    part_size_bytes: u64,
) -> Result<(), WorkerError> {
    if intent.bucket != ARCHIVE_BUCKET
        || intent.object_key != key
        || intent.expected_size_bytes != object.size_bytes
        || intent.expected_sha256 != object.sha256
        || intent.part_size_bytes != part_size_bytes
        || intent.part_count == 0
    {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            "multipart intent 与 #524 object 不一致".into(),
        )));
    }
    Ok(())
}

fn validate_part_authorization(
    authorization: &AuthorizedPart,
    intent: &MultipartIntent,
    part_number: u32,
    byte_size: u64,
    sha256: &str,
    now_epoch_seconds: u64,
) -> Result<(), WorkerError> {
    if authorization.upload_id != intent.upload_id
        || authorization.bucket != intent.bucket
        || authorization.object_key != intent.object_key
        || authorization.part_number != part_number
        || authorization.byte_size != byte_size
        || authorization.sha256 != sha256
        || authorization.expected_object_size_bytes != intent.expected_size_bytes
        || authorization.expected_object_sha256 != intent.expected_sha256
    {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            "part authorization binding mismatch".into(),
        )));
    }
    if authorization.expires_at_epoch_seconds <= now_epoch_seconds {
        return Err(WorkerError::Remote(DepthIngestClientError::GrantExpired));
    }
    Ok(())
}

fn ensure_live_manifest_grant(
    authorization: &AuthorizedManifest,
    archive: &MaterializedArchive,
    expected_object_key: &str,
    now_epoch_seconds: u64,
) -> Result<(), WorkerError> {
    if authorization.bucket != ARCHIVE_BUCKET
        || authorization.object_key != expected_object_key
        || authorization.size_bytes != archive.manifest_size_bytes
        || authorization.sha256 != archive.manifest_sha256
    {
        return Err(WorkerError::Remote(DepthIngestClientError::Remote(
            "manifest authorization binding mismatch".into(),
        )));
    }
    if authorization.expires_at_epoch_seconds <= now_epoch_seconds {
        return Err(WorkerError::Remote(DepthIngestClientError::GrantExpired));
    }
    Ok(())
}

fn read_part(path: &Path, offset: u64, size: u64) -> Result<Vec<u8>, WorkerError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| WorkerError::Local(format!("stat upload object 失败: {error}")))?;
    let end = offset
        .checked_add(size)
        .ok_or_else(|| WorkerError::Local("upload part 范围溢出".into()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || end > metadata.len() {
        return Err(WorkerError::Local(format!(
            "upload part 越界或源文件不安全: {}",
            path.display()
        )));
    }
    let size = usize::try_from(size)
        .map_err(|_| WorkerError::Local("upload part 大小超出内存范围".into()))?;
    let mut bytes = vec![0_u8; size];
    let mut file = File::open(path)
        .map_err(|error| WorkerError::Local(format!("打开 upload object 失败: {error}")))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| WorkerError::Local(format!("定位 upload part 失败: {error}")))?;
    file.read_exact(&mut bytes)
        .map_err(|error| WorkerError::Local(format!("读取 upload part 失败: {error}")))?;
    Ok(bytes)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn retry_delay(config: &DepthArchiveWorkerConfig, attempt_count: u32) -> u64 {
    let shift = attempt_count.saturating_sub(1).min(16);
    config
        .retry_base_ms
        .saturating_mul(1_u64 << shift)
        .min(config.retry_max_ms)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use depth_lab::archive_contract::ingest::{
        DepthArchiveStorage, DepthIngestService, IngestError, MIN_MULTIPART_PART_SIZE_BYTES,
        PresignedGrant, StorageCompleteTarget, StorageManifestTarget, StorageObjectTarget,
        StoragePartTarget, VerifiedManifest, VerifiedObject,
    };
    use depth_lab::archive_contract::{
        ArchiveLane, ArchiveManifest, ArchiveObject, ArchiveObjectKind, ArchivePriority,
    };

    use super::*;

    struct NeverResolve;

    impl DepthArchiveInputResolver for NeverResolve {
        fn resolve(&self, _job: &DepthArchiveJob) -> Result<DepthArchiveLocalInputs, String> {
            Err("materialized recovery must not resolve raw inputs".into())
        }
    }

    #[derive(Clone)]
    struct MemoryStateStore {
        job: Arc<Mutex<DepthArchiveJob>>,
    }

    impl MemoryStateStore {
        fn new(archive: &MaterializedArchive) -> Self {
            let progress = UploadProgress::new(archive);
            Self {
                job: Arc::new(Mutex::new(DepthArchiveJob {
                    job_id: "depth-archive-job-1".into(),
                    session_id: archive.identity.session_id.clone(),
                    run_id: archive.identity.run_id.clone(),
                    generation: archive.identity.generation,
                    state: DepthArchiveJobState::Materialized,
                    attempt_count: 0,
                    next_attempt_at_ms: 0,
                    lease_owner: None,
                    lease_expires_at_ms: None,
                    materialized_json: Some(serde_json::to_string(archive).unwrap()),
                    remote_scope_json: None,
                    progress_json: Some(serde_json::to_string(&progress).unwrap()),
                    last_error_code: None,
                    last_error_message: None,
                })),
            }
        }

        fn snapshot(&self) -> DepthArchiveJob {
            self.job.lock().unwrap().clone()
        }

        fn require_lease(job: &DepthArchiveJob, owner: &str, now_ms: u64) -> anyhow::Result<()> {
            anyhow::ensure!(
                job.lease_owner.as_deref() == Some(owner),
                "wrong lease owner"
            );
            anyhow::ensure!(
                job.lease_expires_at_ms
                    .is_some_and(|expiry| expiry > now_ms),
                "lease expired"
            );
            Ok(())
        }
    }

    impl DepthArchiveStateStore for MemoryStateStore {
        fn claim_next(
            &self,
            now_ms: u64,
            lease_owner: &str,
            lease_duration_ms: u64,
        ) -> anyhow::Result<Option<DepthArchiveLease>> {
            let mut job = self.job.lock().unwrap();
            if job.state == DepthArchiveJobState::Finalized
                || job.next_attempt_at_ms > now_ms
                || job
                    .lease_expires_at_ms
                    .is_some_and(|expiry| expiry > now_ms)
            {
                return Ok(None);
            }
            if job.state == DepthArchiveJobState::RetryWait {
                job.state = DepthArchiveJobState::Pending;
            }
            job.attempt_count += 1;
            job.lease_owner = Some(lease_owner.into());
            job.lease_expires_at_ms = Some(now_ms + lease_duration_ms);
            job.last_error_code = None;
            job.last_error_message = None;
            Ok(Some(DepthArchiveLease {
                job: job.clone(),
                candidates: Vec::new(),
            }))
        }

        fn job(&self, job_id: &str) -> anyhow::Result<Option<DepthArchiveJob>> {
            let job = self.snapshot();
            Ok((job.job_id == job_id).then_some(job))
        }

        fn renew(
            &self,
            job_id: &str,
            lease_owner: &str,
            now_ms: u64,
            lease_duration_ms: u64,
        ) -> anyhow::Result<()> {
            let mut job = self.job.lock().unwrap();
            anyhow::ensure!(job.job_id == job_id, "wrong job");
            Self::require_lease(&job, lease_owner, now_ms)?;
            job.lease_expires_at_ms = Some(now_ms + lease_duration_ms);
            Ok(())
        }

        fn save_materialization(
            &self,
            _job_id: &str,
            _lease_owner: &str,
            _now_ms: u64,
            _materialized_json: &str,
            _progress_json: &str,
        ) -> anyhow::Result<()> {
            anyhow::bail!("materialization already exists")
        }

        fn save_remote_scope(
            &self,
            job_id: &str,
            lease_owner: &str,
            now_ms: u64,
            remote_scope_json: &str,
        ) -> anyhow::Result<()> {
            let mut job = self.job.lock().unwrap();
            anyhow::ensure!(job.job_id == job_id, "wrong job");
            Self::require_lease(&job, lease_owner, now_ms)?;
            if let Some(existing) = &job.remote_scope_json {
                anyhow::ensure!(existing == remote_scope_json, "scope changed");
            }
            job.remote_scope_json = Some(remote_scope_json.into());
            job.state = DepthArchiveJobState::Registered;
            Ok(())
        }

        fn save_progress(
            &self,
            job_id: &str,
            lease_owner: &str,
            now_ms: u64,
            state: DepthArchiveJobState,
            progress_json: &str,
        ) -> anyhow::Result<()> {
            let mut job = self.job.lock().unwrap();
            anyhow::ensure!(job.job_id == job_id, "wrong job");
            Self::require_lease(&job, lease_owner, now_ms)?;
            job.progress_json = Some(progress_json.into());
            job.state = state;
            Ok(())
        }

        fn retry(
            &self,
            job_id: &str,
            lease_owner: &str,
            now_ms: u64,
            delay_ms: u64,
            error_code: &str,
            error_message: &str,
        ) -> anyhow::Result<()> {
            let mut job = self.job.lock().unwrap();
            anyhow::ensure!(job.job_id == job_id, "wrong job");
            Self::require_lease(&job, lease_owner, now_ms)?;
            job.state = DepthArchiveJobState::RetryWait;
            job.next_attempt_at_ms = now_ms + delay_ms;
            job.lease_owner = None;
            job.lease_expires_at_ms = None;
            job.last_error_code = Some(error_code.into());
            job.last_error_message = Some(error_message.into());
            Ok(())
        }

        fn complete(
            &self,
            job_id: &str,
            lease_owner: &str,
            now_ms: u64,
            remote_phase: &str,
        ) -> anyhow::Result<()> {
            let mut job = self.job.lock().unwrap();
            anyhow::ensure!(job.job_id == job_id, "wrong job");
            Self::require_lease(&job, lease_owner, now_ms)?;
            anyhow::ensure!(remote_phase == "finalized", "remote not finalized");
            job.state = DepthArchiveJobState::Finalized;
            job.lease_owner = None;
            job.lease_expires_at_ms = None;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeStorageState {
        next_provider_id: u64,
        objects: BTreeMap<String, Vec<u8>>,
    }

    #[derive(Clone, Default)]
    struct FakeStorage {
        state: Arc<Mutex<FakeStorageState>>,
    }

    impl DepthArchiveStorage for FakeStorage {
        fn create_multipart(&mut self, _target: &StorageObjectTarget) -> Result<String, String> {
            let mut state = self.state.lock().unwrap();
            state.next_provider_id += 1;
            Ok(format!("provider-{}", state.next_provider_id))
        }

        fn authorize_part(&mut self, target: &StoragePartTarget) -> Result<PresignedGrant, String> {
            Ok(PresignedGrant {
                url: format!("https://depth.invalid/part/{}", target.part_number),
                signed_headers: BTreeMap::from([
                    ("content-length".into(), target.byte_size.to_string()),
                    ("x-amz-content-sha256".into(), target.sha256.clone()),
                ]),
            })
        }

        fn complete_multipart(&mut self, _target: &StorageCompleteTarget) -> Result<(), String> {
            Ok(())
        }

        fn abort_multipart(
            &mut self,
            _bucket: &str,
            _object_key: &str,
            _provider_upload_id: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        fn verify_object(
            &mut self,
            _bucket: &str,
            object_key: &str,
        ) -> Result<VerifiedObject, String> {
            let state = self.state.lock().unwrap();
            let bytes = state
                .objects
                .get(object_key)
                .ok_or_else(|| "missing object".to_string())?;
            Ok(VerifiedObject {
                size_bytes: bytes.len() as u64,
                sha256: sha256_bytes(bytes),
            })
        }

        fn authorize_manifest_put(
            &mut self,
            target: &StorageManifestTarget,
        ) -> Result<PresignedGrant, String> {
            Ok(PresignedGrant {
                url: "https://depth.invalid/manifest".into(),
                signed_headers: BTreeMap::from([
                    ("content-length".into(), target.size_bytes.to_string()),
                    ("x-amz-content-sha256".into(), target.sha256.clone()),
                    ("if-none-match".into(), "*".into()),
                ]),
            })
        }

        fn read_manifest(
            &mut self,
            _bucket: &str,
            object_key: &str,
        ) -> Result<VerifiedManifest, String> {
            let state = self.state.lock().unwrap();
            let bytes = state
                .objects
                .get(object_key)
                .ok_or_else(|| "missing manifest".to_string())?;
            Ok(VerifiedManifest {
                size_bytes: bytes.len() as u64,
                sha256: sha256_bytes(bytes),
                manifest: serde_json::from_slice(bytes)
                    .map_err(|error| format!("invalid manifest: {error}"))?,
            })
        }
    }

    struct ServiceClient {
        service: DepthIngestService<FakeStorage>,
        storage: FakeStorage,
        parts: BTreeMap<String, BTreeMap<u32, Vec<u8>>>,
        expire_next_grant: bool,
        fail_next_put: bool,
    }

    impl ServiceClient {
        fn new() -> Self {
            let storage = FakeStorage::default();
            Self {
                service: DepthIngestService::new(storage.clone(), 60).unwrap(),
                storage,
                parts: BTreeMap::new(),
                expire_next_grant: false,
                fail_next_put: false,
            }
        }

        fn ingest(error: IngestError) -> DepthIngestClientError {
            DepthIngestClientError::Remote(format!("#535:{error:?}"))
        }
    }

    impl DepthIngestClient for ServiceClient {
        fn create_archive(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            request: CreateArchiveRequest,
        ) -> Result<ArchiveScope, DepthIngestClientError> {
            self.service
                .create_archive(principal, now_epoch_seconds, request)
                .map_err(Self::ingest)
        }

        fn create_multipart(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            request: CreateMultipartRequest,
        ) -> Result<MultipartIntent, DepthIngestClientError> {
            self.service
                .create_multipart(principal, now_epoch_seconds, request)
                .map_err(Self::ingest)
        }

        fn authorize_part(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            request: AuthorizePartRequest,
        ) -> Result<AuthorizedPart, DepthIngestClientError> {
            let mut authorized = self
                .service
                .authorize_part(principal, now_epoch_seconds, request)
                .map_err(Self::ingest)?;
            if self.expire_next_grant {
                self.expire_next_grant = false;
                authorized.expires_at_epoch_seconds = now_epoch_seconds;
            }
            Ok(authorized)
        }

        fn put_part(
            &mut self,
            authorization: &AuthorizedPart,
            bytes: &[u8],
        ) -> Result<String, DepthIngestClientError> {
            if self.fail_next_put {
                self.fail_next_put = false;
                return Err(DepthIngestClientError::Remote(
                    "injected put failure".into(),
                ));
            }
            self.parts
                .entry(authorization.upload_id.clone())
                .or_default()
                .insert(authorization.part_number, bytes.to_vec());
            Ok(format!("etag-{}", authorization.part_number))
        }

        fn complete_object(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            request: CompleteObjectRequest,
        ) -> Result<ArchiveStatus, DepthIngestClientError> {
            let uploaded = self
                .parts
                .get(&request.upload_id)
                .ok_or_else(|| DepthIngestClientError::Remote("missing fake parts".into()))?;
            let mut bytes = Vec::new();
            for part in &request.parts {
                bytes.extend_from_slice(
                    uploaded.get(&part.part_number).ok_or_else(|| {
                        DepthIngestClientError::Remote("missing fake part".into())
                    })?,
                );
            }
            self.storage
                .state
                .lock()
                .unwrap()
                .objects
                .insert(request.object_key.clone(), bytes);
            self.service
                .complete_object(principal, now_epoch_seconds, request)
                .map_err(Self::ingest)
        }

        fn authorize_manifest(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            request: AuthorizeManifestRequest,
        ) -> Result<AuthorizedManifest, DepthIngestClientError> {
            self.service
                .authorize_manifest(principal, now_epoch_seconds, request)
                .map_err(Self::ingest)
        }

        fn put_manifest(
            &mut self,
            authorization: &AuthorizedManifest,
            bytes: &[u8],
        ) -> Result<(), DepthIngestClientError> {
            let mut state = self.storage.state.lock().unwrap();
            if state.objects.contains_key(&authorization.object_key) {
                return Err(DepthIngestClientError::AlreadyExists);
            }
            state
                .objects
                .insert(authorization.object_key.clone(), bytes.to_vec());
            Ok(())
        }

        fn finalize_manifest(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            request: FinalizeManifestRequest,
        ) -> Result<ArchiveStatus, DepthIngestClientError> {
            self.service
                .finalize_manifest(principal, now_epoch_seconds, request)
                .map_err(Self::ingest)
        }

        fn status(
            &mut self,
            principal: &DepthPrincipal,
            now_epoch_seconds: u64,
            scope: &ArchiveScope,
        ) -> Result<ArchiveStatus, DepthIngestClientError> {
            self.service
                .status(principal, now_epoch_seconds, scope)
                .map_err(Self::ingest)
        }
    }

    fn write_materialized_archive(root: &Path) -> MaterializedArchive {
        let identity = StreamIdentity::new("session-worker", "r0000", 1).unwrap();
        let definitions = [
            (
                "session/depth-profile-evidence.json",
                ArchiveObjectKind::DepthProfileEvidence,
                ArchivePriority::P0,
            ),
            (
                "session/dsv-thumbnail.png",
                ArchiveObjectKind::DsvThumbnail,
                ArchivePriority::P0,
            ),
            (
                "session/frame-pairs.jsonl.zst",
                ArchiveObjectKind::FramePairs,
                ArchivePriority::P0,
            ),
            (
                "session/action_camera_15fps.json",
                ArchiveObjectKind::ActionCamera,
                ArchivePriority::P0,
            ),
            (
                "session/systeminfo.json",
                ArchiveObjectKind::SystemInfo,
                ArchivePriority::P0,
            ),
            (
                "runs/r0000/variant-s-recipe.json",
                ArchiveObjectKind::VariantRecipe,
                ArchivePriority::P0,
            ),
            (
                "runs/r0000/depth-index.jsonl.zst",
                ArchiveObjectKind::DepthIndex,
                ArchivePriority::P0,
            ),
            (
                "runs/r0000/variant-r/depth_00000.zip",
                ArchiveObjectKind::VariantRArchive,
                ArchivePriority::P2,
            ),
        ];
        let mut local = Vec::new();
        let mut declared = Vec::new();
        for (index, (relative_key, kind, priority)) in definitions.into_iter().enumerate() {
            let bytes = format!("depth-object-{index}").into_bytes();
            let path = root.join(relative_key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            let sha256 = sha256_bytes(&bytes);
            local.push(depth_lab::client_archive::LocalArchiveObject {
                relative_key: relative_key.into(),
                local_path: path,
                kind,
                priority,
                size_bytes: bytes.len() as u64,
                sha256: sha256.clone(),
            });
            declared.push(ArchiveObject {
                relative_key: relative_key.into(),
                kind,
                priority,
                size_bytes: bytes.len() as u64,
                sha256,
            });
        }
        let manifest = ArchiveManifest {
            schema_version: ARCHIVE_SCHEMA_VERSION.into(),
            session_id: identity.session_id.clone(),
            lane: ArchiveLane::Lane30,
            non_trainable: true,
            objects: declared,
        };
        manifest.validate().unwrap();
        let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        manifest_bytes.push(b'\n');
        let manifest_path = root.join("session/depth-manifest.json");
        std::fs::write(&manifest_path, &manifest_bytes).unwrap();
        MaterializedArchive {
            schema: depth_lab::client_archive::ARCHIVE_BUILD_SCHEMA.into(),
            identity,
            lane: ArchiveLane::Lane30,
            manifest,
            manifest_path,
            manifest_size_bytes: manifest_bytes.len() as u64,
            manifest_sha256: sha256_bytes(&manifest_bytes),
            objects: local,
        }
    }

    fn principal() -> DepthPrincipal {
        DepthPrincipal {
            subject: "depth-device-1".into(),
            audience: DEPTH_INGEST_AUDIENCE.into(),
            expires_at_epoch_seconds: 10_000,
        }
    }

    fn config() -> DepthArchiveWorkerConfig {
        DepthArchiveWorkerConfig {
            lease_owner: "test-worker".into(),
            lease_duration_ms: 1_000,
            part_size_bytes: MIN_MULTIPART_PART_SIZE_BYTES,
            retry_base_ms: 10,
            retry_max_ms: 10,
        }
    }

    #[test]
    fn expired_grant_retries_then_real_535_service_finalizes_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let archive = write_materialized_archive(temp.path());
        let state = MemoryStateStore::new(&archive);
        let mut client = ServiceClient::new();
        client.expire_next_grant = true;

        let first = process_next_depth_archive(
            &state,
            &mut client,
            &NeverResolve,
            &principal(),
            &config(),
            1_000,
            100,
        )
        .unwrap();
        assert!(matches!(
            first,
            DepthArchiveWorkerOutcome::RetryScheduled { ref error_code, .. }
                if error_code == "DEPTH_GRANT_EXPIRED"
        ));
        assert_eq!(state.snapshot().state, DepthArchiveJobState::RetryWait);
        assert!(
            archive
                .objects
                .iter()
                .all(|object| object.local_path.is_file())
        );
        assert_eq!(
            process_next_depth_archive(
                &state,
                &mut client,
                &NeverResolve,
                &principal(),
                &config(),
                1_009,
                100,
            )
            .unwrap(),
            DepthArchiveWorkerOutcome::Idle
        );

        let recovered = process_next_depth_archive(
            &state,
            &mut client,
            &NeverResolve,
            &principal(),
            &config(),
            1_010,
            101,
        )
        .unwrap();
        assert!(matches!(
            recovered,
            DepthArchiveWorkerOutcome::Finalized { .. }
        ));
        assert_eq!(state.snapshot().state, DepthArchiveJobState::Finalized);
        assert!(
            archive
                .objects
                .iter()
                .all(|object| object.local_path.is_file())
        );
        assert_eq!(
            process_next_depth_archive(
                &state,
                &mut client,
                &NeverResolve,
                &principal(),
                &config(),
                2_000,
                102,
            )
            .unwrap(),
            DepthArchiveWorkerOutcome::Idle
        );
    }

    #[test]
    fn remote_put_failure_retains_local_archive_and_resumes_without_duplicate_job() {
        let temp = tempfile::tempdir().unwrap();
        let archive = write_materialized_archive(temp.path());
        let state = MemoryStateStore::new(&archive);
        let mut client = ServiceClient::new();
        client.fail_next_put = true;
        let failed = process_next_depth_archive(
            &state,
            &mut client,
            &NeverResolve,
            &principal(),
            &config(),
            5_000,
            200,
        )
        .unwrap();
        assert!(matches!(
            failed,
            DepthArchiveWorkerOutcome::RetryScheduled { ref error_code, .. }
                if error_code == "DEPTH_REMOTE_FAILED"
        ));
        assert!(
            archive
                .objects
                .iter()
                .all(|object| object.local_path.is_file())
        );
        let job_id = state.snapshot().job_id;

        let recovered = process_next_depth_archive(
            &state,
            &mut client,
            &NeverResolve,
            &principal(),
            &config(),
            5_010,
            201,
        )
        .unwrap();
        assert_eq!(
            recovered,
            DepthArchiveWorkerOutcome::Finalized {
                job_id: job_id.clone()
            }
        );
        assert_eq!(state.snapshot().job_id, job_id);
        assert_eq!(state.snapshot().attempt_count, 2);
    }
}
