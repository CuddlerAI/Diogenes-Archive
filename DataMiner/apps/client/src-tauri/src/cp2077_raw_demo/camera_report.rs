//! Offline camera-report reader. Large archived reports do not widen other evidence limits.
use super::*;
use depth_lab::cp2077_camera_report::{
    ARCHIVE_BYTES, EXPANDED_REPORT_BYTES, LEGACY_REPORT_BYTES, REPORT_BYTES,
    archive as archive_codec, compact,
};
use std::io::BufReader;

#[derive(Debug)]
pub(super) struct CameraReport {
    pub value: Value,
    pub sha256: String,
    verified_archive: bool,
    #[cfg(test)]
    pub bytes: u64,
}

impl CameraReport {
    pub(super) fn archive_verified(&self) -> bool {
        self.verified_archive
    }
}

struct HashReader<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}
impl<R: io::Read> io::Read for HashReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buffer)?;
        self.hash.update(&buffer[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

pub(super) fn read(path: &Path) -> anyhow::Result<CameraReport> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
        "camera report is not a regular non-reparse file"
    );
    ensure!(
        metadata.len() > 0 && metadata.len() <= REPORT_BYTES,
        "camera report exceeds its {REPORT_BYTES}-byte bound"
    );
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() == metadata.len(),
        "camera report changed before read"
    );
    let mut reader = HashReader {
        inner: BufReader::new(file.take(REPORT_BYTES + 1)),
        hash: Sha256::new(),
        bytes: 0,
    };
    // Parse the full bounded tree, but avoid retaining another report-sized byte Vec.
    let mut value: Value =
        serde_json::from_reader(&mut reader).context("invalid camera report JSON")?;
    ensure!(
        reader.bytes == metadata.len(),
        "camera report length changed during read"
    );
    ensure!(supported_schema(&value), "camera report schema mismatch");
    if value.get("observation_archive").is_some() {
        validate_archive(path, &mut value, reader.bytes)?;
    } else {
        ensure!(
            reader.bytes <= LEGACY_REPORT_BYTES && value["schema"] == RENDER_CAMERA_REPORT_SCHEMA,
            "unarchived camera report exceeds legacy bound"
        );
    }
    Ok(CameraReport {
        verified_archive: value.get("observation_archive").is_some(),
        value,
        sha256: format!("sha256:{:x}", reader.hash.finalize()),
        #[cfg(test)]
        bytes: reader.bytes,
    })
}

pub(super) fn supported_schema(value: &Value) -> bool {
    matches!(
        value.get("schema").and_then(Value::as_str),
        Some(RENDER_CAMERA_REPORT_SCHEMA) | Some(compact::REPORT_SCHEMA)
    )
}

