//! Bounded, opt-in measurement, separate from immutable recording artifacts.
//! Producers only try_send fixed-size events. No formatting, allocation or disk
//! writes on the capture path. File order is NOT cross-thread temporal order.

use crate::{capture::SelectedSourceIdentity, qpc::QpcClock};
#[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Geometry only. This diagnostic never expands low16 or supplies transport identity.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RawWatermarkTraceConfig {
    pub source_width: u32,
    pub source_height: u32,
}

#[cfg(all(feature = "cadence-trace", debug_assertions))]
pub struct CadenceTraceConfig {
    /// Must be an absolute path outside the session artifact directory. Created
    /// exclusively before capture; populated only after the recorder returns.
    /// None disables all trace collection, including extra QPC reads. This is
    /// independent of the timing override, so policy A/B can run without tracing.
    pub output_path: Option<std::path::PathBuf>,
    pub max_events: usize,
    pub raw_watermark: Option<RawWatermarkTraceConfig>,
    /// None uses the ordinary recording policy. Some(4) requests the legacy
    /// fixed-4ms baseline; Some(16) requests fixed-16ms at exactly 30 FPS only.
    /// Explicit overrides remain engineering-only, independent of trace on/off.
    pub experimental_seal_delay_ms: Option<u8>,
    /// With the normal 4ms budget, allow only candidate-empty 30 FPS slots to
    /// wait until end+16ms. A validated candidate restores the normal deadline.
    pub experimental_empty_slot_grace: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceStage {
    CallbackStart,
    TimestampIgnored,
    TimestampFatal,
    RingExhausted,
    Dequeued,
    ReusedFuture,
    GpuReady,
    WatermarkRejected,
    WatermarkStale,
    WatermarkReady,
    StartupArm,
    BeforeArm,
    StartupEligible,
    CutoffRecycle,
    ShutdownRecycle,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SealReason {
    FutureSource,
    QueueEmptyAtDeadline,
    WaitTimeout,
    Control,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Slot {
    pub run_frame: u64,
    pub start_qpc: i64,
    pub end_qpc: i64,
    pub seal_qpc: i64,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Event {
    RawWatermark {
        source_seq: u64,
        source_qpc: i64,
        raw_present_low16: Option<u16>,
        status: &'static str,
        decode_started_qpc: Option<i64>,
    },
    TimestampRejected {
        source_seq: u64,
        source_qpc: i64,
        previous_source_qpc: Option<i64>,
        streak: u64,
        fatal: bool,
    },
    Source {
        stage: SourceStage,
        source_seq: u64,
        source_qpc: i64,
        present_seq: Option<u64>,
        // None denotes startup, before the CFR anchor exists.
        run_frame: Option<u64>,
        queue_depth: usize,
    },
    Enqueue {
        source_seq: u64,
        source_qpc: i64,
        callback_start_qpc: i64,
        copy_submitted_qpc: i64,
        // try_send occurred between these two clock reads. A dequeue can
        // legitimately precede the upper bound or this trace event's insertion.
        send_before_qpc: Option<i64>,
        send_after_qpc: Option<i64>,
        queued: bool,
        queue_depth_before: usize,
        queue_depth_after: usize,
    },
    SlotOpen {
        slot: Slot,
        queue_depth: usize,
    },
    Poll {
        slot: Slot,
        before_qpc: i64,
        result: &'static str,
        queue_depth: usize,
    },
    /// The final queue check is bracketed by before_qpc and observed_qpc. A
    /// recovery is a source arriving after the earlier empty poll/timeout, not
    /// necessarily a source whose timestamp belongs to the current slot.
    SealRecheck {
        slot: Slot,
        before_qpc: Option<i64>,
        recovered_source_seq: Option<u64>,
        queue_depth: usize,
    },
    Observe {
        slot: Slot,
        source_seq: u64,
        source_qpc: i64,
        present_seq: Option<u64>,
        future: bool,
        replaced_source_seq: Option<u64>,
    },
    Seal {
        slot: Slot,
        reason: SealReason,
        source_seq: u64,
        source_qpc: i64,
        present_seq: Option<u64>,
        duplicate: bool,
        superseded: u32,
        queue_depth: usize,
    },
    // Sealed decisions can still be abandoned by cutoff/error before submission.
    OutputSubmitted {
        run_frame: u64,
        source_seq: u64,
    },
}

impl Event {
    pub(crate) fn source(
        stage: SourceStage,
        identity: SelectedSourceIdentity,
        run_frame: Option<u64>,
        queue_depth: usize,
    ) -> Self {
        Self::Source {
            stage,
            source_seq: identity.source_seq,
            source_qpc: identity.source_qpc,
            present_seq: identity.present_seq,
            run_frame,
            queue_depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Entry {
    observed_qpc: Option<i64>,
    #[serde(flatten)]
    event: Event,
}

struct Buffer {
    tx: Sender<Entry>,
    #[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
    rx: Receiver<Entry>,
    dropped: AtomicU64,
    clock_errors: AtomicU64,
    producers: AtomicU64,
    raw_watermark: Option<RawWatermarkTraceConfig>,
    last_raw_source: AtomicU64,
}

// WinRT delegates can outlive the joined control thread. The handler lifetime
// covers pending and running callbacks, unlike a counter around try_send alone.
pub(crate) struct TraceProducer(Arc<Buffer>);
impl Drop for TraceProducer {
    fn drop(&mut self) {
        self.0.producers.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone, Default)]
pub(crate) struct CadenceTrace(Option<Arc<Buffer>>);

impl CadenceTrace {
    pub(crate) fn raw_watermark(&self) -> Option<RawWatermarkTraceConfig> {
        self.0.as_ref().and_then(|b| b.raw_watermark)
    }

    // Before CFR observe: retain the identity even when this candidate is replaced.
    // Future-source retries do not map the same texture again. Errors are diagnostic
    // unknowns and never become present_seq or change a selection decision.
    pub(crate) fn observe_raw_watermark(
        &self,
        clock: QpcClock,
        identity: SelectedSourceIdentity,
        decode: impl FnOnce() -> anyhow::Result<u16>,
    ) {
        let Some(buffer) = &self.0 else {
            return;
        };
        if buffer.raw_watermark.is_none()
            || buffer.last_raw_source.load(Ordering::Relaxed) == identity.source_seq
        {
            return;
        }
        buffer
            .last_raw_source
            .store(identity.source_seq, Ordering::Relaxed);
        let decode_started_qpc = self.now(clock);
        let (raw_present_low16, status) = match decode() {
            Ok(value) => (Some(value), "decoded_low16_only"),
            Err(error) => {
                use present_identity::watermark::DecodeError;
                (
                    None,
                    match error.downcast_ref::<DecodeError>() {
                        Some(DecodeError::IntegrityMismatch) => "integrity_mismatch",
                        Some(DecodeError::AmbiguousBlock) => "ambiguous_block",
                        _ => "readback_or_layout_error",
                    },
                )
            }
        };
        self.emit(clock, || Event::RawWatermark {
            source_seq: identity.source_seq,
            source_qpc: identity.source_qpc,
            raw_present_low16,
            status,
            decode_started_qpc,
        });
    }

    #[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
    fn bounded(capacity: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=65_536).contains(&capacity),
            "cadence trace capacity must be 1..=65536"
        );
        let (tx, rx) = crossbeam_channel::bounded(capacity);
        Ok(Self(Some(Arc::new(Buffer {
            tx,
            rx,
            dropped: AtomicU64::new(0),
            clock_errors: AtomicU64::new(0),
            producers: AtomicU64::new(0),
            raw_watermark: None,
            last_raw_source: AtomicU64::new(0),
        }))))
    }

    pub(crate) fn producer(&self) -> Option<TraceProducer> {
        self.0.as_ref().map(|buffer| {
            buffer.producers.fetch_add(1, Ordering::Relaxed);
            TraceProducer(Arc::clone(buffer))
        })
    }

    // Keep every additional clock read behind the opt-in branch. A diagnostic
    // clock failure is counted, never substituted with a fabricated timestamp.
    #[inline]
    pub(crate) fn now(&self, clock: QpcClock) -> Option<i64> {
        self.0.as_ref().and_then(|buffer| match clock.now() {
            Ok(qpc) => Some(qpc),
            Err(_) => {
                buffer.clock_errors.fetch_add(1, Ordering::Relaxed);
                None
            }
        })
    }

    #[inline]
    pub(crate) fn emit(&self, clock: QpcClock, event: impl FnOnce() -> Event) {
        if self.0.is_some() {
            self.at(self.now(clock), event);
        }
    }

    #[inline]
    pub(crate) fn at(&self, observed_qpc: Option<i64>, event: impl FnOnce() -> Event) {
        if let Some(buffer) = &self.0
            && buffer
                .tx
                .try_send(Entry {
                    observed_qpc,
                    event: event(),
                })
                .is_err()
        {
            buffer.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
pub(crate) struct TraceFile {
    file: std::fs::File,
    trace: CadenceTrace,
    header: serde_json::Value,
}

#[cfg(any(test, all(feature = "cadence-trace", debug_assertions)))]
impl TraceFile {
    pub(crate) fn create(
        path: &std::path::Path,
        capacity: usize,
        header: serde_json::Value,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(path.is_absolute(), "cadence trace path must be absolute");
        let trace = CadenceTrace::bounded(capacity)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            file,
            trace,
            header,
        })
    }

    #[cfg(all(feature = "cadence-trace", debug_assertions))]
    pub(crate) fn with_raw_watermark(mut self, config: Option<RawWatermarkTraceConfig>) -> Self {
        Arc::get_mut(self.trace.0.as_mut().expect("new trace"))
            .expect("unshared trace")
            .raw_watermark = config;
        self
    }

    pub(crate) fn trace(&self) -> CadenceTrace {
        self.trace.clone()
    }

    /// Caller must have joined all producers. A footer is written last: absence
    /// of the footer, dropped events or failed clock reads means incomplete.
    pub(crate) fn finish(self, recording_ok: bool) -> anyhow::Result<()> {
        use std::io::{BufWriter, Write};
        let buffer = self.trace.0.as_ref().expect("trace file has a buffer");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while buffer.producers.load(Ordering::Acquire) != 0 {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "cadence source handler still alive after shutdown; trace left incomplete"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let capacity = buffer.tx.capacity().expect("bounded trace");
        let mut writer = BufWriter::new(self.file);
        let header = serde_json::json!({"kind":"header", "schema":"capture-cadence-trace/v2",
            "context":self.header, "capacity_events":capacity, "non_trainable":true,
            "raw_watermark":buffer.raw_watermark, "raw_identity_scope":"low16 only; never full Present transport; timestamp-rejected sources have no ROI",
            "event_bytes":std::mem::size_of::<Entry>(),
            "callback_start_scope":"GpuCopyHandler entry, after WGC wrapper and callback lock",
            "ordering":"join by source_seq/run_frame and QPC; file order is not cross-thread chronology"});
        serde_json::to_writer(&mut writer, &header)?;
        writeln!(writer)?;
        let mut retained = 0_u64;
        for entry in buffer.rx.try_iter() {
            serde_json::to_writer(&mut writer, &entry)?;
            writeln!(writer)?;
            retained += 1;
        }
        let dropped = buffer.dropped.load(Ordering::Relaxed);
        let clock_errors = buffer.clock_errors.load(Ordering::Relaxed);
        let footer = serde_json::json!({"kind":"footer", "recording_ok":recording_ok,
            "retained_events":retained, "dropped_events":dropped, "clock_errors":clock_errors,
            "complete":dropped == 0 && clock_errors == 0});
        serde_json::to_writer(&mut writer, &footer)?;
        writeln!(writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(seq: u64) -> Event {
        Event::OutputSubmitted {
            run_frame: seq,
            source_seq: seq,
        }
    }

    fn raw_trace(capacity: usize) -> CadenceTrace {
        let mut trace = CadenceTrace::bounded(capacity).unwrap();
        Arc::get_mut(trace.0.as_mut().unwrap())
            .unwrap()
            .raw_watermark = Some(RawWatermarkTraceConfig {
            source_width: 1920,
            source_height: 1080,
        });
        trace
    }

    #[test]
    fn raw_watermark_retains_replaced_identity_without_changing_cfr() {
        use crate::capture::{CapturedFrame, CfrObservation, CfrSlotSelector};
        let trace = raw_trace(20);
        let clock = QpcClock::new().unwrap();
        let mut selector = CfrSlotSelector::default();
        let slot = Slot {
            run_frame: 0,
            start_qpc: 0,
            end_qpc: 100,
            seal_qpc: 104,
        };
        for (seq, low16) in [(1, 65535_u16), (2, 0)] {
            let frame = CapturedFrame {
                slot: seq as usize,
                source_seq: seq,
                source_qpc: seq as i64,
                callback_start_qpc: 1,
                callback_ready_qpc: 2,
                queue_depth: 1,
                present_seq: None,
            };
            trace.observe_raw_watermark(clock, (&frame).into(), || Ok(low16));
            let CfrObservation::Candidate { replaced } = selector.observe(frame, 100) else {
                panic!()
            };
            trace.emit(clock, || Event::Observe {
                slot,
                source_seq: seq,
                source_qpc: seq as i64,
                present_seq: None,
                future: false,
                replaced_source_seq: replaced.map(|f| f.source_seq),
            });
        }
        let selected = selector.seal(None).unwrap();
        assert_eq!(selected.identity.source_seq, 2);
        assert_eq!(selected.identity.present_seq, None);
        assert_eq!(selected.source_superseded, 1);
        let entries: Vec<_> = trace.0.as_ref().unwrap().rx.try_iter().collect();
        assert!(matches!(
            entries[0].event,
            Event::RawWatermark {
                source_seq: 1,
                raw_present_low16: Some(65535),
                ..
            }
        ));
        assert!(matches!(
            entries[2].event,
            Event::RawWatermark {
                source_seq: 2,
                raw_present_low16: Some(0),
                ..
            }
        ));
        assert!(matches!(
            entries[3].event,
            Event::Observe {
                replaced_source_seq: Some(1),
                present_seq: None,
                ..
            }
        ));
    }

    #[test]
    fn raw_watermark_unknown_future_retry_and_disabled_are_noninvasive() {
        let trace = raw_trace(8);
        let clock = QpcClock::new().unwrap();
        let identity = SelectedSourceIdentity {
            source_seq: 1,
            source_qpc: 10,
            present_seq: None,
        };
        trace.observe_raw_watermark(clock, identity, || {
            Err(present_identity::watermark::DecodeError::IntegrityMismatch.into())
        });
        trace.observe_raw_watermark(clock, identity, || panic!("future retry mapped twice"));
        CadenceTrace::default()
            .observe_raw_watermark(clock, identity, || panic!("disabled readback"));
        CadenceTrace::bounded(2)
            .unwrap()
            .observe_raw_watermark(clock, identity, || panic!("ordinary trace readback"));
        let entries: Vec<_> = trace.0.as_ref().unwrap().rx.try_iter().collect();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            entries[0].event,
            Event::RawWatermark {
                raw_present_low16: None,
                status: "integrity_mismatch",
                ..
            }
        ));
        assert_eq!(identity.present_seq, None);
    }

    #[test]
    fn disabled_trace_does_not_evaluate_diagnostics() {
        CadenceTrace::default().emit(QpcClock::new().unwrap(), || {
            panic!("disabled trace evaluated")
        });
        CadenceTrace::default().at(Some(1), || panic!("disabled trace evaluated fields"));
    }

    #[test]
    fn bounded_overflow_and_failed_run_are_explicit_without_hot_path_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let sink = TraceFile::create(&path, 2, serde_json::json!({})).unwrap();
        let trace = sink.trace();
        for seq in 0..5 {
            trace.at(Some(seq as i64), || event(seq));
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert!(TraceFile::create(&path, 2, serde_json::json!({})).is_err());
        sink.finish(false).unwrap();
        let data = std::fs::read_to_string(path).unwrap();
        let rows: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3]["recording_ok"], false);
        assert_eq!(rows[3]["dropped_events"], 3);
        assert_eq!(rows[3]["complete"], false);
    }

    #[test]
    fn concurrent_producers_retain_every_identity_with_bounded_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let sink = TraceFile::create(&path, 2048, serde_json::json!({})).unwrap();
        std::thread::scope(|scope| {
            for offset in [0, 1000] {
                let trace = sink.trace();
                scope.spawn(move || {
                    for seq in offset..offset + 1000 {
                        trace.at(Some(seq as i64), || event(seq));
                    }
                });
            }
        });
        sink.finish(true).unwrap();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut seqs: Vec<u64> = rows[1..2001]
            .iter()
            .map(|row| row["source_seq"].as_u64().unwrap())
            .collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (0..2000).collect::<Vec<_>>());
        assert_eq!(rows[2001]["complete"], true);
    }

    #[test]
    fn rejects_unbounded_or_zero_capacity_before_creating_output() {
        assert!(CadenceTrace::bounded(0).is_err());
        assert!(CadenceTrace::bounded(65_537).is_err());
    }

    #[test]
    fn finish_keeps_the_tail_of_a_retiring_callback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let sink = TraceFile::create(&path, 4, serde_json::json!({})).unwrap();
        let trace = sink.trace();
        let producer = trace.producer().unwrap();
        let callback = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            trace.at(Some(42), || event(42));
            drop(producer);
        });
        sink.finish(true).unwrap();
        callback.join().unwrap();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[1]["source_seq"], 42);
        assert_eq!(rows[2]["retained_events"], 1);
        assert_eq!(rows[2]["complete"], true);
    }
}
