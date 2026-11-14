//! U2 客户端 archive helper。
//!
//! 这里只做本地、可重放的事实转换：读取严格 evidence、重新验证 #529 segment，
//! 物化真正的 ZIP/zstd 对象并最后写 #524 manifest。网络、principal 与 durable lease
//! 留给客户端适配层。docs/107 之后，production clip 链路会复用本模块的
//! 逐 segment 确定性 ZIP 物化；旧 run 级独立 archive 仍保留作为兼容路径。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::archive_contract::{
    ARCHIVE_SCHEMA_VERSION, ArchiveLane, ArchiveManifest, ArchiveObject, ArchiveObjectKind,
    ArchivePriority,
};
use crate::pairing::FramePair;
use crate::pipeline::E2eReport;
use crate::segment_finalize::{
    FrameStateEvidence, StreamIdentity, VerifiedCandidateSegment, verify_candidate_segment,
};

#[path = "raw_zip_provenance.rs"]
mod raw_zip_provenance;
use raw_zip_provenance::MAX_RAW_PROVENANCE_BYTES;
pub use raw_zip_provenance::{
    RAW_DEPTH_ZIP_SCHEMA, RawDepthProvenance, RawRunEvidence, raw_provenance_game,
};

pub const LEGACY_DEPTH_ZIP_SCHEMA: &str = "dm-depth-variant-r-zip-1";

