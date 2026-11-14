//! Camera-only production index writer.
//!
//! 这份 writer 与 depth writer 分开：有效 Present 只接受已经通过 production binding 合成的
//! [`CameraPoseRow`]；缺 pose 的 Present 则写独立诊断行，不接 discovery candidate，也不混入
//! Action/player 字段。两个文件共用一条连续时间线，同一帧只会落入其中一个文件，因此不会
//! 沿用上一帧或拿零值伪造 pose。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use depth_lab::{
    CameraPoseDiagnosticRow, CameraPoseRow, StreamIdentity, validate_camera_pose_row,
    validate_performance_camera_pose_row,
};

pub const CAMERA_POSE_INDEX_FILE: &str = "camera-pose-index.jsonl";
pub const CAMERA_POSE_DIAGNOSTIC_FILE: &str = "camera-pose-diagnostics.jsonl";
// Segment finalization hashes hundreds of MiB of video on the same volume. Keep the render-facing
// production queue bounded, but absorb that short disk-contention window in userspace instead of
// issuing one WriteFile per 240 Hz row. This is an acceptance budget, not a hardware latency claim.
const PRODUCTION_WRITER_BUFFER_BYTES: usize = 2 * 1024 * 1024;

enum ValidationMode {
    Standard,
    PerformanceValidation,
}

pub struct CameraPoseIndexWriter {
    path: PathBuf,
    file: BufWriter<File>,
    diagnostic_path: PathBuf,
    diagnostic_file: BufWriter<File>,
    last_present_seq: Option<u64>,
    identity: Option<StreamIdentity>,
    validation_mode: ValidationMode,
}

