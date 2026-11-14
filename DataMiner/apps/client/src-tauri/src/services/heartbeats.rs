use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use capture_core::qpc::qpc_to_monotonic_ns;
use parking_lot::Mutex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::domain::collector::HeartbeatAudit;
use crate::services::journal::SessionJournal;

pub const HEARTBEAT_ARTIFACT_NAME: &str = "heartbeat.jsonl";

pub struct HeartbeatStore {
    output_root: PathBuf,
    session_writers: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeartbeatArtifactSummary {
    pub rows: u32,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeartbeatReconcileState {
    Absent,
    Verified,
    Rebuilt,
    Orphaned,
}

#[derive(Serialize)]
struct HeartbeatArtifactRow<'a> {
    t_mono_ns: u64,
    seq: u32,
    collector: &'a str,
}

impl HeartbeatStore {
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
    ) -> anyhow::Result<HeartbeatArtifactSummary> {
        self.with_session_writer(session_id, || {
            let heartbeats = journal.heartbeats_for_session(session_id)?;
            ensure!(
                !heartbeats.is_empty(),
                "cannot publish an empty heartbeat artifact"
            );
            let bytes = canonical_bytes(session_id, &heartbeats)?;
            replace_atomic(&self.heartbeat_path(session_id)?, &bytes)?;
            Ok(HeartbeatArtifactSummary {
                rows: u32::try_from(heartbeats.len())
                    .context("heartbeat artifact row count exceeds u32")?,
                sha256: bytes_sha256(&bytes),
            })
        })
    }

    pub fn reconcile_session(
        &self,
        session_id: &str,
        heartbeats: &[HeartbeatAudit],
    ) -> anyhow::Result<HeartbeatReconcileState> {
        self.with_session_writer(session_id, || {
            let path = self.heartbeat_path(session_id)?;
            let staging = staging_path(&path)?;
            if heartbeats.is_empty() {
                return Ok(if path.exists() || staging.exists() {
                    HeartbeatReconcileState::Orphaned
                } else {
                    HeartbeatReconcileState::Absent
                });
            }

            let expected = canonical_bytes(session_id, heartbeats)?;
            if !staging.exists() && fs::read(&path).ok().as_deref() == Some(expected.as_slice()) {
                return Ok(HeartbeatReconcileState::Verified);
            }
            replace_atomic(&path, &expected)?;
            Ok(HeartbeatReconcileState::Rebuilt)
        })
    }

    fn with_session_writer<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let writer = self.session_writer(session_id)?;
        let _writer = writer.lock();
        operation()
    }

    fn session_writer(&self, session_id: &str) -> anyhow::Result<Arc<Mutex<()>>> {
        ensure_safe_component(session_id, "heartbeat session id")?;
        Ok(self
            .session_writers
            .lock()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    fn heartbeat_path(&self, session_id: &str) -> anyhow::Result<PathBuf> {
        ensure_safe_component(session_id, "heartbeat session id")?;
        Ok(self
            .output_root
            .join(session_id)
            .join(HEARTBEAT_ARTIFACT_NAME))
    }
}

fn canonical_bytes(session_id: &str, heartbeats: &[HeartbeatAudit]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut previous_ns = None;
    for (expected_seq, heartbeat) in heartbeats.iter().enumerate() {
        ensure!(
            heartbeat.session_id == session_id,
            "heartbeat artifact contains a different session identity"
        );
        ensure!(
            usize::try_from(heartbeat.seq).ok() == Some(expected_seq),
            "heartbeat artifact sequence is not contiguous"
        );
        ensure!(
            heartbeat.collector == "ok",
            "heartbeat artifact contains an unsupported collector state"
        );
        let t_mono_ns = qpc_to_monotonic_ns(heartbeat.qpc, heartbeat.qpc_frequency)?;
        if let Some(previous_ns) = previous_ns {
            ensure!(
                t_mono_ns > previous_ns,
                "heartbeat monotonic timestamp is not strictly increasing"
            );
        }
        previous_ns = Some(t_mono_ns);
        serde_json::to_writer(
            &mut bytes,
            &HeartbeatArtifactRow {
                t_mono_ns,
                seq: heartbeat.seq,
                collector: &heartbeat.collector,
            },
        )?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn replace_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("heartbeat artifact has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create heartbeat artifact directory {}",
            parent.display()
        )
    })?;
    let staging = staging_path(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staging)
        .with_context(|| {
            format!(
                "failed to open heartbeat staging file {}",
                staging.display()
            )
        })?;
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
            "failed to atomically publish heartbeat artifact {}",
            path.display()
        )
    })?;
    Ok(())
}

fn staging_path(path: &Path) -> anyhow::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("heartbeat artifact path has no UTF-8 file name")?;
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

    fn heartbeat(seq: u32, qpc: i64) -> HeartbeatAudit {
        HeartbeatAudit {
            heartbeat_id: format!("session-1:h{seq:06}"),
            session_id: "session-1".into(),
            run_id: "r0000".into(),
            seq,
            qpc,
            qpc_frequency: 10_000_000,
            collector: "ok".into(),
        }
    }

    #[test]
    fn canonical_heartbeat_artifact_uses_the_minimal_external_schema() {
        let bytes = canonical_bytes(
            "session-1",
            &[heartbeat(0, 10_000_000), heartbeat(1, 20_000_000)],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            concat!(
                "{\"t_mono_ns\":1000000000,\"seq\":0,\"collector\":\"ok\"}\n",
                "{\"t_mono_ns\":2000000000,\"seq\":1,\"collector\":\"ok\"}\n"
            )
        );
    }

    #[test]
    fn heartbeat_artifact_rejects_sequence_and_time_regressions() {
        assert!(canonical_bytes("session-1", &[heartbeat(1, 10_000_000)]).is_err());
        assert!(
            canonical_bytes(
                "session-1",
                &[heartbeat(0, 20_000_000), heartbeat(1, 10_000_000)]
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_replaces_partial_heartbeat_projection_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let store = HeartbeatStore::new(dir.path().to_path_buf());
        let session_dir = dir.path().join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join(HEARTBEAT_ARTIFACT_NAME), b"partial").unwrap();
        let rows = [heartbeat(0, 10_000_000), heartbeat(1, 20_000_000)];

        assert_eq!(
            store.reconcile_session("session-1", &rows).unwrap(),
            HeartbeatReconcileState::Rebuilt
        );
        assert_eq!(
            store.reconcile_session("session-1", &rows).unwrap(),
            HeartbeatReconcileState::Verified
        );
        assert_eq!(
            fs::read(session_dir.join(HEARTBEAT_ARTIFACT_NAME)).unwrap(),
            canonical_bytes("session-1", &rows).unwrap()
        );
    }
}