pub const ARCHIVE_BUILD_SCHEMA: &str = "dm-depth-client-archive-build-1";
pub const VARIANT_RECIPE_SCHEMA: &str = "dm-depth-variant-s-recipe-1";
const RECEIPT_NAME: &str = "archive-build.json";
const MANIFEST_RELATIVE_KEY: &str = "session/depth-manifest.json";
const MAX_SMALL_EVIDENCE_BYTES: u64 = 64 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeEvidencePaths {
    pub frame_pairs_jsonl: PathBuf,
    pub frame_states_json: PathBuf,
    pub e2e_report_json: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedFinalizeEvidence {
    pub pairs: Vec<FramePair>,
    pub frame_states: FrameStateEvidence,
    pub report: E2eReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentCandidateSource {
    pub segment_index: u32,
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSupplementFiles {
    pub depth_profile_evidence: PathBuf,
    pub dsv_thumbnail: PathBuf,
    pub action_camera: PathBuf,
    pub systeminfo: PathBuf,
    pub variant_s_recipe: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantRecipeEvidence {
    pub schema: String,
    pub identity: StreamIdentity,
    pub frames: Vec<VariantRecipeFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantRecipeFrame {
    pub session_frame: u64,
    pub depth_name: String,
    pub expected_sha256: String,
}

pub struct ArchiveBuildRequest<'a> {
    pub identity: &'a StreamIdentity,
    pub lane: ArchiveLane,
    pub segments: &'a [SegmentCandidateSource],
    pub frame_pairs: &'a [FramePair],
    pub supplements: &'a ArchiveSupplementFiles,
    /// 最终本地 archive 根；必须与 segment 目录分离。
    pub output_root: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalArchiveObject {
    pub relative_key: String,
    pub local_path: PathBuf,
    pub kind: ArchiveObjectKind,
    pub priority: ArchivePriority,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedArchive {
    pub schema: String,
    pub identity: StreamIdentity,
    pub lane: ArchiveLane,
    pub manifest: ArchiveManifest,
    pub manifest_path: PathBuf,
    pub manifest_size_bytes: u64,
    pub manifest_sha256: String,
    pub objects: Vec<LocalArchiveObject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveBuildReceipt {
    schema: String,
    identity: StreamIdentity,
    lane: ArchiveLane,
    segment_manifests: Vec<ReceiptSegment>,
    manifest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptSegment {
    segment_index: u32,
    manifest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ZipInventory {
    schema: String,
    segment_index: u32,
    frames: Vec<ZipInventoryFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<ZipProvenanceReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ZipProvenanceReference {
    path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ZipInventoryFrame {
    session_frame: u64,
    present_seq: u64,
    path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug)]
pub enum ClientArchiveError {
    Contract(String),
    Integrity(String),
    Io(String),
}

impl std::fmt::Display for ClientArchiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(message) => write!(formatter, "depth archive contract: {message}"),
            Self::Integrity(message) => write!(formatter, "depth archive integrity: {message}"),
            Self::Io(message) => write!(formatter, "depth archive I/O: {message}"),
        }
    }
}

impl std::error::Error for ClientArchiveError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentUploadRequest<'a> {
    pub identity: &'a StreamIdentity,
    pub segment_index: u32,
    pub manifest_path: &'a Path,
    pub manifest_sha256: &'a str,
    /// 同一 run 内的最终 ZIP 路径；发布后不可覆盖。
    pub output_path: &'a Path,
    /// Only a new ordinary KCD2 run supplies this. Legacy v1 bytes stay unchanged.
    pub raw_evidence: Option<&'a RawRunEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedSegmentUpload {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
    pub frame_count: usize,
}

/// 把一个已经通过 #529 收口门的 segment 物化成 production clip 的 depth ZIP。
///
/// ZIP 内是完整 EXR 集合与 `depth-files.json` inventory；成员顺序、元数据与命名固定，
/// 因而相同输入必得相同 SHA-256。已存在的目标只允许逐成员复核后幂等复用，绝不覆盖。
pub fn materialize_segment_upload(
    request: &SegmentUploadRequest<'_>,
) -> Result<MaterializedSegmentUpload, ClientArchiveError> {
    ensure_safe_output_root(request.output_path)?;
    if request
        .output_path
        .extension()
        .and_then(|value| value.to_str())
        != Some("zip")
    {
        return Err(ClientArchiveError::Contract(
            "depth segment upload must use a .zip path".into(),
        ));
    }
    let segment = verify_candidate_segment(
        request.manifest_path,
        request.identity,
        request.segment_index,
        request.manifest_sha256,
    )
    .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
    let provenance = request
        .raw_evidence
        .map(|evidence| evidence.materialize(&segment))
        .transpose()?;
    create_parent(request.output_path)?;
    if request.output_path.exists() {
        verify_segment_zip_with_provenance(request.output_path, &segment, provenance.as_deref())?;
        return segment_upload_result(request.output_path, segment.frames.len());
    }

    let parent = request.output_path.parent().ok_or_else(|| {
        ClientArchiveError::Contract(format!(
            "depth upload path has no parent: {}",
            request.output_path.display()
        ))
    })?;
    let file_name = request
        .output_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ClientArchiveError::Contract("depth upload name is not UTF-8".into()))?;
    let staging = parent.join(format!(
        ".{file_name}.staging-{}",
        request.identity.generation
    ));
    if staging.exists() {
        regular_file_metadata(&staging)?;
        fs::remove_file(&staging).map_err(io_error)?;
    }

    let result = write_segment_zip_with_provenance(&staging, &segment, provenance.as_deref())
        .and_then(|()| {
            verify_segment_zip_with_provenance(&staging, &segment, provenance.as_deref())
        })
        .and_then(|()| fs::rename(&staging, request.output_path).map_err(io_error))
        .and_then(|()| {
            verify_segment_zip_with_provenance(request.output_path, &segment, provenance.as_deref())
        });
    if result.is_err() && staging.exists() && regular_file_metadata(&staging).is_ok() {
        let _ = fs::remove_file(&staging);
    }
    result?;
    segment_upload_result(request.output_path, segment.frames.len())
}

fn segment_upload_result(
    path: &Path,
    frame_count: usize,
) -> Result<MaterializedSegmentUpload, ClientArchiveError> {
    let metadata = regular_file_metadata(path)?;
    if metadata.len() == 0 {
        return Err(ClientArchiveError::Integrity(
            "depth segment ZIP is empty".into(),
        ));
    }
    Ok(MaterializedSegmentUpload {
        path: path.to_path_buf(),
        size_bytes: metadata.len(),
        sha256: sha256_file(path)?,
        frame_count,
    })
}

pub fn load_finalize_evidence(
    paths: &FinalizeEvidencePaths,
) -> Result<LoadedFinalizeEvidence, ClientArchiveError> {
    let pairs = read_json_lines(&paths.frame_pairs_jsonl)?;
    if pairs.is_empty() {
        return Err(ClientArchiveError::Contract(
            "frame-pairs evidence is empty".into(),
        ));
    }
    let frame_states = read_small_json(&paths.frame_states_json)?;
    let report = read_small_json(&paths.e2e_report_json)?;
    Ok(LoadedFinalizeEvidence {
        pairs,
        frame_states,
        report,
    })
}

pub fn materialize_archive(
    request: &ArchiveBuildRequest<'_>,
) -> Result<MaterializedArchive, ClientArchiveError> {
    if request.segments.is_empty() {
        return Err(ClientArchiveError::Contract(
            "archive requires at least one durable segment candidate".into(),
        ));
    }
    ensure_safe_output_root(request.output_root)?;
    let expected_receipt = expected_receipt(request, String::new());
    if request.output_root.exists() {
        return load_existing_archive(request.output_root, &expected_receipt);
    }

    let segments = verify_segments(request)?;
    validate_variant_recipe(request, &segments)?;
    let parent = request.output_root.parent().ok_or_else(|| {
        ClientArchiveError::Contract(format!(
            "archive output has no parent: {}",
            request.output_root.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(io_error)?;
    ensure_real_directory(parent)?;
    let file_name = request
        .output_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ClientArchiveError::Contract("archive output name is not UTF-8".into()))?;
    let staging = parent.join(format!(
        ".{file_name}.staging-{}",
        request.identity.generation
    ));
    if staging.exists() {
        ensure_real_directory(&staging)?;
        fs::remove_dir_all(&staging).map_err(io_error)?;
    }
    fs::create_dir(&staging).map_err(io_error)?;

    let result = build_staging(request, &segments, &staging).and_then(|materialized| {
        fs::rename(&staging, request.output_root).map_err(io_error)?;
        let mut published = materialized;
        rebase_materialized_paths(&mut published, request.output_root);
        verify_materialized_archive(&published)?;
        Ok(published)
    });
    if result.is_err() && staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub fn verify_materialized_archive(
    archive: &MaterializedArchive,
) -> Result<(), ClientArchiveError> {
    if archive.schema != ARCHIVE_BUILD_SCHEMA
        || archive.manifest.schema_version != ARCHIVE_SCHEMA_VERSION
        || archive.manifest.session_id != archive.identity.session_id
        || archive.manifest.lane != archive.lane
        || !archive.manifest.non_trainable
    {
        return Err(ClientArchiveError::Integrity(
            "materialized archive identity/schema mismatch".into(),
        ));
    }
    archive
        .manifest
        .validate()
        .map_err(|errors| ClientArchiveError::Integrity(format!("manifest invalid: {errors:?}")))?;
    let manifest_bytes = read_regular_file(&archive.manifest_path, None)?;
    if manifest_bytes.len() as u64 != archive.manifest_size_bytes
        || sha256_bytes(&manifest_bytes) != archive.manifest_sha256
    {
        return Err(ClientArchiveError::Integrity(
            "materialized manifest size/hash mismatch".into(),
        ));
    }
    let parsed: ArchiveManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        ClientArchiveError::Integrity(format!("parse materialized manifest: {error}"))
    })?;
    if parsed != archive.manifest {
        return Err(ClientArchiveError::Integrity(
            "materialized manifest bytes differ from durable metadata".into(),
        ));
    }
    if archive.objects.len() != archive.manifest.objects.len() {
        return Err(ClientArchiveError::Integrity(
            "materialized object inventory count mismatch".into(),
        ));
    }
    for (local, declared) in archive.objects.iter().zip(&archive.manifest.objects) {
        if local.relative_key != declared.relative_key
            || local.kind != declared.kind
            || local.priority != declared.priority
            || local.size_bytes != declared.size_bytes
            || local.sha256 != declared.sha256
        {
            return Err(ClientArchiveError::Integrity(format!(
                "local/declarative object mismatch for {}",
                local.relative_key
            )));
        }
        let metadata = regular_file_metadata(&local.local_path)?;
        if metadata.len() != local.size_bytes || sha256_file(&local.local_path)? != local.sha256 {
            return Err(ClientArchiveError::Integrity(format!(
                "local object size/hash mismatch for {}",
                local.local_path.display()
            )));
        }
    }
    Ok(())
}

fn verify_segments(
    request: &ArchiveBuildRequest<'_>,
) -> Result<Vec<VerifiedCandidateSegment>, ClientArchiveError> {
    let mut sources = request.segments.to_vec();
    sources.sort_by_key(|source| source.segment_index);
    let mut seen = BTreeSet::new();
    let mut verified = Vec::with_capacity(sources.len());
    for source in sources {
        if !seen.insert(source.segment_index) {
            return Err(ClientArchiveError::Contract(format!(
                "duplicate candidate segment {}",
                source.segment_index
            )));
        }
        let segment = verify_candidate_segment(
            &source.manifest_path,
            request.identity,
            source.segment_index,
            &source.manifest_sha256,
        )
        .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
        if segment.lane != request.lane {
            return Err(ClientArchiveError::Contract(format!(
                "segment {} lane differs from archive lane",
                source.segment_index
            )));
        }
        verified.push(segment);
    }
    Ok(verified)
}

fn validate_variant_recipe(
    request: &ArchiveBuildRequest<'_>,
    segments: &[VerifiedCandidateSegment],
) -> Result<(), ClientArchiveError> {
    let recipe: VariantRecipeEvidence = read_small_json(&request.supplements.variant_s_recipe)?;
    if recipe.schema != VARIANT_RECIPE_SCHEMA || recipe.identity != *request.identity {
        return Err(ClientArchiveError::Contract(
            "Variant-S recipe identity/schema mismatch".into(),
        ));
    }
    let expected: BTreeMap<_, _> = segments
        .iter()
        .flat_map(|segment| {
            segment
                .frames
                .iter()
                .map(|frame| (frame.session_frame, frame.sha256.as_str()))
        })
        .collect();
    let expected_frame_count = segments
        .iter()
        .map(|segment| segment.frames.len())
        .sum::<usize>();
    if expected.len() != expected_frame_count {
        return Err(ClientArchiveError::Contract(
            "candidate segments overlap on session_frame".into(),
        ));
    }
    let mut observed = BTreeSet::new();
    for frame in recipe.frames {
        if !observed.insert(frame.session_frame)
            || frame.depth_name != format!("depth_{}", frame.session_frame)
            || !valid_sha256(&frame.expected_sha256)
            || expected.get(&frame.session_frame).copied() != Some(frame.expected_sha256.as_str())
        {
            return Err(ClientArchiveError::Contract(format!(
                "Variant-S recipe frame {} is invalid or duplicated",
                frame.session_frame
            )));
        }
    }
    if observed != expected.keys().copied().collect() {
        return Err(ClientArchiveError::Contract(
            "Variant-S recipe does not cover exactly the candidate depth frames".into(),
        ));
    }
    Ok(())
}

fn build_staging(
    request: &ArchiveBuildRequest<'_>,
    segments: &[VerifiedCandidateSegment],
    staging: &Path,
) -> Result<MaterializedArchive, ClientArchiveError> {
    let mut objects = Vec::new();
    for (relative_key, kind, priority, source) in [
        (
            "session/depth-profile-evidence.json",
            ArchiveObjectKind::DepthProfileEvidence,
            ArchivePriority::P0,
            &request.supplements.depth_profile_evidence,
        ),
        (
            "session/dsv-thumbnail.png",
            ArchiveObjectKind::DsvThumbnail,
            ArchivePriority::P0,
            &request.supplements.dsv_thumbnail,
        ),
        (
            "session/action_camera_15fps.json",
            ArchiveObjectKind::ActionCamera,
            ArchivePriority::P0,
            &request.supplements.action_camera,
        ),
        (
            "session/systeminfo.json",
            ArchiveObjectKind::SystemInfo,
            ArchivePriority::P0,
            &request.supplements.systeminfo,
        ),
    ] {
        objects.push(copy_declared_object(
            staging,
            relative_key,
            kind,
            priority,
            source,
        )?);
    }
    let recipe_key = format!("runs/{}/variant-s-recipe.json", request.identity.run_id);
    objects.push(copy_declared_object(
        staging,
        &recipe_key,
        ArchiveObjectKind::VariantRecipe,
        ArchivePriority::P0,
        &request.supplements.variant_s_recipe,
    )?);

    let pairs_key = "session/frame-pairs.jsonl.zst";
    let pairs_path = staging.join(pairs_key);
    create_parent(&pairs_path)?;
    write_pairs_zstd(&pairs_path, request.frame_pairs)?;
    objects.push(local_object(
        pairs_key,
        ArchiveObjectKind::FramePairs,
        ArchivePriority::P0,
        &pairs_path,
    )?);

    let index_key = format!("runs/{}/depth-index.jsonl.zst", request.identity.run_id);
    let index_path = staging.join(&index_key);
    create_parent(&index_path)?;
    write_combined_index(&index_path, segments, request.frame_pairs)?;
    objects.push(local_object(
        &index_key,
        ArchiveObjectKind::DepthIndex,
        ArchivePriority::P0,
        &index_path,
    )?);

    for segment in segments {
        let key = format!(
            "runs/{}/variant-r/depth_{:05}.zip",
            request.identity.run_id, segment.segment_index
        );
        let path = staging.join(&key);
        create_parent(&path)?;
        write_segment_zip(&path, segment)?;
        objects.push(local_object(
            &key,
            ArchiveObjectKind::VariantRArchive,
            ArchivePriority::P2,
            &path,
        )?);
    }
    objects.sort_by(|left, right| left.relative_key.cmp(&right.relative_key));
    let manifest = ArchiveManifest {
        schema_version: ARCHIVE_SCHEMA_VERSION.into(),
        session_id: request.identity.session_id.clone(),
        lane: request.lane,
        non_trainable: true,
        objects: objects
            .iter()
            .map(|object| ArchiveObject {
                relative_key: object.relative_key.clone(),
                kind: object.kind,
                priority: object.priority,
                size_bytes: object.size_bytes,
                sha256: object.sha256.clone(),
            })
            .collect(),
    };
    manifest.validate().map_err(|errors| {
        ClientArchiveError::Contract(format!("#524 manifest validation failed: {errors:?}"))
    })?;
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| ClientArchiveError::Contract(format!("serialize manifest: {error}")))?;
    manifest_bytes.push(b'\n');
    let manifest_path = staging.join(MANIFEST_RELATIVE_KEY);
    create_parent(&manifest_path)?;
    write_new_synced(&manifest_path, &manifest_bytes)?;
    let manifest_sha256 = sha256_bytes(&manifest_bytes);
    let receipt = expected_receipt(request, manifest_sha256.clone());
    let mut receipt_bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| ClientArchiveError::Contract(format!("serialize receipt: {error}")))?;
    receipt_bytes.push(b'\n');
    write_new_synced(&staging.join(RECEIPT_NAME), &receipt_bytes)?;
    Ok(MaterializedArchive {
        schema: ARCHIVE_BUILD_SCHEMA.into(),
        identity: request.identity.clone(),
        lane: request.lane,
        manifest,
        manifest_path,
        manifest_size_bytes: manifest_bytes.len() as u64,
        manifest_sha256,
        objects,
    })
}

fn expected_receipt(
    request: &ArchiveBuildRequest<'_>,
    manifest_sha256: String,
) -> ArchiveBuildReceipt {
    let mut segment_manifests = request
        .segments
        .iter()
        .map(|source| ReceiptSegment {
            segment_index: source.segment_index,
            manifest_sha256: source.manifest_sha256.clone(),
        })
        .collect::<Vec<_>>();
    segment_manifests.sort_by_key(|segment| segment.segment_index);
    ArchiveBuildReceipt {
        schema: ARCHIVE_BUILD_SCHEMA.into(),
        identity: request.identity.clone(),
        lane: request.lane,
        segment_manifests,
        manifest_sha256,
    }
}

fn load_existing_archive(
    root: &Path,
    expected: &ArchiveBuildReceipt,
) -> Result<MaterializedArchive, ClientArchiveError> {
    ensure_real_directory(root)?;
    let receipt: ArchiveBuildReceipt = read_small_json(&root.join(RECEIPT_NAME))?;
    if receipt.schema != expected.schema
        || receipt.identity != expected.identity
        || receipt.lane != expected.lane
        || receipt.segment_manifests != expected.segment_manifests
        || !valid_sha256(&receipt.manifest_sha256)
    {
        return Err(ClientArchiveError::Integrity(
            "existing archive build receipt conflicts with requested candidates".into(),
        ));
    }
    let manifest_path = root.join(MANIFEST_RELATIVE_KEY);
    let manifest_bytes = read_regular_file(&manifest_path, None)?;
    if sha256_bytes(&manifest_bytes) != receipt.manifest_sha256 {
        return Err(ClientArchiveError::Integrity(
            "existing archive manifest hash differs from receipt".into(),
        ));
    }
    let manifest: ArchiveManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        ClientArchiveError::Integrity(format!("parse existing archive manifest: {error}"))
    })?;
    let objects = manifest
        .objects
        .iter()
        .map(|object| LocalArchiveObject {
            relative_key: object.relative_key.clone(),
            local_path: root.join(&object.relative_key),
            kind: object.kind,
            priority: object.priority,
            size_bytes: object.size_bytes,
            sha256: object.sha256.clone(),
        })
        .collect();
    let materialized = MaterializedArchive {
        schema: ARCHIVE_BUILD_SCHEMA.into(),
        identity: receipt.identity,
        lane: receipt.lane,
        manifest,
        manifest_path,
        manifest_size_bytes: manifest_bytes.len() as u64,
        manifest_sha256: receipt.manifest_sha256,
        objects,
    };
    verify_materialized_archive(&materialized)?;
    Ok(materialized)
}

fn copy_declared_object(
    root: &Path,
    relative_key: &str,
    kind: ArchiveObjectKind,
    priority: ArchivePriority,
    source: &Path,
) -> Result<LocalArchiveObject, ClientArchiveError> {
    let bytes = read_regular_file(source, None)?;
    if bytes.is_empty() {
        return Err(ClientArchiveError::Contract(format!(
            "required archive source is empty: {}",
            source.display()
        )));
    }
    let destination = root.join(relative_key);
    create_parent(&destination)?;
    write_new_synced(&destination, &bytes)?;
    local_object(relative_key, kind, priority, &destination)
}

fn write_pairs_zstd(path: &Path, pairs: &[FramePair]) -> Result<(), ClientArchiveError> {
    if pairs.is_empty() {
        return Err(ClientArchiveError::Contract(
            "cannot archive empty frame-pairs".into(),
        ));
    }
    let file = create_new_file(path)?;
    let mut encoder = zstd::Encoder::new(file, ZSTD_LEVEL).map_err(io_error)?;
    for pair in pairs {
        serde_json::to_writer(&mut encoder, pair).map_err(|error| {
            ClientArchiveError::Contract(format!("serialize frame-pair: {error}"))
        })?;
        encoder.write_all(b"\n").map_err(io_error)?;
    }
    let file = encoder.finish().map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn write_combined_index(
    path: &Path,
    segments: &[VerifiedCandidateSegment],
    frame_pairs: &[FramePair],
) -> Result<(), ClientArchiveError> {
    let file = create_new_file(path)?;
    let mut encoder = zstd::Encoder::new(file, ZSTD_LEVEL).map_err(io_error)?;
    let mut previous_present_seq = None;
    let mut previous_session_frame = None;
    for segment in segments {
        let source = File::open(&segment.index_path).map_err(io_error)?;
        for line in BufReader::new(source).lines() {
            let line = line.map_err(io_error)?;
            let row: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
                ClientArchiveError::Integrity(format!("parse candidate index: {error}"))
            })?;
            let present_seq = row
                .get("present_seq")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ClientArchiveError::Integrity("index lacks present_seq".into()))?;
            let session_frame = row
                .get("session_frame")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ClientArchiveError::Integrity("index lacks session_frame".into()))?;
            let present_regressed =
                previous_present_seq.is_some_and(|previous| present_seq < previous);
            let session_not_increasing =
                previous_session_frame.is_some_and(|previous| session_frame <= previous);
            let invalid_alias = match (previous_present_seq, previous_session_frame) {
                (Some(previous_seq), Some(previous_frame)) if previous_seq == present_seq => {
                    !frame_pairs_prove_contiguous_alias(
                        frame_pairs,
                        previous_frame,
                        session_frame,
                        present_seq,
                    )
                }
                _ => false,
            };
            if present_regressed || session_not_increasing || invalid_alias {
                return Err(ClientArchiveError::Integrity(
                    "candidate indices regress, duplicate a session frame, or reuse a non-contiguous source identity".into(),
                ));
            }
            previous_present_seq = Some(present_seq);
            previous_session_frame = Some(session_frame);
            encoder.write_all(line.as_bytes()).map_err(io_error)?;
            encoder.write_all(b"\n").map_err(io_error)?;
        }
    }
    let file = encoder.finish().map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn frame_pairs_prove_contiguous_alias(
    pairs: &[FramePair],
    previous_session_frame: u64,
    session_frame: u64,
    present_seq: u64,
) -> bool {
    let Some(expected_count) = session_frame
        .checked_sub(previous_session_frame)
        .and_then(|delta| delta.checked_add(1))
        .and_then(|count| usize::try_from(count).ok())
    else {
        return false;
    };
    let rows = pairs
        .iter()
        .filter(|pair| (previous_session_frame..=session_frame).contains(&pair.session_frame))
        .collect::<Vec<_>>();
    rows.len() == expected_count
        && rows.iter().enumerate().all(|(offset, pair)| {
            pair.session_frame == previous_session_frame + offset as u64
                && pair.present_seq == Some(present_seq)
        })
}

fn write_segment_zip(
    path: &Path,
    segment: &VerifiedCandidateSegment,
) -> Result<(), ClientArchiveError> {
    write_segment_zip_with_provenance(path, segment, None)
}

fn provenance_reference(bytes: &[u8]) -> ZipProvenanceReference {
    ZipProvenanceReference {
        path: "provenance.json".into(),
        size_bytes: bytes.len() as u64,
        sha256: sha256_bytes(bytes),
    }
}

fn write_segment_zip_with_provenance(
    path: &Path,
    segment: &VerifiedCandidateSegment,
    provenance: Option<&[u8]>,
) -> Result<(), ClientArchiveError> {
    let file = create_new_file(path)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default())
        .large_file(true)
        .unix_permissions(0o644);
    let mut inventory = Vec::with_capacity(segment.frames.len());
    for frame in &segment.frames {
        let name = format!("depth_{}.exr", frame.session_frame);
        writer
            .start_file(&name, options)
            .map_err(|error| ClientArchiveError::Io(format!("start ZIP member: {error}")))?;
        copy_verified_to_writer(
            &frame.source_path,
            frame.size_bytes,
            &frame.sha256,
            &mut writer,
        )?;
        inventory.push(ZipInventoryFrame {
            session_frame: frame.session_frame,
            present_seq: frame.present_seq,
            path: name,
            size_bytes: frame.size_bytes,
            sha256: frame.sha256.clone(),
        });
    }
    if let Some(bytes) = provenance {
        writer
            .start_file("provenance.json", options)
            .map_err(|error| ClientArchiveError::Io(error.to_string()))?;
        writer.write_all(bytes).map_err(io_error)?;
    }
    writer
        .start_file("depth-files.json", options)
        .map_err(|error| ClientArchiveError::Io(format!("start ZIP inventory: {error}")))?;
    let mut inventory_bytes = serde_json::to_vec_pretty(&ZipInventory {
        schema: if provenance.is_some() {
            RAW_DEPTH_ZIP_SCHEMA
        } else {
            LEGACY_DEPTH_ZIP_SCHEMA
        }
        .into(),
        segment_index: segment.segment_index,
        frames: inventory,
        provenance: provenance.map(provenance_reference),
    })
    .map_err(|error| ClientArchiveError::Contract(format!("serialize ZIP inventory: {error}")))?;
    inventory_bytes.push(b'\n');
    writer.write_all(&inventory_bytes).map_err(io_error)?;
    let file = writer
        .finish()
        .map_err(|error| ClientArchiveError::Io(format!("finish ZIP: {error}")))?;
    file.sync_all().map_err(io_error)
}

