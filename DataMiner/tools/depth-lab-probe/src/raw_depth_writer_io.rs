//! Timed, durable file writes for the local raw-depth diagnostic lane only.
//! The file must be synchronized successfully before its caller publishes an index row.

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

const SLOW_SAMPLE_LIMIT: usize = 16;

#[derive(Default, Serialize)]
pub(crate) struct StageTiming {
    calls: u64,
    total_us: u64,
    max_us: u64,
    max_ordinal: u64,
}

impl StageTiming {
    fn record(&mut self, ordinal: u64, micros: u64) {
        self.calls += 1;
        self.total_us = self.total_us.saturating_add(micros);
        if micros >= self.max_us {
            self.max_us = micros;
            self.max_ordinal = ordinal;
        }
    }
}

#[derive(Default, Serialize)]
struct FrameTiming {
    ordinal: u64,
    byte_len: usize,
    queue_wait_us: u64,
    create_us: u64,
    write_us: u64,
    sync_us: u64,
    close_us: u64,
    total_us: u64,
    failed_stage: Option<&'static str>,
}

#[derive(Default, Serialize)]
pub(crate) struct WriterIoMetrics {
    files_synchronized: u64,
    failed_files: u64,
    queue_wait: StageTiming,
    create: StageTiming,
    write: StageTiming,
    sync: StageTiming,
    close: StageTiming,
    total: StageTiming,
    slowest_files: Vec<FrameTiming>,
}

impl WriterIoMetrics {
    fn record(&mut self, sample: FrameTiming) {
        self.queue_wait.record(sample.ordinal, sample.queue_wait_us);
        self.create.record(sample.ordinal, sample.create_us);
        if sample.failed_stage != Some("create") {
            self.write.record(sample.ordinal, sample.write_us);
            self.close.record(sample.ordinal, sample.close_us);
            if sample.failed_stage != Some("write") {
                self.sync.record(sample.ordinal, sample.sync_us);
            }
        }
        self.total.record(sample.ordinal, sample.total_us);
        if sample.failed_stage.is_some() {
            self.failed_files += 1;
        } else {
            self.files_synchronized += 1;
        }
        self.slowest_files.push(sample);
        self.slowest_files
            .sort_unstable_by_key(|row| std::cmp::Reverse(row.total_us));
        self.slowest_files.truncate(SLOW_SAMPLE_LIMIT);
    }
}

fn elapsed_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

