//! Local post-capture Depth finalization.
//!
//! This is deliberately a thin adapter over the existing pairing and segment
//! finalizer contracts. It does not introduce another queue or upload path:
//! recorder projections and the probe's online validated full Present identity
//! are turned into the evidence that `finalize_run_segments` already requires,
//! then the existing journal commit makes the seventh clip artifact visible.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use depth_lab::camera_action::{
    CameraPoseRow, PlayerPoseRow, validate_camera_pose_row, validate_player_pose_row,
};
use depth_lab::pipeline::{DepthIndexRow, RecorderFrameRow};
use depth_lab::probe_control::{AckKind, read_acks};
use depth_lab::segment_finalize::{
    FRAME_STATE_EVIDENCE_SCHEMA, FrameCaptureState, FrameStateBoundary, FrameStateEvidence,
    PublishedDepthSegment, StreamIdentity as FinalizeStreamIdentity,
};
use depth_lab::{CameraMode, StreamIdentity as ProbeStreamIdentity};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::services::depth_lab::{
    ConfirmedProbeFlush, FinalizeDepthRunRequest, finalize_run_segments,
};
use crate::services::journal::{DepthSegmentBoundary, SessionJournal};

const EVIDENCE_DIR: &str = "depth-finalize-evidence";

pub struct FinalizeRecordedDepthRun<'a> {
    pub journal: &'a SessionJournal,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub generation: u64,
    pub run_directory: &'a Path,
    pub probe_root: &'a Path,
    pub flushed: &'a ConfirmedProbeFlush,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunHealth {
    frame_count: u64,
    input_status: String,
    input_queue_overflows: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameProof {
    state: FrameCaptureState,
    camera: bool,
    action: bool,
}

pub fn finalize_recorded_depth_run(
    request: &FinalizeRecordedDepthRun<'_>,
) -> Result<Vec<PublishedDepthSegment>, String> {
    request.flushed.ensure_deliverable()?;
    let identity =
        FinalizeStreamIdentity::new(request.session_id, request.run_id, request.generation)
            .map_err(|error| error.to_string())?;
    let boundaries = request
        .journal
        .depth_segment_boundaries(request.session_id, request.run_id)
        .map_err(|error| format!("读取 Depth segment 边界失败: {error:#}"))?;
    if boundaries.is_empty() {
        return Err("Depth finalizer 没有找到 ready RGB segment".into());
    }

    let evidence_root = request.run_directory.join(EVIDENCE_DIR);
    fs::create_dir_all(&evidence_root)
        .map_err(|error| format!("创建 Depth 收口证据目录失败: {error}"))?;

    let mut frames = Vec::new();
    for boundary in &boundaries {
        let segment_root = request
            .run_directory
            .join("segments")
            .join(format!("{:05}", boundary.segment_index));
        let mut segment_frames = depth_lab::final_action_io::read_recorder_frame_rows(
            &segment_root.join("frames.jsonl.zst"),
        )?;
        validate_segment_frames(boundary, &segment_frames)?;
        for (offset, frame) in segment_frames.iter_mut().enumerate() {
            // Production frames.jsonl predates these two offline convenience
            // fields. The immutable segment directory + journal boundary is
            // authoritative; never infer it from line counts across files.
            frame.segment_index = boundary.segment_index;
            frame.segment_frame = u32::try_from(offset)
                .map_err(|_| "Depth segment frame offset exceeds u32".to_owned())?;
        }
        frames.extend(segment_frames);
    }

    let depth: Vec<DepthIndexRow> = read_jsonl(&request.probe_root.join("depth-index.jsonl"))?;
    let (pairs, report) =
        depth_lab::pipeline::run_selected_present(&frames, &depth, CameraMode::Player)
            .map_err(|error| error.to_string())?;
    if !report.passed {
        return Err(format!(
            "Depth 配对证据未通过: {}",
            report.failures.join("; ")
        ));
    }

    let run_health: RunHealth = read_json(&request.run_directory.join("run.json"))?;
    let action_coverage = run_health.input_status == "raw_input_v1"
        && run_health.input_queue_overflows == 0
        && run_health.frame_count == frames.len() as u64;
    let (camera, raw_provenance_camera) = load_camera_present_set(
        &request.probe_root.join("camera-pose-index.jsonl"),
        &identity,
    )?;
    let player = load_player_present_set(
        &request.probe_root.join("player-pose-index.jsonl"),
        &identity,
    )?;
    let frame_states = build_frame_states(&identity, &frames, &camera, &player, action_coverage)?;
    let raw_evidence = load_raw_evidence(request.probe_root, &identity, raw_provenance_camera)?;

    write_jsonl_atomic(&evidence_root.join("frame-pairs.jsonl"), &pairs)?;
    write_json_atomic(&evidence_root.join("frame-state.json"), &frame_states)?;
    write_json_atomic(&evidence_root.join("e2e-report.json"), &report)?;

    finalize_run_segments(
        request.journal,
        &FinalizeDepthRunRequest {
            session_id: request.session_id,
            run_id: request.run_id,
            generation: request.generation,
            probe_root: request.probe_root,
            run_directory: request.run_directory,
            flushed: request.flushed,
            frame_states: &frame_states,
            pairs: &pairs,
            report: &report,
            raw_evidence: raw_evidence.as_ref(),
        },
    )
}

