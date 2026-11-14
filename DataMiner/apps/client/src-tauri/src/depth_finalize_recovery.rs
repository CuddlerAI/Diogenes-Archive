//! One-shot recovery entry point for a previously Flushed Depth run.
//!
//! The normal recorder calls the same finalizer before publishing terminal run
//! state. This wrapper is deliberately limited to already-completed historical
//! runs: it can attach missing Depth artifacts, but it cannot manufacture a
//! terminal `RecordingOutcome` for a faulted run.

use std::path::PathBuf;

use serde::Serialize;

use crate::services::depth_finalize::{
    FinalizeRecordedDepthRun, finalize_recorded_depth_run, recover_confirmed_flush,
};
use crate::services::journal::SessionJournal;

#[derive(Debug, Clone)]
pub struct RecoveryOptions {
    pub journal_path: PathBuf,
    pub journal_backup_path: PathBuf,
    pub run_directory: PathBuf,
    pub session_id: String,
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryReport {
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
    pub segment_count: usize,
    pub upload_zip_count: usize,
    pub ineligible_segment_count: usize,
    pub journal_backup_path: PathBuf,
}

pub fn run(options: &RecoveryOptions) -> Result<RecoveryReport, String> {
    if options.journal_backup_path.exists() {
        return Err(format!(
            "journal backup already exists: {}",
            options.journal_backup_path.display()
        ));
    }
    let backup_connection = rusqlite::Connection::open(&options.journal_path)
        .map_err(|error| format!("open journal for backup failed: {error}"))?;
    backup_connection
        .execute(
            "VACUUM INTO ?1",
            rusqlite::params![options.journal_backup_path.to_string_lossy().as_ref()],
        )
        .map_err(|error| format!("create consistent journal backup failed: {error}"))?;
    drop(backup_connection);
    let journal = SessionJournal::open(&options.journal_path)
        .map_err(|error| format!("open journal failed: {error:#}"))?;
    journal
        .ensure_completed_depth_recovery_source(&options.session_id, &options.run_id)
        .map_err(|error| format!("unsupported Depth recovery source: {error:#}"))?;
    let probe_root = options.run_directory.join("depth-lab");
    let (identity, flushed) =
        recover_confirmed_flush(&probe_root, &options.session_id, &options.run_id)?;
    let request = FinalizeRecordedDepthRun {
        journal: &journal,
        session_id: &options.session_id,
        run_id: &options.run_id,
        generation: identity.generation,
        run_directory: &options.run_directory,
        probe_root: &probe_root,
        flushed: &flushed,
    };
    let published = match finalize_recorded_depth_run(&request) {
        Ok(published) => published,
        Err(error) => {
            let reject_error = journal
                .reject_depth_finalization(&options.session_id, &options.run_id)
                .err();
            return Err(format!(
                "DEPTH_FINALIZE_FAILED: {error}{}",
                reject_error.map_or_else(String::new, |reject_error| format!(
                    "; marking blocked uploads failed: {reject_error:#}"
                ))
            ));
        }
    };
    let upload_zip_count = published
        .iter()
        .filter(|segment| segment.candidate_eligible)
        .count();
    Ok(RecoveryReport {
        session_id: options.session_id.clone(),
        run_id: options.run_id.clone(),
        generation: identity.generation,
        segment_count: published.len(),
        upload_zip_count,
        ineligible_segment_count: published.len() - upload_zip_count,
        journal_backup_path: options.journal_backup_path.clone(),
    })
}
