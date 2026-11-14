use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::Arc;

use anyhow::{Context, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use md5::Md5;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
use windows::core::PCWSTR;

use crate::services::journal::{
    SessionArtifactKind, SessionArtifactSource, SessionHistoryDetail, SessionJournal,
};

const STAGING_DIRECTORY: &str = ".session-artifact-upload.tmp";
const BUNDLE_DIRECTORY: &str = "session";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

pub fn freeze_next_eligible_session(
    journal: &Arc<SessionJournal>,
    raw_root: &Path,
    app_data_root: &Path,
) -> anyhow::Result<bool> {
    let Some(session_id) = journal.next_session_artifact_freeze_candidate()? else {
        return Ok(false);
    };
    freeze_and_enqueue(journal, raw_root, app_data_root, &session_id)?;
    Ok(true)
}

pub fn freeze_and_enqueue(
    journal: &SessionJournal,
    raw_root: &Path,
    app_data_root: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    ensure_safe_component(session_id, "session id")?;
    let detail = journal.session_detail(session_id)?;
    ensure!(
        detail.session.state == "completed",
        "session artifact bundle cannot freeze from state {}",
        detail.session.state
    );
    // 视角标定证据（第 9 种，docs/46 §2.4）：journal 引用驱动，从 app-data
    // 复制进不可变 bundle，复制前按会话绑定指纹逐字节复核。
    let calibration = match journal.session_view_calibration_evidence(session_id)? {
        Some(evidence) => {
            let bytes = read_regular_file_no_reparse(&app_data_root.join(&evidence.file_path))
                .context(
                    "journal references a view calibration but its evidence file is unreadable",
                )?;
            ensure!(
                sha256_bytes(&bytes) == evidence.file_sha256,
                "input-calibration.json no longer matches the session-bound digest"
            );
            Some(bytes)
        }
        None => None,
    };
    let sources = publish_bundle(raw_root, session_id, &detail, calibration)?;
    journal.enqueue_session_artifacts(session_id, &sources)
}

fn publish_bundle(
    raw_root: &Path,
    session_id: &str,
    detail: &SessionHistoryDetail,
    calibration: Option<Vec<u8>>,
) -> anyhow::Result<Vec<SessionArtifactSource>> {
    let canonical_root = raw_root
        .canonicalize()
        .with_context(|| format!("RAW root {} is unavailable", raw_root.display()))?;
    let session_root = raw_root.join(session_id);
    let canonical_session = session_root
        .canonicalize()
        .with_context(|| format!("RAW session {session_id} is unavailable"))?;
    ensure!(
        canonical_session.starts_with(&canonical_root),
        "session artifact bundle escaped the RAW root"
    );
    ensure_no_reparse_point(&session_root)?;

    let root_manifest = read_regular_file_no_reparse(&session_root.join("session.json"))
        .context("failed to read the immutable session manifest")?;
    let mut manifest: Value =
        serde_json::from_slice(&root_manifest).context("immutable session manifest is invalid")?;
    ensure!(
        manifest.get("session_id").and_then(Value::as_str) == Some(session_id)
            && manifest.get("non_trainable").and_then(Value::as_bool) == Some(true)
            && manifest
                .get("production_certified")
                .and_then(Value::as_bool)
                == Some(false),
        "session artifact freeze refuses non-local or trainable session manifests"
    );
    let manifest_object = manifest
        .as_object_mut()
        .context("immutable session manifest is not a JSON object")?;
    manifest_object.insert(
        "artifact_schema_version".into(),
        Value::String("dm-session-artifact-1".into()),
    );
    manifest_object.insert(
        "finalization".into(),
        json!({
            "state": detail.session.state,
            "ended_at_utc": detail.session.ended_at_utc,
            "stop_reason": detail.session.stop_reason,
            "summary": detail.session,
            "runs": detail.runs,
            "segments": detail.segments,
        }),
    );
    let mut final_manifest =
        serde_json::to_vec_pretty(&manifest).context("failed to serialize session artifact")?;
    final_manifest.push(b'\n');

    let expected_hashes = HashMap::from([
        (
            SessionArtifactKind::Catalog,
            detail.session.catalog_hash.as_str(),
        ),
        (
            SessionArtifactKind::DeviceProfile,
            detail.session.device_profile_hash.as_str(),
        ),
        (
            SessionArtifactKind::RecordingProfile,
            detail.session.recording_profile_hash.as_str(),
        ),
        (
            SessionArtifactKind::GameProfile,
            detail.session.game_profile_hash.as_str(),
        ),
        (
            SessionArtifactKind::Mapping,
            detail.session.mapping_hash.as_str(),
        ),
    ]);
    let mut bytes = vec![(SessionArtifactKind::Session, final_manifest)];
    // 断代前这里是 capture / settings 两条，批量改名把它们塌成了同一个 kind
    // （docs/57 §3.1）。同一个 kind 发两次会在临时目录里撞同一个文件名，报出来的是
    // 「文件已存在」，看着完全不像重命名的后果。
    for kind in [
        SessionArtifactKind::Catalog,
        SessionArtifactKind::DeviceProfile,
        SessionArtifactKind::RecordingProfile,
        SessionArtifactKind::GameProfile,
        SessionArtifactKind::Mapping,
    ] {
        // These root snapshots were originally written from CatalogSelection's
        // exact source bytes. Copying them preserves the server-registered hash;
        // parsing and reserializing here would break that identity.
        let source = read_regular_file_no_reparse(&session_root.join(kind.file_name()))
            .with_context(|| format!("failed to read exact-byte {}", kind.file_name()))?;
        ensure!(
            sha256_bytes(&source) == expected_hashes[&kind],
            "exact-byte {} no longer matches the journal identity",
            kind.file_name()
        );
        bytes.push((kind, source));
    }

    let markers_path = session_root.join(SessionArtifactKind::Markers.file_name());
    if detail.session.marker_count > 0 {
        let markers = read_regular_file_no_reparse(&markers_path)
            .context("journal has markers but markers.jsonl is missing or unreadable")?;
        ensure!(!markers.is_empty(), "markers.jsonl is unexpectedly empty");
        bytes.push((SessionArtifactKind::Markers, markers));
    } else if markers_path.is_file() {
        let markers = read_regular_file_no_reparse(&markers_path)?;
        if !markers.is_empty() {
            anyhow::bail!("markers.jsonl exists but the journal has no markers");
        }
    }

    // 视角标定证据由 journal 引用驱动、自 app-data 复制（调用方已按会话
    // 绑定指纹复核）；session 根下仍不允许出现未经登记的同名文件——与
    // markers 的反夹带规则同构。
    if let Some(calibration) = calibration {
        bytes.push((SessionArtifactKind::InputCalibration, calibration));
    }
    let calibration_path = session_root.join(SessionArtifactKind::InputCalibration.file_name());
    ensure!(
        !calibration_path.is_file(),
        "input-calibration.json exists in the session root without a journal reference"
    );

    let bundle = session_root.join(BUNDLE_DIRECTORY);
    if bundle.exists() {
        verify_existing_bundle(&bundle, &bytes)?;
    } else {
        let staging = session_root.join(STAGING_DIRECTORY);
        remove_stale_staging(&staging)?;
        fs::create_dir(&staging)
            .with_context(|| format!("failed to create {}", staging.display()))?;
        for (kind, contents) in &bytes {
            write_new_synced(&staging.join(kind.file_name()), contents)?;
        }
        durable_rename(&staging, &bundle)?;
    }

    bytes
        .into_iter()
        .map(|(kind, contents)| {
            Ok(SessionArtifactSource {
                kind,
                relative_path: format!("{session_id}/{BUNDLE_DIRECTORY}/{}", kind.file_name()),
                expected_sha256: sha256_bytes(&contents),
                expected_size: u64::try_from(contents.len())?,
                content_md5: content_md5(&contents),
            })
        })
        .collect()
}

fn verify_existing_bundle(
    bundle: &Path,
    expected: &[(SessionArtifactKind, Vec<u8>)],
) -> anyhow::Result<()> {
    ensure_no_reparse_point(bundle)?;
    ensure!(
        bundle.is_dir(),
        "session artifact bundle is not a directory"
    );
    let mut actual_names = fs::read_dir(bundle)?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("session artifact file name is not UTF-8"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    actual_names.sort();
    let mut expected_names = expected
        .iter()
        .map(|(kind, _)| kind.file_name().to_owned())
        .collect::<Vec<_>>();
    expected_names.sort();
    ensure!(
        actual_names == expected_names,
        "existing session artifact bundle changed its file set"
    );
    for (kind, bytes) in expected {
        ensure!(
            read_regular_file_no_reparse(&bundle.join(kind.file_name()))? == *bytes,
            "existing session artifact {} changed after publication",
            kind.file_name()
        );
    }
    Ok(())
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
        .with_context(|| format!("failed to durable-sync {}", path.display()))
}

fn remove_stale_staging(staging: &Path) -> anyhow::Result<()> {
    if !staging.exists() {
        return Ok(());
    }
    ensure_no_reparse_point(staging)?;
    ensure!(
        staging.is_dir()
            && staging.file_name().and_then(|name| name.to_str()) == Some(STAGING_DIRECTORY),
        "refusing to remove an unexpected session artifact staging path"
    );
    fs::remove_dir_all(staging)
        .with_context(|| format!("failed to clear stale {}", staging.display()))
}

fn ensure_no_reparse_point(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "session artifact operation refuses filesystem reparse points"
    );
    Ok(())
}

fn read_regular_file_no_reparse(path: &Path) -> anyhow::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "{} is not a regular non-reparse file",
        path.display()
    );
    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn ensure_safe_component(value: &str, label: &str) -> anyhow::Result<()> {
    let mut components = Path::new(value).components();
    ensure!(
        matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none()
            && !value.is_empty(),
        "{label} is not a safe path component"
    );
    Ok(())
}