/// Local accounting only. Never reaches segment publication or the upload queue.
pub fn audit_local_cp2077_run(
    request: &FinalizeRecordedDepthRun<'_>,
    first_present_seq: u64,
) -> Result<(), String> {
    let summary = request.flushed.local_diagnostic_summary()?;
    let boundaries = request
        .journal
        .depth_segment_boundaries(request.session_id, request.run_id)
        .map_err(|error| error.to_string())?;
    let mut frame_paths = Vec::new();
    let mut frame_count = 0;
    for boundary in &boundaries {
        let path = request
            .run_directory
            .join("segments")
            .join(format!("{:05}", boundary.segment_index))
            .join("frames.jsonl.zst");
        let frames = depth_lab::final_action_io::read_recorder_frame_rows(&path)?;
        validate_segment_frames(boundary, &frames)?;
        frame_count += frames.len() as u64;
        frame_paths.push(path);
    }
    let health: RunHealth = read_json(&request.run_directory.join("run.json"))?;
    if health.input_status != "raw_input_v1"
        || health.input_queue_overflows != 0
        || health.frame_count != frame_count
    {
        return Err("CP2077 local audit requires complete healthy Raw Input and RGB rows".into());
    }
    let identity = ProbeStreamIdentity {
        session_id: request.session_id.into(),
        run_id: request.run_id.into(),
        generation: request.generation,
    };
    let result =
        depth_lab::cp2077_local_audit::audit(&depth_lab::cp2077_local_audit::AuditRequest {
            identity: &identity,
            first_present_seq,
            summary,
            frame_paths: &frame_paths,
            probe_root: request.probe_root,
        });
    let report = match &result {
        Ok(report) => serde_json::to_value(report).map_err(|error| error.to_string())?,
        Err(error) => serde_json::json!({
            "schema": depth_lab::cp2077_local_audit::SCHEMA, "identity": identity,
            "non_trainable": true, "deliverable": false, "error": error,
        }),
    };
    write_json_atomic(
        &request
            .run_directory
            .join("cp2077-local-capture-audit.json"),
        &report,
    )?;
    result.map(|_| ())
}

/// Exact-build D3D12 games (KCD2, CP2077) upload raw ZIP v2 with per-frame camera/player rows and
/// the Ready-bound pair; without it the cloud copy could not rebuild a delivery. A run whose
/// camera rows come from one of those bindings must therefore have the matching Ready snapshots.
fn load_raw_evidence(
    probe_root: &Path,
    identity: &FinalizeStreamIdentity,
    raw_provenance_camera: bool,
) -> Result<Option<depth_lab::client_archive::RawRunEvidence>, String> {
    let mut ready = None;
    for ack in read_acks(probe_root).map_err(|error| error.to_string())? {
        if ack.identity.session_id != identity.session_id
            || ack.identity.run_id != identity.run_id
            || ack.identity.generation != identity.generation
        {
            continue;
        }
        if let AckKind::Ready {
            actual_profile,
            depth_profile_sha256,
            camera_binding_sha256,
            ..
        } = ack.ack
        {
            if !depth_lab::client_archive::raw_provenance_game(&actual_profile.game_id) {
                continue;
            }
            if ready.is_some() {
                return Err("Depth finalizer found duplicate Ready identities".into());
            }
            ready = Some((
                actual_profile.game_id,
                depth_profile_sha256,
                camera_binding_sha256,
            ));
        }
    }
    let Some((_, profile_hash, binding_hash)) = ready else {
        return if raw_provenance_camera {
            Err("raw ZIP v2 finalizer requires matching Ready snapshot identities".into())
        } else {
            Ok(None)
        };
    };
    depth_lab::client_archive::RawRunEvidence::load(
        identity,
        probe_root,
        &profile_hash,
        &binding_hash,
    )
    .map(Some)
    .map_err(|error| error.to_string())
}