fn verify_segment_zip_with_provenance(
    path: &Path,
    segment: &VerifiedCandidateSegment,
    provenance: Option<&[u8]>,
) -> Result<(), ClientArchiveError> {
    let metadata = regular_file_metadata(path)?;
    if metadata.len() == 0 {
        return Err(ClientArchiveError::Integrity(format!(
            "depth segment ZIP is empty: {}",
            path.display()
        )));
    }
    let file = File::open(path).map_err(io_error)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| ClientArchiveError::Integrity(format!("open depth ZIP: {error}")))?;
    if archive.len() != segment.frames.len() + 1 + usize::from(provenance.is_some()) {
        return Err(ClientArchiveError::Integrity(format!(
            "depth ZIP member count mismatch: expected {}, got {}",
            segment.frames.len() + 1 + usize::from(provenance.is_some()),
            archive.len()
        )));
    }

    let mut expected_inventory = Vec::with_capacity(segment.frames.len());
    for (index, frame) in segment.frames.iter().enumerate() {
        let expected_name = format!("depth_{}.exr", frame.session_frame);
        let mut member = archive.by_index(index).map_err(|error| {
            ClientArchiveError::Integrity(format!("read depth ZIP member {index}: {error}"))
        })?;
        if member.name() != expected_name || member.size() != frame.size_bytes {
            return Err(ClientArchiveError::Integrity(format!(
                "depth ZIP member {index} identity/size mismatch"
            )));
        }
        let mut hasher = Sha256::new();
        let copied = std::io::copy(&mut member, &mut HashWriter(&mut hasher)).map_err(io_error)?;
        if copied != frame.size_bytes || format_digest(hasher.finalize()) != frame.sha256 {
            return Err(ClientArchiveError::Integrity(format!(
                "depth ZIP member {} content mismatch",
                expected_name
            )));
        }
        expected_inventory.push(ZipInventoryFrame {
            session_frame: frame.session_frame,
            present_seq: frame.present_seq,
            path: expected_name,
            size_bytes: frame.size_bytes,
            sha256: frame.sha256.clone(),
        });
    }

    if let Some(expected) = provenance {
        let mut member = archive
            .by_index(segment.frames.len())
            .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
        if member.name() != "provenance.json" || member.size() != expected.len() as u64 {
            return Err(ClientArchiveError::Integrity(
                "raw provenance ZIP member mismatch".into(),
            ));
        }
        let mut bytes = Vec::new();
        member
            .by_ref()
            .take(MAX_RAW_PROVENANCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes != expected {
            return Err(ClientArchiveError::Integrity(
                "raw provenance ZIP bytes mismatch".into(),
            ));
        }
        let parsed: RawDepthProvenance = serde_json::from_slice(&bytes)
            .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
        parsed.validate()?;
    }
    let inventory_index = segment.frames.len() + usize::from(provenance.is_some());
    let mut inventory_member = archive.by_index(inventory_index).map_err(|error| {
        ClientArchiveError::Integrity(format!("read depth ZIP inventory: {error}"))
    })?;
    if inventory_member.name() != "depth-files.json" {
        return Err(ClientArchiveError::Integrity(
            "depth ZIP inventory must be the final depth-files.json member".into(),
        ));
    }
    let mut inventory_bytes = Vec::new();
    inventory_member
        .by_ref()
        .take(MAX_RAW_PROVENANCE_BYTES + 1)
        .read_to_end(&mut inventory_bytes)
        .map_err(io_error)?;
    if inventory_bytes.len() as u64 > MAX_RAW_PROVENANCE_BYTES {
        return Err(ClientArchiveError::Integrity(
            "depth ZIP inventory exceeds bound".into(),
        ));
    }
    let inventory: ZipInventory = serde_json::from_slice(&inventory_bytes).map_err(|error| {
        ClientArchiveError::Integrity(format!("parse depth ZIP inventory: {error}"))
    })?;
    let expected = ZipInventory {
        schema: if provenance.is_some() {
            RAW_DEPTH_ZIP_SCHEMA
        } else {
            LEGACY_DEPTH_ZIP_SCHEMA
        }
        .into(),
        segment_index: segment.segment_index,
        frames: expected_inventory,
        provenance: provenance.map(provenance_reference),
    };
    if inventory != expected {
        return Err(ClientArchiveError::Integrity(
            "depth ZIP inventory differs from the verified segment".into(),
        ));
    }
    Ok(())
}

