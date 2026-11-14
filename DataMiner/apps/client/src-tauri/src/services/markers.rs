use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::domain::collector::MarkerAudit;
use crate::services::journal::SessionJournal;

pub const MARKER_ARTIFACT_NAME: &str = "markers.jsonl";
const MARKER_SCHEMA_VERSION: &str = "dm-marker-1";

pub struct MarkerStore {
    output_root: PathBuf,
    session_writers: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkerArtifactSummary {
    pub rows: u32,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerReconcileState {
    Absent,
    Verified,
    Rebuilt,
    Orphaned,
}

#[derive(Serialize)]
struct MarkerArtifactRow<'a> {
    schema_version: &'static str,
    marker_id: &'a str,
    seq: u32,
    session_id: &'a str,
    run_id: &'a str,
    kind: &'a str,
    qpc: i64,
    qpc_frequency: i64,
    run_frame: u64,
    session_frame: u64,
    video_time_us: u64,
    created_at_utc: &'a str,
    non_trainable: bool,
    production_certified: bool,
}

impl MarkerStore {
    #[must_use]
    pub fn new(output_root: PathBuf) -> Self {
        Self {
            output_root,
            session_writers: Mutex::new(HashMap::new()),
        }
    }

    pub fn publish_session(
        &self,
        journal: &SessionJournal,
        session_id: &str,
    ) -> anyhow::Result<MarkerArtifactSummary> {
        self.publish_session_with_snapshot_observer(journal, session_id, || {})
    }

    fn publish_session_with_snapshot_observer(
        &self,
        journal: &SessionJournal,
        session_id: &str,
        snapshot_observer: impl FnOnce(),
    ) -> anyhow::Result<MarkerArtifactSummary> {
        self.with_session_artifact_operation(session_id, || {
            let markers = journal.markers_for_session(session_id)?;
            snapshot_observer();
            ensure!(
                !markers.is_empty(),
                "cannot publish an empty marker artifact"
            );
            let bytes = canonical_bytes(session_id, &markers)?;
            let path = self.marker_path(session_id)?;
            replace_atomic(&path, &bytes)?;
            Ok(MarkerArtifactSummary {
                rows: u32::try_from(markers.len())
                    .context("marker artifact row count exceeds u32")?,
                sha256: bytes_sha256(&bytes),
            })
        })
    }

    pub(crate) fn with_session_artifact_operation<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let writer = self.session_writer(session_id)?;
        let _writer = writer.lock();
        operation()
    }

    pub fn reconcile_session(
        &self,
        session_id: &str,
        markers: &[MarkerAudit],
    ) -> anyhow::Result<MarkerReconcileState> {
        self.with_session_artifact_operation(session_id, || {
            let path = self.marker_path(session_id)?;
            let staging = staging_path(&path)?;
            if markers.is_empty() {
                return Ok(if path.exists() || staging.exists() {
                    MarkerReconcileState::Orphaned
                } else {
                    MarkerReconcileState::Absent
                });
            }

            let expected = canonical_bytes(session_id, markers)?;
            if !staging.exists() && fs::read(&path).ok().as_deref() == Some(expected.as_slice()) {
                return Ok(MarkerReconcileState::Verified);
            }
            replace_atomic(&path, &expected)?;
            Ok(MarkerReconcileState::Rebuilt)
        })
    }

    fn session_writer(&self, session_id: &str) -> anyhow::Result<Arc<Mutex<()>>> {
        ensure_safe_component(session_id, "marker session id")?;
        Ok(self
            .session_writers
            .lock()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    fn marker_path(&self, session_id: &str) -> anyhow::Result<PathBuf> {
        ensure_safe_component(session_id, "marker session id")?;
        Ok(self.output_root.join(session_id).join(MARKER_ARTIFACT_NAME))
    }
}

fn canonical_bytes(session_id: &str, markers: &[MarkerAudit]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for (expected_seq, marker) in markers.iter().enumerate() {
        ensure!(
            marker.session_id == session_id,
            "marker artifact contains a different session identity"
        );
        ensure!(
            usize::try_from(marker.seq).ok() == Some(expected_seq),
            "marker artifact sequence is not contiguous"
        );
        serde_json::to_writer(
            &mut bytes,
            &MarkerArtifactRow {
                schema_version: MARKER_SCHEMA_VERSION,
                marker_id: &marker.marker_id,
                seq: marker.seq,
                session_id: &marker.session_id,
                run_id: &marker.run_id,
                kind: &marker.kind,
                qpc: marker.qpc,
                qpc_frequency: marker.qpc_frequency,
                run_frame: marker.run_frame,
                session_frame: marker.session_frame,
                video_time_us: marker.video_time_us,
                created_at_utc: &marker.created_at_utc,
                non_trainable: true,
                production_certified: false,
            },
        )?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn replace_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("marker artifact has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create marker artifact directory {}",
            parent.display()
        )
    })?;
    let staging = staging_path(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staging)
        .with_context(|| format!("failed to open marker staging file {}", staging.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    let source = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| {
        format!(
            "failed to atomically publish marker artifact {}",
            path.display()
        )
    })?;
    Ok(())
}

fn staging_path(path: &Path) -> anyhow::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("marker artifact path has no UTF-8 file name")?;
    Ok(path.with_file_name(format!(".{file_name}.tmp")))
}

fn ensure_safe_component(value: &str, label: &str) -> anyhow::Result<()> {
    let mut components = Path::new(value).components();
    ensure!(
        !value.is_empty()
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none()
            && !value.contains(['/', '\\']),
        "{label} is not a safe path component"
    );
    Ok(())
}

fn bytes_sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_core::mapping::MappingSnapshot;
    use capture_core::profile::CaptureProfile;