pub fn recover_confirmed_flush(
    probe_root: &Path,
    expected_session_id: &str,
    expected_run_id: &str,
) -> Result<(ProbeStreamIdentity, ConfirmedProbeFlush), String> {
    let validation_only = probe_root.join("provenance").is_dir()
        && fs::read_dir(probe_root.join("provenance"))
            .map_err(|error| format!("读取 Depth provenance 失败: {error}"))?
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("performance-validation-only-")
            });
    let mut flushed = None;
    for ack in read_acks(probe_root).map_err(|error| error.to_string())? {
        if ack.identity.session_id != expected_session_id || ack.identity.run_id != expected_run_id
        {
            continue;
        }
        if let AckKind::Flushed { summary } = ack.ack {
            flushed = Some((ack.identity, summary));
        }
    }
    let (identity, summary) = flushed
        .ok_or_else(|| format!("{expected_session_id}/{expected_run_id} 没有可信的 Flushed ACK"))?;
    let confirmed = ConfirmedProbeFlush::new(&identity, summary, validation_only)?;
    confirmed.ensure_deliverable()?;
    Ok((identity, confirmed))
}

fn build_frame_states(
    identity: &FinalizeStreamIdentity,
    frames: &[RecorderFrameRow],
    camera: &BTreeSet<u64>,
    player: &BTreeSet<u64>,
    action_coverage: bool,
) -> Result<FrameStateEvidence, String> {
    if frames.is_empty() {
        return Err("Depth frame-state 输入为空".into());
    }
    let mut boundaries: Vec<FrameStateBoundary> = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        if frame.session_frame != frames[0].session_frame + index as u64 {
            return Err(format!(
                "Depth frame-state 的 session_frame 不连续: {}",
                frame.session_frame
            ));
        }
        let present_seq = frame.present_seq.ok_or_else(|| {
            format!(
                "session_frame {} 缺少在线完整 present_seq",
                frame.session_frame
            )
        })?;
        let camera_coverage = camera.contains(&present_seq);
        let player_coverage = player.contains(&present_seq);
        let proof = FrameProof {
            // The probe only writes production player rows after the exact-build
            // player-root sampler and camera snapshot both succeed for this Present.
            // Missing evidence remains Unknown; it is never guessed from depth clear values.
            state: if camera_coverage && player_coverage {
                FrameCaptureState::Gameplay
            } else {
                FrameCaptureState::Unknown
            },
            camera: camera_coverage,
            action: action_coverage,
        };
        if let Some(last) = boundaries.last_mut()
            && last.state == proof.state
            && last.camera_coverage_proven == proof.camera
            && last.action_coverage_proven == proof.action
            && last.session_frame_start + last.frame_count == frame.session_frame
        {
            last.frame_count += 1;
        } else {
            boundaries.push(FrameStateBoundary {
                session_frame_start: frame.session_frame,
                frame_count: 1,
                state: proof.state,
                camera_coverage_proven: proof.camera,
                action_coverage_proven: proof.action,
            });
        }
    }
    Ok(FrameStateEvidence {
        schema: FRAME_STATE_EVIDENCE_SCHEMA.into(),
        identity: identity.clone(),
        boundaries,
    })
}

fn load_camera_present_set(
    path: &Path,
    identity: &FinalizeStreamIdentity,
) -> Result<(BTreeSet<u64>, bool), String> {
    let mut raw_provenance = false;
    let rows = read_pose_set(path, identity, |line| {
        let row: CameraPoseRow = serde_json::from_str(line).map_err(|error| error.to_string())?;
        validate_camera_pose_row(&row).map_err(str::to_owned)?;
        // KCD2's combined view-projection and CP2077's D3D12 hook are the raw ZIP v2 bindings.
        raw_provenance |= matches!(
            row.source,
            depth_lab::CameraPoseSource::ResolvedCombinedViewProjection
                | depth_lab::CameraPoseSource::ResolvedD3d12CameraHook
        );
        Ok((row.identity, row.present_seq))
    })?;
    Ok((rows, raw_provenance))
}