/// Inspect only bounded ZIP metadata/provenance after the uploader has verified
/// the whole artifact SHA. Never infer a raw archive's format from its filename.
pub fn read_segment_upload_format(path: &Path) -> Result<&'static str, ClientArchiveError> {
    let mut archive = open_bounded_depth_zip(path)?;
    let count = archive.len();
    if count == 0 {
        return Err(ClientArchiveError::Integrity("empty depth ZIP".into()));
    }
    let inventory_bytes =
        read_bounded_zip_member(&mut archive, count - 1, "depth-files.json", 2 * 1024 * 1024)?;
    let inventory: ZipInventory = serde_json::from_slice(&inventory_bytes)
        .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
    let schema = match inventory.schema.as_str() {
        LEGACY_DEPTH_ZIP_SCHEMA if inventory.provenance.is_none() => LEGACY_DEPTH_ZIP_SCHEMA,
        RAW_DEPTH_ZIP_SCHEMA if inventory.provenance.is_some() => RAW_DEPTH_ZIP_SCHEMA,
        _ => {
            return Err(ClientArchiveError::Integrity(
                "unsupported depth ZIP schema/provenance".into(),
            ));
        }
    };
    if inventory.frames.is_empty()
        || count != inventory.frames.len() + 1 + usize::from(inventory.provenance.is_some())
    {
        return Err(ClientArchiveError::Integrity(
            "depth ZIP inventory count mismatch".into(),
        ));
    }
    let mut last_frame = None;
    for (index, frame) in inventory.frames.iter().enumerate() {
        let member = archive
            .by_index(index)
            .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
        if frame.path != format!("depth_{}.exr", frame.session_frame)
            || member.name() != frame.path
            || member.size() != frame.size_bytes
            || frame.size_bytes == 0
            || !valid_sha256(&frame.sha256)
            || last_frame.is_some_and(|last| frame.session_frame <= last)
        {
            return Err(ClientArchiveError::Integrity(
                "depth ZIP inventory frame mismatch".into(),
            ));
        }
        last_frame = Some(frame.session_frame);
    }
    if let Some(reference) = inventory.provenance {
        if reference.path != "provenance.json"
            || reference.size_bytes == 0
            || reference.size_bytes > MAX_RAW_PROVENANCE_BYTES
        {
            return Err(ClientArchiveError::Integrity(
                "invalid raw provenance reference".into(),
            ));
        }
        let bytes = read_bounded_zip_member(
            &mut archive,
            count - 2,
            "provenance.json",
            MAX_RAW_PROVENANCE_BYTES,
        )?;
        if bytes.len() as u64 != reference.size_bytes || sha256_bytes(&bytes) != reference.sha256 {
            return Err(ClientArchiveError::Integrity(
                "raw provenance size/hash mismatch".into(),
            ));
        }
        let provenance: RawDepthProvenance = serde_json::from_slice(&bytes)
            .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
        provenance.validate()?;
        if provenance.segment_index != inventory.segment_index
            || provenance.frames.len() != inventory.frames.len()
            || provenance
                .frames
                .iter()
                .zip(&inventory.frames)
                .any(|(pose, frame)| {
                    pose.session_frame != frame.session_frame
                        || pose.depth.present_seq != frame.present_seq
                })
        {
            return Err(ClientArchiveError::Integrity(
                "raw provenance/inventory alignment mismatch".into(),
            ));
        }
    }
    Ok(schema)
}

