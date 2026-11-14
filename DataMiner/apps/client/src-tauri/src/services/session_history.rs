use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use serde::Serialize;

use crate::services::journal::{PendingSessionDeletion, SessionJournal};

use super::data_model::DATA_MODEL_ROOT_NAME;

const DELETION_ROOT_NAME: &str = "deletion-staging";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
static DELETION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeletionResult {
    pub session_id: String,
    pub deletion_id: String,
    pub artifacts_removed: bool,
}

pub fn recover_pending_deletions(
    journal: &SessionJournal,
    artifact_root: &Path,
    output_root: &Path,
) -> anyhow::Result<u32> {
    let pending = journal.pending_session_deletions()?;
    for deletion in &pending {
        validate_pending(deletion)?;
        if deletion.state == "cleanup_pending" {
            cleanup_staging(journal, artifact_root, deletion)?;
        } else {
            complete_staged_deletion(journal, artifact_root, output_root, deletion)?;
        }
    }
    u32::try_from(pending.len()).context("pending deletion count exceeds u32")
}

pub fn delete_session(
    journal: &SessionJournal,
    artifact_root: &Path,
    output_root: &Path,
    session_id: &str,
    confirmation: &str,
) -> anyhow::Result<SessionDeletionResult> {
    validate_session_id(session_id)?;
    ensure!(
        confirmation == session_id,
        "session deletion confirmation does not match the selected session"
    );
    let deletion_id = next_deletion_id()?;
    let relative = format!("{DELETION_ROOT_NAME}/{deletion_id}");
    journal.prepare_session_deletion(&deletion_id, session_id, &relative)?;
    let pending = PendingSessionDeletion {
        deletion_id: deletion_id.clone(),
        session_id: session_id.into(),
        state: "prepared".into(),
        staged_relative_path: relative,
    };
    if let Err(error) = stage_sources(artifact_root, output_root, &pending) {
        rollback_and_cancel(journal, artifact_root, output_root, &pending)?;
        return Err(error).context("session artifact staging failed");
    }
    if let Err(error) = journal.mark_session_deletion_staged(&deletion_id) {
        rollback_and_cancel(journal, artifact_root, output_root, &pending)?;
        return Err(error).context("session deletion staging journal failed");
    }
    if let Err(error) = journal.commit_session_deletion(&deletion_id, session_id) {
        rollback_and_cancel(journal, artifact_root, output_root, &pending)?;
        return Err(error).context("session deletion journal commit failed");
    }
    cleanup_staging(journal, artifact_root, &pending)
        .context("session was removed from history but staged file cleanup is pending")?;
    Ok(SessionDeletionResult {
        session_id: session_id.into(),
        deletion_id,
        artifacts_removed: true,
    })
}

pub fn session_directory(
    artifact_root: &Path,
    output_root: &Path,
    session_id: &str,
) -> anyhow::Result<PathBuf> {
    validate_session_id(session_id)?;
    let artifact = output_root.join(session_id);
    if artifact.is_dir() {
        return ensure_child_directory(output_root, &artifact);
    }
    let quarantine_root = artifact_root.join("quarantine");
    let quarantine = quarantine_root.join(session_id);
    ensure!(
        quarantine.is_dir(),
        "session has no local artifact or quarantine directory"
    );
    ensure_child_directory(&quarantine_root, &quarantine)
}

fn complete_staged_deletion(
    journal: &SessionJournal,
    artifact_root: &Path,
    output_root: &Path,
    deletion: &PendingSessionDeletion,
) -> anyhow::Result<()> {
    stage_sources(artifact_root, output_root, deletion)?;
    journal.mark_session_deletion_staged(&deletion.deletion_id)?;
    journal.commit_session_deletion(&deletion.deletion_id, &deletion.session_id)?;
    cleanup_staging(journal, artifact_root, deletion)
}

fn rollback_and_cancel(
    journal: &SessionJournal,
    artifact_root: &Path,
    output_root: &Path,
    deletion: &PendingSessionDeletion,
) -> anyhow::Result<()> {
    rollback_staging(artifact_root, output_root, deletion)?;
    journal.cancel_session_deletion(&deletion.deletion_id, "DELETE_STAGING_FAILED")
}

fn stage_sources(
    artifact_root: &Path,
    output_root: &Path,
    deletion: &PendingSessionDeletion,
) -> anyhow::Result<()> {
    let staging = validated_staging_path(artifact_root, deletion)?;
    ensure!(
        !staging.is_file(),
        "session deletion staging path is a file"
    );
    fs::create_dir_all(&staging)?;
    let artifact_source = output_root.join(&deletion.session_id);
    let artifact_target = staging.join("artifact");
    move_if_present(&artifact_source, &artifact_target)?;
    let quarantine_source = artifact_root.join("quarantine").join(&deletion.session_id);
    let quarantine_target = staging.join("quarantine");
    move_if_present(&quarantine_source, &quarantine_target)?;
    let data_model_root = artifact_root.join(DATA_MODEL_ROOT_NAME);
    move_if_present(
        &data_model_root.join("work").join(&deletion.session_id),
        &staging.join("data-model-work"),
    )?;
    move_if_present(
        &data_model_root.join("ready").join(&deletion.session_id),
        &staging.join("data-model-ready"),
    )?;
    Ok(())
}