fn load_player_present_set(
    path: &Path,
    identity: &FinalizeStreamIdentity,
) -> Result<BTreeSet<u64>, String> {
    read_pose_set(path, identity, |line| {
        let row: PlayerPoseRow = serde_json::from_str(line).map_err(|error| error.to_string())?;
        validate_player_pose_row(&row).map_err(str::to_owned)?;
        Ok((row.identity, row.present_seq))
    })
}

fn read_pose_set<F>(
    path: &Path,
    identity: &FinalizeStreamIdentity,
    mut parse: F,
) -> Result<BTreeSet<u64>, String>
where
    F: FnMut(&str) -> Result<(ProbeStreamIdentity, u64), String>,
{
    let file =
        File::open(path).map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    let mut rows = BTreeSet::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line =
            line.map_err(|error| format!("读取 {}:{} 失败: {error}", path.display(), index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let (actual_identity, present_seq) =
            parse(&line).map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))?;
        if actual_identity.session_id != identity.session_id
            || actual_identity.run_id != identity.run_id
            || actual_identity.generation != identity.generation
        {
            return Err(format!(
                "{}:{} 的 stream identity 不匹配",
                path.display(),
                index + 1
            ));
        }
        if !rows.insert(present_seq) {
            return Err(format!(
                "{} 含重复 present_seq {present_seq}",
                path.display()
            ));
        }
    }
    Ok(rows)
}

fn validate_segment_frames(
    boundary: &DepthSegmentBoundary,
    frames: &[RecorderFrameRow],
) -> Result<(), String> {
    if frames.len() as u64 != boundary.frame_count {
        return Err(format!(
            "segment {:05} frames 数量不匹配: journal={} file={}",
            boundary.segment_index,
            boundary.frame_count,
            frames.len()
        ));
    }
    for (offset, frame) in frames.iter().enumerate() {
        let expected = boundary.session_frame_start + offset as u64;
        if frame.session_frame != expected
            || (frame.segment_index != 0 && frame.segment_index != boundary.segment_index)
            || (frame.segment_frame != 0 && u64::from(frame.segment_frame) != offset as u64)
        {
            return Err(format!(
                "segment {:05} frame identity 不连续: expected={expected} actual={}",
                boundary.segment_index, frame.session_frame
            ));
        }
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("解析 {} 失败: {error}", path.display()))
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, String> {
    let file =
        File::open(path).map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            other => Some((index, other)),
        })
        .map(|(index, line)| {
            let line = line
                .map_err(|error| format!("读取 {}:{} 失败: {error}", path.display(), index + 1))?;
            serde_json::from_str(&line)
                .map_err(|error| format!("{}:{} 解析失败: {error}", path.display(), index + 1))
        })
        .collect()
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    write_atomic(path, &bytes)
}

fn write_jsonl_atomic<T: Serialize>(path: &Path, rows: &[T]) -> Result<(), String> {
    let temporary = path.with_extension("jsonl.tmp");
    let mut file = File::create(&temporary)
        .map_err(|error| format!("创建 {} 失败: {error}", temporary.display()))?;
    for row in rows {
        serde_json::to_writer(&mut file, row).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
    }
    file.flush().map_err(|error| error.to_string())?;
    publish_temporary(&temporary, path)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("写入 {} 失败: {error}", temporary.display()))?;
    publish_temporary(&temporary, path)
}