fn validate_archive(path: &Path, report: &mut Value, report_bytes: u64) -> anyhow::Result<()> {
    let archive = &report["observation_archive"];
    let is_compact = report["schema"] == compact::REPORT_SCHEMA;
    ensure!(
        archive["schema"]
            == if is_compact {
                compact::ARCHIVE_SCHEMA
            } else {
                "dm-cp2077-camera-archive/v1"
            }
            && archive["non_trainable"] == true
            && archive["durable"] == true
            && archive["final_observations_include_archive"] == !is_compact
            && archive["disk_budget_bytes"] == ARCHIVE_BYTES
            && archive["active_draw_capacity"] == 4096
            && archive["file"] == "render-camera-draws.jsonl",
        "invalid camera archive declaration"
    );
    let rows = required_u64(archive, "archived_draws")?;
    let bytes = required_u64(archive, "bytes")?;
    ensure!(bytes <= ARCHIVE_BYTES, "camera archive exceeds disk bound");
    if is_compact {
        ensure!(
            archive["encoding"] == compact::BLOCK_SCHEMA
                && report_bytes <= LEGACY_REPORT_BYTES
                && required_u64(archive, "expanded_bytes")? <= EXPANDED_REPORT_BYTES - report_bytes,
            "compact camera report encoding/expansion exceeds bound"
        );
    }
    let active = &report["observation"]["local_archive"];
    ensure!(
        active["schema"] == "dm-cp2077-camera-active-window/v1"
            && active["frozen"] == true
            && active["enqueue_failures"] == 0
            && active["enqueued_draws"] == rows,
        "camera archive is not completely frozen"
    );
    let observations = report["observation"]["observations"]
        .as_array()
        .context("camera report observations missing")?;
    ensure!(
        if is_compact {
            observations.len() <= 4096
        } else {
            rows <= observations.len() as u64 && observations.len() as u64 - rows <= 4096
        },
        "camera archive/active observation count mismatch"
    );
    let archive_path = path.with_file_name("render-camera-draws.jsonl");
    let metadata = fs::symlink_metadata(&archive_path)?;
    ensure!(
        metadata.is_file() && metadata.file_attributes() & 0x400 == 0 && metadata.len() == bytes,
        "camera archive file/size mismatch"
    );
    let mut hash = Sha256::new();
    let mut expanded_hash = Sha256::new();
    let mut restored = Vec::new();
    let mut count = 0_u64;
    let checked = archive_codec::visit(
        BufReader::new(File::open(archive_path)?.take(ARCHIVE_BYTES + 1)),
        is_compact,
        |line| hash.update(line),
        |raw| {
            if count >= rows {
                return Err("camera archive has extra draws".into());
            }
            expanded_hash.update(raw.get().as_bytes());
            expanded_hash.update(b"\n");
            let row: Value = serde_json::from_str(raw.get()).map_err(|e| e.to_string())?;
            if is_compact {
                restored.push(row);
            } else if row != observations[count as usize] {
                return Err(format!(
                    "camera archive observation differs from final report at row {count}"
                ));
            }
            count += 1;
            Ok(())
        },
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        checked.rows == rows
            && checked.bytes == bytes
            && archive["sha256"] == format!("sha256:{:x}", hash.finalize()),
        "camera archive hash/count mismatch"
    );
    if is_compact {
        ensure!(
            archive["blocks"] == checked.blocks
                && archive["expanded_bytes"] == checked.expanded_bytes
                && checked.expanded_bytes <= EXPANDED_REPORT_BYTES - report_bytes
                && archive["expanded_sha256"] == format!("sha256:{:x}", expanded_hash.finalize()),
            "camera archive expanded hash/count mismatch"
        );
        // Internal, checked full view only. Keep physical v3 schema and original-file SHA;
        // never serialize this expanded view back into an immutable session.
        restored.append(
            report["observation"]["observations"]
                .as_array_mut()
                .unwrap(),
        );
        report["observation"]["observations"] = Value::Array(restored);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> (PathBuf, Value) {
        let archive = b"{\"value\":0.1}\n";
        fs::write(root.join("render-camera-draws.jsonl"), archive).unwrap();
        (
            root.join("render-camera-observation.json"),
            json!({
            "schema":RENDER_CAMERA_REPORT_SCHEMA,
            "observation":{"observations":[{"value":0.1}],"local_archive":{
                "schema":"dm-cp2077-camera-active-window/v1","frozen":true,
                "enqueue_failures":0,"enqueued_draws":1}},
            "observation_archive":{"schema":"dm-cp2077-camera-archive/v1",
                "non_trainable":true,"durable":true,"final_observations_include_archive":true,
                "disk_budget_bytes":ARCHIVE_BYTES,"active_draw_capacity":4096,
                "file":"render-camera-draws.jsonl","archived_draws":1,"bytes":archive.len(),
                "sha256":sha256_bytes(archive)}}),
        )
    }

    #[test]
    fn archived_report_larger_than_64_mib_is_read_without_widening_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let (path, report) = fixture(dir.path());
        let mut file = File::create(&path).unwrap();
        serde_json::to_writer(&mut file, &report).unwrap();
        let spaces = vec![b' '; 1024 * 1024];
        for _ in 0..64 {
            file.write_all(&spaces).unwrap();
        }
        drop(file);
        let checked = read(&path).unwrap();
        assert!(checked.bytes > LEGACY_REPORT_BYTES);
        assert_eq!(checked.sha256, sha256_file(&path).unwrap());
        let mut legacy = report;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("observation_archive");
        let mut file = File::create(&path).unwrap();
        serde_json::to_writer(&mut file, &legacy).unwrap();
        for _ in 0..64 {
            file.write_all(&spaces).unwrap();
        }
        drop(file);
        assert!(
            read(&path)
                .unwrap_err()
                .to_string()
                .contains("legacy bound")
        );
    }

    #[test]
    fn archive_corruption_and_unfrozen_reports_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (path, report) = fixture(dir.path());
        for field in ["sha256", "archived_draws", "file", "bytes"] {
            let mut bad = report.clone();
            bad["observation_archive"][field] = Value::Null;
            fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(read(&path).is_err(), "{field}");
        }
        let mut bad = report.clone();
        bad["observation"]["local_archive"]["frozen"] = json!(false);
        fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(read(&path).is_err());
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        fs::write(
            dir.path().join("render-camera-draws.jsonl"),
            b"{\"value\":0.2}\n",
        )
        .unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn oversized_or_truncated_camera_reports_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        File::create(&path)
            .unwrap()
            .set_len(REPORT_BYTES + 1)
            .unwrap();
        assert!(read(&path).is_err());
        fs::write(&path, b"{\"schema\":").unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn compact_report_restores_exact_draws_and_requires_all_digests_and_counts() {
        use serde_json::value::RawValue;
        let dir = tempfile::tempdir().unwrap();
        let (path, mut report) = fixture(dir.path());
        let raw = RawValue::from_string(
            r#"{"value":-1.0000003576278687,"id":18446744073709551615}"#.into(),
        )
        .unwrap();
        let block = compact::Block::encode(&[&raw, &raw]).unwrap();
        let mut encoded = serde_json::to_vec(&block).unwrap();
        encoded.push(b'\n');
        fs::write(dir.path().join("render-camera-draws.jsonl"), &encoded).unwrap();
        let expanded = format!("{}\n{}\n", raw.get(), raw.get());
        report["schema"] = json!(compact::REPORT_SCHEMA);
        report["observation"]["observations"] = json!([{"tail":true}]);
        report["observation"]["local_archive"]["enqueued_draws"] = json!(2);
        let a = &mut report["observation_archive"];
        a["schema"] = json!(compact::ARCHIVE_SCHEMA);
        a["encoding"] = json!(compact::BLOCK_SCHEMA);
        a["final_observations_include_archive"] = json!(false);
        a["archived_draws"] = json!(2);
        a["bytes"] = json!(encoded.len());
        a["sha256"] = json!(sha256_bytes(&encoded));
        a["blocks"] = json!(1);
        a["expanded_bytes"] = json!(expanded.len());
        a["expanded_sha256"] = json!(sha256_bytes(expanded.as_bytes()));
        let original = serde_json::to_vec(&report).unwrap();
        fs::write(&path, &original).unwrap();
        let checked = read(&path).unwrap();
        let rows = checked.value["observation"]["observations"]
            .as_array()
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], rows[1]);
        assert_eq!(rows[0]["id"], u64::MAX);
        assert_eq!(rows[2]["tail"], true);
        assert_eq!(checked.value["schema"], compact::REPORT_SCHEMA);
        assert_eq!(checked.sha256, sha256_bytes(&original));
        assert_eq!(fs::read(&path).unwrap(), original);
        for field in [
            "schema",
            "encoding",
            "blocks",
            "expanded_bytes",
            "expanded_sha256",
            "archived_draws",
            "final_observations_include_archive",
        ] {
            let mut bad = report.clone();
            bad["observation_archive"][field] = Value::Null;
            fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(read(&path).is_err(), "{field}");
        }
        // A legacy-sized expansion is within the new budget, but a false declaration
        // must still fail the exact decoded count/hash validation.
        report["observation_archive"]["expanded_bytes"] = json!(REPORT_BYTES);
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(
            read(&path)
                .unwrap_err()
                .to_string()
                .contains("expanded hash/count")
        );
        report["observation_archive"]["expanded_bytes"] = json!(EXPANDED_REPORT_BYTES);
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(
            read(&path)
                .unwrap_err()
                .to_string()
                .contains("encoding/expansion exceeds bound")
        );
        report
            .as_object_mut()
            .unwrap()
            .remove("observation_archive");
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    #[ignore = "requires explicit immutable local camera reports and a fresh output path"]
    fn replay_real_archived_camera_reports() {
        let manifest: Value = serde_json::from_slice(
            &fs::read(
                std::env::var("CP2077_CAMERA_READER_REPLAY").expect("explicit replay manifest"),
            )
            .unwrap(),
        )
        .unwrap();
        let mut rows = Vec::new();
        for entry in manifest["reports"].as_array().unwrap() {
            let path = Path::new(entry["path"].as_str().unwrap());
            let checked = read(path).unwrap();
            assert_eq!(checked.sha256, entry["sha256"]);
            assert_eq!(sha256_file(path).unwrap(), checked.sha256);
            rows.push(json!({"bytes":checked.bytes,"sha256":checked.sha256,
                "observations":checked.value["observation"]["observations"].as_array().unwrap().len()}));
        }
        write_json_create_new(
            Path::new(manifest["output"].as_str().unwrap()),
            &json!({"schema":"cp2077-camera-reader-replay/v1","non_trainable":true,
                "acceptance":false,"reports":rows}),
        )
        .unwrap();
    }
}