fn read_bounded_zip_member(
    archive: &mut zip::ZipArchive<File>,
    index: usize,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, ClientArchiveError> {
    let mut member = archive
        .by_index(index)
        .map_err(|error| ClientArchiveError::Integrity(error.to_string()))?;
    if member.name() != name
        || member.size() > limit
        || member.compression() != CompressionMethod::Stored
    {
        return Err(ClientArchiveError::Integrity(format!(
            "invalid bounded ZIP member {name}"
        )));
    }
    let declared = member.size();
    let mut bytes = Vec::new();
    member
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 != declared || bytes.len() as u64 > limit {
        return Err(ClientArchiveError::Integrity(format!(
            "ZIP member {name} length mismatch"
        )));
    }
    Ok(bytes)
}

fn open_bounded_depth_zip(path: &Path) -> Result<zip::ZipArchive<File>, ClientArchiveError> {
    let mut file = File::open(path).map_err(io_error)?;
    let len = file.metadata().map_err(io_error)?.len();
    let tail_len = len.min(65_557);
    file.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(io_error)?;
    let mut tail = vec![0; tail_len as usize];
    file.read_exact(&mut tail).map_err(io_error)?;
    let eocd = tail
        .windows(4)
        .rposition(|part| part == b"PK\x05\x06")
        .filter(|offset| {
            tail.len() >= offset + 22
                && *offset
                    + 22
                    + u16::from_le_bytes(tail[offset + 20..offset + 22].try_into().unwrap())
                        as usize
                    == tail.len()
        })
        .ok_or_else(|| ClientArchiveError::Integrity("depth ZIP lacks bounded EOCD".into()))?;
    let u16_at =
        |offset| u16::from_le_bytes(tail[eocd + offset..eocd + offset + 2].try_into().unwrap());
    let u32_at =
        |offset| u32::from_le_bytes(tail[eocd + offset..eocd + offset + 4].try_into().unwrap());
    if u16_at(4) != 0 || u16_at(6) != 0 {
        return Err(ClientArchiveError::Integrity(
            "split depth ZIP unsupported".into(),
        ));
    }
    let mut entries = u64::from(u16_at(10));
    let mut directory_size = u64::from(u32_at(12));
    let mut directory_offset = u64::from(u32_at(16));
    let mut locator = [0u8; 20];
    if let Some(locator_offset) = (len - tail_len + eocd as u64).checked_sub(20) {
        file.seek(SeekFrom::Start(locator_offset))
            .map_err(io_error)?;
        file.read_exact(&mut locator).map_err(io_error)?;
    }
    if entries == u64::from(u16::MAX)
        || directory_size == u64::from(u32::MAX)
        || directory_offset == u64::from(u32::MAX)
        || &locator[..4] == b"PK\x06\x07"
    {
        if &locator[..4] != b"PK\x06\x07"
            || locator[4..8] != [0; 4]
            || locator[16..20] != [1, 0, 0, 0]
        {
            return Err(ClientArchiveError::Integrity(
                "invalid ZIP64 locator".into(),
            ));
        }
        let record_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap());
        file.seek(SeekFrom::Start(record_offset))
            .map_err(io_error)?;
        let mut record = [0u8; 56];
        file.read_exact(&mut record).map_err(io_error)?;
        let u64_at = |offset| u64::from_le_bytes(record[offset..offset + 8].try_into().unwrap());
        if &record[..4] != b"PK\x06\x06"
            || !(44..=1024).contains(&u64_at(4))
            || record[16..24] != [0; 8]
        {
            return Err(ClientArchiveError::Integrity(
                "invalid bounded ZIP64 record".into(),
            ));
        }
        entries = u64_at(32);
        directory_size = u64_at(40);
        directory_offset = u64_at(48);
    }
    if entries == 0
        || entries > 4098
        || directory_size > 2 * 1024 * 1024
        || directory_offset
            .checked_add(directory_size)
            .is_none_or(|end| end > len)
    {
        return Err(ClientArchiveError::Integrity(
            "depth ZIP central directory exceeds bounds".into(),
        ));
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    zip::ZipArchive::new(file).map_err(|error| ClientArchiveError::Integrity(error.to_string()))
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn copy_verified_to_writer<W: Write>(
    source: &Path,
    expected_size: u64,
    expected_sha256: &str,
    writer: &mut W,
) -> Result<(), ClientArchiveError> {
    let mut file = File::open(source).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read]).map_err(io_error)?;
        hasher.update(&buffer[..read]);
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| ClientArchiveError::Integrity("object size overflow".into()))?;
    }
    let sha256 = format_digest(hasher.finalize());
    if total != expected_size || sha256 != expected_sha256 {
        return Err(ClientArchiveError::Integrity(format!(
            "source changed while creating ZIP: {}",
            source.display()
        )));
    }
    Ok(())
}

