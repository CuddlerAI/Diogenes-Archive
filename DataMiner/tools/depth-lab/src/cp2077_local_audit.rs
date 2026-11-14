//! Read-only post-stop accounting for the CP2077 local diagnostic lane.
//! Reuses ordinary exact pairing and camera/action joins. New exact-build runs also verify the
//! formal player stream; historical camera-only evidence remains replayable. This lane never
//! produces a candidate segment, archive, or upload/journal decision.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::camera_action::{
    CameraPoseProvenance, CameraPoseRow, FullRateCameraActionJoinInputs, IdentifiedRows,
    PlayerPoseRow, join_camera_action_full_rate, validate_player_pose_row,
};
use crate::capture_timing::{DepthIndexEntry, INDEX_SCHEMA_V6, ProjectionScope};
use crate::pipeline::{DepthIndexRow, E2eReport};
use crate::probe_control::FlushSummary;
use crate::{CameraMode, StreamIdentity};

pub const SCHEMA: &str = "cp2077-local-capture-audit/v1";
const MAX_ROWS: usize = 4096;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct FileEvidence {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LocalCaptureAudit {
    pub schema: String,
    pub identity: StreamIdentity,
    pub non_trainable: bool,
    pub deliverable: bool,
    pub player_pose_captured: bool,
    pub mp4_watermark_redecoded: bool,
    pub depth_pixels_decoded: bool,
    pub rgb_frames: usize,
    pub distinct_selected_presents: usize,
    pub camera_rows: usize,
    pub depth_rows: usize,
    pub joined_action_camera_rows: usize,
    /// The shared report retains its even-CFR denominator. All RGB frames are
    /// checked independently against the complete selected-Present set above.
    pub even_cfr_pairing: E2eReport,
    pub files: Vec<FileEvidence>,
}

pub struct AuditRequest<'a> {
    pub identity: &'a StreamIdentity,
    pub first_present_seq: u64,
    pub summary: &'a FlushSummary,
    /// Exact journal-selected files, in segment order; no directory discovery.
    pub frame_paths: &'a [PathBuf],
    pub probe_root: &'a Path,
}

