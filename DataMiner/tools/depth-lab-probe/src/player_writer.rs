use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use depth_lab::{
    PLAYER_POSE_INDEX_SCHEMA_V1, PlayerPoseRow, StreamIdentity, validate_player_pose_row,
};

pub const PLAYER_POSE_INDEX_FILE: &str = "player-pose-index.jsonl";
const PRODUCTION_WRITER_BUFFER_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PlayerRootSample {
    pub present_seq: u64,
    pub present_qpc: i64,
    pub kcd2_root_source: Option<depth_lab::Kcd2LivePoseSourceIdentity>,
    pub cp2077_camera_join: Option<depth_lab::Cp2077CameraJoinReceipt>,
    pub player_position: [f64; 3],
    pub player_rotation_quaternion: [f64; 4],
}

pub struct PlayerPoseIndexWriter {
    path: PathBuf,
    file: BufWriter<File>,
    identity: StreamIdentity,
    executable_sha256: String,
    source: String,
    qpc_frequency_hz: f64,
    pending: Option<PlayerRootSample>,
    last_speed: Option<[f64; 3]>,
    row_count: u64,
}

impl PlayerPoseIndexWriter {
    pub fn new(
        root: impl AsRef<Path>,
        identity: StreamIdentity,
        executable_sha256: String,
        qpc_frequency_hz: u64,
        source: String,
    ) -> Result<Self, String> {
        if qpc_frequency_hz == 0 {
            return Err("player pose QPC frequency is zero".into());
        }
        std::fs::create_dir_all(root.as_ref())
            .map_err(|error| format!("create player pose directory failed: {error}"))?;
        let path = root.as_ref().join(PLAYER_POSE_INDEX_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("create {} failed: {error}", path.display()))?;
        Ok(Self {
            path,
            file: BufWriter::with_capacity(PRODUCTION_WRITER_BUFFER_BYTES, file),
            identity,
            executable_sha256,
            source,
            qpc_frequency_hz: qpc_frequency_hz as f64,
            pending: None,
            last_speed: None,
            row_count: 0,
        })
    }

