//! Diagnostic record of what the production pair sampler observed.
//! `cp2077_player_probe::pair_sampler` reads the (actor-root, FPP camera) pair about once per
//! millisecond for the content join; this sidecar keeps every distinct pair content with its
//! observed QPC span so the join can be re-run offline for coverage, torn pairs and write
//! order. The sampling thread only collapses identical contents here, after it has published
//! to the join's ring, and hands finished entries to the diagnostic worker without waiting,
//! formatting or touching a file: a sampler that stalls loses exactly the short-lived states
//! it exists to catch. Never read by admission, coverage or delivery.
use super::{ACTIVE, Event, note_loss, write_line};
use depth_lab::cp2077_pose_join::SimulationPair;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::Path,
    sync::{Mutex, atomic::Ordering, mpsc::SyncSender},
};

pub(super) const FILE: &str = "cp2077-dense-pairs.jsonl";
const MAX_REASONS: usize = 16;
// Upper edges in microseconds for the gap between consecutive successful pair reads.
const GAP_EDGES_US: [i64; 6] = [1_500, 2_500, 5_000, 10_000, 20_000, 50_000];

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

fn hex(words: &[u32; 8]) -> String {
    words.iter().map(|word| format!("{word:08x}")).collect()
}

/// Rows are serialized by the diagnostic worker, so this string is built off the sampler thread.
fn serialize_hex<S: serde::Serializer>(words: &[u32; 8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&hex(words))
}

#[derive(Debug, Serialize)]
pub(super) struct Row {
    schema: &'static str,
    generation: u64,
    entry: u64,
    first_begin_qpc: i64,
    first_end_qpc: i64,
    last_begin_qpc: i64,
    last_end_qpc: i64,
    reads: u64,
    player_position: [f64; 3],
    player_rotation: [f64; 4],
    #[serde(serialize_with = "serialize_hex")]
    player_words: [u32; 8],
    camera_position: [f64; 3],
    camera_rotation: [f64; 4],
    #[serde(serialize_with = "serialize_hex")]
    camera_words: [u32; 8],
}

#[derive(Default)]
struct Counts {
    pair_reads: u64,
    failed_reads: u64,
    entries: u64,
    unavailable: BTreeMap<String, u64>,
    unavailable_other: u64,
    gaps: [u64; GAP_EDGES_US.len() + 1],
    max_gap_us: i64,
}

impl Counts {
    fn gap(&mut self, microseconds: i64) {
        let bucket = GAP_EDGES_US
            .iter()
            .position(|edge| microseconds <= *edge)
            .unwrap_or(GAP_EDGES_US.len());
        self.gaps[bucket] += 1;
        self.max_gap_us = self.max_gap_us.max(microseconds);
    }

    fn reason(&mut self, reason: &str) {
        if self.unavailable.len() < MAX_REASONS || self.unavailable.contains_key(reason) {
            *self.unavailable.entry(reason.to_owned()).or_default() += 1;
        } else {
            self.unavailable_other += 1;
        }
    }
}

struct Sink {
    run: u64,
    generation: u64,
    frequency: i64,
    sender: SyncSender<Event>,
    counts: Counts,
    open: Option<Row>,
    last_begin: Option<i64>,
}

impl Sink {
    fn close(&mut self) {
        if let Some(row) = self.open.take()
            && self.sender.try_send(Event::DensePair(row)).is_err()
        {
            note_loss();
        }
    }

    fn observe(&mut self, pair: &SimulationPair) {
        self.counts.pair_reads += 1;
        // `last_begin` survives failed reads, so the histogram maximum is a true bound on
        // sampling holes.
        if let Some(last) = self.last_begin.replace(pair.first_qpc) {
            let ticks = i128::from(pair.first_qpc - last) * 1_000_000;
            self.counts.gap((ticks / i128::from(self.frequency)) as i64);
        }
        if let Some(row) = self.open.as_mut().filter(|row| {
            row.player_words == pair.player_words && row.camera_words == pair.camera_words
        }) {
            row.last_begin_qpc = pair.first_qpc;
            row.last_end_qpc = pair.last_qpc;
            row.reads += 1;
            return;
        }
        self.close();
        self.counts.entries += 1;
        self.open = Some(Row {
            schema: "dm-cp2077-dense-pair/v2",
            generation: self.generation,
            entry: self.counts.entries,
            first_begin_qpc: pair.first_qpc,
            first_end_qpc: pair.last_qpc,
            last_begin_qpc: pair.first_qpc,
            last_end_qpc: pair.last_qpc,
            reads: 1,
            player_position: pair.player_position,
            player_rotation: pair.player_rotation,
            player_words: pair.player_words,
            camera_position: pair.camera_position,
            camera_rotation: pair.camera_rotation,
            camera_words: pair.camera_words,
        });
    }