    use crate::domain::collector::MarkerKind;
    use crate::services::journal::{MarkerWrite, TestRunStart};

    const MAPPING: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.test.pc",
      "version":"1",
      "game":{"id":"test","name":"Test","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"test-profile",
      "bindings":[{"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"}]
    }"#;

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

    fn marker(seq: u32) -> MarkerAudit {
        MarkerAudit {
            marker_id: format!("session-1:m{seq:06}"),
            session_id: "session-1".into(),
            run_id: "r0000".into(),
            seq,
            kind: "manual".into(),
            qpc: 100 + i64::from(seq),
            qpc_frequency: 10_000_000,
            run_frame: u64::from(seq),
            session_frame: u64::from(seq),
            video_time_us: u64::from(seq) * 33_333,
            created_at_utc: "2026-07-23T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn canonical_marker_artifact_is_separate_non_trainable_jsonl() {
        let bytes = canonical_bytes("session-1", &[marker(0), marker(1)]).unwrap();
        let lines = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();

        assert_eq!(lines.len(), 2);
        assert_eq!(first["schema_version"], "dm-marker-1");
        assert_eq!(first["run_id"], "r0000");
        assert_eq!(first["video_time_us"], 0);
        assert_eq!(first["non_trainable"], true);
        assert_eq!(first["production_certified"], false);
        assert!(first.get("device").is_none());
        assert!(first.get("type").is_none());
        assert!(first.get("events").is_none());
    }

    #[test]
    fn concurrent_publish_cannot_replace_a_newer_marker_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let output_root = temp.path().join("sessions");
        let journal = Arc::new(SessionJournal::open(temp.path().join("journal.sqlite3")).unwrap());
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let profile = profile();
        journal
            .begin_test_run(&TestRunStart {
                request_id: "run-request-1",
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
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .record_marker(&MarkerWrite {
                request_id: "marker-request-1",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: 150,
            })
            .unwrap();

        let store = Arc::new(MarkerStore::new(output_root.clone()));
        let session_writer = store.session_writer("session-1").unwrap();
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_store = Arc::clone(&store);
        let first_journal = Arc::clone(&journal);
        let first = std::thread::spawn(move || {
            first_store.publish_session_with_snapshot_observer(&first_journal, "session-1", || {
                snapshot_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        snapshot_rx.recv().unwrap();
        assert!(session_writer.is_locked());

        journal
            .record_marker(&MarkerWrite {
                request_id: "marker-request-2",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: 160,
            })
            .unwrap();
        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let second_store = Arc::clone(&store);
        let second_journal = Arc::clone(&journal);
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            second_store.publish_session(&second_journal, "session-1")
        });
        second_started_rx.recv().unwrap();
        release_tx.send(()).unwrap();

        assert_eq!(first.join().unwrap().unwrap().rows, 1);
        assert_eq!(second.join().unwrap().unwrap().rows, 2);
        let markers = journal.markers_for_session("session-1").unwrap();
        assert_eq!(
            fs::read(output_root.join("session-1/markers.jsonl")).unwrap(),
            canonical_bytes("session-1", &markers).unwrap()
        );
    }

    #[test]
    fn terminal_replay_and_session_delete_share_the_session_artifact_lock() {
        let temp = tempfile::tempdir().unwrap();
        let app_data_root = temp.path().join("app");
        let output_root = app_data_root.join("non-trainable-sessions");
        let journal =
            Arc::new(SessionJournal::open(app_data_root.join("journal.sqlite3")).unwrap());
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let profile = profile();
        journal
            .begin_test_run(&TestRunStart {
                request_id: "run-request-1",
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
        journal
            .mark_run_started("session-1", "r0000", 100, 10_000_000)
            .unwrap();
        journal
            .record_marker(&MarkerWrite {
                request_id: "marker-request-1",
                session_id: "session-1",
                run_id: "r0000",
                kind: MarkerKind::Manual,
                qpc: 150,
            })
            .unwrap();
        journal.fail_run("session-1", "r0000", "TEST").unwrap();
        let store = Arc::new(MarkerStore::new(output_root.clone()));
        store.publish_session(&journal, "session-1").unwrap();

        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let replay_store = Arc::clone(&store);
        let replay_journal = Arc::clone(&journal);
        let replay = std::thread::spawn(move || {
            replay_store.publish_session_with_snapshot_observer(
                &replay_journal,
                "session-1",
                || {
                    snapshot_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
        });
        snapshot_rx.recv().unwrap();

        let (delete_started_tx, delete_started_rx) = std::sync::mpsc::channel();
        let delete_store = Arc::clone(&store);
        let delete_journal = Arc::clone(&journal);
        let delete_app_root = app_data_root.clone();
        let delete_output_root = output_root.clone();
        let deletion = std::thread::spawn(move || {
            delete_started_tx.send(()).unwrap();
            delete_store.with_session_artifact_operation("session-1", || {
                crate::services::session_history::delete_session(
                    &delete_journal,
                    &delete_app_root,
                    &delete_output_root,
                    "session-1",
                    "session-1",
                )
            })
        });
        delete_started_rx.recv().unwrap();
        release_tx.send(()).unwrap();

        replay.join().unwrap().unwrap();
        assert!(deletion.join().unwrap().unwrap().artifacts_removed);
        assert!(!journal.session_exists("session-1").unwrap());
        assert!(!output_root.join("session-1").exists());
    }

    #[test]
    fn recovery_rebuilds_partial_or_tampered_marker_projection_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let output_root = temp.path().join("sessions");
        let store = MarkerStore::new(output_root.clone());
        let session_dir = output_root.join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join(MARKER_ARTIFACT_NAME), b"partial").unwrap();
        fs::write(session_dir.join(".markers.jsonl.tmp"), b"stale").unwrap();
        let markers = [marker(0), marker(1)];

        assert_eq!(
            store.reconcile_session("session-1", &markers).unwrap(),
            MarkerReconcileState::Rebuilt
        );
        assert_eq!(
            store.reconcile_session("session-1", &markers).unwrap(),
            MarkerReconcileState::Verified
        );
        assert_eq!(
            fs::read(session_dir.join(MARKER_ARTIFACT_NAME)).unwrap(),
            canonical_bytes("session-1", &markers).unwrap()
        );
        assert!(!session_dir.join(".markers.jsonl.tmp").exists());
    }

    #[test]
    fn durable_journal_marker_survives_artifact_failure_and_is_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let journal_path = temp.path().join("journal.sqlite3");
        let output_root = temp.path().join("sessions");
        fs::create_dir_all(&output_root).unwrap();
        fs::write(output_root.join("session-1"), b"blocks-directory").unwrap();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
        let profile = profile();
        {
            let journal = SessionJournal::open(&journal_path).unwrap();
            journal
                .begin_test_run(&TestRunStart {
                    request_id: "run-request-1",
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
            journal
                .mark_run_started("session-1", "r0000", 100, 10_000_000)
                .unwrap();
            journal
                .record_marker(&MarkerWrite {
                    request_id: "marker-request-1",
                    session_id: "session-1",
                    run_id: "r0000",
                    kind: MarkerKind::Manual,
                    qpc: 150,
                })
                .unwrap();
            let store = MarkerStore::new(output_root.clone());
            assert!(store.publish_session(&journal, "session-1").is_err());
        }

        let reopened = SessionJournal::open(&journal_path).unwrap();
        let persisted = reopened.markers_for_session("session-1").unwrap();
        assert_eq!(persisted.len(), 1);
        fs::remove_file(output_root.join("session-1")).unwrap();
        let store = MarkerStore::new(output_root.clone());
        assert_eq!(
            store.reconcile_session("session-1", &persisted).unwrap(),
            MarkerReconcileState::Rebuilt
        );
        assert!(output_root.join("session-1/markers.jsonl").is_file());
    }
}
