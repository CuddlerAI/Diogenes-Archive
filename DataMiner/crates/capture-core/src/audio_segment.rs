#![cfg_attr(
    not(feature = "process-loopback-audio"),
    allow(dead_code, unused_imports)
)]

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, bounded, select};
use sha2::{Digest, Sha256};

#[cfg(feature = "process-loopback-audio")]
use crate::audio::{AudioPacket, ProcessLoopbackCapture};
use crate::audio_contract::{
    AUDIO_ARTIFACT_NAME, AUDIO_BITS_PER_SAMPLE, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ,
    AudioPcmFormat, AudioSampleSpan, AudioSource, SegmentAudioHealth, SegmentAudioMetadata,
    SegmentAudioState,
};
use crate::qpc::QpcClock;

const AUDIO_COMMAND_QUEUE_CAPACITY: usize = 4;
const SEGMENT_COVERAGE_WAIT: Duration = Duration::from_millis(750);
const AUDIO_WRITER_POLL: Duration = Duration::from_millis(20);
const ZSTD_LEVEL: i32 = 3;

pub type AudioAck<T> = Result<T, String>;

#[derive(Clone, Debug)]
pub struct AudioSegmentSeal {
    pub segment_index: u32,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub path: Option<PathBuf>,
    pub metadata: SegmentAudioMetadata,
}

enum AudioWriterCommand {
    Seal {
        segment_index: u32,
        qpc_start: i64,
        qpc_end: i64,
        response: Sender<AudioAck<AudioSegmentSeal>>,
    },
    Abort,
}

pub struct AudioSegmentWriter {
    commands: Option<Sender<AudioWriterCommand>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    qpc_frequency: i64,
    fallback_error: Option<String>,
}

