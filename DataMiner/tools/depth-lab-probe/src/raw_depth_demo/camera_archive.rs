//! Writer-owned lossless draw archive. The final v3 report contains only its active tail.
use super::*;
use crate::d3d12_render_camera::D3d12RenderDrawObservation;
use depth_lab::cp2077_camera_report::{archive, compact};
use serde_json::value::RawValue;
use std::io::BufReader;

const FILE: &str = "render-camera-draws.jsonl";
const MAX_BYTES: u64 = depth_lab::cp2077_camera_report::ARCHIVE_BYTES;

pub(super) struct ArchiveWriter {
    path: PathBuf,
    file: Option<BufWriter<std::fs::File>>,
    hash: Sha256,
    expanded_hash: Sha256,
    rows: u64,
    bytes: u64,
    blocks: u64,
    expanded_bytes: u64,
}

impl ArchiveWriter {
    pub(super) fn new(directory: &Path) -> Self {
        Self {
            path: directory.join(FILE),
            file: None,
            hash: Sha256::new(),
            expanded_hash: Sha256::new(),
            rows: 0,
            bytes: 0,
            blocks: 0,
            expanded_bytes: 0,
        }
    }

    pub(super) fn append(&mut self, rows: &[D3d12RenderDrawObservation]) -> Result<(), String> {
        self.append_tokens(
            rows.iter()
                .map(|row| serde_json::value::to_raw_value(row).map_err(|e| e.to_string())),
        )
    }