fn durable_rename(source: &Path, destination: &Path) -> anyhow::Result<()> {
    ensure!(
        !destination.exists(),
        "refusing to overwrite immutable bundle"
    );
    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source_wide.as_ptr()),
            PCWSTR::from_raw(destination_wide.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("MoveFileExW(MOVEFILE_WRITE_THROUGH) failed")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn content_md5(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(Md5::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::journal::{
        SessionHistoryDetail, SessionHistoryRun, SessionHistorySummary,
    };

    fn detail(hashes: &HashMap<SessionArtifactKind, String>) -> SessionHistoryDetail {
        SessionHistoryDetail {
            session: SessionHistorySummary {
                session_id: "session-1".into(),
                state: "completed".into(),
                game_id: "synthetic".into(),
                recording_profile_id: "recording".into(),
                mapping_id: "mapping".into(),
                mapping_version: "1".into(),
                session_contract: "dm-local-dev".into(),
                catalog_id: "catalog".into(),
                catalog_version: "1".into(),
                catalog_hash: hashes[&SessionArtifactKind::Catalog].clone(),
                device_profile_id: "device".into(),
                device_profile_version: "1".into(),
                device_profile_hash: hashes[&SessionArtifactKind::DeviceProfile].clone(),
                recording_profile_version: "1".into(),
                recording_profile_hash: hashes[&SessionArtifactKind::RecordingProfile].clone(),
                recording_profile_hash_kind: "exact_source_bytes_sha256".into(),
                game_profile_version: "1".into(),
                game_profile_hash: hashes[&SessionArtifactKind::GameProfile].clone(),
                mapping_hash: hashes[&SessionArtifactKind::Mapping].clone(),
                started_at_utc: "2026-07-26T00:00:00Z".into(),
                ended_at_utc: Some("2026-07-26T00:00:10Z".into()),
                stop_reason: Some("manual".into()),
                error_code: None,
                run_count: 1,
                segment_count: 1,
                ready_segments: 1,
                corrupt_segments: 0,
                quarantined_segments: 0,
                frame_count: 300,
                input_event_count: 4,
                marker_count: 0,
                video_bytes: 1234,
                upload_requested: false,
                upload_requested_at_utc: None,
                upload_job_segments: 0,
                performance_validation_only: false,
                upload_requestable: true,
                upload_pending_segments: 0,
                upload_running_segments: 0,
                upload_retry_wait_segments: 0,
                upload_stored_segments: 0,
                uploaded_segments: 0,
                upload_verified_segments: 0,
                upload_rejected_segments: 0,
                upload_last_error_code: None,
                upload_last_http_status: None,
                upload_last_server_code: None,
                upload_last_server_message: None,
                upload_last_request_id: None,
                reclaimable: false,
            },
            runs: vec![SessionHistoryRun {
                run_id: "session-1:r0000".into(),
                run_name: "r0000".into(),
                seq: 0,
                state: "completed".into(),
                frame_count: Some(300),
                ready_segments: Some(1),
                input_event_count: Some(4),
                input_queue_overflows: Some(0),
                stop_reason: Some("manual".into()),
                error_code: None,
                started_at_utc: Some("2026-07-26T00:00:00Z".into()),
                ended_at_utc: Some("2026-07-26T00:00:10Z".into()),
            }],
            segments: vec![],
            markers: vec![],
            preflights: vec![],
        }
    }

    #[test]
    fn exact_byte_snapshots_are_frozen_without_reserialization_and_replay_is_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let session_root = temp.path().join("session-1");
        fs::create_dir(&session_root).unwrap();
        fs::write(
            session_root.join("session.json"),
            br#"{"schema_version":"dm-local-dev-session-2","session_id":"session-1","non_trainable":true,"production_certified":false}"#,
        )
        .unwrap();
        let exact = HashMap::from([
            (
                SessionArtifactKind::Catalog,
                b"{ \"catalog\": 1 }\r\n".to_vec(),
            ),
            (
                SessionArtifactKind::DeviceProfile,
                b"{\"device\":1}\n".to_vec(),
            ),
            // 合并成一册之后这里只剩一条 recording profile（docs/57 §3.1）。断代前是
            // capture 与 settings 各一条，批量改名把两条塌成了同一个 kind——同一个
            // kind 出现两次会在发布时撞同一个临时路径。刻意保留怪异空白：这一组夹具
            // 要证明的就是「原样字节、不重新序列化」。
            (
                SessionArtifactKind::RecordingProfile,
                b"{ \"recording\" : 1 }".to_vec(),
            ),
            (
                SessionArtifactKind::GameProfile,
                b"{\"game\":1}\r\n".to_vec(),
            ),
            (
                SessionArtifactKind::Mapping,
                b"{ \"mapping\": 1 }\n".to_vec(),
            ),
        ]);
        let hashes = exact
            .iter()
            .map(|(kind, bytes)| (*kind, sha256_bytes(bytes)))
            .collect::<HashMap<_, _>>();
        for (kind, bytes) in &exact {
            fs::write(session_root.join(kind.file_name()), bytes).unwrap();
        }
        let detail = detail(&hashes);

        let first = publish_bundle(temp.path(), "session-1", &detail, None).unwrap();
        assert_eq!(first.len(), 6);
        for (kind, bytes) in &exact {
            assert_eq!(
                fs::read(session_root.join("session").join(kind.file_name())).unwrap(),
                *bytes
            );
        }
        let manifest: Value =
            serde_json::from_slice(&fs::read(session_root.join("session/session.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["artifact_schema_version"], "dm-session-artifact-1");
        assert_eq!(manifest["finalization"]["summary"]["runCount"], 1);

        let second = publish_bundle(temp.path(), "session-1", &detail, None).unwrap();
        assert_eq!(first, second);
        fs::write(
            session_root.join("session/catalog.json"),
            b"{\"tampered\":true}",
        )
        .unwrap();
        let error = publish_bundle(temp.path(), "session-1", &detail, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("changed after publication"),
            "unexpected immutable replay failure: {error:#}"
        );
    }
}
