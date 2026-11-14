use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use capture_core::audio_contract::{AUDIO_ARTIFACT_NAME, SegmentAudioMetadata};
use capture_core::ingest_contract::SessionContract;
use capture_core::profile::CaptureProfile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::platform::windows::catalog::RecordingProfile;
use crate::services::capture_storage::SESSION_ROOT_NAME;
use crate::services::heartbeats::{HeartbeatReconcileState, HeartbeatStore};
use crate::services::journal::{
    RecoveryCommit, RecoveryFinding, RecoveryInventory, RecoveryRun, RecoverySegment,
    RecoverySession, SessionJournal,
};
use crate::services::markers::{MarkerReconcileState, MarkerStore};

const RECOVERY_SCHEMA_VERSION: &str = "dm-local-recovery-1";
static RECOVERY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryReport {
    pub schema_version: &'static str,
    pub recovery_id: String,
    pub state: &'static str,
    pub non_trainable: bool,
    pub production_certified: bool,
    pub created_unix_ms: u64,
    pub scanned_sessions: u32,
    pub verified_ready_segments: u32,
    pub quarantined_items: u32,
    pub corrupt_segments: u32,
    pub orphan_items: u32,
    /// Bytes actually re-hashed this startup, and how long it took. Startup blocks
    /// on recomputing SHA-256 for every journal `ready` artifact, so this cost grows
    /// with the whole local archive rather than with what changed. The policy is
    /// unchanged — these two fields exist so the growth is measurable before it is
    /// decided whether verification should become incremental or move off the
    /// startup path. See TODO.md「启动恢复校验成本」.
    pub verified_bytes: u64,
    pub verification_ms: u64,
    pub report_path: String,
    pub findings: Vec<RecoveryFinding>,
}

impl RecoveryReport {
    pub fn diagnostic_severity(&self) -> &'static str {
        if self
            .findings
            .iter()
            .any(|finding| finding.severity == "error")
        {
            "error"
        } else if self
            .findings
            .iter()
            .any(|finding| finding.severity == "warning")
        {
            "warning"
        } else {
            "info"
        }
    }
}

#[derive(Clone, Debug)]
struct VerificationFailure {
    code: &'static str,
    message: &'static str,
}