fn local_object(
    relative_key: &str,
    kind: ArchiveObjectKind,
    priority: ArchivePriority,
    path: &Path,
) -> Result<LocalArchiveObject, ClientArchiveError> {
    ensure_safe_relative_key(relative_key)?;
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(ClientArchiveError::Integrity(format!(
            "archive object is not a non-empty regular file: {}",
            path.display()
        )));
    }
    Ok(LocalArchiveObject {
        relative_key: relative_key.into(),
        local_path: path.to_path_buf(),
        kind,
        priority,
        size_bytes: metadata.len(),
        sha256: sha256_file(path)?,
    })
}

fn rebase_materialized_paths(archive: &mut MaterializedArchive, root: &Path) {
    archive.manifest_path = root.join(MANIFEST_RELATIVE_KEY);
    for object in &mut archive.objects {
        object.local_path = root.join(&object.relative_key);
    }
}

fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, ClientArchiveError> {
    let metadata = regular_file_metadata(path)?;
    if metadata.len() == 0 || metadata.len() > MAX_SMALL_EVIDENCE_BYTES {
        return Err(ClientArchiveError::Contract(format!(
            "JSONL evidence size is invalid: {}",
            path.display()
        )));
    }
    let file = File::open(path).map_err(io_error)?;
    let mut rows = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(io_error)?;
        if line.trim().is_empty() {
            return Err(ClientArchiveError::Contract(format!(
                "empty JSONL row {} in {}",
                index + 1,
                path.display()
            )));
        }
        rows.push(serde_json::from_str(&line).map_err(|error| {
            ClientArchiveError::Contract(format!(
                "parse JSONL row {} in {}: {error}",
                index + 1,
                path.display()
            ))
        })?);
    }
    Ok(rows)
}