impl CameraPoseIndexWriter {
    /// 新 run 必须拿到一份不存在的索引；已有同名文件说明 run 隔离没做好，拒绝追加。
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        Self::with_validation_mode(root, ValidationMode::Standard)
    }

    /// Performance evidence has its own schema; this mode cannot write ordinary capture rows.
    pub fn new_performance_validation(root: impl AsRef<Path>) -> Result<Self, String> {
        Self::with_validation_mode(root, ValidationMode::PerformanceValidation)
    }

    fn with_validation_mode(
        root: impl AsRef<Path>,
        validation_mode: ValidationMode,
    ) -> Result<Self, String> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)
            .map_err(|error| format!("建 Camera 索引目录 {} 失败: {error}", root.display()))?;
        let path = root.join(CAMERA_POSE_INDEX_FILE);
        let diagnostic_path = root.join(CAMERA_POSE_DIAGNOSTIC_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("新建 Camera 索引 {} 失败: {error}", path.display()))?;
        let diagnostic_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&diagnostic_path)
        {
            Ok(file) => file,
            Err(error) => {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(format!(
                    "新建 Camera 诊断索引 {} 失败: {error}",
                    diagnostic_path.display()
                ));
            }
        };
        Ok(Self {
            path,
            file: BufWriter::with_capacity(PRODUCTION_WRITER_BUFFER_BYTES, file),
            diagnostic_path,
            diagnostic_file: BufWriter::with_capacity(
                PRODUCTION_WRITER_BUFFER_BYTES,
                diagnostic_file,
            ),
            last_present_seq: None,
            identity: None,
            validation_mode,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn diagnostic_path(&self) -> &Path {
        &self.diagnostic_path
    }

    pub fn write_row(&mut self, row: &CameraPoseRow) -> Result<(), String> {
        let validation = match self.validation_mode {
            ValidationMode::Standard => validate_camera_pose_row(row),
            ValidationMode::PerformanceValidation => validate_performance_camera_pose_row(row),
        };
        validation.map_err(|reason| {
            format!(
                "Camera pose seq {} 未通过 writer 模式校验: {reason}",
                row.present_seq
            )
        })?;
        self.validate_next(row.present_seq, &row.identity)?;

        let line = serde_json::to_string(row)
            .map_err(|error| format!("序列化 Camera pose 失败: {error}"))?;
        writeln!(self.file, "{line}")
            .map_err(|error| format!("写 Camera 索引 {} 失败: {error}", self.path.display()))?;
        self.last_present_seq = Some(row.present_seq);
        self.identity.get_or_insert_with(|| row.identity.clone());
        Ok(())
    }

    pub fn write_diagnostic(&mut self, row: &CameraPoseDiagnosticRow) -> Result<(), String> {
        row.validate().map_err(|reason| {
            format!(
                "Camera diagnostic seq {} 未通过校验: {reason}",
                row.present_seq
            )
        })?;
        self.validate_next(row.present_seq, &row.identity)?;
        let line = serde_json::to_string(row)
            .map_err(|error| format!("序列化 Camera diagnostic 失败: {error}"))?;
        writeln!(self.diagnostic_file, "{line}").map_err(|error| {
            format!(
                "写 Camera 诊断索引 {} 失败: {error}",
                self.diagnostic_path.display()
            )
        })?;
        self.last_present_seq = Some(row.present_seq);
        self.identity.get_or_insert_with(|| row.identity.clone());
        Ok(())
    }

    /// Flush both bounded userspace buffers after the production sender has been quiesced.
    pub fn finish(&mut self) -> Result<(), String> {
        self.file
            .flush()
            .map_err(|error| format!("flush Camera 索引 {} 失败: {error}", self.path.display()))?;
        self.diagnostic_file.flush().map_err(|error| {
            format!(
                "flush Camera 诊断索引 {} 失败: {error}",
                self.diagnostic_path.display()
            )
        })
    }

    fn validate_next(&self, present_seq: u64, identity: &StreamIdentity) -> Result<(), String> {
        if let Some(expected) = &self.identity
            && expected != identity
        {
            return Err("Camera pose/diagnostic stream identity 发生变化".into());
        }
        if let Some(previous) = self.last_present_seq {
            if present_seq == previous {
                return Err(format!("Camera present_seq {present_seq} 重复"));
            }
            if present_seq < previous {
                return Err(format!(
                    "Camera present_seq 乱序: {previous} -> {present_seq}"
                ));
            }
            if previous.checked_add(1) != Some(present_seq) {
                return Err(format!(
                    "Camera present_seq 缺帧: {previous} -> {present_seq}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth_lab::camera::{Intrinsics, TranslationSlot};
    use depth_lab::{
        CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1, CAMERA_POSE_INDEX_SCHEMA_V1, CameraPoseDiagnosticReason,
        CameraPoseProvenance, CameraPoseSource, MatrixEncoding, MatrixLayout, MatrixRole,
        MatrixSourceBinding, ShaderStage, StreamIdentity, ViewSourceBinding,
    };

    const HASH: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dm-camera-writer-{tag}-{}", std::process::id()))
    }

    fn source(role: MatrixRole, bind_slot: u32) -> MatrixSourceBinding {
        MatrixSourceBinding {
            shader_stage: ShaderStage::Vertex,
            bind_slot,
            pass_ordinal: 0,
            draw_ordinal: 0,
            buffer_bytes: 64,
            offset: 0,
            encoding: MatrixEncoding::F32LittleEndian,
            layout: MatrixLayout::RowMajor,
            transpose: false,
            role,
        }
    }

    fn row(present_seq: u64) -> CameraPoseRow {
        CameraPoseRow {
            schema: CAMERA_POSE_INDEX_SCHEMA_V1.into(),
            identity: StreamIdentity {
                session_id: "s1".into(),
                run_id: "r1".into(),
                generation: 1,
            },
            present_seq,
            present_qpc: present_seq as i64 + 100,
            source: CameraPoseSource::ResolvedMainCameraHook,
            provenance: CameraPoseProvenance::ResolvedMainCameraHook {
                binding_id: "b1".into(),
                binding_version: "1".into(),
                profile_id: "synthetic".into(),
                profile_version: "dm-depth-profile-1".into(),
                executable_sha256: HASH.into(),
                dsv_id: "main-scene-v1".into(),
                render_width: 1920,
                render_height: 1080,
                view_metres_per_unit: Some("1".into()),
                view_source: ViewSourceBinding {
                    source: source(MatrixRole::View, 0),
                    translation: TranslationSlot::Row3,
                },
                projection_source: source(MatrixRole::Projection, 1),
                view_matrix: Box::new([
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                ]),
                projection_matrix: Box::new([
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 16.0 / 9.0, 0.0, 0.0],
                    [0.0, 0.0, 1.001, 1.0],
                    [0.0, 0.0, -0.1, 0.0],
                ]),
            },
            camera_position: [0.0, 0.0, 0.0],
            camera_rotation_quaternion: [0.0, 0.0, 0.0, 1.0],
            camera_intrinsics: Intrinsics {
                fx: 960.0,
                fy: 960.0,
                cx: 960.0,
                cy: 540.0,
            },
            camera_speed: None,
        }
    }

    fn diagnostic(present_seq: u64, reason: CameraPoseDiagnosticReason) -> CameraPoseDiagnosticRow {
        CameraPoseDiagnosticRow {
            schema: CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1.into(),
            identity: row(present_seq).identity,
            present_seq,
            reason,
        }
    }

    fn performance_row(present_seq: u64) -> CameraPoseRow {
        let binding: depth_lab::Kcd2CombinedCameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/kingdom-come-deliverance-2-steam/camera-binding.json"
        ))
        .unwrap();
        let source = &binding.live_pose_plan.view_projection_binding;
        let mut value = row(present_seq);
        value.schema = depth_lab::PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1.into();
        value.source = CameraPoseSource::ResolvedCombinedViewProjection;
        value.provenance = CameraPoseProvenance::ResolvedCombinedViewProjection {
            binding_id: binding.binding_id,
            binding_version: binding.binding_version,
            profile_id: binding.profile_id,
            profile_version: binding.profile_version,
            executable_sha256: binding.executable_sha256,
            dsv_id: "kcd2-r24g8-d24-main-scene-v1".into(),
            render_width: binding.render_width,
            render_height: binding.render_height,
            view_metres_per_unit: binding.view_metres_per_unit,
            axis_mapping: binding.axis_mapping,
            source_qpc: value.present_qpc - 1,
            root_signature_sha256: format!("sha256:{}", source.root_signature_sha256),
            root_parameter_index: source.root_parameter_index,
            pass_ordinal: source.pass_ordinal,
            draw_ordinal: source.draw_ordinal,
            // Observed finite reversed-Z 2 cm near plane, deliberately outside standard pins.
            projection_a: -2.5000061997793486e-6,
            projection_b: 0.020000049844384193,
            projection_decomposition_residual: 0.0,
            held_out_depth_row_residual: 0.0,
        };
        value
    }

    #[test]
    fn explicit_writer_modes_reject_cross_schema_and_foreign_sources() {
        let standard_root = temp_root("standard-rejects-performance");
        let performance_root = temp_root("performance-schema");
        let _ = std::fs::remove_dir_all(&standard_root);
        let _ = std::fs::remove_dir_all(&performance_root);
        let mut standard = CameraPoseIndexWriter::new(&standard_root).unwrap();
        let mut performance =
            CameraPoseIndexWriter::new_performance_validation(&performance_root).unwrap();
        let perf_row = performance_row(100);
        assert!(standard.write_row(&perf_row).is_err());
        let mut ordinary = perf_row.clone();
        ordinary.schema = CAMERA_POSE_INDEX_SCHEMA_V1.into();
        assert!(performance.write_row(&ordinary).is_err());
        let mut foreign = row(100);
        foreign.schema = depth_lab::PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1.into();
        assert!(performance.write_row(&foreign).is_err());
        let mut unknown = perf_row.clone();
        unknown.schema = "foreign-camera-pose-index/v1".into();
        assert!(performance.write_row(&unknown).is_err());
        performance.write_row(&perf_row).unwrap();
        performance.finish().unwrap();
        let stored: CameraPoseRow =
            serde_json::from_str(std::fs::read_to_string(performance.path()).unwrap().trim())
                .unwrap();
        assert_eq!(stored, perf_row);
        assert_eq!(
            performance.path().file_name().unwrap(),
            CAMERA_POSE_INDEX_FILE
        );
        drop(standard);
        drop(performance);
        let _ = std::fs::remove_dir_all(standard_root);
        let _ = std::fs::remove_dir_all(performance_root);
    }

    #[test]
    fn performance_writer_keeps_strict_identity_and_present_continuity() {
        let root = temp_root("performance-continuity");
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = CameraPoseIndexWriter::new_performance_validation(&root).unwrap();
        writer.write_row(&performance_row(100)).unwrap();
        for seq in [100, 99, 102] {
            assert!(writer.write_row(&performance_row(seq)).is_err());
        }
        let mut other_run = performance_row(101);
        other_run.identity.run_id = "other-run".into();
        assert!(writer.write_row(&other_run).is_err());
        writer.write_row(&performance_row(101)).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(writer.path())
                .unwrap()
                .lines()
                .count(),
            2
        );
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn writes_camera_only_jsonl_and_refuses_existing_run_file() {
        let root = temp_root("create-new");
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = CameraPoseIndexWriter::new(&root).unwrap();
        writer.write_row(&row(100)).unwrap();
        assert!(CameraPoseIndexWriter::new(&root).is_err());
        writer.finish().unwrap();
        let text = std::fs::read_to_string(writer.path()).unwrap();
        assert!(text.contains("\"schema\":\"camera-pose-index/v1\""));
        assert!(!text.contains("player_position"));
        assert!(writer.diagnostic_path().exists());
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_out_of_order_and_gap_fail_closed() {
        for (tag, next) in [("duplicate", 100), ("out-of-order", 99), ("gap", 102)] {
            let root = temp_root(tag);
            let _ = std::fs::remove_dir_all(&root);
            let mut writer = CameraPoseIndexWriter::new(&root).unwrap();
            writer.write_row(&row(100)).unwrap();
            assert!(writer.write_row(&row(next)).is_err(), "case {tag}");
            drop(writer);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn an_explicit_absence_keeps_the_timeline_contiguous_without_a_fake_pose() {
        let root = temp_root("diagnostic-gap");
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = CameraPoseIndexWriter::new(&root).unwrap();
        writer.write_row(&row(100)).unwrap();
        writer
            .write_diagnostic(&diagnostic(
                101,
                CameraPoseDiagnosticReason::NoMainSceneDepth,
            ))
            .unwrap();
        writer.write_row(&row(102)).unwrap();
        writer.finish().unwrap();

        let poses = std::fs::read_to_string(writer.path()).unwrap();
        assert!(!poses.contains("\"present_seq\":101"));
        let diagnostics = std::fs::read_to_string(writer.diagnostic_path()).unwrap();
        assert!(diagnostics.contains("\"reason\":\"no_main_scene_depth\""));
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_buffer_flushes_a_long_contiguous_stream_at_finish() {
        let root = temp_root("buffered-finish");
        let _ = std::fs::remove_dir_all(&root);
        let mut writer = CameraPoseIndexWriter::new(&root).unwrap();
        for present_seq in 10_000..12_048 {
            writer.write_row(&row(present_seq)).unwrap();
        }
        writer.finish().unwrap();
        let text = std::fs::read_to_string(writer.path()).unwrap();
        assert_eq!(text.lines().count(), 2_048);
        assert!(text.contains("\"present_seq\":10000"));
        assert!(text.contains("\"present_seq\":12047"));
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }
}