fn publish_temporary(temporary: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| format!("替换 {} 失败: {error}", destination.display()))?;
    }
    fs::rename(temporary, destination)
        .map_err(|error| format!("发布 {} 失败: {error}", destination.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kcd2_raw_archive_requires_ready_bound_snapshots_while_legacy_stays_v1() {
        use depth_lab::probe_control::{
            ACK_SCHEMA_V3, DepthProfileIdentity, GpuAdapterIdentity, ProbeAck, write_ack,
        };
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        let identity = FinalizeStreamIdentity::new("s1", "r0000", 7).unwrap();
        assert!(
            load_raw_evidence(temp.path(), &identity, false)
                .unwrap()
                .is_none()
        );
        assert!(load_raw_evidence(temp.path(), &identity, true).is_err());
        let catalog = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../config/catalogs/dev/kingdom-come-deliverance-2-steam");
        let mut paths = Vec::new();
        let mut hashes = Vec::new();
        for stem in ["depth-profile", "camera-binding"] {
            let bytes = fs::read(catalog.join(format!("{stem}.json"))).unwrap();
            let hash = format!("sha256:{:x}", Sha256::digest(&bytes));
            let path = temp.path().join("provenance").join(format!(
                "{stem}-{}.json",
                hash.trim_start_matches("sha256:")
            ));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            paths.push(path);
            hashes.push(hash);
        }
        let profile: depth_lab::DepthProfile =
            serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        let binding: depth_lab::CameraBindingDocument =
            serde_json::from_slice(&fs::read(&paths[1]).unwrap()).unwrap();
        let ack = ProbeAck {
            schema: ACK_SCHEMA_V3.into(),
            command_id: "kcd2-finalize-fixture".into(),
            identity: ProbeStreamIdentity {
                session_id: "s1".into(),
                run_id: "r0000".into(),
                generation: 7,
            },
            ordinal: 1,
            ack: AckKind::Ready {
                actual_profile: DepthProfileIdentity {
                    schema_version: profile.schema_version,
                    game_id: profile.game_id,
                    lane: "lane30".into(),
                    executable_sha256: profile.executable_sha256,
                },
                actual_binding: binding.identity(),
                depth_profile_sha256: hashes[0].clone(),
                camera_binding_sha256: hashes[1].clone(),
                probe_version: "depth-2026.09.4".into(),
                actual_swapchain_adapter: GpuAdapterIdentity {
                    description: "NVIDIA fixture".into(),
                    driver_version: "1.0.0.0".into(),
                    luid_high: 0,
                    luid_low: 1,
                },
                first_present_seq: 10,
                resource_epoch: 1,
                performance_metrics_enabled: false,
            },
        };
        write_ack(temp.path(), &ack).unwrap();
        assert!(
            load_raw_evidence(temp.path(), &identity, true)
                .unwrap()
                .is_some()
        );
        assert!(
            load_raw_evidence(
                temp.path(),
                &FinalizeStreamIdentity::new("s1", "r0000", 8).unwrap(),
                true
            )
            .is_err()
        );
        fs::write(&paths[1], b"{}").unwrap();
        assert!(load_raw_evidence(temp.path(), &identity, true).is_err());
    }

    fn frame(session_frame: u64, present_seq: u64) -> RecorderFrameRow {
        RecorderFrameRow {
            session_frame,
            segment_index: 0,
            segment_frame: session_frame as u32,
            source_qpc: session_frame as i64 + 1,
            duplicate: false,
            present_seq: Some(present_seq),
        }
    }

    #[test]
    fn exact_camera_and_player_rows_prove_gameplay_without_guessing_gaps() {
        let identity = FinalizeStreamIdentity::new("s1", "r0000", 7).unwrap();
        let frames = vec![frame(0, 100), frame(1, 101), frame(2, 102)];
        let camera = BTreeSet::from([100, 101, 102]);
        let player = BTreeSet::from([100, 102]);
        let evidence = build_frame_states(&identity, &frames, &camera, &player, true).unwrap();
        assert_eq!(evidence.boundaries.len(), 3);
        assert_eq!(evidence.boundaries[0].state, FrameCaptureState::Gameplay);
        assert_eq!(evidence.boundaries[1].state, FrameCaptureState::Unknown);
        assert_eq!(evidence.boundaries[2].state, FrameCaptureState::Gameplay);
    }

    #[test]
    fn input_overflow_remains_visible_as_missing_action_coverage() {
        let identity = FinalizeStreamIdentity::new("s1", "r0000", 7).unwrap();
        let frames = vec![frame(0, 100), frame(1, 101)];
        let rows = BTreeSet::from([100, 101]);
        let evidence = build_frame_states(&identity, &frames, &rows, &rows, false).unwrap();
        assert_eq!(evidence.boundaries.len(), 1);
        assert!(!evidence.boundaries[0].action_coverage_proven);
    }
}