fn rollback_staging(
    artifact_root: &Path,
    output_root: &Path,
    deletion: &PendingSessionDeletion,
) -> anyhow::Result<()> {
    let staging = validated_staging_path(artifact_root, deletion)?;
    if !staging.is_dir() {
        return Ok(());
    }
    restore_if_present(
        &staging.join("artifact"),
        &output_root.join(&deletion.session_id),
    )?;
    restore_if_present(
        &staging.join("quarantine"),
        &artifact_root.join("quarantine").join(&deletion.session_id),
    )?;
    let data_model_root = artifact_root.join(DATA_MODEL_ROOT_NAME);
    restore_if_present(
        &staging.join("data-model-work"),
        &data_model_root.join("work").join(&deletion.session_id),
    )?;
    restore_if_present(
        &staging.join("data-model-ready"),
        &data_model_root.join("ready").join(&deletion.session_id),
    )?;
    if staging.is_dir() && fs::read_dir(&staging)?.next().is_none() {
        fs::remove_dir(&staging)?;
    }
    Ok(())
}

fn cleanup_staging(
    journal: &SessionJournal,
    artifact_root: &Path,
    deletion: &PendingSessionDeletion,
) -> anyhow::Result<()> {
    let staging = validated_staging_path(artifact_root, deletion)?;
    if staging.exists() {
        ensure!(
            staging.is_dir(),
            "session deletion staging path is not a directory"
        );
        ensure_tree_has_no_reparse_points(&staging)?;
        fs::remove_dir_all(&staging).with_context(|| {
            format!(
                "failed to remove staged session deletion {}",
                deletion.deletion_id
            )
        })?;
    }
    journal.finish_session_deletion(&deletion.deletion_id)
}

fn move_if_present(source: &Path, target: &Path) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    ensure!(
        source.is_dir(),
        "session deletion source is not a directory"
    );
    ensure_tree_has_no_reparse_points(source)?;
    ensure!(
        !target.exists(),
        "session deletion staging target already exists"
    );
    fs::rename(source, target).with_context(|| {
        format!(
            "failed to stage session directory {}",
            source.file_name().unwrap_or_default().to_string_lossy()
        )
    })
}

fn restore_if_present(source: &Path, target: &Path) -> anyhow::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    ensure!(
        !target.exists(),
        "cannot roll back session deletion over an existing directory"
    );
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(source, target)?;
    Ok(())
}

fn ensure_tree_has_no_reparse_points(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "session deletion refuses filesystem reparse points"
    );
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            ensure_tree_has_no_reparse_points(&entry?.path())?;
        }
    }
    Ok(())
}

fn validated_staging_path(
    artifact_root: &Path,
    deletion: &PendingSessionDeletion,
) -> anyhow::Result<PathBuf> {
    validate_session_id(&deletion.session_id)?;
    ensure!(
        !deletion.deletion_id.is_empty()
            && deletion
                .deletion_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid session deletion id"
    );
    let relative = Path::new(&deletion.staged_relative_path);
    ensure!(
        !relative.is_absolute()
            && !relative
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::RootDir)),
        "invalid session deletion staging path"
    );
    let expected = Path::new(DELETION_ROOT_NAME).join(&deletion.deletion_id);
    ensure!(
        relative == expected,
        "session deletion staging identity mismatch"
    );
    Ok(artifact_root.join(relative))
}

fn ensure_child_directory(root: &Path, directory: &Path) -> anyhow::Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let directory = fs::canonicalize(directory)?;
    ensure!(
        directory.starts_with(&root) && directory != root,
        "session directory escaped its configured root"
    );
    Ok(directory)
}

fn validate_pending(deletion: &PendingSessionDeletion) -> anyhow::Result<()> {
    ensure!(
        matches!(
            deletion.state.as_str(),
            "prepared" | "staged" | "cleanup_pending"
        ),
        "invalid pending session deletion state"
    );
    Ok(())
}

fn validate_session_id(session_id: &str) -> anyhow::Result<()> {
    ensure!(
        !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid session id"
    );
    Ok(())
}