impl AudioSegmentWriter {
    #[cfg(feature = "process-loopback-audio")]
    #[must_use]
    pub fn start(target_process_id: u32, clock: QpcClock, spool_dir: PathBuf) -> Self {
        let capture = match ProcessLoopbackCapture::start(target_process_id, clock) {
            Ok(capture) => capture,
            Err(error) => {
                tracing::warn!(
                    target_process_id,
                    error = %error,
                    "process loopback audio initialization failed"
                );
                return Self::fallback(clock.frequency(), "AUDIO_INIT_FAILED".into());
            }
        };
        let (commands, command_rx) = bounded(AUDIO_COMMAND_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_worker = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("dm-audio-segment-writer".into())
            .spawn(move || {
                run_audio_writer(capture, clock, spool_dir, command_rx, stop_for_worker)
            });
        match join {
            Ok(join) => Self {
                commands: Some(commands),
                stop,
                join: Some(join),
                qpc_frequency: clock.frequency(),
                fallback_error: None,
            },
            Err(error) => {
                tracing::warn!(error = %error, "audio segment writer thread failed to start");
                Self::fallback(clock.frequency(), "AUDIO_WRITER_START_FAILED".into())
            }
        }
    }

    #[cfg(not(feature = "process-loopback-audio"))]
    #[must_use]
    pub fn start(_target_process_id: u32, clock: QpcClock, _spool_dir: PathBuf) -> Self {
        Self::fallback(clock.frequency(), "AUDIO_FEATURE_UNAVAILABLE".into())
    }

    fn fallback(qpc_frequency: i64, reason: String) -> Self {
        Self {
            commands: None,
            stop: Arc::new(AtomicBool::new(false)),
            join: None,
            qpc_frequency,
            fallback_error: Some(reason),
        }
    }

    pub fn request_seal(
        &self,
        segment_index: u32,
        qpc_start: i64,
        qpc_end: i64,
    ) -> Receiver<AudioAck<AudioSegmentSeal>> {
        let (response, receiver) = bounded(1);
        let command = AudioWriterCommand::Seal {
            segment_index,
            qpc_start,
            qpc_end,
            response: response.clone(),
        };
        let send_error = match &self.commands {
            Some(commands) => commands
                .try_send(command)
                .err()
                .map(|_| "AUDIO_COMMAND_QUEUE_FAILED".into()),
            None => self.fallback_error.clone(),
        };
        if let Some(reason) = send_error {
            let _ = response.send(Ok(absent_seal(
                segment_index,
                qpc_start,
                qpc_end,
                self.qpc_frequency,
                reason,
                SegmentAudioHealth::default(),
            )));
        }
        receiver
    }

    pub fn abort(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.try_send(AudioWriterCommand::Abort);
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        self.commands.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AudioSegmentWriter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.commands.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(feature = "process-loopback-audio")]
fn run_audio_writer(
    capture: ProcessLoopbackCapture,
    clock: QpcClock,
    spool_dir: PathBuf,
    commands: Receiver<AudioWriterCommand>,
    stop: Arc<AtomicBool>,
) {
    let packets = capture.packet_receiver();
    let mut buffered = VecDeque::new();
    let mut capture_disconnected = false;
    let mut overflow_baseline = 0_u64;
    while !stop.load(Ordering::Acquire) {
        if capture_disconnected {
            match commands.recv_timeout(AUDIO_WRITER_POLL) {
                Ok(command) => handle_command(
                    command,
                    &capture,
                    clock,
                    &spool_dir,
                    &packets,
                    &mut buffered,
                    &mut capture_disconnected,
                    &mut overflow_baseline,
                ),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            select! {
                recv(commands) -> command => match command {
                    Ok(command) => handle_command(
                        command,
                        &capture,
                        clock,
                        &spool_dir,
                        &packets,
                        &mut buffered,
                        &mut capture_disconnected,
                        &mut overflow_baseline,
                    ),
                    Err(_) => break,
                },
                recv(packets) -> packet => match packet {
                    Ok(packet) => buffered.push_back(packet),
                    Err(_) => capture_disconnected = true,
                },
                default(AUDIO_WRITER_POLL) => {}
            }
        }
    }
    cleanup_active_audio_files(&spool_dir);
}

#[cfg(feature = "process-loopback-audio")]
#[allow(clippy::too_many_arguments)]
fn handle_command(
    command: AudioWriterCommand,
    capture: &ProcessLoopbackCapture,
    clock: QpcClock,
    spool_dir: &Path,
    packets: &Receiver<AudioPacket>,
    buffered: &mut VecDeque<AudioPacket>,
    capture_disconnected: &mut bool,
    overflow_baseline: &mut u64,
) {
    match command {
        AudioWriterCommand::Seal {
            segment_index,
            qpc_start,
            qpc_end,
            response,
        } => {
            collect_through_boundary(packets, buffered, clock, qpc_end, capture_disconnected);
            let current_overflows = capture.queue_overflows();
            let queue_overflows = current_overflows.saturating_sub(*overflow_baseline);
            *overflow_baseline = current_overflows;
            let seal = seal_segment(
                segment_index,
                qpc_start,
                qpc_end,
                clock,
                spool_dir,
                buffered,
                queue_overflows,
                *capture_disconnected,
            );
            discard_before_boundary(buffered, clock, qpc_end);
            let _ = response.send(seal.map_err(|error| format!("{error:#}")));
        }
        AudioWriterCommand::Abort => cleanup_active_audio_files(spool_dir),
    }
}

#[cfg(feature = "process-loopback-audio")]
fn collect_through_boundary(
    packets: &Receiver<AudioPacket>,
    buffered: &mut VecDeque<AudioPacket>,
    clock: QpcClock,
    qpc_end: i64,
    capture_disconnected: &mut bool,
) {
    buffered.extend(packets.try_iter());
    let deadline = Instant::now() + SEGMENT_COVERAGE_WAIT;
    while !*capture_disconnected && !covers_boundary(buffered, clock, qpc_end) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match packets.recv_timeout(remaining.min(AUDIO_WRITER_POLL)) {
            Ok(packet) => buffered.push_back(packet),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => *capture_disconnected = true,
        }
    }
}

#[cfg(feature = "process-loopback-audio")]
fn covers_boundary(buffered: &VecDeque<AudioPacket>, clock: QpcClock, qpc_end: i64) -> bool {
    buffered.back().is_some_and(|packet| {
        packet_end_qpc(packet, clock).is_ok_and(|packet_end| packet_end >= qpc_end)
    })
}

#[cfg(feature = "process-loopback-audio")]
fn discard_before_boundary(buffered: &mut VecDeque<AudioPacket>, clock: QpcClock, qpc_end: i64) {
    while buffered.front().is_some_and(|packet| {
        packet_end_qpc(packet, clock).is_ok_and(|packet_end| packet_end <= qpc_end)
    }) {
        buffered.pop_front();
    }
}

#[cfg(feature = "process-loopback-audio")]
fn packet_end_qpc(packet: &AudioPacket, clock: QpcClock) -> anyhow::Result<i64> {
    let first = packet.first_frame_qpc(clock)?;
    qpc_after_frames(first, u64::from(packet.frames), clock.frequency())
}

#[cfg(feature = "process-loopback-audio")]
#[allow(clippy::too_many_arguments)]
fn seal_segment(
    segment_index: u32,
    qpc_start: i64,
    qpc_end: i64,
    clock: QpcClock,
    spool_dir: &Path,
    packets: &VecDeque<AudioPacket>,
    queue_overflows: u64,
    capture_disconnected: bool,
) -> anyhow::Result<AudioSegmentSeal> {
    let timed = packets
        .iter()
        .map(|packet| {
            Ok(TimedAudioPacket {
                stream_frame_start: packet.stream_frame_start,
                first_qpc: packet.first_frame_qpc(clock)?,
                frames: packet.frames,
                pcm: packet.pcm.clone(),
                discontinuity: packet.discontinuity(),
                timestamp_error: packet.timestamp_error(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let built = build_segment(
        &timed,
        qpc_start,
        qpc_end,
        clock.frequency(),
        queue_overflows,
        capture_disconnected,
    )?;
    materialize_segment(
        segment_index,
        qpc_start,
        qpc_end,
        clock.frequency(),
        spool_dir,
        built,
    )
}

#[derive(Clone, Debug)]
struct TimedAudioPacket {
    stream_frame_start: u64,
    first_qpc: i64,
    frames: u32,
    pcm: Vec<u8>,
    discontinuity: bool,
    timestamp_error: bool,
}

struct BuiltSegment {
    state: SegmentAudioState,
    failure_code: Option<String>,
    health: SegmentAudioHealth,
    frames: u64,
    pcm: Option<Vec<u8>>,
}

fn build_segment(
    packets: &[TimedAudioPacket],
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    queue_overflows: u64,
    capture_disconnected: bool,
) -> anyhow::Result<BuiltSegment> {
    ensure!(qpc_frequency > 0, "audio QPC frequency must be positive");
    ensure!(qpc_end > qpc_start, "audio segment QPC range is empty");
    let frames = frames_for_qpc_span(qpc_end - qpc_start, qpc_frequency)?;
    ensure!(frames > 0, "audio segment contains zero PCM frames");
    let bytes_per_frame = u64::from(AUDIO_CHANNELS) * u64::from(AUDIO_BITS_PER_SAMPLE) / 8;
    let byte_count = frames
        .checked_mul(bytes_per_frame)
        .context("audio segment PCM byte count overflow")?;
    let mut pcm = vec![0_u8; usize::try_from(byte_count)?];
    let mut covered = vec![false; usize::try_from(frames)?];
    let mut timestamp_errors = 0_u64;
    let mut saw_discontinuity = false;
    let mut max_qpc_drift_us = 0_u64;
    let baseline = packets
        .first()
        .map(|packet| (packet.stream_frame_start, packet.first_qpc));

    for packet in packets {
        let expected_bytes = usize::try_from(u64::from(packet.frames) * bytes_per_frame)?;
        ensure!(
            packet.pcm.len() == expected_bytes,
            "audio packet PCM byte count does not match its frame count"
        );
        timestamp_errors += u64::from(packet.timestamp_error);
        saw_discontinuity |= packet.discontinuity;
        if let Some((baseline_frame, baseline_qpc)) = baseline
            && packet.stream_frame_start >= baseline_frame
        {
            let expected_qpc = qpc_after_frames(
                baseline_qpc,
                packet.stream_frame_start - baseline_frame,
                qpc_frequency,
            )?;
            let drift_ticks = packet.first_qpc.abs_diff(expected_qpc);
            let drift_us = drift_ticks.saturating_mul(1_000_000) / qpc_frequency as u64;
            max_qpc_drift_us = max_qpc_drift_us.max(drift_us);
        }

        let destination_start = rounded_ratio(
            (i128::from(packet.first_qpc) - i128::from(qpc_start))
                * i128::from(AUDIO_SAMPLE_RATE_HZ),
            i128::from(qpc_frequency),
        );
        for source_frame in 0..u64::from(packet.frames) {
            let destination = destination_start + i128::from(source_frame);
            if destination < 0 || destination >= i128::from(frames) {
                continue;
            }
            let destination = usize::try_from(destination)?;
            if covered[destination] {
                continue;
            }
            let source_offset = usize::try_from(source_frame * bytes_per_frame)?;
            let destination_offset = destination * usize::try_from(bytes_per_frame)?;
            let width = usize::try_from(bytes_per_frame)?;
            pcm[destination_offset..destination_offset + width]
                .copy_from_slice(&packet.pcm[source_offset..source_offset + width]);
            covered[destination] = true;
        }
    }

    let gaps = uncovered_spans(&covered);
    let gap_frames = gaps.iter().try_fold(0_u64, |sum, span| {
        sum.checked_add(span.frames)
            .context("audio uncovered frame count overflow")
    })?;
    let health = SegmentAudioHealth {
        discontinuities: gaps.clone(),
        silence_filled_spans: gaps,
        queue_overflows,
        timestamp_errors,
        max_qpc_drift_us,
    };

    let absent_code = if queue_overflows > 0 {
        Some("AUDIO_QUEUE_OVERFLOW")
    } else if timestamp_errors > 0 {
        Some("AUDIO_TIMESTAMP_ERROR")
    } else if capture_disconnected {
        Some("AUDIO_CAPTURE_STOPPED")
    } else if saw_discontinuity && gap_frames == 0 {
        Some("AUDIO_UNBOUNDED_DISCONTINUITY")
    } else if gap_frames > 0 && !gap_is_degraded_allowed(gap_frames, frames) {
        Some("AUDIO_GAP_EXCEEDED")
    } else {
        None
    };
    if let Some(code) = absent_code {
        return Ok(BuiltSegment {
            state: SegmentAudioState::Absent,
            failure_code: Some(code.into()),
            health,
            frames,
            pcm: None,
        });
    }
    if gap_frames > 0 {
        Ok(BuiltSegment {
            state: SegmentAudioState::Degraded,
            failure_code: Some("AUDIO_BOUNDED_GAP_FILLED".into()),
            health,
            frames,
            pcm: Some(pcm),
        })
    } else {
        Ok(BuiltSegment {
            state: SegmentAudioState::Ready,
            failure_code: None,
            health,
            frames,
            pcm: Some(pcm),
        })
    }
}

fn materialize_segment(
    segment_index: u32,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    spool_dir: &Path,
    built: BuiltSegment,
) -> anyhow::Result<AudioSegmentSeal> {
    if let Some(pcm) = built.pcm {
        let result = write_audio_spool(spool_dir, segment_index, &pcm);
        return match result {
            Ok((path, bytes, sha256)) => {
                let metadata = SegmentAudioMetadata {
                    state: built.state,
                    source: AudioSource::ProcessLoopback,
                    path: Some(AUDIO_ARTIFACT_NAME.into()),
                    sha256: Some(sha256),
                    bytes: Some(bytes),
                    format: AudioPcmFormat::default(),
                    qpc_frequency,
                    first_sample_qpc: Some(qpc_start),
                    frames: Some(built.frames),
                    failure_code: built.failure_code,
                    health: built.health,
                };
                if let Err(error) = metadata.validate() {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                Ok(AudioSegmentSeal {
                    segment_index,
                    qpc_start,
                    qpc_end,
                    path: Some(path),
                    metadata,
                })
            }
            Err(error) => {
                tracing::warn!(
                    segment_index,
                    error = %error,
                    "audio sidecar spool write failed"
                );
                Ok(absent_seal(
                    segment_index,
                    qpc_start,
                    qpc_end,
                    qpc_frequency,
                    "AUDIO_WRITE_FAILED".into(),
                    built.health,
                ))
            }
        };
    }
    Ok(absent_seal(
        segment_index,
        qpc_start,
        qpc_end,
        qpc_frequency,
        built
            .failure_code
            .unwrap_or_else(|| "AUDIO_UNAVAILABLE".into()),
        built.health,
    ))
}

fn absent_seal(
    segment_index: u32,
    qpc_start: i64,
    qpc_end: i64,
    qpc_frequency: i64,
    failure_code: String,
    health: SegmentAudioHealth,
) -> AudioSegmentSeal {
    AudioSegmentSeal {
        segment_index,
        qpc_start,
        qpc_end,
        path: None,
        metadata: SegmentAudioMetadata {
            state: SegmentAudioState::Absent,
            source: AudioSource::ProcessLoopback,
            path: None,
            sha256: None,
            bytes: None,
            format: AudioPcmFormat::default(),
            qpc_frequency,
            first_sample_qpc: None,
            frames: None,
            failure_code: Some(failure_code),
            health,
        },
    }
}

fn write_audio_spool(
    spool_dir: &Path,
    segment_index: u32,
    pcm: &[u8],
) -> anyhow::Result<(PathBuf, u64, String)> {
    fs::create_dir_all(spool_dir)
        .with_context(|| format!("failed to create audio spool {}", spool_dir.display()))?;
    let active = spool_dir.join(format!("{segment_index:05}.audio.pcm.zst.active"));
    let sealed = spool_dir.join(format!("{segment_index:05}.audio.pcm.zst.sealed"));
    ensure!(
        !active.exists() && !sealed.exists(),
        "audio spool already exists"
    );
    let file = File::create(&active)
        .with_context(|| format!("failed to create audio spool {}", active.display()))?;
    let writer = BufWriter::new(file);
    let mut encoder = zstd::stream::write::Encoder::new(writer, ZSTD_LEVEL)?;
    encoder.write_all(pcm)?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    writer.get_ref().sync_data()?;
    drop(writer);
    fs::rename(&active, &sealed).with_context(|| {
        format!(
            "failed to seal audio spool {} -> {}",
            active.display(),
            sealed.display()
        )
    })?;
    let bytes = fs::metadata(&sealed)?.len();
    let sha256 = sha256_file(&sealed)?;
    Ok((sealed, bytes, sha256))
}

fn cleanup_active_audio_files(spool_dir: &Path) {
    let Ok(entries) = fs::read_dir(spool_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".audio.pcm.zst.active"))
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn uncovered_spans(covered: &[bool]) -> Vec<AudioSampleSpan> {
    let mut spans = Vec::new();
    let mut index = 0_usize;
    while index < covered.len() {
        if covered[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < covered.len() && !covered[index] {
            index += 1;
        }
        spans.push(AudioSampleSpan {
            start_frame: start as u64,
            frames: (index - start) as u64,
        });
    }
    spans
}

fn gap_is_degraded_allowed(gap_frames: u64, total_frames: u64) -> bool {
    let max_by_time = u64::from(AUDIO_SAMPLE_RATE_HZ) / 10;
    let max_by_percent = total_frames / 100;
    gap_frames <= max_by_time && gap_frames <= max_by_percent
}

fn frames_for_qpc_span(qpc_span: i64, qpc_frequency: i64) -> anyhow::Result<u64> {
    ensure!(qpc_span > 0, "audio QPC span must be positive");
    u64::try_from(rounded_ratio(
        i128::from(qpc_span) * i128::from(AUDIO_SAMPLE_RATE_HZ),
        i128::from(qpc_frequency),
    ))
    .context("audio QPC span exceeds u64 frames")
}

fn qpc_after_frames(first_qpc: i64, frames: u64, qpc_frequency: i64) -> anyhow::Result<i64> {
    ensure!(qpc_frequency > 0, "audio QPC frequency must be positive");
    let delta = rounded_ratio(
        i128::from(frames) * i128::from(qpc_frequency),
        i128::from(AUDIO_SAMPLE_RATE_HZ),
    );
    let value = i128::from(first_qpc)
        .checked_add(delta)
        .context("audio packet QPC range overflow")?;
    i64::try_from(value).context("audio packet QPC range exceeds i64")
}

fn rounded_ratio(numerator: i128, denominator: i128) -> i128 {
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        -((-numerator + denominator / 2) / denominator)
    }
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(start_frame: u64, first_qpc: i64, frames: u32) -> TimedAudioPacket {
        TimedAudioPacket {
            stream_frame_start: start_frame,
            first_qpc,
            frames,
            pcm: vec![7; frames as usize * 4],
            discontinuity: false,
            timestamp_error: false,
        }
    }

    #[test]
    fn qpc_ratio_rounding_is_symmetric_around_zero() {
        assert_eq!(rounded_ratio(4, 10), 0);
        assert_eq!(rounded_ratio(5, 10), 1);
        assert_eq!(rounded_ratio(-4, 10), 0);
        assert_eq!(rounded_ratio(-5, 10), -1);
        assert_eq!(rounded_ratio(-14, 10), -1);
        assert_eq!(rounded_ratio(-15, 10), -2);
    }

    #[test]
    fn exact_half_open_packets_produce_ready_pcm() {
        let packets = vec![packet(0, 1_000_000, 48_000)];
        let built = build_segment(&packets, 1_000_000, 11_000_000, 10_000_000, 0, false).unwrap();
        assert_eq!(built.state, SegmentAudioState::Ready);
        assert_eq!(built.frames, 48_000);
        assert_eq!(built.pcm.unwrap().len(), 192_000);
        assert!(built.health.is_pristine());
    }

    #[test]
    fn packet_crossing_boundary_is_sliced_without_overlap() {
        let packets = vec![packet(0, 900_000, 48_960)];
        let first = build_segment(&packets, 1_000_000, 6_000_000, 10_000_000, 0, false).unwrap();
        let second = build_segment(&packets, 6_000_000, 11_000_000, 10_000_000, 0, false).unwrap();
        assert_eq!(first.state, SegmentAudioState::Ready);
        assert_eq!(second.state, SegmentAudioState::Ready);
        assert_eq!(first.frames + second.frames, 48_000);
    }

    #[test]
    fn small_precise_gap_is_degraded_but_large_gap_is_absent() {
        let mut packets = vec![packet(0, 1_000_000, 480_000)];
        packets[0].pcm.truncate((480_000 - 2_400) * 4);
        packets[0].frames = 477_600;
        let degraded =
            build_segment(&packets, 1_000_000, 101_000_000, 10_000_000, 0, false).unwrap();
        assert_eq!(degraded.state, SegmentAudioState::Degraded);
        assert_eq!(degraded.health.silence_filled_spans[0].frames, 2_400);

        packets[0].pcm.truncate((480_000 - 4_801) * 4);
        packets[0].frames = 475_199;
        let absent = build_segment(&packets, 1_000_000, 101_000_000, 10_000_000, 0, false).unwrap();
        assert_eq!(absent.state, SegmentAudioState::Absent);
        assert!(absent.pcm.is_none());
    }

    #[test]
    fn overflow_or_timestamp_error_never_publishes_audio() {
        let packets = vec![packet(0, 1_000_000, 48_000)];
        let overflow =
            build_segment(&packets, 1_000_000, 11_000_000, 10_000_000, 1, false).unwrap();
        assert_eq!(overflow.state, SegmentAudioState::Absent);

        let mut bad_timestamp = packets;
        bad_timestamp[0].timestamp_error = true;
        let timestamp =
            build_segment(&bad_timestamp, 1_000_000, 11_000_000, 10_000_000, 0, false).unwrap();
        assert_eq!(timestamp.state, SegmentAudioState::Absent);
    }

    #[test]
    fn spool_is_zstd_pcm_and_uses_atomic_sealed_name() {
        let dir = tempfile::tempdir().unwrap();
        let pcm = [1_u8, 2, 3, 4].repeat(128);
        let (path, bytes, hash) = write_audio_spool(dir.path(), 7, &pcm).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "00007.audio.pcm.zst.sealed"
        );
        assert!(bytes > 0);
        assert_eq!(hash.len(), 71);
        assert!(hash.starts_with("sha256:"));
        let decoded = zstd::stream::decode_all(File::open(path).unwrap()).unwrap();
        assert_eq!(decoded, pcm);
    }
}