    fn append_tokens(
        &mut self,
        rows: impl Iterator<Item = Result<Box<RawValue>, String>>,
    ) -> Result<(), String> {
        if self.file.is_none() {
            self.file = Some(BufWriter::new(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&self.path)
                    .map_err(|e| format!("create camera archive: {e}"))?,
            ));
        }
        let mut pending = Vec::new();
        let mut pending_bytes = 0;
        for row in rows {
            let row = row?;
            let size = row.get().len() + 1;
            if size as u64 > depth_lab::cp2077_camera_report::ARCHIVE_ROW_BYTES {
                return Err("camera draw exceeds row bound".into());
            }
            if pending.len() == compact::BLOCK_ROWS
                || pending_bytes + size > compact::BLOCK_EXPANDED_BYTES
            {
                self.append_block(&pending)?;
                pending.clear();
                pending_bytes = 0;
            }
            pending.push(row);
            pending_bytes += size;
        }
        if !pending.is_empty() {
            self.append_block(&pending)?;
        }
        let file = self.file.as_mut().ok_or("camera archive file absent")?;
        file.flush()
            .and_then(|_| file.get_ref().sync_all())
            .map_err(|e| format!("sync camera archive: {e}"))
    }

    fn append_block(&mut self, rows: &[Box<RawValue>]) -> Result<(), String> {
        let block = compact::Block::encode(&rows.iter().map(|r| r.as_ref()).collect::<Vec<_>>())?;
        let mut bytes = serde_json::to_vec(&block).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        let expanded = rows.iter().map(|r| r.get().len() as u64 + 1).sum::<u64>();
        if bytes.len() as u64 > compact::BLOCK_BYTES || self.bytes + bytes.len() as u64 > MAX_BYTES
        {
            return Err("camera archive exceeds bounded disk budget".into());
        }
        if self.expanded_bytes + expanded > depth_lab::cp2077_camera_report::EXPANDED_REPORT_BYTES {
            return Err("camera archive exceeds bounded expansion budget".into());
        }
        self.file
            .as_mut()
            .ok_or("camera archive file absent")?
            .write_all(&bytes)
            .map_err(|e| format!("write camera archive: {e}"))?;
        self.hash.update(&bytes);
        self.bytes += bytes.len() as u64;
        self.blocks += 1;
        self.rows += rows.len() as u64;
        self.expanded_bytes += expanded;
        for row in rows {
            self.expanded_hash.update(row.get().as_bytes());
            self.expanded_hash.update(b"\n");
        }
        Ok(())
    }

    pub(super) fn write_report(
        &mut self,
        output: &mut impl Write,
        report: &RenderCameraReport<'_>,
    ) -> Result<(), String> {
        if self.file.is_none() {
            self.append(&[])?;
        }
        if let Some(file) = self.file.as_mut() {
            file.flush()
                .and_then(|_| file.get_ref().sync_all())
                .map_err(|e| e.to_string())?;
        }
        let state = report
            .observation
            .local_archive
            .as_ref()
            .ok_or("camera archive mode absent")?;
        // Preserve failed-session diagnostics too. The reader still rejects nonzero
        // enqueue_failures; writing that evidence must not hide the failure from operators.
        if self.rows != state.enqueued_draws || !state.frozen {
            return Err("camera archive is incomplete or not frozen".into());
        }
        let mut hash = Sha256::new();
        let mut expanded_hash = Sha256::new();
        let counts = archive::visit(
            BufReader::new(std::fs::File::open(&self.path).map_err(|e| e.to_string())?),
            true,
            |line| hash.update(line),
            |row| {
                expanded_hash.update(row.get().as_bytes());
                expanded_hash.update(b"\n");
                Ok(())
            },
        )?;
        if counts
            != (archive::Counts {
                rows: self.rows,
                blocks: self.blocks,
                bytes: self.bytes,
                expanded_bytes: self.expanded_bytes,
            })
            || hash.finalize() != self.hash.clone().finalize()
            || expanded_hash.finalize() != self.expanded_hash.clone().finalize()
        {
            return Err("camera archive hash/count mismatch".into());
        }
        // RawValue also preserves the live tail's original f64 tokens. No Value round-trip.
        let bytes = serde_json::to_vec(report).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > depth_lab::cp2077_camera_report::LEGACY_REPORT_BYTES
            || bytes.len() as u64 + self.expanded_bytes
                > depth_lab::cp2077_camera_report::EXPANDED_REPORT_BYTES
            || report.observation.observations.len()
                > crate::d3d12_render_camera::MAX_PRESENT_OBSERVATIONS
        {
            return Err("camera report tail/expansion exceeds bound".into());
        }
        let mut fields: std::collections::BTreeMap<String, Box<RawValue>> =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        fields.insert(
            "schema".into(),
            serde_json::value::to_raw_value(compact::REPORT_SCHEMA).map_err(|e| e.to_string())?,
        );
        let declaration = serde_json::json!({
            "schema":compact::ARCHIVE_SCHEMA,"encoding":compact::BLOCK_SCHEMA,"non_trainable":true,"file":FILE,
            "archived_draws":self.rows,"blocks":self.blocks,"bytes":self.bytes,"sha256":format!("sha256:{:x}",self.hash.clone().finalize()),
            "expanded_bytes":self.expanded_bytes,"expanded_sha256":format!("sha256:{:x}",self.expanded_hash.clone().finalize()),
            "final_observations_include_archive":false,"active_draw_capacity":crate::d3d12_render_camera::MAX_PRESENT_OBSERVATIONS,
            "durable":true,"disk_budget_bytes":MAX_BYTES
        });
        fields.insert(
            "observation_archive".into(),
            serde_json::value::to_raw_value(&declaration).map_err(|e| e.to_string())?,
        );
        let encoded = serde_json::to_vec(&fields).map_err(|e| e.to_string())?;
        if encoded.len() as u64 > depth_lab::cp2077_camera_report::LEGACY_REPORT_BYTES
            || encoded.len() as u64 + self.expanded_bytes
                > depth_lab::cp2077_camera_report::EXPANDED_REPORT_BYTES
        {
            return Err("camera report metadata/expansion exceeds bound".into());
        }
        output
            .write_all(&encoded)
            .map_err(|e| format!("serialize compact camera report: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_render_camera::observation_archive::ArchiveState;

    fn directory() -> PathBuf {
        let p = std::env::temp_dir().join(format!("dm-camera-archive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        p
    }
    fn cleanup(p: &Path) {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let actual = p.canonicalize().unwrap();
        assert!(actual.starts_with(&root) && actual != root);
        std::fs::remove_dir_all(actual).unwrap();
    }
    fn report(count: u64, tail: Vec<D3d12RenderDrawObservation>) -> RenderCameraReport<'static> {
        RenderCameraReport {
            schema: "dm-cp2077-render-camera-observation/v2",
            process_launch_id: "test",
            non_trainable: true,
            production_certified: false,
            deliverable: false,
            recording_ready: false,
            observation: D3d12RenderCameraSnapshot {
                observations: tail,
                local_archive: Some(ArchiveState {
                    schema: "dm-cp2077-camera-active-window/v1",
                    enqueued_draws: count,
                    frozen: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    fn restored(writer: &ArchiveWriter) -> Vec<Box<RawValue>> {
        let mut rows = Vec::new();
        archive::visit(
            BufReader::new(std::fs::File::open(&writer.path).unwrap()),
            true,
            |_| {},
            |row| {
                rows.push(RawValue::from_string(row.get().into()).unwrap());
                Ok(())
            },
        )
        .unwrap();
        rows
    }

    #[test]
    fn camera_archive_streams_full_history_and_rejects_corruption_or_missing_batches() {
        let path = directory();
        let mut writer = ArchiveWriter::new(&path);
        for chunk in 0..20 {
            let rows: Vec<_> = (0..270)
                .map(|i| D3d12RenderDrawObservation {
                    present_seq: (chunk * 270 + i + 1) as u64,
                    present_qpc: 1000 + (chunk * 270 + i) as i64,
                    physical_source: format!("immutable-{chunk}-{i}"),
                    ..Default::default()
                })
                .collect();
            writer.append(&rows).unwrap();
        }
        let tail = vec![D3d12RenderDrawObservation {
            present_seq: 5401,
            physical_source: "tail".into(),
            ..Default::default()
        }];
        let report = report(5400, tail);
        let mut bytes = Vec::new();
        writer.write_report(&mut bytes, &report).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema"], compact::REPORT_SCHEMA);
        assert_eq!(
            value["observation"]["observations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut rows: Vec<serde_json::Value> = restored(&writer)
            .iter()
            .map(|r| serde_json::from_str(r.get()).unwrap())
            .collect();
        rows.extend(
            value["observation"]["observations"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        assert_eq!(rows.len(), 5401);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row["present_seq"], (i + 1) as u64);
        }
        assert_eq!(rows[5399]["physical_source"], "immutable-19-269");
        assert_eq!(rows[5400]["physical_source"], "tail");
        assert_eq!(value["observation_archive"]["durable"], true);
        assert!(
            writer
                .write_report(&mut Vec::new(), &self::report(5399, vec![]))
                .is_err()
        );
        let original = std::fs::read(&writer.path).unwrap();
        let mut changed = original.clone();
        let at = changed.windows(9).position(|w| w == b"immutable").unwrap();
        changed[at] = b'I';
        std::fs::write(&writer.path, &changed).unwrap();
        assert!(
            writer
                .write_report(&mut Vec::new(), &report)
                .unwrap_err()
                .contains("hash/count")
        );
        std::fs::write(&writer.path, &original[..original.len() - 1]).unwrap();
        assert!(writer.write_report(&mut Vec::new(), &report).is_err());
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn camera_archive_disk_errors_and_unfrozen_state_remain_failures() {
        let path = directory();
        std::fs::create_dir(path.join(FILE)).unwrap();
        let mut writer = ArchiveWriter::new(&path);
        assert!(
            writer
                .append(&[D3d12RenderDrawObservation::default()])
                .is_err()
        );
        let mut r = report(0, vec![]);
        r.observation.local_archive.as_mut().unwrap().frozen = false;
        assert!(writer.write_report(&mut Vec::new(), &r).is_err());
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn camera_archive_first_present_failure_has_a_verifiable_empty_archive() {
        let path = directory();
        let mut writer = ArchiveWriter::new(&path);
        let r = report(
            0,
            vec![D3d12RenderDrawObservation {
                present_seq: 1,
                ..Default::default()
            }],
        );
        let mut bytes = Vec::new();
        writer.write_report(&mut bytes, &r).unwrap();
        assert_eq!(std::fs::metadata(path.join(FILE)).unwrap().len(), 0);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["observation"]["observations"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            v["observation_archive"]["sha256"],
            format!("sha256:{:x}", Sha256::digest([]))
        );
        let mut failed = report(0, vec![]);
        failed
            .observation
            .local_archive
            .as_mut()
            .unwrap()
            .enqueue_failures = 1;
        let mut failure_bytes = Vec::new();
        writer.write_report(&mut failure_bytes, &failed).unwrap();
        let failure: serde_json::Value = serde_json::from_slice(&failure_bytes).unwrap();
        assert_eq!(
            failure["observation"]["local_archive"]["enqueue_failures"],
            1
        );
        writer.bytes = MAX_BYTES;
        assert!(
            writer
                .append(&[D3d12RenderDrawObservation::default()])
                .unwrap_err()
                .contains("disk budget")
        );
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn camera_archive_expansion_ceiling_rejects_before_writing_an_extra_block() {
        let path = directory();
        let mut writer = ArchiveWriter::new(&path);
        writer.append(&[]).unwrap();
        let row = serde_json::value::to_raw_value(&D3d12RenderDrawObservation::default()).unwrap();
        let expanded = row.get().len() as u64 + 1;
        // Exercise cumulative accounting with a small fixture, not a 512 MiB allocation.
        writer.expanded_bytes = depth_lab::cp2077_camera_report::EXPANDED_REPORT_BYTES - expanded;
        writer.append_block(std::slice::from_ref(&row)).unwrap();
        assert_eq!(
            writer.expanded_bytes,
            depth_lab::cp2077_camera_report::EXPANDED_REPORT_BYTES
        );
        writer.file.as_mut().unwrap().flush().unwrap();
        let before = std::fs::read(&writer.path).unwrap();
        let counts = (writer.bytes, writer.rows, writer.blocks);
        assert!(
            writer
                .append_block(&[row])
                .unwrap_err()
                .contains("expansion budget")
        );
        assert_eq!((writer.bytes, writer.rows, writer.blocks), counts);
        assert_eq!(std::fs::read(&writer.path).unwrap(), before);
        drop(writer);
        cleanup(&path);
    }

    #[test]
    fn camera_archive_preserves_numeric_tokens_without_float_reparse() {
        use crate::d3d12_render_camera::D3d12RenderMatrixObservation;
        let path = directory();
        let mut writer = ArchiveWriter::new(&path);
        let values = [-1.0000003576278687_f64, -0.0, f64::from_bits(1)];
        for (index, value) in values.iter().enumerate() {
            writer
                .append(&[D3d12RenderDrawObservation {
                    present_seq: u64::MAX - index as u64,
                    matrices: vec![D3d12RenderMatrixObservation {
                        copy_receipt: None,
                        source_tag: 1,
                        root_parameter_index: 0,
                        cbv_resource_offset: 0,
                        matrix_offset: 0,
                        transpose: false,
                        matrix_kind: "view".into(),
                        matrix: [[0.0; 4]; 4],
                        translation_slot: None,
                        basis_determinant: Some(*value),
                        clip_w_sign: None,
                        projection_a: None,
                        projection_b: None,
                        vertical_fov_degrees: None,
                        horizontal_fov_degrees: None,
                        configured_vertical_fov_matches: None,
                        handedness_observation: String::new(),
                        axis_mapping: String::new(),
                        physical_source: String::new(),
                        stable_source: None,
                    }],
                    ..Default::default()
                }])
                .unwrap();
        }
        let mut output = Vec::new();
        // Keep the same awkward values in the live tail too. Both storage paths must
        // preserve the native serializer's spelling, including negative zero/subnormals.
        let tail: Vec<D3d12RenderDrawObservation> = restored(&writer)
            .iter()
            .map(|raw| serde_json::from_str(raw.get()).unwrap())
            .collect();
        writer
            .write_report(&mut output, &report(values.len() as u64, tail.clone()))
            .unwrap();
        let tail_report = String::from_utf8(output).unwrap();
        for row in &tail {
            assert!(tail_report.contains(&serde_json::to_string(row).unwrap()));
        }
        let text = restored(&writer)
            .iter()
            .map(|r| r.get())
            .collect::<Vec<_>>()
            .join("\n");
        // Compare exact JSON numeric tokens: decoding both sides through the
        // same lossy float parser would conceal the regression.
        for (index, value) in values.iter().enumerate() {
            let token = format!(
                "\"basis_determinant\":{}",
                serde_json::to_string(value).unwrap()
            );
            assert!(
                text.contains(&token),
                "archived numeric token changed: {token}"
            );
            assert!(text.contains(&format!("\"present_seq\":{}", u64::MAX - index as u64)));
        }
        drop(writer);
        cleanup(&path);
    }

    #[test]
    #[ignore = "requires an explicit immutable local camera archive replay manifest"]
    fn replay_camera_archive_preserves_every_original_row() {
        use serde_json::value::RawValue;
        let manifest = std::env::var("CP2077_CAMERA_ARCHIVE_REPLAY").unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let source = Path::new(config["archive"].as_str().unwrap());
        let source_bytes = std::fs::read(source).unwrap();
        assert!(source_bytes.len() as u64 <= MAX_BYTES);
        let hash = format!("sha256:{:x}", Sha256::digest(&source_bytes));
        assert_eq!(config["sha256"], hash);
        let source_rows: Vec<_> = source_bytes
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(config["rows"], source_rows.len());
        let path = PathBuf::from(config["derived_directory"].as_str().unwrap());
        std::fs::create_dir(&path).unwrap();
        let mut writer = ArchiveWriter::new(&path);
        // Match native retirement batches by Present, not an artificially large whole-run dictionary.
        let mut batch = Vec::new();
        let mut previous = None;
        for line in &source_rows {
            let fields: serde_json::Value = serde_json::from_slice(line).unwrap();
            let present = fields["present_seq"].as_u64().unwrap();
            if previous.is_some_and(|p| p != present) {
                writer.append_tokens(batch.drain(..).map(Ok)).unwrap();
            }
            previous = Some(present);
            batch.push(serde_json::from_slice::<Box<RawValue>>(line).unwrap());
        }
        writer.append_tokens(batch.into_iter().map(Ok)).unwrap();
        let mut output = Vec::new();
        writer
            .write_report(&mut output, &report(writer.rows, vec![]))
            .unwrap();
        // Keep an explicitly DERIVED artifact outside the source session for client/Python replay.
        std::fs::write(path.join("render-camera-observation.json"), &output).unwrap();
        let replay = restored(&writer);
        assert_eq!(replay.len(), source_rows.len());
        for (original, final_row) in source_rows.iter().zip(&replay) {
            assert_eq!(*original, final_row.get().as_bytes());
        }
        assert_eq!(source_bytes, std::fs::read(source).unwrap());
        let receipt = serde_json::json!({
            "schema":"local-cp2077-camera-archive-serialization-replay/v1",
            "non_trainable":true,"acceptance":false,"source_sha256":hash,
            "rows_checked_byte_exact":source_rows.len(),"source_unchanged":true,
            "encoded_archive_bytes":writer.bytes,"expanded_archive_bytes":writer.expanded_bytes,
            "encoded_blocks":writer.blocks,"source_archive_bytes":source_bytes.len(),
            "raw_session_rewritten":false
        });
        let output_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(config["output"].as_str().unwrap())
            .unwrap();
        serde_json::to_writer_pretty(output_file, &receipt).unwrap();
        drop(writer);
    }
}