    pub fn push(&mut self, sample: PlayerRootSample) -> Result<(), String> {
        let Some(previous) = self.pending.replace(sample) else {
            return Ok(());
        };
        let current = self.pending.as_ref().expect("sample was just installed");
        if current.present_seq
            != previous
                .present_seq
                .checked_add(1)
                .ok_or("present_seq overflow")?
        {
            return Err(format!(
                "player pose present_seq gap: {} -> {}",
                previous.present_seq, current.present_seq
            ));
        }
        if current.present_qpc <= previous.present_qpc {
            return Err(format!(
                "player pose QPC did not increase: {} -> {}",
                previous.present_qpc, current.present_qpc
            ));
        }
        let dt = (current.present_qpc - previous.present_qpc) as f64 / self.qpc_frequency_hz;
        let speed = [
            (current.player_position[0] - previous.player_position[0]) / dt,
            (current.player_position[1] - previous.player_position[1]) / dt,
            (current.player_position[2] - previous.player_position[2]) / dt,
        ];
        self.write(previous, speed)?;
        self.last_speed = Some(speed);
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64, String> {
        let pending = self
            .pending
            .take()
            .ok_or("player pose stream did not contain a sample")?;
        let speed = self
            .last_speed
            .ok_or("player pose stream needs at least two samples for velocity")?;
        self.write(pending, speed)?;
        self.file
            .flush()
            .map_err(|error| format!("flush {} failed: {error}", self.path.display()))?;
        Ok(self.row_count)
    }

    fn write(&mut self, sample: PlayerRootSample, speed: [f64; 3]) -> Result<(), String> {
        let row = PlayerPoseRow {
            schema: PLAYER_POSE_INDEX_SCHEMA_V1.into(),
            identity: self.identity.clone(),
            present_seq: sample.present_seq,
            present_qpc: sample.present_qpc,
            executable_sha256: self.executable_sha256.clone(),
            source: self.source.clone(),
            kcd2_root_source: sample.kcd2_root_source,
            cp2077_camera_join: sample.cp2077_camera_join,
            player_position: sample.player_position,
            player_rotation_quaternion: sample.player_rotation_quaternion,
            player_speed: Some(speed),
        };
        validate_player_pose_row(&row)
            .map_err(|reason| format!("invalid player pose {}: {reason}", row.present_seq))?;
        let line = serde_json::to_string(&row)
            .map_err(|error| format!("serialize player pose failed: {error}"))?;
        writeln!(self.file, "{line}")
            .map_err(|error| format!("write {} failed: {error}", self.path.display()))?;
        self.row_count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> StreamIdentity {
        StreamIdentity {
            session_id: "s".into(),
            run_id: "r".into(),
            generation: 1,
        }
    }

    fn sample(seq: u64, qpc: i64, x: f64) -> PlayerRootSample {
        PlayerRootSample {
            present_seq: seq,
            present_qpc: qpc,
            kcd2_root_source: None,
            cp2077_camera_join: None,
            player_position: [x, 0.0, 0.0],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
        }
    }

    #[test]
    fn buffers_one_row_so_every_emitted_pose_has_real_velocity() {
        let root = std::env::temp_dir().join(format!("dm-player-writer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = PlayerPoseIndexWriter::new(
            &root,
            identity(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            1_000,
            "fallout4-player-root-exact-build/v1".into(),
        )
        .unwrap();
        writer.push(sample(10, 1_000, 0.0)).unwrap();
        writer.push(sample(11, 1_100, 1.0)).unwrap();
        assert_eq!(writer.finish().unwrap(), 2);
        let text = std::fs::read_to_string(root.join(PLAYER_POSE_INDEX_FILE)).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"player_speed\":[10.0,0.0,0.0]"));
        assert!(!text.contains("kcd2_root_source"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kcd2_v2_writer_keeps_each_buffered_samples_observed_source() {
        let root = std::env::temp_dir().join(format!("dm-player-source-{}", uuid::Uuid::new_v4()));
        let mut writer = PlayerPoseIndexWriter::new(
            &root,
            identity(),
            depth_lab::KCD2_EXECUTABLE_SHA256.into(),
            1_000,
            "kcd2-horizontal-player-root-exact-build/v2".into(),
        )
        .unwrap();
        let plan = depth_lab::Kcd2LivePosePlan::pinned_v3();
        let mut expected = Vec::new();
        for (seq, pass, draw) in [(10, 15, 2), (11, 18, 4)] {
            let source = depth_lab::Kcd2LivePoseSourceIdentity {
                root_signature_sha256: plan
                    .horizontal_player_root_binding
                    .root_signature_sha256
                    .clone(),
                root_parameter_index: 0,
                pass_ordinal: pass,
                draw_ordinal: draw,
            };
            let mut row = sample(seq, seq as i64 * 100, seq as f64);
            row.kcd2_root_source = Some(source.clone());
            expected.push(source);
            writer.push(row).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), 2);
        let text = std::fs::read_to_string(root.join(PLAYER_POSE_INDEX_FILE)).unwrap();
        let rows = text
            .lines()
            .map(|line| serde_json::from_str::<PlayerPoseRow>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        for (row, source) in rows.iter().zip(expected) {
            validate_player_pose_row(row).unwrap();
            assert_eq!(row.kcd2_root_source.as_ref(), Some(&source));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gaps_fail_closed() {
        let root = std::env::temp_dir().join(format!("dm-player-gap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = PlayerPoseIndexWriter::new(
            &root,
            identity(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            1_000,
            "fallout4-player-root-exact-build/v1".into(),
        )
        .unwrap();
        writer.push(sample(10, 1_000, 0.0)).unwrap();
        assert!(writer.push(sample(12, 1_100, 1.0)).is_err());
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cp2077_source_accepts_only_the_pinned_executable() {
        let mut row = PlayerPoseRow {
            schema: PLAYER_POSE_INDEX_SCHEMA_V1.into(),
            identity: identity(),
            present_seq: 10,
            present_qpc: 1_000,
            executable_sha256: format!("sha256:{}", "0".repeat(64)),
            source: depth_lab::CP2077_PLAYER_POSE_SOURCE.into(),
            kcd2_root_source: None,
            cp2077_camera_join: Some(depth_lab::Cp2077CameraJoinReceipt {
                state_first_qpc: 970,
                state_last_qpc: 999,
                state_reads: 29,
                position_residual_metres: 0.0002,
                rotation_residual_degrees: 3e-6,
                candidates: 1,
                torn_candidates: 0,
            }),
            player_position: [0.0; 3],
            player_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            player_speed: Some([0.0; 3]),
        };
        assert_eq!(
            validate_player_pose_row(&row),
            Err("CP2077 player pose build is invalid")
        );
        row.executable_sha256 = depth_lab::CP2077_BUILD_20383525_EXE_SHA256.into();
        validate_player_pose_row(&row).unwrap();
    }
}