    fn failed(&mut self, reason: &str) {
        self.counts.failed_reads += 1;
        self.counts.reason(reason);
        self.close();
    }

    fn summary(&self) -> serde_json::Value {
        serde_json::json!({"schema":"dm-cp2077-dense-pair-summary/v2",
            "generation":self.generation, "pair_reads":self.counts.pair_reads,
            "failed_reads":self.counts.failed_reads, "entries":self.counts.entries,
            "unavailable":self.counts.unavailable,
            "unavailable_other_reasons":self.counts.unavailable_other,
            "read_gap_upper_edges_us":GAP_EDGES_US, "read_gap_counts":self.counts.gaps,
            "max_read_gap_us":self.counts.max_gap_us,
            "read_gap_scope":"between_successive_successful_sampler_thread_reads_including_failed_stretches",
            "thread_priority_raised":crate::cp2077_player_probe::pair_sampler_priority_raised()})
    }
}

pub(super) fn create(
    root: &Path,
    identity: &depth_lab::StreamIdentity,
    frequency: i64,
    axis: depth_lab::D3d12AxisMapping,
) -> std::io::Result<BufWriter<File>> {
    let mut file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(FILE))?,
    );
    let header = serde_json::json!({"schema":"dm-cp2077-dense-pair-header/v2", "identity":identity,
        "qpc_frequency":frequency, "diagnostic_only":true, "non_trainable":true,
        "observer":"production_pair_sampler_thread; present_path_reads_are_not_recorded_here",
        "player_chain_id":crate::cp2077_player_probe::PROVIDER,
        "camera_chain_id":crate::cp2077_player_probe::fpp_camera::CHAIN_ID,
        "position_units":"metres", "world_axis_mapping":axis, "quaternion_order":"xyzw",
        "words":"native_WorldTransform_u32x8_hex",
        "pair_semantics":"camera_stable_read_bracketed_by_two_identical_actor_root_stable_reads; not_an_engine_transaction",
        "entry_semantics":"consecutive_identical_native_pair_contents_collapse_into_one_entry_with_its_observed_qpc_span",
        "render_reference":"camera-pose-index.jsonl", "engine_state_frame_known":false});
    write_line(&mut file, &header)?;
    file.flush()?;
    Ok(file)
}

pub(super) fn begin(run: u64, generation: u64, frequency: i64, sender: SyncSender<Event>) {
    if let Ok(mut sink) = SINK.lock() {
        *sink = Some(Sink {
            run,
            generation,
            frequency,
            sender,
            counts: Counts::default(),
            open: None,
            last_begin: None,
        });
    }
}

/// Closes the open entry and hands the summary to the worker. Must run before the worker's
/// channel is dropped so that both still reach the file. The sampler has been joined by now
/// and this is the control path, so both sends wait for queue room instead of dropping the
/// tail; they fail only when the worker is already gone.
pub(super) fn finish() {
    if let Ok(mut guard) = SINK.lock()
        && let Some(mut sink) = guard.take()
    {
        let tail = sink.open.take().map(Event::DensePair);
        let summary = Event::DenseSummary(sink.summary());
        for event in tail.into_iter().chain([summary]) {
            if sink.sender.send(event).is_err() {
                note_loss();
                crate::probe::log("CP2077 dense pair tail lost: diagnostic worker already exited");
                break;
            }
        }
    }
}

fn with_sink(record: impl FnOnce(&mut Sink)) {
    let run = ACTIVE.load(Ordering::Acquire);
    if run == 0 {
        return;
    }
    // Only the sampler thread and the two lifecycle calls ever take this lock.
    if let Ok(mut guard) = SINK.lock()
        && let Some(sink) = guard.as_mut().filter(|sink| sink.run == run)
    {
        record(sink);
    }
}