fn read_small_json<T: DeserializeOwned>(path: &Path) -> Result<T, ClientArchiveError> {
    let bytes = read_regular_file(path, Some(MAX_SMALL_EVIDENCE_BYTES))?;
    if bytes.is_empty() {
        return Err(ClientArchiveError::Contract(format!(
            "JSON evidence is empty: {}",
            path.display()
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ClientArchiveError::Contract(format!("parse {}: {error}", path.display())))
}

fn read_regular_file(path: &Path, max: Option<u64>) -> Result<Vec<u8>, ClientArchiveError> {
    let metadata = regular_file_metadata(path)?;
    if max.is_some_and(|limit| metadata.len() > limit) {
        return Err(ClientArchiveError::Contract(format!(
            "file exceeds bounded evidence size: {}",
            path.display()
        )));
    }
    fs::read(path).map_err(io_error)
}

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata, ClientArchiveError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ClientArchiveError::Integrity(format!(
            "path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn ensure_safe_output_root(path: &Path) -> Result<(), ClientArchiveError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(ClientArchiveError::Contract(format!(
            "archive output root is unsafe: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_safe_relative_key(value: &str) -> Result<(), ClientArchiveError> {
    let path = Path::new(value);
    if path.is_absolute()
        || value.contains('\\')
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(ClientArchiveError::Contract(format!(
            "unsafe archive relative key: {value}"
        )));
    }
    Ok(())
}

fn create_parent(path: &Path) -> Result<(), ClientArchiveError> {
    let parent = path.parent().ok_or_else(|| {
        ClientArchiveError::Contract(format!("path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(io_error)?;
    ensure_real_directory(parent)
}

fn ensure_real_directory(path: &Path) -> Result<(), ClientArchiveError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ClientArchiveError::Integrity(format!(
            "path is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn create_new_file(path: &Path) -> Result<File, ClientArchiveError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), ClientArchiveError> {
    let mut file = create_new_file(path)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn sha256_file(path: &Path) -> Result<String, ClientArchiveError> {
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format_digest(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format_digest(Sha256::digest(bytes))
}

fn format_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn io_error(error: impl std::fmt::Display) -> ClientArchiveError {
    ClientArchiveError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_format_reader_rejects_unknown_schema_and_oversized_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unknown.zip");
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("depth-files.json", options).unwrap();
        writer
            .write_all(b"{\"schema\":\"future-zip\",\"segment_index\":0,\"frames\":[]}")
            .unwrap();
        writer.finish().unwrap();
        assert!(read_segment_upload_format(&path).is_err());
        let mut bytes = fs::read(&path).unwrap();
        let eocd = bytes
            .windows(4)
            .rposition(|bytes| bytes == b"PK\x05\x06")
            .unwrap();
        bytes[eocd + 12..eocd + 16].copy_from_slice(&(3 * 1024 * 1024u32).to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(
            read_segment_upload_format(&path)
                .unwrap_err()
                .to_string()
                .contains("central directory exceeds bounds")
        );
    }
    use crate::archive_contract::ArchiveLane;
    use crate::capture_timing::{
        DepthIndexEntry, DepthStorage, INDEX_SCHEMA_V4, IndexProjection, IndexProjectionSource,
        StorageTransform,
    };
    use crate::decode::DepthFormat;
    use crate::depth::Projection;
    use crate::pairing::{AbsentReason, CameraMode, FramePair, PairedDepth, PairingReport};
    use crate::pipeline::{E2eReport, WatermarkStats};
    use crate::segment_finalize::{
        FLUSH_EVIDENCE_SCHEMA, FRAME_STATE_EVIDENCE_SCHEMA, FinalizeRequest, FrameCaptureState,
        FrameStateBoundary, FrameStateEvidence, ProbeFlushedEvidence, SegmentBoundary,
        StreamIdentity, VerifiedPairingEvidence, finalize_segments,
    };

    fn identity() -> StreamIdentity {
        StreamIdentity::new("session-a", "r0000", 7).unwrap()
    }

    fn pair(session_frame: u64, present_seq: u64) -> FramePair {
        FramePair {
            session_frame,
            segment_index: 0,
            segment_frame: u32::try_from(session_frame).unwrap(),
            source_qpc: session_frame as i64 * 1_000,
            duplicate: false,
            camera_mode: CameraMode::Player,
            present_seq: Some(present_seq),
            depth: Some(PairedDepth {
                present_seq,
                depth_name: format!("depth_{session_frame}"),
                dsv_id: "main-scene-v1".into(),
                inherited: false,
            }),
            depth_absent_reason: None,
        }
    }

    fn report(frame_count: u64) -> E2eReport {
        E2eReport {
            pairing: PairingReport {
                total_frames: frame_count,
                eligible_even_frames: frame_count,
                paired_even_frames: frame_count,
                inherited_frames: 0,
                unreferenced_depth_frames: 0,
                coverage: 1.0,
            },
            watermark: WatermarkStats {
                decoded: frame_count,
                ..WatermarkStats::default()
            },
            passed: true,
            failures: Vec::new(),
        }
    }

    fn states(identity: &StreamIdentity, frame_count: u64) -> FrameStateEvidence {
        FrameStateEvidence {
            schema: FRAME_STATE_EVIDENCE_SCHEMA.into(),
            identity: identity.clone(),
            boundaries: vec![FrameStateBoundary {
                session_frame_start: 0,
                frame_count,
                state: FrameCaptureState::Gameplay,
                camera_coverage_proven: true,
                action_coverage_proven: true,
            }],
        }
    }

    fn write_v4_traditional_probe(root: &Path, sequences: &[u64]) {
        fs::create_dir_all(root.join("raw")).unwrap();
        let mut index = String::new();
        for present_seq in sequences {
            let relative_path = format!("raw/{present_seq:012}.exr");
            // 盘上是 `1-d`；还原后 [1, 1, .75, .5]，是非空 traditional-Z。
            crate::exr_out::write_buffer_frame(
                &root.join(&relative_path),
                &[0.0, 0.0, 0.25, 0.5],
                2,
                2,
            )
            .unwrap();
            let entry = DepthIndexEntry {
                schema: INDEX_SCHEMA_V4.into(),
                present_seq: *present_seq,
                source_frame: *present_seq,
                depth_name: None,
                width: 2,
                height: 2,
                format: DepthFormat::D24S8,
                dsv_id: "main-scene-v1".into(),
                storage: DepthStorage::raw_buffer_exr(StorageTransform::OneMinusD),
                path: relative_path,
                readback_latency_frames: 2,
                projection: IndexProjection::known(
                    Projection::traditional_z(0.1, 100.0, 1.0),
                    Some(1.0),
                    IndexProjectionSource::EngineDefault,
                ),
                health: None,
                pass: None,
            };
            index.push_str(&serde_json::to_string(&entry).unwrap());
            index.push('\n');
        }
        fs::write(root.join("depth-index.jsonl"), index).unwrap();
    }

    fn write_json(path: &Path, value: &impl Serialize) {
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn v4_one_minus_d_is_preserved_through_segment_and_archive_materialization() {
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        let segments_root = temp.path().join("depth-segments");
        let archive_root = temp.path().join("archive");
        let id = identity();
        let pairs = vec![pair(0, 10), pair(1, 12)];
        let pairing = VerifiedPairingEvidence::new(id.clone(), pairs.clone(), report(2)).unwrap();
        let flushed = ProbeFlushedEvidence {
            schema: FLUSH_EVIDENCE_SCHEMA.into(),
            identity: id.clone(),
            writer_queue_depth: 0,
            last_present_seq: 12,
            cutoff_present_seq: 12,
        };
        let frame_states = states(&id, 2);
        let boundaries = [SegmentBoundary {
            segment_index: 0,
            session_frame_start: 0,
            frame_count: 2,
        }];
        write_v4_traditional_probe(&probe, &[10, 12]);
        let published = finalize_segments(&FinalizeRequest {
            identity: &id,
            archive_lane: ArchiveLane::Lane30,
            flushed: &flushed,
            pairing: &pairing,
            frame_states: &frame_states,
            boundaries: &boundaries,
            probe_root: &probe,
            output_root: &segments_root,
        })
        .unwrap();
        assert_eq!(published.len(), 1);
        assert!(published[0].candidate_eligible);
        let candidate = verify_candidate_segment(
            &published[0].manifest_path,
            &id,
            0,
            &published[0].manifest_sha256,
        )
        .unwrap();

        let supplements_root = temp.path().join("supplements");
        fs::create_dir(&supplements_root).unwrap();
        for name in [
            "depth-profile-evidence.json",
            "dsv-thumbnail.png",
            "action_camera_15fps.json",
            "systeminfo.json",
        ] {
            fs::write(supplements_root.join(name), format!("fixture:{name}")).unwrap();
        }
        let recipe_path = supplements_root.join("variant-s-recipe.json");
        write_json(
            &recipe_path,
            &VariantRecipeEvidence {
                schema: VARIANT_RECIPE_SCHEMA.into(),
                identity: id.clone(),
                frames: candidate
                    .frames
                    .iter()
                    .map(|frame| VariantRecipeFrame {
                        session_frame: frame.session_frame,
                        depth_name: format!("depth_{}", frame.session_frame),
                        expected_sha256: frame.sha256.clone(),
                    })
                    .collect(),
            },
        );
        let supplements = ArchiveSupplementFiles {
            depth_profile_evidence: supplements_root.join("depth-profile-evidence.json"),
            dsv_thumbnail: supplements_root.join("dsv-thumbnail.png"),
            action_camera: supplements_root.join("action_camera_15fps.json"),
            systeminfo: supplements_root.join("systeminfo.json"),
            variant_s_recipe: recipe_path,
        };
        let sources = [SegmentCandidateSource {
            segment_index: 0,
            manifest_path: published[0].manifest_path.clone(),
            manifest_sha256: published[0].manifest_sha256.clone(),
        }];
        let request = ArchiveBuildRequest {
            identity: &id,
            lane: ArchiveLane::Lane30,
            segments: &sources,
            frame_pairs: &pairs,
            supplements: &supplements,
            output_root: &archive_root,
        };
        let archive = materialize_archive(&request).unwrap();
        verify_materialized_archive(&archive).unwrap();
        assert!(
            archive.manifest_path.is_file(),
            "manifest must be published last"
        );
        assert!(
            archive
                .objects
                .iter()
                .all(|object| object.local_path.is_file())
        );

        let combined = archive
            .objects
            .iter()
            .find(|object| object.kind == ArchiveObjectKind::DepthIndex)
            .unwrap();
        let decoded = zstd::decode_all(File::open(&combined.local_path).unwrap()).unwrap();
        let rows = String::from_utf8(decoded)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row["schema"] == INDEX_SCHEMA_V4));
        assert!(
            rows.iter()
                .all(|row| row["storage"]["transform"] == "one_minus_d")
        );

        let variant_r = archive
            .objects
            .iter()
            .find(|object| object.kind == ArchiveObjectKind::VariantRArchive)
            .unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&variant_r.local_path).unwrap()).unwrap();
        assert!(zip.by_name("depth_0.exr").is_ok());
        assert!(zip.by_name("depth_1.exr").is_ok());
        assert!(zip.by_name("depth-files.json").is_ok());

        let clip_zip_path = temp.path().join("clip-depth-00000.zip");
        let clip_zip_request = SegmentUploadRequest {
            identity: &id,
            segment_index: 0,
            manifest_path: &published[0].manifest_path,
            manifest_sha256: &published[0].manifest_sha256,
            output_path: &clip_zip_path,
            raw_evidence: None,
        };
        let clip_zip = materialize_segment_upload(&clip_zip_request).unwrap();
        assert_eq!(
            read_segment_upload_format(&clip_zip_path).unwrap(),
            LEGACY_DEPTH_ZIP_SCHEMA
        );
        assert_eq!(clip_zip.frame_count, 2);
        assert!(clip_zip.size_bytes > 0);
        assert_eq!(
            materialize_segment_upload(&clip_zip_request)
                .unwrap()
                .sha256,
            clip_zip.sha256,
            "restart replay must reuse the exact immutable ZIP"
        );
        let second_path = temp.path().join("clip-depth-00000-copy.zip");
        let second = materialize_segment_upload(&SegmentUploadRequest {
            output_path: &second_path,
            ..clip_zip_request.clone()
        })
        .unwrap();
        assert_eq!(
            second.sha256, clip_zip.sha256,
            "same verified EXRs and inventory must produce deterministic ZIP bytes"
        );
        let mut tampered = fs::read(&clip_zip_path).unwrap();
        tampered[0] ^= 0xff;
        fs::write(&clip_zip_path, tampered).unwrap();
        assert!(matches!(
            materialize_segment_upload(&clip_zip_request),
            Err(ClientArchiveError::Integrity(_))
        ));

        let replayed = materialize_archive(&request).unwrap();
        assert_eq!(replayed.manifest_sha256, archive.manifest_sha256);
        fs::write(&combined.local_path, b"tampered").unwrap();
        assert!(matches!(
            verify_materialized_archive(&archive),
            Err(ClientArchiveError::Integrity(_))
        ));
        assert!(
            archive_root.exists(),
            "integrity failure must retain local archive"
        );
    }

    #[test]
    fn finalize_evidence_requires_pairs_state_and_report_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = FinalizeEvidencePaths {
            frame_pairs_jsonl: temp.path().join("frame-pairs.jsonl"),
            frame_states_json: temp.path().join("frame-state.json"),
            e2e_report_json: temp.path().join("report.json"),
        };
        let id = identity();
        let pair = pair(0, 10);
        fs::write(
            &paths.frame_pairs_jsonl,
            format!("{}\n", serde_json::to_string(&pair).unwrap()),
        )
        .unwrap();
        write_json(&paths.frame_states_json, &states(&id, 1));
        write_json(&paths.e2e_report_json, &report(1));
        load_finalize_evidence(&paths).unwrap();

        for missing in [
            &paths.frame_pairs_jsonl,
            &paths.frame_states_json,
            &paths.e2e_report_json,
        ] {
            let bytes = fs::read(missing).unwrap();
            fs::remove_file(missing).unwrap();
            assert!(load_finalize_evidence(&paths).is_err());
            fs::write(missing, bytes).unwrap();
        }
    }

    #[test]
    fn empty_candidate_set_never_creates_an_archive() {
        let temp = tempfile::tempdir().unwrap();
        let id = identity();
        let supplements = ArchiveSupplementFiles {
            depth_profile_evidence: temp.path().join("missing-profile"),
            dsv_thumbnail: temp.path().join("missing-thumbnail"),
            action_camera: temp.path().join("missing-action-camera"),
            systeminfo: temp.path().join("missing-systeminfo"),
            variant_s_recipe: temp.path().join("missing-recipe"),
        };
        let error = materialize_archive(&ArchiveBuildRequest {
            identity: &id,
            lane: ArchiveLane::Lane30,
            segments: &[],
            frame_pairs: &[],
            supplements: &supplements,
            output_root: &temp.path().join("archive"),
        })
        .unwrap_err();
        assert!(error.to_string().contains("at least one"));
        assert!(!temp.path().join("archive").exists());
    }

    #[test]
    fn combined_index_accepts_only_a_frame_pair_proven_alias_across_segments() {
        let temp = tempfile::tempdir().unwrap();
        let id = identity();
        let first_index = temp.path().join("first.jsonl");
        let second_index = temp.path().join("second.jsonl");
        fs::write(&first_index, "{\"present_seq\":100,\"session_frame\":0}\n").unwrap();
        fs::write(&second_index, "{\"present_seq\":100,\"session_frame\":2}\n").unwrap();
        let segments = [
            VerifiedCandidateSegment {
                identity: id.clone(),
                lane: ArchiveLane::Lane30,
                segment_index: 0,
                session_frame_start: 0,
                frame_count: 2,
                index_path: first_index,
                frames: vec![],
            },
            VerifiedCandidateSegment {
                identity: id,
                lane: ArchiveLane::Lane30,
                segment_index: 1,
                session_frame_start: 2,
                frame_count: 1,
                index_path: second_index,
                frames: vec![],
            },
        ];
        let mut odd = pair(1, 100);
        odd.depth = None;
        odd.depth_absent_reason = Some(AbsentReason::OddFrameBySpec);
        let mut duplicate_even = pair(2, 100);
        duplicate_even.duplicate = true;
        duplicate_even.depth.as_mut().unwrap().inherited = true;
        let pairs = [pair(0, 100), odd, duplicate_even];
        let output = temp.path().join("combined.jsonl.zst");
        write_combined_index(&output, &segments, &pairs).unwrap();
        let decoded = zstd::decode_all(File::open(output).unwrap()).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap().lines().count(), 2);

        let mut broken = pairs.to_vec();
        broken[1].present_seq = Some(101);
        let rejected = temp.path().join("rejected.jsonl.zst");
        assert!(matches!(
            write_combined_index(&rejected, &segments, &broken),
            Err(ClientArchiveError::Integrity(_))
        ));
    }
}