fn next_deletion_id() -> anyhow::Result<String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_millis();
    Ok(format!(
        "delete-{millis}-{:04}",
        DELETION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_core::mapping::MappingSnapshot;
    use capture_core::profile::CaptureProfile;
    use rusqlite::Connection;

    use crate::services::journal::TestRunStart;

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

    fn journal_with_session(root: &Path) -> SessionJournal {
        let journal = SessionJournal::open(root.join("journal.sqlite3")).unwrap();
        let profile = profile();
        let mapping = MappingSnapshot::from_slice(MAPPING).unwrap();
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
        journal
    }

    #[test]
    fn rejects_path_shaped_session_ids() {
        for invalid in [
            "",
            "../session",
            "session/name",
            r"session\name",
            "session.name",
        ] {
            assert!(validate_session_id(invalid).is_err());
        }
        assert!(validate_session_id("dev-1234_0000").is_ok());
    }

    #[test]
    fn active_session_delete_is_rejected_without_moving_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path();
        let output = app_data.join("non-trainable-sessions");
        let session_dir = output.join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("evidence.bin"), b"keep").unwrap();
        let journal = journal_with_session(app_data);

        let error =
            delete_session(&journal, app_data, &output, "session-1", "session-1").unwrap_err();

        assert!(error.to_string().contains("cannot be deleted"));
        assert!(session_dir.join("evidence.bin").is_file());
        assert!(journal.session_exists("session-1").unwrap());
    }

    #[test]
    fn rejected_delete_preserves_data_model_job_and_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path();
        let output = app_data.join("non-trainable-sessions");
        let session_dir = output.join("session-1");
        let data_model_work = app_data.join(DATA_MODEL_ROOT_NAME).join("work/session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&data_model_work).unwrap();
        fs::write(session_dir.join("ready.bin"), b"ready").unwrap();
        fs::write(data_model_work.join("chunk.tmp"), b"work").unwrap();
        let journal = journal_with_session(app_data);
        journal.fail_run("session-1", "r0000", "TEST").unwrap();
        let job_before = journal.data_model_job("session-1").unwrap();

        let error =
            delete_session(&journal, app_data, &output, "session-1", "wrong-session").unwrap_err();

        assert!(error.to_string().contains("confirmation"));
        assert_eq!(journal.data_model_job("session-1").unwrap(), job_before);
        assert!(session_dir.join("ready.bin").is_file());
        assert!(data_model_work.join("chunk.tmp").is_file());
        assert!(journal.pending_session_deletions().unwrap().is_empty());
    }

    #[test]
    fn terminal_session_delete_removes_journal_artifact_and_quarantine() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path();
        let output = app_data.join("non-trainable-sessions");
        let session_dir = output.join("session-1");
        let quarantine_dir = app_data.join("quarantine/session-1");
        let data_model_work = app_data.join(DATA_MODEL_ROOT_NAME).join("work/session-1");
        let data_model_ready = app_data.join(DATA_MODEL_ROOT_NAME).join("ready/session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&quarantine_dir).unwrap();
        fs::create_dir_all(&data_model_work).unwrap();
        fs::create_dir_all(&data_model_ready).unwrap();
        fs::write(session_dir.join("ready.bin"), b"ready").unwrap();
        fs::write(quarantine_dir.join("failed.bin"), b"failed").unwrap();
        fs::write(data_model_work.join("chunk.tmp"), b"work").unwrap();
        fs::write(data_model_ready.join("metadata.json"), b"ready").unwrap();
        let journal = journal_with_session(app_data);
        journal.fail_run("session-1", "r0000", "TEST").unwrap();

        let result = delete_session(&journal, app_data, &output, "session-1", "session-1").unwrap();

        assert!(result.artifacts_removed);
        assert!(!journal.session_exists("session-1").unwrap());
        assert!(journal.data_model_job("session-1").unwrap().is_none());
        assert!(!session_dir.exists());
        assert!(!quarantine_dir.exists());
        assert!(!data_model_work.exists());
        assert!(!data_model_ready.exists());
        assert!(journal.pending_session_deletions().unwrap().is_empty());
        let audit_session: String = Connection::open(app_data.join("journal.sqlite3"))
            .unwrap()
            .query_row("SELECT session_id FROM session_deletions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(audit_session.starts_with("deleted:"));
    }

    #[test]
    fn startup_finishes_cleanup_pending_delete_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path();
        let output = app_data.join("non-trainable-sessions");
        let session_dir = output.join("session-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("ready.bin"), b"ready").unwrap();
        let journal = journal_with_session(app_data);
        journal.fail_run("session-1", "r0000", "TEST").unwrap();
        let pending = PendingSessionDeletion {
            deletion_id: "delete-test-0001".into(),
            session_id: "session-1".into(),
            state: "prepared".into(),
            staged_relative_path: "deletion-staging/delete-test-0001".into(),
        };
        journal
            .prepare_session_deletion(
                &pending.deletion_id,
                &pending.session_id,
                &pending.staged_relative_path,
            )
            .unwrap();
        stage_sources(app_data, &output, &pending).unwrap();
        journal
            .mark_session_deletion_staged(&pending.deletion_id)
            .unwrap();
        journal
            .commit_session_deletion(&pending.deletion_id, &pending.session_id)
            .unwrap();
        assert!(
            app_data
                .join(&pending.staged_relative_path)
                .join("artifact/ready.bin")
                .is_file()
        );

        assert_eq!(
            recover_pending_deletions(&journal, app_data, &output).unwrap(),
            1
        );
        assert_eq!(
            recover_pending_deletions(&journal, app_data, &output).unwrap(),
            0
        );
        assert!(!app_data.join(&pending.staged_relative_path).exists());
        assert!(!journal.session_exists("session-1").unwrap());
    }
}