/// Offline callers may replay facts from a failed run, but success here does
/// not authenticate its Flushed ACK or change the original terminal state.
pub fn audit(request: &AuditRequest<'_>) -> Result<LocalCaptureAudit, String> {
    let summary = request.summary;
    summary.validate().map_err(|error| error.to_string())?;
    if summary.writer_bypass {
        return Err("CP2077 local audit requires the active probe writer".into());
    }
    if summary.camera_diagnostic_row_count != 0 || !summary.errors.is_empty() {
        return Err("probe retained camera diagnostics or flush errors".into());
    }
    let camera_path = request.probe_root.join("camera-pose-index.jsonl");
    let depth_path = request.probe_root.join("depth-index.jsonl");
    let player_path = request.probe_root.join("player-pose-index.jsonl");
    let mut index_paths = vec![&camera_path, &depth_path];
    if summary.player_pose_row_count.is_some() {
        index_paths.push(&player_path);
    }
    let mut files = request
        .frame_paths
        .iter()
        .chain(index_paths)
        .map(|path| fingerprint(path))
        .collect::<Result<Vec<_>, _>>()?;
    let binding = ready_binding(request, &mut files)?;
    let mut frames = Vec::new();
    let mut actions = Vec::new();
    for path in request.frame_paths {
        frames.extend(crate::final_action_io::read_recorder_frame_rows(path)?);
        actions.extend(crate::final_action_io::read_action_frame_rows(path)?);
        if frames.len() > MAX_ROWS {
            return Err("local audit frame budget exceeded".into());
        }
    }
    if frames.is_empty() || actions.len() != frames.len() {
        return Err("local audit requires complete RGB/action rows".into());
    }
    let camera: Vec<CameraPoseRow> = read_index(&camera_path)?;
    let depth: Vec<DepthIndexEntry> = read_index(&depth_path)?;
    let player: Option<Vec<PlayerPoseRow>> = summary
        .player_pose_row_count
        .map(|_| read_index(&player_path))
        .transpose()?;
    for pose in &camera {
        if !matches!(&pose.provenance, CameraPoseProvenance::ResolvedD3d12CameraHook { binding: row_binding, .. } if row_binding.as_ref() == &binding)
        {
            return Err("camera row binding differs from the Ready snapshot".into());
        }
    }
    if camera.first().map(|row| row.present_seq) != Some(request.first_present_seq)
        || camera.last().map(|row| row.present_seq) != Some(summary.cutoff_present_seq)
        || camera.len() as u64 != summary.camera_pose_row_count
        || depth.len() as u64 != summary.depth_row_count
    {
        return Err("recorded camera/depth counts or Present window differ from Flushed".into());
    }
    if let Some(player) = player.as_ref() {
        let expected_player_rows = summary
            .player_pose_row_count
            .ok_or("loaded CP2077 player rows without an enabled flush stream")?;
        validate_player_rows(
            request.identity,
            request.first_present_seq,
            summary.cutoff_present_seq,
            expected_player_rows,
            &camera
                .iter()
                .map(|row| (row.present_seq, row.present_qpc))
                .collect::<Vec<_>>(),
            player,
        )?;
    }
    let selected = frames
        .iter()
        .map(|frame| frame.present_seq.ok_or("RGB Present identity missing"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let selected_summary = summary
        .selected_present
        .as_ref()
        .ok_or("selected-Present flush missing")?;
    if selected_summary.generation != request.identity.generation
        || selected_summary.selected_frame_count != frames.len() as u64
        || selected_summary.distinct_selected_depth_count != selected.len() as u64
        || depth
            .iter()
            .map(|row| row.present_seq)
            .collect::<BTreeSet<_>>()
            != selected
        || depth.len() != selected.len()
    {
        return Err("selected-Present accounting does not match recorded RGB/depth".into());
    }
    let depth_pairs: Vec<_> = depth
        .iter()
        .map(|row| DepthIndexRow {
            present_seq: row.present_seq,
            dsv_id: row.dsv_id.clone(),
            width: row.width,
            height: row.height,
        })
        .collect();
    let (pairs, pairing) =
        crate::pipeline::run_selected_present(&frames, &depth_pairs, CameraMode::Player)
            .map_err(|error| error.to_string())?;
    if !pairing.passed {
        return Err(format!(
            "exact RGB/depth pairing failed: {:?}",
            pairing.failures
        ));
    }
    let joined = join_camera_action_full_rate(FullRateCameraActionJoinInputs {
        camera: IdentifiedRows::new(request.identity, &camera),
        actions: IdentifiedRows::new(request.identity, &actions),
        frame_pairs: IdentifiedRows::new(request.identity, &pairs),
    })
    .map_err(|error| error.to_string())?;
    let root = request
        .probe_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut paths = BTreeSet::new();
    for row in &depth {
        let pose = camera
            .iter()
            .find(|pose| pose.present_seq == row.present_seq)
            .ok_or("exact depth camera is absent")?;
        let CameraPoseProvenance::ResolvedD3d12CameraHook {
            binding,
            projection_matrix,
            dsv_id,
            ..
        } = &pose.provenance
        else {
            return Err("CP2077 local audit requires the bound D3D12 camera".into());
        };
        if binding.profile_id != crate::raw_depth_demo::CP2077_GAME_ID
            || binding.holstered_camera.is_none()
            || *dsv_id != row.dsv_id
        {
            return Err("CP2077 local audit received a different camera policy".into());
        }
        if row.schema != INDEX_SCHEMA_V6
            || row.source_frame != row.present_seq
            || row.projection.proj_scope != ProjectionScope::ObservedThisFrame
        {
            return Err("depth lacks exact-Present projection evidence".into());
        }
        row.resolve_transform()?;
        row.resolve_health()?;
        let projection = crate::camera_track::projection_from_matrix(projection_matrix)
            .ok_or("invalid camera projection")?;
        let (Some(min), Some(max)) = (
            row.projection.viewport_min_depth,
            row.projection.viewport_max_depth,
        ) else {
            return Err("depth viewport evidence missing".into());
        };
        if !min.is_finite()
            || !max.is_finite()
            || min < 0.0
            || max > 1.0
            || min >= max
            || row.projection.proj_a != Some(min + (max - min) * projection.a)
            || row.projection.proj_b != Some((max - min) * projection.b)
        {
            return Err(format!(
                "depth/camera projection differs at Present {}",
                row.present_seq
            ));
        }
        let path = Path::new(&row.path);
        if path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("depth path escapes the probe root".into());
        }
        let path = root
            .join(path)
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !path.starts_with(&root) || !paths.insert(path.clone()) {
            return Err("depth path is outside the run or reused by another Present".into());
        }
        files.push(fingerprint(&path)?);
    }
    for evidence in &files {
        if fingerprint(&evidence.path)?.sha256 != evidence.sha256 {
            return Err("capture files changed during local audit".into());
        }
    }
    Ok(LocalCaptureAudit {
        schema: SCHEMA.into(),
        identity: request.identity.clone(),
        non_trainable: true,
        deliverable: false,
        player_pose_captured: player.is_some(),
        mp4_watermark_redecoded: false,
        depth_pixels_decoded: false,
        rgb_frames: frames.len(),
        distinct_selected_presents: selected.len(),
        camera_rows: camera.len(),
        depth_rows: depth.len(),
        joined_action_camera_rows: joined.len(),
        even_cfr_pairing: pairing,
        files,
    })
}

fn validate_player_rows(
    identity: &StreamIdentity,
    first_present_seq: u64,
    cutoff_present_seq: u64,
    expected_count: u64,
    camera_presents: &[(u64, i64)],
    player: &[PlayerPoseRow],
) -> Result<(), String> {
    for row in player {
        validate_player_pose_row(row).map_err(str::to_owned)?;
        if row.identity != *identity
            || row.source != crate::CP2077_PLAYER_POSE_SOURCE
            || row.executable_sha256 != crate::CP2077_BUILD_20383525_EXE_SHA256
        {
            return Err("CP2077 player row identity, source, or build differs".into());
        }
    }
    if player.first().map(|row| row.present_seq) != Some(first_present_seq)
        || player.last().map(|row| row.present_seq) != Some(cutoff_present_seq)
        || player.len() as u64 != expected_count
        || player
            .iter()
            .map(|row| (row.present_seq, row.present_qpc))
            .collect::<Vec<_>>()
            != camera_presents
    {
        return Err("recorded CP2077 player rows differ from the formal Present window".into());
    }
    Ok(())
}

fn ready_binding(
    request: &AuditRequest<'_>,
    files: &mut Vec<FileEvidence>,
) -> Result<crate::D3d12CameraBinding, String> {
    use crate::probe_control::{AckKind, read_acks};
    let mut ready = None;
    for ack in read_acks(request.probe_root).map_err(|error| error.to_string())? {
        if ack.identity != *request.identity {
            continue;
        }
        if let AckKind::Ready {
            actual_profile,
            actual_binding,
            depth_profile_sha256,
            camera_binding_sha256,
            first_present_seq,
            ..
        } = ack.ack
        {
            if ready.is_some()
                || actual_profile.game_id != crate::raw_depth_demo::CP2077_GAME_ID
                || first_present_seq != request.first_present_seq
            {
                return Err("local audit Ready identity is ambiguous or mismatched".into());
            }
            ready = Some((
                actual_profile,
                actual_binding,
                depth_profile_sha256,
                camera_binding_sha256,
            ));
        }
    }
    let (actual_profile, actual_binding, profile_hash, binding_hash) =
        ready.ok_or("CP2077 Ready snapshot identity is absent")?;
    let mut snapshot = |stem: &str, hash: &str| -> Result<Vec<u8>, String> {
        if hash.len() != 71
            || !hash.starts_with("sha256:")
            || !hash[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid Ready snapshot hash".into());
        }
        let path = request
            .probe_root
            .join("provenance")
            .join(format!("{stem}-{}.json", &hash[7..]));
        let evidence = fingerprint(&path)?;
        if evidence.sha256 != hash || evidence.bytes > MAX_INDEX_BYTES {
            return Err("Ready snapshot hash/size differs".into());
        }
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        files.push(evidence);
        Ok(bytes)
    };
    let profile: crate::DepthProfile =
        serde_json::from_slice(&snapshot("depth-profile", &profile_hash)?)
            .map_err(|error| error.to_string())?;
    let binding: crate::CameraBindingDocument =
        serde_json::from_slice(&snapshot("camera-binding", &binding_hash)?)
            .map_err(|error| error.to_string())?;
    binding
        .validate_for_profile(&profile, &profile.executable_sha256)
        .map_err(|error| error.to_string())?;
    if binding.identity() != actual_binding
        || profile.game_id != actual_profile.game_id
        || profile.executable_sha256 != actual_profile.executable_sha256
        || profile.schema_version != actual_profile.schema_version
    {
        return Err("Ready profile/binding differs from snapshot identity".into());
    }
    match binding {
        crate::CameraBindingDocument::V2(binding)
            if binding.schema == crate::CAMERA_BINDING_SCHEMA_V3 =>
        {
            Ok(binding)
        }
        _ => Err("local audit requires the CP2077 holstered binding".into()),
    }
}

fn fingerprint(path: &Path) -> Result<FileEvidence, String> {
    let mut file = File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err("audit input is not a nonempty regular file".into());
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(FileEvidence {
        path: path.into(),
        bytes: metadata.len(),
        sha256: format!("sha256:{:x}", hash.finalize()),
    })
}

fn read_index<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>, String> {
    if std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_INDEX_BYTES
    {
        return Err("local audit index exceeds byte budget".into());
    }
    let reader = BufReader::new(File::open(path).map_err(|error| error.to_string())?);
    let mut rows = Vec::new();
    for line in reader.lines() {
        if rows.len() >= MAX_ROWS {
            return Err("local audit index exceeds row budget".into());
        }
        rows.push(
            serde_json::from_str(&line.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_indices_reject_empty_rows_and_non_regular_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.jsonl");
        std::fs::write(&path, b"{}\n\n").unwrap();
        assert!(read_index::<serde_json::Value>(&path).is_err());
        assert!(fingerprint(temp.path()).is_err());
    }

    #[test]
    fn formal_player_rows_require_the_exact_build_identity_and_camera_window() {
        let identity = StreamIdentity {
            session_id: "session".into(),
            run_id: "run".into(),
            generation: 7,
        };
        let row = |present_seq| PlayerPoseRow {
            schema: crate::PLAYER_POSE_INDEX_SCHEMA_V1.into(),
            identity: identity.clone(),
            present_seq,
            present_qpc: present_seq as i64 * 100,
            executable_sha256: crate::CP2077_BUILD_20383525_EXE_SHA256.into(),
            source: crate::CP2077_PLAYER_POSE_SOURCE.into(),
            kcd2_root_source: None,
            cp2077_camera_join: Some(crate::Cp2077CameraJoinReceipt {
                state_first_qpc: present_seq as i64 * 100 - 30,
                state_last_qpc: present_seq as i64 * 100 - 1,
                state_reads: 29,
                position_residual_metres: 0.0002,
                rotation_residual_degrees: 3e-6,
                candidates: 1,
                torn_candidates: 0,
            }),
            player_position: [present_seq as f64, 0.0, 0.0],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: Some([1.0, 0.0, 0.0]),
        };
        let rows = vec![row(10), row(11)];
        validate_player_rows(&identity, 10, 11, 2, &[(10, 1_000), (11, 1_100)], &rows).unwrap();
        assert!(
            validate_player_rows(&identity, 10, 11, 2, &[(10, 1_000), (12, 1_200)], &rows).is_err()
        );
        assert!(
            validate_player_rows(&identity, 10, 11, 2, &[(10, 1_000), (11, 1_101)], &rows).is_err()
        );
        let mut wrong_build = rows;
        wrong_build[0].executable_sha256 = format!("sha256:{}", "0".repeat(64));
        assert!(
            validate_player_rows(
                &identity,
                10,
                11,
                2,
                &[(10, 1_000), (11, 1_100)],
                &wrong_build
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "read-only historical capture replay; requires explicit input and separate output paths"]
    fn replay_local_capture_and_missing_row_failures() {
        use crate::probe_control::{AckKind, read_acks};
        let run = PathBuf::from(std::env::var_os("DM_CP2077_LOCAL_AUDIT_RUN").unwrap());
        let output = PathBuf::from(std::env::var_os("DM_CP2077_LOCAL_AUDIT_OUTPUT").unwrap());
        assert!(
            !output
                .canonicalize()
                .unwrap()
                .starts_with(run.canonicalize().unwrap())
        );
        let probe_root = run.join("depth-lab");
        let acks = read_acks(&probe_root).unwrap();
        let (identity, first) = acks
            .iter()
            .find_map(|ack| match &ack.ack {
                AckKind::Ready {
                    first_present_seq, ..
                } => Some((ack.identity.clone(), *first_present_seq)),
                _ => None,
            })
            .unwrap();
        let original = acks
            .iter()
            .find_map(|ack| match &ack.ack {
                AckKind::Flushed { summary } => Some(summary.clone()),
                _ => None,
            })
            .unwrap();
        let mut frame_paths = std::fs::read_dir(run.join("segments"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("frames.jsonl.zst"))
            .collect::<Vec<_>>();
        frame_paths.sort();
        let mut summary = original.clone();
        summary.player_pose_row_count = None;
        let request = |summary| AuditRequest {
            identity: &identity,
            first_present_seq: first,
            summary,
            frame_paths: &frame_paths,
            probe_root: &probe_root,
        };
        // Reproduce the original false claim; correction exists only in memory.
        assert_eq!(original.player_pose_row_count, Some(0));
        assert!(audit(&request(&original)).is_err());
        let report = audit(&request(&summary)).unwrap();
        assert_eq!(report.rgb_frames, 1022);
        assert_eq!(report.depth_rows, 472);
        assert_eq!(report.camera_rows, 485);
        assert_eq!(report.joined_action_camera_rows, 1022);
        assert!(!report.deliverable);
        let mut wrong = summary.clone();
        wrong.camera_pose_row_count -= 1;
        assert!(audit(&request(&wrong)).unwrap_err().contains("counts"));
        let temp = tempfile::tempdir().unwrap();
        // Mutate only independent copies of the small indices. Raw EXRs are
        // never touched; these failures must be found before reading pixels.
        for file in report
            .files
            .iter()
            .filter(|entry| entry.path.starts_with(&probe_root))
        {
            let relative = file.path.strip_prefix(&probe_root).unwrap();
            if relative
                .extension()
                .is_some_and(|extension| extension == "exr")
            {
                continue;
            }
            let destination = temp.path().join(relative);
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::copy(&file.path, destination).unwrap();
        }
        let ack_dir = temp.path().join("probe-control/acks");
        std::fs::create_dir_all(&ack_dir).unwrap();
        for entry in std::fs::read_dir(probe_root.join("probe-control/acks")).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), ack_dir.join(entry.file_name())).unwrap();
        }
        let camera_path = temp.path().join("camera-pose-index.jsonl");
        let original_camera = std::fs::read_to_string(&camera_path).unwrap();
        let mut camera: Vec<serde_json::Value> = original_camera
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        camera[100]["identity"]["generation"] = serde_json::json!(identity.generation + 1);
        std::fs::write(
            &camera_path,
            camera
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let mut changed = request(&summary);
        changed.probe_root = temp.path();
        let error = audit(&changed).unwrap_err();
        assert!(error.to_lowercase().contains("identity"), "{error}");
        std::fs::write(&camera_path, &original_camera).unwrap();
        let depth_path = temp.path().join("depth-index.jsonl");
        let raw_depth = std::fs::read_to_string(&depth_path).unwrap();
        std::fs::write(
            &depth_path,
            raw_depth
                .lines()
                .skip(1)
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        assert!(audit(&changed).unwrap_err().contains("counts"));
        // Preserve proof of the scope: original ACK is still unchanged on disk.
        assert_eq!(read_acks(&probe_root).unwrap(), acks);
        std::fs::write(
            output.join("local-capture-audit.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
