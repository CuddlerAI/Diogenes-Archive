use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use capture_core::input::RawInputEvent;
use capture_core::projection::{FrameProjector, FrameTiming};
use mp4::Mp4Reader;
use serde::{Deserialize, Serialize};

use super::*;

const CURSOR_SCHEMA_VERSION: &str = "dm-data-demo-incremental-cursor-3";
const LEGACY_CURSOR_SCHEMA_VERSION: &str = "dm-data-demo-incremental-cursor-2";
const CURSOR_FILE_NAME: &str = ".incremental-state.json";
const CURSOR_STAGING_PREFIX: &str = ".incremental-state.staging-";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadySegmentRef {
    pub run_id: String,
    pub segment_index: u32,
}

/// The recorder's durable first-run wall-clock sample for live conversion.
///
/// `run.json` is only sealed after recording stops, so the client supplies the
/// same values from its journal while full chunks are being published. Once
/// `run.json` exists, its copy remains authoritative and must agree byte-for-byte
/// with this context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncrementalWallClockContext {
    pub anchor_at_utc: Option<String>,
    pub anchor_qpc: i64,
    pub qpc_frequency: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncrementalReport {
    pub session_id: String,
    pub ready_segments: u32,
    pub ready_frames: u64,
    pub published_chunks: u32,
    pub published_frames: u64,
    pub pending_frames: u64,
    pub output: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct IncrementalCursor {
    schema_version: String,
    session_id: String,
    mapping_content_hash: String,
    catalog: Option<DataModelCatalog>,
    wall_clock: Option<WallClockAnchor>,
    native_format: CursorNativeFormat,
    segments: Vec<CursorSegment>,
    ready_frames: u64,
    published_chunks: u32,
    published_chunk_hashes: Vec<BTreeMap<String, String>>,
    projection: ProjectionCheckpoint,
    video_template: VideoDescriptor,
    tail: Vec<TailSegment>,
}

#[derive(Deserialize)]
struct LegacyCursorIdentity {
    schema_version: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorNativeFormat {
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    chunk_frames: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorSegment {
    run_id: String,
    segment_index: u32,
    frame_start: u64,
    frame_count: u64,
    source_hashes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectionCheckpoint {
    run_id: String,
    previous_timing: CursorFrameTiming,
    active_inputs: Vec<ActiveInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ActiveInput {
    Key { vk: u16, scan_code: u16, flags: u16 },
    MouseButton { vk: u16 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorFrameTiming {
    session_frame: u64,
    output_qpc_start: i64,
    output_qpc_end: i64,
    source_qpc: i64,
    source_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    present_seq: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TailSegment {
    run_id: String,
    segment_index: u32,
    global_frame_start: u64,
    sample_start: u32,
    rows: Vec<RawFrameProjection>,
    video: VideoDescriptor,
    video_sha256: String,
    #[serde(default)]
    audio: Option<SegmentAudioMetadata>,
    #[serde(default)]
    audio_sha256: Option<String>,
}

struct ReferencedValidation {
    session: ValidatedSession,
    segments: Vec<CursorSegment>,
}

pub fn advance_incremental(
    session_dir: &Path,
    work_output: &Path,
    refs: &[ReadySegmentRef],
    wall_clock_context: Option<&IncrementalWallClockContext>,
) -> anyhow::Result<IncrementalReport> {
    ensure_output_outside_source(session_dir, work_output)?;
    cleanup_legacy_migration_staging(work_output)?;
    if work_output.join("metadata.jsonl").is_file() {
        let validated = validate_referenced_session(session_dir, refs, wall_clock_context)?;
        validate_chunk_boundaries(&validated.session)?;
        let report = verify_dataset_binding(work_output, &validated.session)
            .context("incremental work output is already finalized but invalid")?;
        return Ok(incremental_report_from_counts(
            &validated.session.session.session_id,
            validated.segments.len(),
            session_frame_count(&validated.session),
            report.chunk_count,
            validated.session.native_format,
            work_output,
        ));
    }

    let refs = normalized_refs(refs)?;
    let header = validate_session_header(session_dir, &ComplianceProfile::LocalDebugNonTrainable)?;
    let wall_clock = resolve_incremental_wall_clock(session_dir, &header, wall_clock_context)?;
    fs::create_dir_all(work_output)
        .with_context(|| format!("failed to create {}", work_output.display()))?;
    ensure_output_outside_source(session_dir, work_output)?;
    cleanup_cursor_staging(work_output)?;
    let derived_session_dir = dataset_session_dir_from_header(&header, work_output);
    let cursor_path = work_output.join(CURSOR_FILE_NAME);
    reset_legacy_incremental_output(&cursor_path)?;
    fs::create_dir_all(&derived_session_dir)?;
    cleanup_owned_staging_directories(&derived_session_dir)?;

    let cursor = load_usable_cursor(
        &cursor_path,
        &header,
        &refs,
        &derived_session_dir,
        wall_clock,
    )?;
    let mut cursor = match cursor {
        Some(cursor) => {
            advance_from_cursor(session_dir, &header, &derived_session_dir, &refs, cursor)?
        }
        None => recover_incremental(session_dir, &derived_session_dir, &refs, header, wall_clock)?,
    };
    cursor.published_chunk_hashes = chunk_member_hashes(&derived_session_dir)?;
    write_cursor(&cursor_path, &cursor)?;

    Ok(incremental_report_from_counts(
        &cursor.session_id,
        cursor.segments.len(),
        cursor.ready_frames,
        cursor.published_chunks,
        cursor.native_format.into_native()?,
        work_output,
    ))
}

fn reset_legacy_incremental_output(cursor_path: &Path) -> anyhow::Result<()> {
    if !cursor_path.is_file() {
        return Ok(());
    }
    let Ok(probe) = read_json::<LegacyCursorIdentity>(cursor_path) else {
        return Ok(());
    };
    if probe.schema_version != LEGACY_CURSOR_SCHEMA_VERSION {
        return Ok(());
    }
    // v2 did not bind catalog, wall-clock, or published chunk hashes. Remove
    // only the cursor; recovery will compare every retained chunk against fresh
    // RAW-derived canonical bytes before accepting or upgrading it. The v2
    // identity fields are not trusted either: a damaged cursor cannot authorize
    // data loss, but it also must not block recovery of exact canonical chunks.
    fs::remove_file(cursor_path).with_context(|| {
        format!(
            "failed to discard legacy incremental cursor {}",
            cursor_path.display()
        )
    })?;
    Ok(())
}

fn resolve_incremental_wall_clock(
    session_dir: &Path,
    header: &ValidatedSessionHeader,
    supplied: Option<&IncrementalWallClockContext>,
) -> anyhow::Result<Option<WallClockAnchor>> {
    let recorded = first_recorded_wall_clock_run(session_dir)?;

    if let (Some(supplied), Some(recorded)) = (supplied, recorded.as_ref()) {
        ensure!(
            recorded
                .qpc_frequency
                .is_none_or(|value| value == supplied.qpc_frequency)
                && recorded
                    .anchor_qpc
                    .is_none_or(|value| value == supplied.anchor_qpc)
                && recorded
                    .anchor_at_utc
                    .as_ref()
                    .zip(supplied.anchor_at_utc.as_ref())
                    .is_none_or(|(recorded, supplied)| recorded == supplied),
            "incremental wall-clock context differs from the first run manifest"
        );
    }

    let context = match (supplied, recorded) {
        (Some(supplied), Some(recorded)) => Some(IncrementalWallClockContext {
            anchor_at_utc: supplied.anchor_at_utc.clone().or(recorded.anchor_at_utc),
            anchor_qpc: supplied.anchor_qpc,
            qpc_frequency: supplied.qpc_frequency,
        }),
        (Some(supplied), None) => Some(supplied.clone()),
        (None, Some(recorded)) => Some(IncrementalWallClockContext {
            anchor_at_utc: recorded.anchor_at_utc,
            anchor_qpc: recorded
                .anchor_qpc
                .context("recorded wall-clock run has no QPC anchor")?,
            qpc_frequency: recorded
                .qpc_frequency
                .context("recorded wall-clock run has no QPC frequency")?,
        }),
        (None, None) => None,
    };
    let Some(context) = context else {
        return Ok(None);
    };
    ensure!(
        context.qpc_frequency > 0,
        "incremental wall-clock QPC frequency must be positive"
    );
    WallClockAnchor::new(
        &header.session.session_id,
        context.anchor_qpc,
        context
            .anchor_at_utc
            .as_deref()
            .map(|utc| (utc, context.anchor_qpc)),
        Some(context.qpc_frequency),
    )
}

fn first_recorded_wall_clock_run(session_dir: &Path) -> anyhow::Result<Option<RawRun>> {
    let runs_dir = session_dir.join("runs");
    if !runs_dir.is_dir() {
        return Ok(None);
    }
    for run_dir in sorted_directories(&runs_dir, "r")? {
        let run_path = run_dir.join("run.json");
        if !run_path.is_file() {
            continue;
        }
        let run: RawRun = read_json(&run_path)?;
        if run.anchor_qpc.is_some() && run.qpc_frequency.is_some() {
            return Ok(Some(run));
        }
    }
    Ok(None)
}

pub fn finalize_incremental(
    session_dir: &Path,
    work_output: &Path,
    refs: &[ReadySegmentRef],
) -> anyhow::Result<ConversionReport> {
    ensure_output_outside_source(session_dir, work_output)?;
    cleanup_legacy_migration_staging(work_output)?;
    let validated =
        validate_ready_session(session_dir, &ComplianceProfile::LocalDebugNonTrainable)?;
    validate_chunk_boundaries(&validated)?;
    ensure_refs_match_final_session(refs, &validated)?;

    if work_output.join("metadata.jsonl").is_file() {
        let metadata_values: Vec<serde_json::Value> =
            read_jsonl(&work_output.join("metadata.jsonl"))?;
        let finalized_schema = metadata_values
            .first()
            .and_then(serde_json::Value::as_object)
            .and_then(|object| object.get("schema_version"))
            .and_then(serde_json::Value::as_str);
        if finalized_schema == Some(LEGACY_DATA_MODEL_SCHEMA_VERSION) {
            ensure!(
                rebuild_exact_legacy_catalog_output(&validated, work_output)?,
                "legacy finalized DataModel output is not an exact dm-1 predecessor of current RAW"
            );
        }
        let report = match verify_dataset_binding(work_output, &validated) {
            Ok(report) => report,
            Err(_error) if rebuild_exact_legacy_catalog_output(&validated, work_output)? => {
                verify_dataset_binding(work_output, &validated)
                    .context("rebuilt finalized DataModel output does not bind to current RAW")?
            }
            Err(error) => {
                return Err(error)
                    .context("existing finalized DataModel output does not bind to current RAW");
            }
        };
        remove_cursor_artifacts(work_output)?;
        return Ok(ConversionReport {
            session_id: report.session_id,
            frame_count: report.frame_count,
            chunk_count: report.chunk_count,
            output: work_output.to_path_buf(),
        });
    }

    let cursor_path = work_output.join(CURSOR_FILE_NAME);
    let derived_session_dir = dataset_session_dir(&validated, work_output);
    reset_legacy_incremental_output(&cursor_path)?;
    let cursor_is_current = read_json::<IncrementalCursor>(&cursor_path)
        .is_ok_and(|cursor| cursor.schema_version == CURSOR_SCHEMA_VERSION);
    if derived_session_dir.is_dir() && !cursor_is_current {
        ensure!(
            reconcile_exact_incremental_chunks(&validated, &derived_session_dir)?.is_some(),
            "cursorless incremental chunks do not exactly match current RAW"
        );
        validate_existing_full_chunks(&validated, &derived_session_dir)
            .context("upgraded cursorless incremental chunks are still invalid")?;
    }

    let parent = work_output
        .parent()
        .context("incremental work output has no parent")?;
    fs::create_dir_all(parent)?;
    let output_name = work_output
        .file_name()
        .and_then(|value| value.to_str())
        .context("incremental work output has no UTF-8 file name")?;
    cleanup_finalize_staging(parent, output_name)?;
    let staging = parent.join(format!(
        ".{output_name}.finalize-staging-{}",
        unique_suffix()
    ));
    ensure!(
        !staging.exists(),
        "refusing to overwrite finalize staging {}",
        staging.display()
    );
    fs::create_dir(&staging)?;
    let mut guard = StagingGuard::new(staging.clone());
    let staged_report = materialize_dataset(&validated, &staging)?;
    verify_dataset(&staging).context("final incremental dataset failed verification")?;

    if !work_output.exists() {
        fs::rename(&staging, work_output)?;
        guard.disarm();
    } else {
        merge_final_dataset(&validated, &staging, work_output)?;
        verify_dataset_binding(work_output, &validated)
            .context("published incremental dataset failed verification")?;
    }

    Ok(ConversionReport {
        output: work_output.to_path_buf(),
        ..staged_report
    })
}

fn rebuild_exact_legacy_catalog_output(
    session: &ValidatedSession,
    output: &Path,
) -> anyhow::Result<bool> {
    let parent = output
        .parent()
        .context("finalized DataModel output has no parent")?;
    let output_name = output
        .file_name()
        .and_then(|value| value.to_str())
        .context("finalized DataModel output has no UTF-8 file name")?;
    let staging_prefix = format!(".{output_name}.legacy-rebuild-staging-");
    cleanup_owned_staging_prefix(parent, &staging_prefix)?;
    let staging = parent.join(format!("{staging_prefix}{}", unique_suffix()));
    ensure!(!staging.exists(), "legacy rebuild staging already exists");
    fs::create_dir(&staging)?;
    let staging_guard = StagingGuard::new(staging.clone());
    materialize_dataset(session, &staging)?;
    verify_dataset_binding(&staging, session)?;

    let Some(replacements) = catalog_only_legacy_replacements(output, &staging)? else {
        return Ok(false);
    };
    // Patch only the small JSON metadata members. Each replacement is atomic,
    // and the comparison accepts a mix of old and already-replaced members, so
    // a crash between files resumes safely without copying or swapping media.
    for relative in replacements {
        atomic_replace_file(&staging.join(&relative), &output.join(&relative))?;
    }
    drop(staging_guard);
    Ok(true)
}

fn catalog_only_legacy_replacements(
    legacy: &Path,
    canonical: &Path,
) -> anyhow::Result<Option<Vec<PathBuf>>> {
    let legacy_files = dataset_file_paths(legacy)?;
    let canonical_files = dataset_file_paths(canonical)?;
    if legacy_files != canonical_files {
        return Ok(None);
    }
    let mut replacements = Vec::new();
    for relative in legacy_files {
        let legacy_path = legacy.join(&relative);
        let canonical_path = canonical.join(&relative);
        if relative == Path::new("metadata.jsonl")
            || relative.file_name().and_then(|value| value.to_str()) == Some("metadata.json")
            || relative.file_name().and_then(|value| value.to_str()) == Some("meta.json")
        {
            let Some(mut legacy_json) =
                parse_single_json_member(&relative, &fs::read(&legacy_path)?)
            else {
                return Ok(None);
            };
            let Some(canonical_json) =
                parse_single_json_member(&relative, &fs::read(&canonical_path)?)
            else {
                return Ok(None);
            };
            let Some(was_legacy) =
                normalize_legacy_data_model_member(&mut legacy_json, &canonical_json)
            else {
                return Ok(None);
            };
            if legacy_json != canonical_json {
                return Ok(None);
            }
            if was_legacy {
                replacements.push(relative);
            } else if !files_equal(&legacy_path, &canonical_path)? {
                return Ok(None);
            }
        } else if !files_equal(&legacy_path, &canonical_path)? {
            return Ok(None);
        }
    }
    Ok((!replacements.is_empty()).then_some(replacements))
}

/// Normalize exactly the schema delta introduced by DataModel dm-2.
///
/// dm-1 did not define a top-level `catalog`. The canonical absence is eligible,
/// as is the exact catalog value emitted by the short-lived pre-versioned #316
/// writer; standalone verification still rejects that version lie, but finalize
/// can safely repair it after a full canonical comparison. A dm-2 member missing
/// the key, a mismatched catalog, or any unknown schema is corruption, not a
/// migration candidate.
fn normalize_legacy_data_model_member(
    legacy: &mut serde_json::Value,
    canonical: &serde_json::Value,
) -> Option<bool> {
    let canonical_object = canonical.as_object()?;
    if canonical_object.get("schema_version")?.as_str()? != DATA_MODEL_SCHEMA_VERSION {
        return None;
    }
    let canonical_catalog = canonical_object.get("catalog")?.clone();
    let legacy_object = legacy.as_object_mut()?;
    let legacy_schema = legacy_object.get("schema_version")?.as_str()?.to_owned();
    match legacy_schema.as_str() {
        LEGACY_DATA_MODEL_SCHEMA_VERSION => {
            if legacy_object
                .get("catalog")
                .is_some_and(|catalog| catalog != &canonical_catalog)
            {
                return None;
            }
            legacy_object.insert(
                "schema_version".into(),
                serde_json::Value::String(DATA_MODEL_SCHEMA_VERSION.into()),
            );
            legacy_object.insert("catalog".into(), canonical_catalog);
            Some(true)
        }
        DATA_MODEL_SCHEMA_VERSION => {
            (legacy_object.get("catalog") == Some(&canonical_catalog)).then_some(false)
        }
        _ => None,
    }
}

fn parse_single_json_member(path: &Path, bytes: &[u8]) -> Option<serde_json::Value> {
    if path == Path::new("metadata.jsonl") {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let value = serde_json::from_str(lines.next()?).ok()?;
        lines.next().is_none().then_some(value)
    } else {
        serde_json::from_slice(bytes).ok()
    }
}

fn dataset_file_paths(root: &Path) -> anyhow::Result<BTreeSet<PathBuf>> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeSet<PathBuf>) -> anyhow::Result<()> {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(root, &path, files)?;
            } else if path.is_file() {
                files.insert(path.strip_prefix(root)?.to_path_buf());
            }
        }
        Ok(())
    }

    let mut files = BTreeSet::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn cleanup_owned_staging_prefix(parent: &Path, prefix: &str) -> anyhow::Result<()> {
    for entry in fs::read_dir(parent)? {
        let path = entry?.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        {
            fs::remove_dir_all(&path).with_context(|| {
                format!("failed to clean stale migration staging {}", path.display())
            })?;
        }
    }
    Ok(())
}

fn cleanup_legacy_migration_staging(output: &Path) -> anyhow::Result<()> {
    if !output.is_dir() {
        return Ok(());
    }
    let output_name = output
        .file_name()
        .and_then(|value| value.to_str())
        .context("DataModel output has no UTF-8 file name")?;
    if let Some(parent) = output.parent() {
        cleanup_owned_staging_prefix(parent, &format!(".{output_name}.legacy-rebuild-staging-"))?;
    }
    for game in fs::read_dir(output)? {
        let game = game?.path();
        if !game.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&game)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if path.is_dir()
                && name.starts_with('.')
                && name.contains(".legacy-incremental-validation-")
            {
                fs::remove_dir_all(&path).with_context(|| {
                    format!("failed to clean stale migration staging {}", path.display())
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    File::options().write(true).open(source)?.sync_all()?;
    fs::metadata(destination).with_context(|| {
        format!(
            "legacy metadata destination is unavailable: {}",
            destination.display()
        )
    })?;
    fs::rename(source, destination).with_context(|| {
        format!(
            "failed to atomically replace legacy metadata {}",
            destination.display()
        )
    })
}

#[cfg(windows)]
fn atomic_replace_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    File::options().write(true).open(source)?.sync_all()?;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| "failed to atomically replace legacy DataModel metadata")
}

fn validate_referenced_session(
    session_dir: &Path,
    refs: &[ReadySegmentRef],
    wall_clock_context: Option<&IncrementalWallClockContext>,
) -> anyhow::Result<ReferencedValidation> {
    ensure!(
        !refs.is_empty(),
        "incremental conversion has no ready segment refs"
    );
    let refs = normalized_refs(refs)?;
    let header = validate_session_header(session_dir, &ComplianceProfile::LocalDebugNonTrainable)?;
    let wall_clock = resolve_incremental_wall_clock(session_dir, &header, wall_clock_context)?;
    let (segments, cursor_segments, _) =
        validate_refs_from_checkpoint(session_dir, &header, &refs, 0, None)?;
    let ValidatedSessionHeader {
        session,
        mapping,
        profile,
        native_format,
        view_scale,
    } = header;
    Ok(ReferencedValidation {
        session: ValidatedSession {
            session,
            mapping,
            native_format,
            audio_enabled: profile.audio.is_some(),
            segments,
            compliance: ComplianceProfile::LocalDebugNonTrainable,
            wall_clock,
            view_scale,
        },
        segments: cursor_segments,
    })
}

fn validate_refs_from_checkpoint(
    session_dir: &Path,
    header: &ValidatedSessionHeader,
    refs: &[ReadySegmentRef],
    frame_start: u64,
    checkpoint: Option<&ProjectionCheckpoint>,
) -> anyhow::Result<(
    Vec<ValidatedSegment>,
    Vec<CursorSegment>,
    ProjectionCheckpoint,
)> {
    ensure!(!refs.is_empty(), "no new ready segment refs to validate");
    let mut segments = Vec::with_capacity(refs.len());
    let mut cursor_segments = Vec::with_capacity(refs.len());
    let mut current_run = checkpoint.map(|value| value.run_id.clone());
    let mut active_inputs = checkpoint
        .map(|value| value.active_inputs.clone())
        .unwrap_or_default();
    let mut projector = if checkpoint.is_some_and(|value| value.run_id == refs[0].run_id) {
        warm_projector(checkpoint.unwrap(), &header.profile, &header.mapping)?
    } else {
        current_run = None;
        active_inputs.clear();
        FrameProjector::default()
    };
    let mut previous_session_frame = checkpoint.map(|value| value.previous_timing.session_frame);
    let mut previous_qpc_end = checkpoint.map(|value| value.previous_timing.output_qpc_end);
    let mut next_frame_start = frame_start;
    let mut projection = checkpoint.cloned();

    for ready_ref in refs {
        if current_run.as_deref() != Some(&ready_ref.run_id) {
            current_run = Some(ready_ref.run_id.clone());
            projector = FrameProjector::default();
            active_inputs.clear();
        }
        let segment_dir = session_dir
            .join("runs")
            .join(&ready_ref.run_id)
            .join("segments")
            .join(format!("{:05}", ready_ref.segment_index));
        let segment = validate_segment(
            &segment_dir,
            &header.session,
            &header.profile,
            &header.mapping,
            &ready_ref.run_id,
            None,
            header.native_format,
            &mut projector,
            &ComplianceProfile::LocalDebugNonTrainable,
        )?;
        let first = segment
            .rows
            .first()
            .context("ready segment has no frame rows")?;
        if let Some(previous) = previous_session_frame {
            ensure!(
                first.session_frame == previous + 1,
                "session_frame is not continuous across referenced ready segments"
            );
        } else {
            ensure!(
                first.session_frame == 0,
                "first referenced ready segment does not begin at session frame zero"
            );
        }
        if let Some(previous) = previous_qpc_end {
            ensure!(
                first.output_qpc_start >= previous,
                "referenced ready segment QPC timeline moved backwards"
            );
        }
        previous_session_frame = segment.rows.last().map(|row| row.session_frame);
        previous_qpc_end = segment.rows.last().map(|row| row.output_qpc_end);
        update_active_inputs(&mut active_inputs, &segment.events)?;
        let previous_timing = CursorFrameTiming::from_row(
            segment
                .rows
                .last()
                .context("ready segment has no final frame row")?,
        );
        projection = Some(ProjectionCheckpoint {
            run_id: ready_ref.run_id.clone(),
            previous_timing,
            active_inputs: active_inputs.clone(),
        });
        let frame_count = u64::try_from(segment.rows.len())?;
        cursor_segments.push(CursorSegment {
            run_id: ready_ref.run_id.clone(),
            segment_index: ready_ref.segment_index,
            frame_start: next_frame_start,
            frame_count,
            source_hashes: segment.source_hashes.clone(),
        });
        next_frame_start = next_frame_start
            .checked_add(frame_count)
            .context("incremental ready frame count overflow")?;
        segments.push(segment);
    }
    Ok((
        segments,
        cursor_segments,
        projection.context("ready refs produced no projection checkpoint")?,
    ))
}

impl CursorFrameTiming {
    fn from_row(row: &RawFrameProjection) -> Self {
        Self {
            session_frame: row.session_frame,
            output_qpc_start: row.output_qpc_start,
            output_qpc_end: row.output_qpc_end,
            source_qpc: row.source_qpc,
            source_seq: row.source_seq,
            present_seq: row.present_seq,
        }
    }

    fn warm_timing(&self) -> FrameTiming {
        FrameTiming {
            frame: 0,
            session_frame: self.session_frame,
            output_qpc_start: self.output_qpc_start,
            output_qpc_end: self.output_qpc_end,
            source_qpc: self.source_qpc,
            source_seq: self.source_seq,
            present_seq: self.present_seq,
            duplicate: false,
            source_superseded: 0,
            queue_drop: 0,
        }
    }
}

fn warm_projector(
    checkpoint: &ProjectionCheckpoint,
    profile: &CaptureProfile,
    mapping: &MappingSnapshot,
) -> anyhow::Result<FrameProjector> {
    let timing = checkpoint.previous_timing.warm_timing();
    let events = checkpoint
        .active_inputs
        .iter()
        .enumerate()
        .map(|(index, active)| {
            Ok(active.warm_event(
                u64::try_from(index + 1)?,
                &checkpoint.run_id,
                timing.output_qpc_start,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut projector = FrameProjector::default();
    projector.project_segment(
        std::slice::from_ref(&timing),
        &events,
        &checkpoint.run_id,
        profile,
        mapping.mapping(),
    )?;
    Ok(projector)
}

impl ActiveInput {
    fn warm_event(&self, seq: u64, run_id: &str, qpc: i64) -> RawInputEvent {
        let mut event = RawInputEvent {
            seq,
            run_id: run_id.to_owned(),
            qpc,
            event_type: String::new(),
            device: String::new(),
            vk: None,
            scan_code: None,
            flags: None,
            repeat: None,
            dx: None,
            dy: None,
            absolute: None,
            delta: None,
            horizontal: None,
            control: false,
        };
        match self {
            Self::Key {
                vk,
                scan_code,
                flags,
            } => {
                event.event_type = "key_down".into();
                event.device = "incremental-cursor-keyboard".into();
                event.vk = Some(*vk);
                event.scan_code = Some(*scan_code);
                event.flags = Some(*flags);
                event.repeat = Some(false);
            }
            Self::MouseButton { vk } => {
                event.event_type = "mouse_down".into();
                event.device = "incremental-cursor-mouse".into();
                event.vk = Some(*vk);
            }
        }
        event
    }
}

fn update_active_inputs(
    active: &mut Vec<ActiveInput>,
    events: &[RawInputEvent],
) -> anyhow::Result<()> {
    for event in events {
        if event.control || event.repeat.unwrap_or(false) {
            continue;
        }
        match event.event_type.as_str() {
            "key_down" | "key_up" => {
                let vk = event.vk.context("keyboard event has no vk")?;
                let scan_code = event.scan_code.context("keyboard event has no scan_code")?;
                let flags = event.flags.unwrap_or_default();
                let extended = flags & 0x06;
                active.retain(|item| {
                    !matches!(
                        item,
                        ActiveInput::Key {
                            scan_code: active_scan,
                            flags: active_flags,
                            ..
                        } if *active_scan == scan_code && *active_flags & 0x06 == extended
                    )
                });
                if event.event_type == "key_down" {
                    active.push(ActiveInput::Key {
                        vk,
                        scan_code,
                        flags,
                    });
                }
            }
            "mouse_down" | "mouse_up" => {
                let vk = event.vk.context("mouse button event has no vk")?;
                active.retain(|item| {
                    !matches!(item, ActiveInput::MouseButton { vk: active_vk } if *active_vk == vk)
                });
                if event.event_type == "mouse_down" {
                    active.push(ActiveInput::MouseButton { vk });
                }
            }
            "mouse_move" | "wheel" => {}
            other => anyhow::bail!("unsupported Raw Input event type {other}"),
        }
    }
    active.sort();
    Ok(())
}

fn normalized_refs(refs: &[ReadySegmentRef]) -> anyhow::Result<Vec<ReadySegmentRef>> {
    let mut normalized = refs.to_vec();
    normalized.sort();
    ensure!(
        normalized.windows(2).all(|window| window[0] != window[1]),
        "ready segment refs contain duplicates"
    );

    let mut previous_run = None::<u32>;
    let mut current_run = None::<String>;
    let mut expected_segment = 0_u32;
    for ready_ref in &normalized {
        ensure_safe_component(&ready_ref.run_id, "ready segment run id")?;
        let run_index = parse_run_index(&ready_ref.run_id)?;
        if current_run.as_deref() != Some(&ready_ref.run_id) {
            ensure!(
                previous_run.is_none_or(|previous| run_index > previous),
                "ready segment run refs are not strictly ordered"
            );
            previous_run = Some(run_index);
            current_run = Some(ready_ref.run_id.clone());
            expected_segment = 0;
        }
        ensure!(
            ready_ref.segment_index == expected_segment,
            "ready segment refs for {} are not contiguous from segment zero",
            ready_ref.run_id
        );
        expected_segment = expected_segment
            .checked_add(1)
            .context("ready segment index overflow")?;
    }
    Ok(normalized)
}

fn parse_run_index(run_id: &str) -> anyhow::Result<u32> {
    let digits = run_id
        .strip_prefix('r')
        .context("ready segment run id must start with r")?;
    ensure!(
        digits.len() == 4 && digits.bytes().all(|byte| byte.is_ascii_digit()),
        "ready segment run id must use rNNNN"
    );
    digits.parse().context("ready segment run index overflow")
}

fn ensure_refs_match_final_session(
    refs: &[ReadySegmentRef],
    session: &ValidatedSession,
) -> anyhow::Result<()> {
    let actual = normalized_refs(refs)?;
    let expected = session
        .segments
        .iter()
        .map(|segment| ReadySegmentRef {
            run_id: segment.run_id.clone(),
            segment_index: segment.segment_index,
        })
        .collect::<Vec<_>>();
    ensure!(
        actual == expected,
        "final ready segment refs do not exactly cover run.json payloads"
    );
    Ok(())
}

fn dataset_session_dir(session: &ValidatedSession, output: &Path) -> PathBuf {
    output
        .join(slug(&session.mapping.mapping().game.id))
        .join(&session.session.session_id)
}

fn dataset_session_dir_from_header(header: &ValidatedSessionHeader, output: &Path) -> PathBuf {
    output
        .join(slug(&header.mapping.mapping().game.id))
        .join(&header.session.session_id)
}

fn session_frame_count(session: &ValidatedSession) -> u64 {
    session
        .segments
        .iter()
        .map(|segment| segment.rows.len() as u64)
        .sum()
}

fn incremental_report_from_counts(
    session_id: &str,
    ready_segments: usize,
    ready_frames: u64,
    published_chunks: u32,
    native_format: NativeFormat,
    output: &Path,
) -> IncrementalReport {
    let published_frames =
        (u64::from(published_chunks) * native_format.chunk_frames as u64).min(ready_frames);
    IncrementalReport {
        session_id: session_id.to_owned(),
        ready_segments: u32::try_from(ready_segments).unwrap_or(u32::MAX),
        ready_frames,
        published_chunks,
        published_frames,
        pending_frames: ready_frames - published_frames,
        output: output.to_path_buf(),
    }
}

impl CursorNativeFormat {
    fn from_native(value: NativeFormat) -> anyhow::Result<Self> {
        Ok(Self {
            width: value.width,
            height: value.height,
            fps_num: value.fps_num,
            fps_den: value.fps_den,
            chunk_frames: u32::try_from(value.chunk_frames)?,
        })
    }

    fn into_native(self) -> anyhow::Result<NativeFormat> {
        NativeFormat::new(
            self.width,
            self.height,
            self.fps_num,
            self.fps_den,
            self.chunk_frames,
        )
    }
}

fn load_usable_cursor(
    cursor_path: &Path,
    header: &ValidatedSessionHeader,
    refs: &[ReadySegmentRef],
    derived_session_dir: &Path,
    wall_clock: Option<WallClockAnchor>,
) -> anyhow::Result<Option<IncrementalCursor>> {
    let Ok(cursor) = read_json::<IncrementalCursor>(cursor_path) else {
        return Ok(None);
    };
    validate_cursor(&cursor, header, refs, wall_clock)?;
    let session = validated_session_from_header(header, Vec::new(), wall_clock);
    if validate_existing_full_chunks(&session, derived_session_dir).ok()
        != Some(cursor.published_chunks)
        || chunk_member_hashes(derived_session_dir)? != cursor.published_chunk_hashes
    {
        return Ok(None);
    }
    Ok(Some(cursor))
}

fn validate_cursor(
    cursor: &IncrementalCursor,
    header: &ValidatedSessionHeader,
    refs: &[ReadySegmentRef],
    wall_clock: Option<WallClockAnchor>,
) -> anyhow::Result<()> {
    ensure!(
        cursor.schema_version == CURSOR_SCHEMA_VERSION,
        "unsupported incremental cursor schema"
    );
    ensure!(
        cursor.session_id == header.session.session_id
            && cursor.mapping_content_hash == header.mapping.content_hash()
            && cursor.catalog == data_model_catalog(header.session.catalog.as_ref())
            && cursor.wall_clock == wall_clock,
        "incremental cursor source identity mismatch"
    );
    ensure!(
        cursor.native_format == CursorNativeFormat::from_native(header.native_format)?,
        "incremental cursor native format mismatch"
    );
    ensure!(
        !cursor.segments.is_empty() && cursor.segments.len() <= refs.len(),
        "incremental cursor segment prefix is invalid"
    );
    let mut next_frame = 0_u64;
    for (segment, ready_ref) in cursor.segments.iter().zip(refs) {
        ensure!(
            segment.run_id == ready_ref.run_id
                && segment.segment_index == ready_ref.segment_index
                && segment.frame_start == next_frame
                && segment.frame_count > 0,
            "incremental cursor does not match the ready ref prefix"
        );
        let valid_payload_count = if header.profile.audio.is_some() {
            matches!(segment.source_hashes.len(), 4 | 5)
        } else {
            segment.source_hashes.len() == 4
        };
        ensure!(
            valid_payload_count
                && segment
                    .source_hashes
                    .values()
                    .all(|hash| valid_sha256(hash)),
            "incremental cursor source hashes are invalid"
        );
        next_frame = next_frame
            .checked_add(segment.frame_count)
            .context("incremental cursor frame count overflow")?;
    }
    ensure!(
        next_frame == cursor.ready_frames,
        "incremental cursor ready frame count mismatch"
    );
    let native = cursor.native_format.into_native()?;
    let expected_chunks = cursor.ready_frames / native.chunk_frames as u64;
    ensure!(
        u64::from(cursor.published_chunks) == expected_chunks,
        "incremental cursor published chunk count mismatch"
    );
    ensure!(
        cursor.published_chunk_hashes.len() == cursor.published_chunks as usize
            && cursor
                .published_chunk_hashes
                .iter()
                .all(|members| !members.is_empty()
                    && members.values().all(|hash| valid_sha256(hash))),
        "incremental cursor published chunk hashes are invalid"
    );
    ensure!(
        cursor.projection.run_id
            == cursor
                .segments
                .last()
                .context("incremental cursor has no final segment")?
                .run_id
            && cursor
                .projection
                .previous_timing
                .session_frame
                .checked_add(1)
                == Some(cursor.ready_frames),
        "incremental cursor projection checkpoint mismatch"
    );
    ensure!(
        cursor
            .projection
            .active_inputs
            .windows(2)
            .all(|window| window[0] < window[1]),
        "incremental cursor active input state is not sorted and unique"
    );
    ensure!(
        u32::from(cursor.video_template.width) == native.width
            && u32::from(cursor.video_template.height) == native.height,
        "incremental cursor video template mismatch"
    );
    validate_tail(cursor)?;
    Ok(())
}

fn validate_tail(cursor: &IncrementalCursor) -> anyhow::Result<()> {
    let native = cursor.native_format.into_native()?;
    let published_frames = u64::from(cursor.published_chunks) * native.chunk_frames as u64;
    let mut next_frame = published_frames;
    for tail in &cursor.tail {
        ensure!(
            tail.global_frame_start == next_frame && !tail.rows.is_empty(),
            "incremental cursor tail is not contiguous"
        );
        let sample_end = u64::from(tail.sample_start)
            .checked_add(u64::try_from(tail.rows.len())?)
            .context("incremental cursor tail sample interval overflow")?;
        ensure!(
            sample_end <= u64::from(tail.video.sample_count) && valid_sha256(&tail.video_sha256),
            "incremental cursor tail source identity is invalid"
        );
        if let Some(audio) = &tail.audio {
            audio.validate()?;
            ensure!(
                audio.has_artifact() == tail.audio_sha256.is_some()
                    && tail.audio_sha256.as_deref().is_none_or(valid_sha256),
                "incremental cursor tail audio identity is invalid"
            );
        } else {
            ensure!(
                tail.audio_sha256.is_none(),
                "legacy incremental tail carries an audio hash"
            );
        }
        for (offset, row) in tail.rows.iter().enumerate() {
            ensure!(
                row.session_frame == next_frame + offset as u64 && row.run_id == tail.run_id,
                "incremental cursor tail frame identity mismatch"
            );
        }
        ensure!(
            cursor.segments.iter().any(|segment| {
                segment.run_id == tail.run_id
                    && segment.segment_index == tail.segment_index
                    && segment.source_hashes.get("video.mp4") == Some(&tail.video_sha256)
                    && segment.source_hashes.get(AUDIO_ARTIFACT_NAME) == tail.audio_sha256.as_ref()
            }),
            "incremental cursor tail is not backed by a validated segment"
        );
        next_frame = next_frame
            .checked_add(u64::try_from(tail.rows.len())?)
            .context("incremental cursor tail frame count overflow")?;
    }
    ensure!(
        next_frame == cursor.ready_frames,
        "incremental cursor tail does not cover pending frames"
    );
    Ok(())
}

fn advance_from_cursor(
    session_dir: &Path,
    header: &ValidatedSessionHeader,
    derived_session_dir: &Path,
    refs: &[ReadySegmentRef],
    mut cursor: IncrementalCursor,
) -> anyhow::Result<IncrementalCursor> {
    let new_refs = &refs[cursor.segments.len()..];
    if new_refs.is_empty() {
        return Ok(cursor);
    }
    let (new_segments, new_cursor_segments, projection) = validate_refs_from_checkpoint(
        session_dir,
        header,
        new_refs,
        cursor.ready_frames,
        Some(&cursor.projection),
    )?;
    validate_chunk_boundaries_from(
        cursor.ready_frames,
        &new_segments,
        header.native_format,
        &cursor.video_template,
    )?;

    for (segment, cursor_segment) in new_segments.iter().zip(&new_cursor_segments) {
        cursor.tail.push(TailSegment::from_validated(
            segment,
            cursor_segment.frame_start,
        )?);
    }
    cursor.ready_frames = new_cursor_segments
        .last()
        .and_then(|segment| segment.frame_start.checked_add(segment.frame_count))
        .context("new ready segments did not advance the cursor")?;
    cursor.segments.extend(new_cursor_segments);
    cursor.projection = projection;

    let full_chunks =
        u32::try_from(cursor.ready_frames / header.native_format.chunk_frames as u64)?;
    let context = validated_session_from_header(header, Vec::new(), cursor.wall_clock);
    publish_full_chunks_from_tail(
        session_dir,
        &context,
        derived_session_dir,
        cursor.published_chunks,
        full_chunks,
        &cursor.tail,
        &cursor.video_template,
    )?;
    cursor.published_chunks = full_chunks;
    let published_frames = u64::from(full_chunks) * header.native_format.chunk_frames as u64;
    trim_tail(&mut cursor.tail, published_frames)?;
    validate_tail(&cursor)?;
    Ok(cursor)
}

fn recover_incremental(
    session_dir: &Path,
    derived_session_dir: &Path,
    refs: &[ReadySegmentRef],
    header: ValidatedSessionHeader,
    wall_clock: Option<WallClockAnchor>,
) -> anyhow::Result<IncrementalCursor> {
    ensure!(
        !refs.is_empty(),
        "incremental conversion has no ready segment refs"
    );
    let (segments, cursor_segments, projection) =
        validate_refs_from_checkpoint(session_dir, &header, refs, 0, None)?;
    let session = validated_session_from_header(&header, segments, wall_clock);
    validate_chunk_boundaries(&session)?;
    ensure!(
        reconcile_exact_incremental_chunks(&session, derived_session_dir)?.is_some(),
        "incremental chunks without a usable cursor do not exactly match current RAW"
    );
    let existing_chunks = validate_existing_full_chunks(&session, derived_session_dir)
        .context("recovered incremental chunks are still invalid")?;
    let ready_frames = cursor_segments
        .last()
        .and_then(|segment| segment.frame_start.checked_add(segment.frame_count))
        .context("ready refs produced no frames")?;
    let full_chunks = u32::try_from(ready_frames / header.native_format.chunk_frames as u64)?;
    ensure!(
        existing_chunks <= full_chunks,
        "incremental output contains more full chunks than referenced RAW frames"
    );
    let existing_frames = u64::from(existing_chunks) * header.native_format.chunk_frames as u64;
    let mut tail = session
        .segments
        .iter()
        .zip(&cursor_segments)
        .map(|(segment, cursor_segment)| {
            TailSegment::from_validated(segment, cursor_segment.frame_start)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    trim_tail(&mut tail, existing_frames)?;
    let video_template = session
        .segments
        .first()
        .context("ready refs produced no video template")?
        .video
        .clone();
    publish_full_chunks_from_tail(
        session_dir,
        &session,
        derived_session_dir,
        existing_chunks,
        full_chunks,
        &tail,
        &video_template,
    )?;
    let published_frames = u64::from(full_chunks) * header.native_format.chunk_frames as u64;
    trim_tail(&mut tail, published_frames)?;
    let cursor = IncrementalCursor {
        schema_version: CURSOR_SCHEMA_VERSION.into(),
        session_id: header.session.session_id.clone(),
        mapping_content_hash: header.mapping.content_hash().into(),
        catalog: data_model_catalog(header.session.catalog.as_ref()),
        wall_clock,
        native_format: CursorNativeFormat::from_native(header.native_format)?,
        segments: cursor_segments,
        ready_frames,
        published_chunks: full_chunks,
        published_chunk_hashes: Vec::new(),
        projection,
        video_template,
        tail,
    };
    validate_tail(&cursor)?;
    Ok(cursor)
}

/// Reconcile existing owned chunks with fresh RAW-derived canonical bytes.
///
/// Returns `None` when any non-legacy byte differs. `Some(false)` means the
/// chunks were already canonical; `Some(true)` means only whole-file legacy
/// catalog/UTC omissions were atomically upgraded.
fn reconcile_exact_incremental_chunks(
    session: &ValidatedSession,
    session_dir: &Path,
) -> anyhow::Result<Option<bool>> {
    let chunks = existing_chunk_directories(session_dir)?;
    if chunks.is_empty() {
        return Ok(Some(false));
    }

    // A missing or damaged cursor cannot itself prove that the chunks are from
    // the old schema. Re-materialize the current RAW into an isolated sibling
    // and require every existing byte/field to match it after normalizing only
    // the dm-1 -> dm-2 schema/catalog delta and the wall-clock field that old
    // incremental outputs did not carry. This prevents
    // recovery from silently discarding unrelated corruption or tampering.
    let parent = session_dir
        .parent()
        .context("incremental DataModel session directory has no parent")?;
    let session_name = session_dir
        .file_name()
        .and_then(|value| value.to_str())
        .context("incremental DataModel session directory has no UTF-8 name")?;
    let staging_prefix = format!(".{session_name}.legacy-incremental-validation-");
    cleanup_owned_staging_prefix(parent, &staging_prefix)?;
    let staging = parent.join(format!("{staging_prefix}{}", unique_suffix()));
    ensure!(
        !staging.exists(),
        "legacy incremental validation staging already exists"
    );
    fs::create_dir(&staging)?;
    let staging_guard = StagingGuard::new(staging.clone());
    materialize_dataset(session, &staging)?;
    let canonical_session_dir = dataset_session_dir(session, &staging);
    let canonical_chunks = existing_chunk_directories(&canonical_session_dir)?;
    if chunks.len() > canonical_chunks.len() {
        return Ok(None);
    }

    let mut replacements = Vec::<(PathBuf, PathBuf)>::new();
    for (chunk, canonical_chunk) in chunks.iter().zip(&canonical_chunks) {
        let mut meta: serde_json::Value = match read_json(&chunk.join("meta.json")) {
            Ok(meta) => meta,
            Err(_) => return Ok(None),
        };
        let canonical_meta: serde_json::Value = read_json(&canonical_chunk.join("meta.json"))?;
        let Some(meta_was_legacy) = normalize_legacy_data_model_member(&mut meta, &canonical_meta)
        else {
            return Ok(None);
        };
        if meta != canonical_meta {
            return Ok(None);
        }
        if meta_was_legacy {
            replacements.push((canonical_chunk.join("meta.json"), chunk.join("meta.json")));
        } else if !files_equal(&chunk.join("meta.json"), &canonical_chunk.join("meta.json"))? {
            return Ok(None);
        }

        let mut rows: Vec<serde_json::Value> = match read_jsonl(&chunk.join("p0.jsonl")) {
            Ok(rows) => rows,
            Err(_) => return Ok(None),
        };
        let canonical_rows: Vec<serde_json::Value> = read_jsonl(&canonical_chunk.join("p0.jsonl"))?;
        if rows.len() != canonical_rows.len() {
            return Ok(None);
        }
        let missing_utc_rows = rows
            .iter()
            .zip(&canonical_rows)
            .filter(|(row, canonical)| {
                row.as_object()
                    .is_some_and(|object| !object.contains_key("t_utc_est_us"))
                    && canonical
                        .as_object()
                        .is_some_and(|object| object.contains_key("t_utc_est_us"))
            })
            .count();
        if missing_utc_rows != 0 && missing_utc_rows != rows.len() {
            return Ok(None);
        }
        let utc_was_missing = missing_utc_rows == rows.len();
        if utc_was_missing && !meta_was_legacy {
            return Ok(None);
        }
        for (row, canonical_row) in rows.iter_mut().zip(&canonical_rows) {
            let (Some(row_object), Some(canonical_row_object)) =
                (row.as_object_mut(), canonical_row.as_object())
            else {
                return Ok(None);
            };
            if !row_object.contains_key("t_utc_est_us")
                && canonical_row_object.contains_key("t_utc_est_us")
            {
                row_object.insert(
                    "t_utc_est_us".into(),
                    canonical_row_object["t_utc_est_us"].clone(),
                );
            }
            if row != canonical_row {
                return Ok(None);
            }
        }
        if utc_was_missing {
            replacements.push((canonical_chunk.join("p0.jsonl"), chunk.join("p0.jsonl")));
        } else if !files_equal(&chunk.join("p0.jsonl"), &canonical_chunk.join("p0.jsonl"))? {
            return Ok(None);
        }

        let members = dataset_file_paths(chunk)?;
        if members != dataset_file_paths(canonical_chunk)? {
            return Ok(None);
        }
        for relative in members {
            if relative == Path::new("meta.json") || relative == Path::new("p0.jsonl") {
                continue;
            }
            if !files_equal(&chunk.join(&relative), &canonical_chunk.join(&relative))? {
                return Ok(None);
            }
        }
    }
    // Patch only the old JSON members after every chunk has been proven to be
    // canonical modulo the omitted fields. A crash between replacements leaves
    // all media and directories intact; the next recovery accepts the already
    // canonical members and resumes the remaining atomic replacements.
    let changed = !replacements.is_empty();
    for (source, destination) in replacements {
        atomic_replace_file(&source, &destination)?;
    }
    drop(staging_guard);
    Ok(Some(changed))
}

fn chunk_member_hashes(session_dir: &Path) -> anyhow::Result<Vec<BTreeMap<String, String>>> {
    existing_chunk_directories(session_dir)?
        .into_iter()
        .map(|chunk| {
            let mut hashes = BTreeMap::new();
            for entry in fs::read_dir(&chunk)? {
                let entry = entry?;
                let path = entry.path();
                ensure!(path.is_file(), "published chunk contains a non-file member");
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("published chunk member has a non-UTF-8 name"))?;
                hashes.insert(name, sha256_file(&path)?);
            }
            Ok(hashes)
        })
        .collect()
}

fn validated_session_from_header(
    header: &ValidatedSessionHeader,
    segments: Vec<ValidatedSegment>,
    wall_clock: Option<WallClockAnchor>,
) -> ValidatedSession {
    ValidatedSession {
        session: header.session.clone(),
        mapping: header.mapping.clone(),
        native_format: header.native_format,
        audio_enabled: header.profile.audio.is_some(),
        segments,
        compliance: ComplianceProfile::LocalDebugNonTrainable,
        wall_clock,
        view_scale: header.view_scale,
    }
}

impl TailSegment {
    fn from_validated(segment: &ValidatedSegment, global_frame_start: u64) -> anyhow::Result<Self> {
        Ok(Self {
            run_id: segment.run_id.clone(),
            segment_index: segment.segment_index,
            global_frame_start,
            sample_start: 0,
            rows: segment.rows.clone(),
            video: segment.video.clone(),
            video_sha256: segment
                .source_hashes
                .get("video.mp4")
                .context("validated segment has no video hash")?
                .clone(),
            audio: segment.audio.clone(),
            audio_sha256: segment.source_hashes.get(AUDIO_ARTIFACT_NAME).cloned(),
        })
    }
}

fn trim_tail(tail: &mut Vec<TailSegment>, frame_start: u64) -> anyhow::Result<()> {
    while tail.first().is_some_and(|segment| {
        segment.global_frame_start + segment.rows.len() as u64 <= frame_start
    }) {
        tail.remove(0);
    }
    if let Some(first) = tail.first_mut()
        && first.global_frame_start < frame_start
    {
        let trim = usize::try_from(frame_start - first.global_frame_start)?;
        ensure!(
            trim < first.rows.len(),
            "tail trim consumed an entire segment"
        );
        first.rows.drain(..trim);
        first.sample_start = first
            .sample_start
            .checked_add(u32::try_from(trim)?)
            .context("tail sample start overflow")?;
        first.global_frame_start = frame_start;
    }
    Ok(())
}

fn validate_chunk_boundaries_from(
    start_gframe: u64,
    segments: &[ValidatedSegment],
    native_format: NativeFormat,
    video_template: &VideoDescriptor,
) -> anyhow::Result<()> {
    let mut gframe = start_gframe;
    for segment in segments {
        ensure!(
            same_codec_config(video_template, &segment.video),
            "source MP4 codec configuration changes at {}/{}",
            segment.run_id,
            segment.segment_index
        );
        let sync = segment
            .video
            .sync_samples
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for local in 0..segment.video.sample_count {
            if gframe.is_multiple_of(native_format.chunk_frames as u64) {
                ensure!(
                    sync.contains(&local),
                    "data_demo native GOP boundary gframe {gframe} is not an H.264 sync sample (source {}/{}, local frame {local}); refusing lossy or fake conversion",
                    segment.run_id,
                    segment.segment_index
                );
            }
            gframe = gframe
                .checked_add(1)
                .context("incremental gframe overflow")?;
        }
    }
    Ok(())
}

fn cleanup_cursor_staging(output: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(output)? {
        let path = entry?.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with(CURSOR_STAGING_PREFIX))
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn write_cursor(path: &Path, cursor: &IncrementalCursor) -> anyhow::Result<()> {
    let parent = path.parent().context("incremental cursor has no parent")?;
    let staging = parent.join(format!("{CURSOR_STAGING_PREFIX}{}.json", unique_suffix()));
    let file = File::create(&staging)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, cursor)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&staging, path)?;
    Ok(())
}

fn remove_cursor_artifacts(output: &Path) -> anyhow::Result<()> {
    let cursor = output.join(CURSOR_FILE_NAME);
    if cursor.exists() {
        fs::remove_file(cursor)?;
    }
    cleanup_cursor_staging(output)
}

fn cleanup_owned_staging_directories(session_dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(session_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if path.is_dir() && name.starts_with(".c") && name.contains(".staging-") {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to clean stale staging {}", path.display()))?;
        }
    }
    Ok(())
}

fn cleanup_finalize_staging(parent: &Path, output_name: &str) -> anyhow::Result<()> {
    let prefix = format!(".{output_name}.finalize-staging-");
    for entry in fs::read_dir(parent)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if path.is_dir() && name.starts_with(&prefix) {
            fs::remove_dir_all(&path).with_context(|| {
                format!("failed to clean stale finalize staging {}", path.display())
            })?;
        }
    }
    Ok(())
}

fn validate_existing_full_chunks(
    session: &ValidatedSession,
    session_dir: &Path,
) -> anyhow::Result<u32> {
    let chunks = existing_chunk_directories(session_dir)?;
    for (index, chunk) in chunks.iter().enumerate() {
        let chunk_index = u32::try_from(index)?;
        validate_existing_full_chunk(session, chunk, chunk_index)?;
    }
    u32::try_from(chunks.len()).context("incremental chunk count overflow")
}

fn existing_chunk_directories(session_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut chunks = fs::read_dir(session_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    chunks.sort();
    for (index, chunk) in chunks.iter().enumerate() {
        ensure!(
            chunk.file_name().and_then(|value| value.to_str())
                == Some(format!("c{index:04}").as_str()),
            "incremental chunk directories are not contiguous"
        );
    }
    Ok(chunks)
}

fn validate_existing_full_chunk(
    session: &ValidatedSession,
    chunk_dir: &Path,
    chunk_index: u32,
) -> anyhow::Result<()> {
    let meta_value: serde_json::Value = read_json(&chunk_dir.join("meta.json"))?;
    validate_data_model_schema_member(&meta_value, "published incremental chunk metadata")?;
    let meta: ChunkMeta = serde_json::from_value(meta_value)
        .context("failed to parse published incremental chunk metadata")?;
    let rows: Vec<DataDemoFrame> = read_jsonl(&chunk_dir.join("p0.jsonl"))?;
    ensure!(
        rows.len() == session.native_format.chunk_frames,
        "published incremental chunk {chunk_index} is not complete"
    );
    ensure!(
        meta.schema_version == DATA_MODEL_SCHEMA_VERSION
            && meta.session == session.session.session_id
            && meta.chunk == chunk_index
            && meta.video.frames == rows.len() as u64,
        "published incremental chunk {chunk_index} metadata mismatch"
    );
    ensure!(
        meta.non_trainable
            && !meta.is_trainable
            && !meta.production_certified
            && meta.attestation.is_none(),
        "published incremental chunk {chunk_index} widened compliance"
    );
    ensure!(
        meta.provenance.raw_session_id == session.session.session_id
            && meta.provenance.mapping_id == session.mapping.mapping().mapping_id
            && meta.provenance.mapping_version == session.mapping.mapping().version
            && meta.provenance.mapping_content_hash == session.mapping.content_hash(),
        "published incremental chunk {chunk_index} provenance mismatch"
    );
    ensure!(
        meta.catalog == data_model_catalog(session.session.catalog.as_ref()),
        "published incremental chunk {chunk_index} catalog mismatch"
    );
    for (local, row) in rows.iter().enumerate() {
        let expected_gframe =
            u64::from(chunk_index) * session.native_format.chunk_frames as u64 + local as u64;
        ensure!(
            row.frame == local as u64 && row.gframe == expected_gframe,
            "published incremental chunk {chunk_index} frame sequence mismatch"
        );
        ensure!(
            row.video_t_ms == session.native_format.video_time_ms(expected_gframe),
            "published incremental chunk {chunk_index} video timeline mismatch"
        );
        let expected_wall_clock = session
            .wall_clock
            .as_ref()
            .map(|anchor| anchor.estimate_us(row.qpc))
            .transpose()?;
        ensure!(
            row.t_utc_est_us == expected_wall_clock,
            "published incremental chunk {chunk_index} wall-clock projection mismatch"
        );
        ensure!(
            is_sorted_unique(&row.actions)
                && row
                    .actions
                    .iter()
                    .all(|action| valid_action_semantic(action)),
            "published incremental chunk {chunk_index} has invalid semantic actions"
        );
    }
    let video = inspect_video(
        &chunk_dir.join("p0.mp4"),
        Some(rows.len() as u64),
        Some(session.native_format),
    )?;
    ensure!(
        video.sync_samples.first() == Some(&0),
        "published incremental chunk {chunk_index} is not independently decodable"
    );
    validate_chunk_audio_files(chunk_dir, &meta, session.audio_enabled)?;
    Ok(())
}

fn publish_full_chunks_from_tail(
    raw_session_dir: &Path,
    session: &ValidatedSession,
    output_session_dir: &Path,
    existing_chunks: u32,
    full_chunks: u32,
    tail: &[TailSegment],
    video_template: &VideoDescriptor,
) -> anyhow::Result<()> {
    if existing_chunks == full_chunks {
        return Ok(());
    }
    let existing_frames = u64::from(existing_chunks) * session.native_format.chunk_frames as u64;
    let full_frames = u64::from(full_chunks) * session.native_format.chunk_frames as u64;
    ensure!(
        tail.first().map(|segment| segment.global_frame_start) == Some(existing_frames),
        "incremental tail does not begin at the first unpublished frame"
    );
    let mut next_chunk = existing_chunks;
    let mut open_chunk = None::<(OpenChunk, StagingGuard, PathBuf)>;

    for segment in tail {
        if segment.global_frame_start >= full_frames {
            break;
        }
        let segment_dir = raw_session_dir
            .join("runs")
            .join(&segment.run_id)
            .join("segments")
            .join(format!("{:05}", segment.segment_index));
        let video_path = segment_dir.join("video.mp4");
        ensure!(
            sha256_file(&video_path)? == segment.video_sha256,
            "source video changed after ready validation"
        );
        let file = File::open(&video_path)?;
        let size = file.metadata()?.len();
        let mut reader = Mp4Reader::read_header(BufReader::new(file), size)?;
        let track_id = reader.tracks().values().next().unwrap().track_id();
        let segment_audio = segment
            .audio
            .as_ref()
            .map(|audio| {
                SegmentAudioData::load_from(&segment_dir, audio, segment.audio_sha256.as_deref())
            })
            .transpose()?;
        for (local, raw_row) in segment.rows.iter().enumerate() {
            let gframe = segment
                .global_frame_start
                .checked_add(u64::try_from(local)?)
                .context("incremental gframe overflow")?;
            if gframe >= full_frames {
                break;
            }
            let sample_id = segment
                .sample_start
                .checked_add(u32::try_from(local)?)
                .and_then(|value| value.checked_add(1))
                .context("source MP4 sample index overflow")?;
            let sample = reader
                .read_sample(track_id, sample_id)?
                .context("source MP4 sample disappeared after validation")?;
            if open_chunk.is_none() {
                ensure!(
                    sample.is_sync,
                    "output chunk would not start with a sync sample"
                );
                let final_path = output_session_dir.join(format!("c{next_chunk:04}"));
                ensure!(
                    !final_path.exists(),
                    "refusing to overwrite incremental chunk {}",
                    final_path.display()
                );
                let staging_path = output_session_dir
                    .join(format!(".c{next_chunk:04}.staging-{}", unique_suffix()));
                let chunk = OpenChunk::create_at(
                    staging_path.clone(),
                    next_chunk,
                    video_template,
                    session.native_format.chunk_frames,
                    session.audio_enabled,
                )?;
                open_chunk = Some((chunk, StagingGuard::new(staging_path), final_path));
            }
            let row = DataDemoFrame::from_raw(
                raw_row,
                gframe,
                session.native_format,
                session.wall_clock.as_ref(),
                session.view_scale.as_ref(),
            )?;
            let source = format!("{}/{:05}", segment.run_id, segment.segment_index);
            let (chunk, _, _) = open_chunk.as_mut().unwrap();
            let audio = segment_audio
                .as_ref()
                .map(|audio| audio.slice(raw_row))
                .transpose()?;
            chunk.write_sample(sample, row, source, audio)?;
            if chunk.rows.len() == session.native_format.chunk_frames {
                let (chunk, mut guard, final_path) = open_chunk.take().unwrap();
                chunk.finish(session)?;
                fs::rename(&guard.path, &final_path).with_context(|| {
                    format!(
                        "failed to publish incremental chunk {} -> {}",
                        guard.path.display(),
                        final_path.display()
                    )
                })?;
                guard.disarm();
                next_chunk = next_chunk
                    .checked_add(1)
                    .context("incremental chunk index overflow")?;
            }
        }
    }
    ensure!(
        open_chunk.is_none(),
        "incremental full chunk remained incomplete"
    );
    ensure!(
        next_chunk == full_chunks,
        "incremental chunk publish count mismatch"
    );
    Ok(())
}

fn merge_final_dataset(
    session: &ValidatedSession,
    staging: &Path,
    output: &Path,
) -> anyhow::Result<()> {
    let staged_session = dataset_session_dir(session, staging);
    let output_session = dataset_session_dir(session, output);
    fs::create_dir_all(&output_session)?;
    cleanup_owned_staging_directories(&output_session)?;

    let existing_chunks = existing_chunk_directories(&output_session)?;
    let staged_chunks = dataset_chunk_directories(&staged_session)?;
    ensure!(
        existing_chunks.len() <= staged_chunks.len(),
        "incremental output has more chunks than final RAW session"
    );
    for (index, existing_chunk) in existing_chunks.iter().enumerate() {
        compare_chunk_directories(existing_chunk, &staged_chunks[index])?;
    }
    for (index, staged_chunk) in staged_chunks.iter().enumerate().skip(existing_chunks.len()) {
        fs::rename(staged_chunk, output_session.join(format!("c{index:04}")))?;
    }

    publish_file_idempotent(
        &staged_session.join("framepairs.jsonl"),
        &output_session.join("framepairs.jsonl"),
    )?;
    publish_file_idempotent(
        &staged_session.join("metadata.json"),
        &output_session.join("metadata.json"),
    )?;
    publish_file_idempotent(
        &staging.join("_dataset_stats.json"),
        &output.join("_dataset_stats.json"),
    )?;
    publish_file_idempotent(&staging.join("DATASHEET.md"), &output.join("DATASHEET.md"))?;
    remove_cursor_artifacts(output)?;
    // metadata.jsonl is the final completeness marker for an in-place work dataset.
    publish_file_idempotent(
        &staging.join("metadata.jsonl"),
        &output.join("metadata.jsonl"),
    )?;
    Ok(())
}

pub(super) fn verify_dataset_binding(
    dataset: &Path,
    session: &ValidatedSession,
) -> anyhow::Result<VerificationReport> {
    let report = verify_dataset(dataset)?;
    let rows: Vec<SessionMetadata> = read_jsonl(&dataset.join("metadata.jsonl"))?;
    let metadata = rows
        .into_iter()
        .next()
        .context("verified dataset has no metadata row")?;
    let frame_count = session_frame_count(session);
    let chunk_count =
        u32::try_from(frame_count.div_ceil(session.native_format.chunk_frames as u64))?;
    let expected_sources = session
        .segments
        .iter()
        .map(|segment| SourceSegment {
            run_id: segment.run_id.clone(),
            segment_index: segment.segment_index,
            hashes: segment.source_hashes.clone(),
        })
        .collect::<Vec<_>>();
    let mapping = session.mapping.mapping();
    ensure!(
        report.session_id == session.session.session_id
            && report.frame_count == frame_count
            && report.chunk_count == chunk_count,
        "finalized DataModel output frame identity does not match current RAW"
    );
    ensure!(
        metadata.workflow_id == session.session.session_id
            && metadata.game == mapping.game.name
            && metadata.category == mapping.taxonomy.content
            && metadata.platform == mapping.game.platform,
        "finalized DataModel output workflow identity does not match current RAW"
    );
    ensure!(
        metadata.capture.width == session.native_format.width
            && metadata.capture.height == session.native_format.height
            && metadata.capture.fps_num == session.native_format.fps_num
            && metadata.capture.fps_den == session.native_format.fps_den
            && metadata.capture.frame_count == frame_count
            && metadata.capture.chunks == chunk_count
            && metadata.capture.chunk_frames == u32::try_from(session.native_format.chunk_frames)?,
        "finalized DataModel output capture identity does not match current RAW"
    );
    ensure!(
        metadata.catalog == data_model_catalog(session.session.catalog.as_ref()),
        "finalized DataModel output catalog does not match current RAW"
    );
    ensure!(
        metadata.provenance.raw_schema_version == session.session.schema_version
            && metadata.provenance.raw_session_id == session.session.session_id
            && metadata.provenance.mapping_id == mapping.mapping_id
            && metadata.provenance.mapping_version == mapping.version
            && metadata.provenance.mapping_content_hash == session.mapping.content_hash()
            && metadata.provenance.source_segments == expected_sources
            && metadata.provenance.run_frame_ranges == build_run_frame_ranges(session),
        "finalized DataModel output provenance does not match current RAW"
    );
    // docs/46 §6 的诚实链闭环：数据集声明的标定常数必须与本会话 hash 复核过的
    // settings 快照派生值一致——verify_dataset 只能证明数据集自洽，跨到 RAW
    // 的"常数没被换过"由这里作证。
    ensure!(
        metadata.view_scale
            == session
                .view_scale
                .as_ref()
                .map(ViewScaleRef::from_constants),
        "finalized DataModel output view_scale declaration does not match current RAW settings snapshot"
    );
    let dataset_session = dataset_session_dir(session, dataset);
    let mut row_index = 0_usize;
    let mut raw_rows = session
        .segments
        .iter()
        .flat_map(|segment| segment.rows.iter());
    for chunk in dataset_chunk_directories(&dataset_session)? {
        let rows: Vec<DataDemoFrame> = read_jsonl(&chunk.join("p0.jsonl"))?;
        for row in rows {
            let raw = raw_rows
                .next()
                .context("finalized DataModel output has more rows than current RAW")?;
            let expected_wall_clock = session
                .wall_clock
                .as_ref()
                .map(|anchor| anchor.estimate_us(raw.output_qpc_start))
                .transpose()?;
            ensure!(
                row.qpc == raw.output_qpc_start && row.t_utc_est_us == expected_wall_clock,
                "finalized DataModel output wall-clock projection does not match current RAW"
            );
            row_index += 1;
        }
    }
    ensure!(
        row_index == usize::try_from(frame_count)? && raw_rows.next().is_none(),
        "finalized DataModel output wall-clock row count does not match current RAW"
    );
    Ok(report)
}

fn compare_chunk_directories(left: &Path, right: &Path) -> anyhow::Result<()> {
    let names = |directory: &Path| -> anyhow::Result<BTreeSet<String>> {
        Ok(fs::read_dir(directory)?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect())
    };
    let left_names = names(left)?;
    ensure!(
        left_names == names(right)?,
        "published incremental chunk member set differs from final replay"
    );
    for name in left_names {
        ensure!(
            sha256_file(&left.join(&name))? == sha256_file(&right.join(&name))?,
            "published incremental chunk differs from final replay: {name}"
        );
    }
    Ok(())
}

fn publish_file_idempotent(source: &Path, destination: &Path) -> anyhow::Result<()> {
    if destination.exists() {
        ensure!(
            files_equal(source, destination)?,
            "final incremental file differs from replay: {}",
            destination.display()
        );
    } else {
        fs::rename(source, destination).with_context(|| {
            format!(
                "failed to publish final incremental file {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> anyhow::Result<bool> {
    if fs::metadata(left)?.len() != fs::metadata(right)?.len() {
        return Ok(false);
    }
    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let mut left_buffer = [0_u8; 16 * 1024];
    let mut right_buffer = [0_u8; 16 * 1024];
    loop {
        let left_read = left_file.read(&mut left_buffer)?;
        let right_read = right_file.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        ^ u128::from(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_refs_allow_prior_zero_frame_runs_without_omitting_frames() {
        let refs = normalized_refs(&[ReadySegmentRef {
            run_id: "r0001".into(),
            segment_index: 0,
        }])
        .unwrap();
        assert_eq!(refs[0].run_id, "r0001");
    }
}