pub(crate) fn write_durable_file(
    path: &Path,
    bytes: &[u8],
    ordinal: u64,
    enqueued_at: Instant,
    metrics: &mut WriterIoMetrics,
) -> Result<(), String> {
    let started = Instant::now();
    let mut sample = FrameTiming {
        ordinal,
        byte_len: bytes.len(),
        queue_wait_us: elapsed_us(enqueued_at),
        ..FrameTiming::default()
    };
    let stage = Instant::now();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    sample.create_us = elapsed_us(stage);
    let result = match output {
        Err(error) => {
            sample.failed_stage = Some("create");
            Err(format!("create raw D32 frame failed: {error}"))
        }
        Ok(mut output) => {
            let stage = Instant::now();
            let write = output.write_all(bytes);
            sample.write_us = elapsed_us(stage);
            let result = match write {
                Err(error) => {
                    sample.failed_stage = Some("write");
                    Err(format!("write raw D32 frame failed: {error}"))
                }
                Ok(()) => {
                    let stage = Instant::now();
                    let sync = output.sync_all();
                    sample.sync_us = elapsed_us(stage);
                    sync.map_err(|error| {
                        sample.failed_stage = Some("sync");
                        format!("sync raw D32 frame failed: {error}")
                    })
                }
            };
            let stage = Instant::now();
            drop(output);
            sample.close_us = elapsed_us(stage);
            result
        }
    };
    sample.total_us = elapsed_us(started);
    metrics.record(sample);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fresh_directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("dm-writer-io-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn durable_write_preserves_bytes_and_refuses_overwrite() {
        let directory = fresh_directory();
        let path = directory.join("frame.bin");
        let bytes = [0, 1, 254, 255, 13, 10];
        let mut metrics = WriterIoMetrics::default();
        write_durable_file(&path, &bytes, 0, Instant::now(), &mut metrics).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            write_durable_file(&path, b"replacement", 1, Instant::now(), &mut metrics).is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(metrics.files_synchronized, 1);
        assert_eq!(metrics.failed_files, 1);
        assert_eq!(metrics.create.calls, 2);
        assert_eq!(metrics.write.calls, 1);
        assert_eq!(metrics.sync.calls, 1);
        assert_eq!(metrics.close.calls, 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn metrics_keep_bounded_slowest_files_and_count_every_failure() {
        let mut metrics = WriterIoMetrics::default();
        for ordinal in 0..100 {
            metrics.record(FrameTiming {
                ordinal,
                total_us: ordinal,
                failed_stage: Some("sync"),
                ..FrameTiming::default()
            });
        }
        assert_eq!(metrics.files_synchronized, 0);
        assert_eq!(metrics.failed_files, 100);
        assert_eq!(metrics.sync.calls, 100);
        assert_eq!(metrics.total.max_ordinal, 99);
        assert_eq!(metrics.slowest_files.len(), SLOW_SAMPLE_LIMIT);
        assert_eq!(metrics.slowest_files.last().unwrap().ordinal, 84);
    }

    /// Explicit local disk replay, never a game/RGB/depth certification. The output
    /// directory must be new. Only generated replay files are removed after audit.
    #[test]
    #[ignore = "writes up to 8.5 GB at 16 Hz to an explicitly configured disk"]
    fn paced_local_storage_replay() {
        use std::sync::mpsc::{TrySendError, sync_channel};

        let output =
            std::path::PathBuf::from(std::env::var_os("DM_WRITER_REPLAY_OUTPUT").expect("output"));
        let source =
            std::path::PathBuf::from(std::env::var_os("DM_WRITER_REPLAY_FRAME").expect("source"));
        let frames: u64 = std::env::var("DM_WRITER_REPLAY_FRAMES")
            .unwrap_or_else(|_| "128".into())
            .parse()
            .unwrap();
        assert!(output.is_absolute() && source.is_absolute());
        assert!((2..=1024).contains(&frames));
        let bytes = std::fs::read(source).unwrap();
        assert_eq!(bytes.len(), 1920 * 1080 * 4);
        std::fs::create_dir(&output).expect("refusing an existing replay directory");
        let output = output.canonicalize().unwrap();
        let writer_output = output.clone();
        let (tx, rx) = sync_channel::<(u64, Vec<u8>, Instant)>(32);
        let worker = std::thread::spawn(move || {
            let mut metrics = WriterIoMetrics::default();
            for (ordinal, bytes, enqueued) in rx {
                write_durable_file(
                    &writer_output.join(format!("replay-{ordinal:04}.bin")),
                    &bytes,
                    ordinal,
                    enqueued,
                    &mut metrics,
                )
                .unwrap();
            }
            metrics
        });
        let started = Instant::now();
        let mut admitted = 0;
        let mut full = 0;
        let mut max_producer_lateness_us = 0;
        for ordinal in 0..frames {
            let due = started + Duration::from_micros(ordinal * 62_500);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            max_producer_lateness_us = max_producer_lateness_us.max(elapsed_us(due));
            match tx.try_send((ordinal, bytes.clone(), Instant::now())) {
                Ok(()) => admitted += 1,
                Err(TrySendError::Full(_)) => {
                    full += 1;
                    break;
                }
                Err(error) => panic!("writer disconnected: {error}"),
            }
        }
        drop(tx);
        let metrics = worker.join().unwrap();
        let replay_elapsed_ms = started.elapsed().as_millis();
        // Verify only files created by this replay before removing them. Source
        // capture files and the new report are never removed or rewritten.
        for ordinal in 0..admitted {
            let path = output.join(format!("replay-{ordinal:04}.bin"));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            std::fs::remove_file(path).unwrap();
        }
        let report = serde_json::json!({
            "schema": "dm-local-raw-writer-storage-replay/v1", "requested_frames": frames,
            "admitted_frames": admitted, "queue_full": full, "queue_capacity": 32,
            "replay_elapsed_ms": replay_elapsed_ms, "producer_max_lateness_us": max_producer_lateness_us,
            "writer_io": metrics, "generated_files_verified_and_removed": admitted,
            "game_capture": false, "production_certified": false
        });
        let mut report_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join("replay-report.json"))
            .unwrap();
        serde_json::to_writer_pretty(&mut report_file, &report).unwrap();
        report_file.sync_all().unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
        assert_eq!(full, 0, "storage replay filled the bounded writer queue");
        assert_eq!(admitted, frames);
    }
}