/// Production sampler thread: one successful pair read.
pub(crate) fn observe(pair: &SimulationPair) {
    with_sink(|sink| sink.observe(pair));
}

/// Production sampler thread: a failed read ends the span, so the next entry must not look
/// continuous.
pub(crate) fn interrupt(reason: &str) {
    with_sink(|sink| sink.failed(reason));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn pair(root: u32, camera: u32, qpc: i64) -> SimulationPair {
        SimulationPair {
            first_qpc: qpc,
            first_end_qpc: qpc + 20,
            last_qpc: qpc + 20,
            reads: 1,
            player_words: [root; 8],
            camera_words: [camera; 8],
            player_position: [f64::from(root), 0.0, 0.0],
            player_rotation: [0.0, 0.0, 0.0, 1.0],
            camera_position: [f64::from(camera), 1.7, 0.0],
            camera_rotation: [0.0, 0.0, 0.0, 1.0],
            follows_gap: false,
        }
    }

    fn sink(sender: SyncSender<Event>) -> Sink {
        Sink {
            run: 1,
            generation: 7,
            frequency: 1_000_000,
            sender,
            counts: Counts::default(),
            open: None,
            last_begin: None,
        }
    }

    #[test]
    fn identical_contents_collapse_and_a_failed_read_ends_the_entry() {
        let (sender, receiver) = sync_channel(8);
        let mut sink = sink(sender);
        sink.observe(&pair(1, 1, 1_000));
        sink.observe(&pair(1, 1, 2_000));
        // Root written first: same camera content, new actor root.
        sink.observe(&pair(2, 1, 3_000));
        sink.failed("audit busy");
        sink.observe(&pair(2, 1, 40_000));
        sink.close();
        let rows: Vec<_> = receiver
            .try_iter()
            .map(|event| match event {
                Event::DensePair(row) => row,
                _ => panic!("only dense rows were sent"),
            })
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (rows[0].first_begin_qpc, rows[0].last_end_qpc, rows[0].reads),
            (1_000, 2_020, 2)
        );
        assert_eq!(rows[1].player_words, [2; 8]);
        assert_eq!(rows[1].camera_words, rows[0].camera_words);
        let line = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(line["player_words"], hex(&[2; 8]));
        // The gap across the failed stretch is kept, so the maximum bounds real holes.
        assert_eq!(sink.counts.max_gap_us, 37_000);
        assert_eq!(sink.summary()["failed_reads"], 1);
        assert_eq!(sink.summary()["entries"], 3);
    }

    #[test]
    fn a_full_worker_queue_is_a_counted_loss_never_a_wait() {
        let (sender, _receiver) = sync_channel(1);
        let mut sink = sink(sender);
        for index in 0..4 {
            sink.observe(&pair(index, index, i64::from(index) * 1_000));
        }
        sink.close();
        assert_eq!(sink.counts.entries, 4);
    }

    #[test]
    fn gap_histogram_uses_inclusive_upper_edges_and_keeps_the_maximum() {
        let mut counts = Counts::default();
        for gap in [900, 1_500, 1_501, 49_999, 50_000, 50_001, 700_000] {
            counts.gap(gap);
        }
        assert_eq!(counts.gaps, [2, 1, 0, 0, 0, 2, 2]);
        assert_eq!(counts.max_gap_us, 700_000);
    }

    #[test]
    fn unavailable_reasons_are_bounded_without_losing_the_total() {
        let mut counts = Counts::default();
        for index in 0..MAX_REASONS + 3 {
            counts.reason(&format!("reason {index}"));
        }
        counts.reason("reason 0");
        assert_eq!(counts.unavailable.len(), MAX_REASONS);
        assert_eq!(counts.unavailable["reason 0"], 2);
        assert_eq!(counts.unavailable_other, 3);
    }

    #[test]
    fn words_render_as_fixed_width_native_hex() {
        assert_eq!(
            hex(&[1, 0xdead_beef, 0, 0, 0, 0, 0, 0x3f80_0000]),
            "00000001deadbeef00000000000000000000000000000000000000003f800000"
        );
    }
}
