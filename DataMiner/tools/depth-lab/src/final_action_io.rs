use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::IdentifiedRows;
use crate::camera_action::{
    ActionFrameRow, CameraActionJoinInputs, CameraPoseRow, FinalCameraActionInputs,
    FullRateCameraActionJoinInputs, MouseNormalization, PlayerPoseRow, assemble_final_pose_rows,
    join_camera_action, join_camera_action_full_rate,
};
use crate::pairing::FramePair;
use crate::pipeline::{DepthIndexRow, RecorderFrameRow};

/// Read the recorder's production frame projection, including its zstd form,
/// through the same parser used by the offline action assembler.
pub fn read_recorder_frame_rows(path: &Path) -> Result<Vec<RecorderFrameRow>, String> {
    read_json_lines(path)
}

/// Read the same immutable projection as action facts without inventing player pose.
pub fn read_action_frame_rows(path: &Path) -> Result<Vec<ActionFrameRow>, String> {
    read_json_lines::<serde_json::Value>(path)?
        .into_iter()
        .map(|mut row| {
            // These are recorder identity fields, not ActionFrameRow fields. Validate
            // them before projecting, while keeping strict rejection of unknown action data.
            serde_json::from_value::<RecorderFrameRow>(row.clone())
                .map_err(|error| error.to_string())?;
            let object = row
                .as_object_mut()
                .ok_or("recorder action row is not an object")?;
            for key in ["present_seq", "segment_index", "segment_frame"] {
                object.remove(key);
            }
            serde_json::from_value(row).map_err(|error| error.to_string())
        })
        .collect()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WallClockRow {
    session_frame: u64,
    time: String,
}

#[allow(clippy::too_many_arguments)]
pub fn assemble_pose_index(
    depth_path: &Path,
    camera_path: &Path,
    player_path: &Path,
    actions_path: &Path,
    pairs_path: &Path,
    wallclock_path: &Path,
    mouse_path: &Path,
    qpc_frequency_hz: u64,
    output_path: &Path,
) -> Result<u64, String> {
    let depth: Vec<DepthIndexRow> = read_json_lines(depth_path)?;
    let camera: Vec<CameraPoseRow> = read_json_lines(camera_path)?;
    let player: Vec<PlayerPoseRow> = read_json_lines(player_path)?;
    let actions = read_action_frame_rows(actions_path)?;
    let pairs: Vec<FramePair> = read_json_lines(pairs_path)?;
    let wallclock_rows: Vec<WallClockRow> = read_json_lines(wallclock_path)?;
    let mouse: MouseNormalization = serde_json::from_slice(
        &std::fs::read(mouse_path)
            .map_err(|error| format!("read {} failed: {error}", mouse_path.display()))?,
    )
    .map_err(|error| format!("parse {} failed: {error}", mouse_path.display()))?;
    let identity = camera
        .first()
        .map(|row| row.identity.clone())
        .ok_or("camera pose index is empty")?;
    if player.first().map(|row| &row.identity) != Some(&identity) {
        return Err("player pose identity does not match camera identity".into());
    }
    if actions
        .first()
        .is_none_or(|row| row.run_id != identity.run_id)
    {
        return Err("action rows do not match camera run_id".into());
    }
    let joined = join_camera_action(CameraActionJoinInputs {
        depth: IdentifiedRows::new(&identity, &depth),
        camera: IdentifiedRows::new(&identity, &camera),
        actions: IdentifiedRows::new(&identity, &actions),
        frame_pairs: IdentifiedRows::new(&identity, &pairs),
    })
    .map_err(|error| error.to_string())?;
    let mut wallclock = BTreeMap::new();
    for row in wallclock_rows {
        if wallclock.insert(row.session_frame, row.time).is_some() {
            return Err(format!("duplicate wall clock frame {}", row.session_frame));
        }
    }
    let final_rows = assemble_final_pose_rows(FinalCameraActionInputs {
        identity: &identity,
        joined: &joined,
        player: IdentifiedRows::new(&identity, &player),
        wallclock_by_session_frame: &wallclock,
        qpc_frequency_hz,
        mouse: &mouse,
    })?;
    write_pose_rows(output_path, &final_rows)?;
    u64::try_from(final_rows.len()).map_err(|_| "final pose row count overflow".into())
}

#[allow(clippy::too_many_arguments)]
pub fn assemble_pose_index_full_rate(
    camera_path: &Path,
    player_path: &Path,
    actions_path: &Path,
    pairs_path: &Path,
    wallclock_path: &Path,
    mouse_path: &Path,
    qpc_frequency_hz: u64,
    output_path: &Path,
) -> Result<u64, String> {
    let camera: Vec<CameraPoseRow> = read_json_lines(camera_path)?;
    let player: Vec<PlayerPoseRow> = read_json_lines(player_path)?;
    let actions = read_action_frame_rows(actions_path)?;
    let pairs: Vec<FramePair> = read_json_lines(pairs_path)?;
    let wallclock_rows: Vec<WallClockRow> = read_json_lines(wallclock_path)?;
    let mouse: MouseNormalization = serde_json::from_slice(
        &std::fs::read(mouse_path)
            .map_err(|error| format!("read {} failed: {error}", mouse_path.display()))?,
    )
    .map_err(|error| format!("parse {} failed: {error}", mouse_path.display()))?;
    let identity = camera
        .first()
        .map(|row| row.identity.clone())
        .ok_or("camera pose index is empty")?;
    if player.first().map(|row| &row.identity) != Some(&identity) {
        return Err("player pose identity does not match camera identity".into());
    }
    if actions
        .first()
        .is_none_or(|row| row.run_id != identity.run_id)
    {
        return Err("action rows do not match camera run_id".into());
    }
    let joined = join_camera_action_full_rate(FullRateCameraActionJoinInputs {
        camera: IdentifiedRows::new(&identity, &camera),
        actions: IdentifiedRows::new(&identity, &actions),
        frame_pairs: IdentifiedRows::new(&identity, &pairs),
    })
    .map_err(|error| error.to_string())?;
    let mut wallclock = BTreeMap::new();
    for row in wallclock_rows {
        if wallclock.insert(row.session_frame, row.time).is_some() {
            return Err(format!("duplicate wall clock frame {}", row.session_frame));
        }
    }
    let final_rows = assemble_final_pose_rows(FinalCameraActionInputs {
        identity: &identity,
        joined: &joined,
        player: IdentifiedRows::new(&identity, &player),
        wallclock_by_session_frame: &wallclock,
        qpc_frequency_hz,
        mouse: &mouse,
    })?;
    write_pose_rows(output_path, &final_rows)?;
    u64::try_from(final_rows.len()).map_err(|_| "final pose row count overflow".into())
}

fn write_pose_rows(
    output_path: &Path,
    final_rows: &[crate::camera_action::FinalPoseRow],
) -> Result<(), String> {
    let parent = output_path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", output_path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {} failed: {error}", parent.display()))?;
    let temp = output_path.with_extension("jsonl.tmp");
    let mut file = std::fs::File::create(&temp)
        .map_err(|error| format!("create {} failed: {error}", temp.display()))?;
    for row in final_rows {
        serde_json::to_writer(&mut file, row)
            .map_err(|error| format!("serialize final pose failed: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("write {} failed: {error}", temp.display()))?;
    }
    file.flush()
        .map_err(|error| format!("flush {} failed: {error}", temp.display()))?;
    std::fs::rename(&temp, output_path).map_err(|error| {
        format!(
            "publish {} to {} failed: {error}",
            temp.display(),
            output_path.display()
        )
    })?;
    Ok(())
}

fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, String> {
    if path.is_dir() {
        let mut pending = vec![path.to_path_buf()];
        let mut files = Vec::new();
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).map_err(|error| {
                format!("read directory {} failed: {error}", directory.display())
            })? {
                let entry = entry.map_err(|error| {
                    format!(
                        "enumerate directory {} failed: {error}",
                        directory.display()
                    )
                })?;
                let kind = entry.file_type().map_err(|error| {
                    format!("read file type {} failed: {error}", entry.path().display())
                })?;
                let entry_path = entry.path();
                if kind.is_dir() {
                    pending.push(entry_path);
                } else if kind.is_file()
                    && entry_path
                        .file_name()
                        .is_some_and(|name| name == "frames.jsonl" || name == "frames.jsonl.zst")
                {
                    files.push(entry_path);
                }
            }
        }
        files.sort();
        if files.is_empty() {
            return Err(format!("{} contains no frames.jsonl(.zst)", path.display()));
        }
        let mut rows = Vec::new();
        for file in files {
            rows.extend(read_json_lines(&file)?);
        }
        return Ok(rows);
    }
    let file = std::fs::File::open(path)
        .map_err(|error| format!("open {} failed: {error}", path.display()))?;
    let reader: Box<dyn Read> = if path.extension().is_some_and(|extension| extension == "zst") {
        Box::new(
            zstd::stream::read::Decoder::new(file)
                .map_err(|error| format!("open zstd {} failed: {error}", path.display()))?,
        )
    } else {
        Box::new(file)
    };
    let mut rows = Vec::new();
    for (index, line) in BufReader::new(reader).lines().enumerate() {
        let line = line.map_err(|error| {
            format!("read {} line {} failed: {error}", path.display(), index + 1)
        })?;
        if line.trim().is_empty() {
            return Err(format!("{} line {} is empty", path.display(), index + 1));
        }
        rows.push(serde_json::from_str(&line).map_err(|error| {
            format!(
                "parse {} line {} failed: {error}",
                path.display(),
                index + 1
            )
        })?);
    }
    if rows.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_recorder_metadata_is_validated_without_relaxing_action_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("frames.jsonl");
        let mut row = serde_json::json!({
            "frame": 0, "session_frame": 0, "run_id": "r0000", "video_t_us": 0,
            "output_qpc_start": 100, "output_qpc_end": 200, "source_qpc": 90,
            "source_seq": 1, "duplicate": false, "source_superseded": 0, "queue_drop": 0,
            "keys": [87], "buttons": [], "actions": ["move.forward"],
            "pointerDx": 0, "pointerDy": 0, "pointerBins": [0, 0], "events": [], "src": "raw_input_v1"
        });
        let save = |row: &serde_json::Value| std::fs::write(&path, format!("{row}\n")).unwrap();
        save(&row);
        let legacy = read_action_frame_rows(&path).unwrap();
        row["present_seq"] = serde_json::json!(65537u64);
        row["segment_index"] = serde_json::json!(1);
        row["segment_frame"] = serde_json::json!(0);
        save(&row);
        assert_eq!(read_action_frame_rows(&path).unwrap(), legacy);
        assert_eq!(
            read_recorder_frame_rows(&path).unwrap()[0].present_seq,
            Some(65537)
        );
        row["present_seq"] = serde_json::json!("bad identity");
        save(&row);
        assert!(read_action_frame_rows(&path).is_err());
        row["present_seq"] = serde_json::json!(65537u64);
        row["unrecognized_action"] = serde_json::json!(true);
        save(&row);
        assert!(read_action_frame_rows(&path).is_err());
        row.as_object_mut().unwrap().remove("unrecognized_action");
        row.as_object_mut().unwrap().remove("events");
        save(&row);
        assert!(read_action_frame_rows(&path).is_err());
    }
}