#[derive(Debug, Deserialize)]
struct RecoveryMeta {
    schema_version: String,
    session_id: String,
    run_id: String,
    segment_index: u32,
    session_frame_start: u64,
    n_frames: u64,
    state: String,
    /// legacy 一代的两个训练声明；中性一代整体缺席（issue #855 §B）。
    #[serde(default)]
    non_trainable: Option<bool>,
    #[serde(default)]
    production_certified: Option<bool>,
    qpc_start: i64,
    qpc_end: i64,
    fps_num: u32,
    fps_den: u32,
    width: u32,
    height: u32,
    video: RecoveryArtifactFile,
    frames: RecoveryArtifactRows,
    events: RecoveryArtifactRows,
    #[serde(default)]
    audio: Option<SegmentAudioMetadata>,
    mapping: RecoveryMapping,
    #[serde(default)]
    catalog: Option<RecoveryCatalog>,
    health: RecoveryHealth,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCatalog {
    /// legacy 目录身份的 `mode`；中性一代不再自称车道，字段缺席（issue #855 §A.4）。
    /// 存量段里它仍然是 `dev_non_trainable`，那些字节一个都不许变——所以这里必须
    /// 是 `Option` 而不是删掉：删了，`deny_unknown_fields` 会把每一份存量 manifest
    /// 判成损坏。
    #[serde(default)]
    mode: Option<String>,
    // 渠道派生的分类（docs/57 §3.3）。与 `provenance` 同理只为让
    // `deny_unknown_fields` 接受带它的 manifest，不参与下面的 manifest↔journal 对账
    // ——那条对账比的是身份，分类由渠道决定、journal 里没有对应可比的列。
    #[allow(dead_code)]
    non_trainable: Option<bool>,
    // Recorded by the client since docs/54 §6.2, but deliberately NOT part of the
    // manifest↔journal cross-check below: provenance is how the catalog reached
    // the client, not an identity the journal can corroborate. It is declared
    // here only so `deny_unknown_fields` keeps accepting manifests that carry it.
    //
    // `Option` is load-bearing: every session sealed before this field existed
    // has no `provenance` key, and recovery's whole job is reading old artifacts.
    // Making it required would mark every pre-existing ready segment corrupt.
    #[allow(dead_code)]
    provenance: Option<String>,
    catalog_id: String,
    catalog_version: String,
    catalog_content_hash: String,
    device: RecoveryCatalogEntry,
    /// capture + settings 两本册子并成一本（docs/57 §3.1）。这是 session.json 里
    /// catalog 段的读侧镜像，字段名必须与写侧（`RecordingCatalogContext`）一致——
    /// 这个 struct 是 `deny_unknown_fields` 的，名字对不上就是整份 manifest 读不了，
    /// 报出来是 `RECOVERY_SESSION_MANIFEST_INVALID`，看不出跟改名有关。
    recording: RecoveryCatalogEntry,
    game: RecoveryCatalogEntry,
    mapping: RecoveryCatalogEntry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCatalogEntry {
    id: String,
    version: String,
    content_hash: String,
}

#[derive(Debug, Deserialize)]
struct RecoveryArtifactFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct RecoveryArtifactRows {
    path: String,
    lines: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct RecoveryMapping {
    id: String,
    version: String,
    content_hash: String,
}

#[derive(Debug, Deserialize)]
struct RecoveryHealth {
    duplicate_frames: u64,
    source_superseded: u64,
    queue_drops: u64,
    input_overflow: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverySessionManifest {
    schema_version: String,
    session_id: String,
    /// legacy 一代的两个训练声明；中性一代整体缺席（issue #855 §B）。
    #[serde(default)]
    non_trainable: Option<bool>,
    #[serde(default)]
    production_certified: Option<bool>,
    /// 引擎 `ensure_session_contract` 在 `consent_id` 在场时写出。今天没有来源会
    /// 填它（docs/100 §3 推论 3），也没有人读它——但 `deny_unknown_fields` 下这一项
    /// 必须声明：删了它，一份带 consent 的 manifest 会被整份判成损坏。
    /// 换言之它不是死码，是**解析承重**，所以显式 allow 而不是删。
    #[allow(dead_code)]
    #[serde(default)]
    consent_id: Option<String>,
    game: RecoverySessionGame,
    mapping: RecoverySessionMapping,
    catalog: RecoveryCatalog,
    capture_policy: RecoveryCapturePolicy,
    contract: RecoveryProjectionContract,
    todo: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverySessionGame {
    id: String,
    /// 可空：受保护进程拒绝观察时 session.json 如实写 null（docs/55 §3.1）。
    /// 恢复比对仍是等值比较——`Option` 的相等语义正好表达「当时没测到，现在
    /// 也必须是没测到」。
    exe_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverySessionMapping {
    id: String,
    version: String,
    content_hash: String,
    recording_profile_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCapturePolicy {
    source: String,
    width: u32,
    height: u32,
    scale: bool,
    #[serde(default)]
    minimum_source_width: Option<u32>,
    #[serde(default)]
    minimum_source_height: Option<u32>,
    fps_num: u32,
    fps_den: u32,
    avg_bitrate_bps: u32,
    max_bitrate_bps: u32,
    segment_seconds: u32,
    gop_frames: u32,
    audio: serde_json::Value,
    #[serde(default)]
    segment_policy_override: Option<RecoverySegmentPolicyOverride>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverySegmentPolicyOverride {
    reason: String,
    catalog_segment_seconds: u32,
    catalog_test_segments: u32,
    effective_segment_seconds: u32,
    effective_test_segments: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryProjectionContract {
    version: String,
    pointer_clamp: u32,
    pointer_mu: u32,
    pointer_ref: u32,
    pointer_max_bin: u32,
}

#[cfg(test)]
pub fn recover(
    journal: &SessionJournal,
    app_data_root: &Path,
    output_root: &Path,
) -> anyhow::Result<RecoveryReport> {
    recover_with_roots(journal, app_data_root, app_data_root, output_root)
}

pub fn recover_with_roots(
    journal: &SessionJournal,
    control_root: &Path,
    artifact_root: &Path,
    output_root: &Path,
) -> anyhow::Result<RecoveryReport> {
    fs::create_dir_all(output_root)
        .with_context(|| format!("failed to create session root {}", output_root.display()))?;
    let quarantine_root = artifact_root.join("quarantine");
    let recovery_root = control_root.join("recovery");
    fs::create_dir_all(&quarantine_root).with_context(|| {
        format!(
            "failed to create recovery quarantine {}",
            quarantine_root.display()
        )
    })?;
    fs::create_dir_all(&recovery_root).with_context(|| {
        format!(
            "failed to create recovery report directory {}",
            recovery_root.display()
        )
    })?;

    let created_unix_ms = unix_ms()?;
    let recovery_id = format!(
        "recovery-{created_unix_ms}-{:04}",
        RECOVERY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );

    let mut findings = Vec::new();
    let mut quarantined_items = 0_u32;
    quarantine_preflight_scratch(
        artifact_root,
        &quarantine_root,
        &recovery_id,
        &mut findings,
        &mut quarantined_items,
    )?;

    let inventory = journal.recovery_inventory()?;
    validate_inventory_paths(&inventory)?;
    let mut orphan_items = 0_u32;
    let session_ids = inventory
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<HashSet<_>>();

    for entry in sorted_entries(output_root)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !session_ids.contains(name.as_str()) {
            orphan_items = orphan_items.saturating_add(1);
            findings.push(RecoveryFinding {
                severity: "warning",
                code: "RECOVERY_ORPHAN_SESSION",
                action: "left_unconsumed",
                session_id: None,
                run_name: None,
                segment_seq: None,
                relative_path: Some(root_relative(artifact_root, &entry.path())?),
                message: "Filesystem session has no journal identity and was left unchanged."
                    .into(),
            });
        }
    }

    let runs_by_session = group_runs(&inventory.runs);
    let segments_by_run = group_segments(&inventory.segments);
    let heartbeats_by_session = group_heartbeats(&inventory.heartbeats);
    let markers_by_session = group_markers(&inventory.markers);
    for session in &inventory.sessions {
        let session_dir = output_root.join(&session.id);
        let known_runs = runs_by_session
            .get(session.id.as_str())
            .cloned()
            .unwrap_or_default();
        let known_run_names = known_runs
            .iter()
            .map(|run| run.run_name.as_str())
            .collect::<HashSet<_>>();
        let runs_dir = session_dir.join("runs");
        if runs_dir.is_dir() {
            for entry in sorted_entries(&runs_dir)? {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !known_run_names.contains(name.as_str()) {
                    orphan_items = orphan_items.saturating_add(1);
                    findings.push(RecoveryFinding {
                        severity: "warning",
                        code: "RECOVERY_ORPHAN_RUN",
                        action: "left_unconsumed",
                        session_id: Some(session.id.clone()),
                        run_name: None,
                        segment_seq: None,
                        relative_path: Some(root_relative(artifact_root, &entry.path())?),
                        message: "Filesystem run has no journal identity and was left unchanged."
                            .into(),
                    });
                }
            }
        }

        for run in known_runs {
            let run_dir = runs_dir.join(&run.run_name);
            let known_segments = segments_by_run
                .get(run.id.as_str())
                .cloned()
                .unwrap_or_default();
            let known_segment_seq = known_segments
                .iter()
                .map(|segment| segment.seq)
                .collect::<HashSet<_>>();
            let segments_dir = run_dir.join("segments");
            if segments_dir.is_dir() {
                for entry in sorted_entries(&segments_dir)? {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if is_segment_staging_name(&name) {
                        let destination = quarantine_root
                            .join(&session.id)
                            .join(&run.run_name)
                            .join("segments")
                            .join(&name);
                        move_to_quarantine(&entry.path(), &destination)?;
                        quarantined_items = quarantined_items.saturating_add(1);
                        findings.push(quarantine_finding(
                            artifact_root,
                            session,
                            run,
                            None,
                            &destination,
                            "RECOVERY_STAGING_SEGMENT",
                        )?);
                    } else if let Some(seq) = ready_segment_name(&name)
                        && !known_segment_seq.contains(&seq)
                    {
                        orphan_items = orphan_items.saturating_add(1);
                        findings.push(RecoveryFinding {
                            severity: "warning",
                            code: "RECOVERY_ORPHAN_READY_DIRECTORY",
                            action: "left_unconsumed",
                            session_id: Some(session.id.clone()),
                            run_name: Some(run.run_name.clone()),
                            segment_seq: Some(seq),
                            relative_path: Some(root_relative(artifact_root, &entry.path())?),
                            message: "Published directory has no segment journal row and was not accepted as ready."
                                .into(),
                        });
                    }
                }
            }

            for segment in &known_segments {
                if segment.state != "ready" && segment.state != "corrupt" {
                    let unpublished = segments_dir.join(format!("{:05}", segment.seq));
                    if unpublished.exists() {
                        let destination = quarantine_root
                            .join(&session.id)
                            .join(&run.run_name)
                            .join("segments")
                            .join(format!("{:05}", segment.seq));
                        move_to_quarantine(&unpublished, &destination)?;
                        quarantined_items = quarantined_items.saturating_add(1);
                        findings.push(quarantine_finding(
                            artifact_root,
                            session,
                            run,
                            Some(segment.seq),
                            &destination,
                            "RECOVERY_UNCOMMITTED_SEGMENT",
                        )?);
                    }
                }
            }

            let spool_dir = run_dir.join("spool");
            if spool_dir.is_dir() {
                for entry in sorted_entries(&spool_dir)? {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if is_recoverable_spool_name(&name) {
                        let destination = quarantine_root
                            .join(&session.id)
                            .join(&run.run_name)
                            .join("spool")
                            .join(&name);
                        move_to_quarantine(&entry.path(), &destination)?;
                        quarantined_items = quarantined_items.saturating_add(1);
                        findings.push(quarantine_finding(
                            artifact_root,
                            session,
                            run,
                            spool_segment_seq(&name),
                            &destination,
                            "RECOVERY_RAW_INPUT_SPOOL",
                        )?);
                    }
                }
            }
        }
    }

    let heartbeat_store = HeartbeatStore::new(output_root.to_path_buf());
    for session in &inventory.sessions {
        let heartbeats = heartbeats_by_session
            .get(session.id.as_str())
            .cloned()
            .unwrap_or_default();
        match heartbeat_store.reconcile_session(&session.id, &heartbeats)? {
            HeartbeatReconcileState::Rebuilt => findings.push(RecoveryFinding {
                severity: "info",
                code: "RECOVERY_HEARTBEAT_ARTIFACT_REBUILT",
                action: "rebuilt_from_journal",
                session_id: Some(session.id.clone()),
                run_name: None,
                segment_seq: None,
                relative_path: Some(format!(
                    "{SESSION_ROOT_NAME}/{}/heartbeat.jsonl",
                    session.id,
                )),
                message:
                    "Session heartbeat artifact was atomically rebuilt from SQLite journal facts."
                        .into(),
            }),
            HeartbeatReconcileState::Orphaned => {
                orphan_items = orphan_items.saturating_add(1);
                findings.push(RecoveryFinding {
                    severity: "warning",
                    code: "RECOVERY_ORPHAN_HEARTBEAT_ARTIFACT",
                    action: "left_unconsumed",
                    session_id: Some(session.id.clone()),
                    run_name: None,
                    segment_seq: None,
                    relative_path: Some(format!(
                        "{SESSION_ROOT_NAME}/{}/heartbeat.jsonl",
                        session.id,
                    )),
                    message: "Heartbeat artifact has no journal rows and was left unchanged."
                        .into(),
                });
            }
            HeartbeatReconcileState::Absent | HeartbeatReconcileState::Verified => {}
        }
    }

    let marker_store = MarkerStore::new(output_root.to_path_buf());
    for session in &inventory.sessions {
        let markers = markers_by_session
            .get(session.id.as_str())
            .cloned()
            .unwrap_or_default();
        match marker_store.reconcile_session(&session.id, &markers)? {
            MarkerReconcileState::Rebuilt => findings.push(RecoveryFinding {
                severity: "info",
                code: "RECOVERY_MARKER_ARTIFACT_REBUILT",
                action: "rebuilt_from_journal",
                session_id: Some(session.id.clone()),
                run_name: None,
                segment_seq: None,
                relative_path: Some(format!("{SESSION_ROOT_NAME}/{}/markers.jsonl", session.id,)),
                message:
                    "Session marker artifact was atomically rebuilt from SQLite journal facts."
                        .into(),
            }),
            MarkerReconcileState::Orphaned => {
                orphan_items = orphan_items.saturating_add(1);
                findings.push(RecoveryFinding {
                    severity: "warning",
                    code: "RECOVERY_ORPHAN_MARKER_ARTIFACT",
                    action: "left_unconsumed",
                    session_id: Some(session.id.clone()),
                    run_name: None,
                    segment_seq: None,
                    relative_path: Some(format!(
                        "{SESSION_ROOT_NAME}/{}/markers.jsonl",
                        session.id,
                    )),
                    message: "Marker artifact has no journal rows and was left unchanged.".into(),
                });
            }
            MarkerReconcileState::Absent | MarkerReconcileState::Verified => {}
        }
    }

    let unfinished_run_ids = inventory
        .runs
        .iter()
        .filter(|run| matches!(run.state.as_str(), "starting" | "recording"))
        .map(|run| run.id.clone())
        .collect::<Vec<_>>();
    for run in inventory
        .runs
        .iter()
        .filter(|run| matches!(run.state.as_str(), "starting" | "recording"))
    {
        findings.push(RecoveryFinding {
            severity: "error",
            code: "RECOVERY_UNFINISHED_RUN",
            action: "marked_faulted",
            session_id: Some(run.session_id.clone()),
            run_name: Some(run.run_name.clone()),
            segment_seq: None,
            relative_path: Some(format!(
                "{SESSION_ROOT_NAME}/{}/runs/{}",
                run.session_id, run.run_name
            )),
            message: "Run was active when the previous process ended and was closed as faulted."
                .into(),
        });
    }
    let quarantined_segment_ids = inventory
        .segments
        .iter()
        .filter(|segment| matches!(segment.state.as_str(), "active" | "finalizing"))
        .map(|segment| segment.id.clone())
        .collect::<Vec<_>>();

    let session_by_id = inventory
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    let mut catalog_failures = HashMap::new();
    let mut corrupt_session_ids = Vec::new();
    for session in &inventory.sessions {
        let session_dir = output_root.join(&session.id);
        if let Err(failure) = verify_session_catalog_snapshots(&session_dir, session) {
            catalog_failures.insert(session.id.as_str(), failure.clone());
            corrupt_session_ids.push(session.id.clone());
            findings.push(RecoveryFinding {
                severity: "error",
                code: failure.code,
                action: "marked_corrupt",
                session_id: Some(session.id.clone()),
                run_name: None,
                segment_seq: None,
                relative_path: Some(format!("{SESSION_ROOT_NAME}/{}", session.id)),
                message: failure.message.into(),
            });
        }
    }
    let run_by_id = inventory
        .runs
        .iter()
        .map(|run| (run.id.as_str(), run))
        .collect::<HashMap<_, _>>();
    let mut verified_ready_segments = 0_u32;
    let mut verified_bytes = 0_u64;
    let mut corrupt_segment_ids = Vec::new();
    let verification_started = Instant::now();
    for segment in inventory
        .segments
        .iter()
        .filter(|segment| segment.state == "ready")
    {
        let session = session_by_id
            .get(segment.session_id.as_str())
            .context("recovery segment references a missing session")?;
        let run = run_by_id
            .get(segment.run_id.as_str())
            .context("recovery segment references a missing run")?;
        let session_dir = output_root.join(&segment.session_id);
        let verification = if let Some(failure) = catalog_failures.get(segment.session_id.as_str())
        {
            Err(failure.clone())
        } else {
            verify_ready_segment(&session_dir, session, run, segment, &mut verified_bytes)
        };
        match verification {
            Ok(()) => {
                verified_ready_segments = verified_ready_segments.saturating_add(1);
            }
            Err(failure) => {
                corrupt_segment_ids.push(segment.id.clone());
                findings.push(RecoveryFinding {
                    severity: "error",
                    code: failure.code,
                    action: "marked_corrupt",
                    session_id: Some(segment.session_id.clone()),
                    run_name: Some(segment.run_name.clone()),
                    segment_seq: Some(segment.seq),
                    relative_path: Some(format!(
                        "{SESSION_ROOT_NAME}/{}/runs/{}/segments/{:05}",
                        segment.session_id, segment.run_name, segment.seq
                    )),
                    message: failure.message.into(),
                });
            }
        }
    }

    let verification_ms = u64::try_from(verification_started.elapsed().as_millis())
        .context("recovery verification duration exceeds u64")?;

    let report_path = format!("recovery/{recovery_id}.json");
    let report = RecoveryReport {
        schema_version: RECOVERY_SCHEMA_VERSION,
        recovery_id: recovery_id.clone(),
        state: "completed",
        non_trainable: true,
        production_certified: false,
        created_unix_ms,
        scanned_sessions: u32::try_from(inventory.sessions.len())
            .context("recovery session count exceeds u32")?,
        verified_ready_segments,
        quarantined_items,
        corrupt_segments: u32::try_from(corrupt_segment_ids.len())
            .context("corrupt segment count exceeds u32")?,
        orphan_items,
        verified_bytes,
        verification_ms,
        report_path: report_path.clone(),
        findings,
    };

    write_report_atomic(&recovery_root.join(format!("{recovery_id}.json")), &report)?;
    journal.commit_recovery(&RecoveryCommit {
        recovery_id: &recovery_id,
        report_path: &report_path,
        scanned_sessions: report.scanned_sessions,
        verified_ready_segments,
        quarantined_items,
        corrupt_segments: report.corrupt_segments,
        orphan_items,
        unfinished_run_ids: &unfinished_run_ids,
        quarantined_segment_ids: &quarantined_segment_ids,
        corrupt_segment_ids: &corrupt_segment_ids,
        corrupt_session_ids: &corrupt_session_ids,
        findings: &report.findings,
    })?;
    Ok(report)
}

fn quarantine_preflight_scratch(
    artifact_root: &Path,
    quarantine_root: &Path,
    recovery_id: &str,
    findings: &mut Vec<RecoveryFinding>,
    quarantined_items: &mut u32,
) -> anyhow::Result<()> {
    let scratch = artifact_root.join(".preflight-scratch");
    let metadata = match fs::symlink_metadata(&scratch) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| "failed to inspect the fixed preflight scratch directory");
        }
    };
    ensure!(
        metadata.is_dir() && !is_reparse_point(&metadata),
        "fixed preflight scratch path is not a plain directory; refusing recovery"
    );
    let destination = quarantine_root.join(recovery_id).join("preflight-scratch");
    move_to_quarantine(&scratch, &destination)?;
    *quarantined_items = quarantined_items.saturating_add(1);
    findings.push(RecoveryFinding {
        severity: "warning",
        code: "RECOVERY_PREFLIGHT_SCRATCH",
        action: "moved_to_quarantine",
        session_id: None,
        run_name: None,
        segment_seq: None,
        relative_path: Some(root_relative(artifact_root, &destination)?),
        message: "Interrupted hardware-probe scratch was preserved outside the session tree."
            .into(),
    });
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn validate_inventory_paths(inventory: &RecoveryInventory) -> anyhow::Result<()> {
    for session in &inventory.sessions {
        ensure_safe_component(&session.id, "session id")?;
        ensure!(
            matches!(
                session.state.as_str(),
                "active" | "paused" | "completed" | "faulted" | "corrupt"
            ),
            "journal session has an unsupported recovery state"
        );
    }
    for run in &inventory.runs {
        ensure_safe_component(&run.run_name, "run name")?;
    }
    for marker in &inventory.markers {
        ensure_safe_component(&marker.session_id, "marker session id")?;
        ensure_safe_component(&marker.run_id, "marker run name")?;
        ensure!(
            marker.kind == "manual" && marker.qpc >= 0 && marker.qpc_frequency > 0,
            "journal marker has an unsupported identity or timeline"
        );
    }
    for heartbeat in &inventory.heartbeats {
        ensure_safe_component(&heartbeat.session_id, "heartbeat session id")?;
        ensure_safe_component(&heartbeat.run_id, "heartbeat run name")?;
        ensure!(
            heartbeat.collector == "ok" && heartbeat.qpc >= 0 && heartbeat.qpc_frequency > 0,
            "journal heartbeat has an unsupported identity or timeline"
        );
    }
    Ok(())
}

fn ensure_safe_component(value: &str, label: &str) -> anyhow::Result<()> {
    let mut components = Path::new(value).components();
    ensure!(
        !value.is_empty()
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none()
            && !value.contains(['/', '\\']),
        "journal {label} is not a safe path component"
    );
    Ok(())
}

fn group_runs(runs: &[RecoveryRun]) -> HashMap<&str, Vec<&RecoveryRun>> {
    let mut grouped = HashMap::<&str, Vec<&RecoveryRun>>::new();
    for run in runs {
        grouped
            .entry(run.session_id.as_str())
            .or_default()
            .push(run);
    }
    grouped
}

fn group_segments(segments: &[RecoverySegment]) -> HashMap<&str, Vec<&RecoverySegment>> {
    let mut grouped = HashMap::<&str, Vec<&RecoverySegment>>::new();
    for segment in segments {
        grouped
            .entry(segment.run_id.as_str())
            .or_default()
            .push(segment);
    }
    grouped
}

fn group_markers(
    markers: &[crate::domain::collector::MarkerAudit],
) -> HashMap<&str, Vec<crate::domain::collector::MarkerAudit>> {
    let mut grouped = HashMap::<&str, Vec<_>>::new();
    for marker in markers {
        grouped
            .entry(marker.session_id.as_str())
            .or_default()
            .push(marker.clone());
    }
    grouped
}

fn group_heartbeats(
    heartbeats: &[crate::domain::collector::HeartbeatAudit],
) -> HashMap<&str, Vec<crate::domain::collector::HeartbeatAudit>> {
    let mut grouped = HashMap::<&str, Vec<_>>::new();
    for heartbeat in heartbeats {
        grouped
            .entry(heartbeat.session_id.as_str())
            .or_default()
            .push(heartbeat.clone());
    }
    grouped
}

fn sorted_entries(path: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to scan recovery path {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn is_segment_staging_name(name: &str) -> bool {
    name.len() == 10
        && name.starts_with('.')
        && name.ends_with(".tmp")
        && name[1..6].bytes().all(|byte| byte.is_ascii_digit())
}

fn ready_segment_name(name: &str) -> Option<u32> {
    (name.len() == 5 && name.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| name.parse().ok())
        .flatten()
}

fn is_recoverable_spool_name(name: &str) -> bool {
    name.ends_with(".events.jsonl.active")
        || name.ends_with(".events.jsonl.sealed")
        || name.ends_with(".audio.pcm.zst.active")
        || name.ends_with(".audio.pcm.zst.sealed")
}

fn spool_segment_seq(name: &str) -> Option<u32> {
    name.get(..5)
        .filter(|prefix| prefix.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|prefix| prefix.parse().ok())
}

fn move_to_quarantine(source: &Path, destination: &Path) -> anyhow::Result<()> {
    ensure!(
        source.exists(),
        "recovery source disappeared before quarantine"
    );
    ensure!(
        !destination.exists(),
        "recovery quarantine collision; refusing to overwrite existing evidence"
    );
    let parent = destination
        .parent()
        .context("recovery quarantine destination has no parent")?;
    fs::create_dir_all(parent)?;
    fs::rename(source, destination).with_context(|| {
        format!(
            "failed to move recovery evidence from {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn quarantine_finding(
    artifact_root: &Path,
    session: &RecoverySession,
    run: &RecoveryRun,
    segment_seq: Option<u32>,
    retained_path: &Path,
    code: &'static str,
) -> anyhow::Result<RecoveryFinding> {
    Ok(RecoveryFinding {
        severity: "warning",
        code,
        action: "moved_to_quarantine",
        session_id: Some(session.id.clone()),
        run_name: Some(run.run_name.clone()),
        segment_seq,
        relative_path: Some(root_relative(artifact_root, retained_path)?),
        message: "Uncommitted capture evidence was preserved in artifact quarantine.".into(),
    })
}

fn root_relative(root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path
        .strip_prefix(root)
        .context("recovery diagnostic path escaped its configured storage root")?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn verify_ready_segment(
    session_dir: &Path,
    session: &RecoverySession,
    run: &RecoveryRun,
    segment: &RecoverySegment,
    verified_bytes: &mut u64,
) -> Result<(), VerificationFailure> {
    let prefix = format!("runs/{}/segments/{:05}", run.run_name, segment.seq);
    let expected_paths = [
        ("video.mp4", segment.video_path.as_deref()),
        ("frames.jsonl.zst", segment.frames_path.as_deref()),
        ("events.jsonl.zst", segment.events_path.as_deref()),
        ("meta.json", segment.meta_path.as_deref()),
        ("sha256.json", segment.manifest_path.as_deref()),
    ];
    for (name, persisted) in expected_paths {
        if persisted != Some(format!("{prefix}/{name}").as_str()) {
            return Err(verification_failure(
                "RECOVERY_READY_PATH_INVALID",
                "Ready segment contains a missing or unexpected journal-relative path.",
            ));
        }
    }

    let segment_dir = session_dir
        .join("runs")
        .join(&run.run_name)
        .join("segments")
        .join(format!("{:05}", segment.seq));
    let mut required_names = vec![
        "video.mp4",
        "frames.jsonl.zst",
        "events.jsonl.zst",
        "meta.json",
        "sha256.json",
    ];
    if segment
        .audio
        .as_ref()
        .is_some_and(SegmentAudioMetadata::has_artifact)
    {
        required_names.push(AUDIO_ARTIFACT_NAME);
    }
    if required_names
        .iter()
        .any(|name| !segment_dir.join(name).is_file())
    {
        return Err(verification_failure(
            "RECOVERY_READY_FILE_MISSING",
            "Ready segment is missing one or more required artifact files.",
        ));
    }

    let mut persisted_hashes = vec![
        ("video.mp4", segment.video_sha256.as_deref()),
        ("frames.jsonl.zst", segment.frames_sha256.as_deref()),
        ("events.jsonl.zst", segment.events_sha256.as_deref()),
        ("meta.json", segment.meta_sha256.as_deref()),
    ];
    if let Some(audio) = segment.audio.as_ref().filter(|audio| audio.has_artifact()) {
        persisted_hashes.push((AUDIO_ARTIFACT_NAME, audio.sha256.as_deref()));
    }
    let manifest_bytes = fs::read(segment_dir.join("sha256.json")).map_err(|_| {
        verification_failure(
            "RECOVERY_READY_MANIFEST_INVALID",
            "Ready segment SHA-256 manifest could not be read.",
        )
    })?;
    let manifest: HashMap<String, String> =
        serde_json::from_slice(&manifest_bytes).map_err(|_| {
            verification_failure(
                "RECOVERY_READY_MANIFEST_INVALID",
                "Ready segment SHA-256 manifest is invalid JSON.",
            )
        })?;
    if manifest.len() != persisted_hashes.len() {
        return Err(verification_failure(
            "RECOVERY_READY_MANIFEST_INVALID",
            "Ready segment SHA-256 manifest has unexpected entries.",
        ));
    }
    for (name, persisted_hash) in persisted_hashes {
        let Some(persisted_hash) = persisted_hash else {
            return Err(verification_failure(
                "RECOVERY_READY_JOURNAL_INCOMPLETE",
                "Ready segment journal row is missing an artifact hash.",
            ));
        };
        let actual_hash =
            file_sha256_counted(&segment_dir.join(name), verified_bytes).map_err(|_| {
                verification_failure(
                    "RECOVERY_READY_FILE_UNREADABLE",
                    "Ready segment artifact could not be fully read for SHA-256 verification.",
                )
            })?;
        if manifest.get(name).map(String::as_str) != Some(persisted_hash)
            || actual_hash != persisted_hash
        {
            return Err(verification_failure(
                "RECOVERY_READY_HASH_MISMATCH",
                "Ready segment artifact hash differs from its manifest or journal identity.",
            ));
        }
    }

    let meta_bytes = fs::read(segment_dir.join("meta.json")).map_err(|_| {
        verification_failure(
            "RECOVERY_READY_META_INVALID",
            "Ready segment metadata could not be read.",
        )
    })?;
    let meta: RecoveryMeta = serde_json::from_slice(&meta_bytes).map_err(|_| {
        verification_failure(
            "RECOVERY_READY_META_INVALID",
            "Ready segment metadata is invalid JSON.",
        )
    })?;
    // 这一场属于哪一代，是它创建时写死的历史事实（issue #855 §B）。段必须按**它
    // 自己那一代**校验，不按今天的目录重新推断。`dm-local-dev-session-1` 没有目录
    // 证据，解析不出契约，走下面的 `None` 分支——与断代前逐字同一个行为。
    let contract = SessionContract::for_session_schema_version(&session.schema_version);
    let catalog_identity_valid = match contract {
        Some(contract) => meta
            .catalog
            .as_ref()
            .is_some_and(|catalog| catalog_matches_session(catalog, session, contract)),
        None => true,
    };
    // meta 的世代、state 与训练声明全部由契约给出；对不上即判损坏。契约解析不出来
    // 时（第一代）没有可比的期望值，沿用断代前"跳过这几项"的行为。
    let generation_valid = contract.is_none_or(|contract| {
        meta.schema_version == contract.recording_schema_version(segment.audio.is_some())
            && meta.state == contract.segment_ready_state()
            && meta.non_trainable == contract.declares_training_classification().then_some(true)
            && meta.production_certified
                == contract.declares_training_classification().then_some(false)
    });
    let identity_valid = generation_valid
        && catalog_identity_valid
        && meta.session_id == segment.session_id
        && meta.run_id == run.run_name
        && meta.segment_index == segment.seq
        && meta.session_frame_start == segment.session_frame_start
        && Some(meta.n_frames) == segment.n_frames
        && Some(meta.qpc_start) == segment.qpc_start
        && Some(meta.qpc_end) == segment.qpc_end
        && meta.fps_num == run.fps_num
        && meta.fps_den == run.fps_den
        && meta.width == run.width
        && meta.height == run.height
        && meta.video.path == "video.mp4"
        && Some(meta.video.bytes) == segment.bytes
        && meta.video.sha256 == segment.video_sha256.as_deref().unwrap_or_default()
        && meta.frames.path == "frames.jsonl.zst"
        && meta.frames.lines == meta.n_frames
        && meta.frames.sha256 == segment.frames_sha256.as_deref().unwrap_or_default()
        && meta.events.path == "events.jsonl.zst"
        && Some(meta.events.lines) == segment.event_count
        && meta.events.sha256 == segment.events_sha256.as_deref().unwrap_or_default()
        && meta.audio == segment.audio
        && meta.mapping.id == session.mapping_id
        && meta.mapping.version == session.mapping_version
        && meta.mapping.content_hash == session.mapping_hash
        && Some(meta.health.duplicate_frames) == segment.duplicate_frames
        && Some(meta.health.source_superseded) == segment.source_superseded
        && Some(meta.health.queue_drops) == segment.source_drops
        && meta.health.queue_drops == 0
        && meta.health.input_overflow == 0;
    if !identity_valid {
        return Err(verification_failure(
            "RECOVERY_READY_META_MISMATCH",
            "Ready segment metadata no longer matches its journal identity or health contract.",
        ));
    }
    if let Some(audio) = &segment.audio {
        if audio.has_artifact() {
            let audio_path = segment_dir.join(AUDIO_ARTIFACT_NAME);
            let audio_bytes = fs::metadata(&audio_path)
                .map_err(|_| {
                    verification_failure(
                        "RECOVERY_READY_FILE_UNREADABLE",
                        "Ready audio metadata could not be read.",
                    )
                })?
                .len();
            if audio.bytes != Some(audio_bytes) {
                return Err(verification_failure(
                    "RECOVERY_READY_SIZE_MISMATCH",
                    "Ready audio byte count differs from its journal and metadata.",
                ));
            }
        } else if segment_dir.join(AUDIO_ARTIFACT_NAME).exists() {
            return Err(verification_failure(
                "RECOVERY_READY_MANIFEST_INVALID",
                "Absent audio unexpectedly retained a sidecar.",
            ));
        }
    } else if segment_dir.join(AUDIO_ARTIFACT_NAME).exists() {
        return Err(verification_failure(
            "RECOVERY_READY_MANIFEST_INVALID",
            "Legacy ready segment unexpectedly contains an audio sidecar.",
        ));
    }
    let video_bytes = fs::metadata(segment_dir.join("video.mp4"))
        .map_err(|_| {
            verification_failure(
                "RECOVERY_READY_FILE_UNREADABLE",
                "Ready video metadata could not be read.",
            )
        })?
        .len();
    if video_bytes != meta.video.bytes {
        return Err(verification_failure(
            "RECOVERY_READY_SIZE_MISMATCH",
            "Ready video byte count differs from its journal and metadata.",
        ));
    }
    Ok(())
}

fn catalog_matches_session(
    catalog: &RecoveryCatalog,
    session: &RecoverySession,
    contract: SessionContract,
) -> bool {
    // 产物里的 `mode` 是 **legacy 的**词，journal 里存的是契约的 source family。
    // 断代前两者恰好是同一个字符串，现在不是了——各自与契约比，存量段因此原样通过。
    session.session_contract == contract.source_family()
        && catalog.mode.as_deref() == contract.legacy_catalog_mode()
        && catalog.catalog_id == session.catalog_id
        && catalog.catalog_version == session.catalog_version
        && catalog.catalog_content_hash == session.catalog_hash
        && catalog_entry_matches(
            &catalog.device,
            &session.device_profile_id,
            &session.device_profile_version,
            &session.device_profile_hash,
        )
        && catalog_entry_matches(
            &catalog.recording,
            &session.recording_profile_id,
            &session.recording_profile_version,
            &session.recording_profile_hash,
        )
        && catalog_entry_matches(
            &catalog.game,
            &session.game_id,
            &session.game_profile_version,
            &session.game_profile_hash,
        )
        && catalog_entry_matches(
            &catalog.mapping,
            &session.mapping_id,
            &session.mapping_version,
            &session.mapping_hash,
        )
}

fn catalog_entry_matches(
    entry: &RecoveryCatalogEntry,
    id: &str,
    version: &str,
    content_hash: &str,
) -> bool {
    entry.id == id && entry.version == version && entry.content_hash == content_hash
}

fn verify_session_catalog_snapshots(
    session_dir: &Path,
    session: &RecoverySession,
) -> Result<(), VerificationFailure> {
    if session.schema_version == "dm-local-dev-session-1"
        && session.recording_profile_hash_kind == "legacy_reencoded_json_sha256"
    {
        // v1-v4 used a reserialized CaptureProfile hash and did not persist the
        // full catalog snapshot set. Keep those immutable artifacts readable;
        // never reinterpret or rewrite them as exact-source-byte sessions.
        return Ok(());
    }
    let Some(contract) = SessionContract::for_session_schema_version(&session.schema_version)
    else {
        return Err(verification_failure(
            "RECOVERY_SESSION_CATALOG_SCHEMA_INVALID",
            "Session catalog hash semantics are unsupported or internally inconsistent.",
        ));
    };
    if session.recording_profile_hash_kind != "exact_source_bytes_sha256" {
        return Err(verification_failure(
            "RECOVERY_SESSION_CATALOG_SCHEMA_INVALID",
            "Session catalog hash semantics are unsupported or internally inconsistent.",
        ));
    }
    // v33 之前这里比的是 `catalog_mode == "dev_non_trainable"`。那一列现在存契约的
    // source family，判据因此换成"契约与列自洽"——对 v33 迁上来的 legacy 行逐字
    // 等价（它们的列值就是 `dm-local-dev`），而 v19 之前那批没有目录证据的行
    // （迁成 `legacy`）继续在这里被拒，与断代前一模一样。
    if session.session_contract != contract.source_family() {
        return Err(verification_failure(
            "RECOVERY_SESSION_CATALOG_MODE_INVALID",
            "Local recorder session has an unsupported ingest contract.",
        ));
    }
    let expected = [
        ("catalog.json", session.catalog_hash.as_str()),
        ("device-profile.json", session.device_profile_hash.as_str()),
        (
            "recording-profile.json",
            session.recording_profile_hash.as_str(),
        ),
        ("game-profile.json", session.game_profile_hash.as_str()),
        ("mapping.json", session.mapping_hash.as_str()),
    ];
    for (name, expected_hash) in expected {
        let path = session_dir.join(name);
        if !path.is_file() {
            return Err(verification_failure(
                "RECOVERY_SESSION_CATALOG_SNAPSHOT_MISSING",
                "Versioned session catalog snapshot is missing.",
            ));
        }
        let actual_hash = file_sha256(&path).map_err(|_| {
            verification_failure(
                "RECOVERY_SESSION_CATALOG_SNAPSHOT_UNREADABLE",
                "Versioned session catalog snapshot could not be fully read.",
            )
        })?;
        if actual_hash != expected_hash {
            return Err(verification_failure(
                "RECOVERY_SESSION_CATALOG_SNAPSHOT_HASH_MISMATCH",
                "Versioned session catalog snapshot no longer matches its exact-byte journal hash.",
            ));
        }
    }
    let manifest_bytes = fs::read(session_dir.join("session.json")).map_err(|_| {
        verification_failure(
            "RECOVERY_SESSION_MANIFEST_MISSING",
            "Versioned session manifest is missing or unreadable.",
        )
    })?;
    let manifest: RecoverySessionManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| {
            verification_failure(
                "RECOVERY_SESSION_MANIFEST_INVALID",
                "Versioned session manifest is invalid or has unexpected fields.",
            )
        })?;
    let recording_profile_bytes =
        fs::read(session_dir.join("recording-profile.json")).map_err(|_| {
            verification_failure(
                "RECOVERY_SESSION_CATALOG_SNAPSHOT_UNREADABLE",
                "Versioned recording profile snapshot could not be fully read.",
            )
        })?;
    // 快照里躺的是**整册** recording profile，不是引擎投影。`CaptureProfile` 是
    // `deny_unknown_fields` 的，直接往它上面反序列化必然失败（docs/57 §3.1 之后
    // 那份 JSON 多了 schema_version / game_id / window_mode / view_scale 等等）——
    // 下面的对账只用得到投影出来的那几个字段，所以先按整册解析再投影。
    let recording_profile: RecordingProfile = serde_json::from_slice(&recording_profile_bytes)
        .map_err(|_| {
            verification_failure(
                "RECOVERY_SESSION_CAPTURE_PROFILE_INVALID",
                "Versioned recording profile snapshot is invalid JSON.",
            )
        })?;
    if !session_manifest_matches(&manifest, session, &recording_profile.capture_profile()) {
        return Err(verification_failure(
            "RECOVERY_SESSION_MANIFEST_MISMATCH",
            "Versioned session manifest no longer matches journal and catalog snapshot facts.",
        ));
    }
    Ok(())
}

fn session_manifest_matches(
    manifest: &RecoverySessionManifest,
    session: &RecoverySession,
    profile: &CaptureProfile,
) -> bool {
    let audio_policy_matches = match &profile.audio {
        Some(audio) => serde_json::to_value(audio)
            .is_ok_and(|expected| manifest.capture_policy.audio == expected),
        None => manifest.capture_policy.audio == serde_json::Value::Bool(false),
    };
    let source_policy_matches = match (
        manifest.capture_policy.minimum_source_width,
        manifest.capture_policy.minimum_source_height,
    ) {
        (None, None) => !manifest.capture_policy.scale,
        (Some(width), Some(height)) => {
            (width, height) == profile.source_geometry_rule().dimensions()
                && manifest.capture_policy.scale == profile.allows_source_scaling()
        }
        _ => false,
    };
    let segment_policy_matches = match &manifest.capture_policy.segment_policy_override {
        None => manifest.capture_policy.segment_seconds == profile.segment_seconds,
        Some(policy) => {
            !policy.reason.trim().is_empty()
                && policy.catalog_segment_seconds == profile.segment_seconds
                && policy.catalog_test_segments == profile.test_segments
                && policy.effective_segment_seconds == manifest.capture_policy.segment_seconds
                && policy.effective_test_segments > 0
                && u64::from(policy.catalog_segment_seconds)
                    * u64::from(policy.catalog_test_segments)
                    == u64::from(policy.effective_segment_seconds)
                        * u64::from(policy.effective_test_segments)
        }
    };
    let Some(contract) = SessionContract::for_session_schema_version(&session.schema_version)
    else {
        return false;
    };
    manifest.schema_version == contract.session_schema_version(profile.audio.is_some())
        && session.schema_version == manifest.schema_version
        && manifest.session_id == session.id
        // legacy 的两个训练声明必须在场且为那一对定值；中性一代必须一个都不带
        // （issue #855 §B）。两个方向都拦：只拦前半条，一份自称中性却仍在申报训练
        // 分类的 manifest 会被判成完好。consent 的在场性不再与任何车道成对
        // （docs/100），这里不校验它——它是一条如实记录的载体，不是分类的输入。
        && manifest.non_trainable == contract.declares_training_classification().then_some(true)
        && manifest.production_certified
            == contract.declares_training_classification().then_some(false)
        && manifest.game.id == session.game_id
        && manifest.game.exe_hash == session.game_executable_sha256
        && manifest.mapping.id == session.mapping_id
        && manifest.mapping.version == session.mapping_version
        && manifest.mapping.content_hash == session.mapping_hash
        && manifest.mapping.recording_profile_id == session.recording_profile_id
        && catalog_matches_session(&manifest.catalog, session, contract)
        && profile.profile_id == session.recording_profile_id
        // 引擎写侧的同一条规则（record_until_stopped）：目录必须配 non-trainable
        // profile。
        && profile.non_trainable
        && manifest.capture_policy.source == "game_window_only"
        && manifest.capture_policy.width == profile.width
        && manifest.capture_policy.height == profile.height
        && source_policy_matches
        && manifest.capture_policy.fps_num == profile.fps_num
        && manifest.capture_policy.fps_den == profile.fps_den
        && manifest.capture_policy.avg_bitrate_bps == profile.bitrate_bps
        && manifest.capture_policy.max_bitrate_bps == profile.max_bitrate_bps
        && segment_policy_matches
        && manifest.capture_policy.gop_frames == profile.gop_frames
        && audio_policy_matches
        && manifest.contract.version == "dm-1"
        && manifest.contract.pointer_clamp == 200
        && manifest.contract.pointer_mu == 8
        && manifest.contract.pointer_ref == 200
        && manifest.contract.pointer_max_bin == 10
        && manifest.todo == ["annotator_device_consent_grant", "crash_recovery"]
}

fn verification_failure(code: &'static str, message: &'static str) -> VerificationFailure {
    VerificationFailure { code, message }
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut ignored_bytes = 0_u64;
    file_sha256_counted(path, &mut ignored_bytes)
}

/// Updates the caller's counter after every successful read, including reads
/// completed before a later I/O error or hash mismatch aborts verification.
fn file_sha256_counted(path: &Path, verified_bytes: &mut u64) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    // Startup recovery runs on the Windows GUI main thread, whose debug stack
    // is too small for a 1 MiB local array once Tauri's setup frames are added.
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        *verified_bytes = verified_bytes.saturating_add(count as u64);
        hasher.update(&buffer[..count]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn write_report_atomic(path: &Path, report: &RecoveryReport) -> anyhow::Result<()> {
    ensure!(!path.exists(), "recovery report id collision");
    let file_name = path
        .file_name()
        .context("recovery report path has no file name")?
        .to_string_lossy();
    let staging = path.with_file_name(format!(".{file_name}.tmp"));
    ensure!(!staging.exists(), "recovery report staging collision");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    serde_json::to_writer_pretty(&mut file, report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staging, path)?;
    Ok(())
}

fn unix_ms() -> anyhow::Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_millis();
    u64::try_from(millis).context("system timestamp exceeds u64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use capture_core::mapping::MappingSnapshot;
    use capture_core::profile::CaptureProfile;
    use capture_core::recording::PublishedSegment;

    use crate::domain::collector::MarkerKind;
    use crate::platform::windows::catalog::CatalogSelection;
    use crate::services::journal::{HeartbeatWrite, MarkerWrite, TestRunStart};

    #[test]
    fn ready_hashing_does_not_require_a_megabyte_of_thread_stack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.bin");
        fs::write(&path, b"ready-artifact").unwrap();

        let hash = std::thread::Builder::new()
            .name("small-startup-stack".into())
            .stack_size(256 * 1024)
            .spawn(move || file_sha256(&path))
            .unwrap()
            .join()
            .unwrap()
            .unwrap();

        assert_eq!(
            hash,
            format!("sha256:{:x}", Sha256::digest(b"ready-artifact"))
        );
    }

    const MAPPING: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.test.pc",
      "version":"1",
      "game":{"id":"test","name":"Test","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"test-profile",
      "bindings":[{"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"}]
    }"#;

    struct Fixture {
        _temp: tempfile::TempDir,
        app_root: PathBuf,
        output_root: PathBuf,
        journal: SessionJournal,
        mapping: MappingSnapshot,
        catalog: CatalogSelection,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_profile(profile())
        }

        fn with_profile(profile: CaptureProfile) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let app_root = temp.path().join("app");
            let output_root = app_root.join("non-trainable-sessions");
            let journal = SessionJournal::open(app_root.join("journal.sqlite3")).unwrap();
            let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
            let catalog = CatalogSelection::for_test(&profile, &mapping);
            journal
                .begin_test_run(&TestRunStart {
                    request_id: "request-1",
                    session_id: "session-1",
                    run_id: "r0000",
                    run_seq: 0,
                    session_frame_start: 0,
                    process_id: 42,
                    target_window_id: "window-digest",
                    executable_sha256: Some("sha256:exe"),
                    executable_sha256_missing_reason: None,
                    profile: &profile,
                    mapping: &mapping,
                    view_calibration: None,
                })
                .unwrap();
            write_catalog_snapshots(&output_root.join("session-1"), &catalog);
            write_session_manifest(&output_root.join("session-1"), &profile, &mapping, &catalog);
            Self {
                _temp: temp,
                app_root,
                output_root,
                journal,
                mapping,
                catalog,
            }
        }

        fn run_dir(&self) -> PathBuf {
            self.output_root.join("session-1/runs/r0000")
        }

        fn publish_ready(&self) -> PathBuf {
            self.journal
                .mark_run_started("session-1", "r0000", 100, 10_000_000)
                .unwrap();
            self.journal
                .mark_segment_started("session-1", "r0000", 0, 0)
                .unwrap();
            self.journal
                .mark_segment_finalizing("session-1", "r0000", 0, 0, 300)
                .unwrap();
            let segment_dir = write_ready_segment(&self.run_dir(), &self.mapping, &self.catalog);
            let published = published_segment(&segment_dir);
            self.journal
                .mark_segment_ready("session-1", "r0000", &published, &self.run_dir())
                .unwrap();
            segment_dir
        }
    }

    fn profile() -> CaptureProfile {
        CaptureProfile {
            profile_id: "test-profile".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 8_000_000,
            max_bitrate_bps: 12_000_000,
            gop_frames: 120,
            segment_seconds: 10,
            test_segments: 1,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    fn write_catalog_snapshots(session_directory: &Path, catalog: &CatalogSelection) {
        fs::create_dir_all(session_directory).unwrap();
        let context = catalog.recording_context();
        for (name, bytes) in [
            ("catalog.json", context.catalog_source_bytes.as_slice()),
            (
                "device-profile.json",
                context.device.source_bytes.as_slice(),
            ),
            (
                "recording-profile.json",
                context.recording.source_bytes.as_slice(),
            ),
            ("game-profile.json", context.game.source_bytes.as_slice()),
            ("mapping.json", context.mapping.source_bytes.as_slice()),
        ] {
            fs::write(session_directory.join(name), bytes).unwrap();
        }
    }

    fn write_session_manifest(
        session_directory: &Path,
        profile: &CaptureProfile,
        mapping: &MappingSnapshot,
        catalog: &CatalogSelection,
    ) {
        let mapping = mapping.mapping();
        let manifest = serde_json::json!({
            "schema_version": "dm-local-dev-session-2",
            "session_id": "session-1",
            "non_trainable": true,
            "production_certified": false,
            "game": {
                "id": mapping.game.id,
                "exe_hash": "sha256:exe",
            },
            "mapping": {
                "id": mapping.mapping_id,
                "version": mapping.version,
                "content_hash": catalog.mapping.identity.content_hash,
                "recording_profile_id": mapping.recording_profile_id,
            },
            "catalog": catalog.recording_context(),
            "capture_policy": {
                "source": "game_window_only",
                "width": profile.width,
                "height": profile.height,
                "scale": false,
                "fps_num": profile.fps_num,
                "fps_den": profile.fps_den,
                "avg_bitrate_bps": profile.bitrate_bps,
                "max_bitrate_bps": profile.max_bitrate_bps,
                "segment_seconds": profile.segment_seconds,
                "gop_frames": profile.gop_frames,
                "audio": false,
                "segment_policy_override": null,
            },
            "contract": {
                "version": "dm-1",
                "pointer_clamp": 200,
                "pointer_mu": 8,
                "pointer_ref": 200,
                "pointer_max_bin": 10,
            },
            "todo": ["annotator_device_consent_grant", "crash_recovery"],
        });
        fs::write(
            session_directory.join("session.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn write_ready_segment(
        run_directory: &Path,
        mapping: &MappingSnapshot,
        catalog: &CatalogSelection,
    ) -> PathBuf {
        let segment_dir = run_directory.join("segments/00000");
        fs::create_dir_all(&segment_dir).unwrap();
        fs::write(segment_dir.join("video.mp4"), b"video").unwrap();
        fs::write(segment_dir.join("frames.jsonl.zst"), b"frames").unwrap();
        fs::write(segment_dir.join("events.jsonl.zst"), b"events").unwrap();
        let video_hash = file_sha256(&segment_dir.join("video.mp4")).unwrap();
        let frames_hash = file_sha256(&segment_dir.join("frames.jsonl.zst")).unwrap();
        let events_hash = file_sha256(&segment_dir.join("events.jsonl.zst")).unwrap();
        let meta = serde_json::json!({
            "schema_version": "dm-local-dev-recording-1",
            "session_id": "session-1",
            "run_id": "r0000",
            "segment_index": 0,
            "session_frame_start": 0,
            "n_frames": 300,
            "state": "non_trainable_ready",
            "non_trainable": true,
            "production_certified": false,
            "qpc_start": 100,
            "qpc_end": 200,
            "fps_num": 30,
            "fps_den": 1,
            "width": 1920,
            "height": 1080,
            "video": {"path": "video.mp4", "bytes": 5, "sha256": video_hash},
            "frames": {"path": "frames.jsonl.zst", "lines": 300, "sha256": frames_hash},
            "events": {"path": "events.jsonl.zst", "lines": 2, "sha256": events_hash},
            "mapping": {
                "id": mapping.mapping().mapping_id,
                "version": mapping.mapping().version,
                "content_hash": mapping.content_hash()
            },
            "catalog": catalog.recording_context(),
            "health": {
                "duplicate_frames": 0,
                "source_superseded": 10,
                "queue_drops": 0,
                "input_overflow": 0
            }
        });
        let meta_bytes = serde_json::to_vec_pretty(&meta).unwrap();
        fs::write(segment_dir.join("meta.json"), &meta_bytes).unwrap();
        let meta_hash = file_sha256(&segment_dir.join("meta.json")).unwrap();
        let manifest = serde_json::json!({
            "video.mp4": video_hash,
            "frames.jsonl.zst": frames_hash,
            "events.jsonl.zst": events_hash,
            "meta.json": meta_hash
        });
        fs::write(
            segment_dir.join("sha256.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        segment_dir
    }

    fn published_segment(segment_dir: &Path) -> PublishedSegment {
        PublishedSegment {
            index: 0,
            frame_count: 300,
            event_count: 2,
            qpc_start: 100,
            qpc_end: 200,
            video_bytes: 5,
            video_sha256: file_sha256(&segment_dir.join("video.mp4")).unwrap(),
            frames_sha256: file_sha256(&segment_dir.join("frames.jsonl.zst")).unwrap(),
            events_sha256: file_sha256(&segment_dir.join("events.jsonl.zst")).unwrap(),
            meta_sha256: file_sha256(&segment_dir.join("meta.json")).unwrap(),
            audio: None,
        }
    }

    #[test]
    fn intact_ready_artifact_is_reverified_without_rewrite() {
        let fixture = Fixture::new();
        let segment_dir = fixture.publish_ready();
        let video_path = segment_dir.join("video.mp4");
        let original_modified = fs::metadata(&video_path).unwrap().modified().unwrap();

        let first = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let second = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        // 带上 findings：校验失败时它里面就是具体的 RECOVERY_* 码，否则这条断言只会
        // 说「0 != 1」，把人推去猜。
        assert_eq!(first.verified_ready_segments, 1, "{:?}", first.findings);
        assert_eq!(second.verified_ready_segments, 1, "{:?}", second.findings);
        assert_eq!(first.corrupt_segments, 0);
        assert_eq!(second.quarantined_items, 0);
        assert_eq!(
            fs::metadata(&video_path).unwrap().modified().unwrap(),
            original_modified
        );
        assert_eq!(fs::read(&video_path).unwrap(), b"video");
        assert!(fixture.app_root.join(&first.report_path).is_file());
        let inventory = fixture.journal.recovery_inventory().unwrap();
        assert_eq!(inventory.segments[0].state, "ready");
    }

    #[test]
    fn equal_horizon_segment_override_is_recovered_against_the_catalog_profile() {
        let mut catalog_profile = profile();
        catalog_profile.test_segments = 30;
        let fixture = Fixture::with_profile(catalog_profile);
        let manifest_path = fixture.output_root.join("session-1/session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["capture_policy"]["segment_seconds"] = serde_json::json!(300);
        manifest["capture_policy"]["segment_policy_override"] = serde_json::json!({
            "reason": "cp2077_local_depth_trial_single_video",
            "catalog_segment_seconds": 10,
            "catalog_test_segments": 30,
            "effective_segment_seconds": 300,
            "effective_test_segments": 1,
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let session = fixture
            .journal
            .recovery_inventory()
            .unwrap()
            .sessions
            .into_iter()
            .next()
            .unwrap();

        assert!(
            verify_session_catalog_snapshots(&fixture.output_root.join("session-1"), &session)
                .is_ok()
        );
    }

    #[test]
    fn segment_override_that_changes_the_capture_horizon_is_rejected() {
        let mut catalog_profile = profile();
        catalog_profile.test_segments = 30;
        let fixture = Fixture::with_profile(catalog_profile);
        let manifest_path = fixture.output_root.join("session-1/session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["capture_policy"]["segment_seconds"] = serde_json::json!(300);
        manifest["capture_policy"]["segment_policy_override"] = serde_json::json!({
            "reason": "cp2077_local_depth_trial_single_video",
            "catalog_segment_seconds": 10,
            "catalog_test_segments": 30,
            "effective_segment_seconds": 300,
            "effective_test_segments": 2,
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let session = fixture
            .journal
            .recovery_inventory()
            .unwrap()
            .sessions
            .into_iter()
            .next()
            .unwrap();

        let failure =
            verify_session_catalog_snapshots(&fixture.output_root.join("session-1"), &session)
                .unwrap_err();
        assert_eq!(failure.code, "RECOVERY_SESSION_MANIFEST_MISMATCH");
    }

    #[test]
    fn scalable_source_policy_is_recovered_without_weakening_output_profile_checks() {
        let fixture = Fixture::new();
        fixture.publish_ready();
        let manifest_path = fixture.output_root.join("session-1/session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["capture_policy"]["scale"] = serde_json::json!(true);
        manifest["capture_policy"]["minimum_source_width"] = serde_json::json!(960);
        manifest["capture_policy"]["minimum_source_height"] = serde_json::json!(540);
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let report = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(report.verified_ready_segments, 1);
        assert_eq!(report.corrupt_segments, 0);
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.code == "RECOVERY_SESSION_MANIFEST_MISMATCH")
        );
    }

    #[test]
    fn interrupted_preflight_scratch_is_quarantined_once_outside_session_tree() {
        let temp = tempfile::tempdir().unwrap();
        let app_root = temp.path().join("app");
        let output_root = app_root.join("non-trainable-sessions");
        let journal = SessionJournal::open(app_root.join("journal.sqlite3")).unwrap();
        let scratch = app_root.join(".preflight-scratch");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(
            scratch.join("hardware-preflight-0001.mp4"),
            b"partial-probe",
        )
        .unwrap();

        let first = recover(&journal, &app_root, &output_root).unwrap();
        let second = recover(&journal, &app_root, &output_root).unwrap();
        let retained = app_root
            .join("quarantine")
            .join(&first.recovery_id)
            .join("preflight-scratch/hardware-preflight-0001.mp4");

        assert_eq!(first.quarantined_items, 1);
        assert_eq!(first.diagnostic_severity(), "warning");
        assert_eq!(first.orphan_items, 0);
        assert!(first.findings.iter().any(|finding| {
            finding.code == "RECOVERY_PREFLIGHT_SCRATCH"
                && finding.action == "moved_to_quarantine"
                && finding.session_id.is_none()
                && finding
                    .relative_path
                    .as_deref()
                    .is_some_and(|path| path.starts_with("quarantine/recovery-"))
        }));
        assert!(retained.is_file());
        assert!(!scratch.exists());
        assert_eq!(second.quarantined_items, 0);
        assert_eq!(second.diagnostic_severity(), "info");
        assert!(
            !second
                .findings
                .iter()
                .any(|finding| finding.code == "RECOVERY_PREFLIGHT_SCRATCH")
        );
    }

    #[test]
    fn split_roots_keep_quarantine_with_artifacts_and_report_with_control_data() {
        let temp = tempfile::tempdir().unwrap();
        let control_root = temp.path().join("control");
        let artifact_root = temp.path().join("artifacts");
        let output_root = artifact_root.join("non-trainable-sessions");
        let journal = SessionJournal::open(control_root.join("journal.sqlite3")).unwrap();
        let scratch = artifact_root.join(".preflight-scratch");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("hardware-preflight.mp4"), b"partial-probe").unwrap();

        let report =
            recover_with_roots(&journal, &control_root, &artifact_root, &output_root).unwrap();

        assert!(
            artifact_root
                .join("quarantine")
                .join(&report.recovery_id)
                .join("preflight-scratch/hardware-preflight.mp4")
                .is_file()
        );
        assert!(control_root.join(&report.report_path).is_file());
        assert!(!control_root.join("quarantine").exists());
        assert!(!artifact_root.join("recovery").exists());
    }

    #[test]
    fn empty_recovery_is_info_and_remains_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let app_root = temp.path().join("app");
        let output_root = app_root.join("non-trainable-sessions");
        let journal = SessionJournal::open(app_root.join("journal.sqlite3")).unwrap();

        let first = recover(&journal, &app_root, &output_root).unwrap();
        let second = recover(&journal, &app_root, &output_root).unwrap();

        assert_eq!(first.diagnostic_severity(), "info");
        assert_eq!(second.diagnostic_severity(), "info");
        assert!(first.findings.is_empty());
        assert!(second.findings.is_empty());
    }

    #[test]
    fn tampered_exact_byte_catalog_snapshot_marks_session_corrupt_without_repair() {
        let fixture = Fixture::new();
        fixture.publish_ready();
        // 断代后会话目录里只有一份 recording-profile.json（docs/57 §3.1）。改
        // capture-profile.json 等于什么都没改——那个文件根本不存在，校验照常通过，
        // 用例就变成了「篡改不会被发现」的反向证明。
        let recording_snapshot = fixture.output_root.join("session-1/recording-profile.json");
        fs::write(&recording_snapshot, b"tampered-recording-profile").unwrap();

        let report = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(report.corrupt_segments, 1);
        assert_eq!(report.diagnostic_severity(), "error");
        assert!(report.findings.iter().any(|finding| {
            finding.code == "RECOVERY_SESSION_CATALOG_SNAPSHOT_HASH_MISMATCH"
                && finding.session_id.as_deref() == Some("session-1")
                && finding.run_name.is_none()
        }));
        assert_eq!(
            fs::read(&recording_snapshot).unwrap(),
            b"tampered-recording-profile"
        );
        let history = fixture.journal.session_detail("session-1").unwrap();
        assert_eq!(history.session.state, "corrupt");
        assert_eq!(history.segments[0].state, "corrupt");
    }

    #[test]
    fn tampered_v2_session_manifest_is_retained_and_marks_ready_data_corrupt() {
        let fixture = Fixture::new();
        fixture.publish_ready();
        let manifest_path = fixture.output_root.join("session-1/session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["catalog"]["game"]["version"] = serde_json::json!("tampered-version");
        let tampered_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(&manifest_path, &tampered_bytes).unwrap();

        let report = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(report.corrupt_segments, 1);
        assert_eq!(report.diagnostic_severity(), "error");
        assert!(report.findings.iter().any(|finding| {
            finding.code == "RECOVERY_SESSION_MANIFEST_MISMATCH"
                && finding.session_id.as_deref() == Some("session-1")
                && finding.run_name.is_none()
        }));
        assert_eq!(fs::read(&manifest_path).unwrap(), tampered_bytes);
        let history = fixture.journal.session_detail("session-1").unwrap();
        assert_eq!(history.session.state, "corrupt");
        assert_eq!(history.segments[0].state, "corrupt");
    }

    #[test]
    fn unfinished_artifacts_are_quarantined_and_replay_is_idempotent() {
        let fixture = Fixture::new();
        fixture
            .journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        fixture
            .journal
            .mark_segment_started("session-1", "r0000", 0, 0)
            .unwrap();
        let segments_dir = fixture.run_dir().join("segments");
        fs::create_dir_all(segments_dir.join(".00000.tmp")).unwrap();
        fs::write(segments_dir.join(".00000.tmp/video.mp4"), b"partial").unwrap();
        fs::create_dir_all(segments_dir.join("00000")).unwrap();
        fs::write(segments_dir.join("00000/video.mp4"), b"uncommitted").unwrap();
        let spool_dir = fixture.run_dir().join("spool");
        fs::create_dir_all(&spool_dir).unwrap();
        fs::write(spool_dir.join("00000.events.jsonl.active"), b"event\n").unwrap();
        fs::write(
            spool_dir.join("00000.audio.pcm.zst.active"),
            b"partial-audio",
        )
        .unwrap();

        let first = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let second = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(first.quarantined_items, 4);
        assert_eq!(second.quarantined_items, 0);
        assert_eq!(first.diagnostic_severity(), "error");
        assert_eq!(second.diagnostic_severity(), "info");
        assert!(first.findings.iter().any(|finding| {
            finding.action == "moved_to_quarantine"
                && finding
                    .relative_path
                    .as_deref()
                    .is_some_and(|path| path.starts_with("quarantine/"))
        }));
        assert!(
            fixture
                .app_root
                .join("quarantine/session-1/r0000/segments/.00000.tmp/video.mp4")
                .is_file()
        );
        assert!(
            fixture
                .app_root
                .join("quarantine/session-1/r0000/segments/00000/video.mp4")
                .is_file()
        );
        assert!(
            fixture
                .app_root
                .join("quarantine/session-1/r0000/spool/00000.events.jsonl.active")
                .is_file()
        );
        assert!(
            fixture
                .app_root
                .join("quarantine/session-1/r0000/spool/00000.audio.pcm.zst.active")
                .is_file()
        );
        let inventory = fixture.journal.recovery_inventory().unwrap();
        assert_eq!(inventory.sessions[0].state, "faulted");
        assert_eq!(inventory.runs[0].state, "faulted");
        assert_eq!(inventory.segments[0].state, "quarantined");
        let history = fixture.journal.session_detail("session-1").unwrap();
        assert_eq!(history.session.quarantined_segments, 1);
        assert_eq!(history.segments[0].state, "quarantined");
    }

    #[test]
    fn startup_rebuilds_marker_artifact_from_journal_idempotently() {
        let fixture = Fixture::new();
        fixture
            .journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        fixture
            .journal
            .record_marker(&MarkerWrite {
                request_id: "marker-request-1",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: 150,
            })
            .unwrap();
        let session_dir = fixture.output_root.join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("markers.jsonl"), b"partial").unwrap();

        let first = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let second = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let marker_text = fs::read_to_string(session_dir.join("markers.jsonl")).unwrap();
        let marker: serde_json::Value = serde_json::from_str(marker_text.trim()).unwrap();

        assert!(first.findings.iter().any(|finding| {
            finding.code == "RECOVERY_MARKER_ARTIFACT_REBUILT"
                && finding.action == "rebuilt_from_journal"
        }));
        assert!(
            !second
                .findings
                .iter()
                .any(|finding| finding.code == "RECOVERY_MARKER_ARTIFACT_REBUILT")
        );
        assert_eq!(marker["schema_version"], "dm-marker-1");
        assert_eq!(marker["session_id"], "session-1");
        assert_eq!(marker["run_id"], "r0000");
        let history = fixture.journal.session_detail("session-1").unwrap();
        assert_eq!(history.session.marker_count, 1);
        assert_eq!(history.markers[0].alignment_state, "outside_video");
    }

    #[test]
    fn startup_rebuilds_heartbeat_artifact_from_journal_idempotently() {
        let fixture = Fixture::new();
        fixture
            .journal
            .mark_run_started("session-1", "r0000", 10_000_000, 10_000_000)
            .unwrap();
        fixture
            .journal
            .record_heartbeat(&HeartbeatWrite {
                session_id: "session-1",
                run_id: "r0000",
                qpc: 10_000_000,
            })
            .unwrap();
        fixture
            .journal
            .record_heartbeat(&HeartbeatWrite {
                session_id: "session-1",
                run_id: "r0000",
                qpc: 20_000_000,
            })
            .unwrap();
        let session_dir = fixture.output_root.join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("heartbeat.jsonl"), b"partial").unwrap();

        let first = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let second = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let heartbeat_text = fs::read_to_string(session_dir.join("heartbeat.jsonl")).unwrap();
        let rows = heartbeat_text.lines().collect::<Vec<_>>();

        assert!(first.findings.iter().any(|finding| {
            finding.code == "RECOVERY_HEARTBEAT_ARTIFACT_REBUILT"
                && finding.action == "rebuilt_from_journal"
        }));
        assert!(
            !second
                .findings
                .iter()
                .any(|finding| { finding.code == "RECOVERY_HEARTBEAT_ARTIFACT_REBUILT" })
        );
        assert_eq!(rows.len(), 2);
        let first_row: serde_json::Value = serde_json::from_str(rows[0]).unwrap();
        assert_eq!(first_row["t_mono_ns"], 1_000_000_000_u64);
        assert_eq!(first_row["seq"], 0);
        assert_eq!(first_row["collector"], "ok");
        assert!(first_row.get("run_id").is_none());
    }

    #[test]
    fn tampered_ready_artifact_is_marked_corrupt_without_repair() {
        let fixture = Fixture::new();
        let segment_dir = fixture.publish_ready();
        fs::write(segment_dir.join("video.mp4"), b"tampered").unwrap();

        let first = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();
        let second = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(first.corrupt_segments, 1);
        assert_eq!(second.corrupt_segments, 0);
        assert_eq!(first.diagnostic_severity(), "error");
        assert_eq!(second.diagnostic_severity(), "info");
        assert_eq!(
            fs::read(segment_dir.join("video.mp4")).unwrap(),
            b"tampered"
        );
        let inventory = fixture.journal.recovery_inventory().unwrap();
        assert_eq!(inventory.sessions[0].state, "corrupt");
        assert_eq!(inventory.runs[0].state, "faulted");
        assert_eq!(inventory.segments[0].state, "corrupt");
        let history = fixture.journal.session_detail("session-1").unwrap();
        assert_eq!(history.session.corrupt_segments, 1);
        assert_eq!(history.segments[0].state, "corrupt");
    }

    #[test]
    fn verification_bytes_include_reads_before_a_later_hash_mismatch() {
        let fixture = Fixture::new();
        let segment_dir = fixture.publish_ready();
        let tampered_frames = b"tampered-frames";
        fs::write(segment_dir.join("frames.jsonl.zst"), tampered_frames).unwrap();

        let report = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(report.corrupt_segments, 1);
        assert_eq!(
            report.verified_bytes,
            b"video".len() as u64 + tampered_frames.len() as u64
        );
    }

    #[test]
    fn orphan_session_is_reported_but_left_unchanged() {
        let fixture = Fixture::new();
        let orphan = fixture
            .output_root
            .join("legacy-session/runs/r0000/segments/00000");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("video.mp4"), b"legacy").unwrap();

        let report = recover(&fixture.journal, &fixture.app_root, &fixture.output_root).unwrap();

        assert_eq!(report.orphan_items, 1);
        assert!(orphan.join("video.mp4").is_file());
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "RECOVERY_ORPHAN_SESSION")
        );
    }
}
