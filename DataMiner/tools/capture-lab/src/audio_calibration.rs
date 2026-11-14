use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, ensure};
use capture_core::audio::{
    AudioCaptureHealth, AudioPacket, AudioPcmFormat, ProcessLoopbackCapture,
};
use capture_core::qpc::QpcClock;
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    CALLBACK_EVENT, HWAVEOUT, WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WAVEHDR, waveOutClose,
    waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader, waveOutWrite,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::core::PSTR;

const INITIAL_SILENCE_MS: u64 = 500;
const BURST_PERIOD_MS: u64 = 1_000;
const BURST_DURATION_MS: u64 = 50;
const TONE_HZ: f64 = 1_000.0;
const TONE_AMPLITUDE: f64 = 0.35;
const DETECTION_THRESHOLD: i16 = 5_000;
const RELEASE_HOLD_MS: u64 = 10;
const PLAYBACK_READY_TIMEOUT: Duration = Duration::from_secs(5);
const PLAYBACK_POLL_MS: u32 = 100;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ToneSchedule {
    pub submit_qpc: i64,
    pub first_burst_qpc: i64,
    pub period_qpc: i64,
    pub burst_duration_qpc: i64,
    pub burst_count: u32,
}

impl ToneSchedule {
    #[must_use]
    pub fn burst_index_at(self, qpc: i64) -> Option<u32> {
        if qpc < self.first_burst_qpc || self.period_qpc <= 0 {
            return None;
        }
        let elapsed = qpc - self.first_burst_qpc;
        let index = u32::try_from(elapsed / self.period_qpc).ok()?;
        if index >= self.burst_count || elapsed % self.period_qpc >= self.burst_duration_qpc {
            return None;
        }
        Some(index)
    }
}

pub struct TonePlayback {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TonePlayback {
    pub fn start(
        capture_duration: Duration,
        clock: QpcClock,
    ) -> anyhow::Result<(Self, ToneSchedule)> {
        ensure!(
            capture_duration >= Duration::from_secs(2),
            "audio calibration requires at least two seconds"
        );
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_worker = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("dm-audio-tone-lab".into())
            .spawn(move || {
                let result =
                    play_tone_sequence(capture_duration, clock, &stop_for_worker, &ready_tx);
                if let Err(error) = &result {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("{error:#}")));
                }
                result
            })
            .context("failed to spawn audio tone lab thread")?;
        match ready_rx.recv_timeout(PLAYBACK_READY_TIMEOUT) {
            Ok(Ok(schedule)) => Ok((
                Self {
                    stop,
                    worker: Some(worker),
                },
                schedule,
            )),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error).context("tone playback initialization failed")
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error).context("timed out waiting for tone playback")
            }
        }
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .expect("tone playback worker missing")
            .join()
            .map_err(|_| anyhow::anyhow!("tone playback worker panicked"))?
    }
}

impl Drop for TonePlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct VideoBurstMarker {
    pub burst_index: u32,
    pub output_frame: u64,
    pub output_frame_qpc: i64,
    pub source_qpc: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioBurstObservation {
    pub stream_frame: u64,
    pub packet_arrival_qpc: i64,
    pub packet_qpc_position_100ns: u64,
    pub sample_qpc: Option<i64>,
    pub timestamp_error: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CalibrationDistribution {
    pub count: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCalibrationState {
    Ready,
    Degraded,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioCalibrationReport {
    pub schema_version: &'static str,
    pub state: AudioCalibrationState,
    pub source: &'static str,
    pub format: AudioPcmFormat,
    pub system_src: bool,
    pub raw_probe_path: Option<&'static str>,
    pub expected_bursts: u32,
    pub render_flash_qpc_ticks: Vec<i64>,
    pub video_markers: Vec<VideoBurstMarker>,
    pub audio_bursts: Vec<AudioBurstObservation>,
    pub signed_offset_us: CalibrationDistribution,
    pub constant_offset_us: Option<f64>,
    pub residual_absolute_error_us: CalibrationDistribution,
    pub single_run_timing_passed: bool,
    pub hard_gate_passed: Option<bool>,
    pub health: AudioCaptureHealth,
    pub limitations: Vec<String>,
}

impl AudioCalibrationReport {
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            schema_version: "dm-audio-loopback-lab-1",
            state: AudioCalibrationState::Failed,
            source: "process_loopback",
            format: AudioPcmFormat::pcm_s16le_48k_stereo(),
            system_src: true,
            raw_probe_path: None,
            expected_bursts: 0,
            render_flash_qpc_ticks: Vec::new(),
            video_markers: Vec::new(),
            audio_bursts: Vec::new(),
            signed_offset_us: CalibrationDistribution::default(),
            constant_offset_us: None,
            residual_absolute_error_us: CalibrationDistribution::default(),
            single_run_timing_passed: false,
            hard_gate_passed: None,
            health: AudioCaptureHealth {
                terminal_error: Some(error.into()),
                ..AudioCaptureHealth::default()
            },
            limitations: default_limitations(),
        }
    }
}

pub struct ActiveAudioCalibration {
    capture: ProcessLoopbackCapture,
    playback: Option<TonePlayback>,
    schedule: Option<ToneSchedule>,
    packets: Vec<AudioPacket>,
}

impl ActiveAudioCalibration {
    pub fn prepare(clock: QpcClock) -> anyhow::Result<Self> {
        let capture = ProcessLoopbackCapture::start(std::process::id(), clock)?;
        Ok(Self {
            capture,
            playback: None,
            schedule: None,
            packets: Vec::new(),
        })
    }

    pub fn start_playback(
        &mut self,
        capture_duration: Duration,
        clock: QpcClock,
    ) -> anyhow::Result<ToneSchedule> {
        ensure!(self.playback.is_none(), "tone playback already started");
        let (playback, schedule) = TonePlayback::start(capture_duration, clock)?;
        self.playback = Some(playback);
        self.schedule = Some(schedule);
        Ok(schedule)
    }

    pub fn drain(&mut self) {
        self.capture.drain_packets(&mut self.packets);
    }

    pub fn finish(
        mut self,
        clock: QpcClock,
        video_markers: Vec<VideoBurstMarker>,
        render_flash_qpc_ticks: Vec<i64>,
        frame_duration_us: f64,
        run_dir: &Path,
    ) -> anyhow::Result<AudioCalibrationReport> {
        let playback_result = match self.playback.take() {
            Some(playback) => playback.finish(),
            None => Err(anyhow::anyhow!("tone playback did not start")),
        };
        let health = self.capture.finish(&mut self.packets)?;
        playback_result?;
        let schedule = self.schedule.context("tone playback schedule is missing")?;
        build_report(
            clock,
            schedule,
            CalibrationInputs {
                packets: self.packets,
                health,
                video_markers,
                render_flash_qpc_ticks,
                frame_duration_us,
            },
            run_dir,
        )
    }
}

fn play_tone_sequence(
    capture_duration: Duration,
    clock: QpcClock,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<anyhow::Result<ToneSchedule>>,
) -> anyhow::Result<()> {
    let format = AudioPcmFormat::pcm_s16le_48k_stereo();
    let playback_duration = capture_duration + Duration::from_secs(1);
    let mut pcm = generate_tone_pcm(playback_duration, format);
    let event = KernelEvent::auto_reset()?;
    let wave_format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: format.channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.average_bytes_per_second,
        nBlockAlign: format.block_align,
        wBitsPerSample: format.bits_per_sample,
        cbSize: 0,
    };
    let mut output = HWAVEOUT::default();
    mm_ok(
        unsafe {
            waveOutOpen(
                Some(&mut output),
                WAVE_MAPPER,
                &wave_format,
                Some(event.handle.0 as usize),
                None,
                CALLBACK_EVENT,
            )
        },
        "waveOutOpen",
    )?;
    unsafe { ResetEvent(event.handle) }.context("ResetEvent failed for tone playback")?;

    let mut header = WAVEHDR {
        lpData: PSTR(pcm.as_mut_ptr().cast::<u8>()),
        dwBufferLength: u32::try_from(
            pcm.len()
                .checked_mul(std::mem::size_of::<i16>())
                .context("tone PCM byte count overflowed")?,
        )?,
        ..Default::default()
    };
    let header_size = u32::try_from(std::mem::size_of::<WAVEHDR>())?;
    let mut prepared = false;
    let playback_result = (|| -> anyhow::Result<()> {
        mm_ok(
            unsafe { waveOutPrepareHeader(output, &mut header, header_size) },
            "waveOutPrepareHeader",
        )?;
        prepared = true;
        let submit_qpc = clock.now()?;
        mm_ok(
            unsafe { waveOutWrite(output, &mut header, header_size) },
            "waveOutWrite",
        )?;
        let schedule = tone_schedule(submit_qpc, capture_duration, clock.frequency());
        let _ = ready.send(Ok(schedule));

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let wait = unsafe { WaitForSingleObject(event.handle, PLAYBACK_POLL_MS) };
            if wait == WAIT_OBJECT_0 {
                break;
            }
            ensure!(wait == WAIT_TIMEOUT, "tone playback wait failed: {wait:?}");
        }
        Ok(())
    })();

    let _ = unsafe { waveOutReset(output) };
    if prepared {
        let _ = unsafe { waveOutUnprepareHeader(output, &mut header, header_size) };
    }
    let _ = unsafe { waveOutClose(output) };
    playback_result
}

fn tone_schedule(submit_qpc: i64, duration: Duration, frequency: i64) -> ToneSchedule {
    let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let burst_count = if duration_ms <= INITIAL_SILENCE_MS {
        0
    } else {
        u32::try_from((duration_ms - INITIAL_SILENCE_MS - 1) / BURST_PERIOD_MS + 1)
            .unwrap_or(u32::MAX)
    };
    ToneSchedule {
        submit_qpc,
        first_burst_qpc: submit_qpc + millis_to_qpc(INITIAL_SILENCE_MS, frequency),
        period_qpc: millis_to_qpc(BURST_PERIOD_MS, frequency),
        burst_duration_qpc: millis_to_qpc(BURST_DURATION_MS, frequency),
        burst_count,
    }
}

fn millis_to_qpc(milliseconds: u64, frequency: i64) -> i64 {
    ((i128::from(milliseconds) * i128::from(frequency) + 500) / 1_000) as i64
}

fn generate_tone_pcm(duration: Duration, format: AudioPcmFormat) -> Vec<i16> {
    let total_frames = (duration.as_secs_f64() * f64::from(format.sample_rate)).ceil() as usize;
    let initial_frames = (INITIAL_SILENCE_MS * u64::from(format.sample_rate) / 1_000) as usize;
    let period_frames = (BURST_PERIOD_MS * u64::from(format.sample_rate) / 1_000) as usize;
    let burst_frames = (BURST_DURATION_MS * u64::from(format.sample_rate) / 1_000) as usize;
    let mut pcm = Vec::with_capacity(total_frames * usize::from(format.channels));
    for frame in 0..total_frames {
        let sample =
            if frame >= initial_frames && (frame - initial_frames) % period_frames < burst_frames {
                let burst_frame = (frame - initial_frames) % period_frames;
                let phase = std::f64::consts::TAU * TONE_HZ * burst_frame as f64
                    / f64::from(format.sample_rate);
                (phase.sin() * TONE_AMPLITUDE * f64::from(i16::MAX)) as i16
            } else {
                0
            };
        for _ in 0..format.channels {
            pcm.push(sample);
        }
    }
    pcm
}

struct CalibrationInputs {
    packets: Vec<AudioPacket>,
    health: AudioCaptureHealth,
    video_markers: Vec<VideoBurstMarker>,
    render_flash_qpc_ticks: Vec<i64>,
    frame_duration_us: f64,
}

fn build_report(
    clock: QpcClock,
    schedule: ToneSchedule,
    inputs: CalibrationInputs,
    run_dir: &Path,
) -> anyhow::Result<AudioCalibrationReport> {
    let CalibrationInputs {
        packets,
        health,
        video_markers,
        render_flash_qpc_ticks,
        frame_duration_us,
    } = inputs;
    let audio_bursts = detect_audio_bursts(&packets, clock)?;
    let offsets = video_markers
        .iter()
        .zip(audio_bursts.iter())
        .filter_map(|(video, audio)| {
            audio
                .sample_qpc
                .map(|audio_qpc| clock.elapsed_micros(video.output_frame_qpc, audio_qpc))
        })
        .collect::<Vec<_>>();
    let constant_offset_us = median(&offsets);
    let residuals = constant_offset_us.map_or_else(Vec::new, |offset| {
        offsets.iter().map(|value| (value - offset).abs()).collect()
    });
    let residual_distribution = distribution(residuals);
    let complete = burst_evidence_complete(
        schedule.burst_count,
        render_flash_qpc_ticks.len(),
        &video_markers,
        audio_bursts.len(),
        offsets.len(),
    );
    let single_run_timing_passed = complete
        && residual_distribution.p95 <= frame_duration_us
        && residual_distribution.max <= frame_duration_us * 2.0;
    write_pcm_probe(run_dir, &packets)?;

    let degraded = health.queue_overflows != 0
        || health.discontinuities != 0
        || health.timestamp_errors != 0
        || health.terminal_error.is_some()
        || !complete;
    let mut limitations = default_limitations();
    if health.nonzero_device_positions == 0 {
        limitations.push(
            "DevicePosition was zero for every packet; cumulative requested-format samples remain the only sample clock."
                .into(),
        );
    }
    if !complete {
        limitations.push(format!(
            "Expected {} burst pair(s), observed {} completed render flash(es), {} video marker(s) and {} audio edge(s).",
            schedule.burst_count,
            render_flash_qpc_ticks.len(),
            video_markers.len(),
            audio_bursts.len()
        ));
    }
    Ok(AudioCalibrationReport {
        schema_version: "dm-audio-loopback-lab-1",
        state: if degraded {
            AudioCalibrationState::Degraded
        } else {
            AudioCalibrationState::Ready
        },
        source: "process_loopback",
        format: AudioPcmFormat::pcm_s16le_48k_stereo(),
        system_src: true,
        raw_probe_path: Some("audio-calibration.pcm.zst"),
        expected_bursts: schedule.burst_count,
        render_flash_qpc_ticks,
        video_markers,
        audio_bursts,
        signed_offset_us: distribution(offsets),
        constant_offset_us,
        residual_absolute_error_us: residual_distribution,
        single_run_timing_passed,
        hard_gate_passed: None,
        health,
        limitations,
    })
}

fn burst_evidence_complete(
    expected_bursts: u32,
    render_flash_count: usize,
    video_markers: &[VideoBurstMarker],
    audio_burst_count: usize,
    offset_count: usize,
) -> bool {
    let expected_bursts = expected_bursts as usize;
    expected_bursts > 0
        && render_flash_count == expected_bursts
        && video_markers.len() == expected_bursts
        && video_markers
            .iter()
            .enumerate()
            .all(|(index, marker)| marker.burst_index as usize == index)
        && audio_burst_count == expected_bursts
        && offset_count == expected_bursts
}

fn detect_audio_bursts(
    packets: &[AudioPacket],
    clock: QpcClock,
) -> anyhow::Result<Vec<AudioBurstObservation>> {
    let format = AudioPcmFormat::pcm_s16le_48k_stereo();
    let frame_bytes = usize::from(format.block_align);
    let release_hold_frames = RELEASE_HOLD_MS * u64::from(format.sample_rate) / 1_000;
    let mut active = false;
    let mut below_frames = 0_u64;
    let mut observations = Vec::new();

    for packet in packets {
        ensure!(
            packet.pcm.len() == packet.frames as usize * frame_bytes,
            "audio packet PCM length does not match frame count"
        );
        for (frame_offset, frame) in packet.pcm.chunks_exact(frame_bytes).enumerate() {
            let peak = frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs())
                .max()
                .unwrap_or(0);
            if peak >= DETECTION_THRESHOLD.unsigned_abs() {
                if !active {
                    let sample_qpc = if packet.timestamp_error() {
                        None
                    } else {
                        let packet_qpc = packet.first_frame_qpc(clock)?;
                        Some(
                            packet_qpc
                                + ((frame_offset as i128 * i128::from(clock.frequency()))
                                    / i128::from(format.sample_rate))
                                    as i64,
                        )
                    };
                    observations.push(AudioBurstObservation {
                        stream_frame: packet.stream_frame_start + frame_offset as u64,
                        packet_arrival_qpc: packet.arrival_qpc,
                        packet_qpc_position_100ns: packet.qpc_position_100ns,
                        sample_qpc,
                        timestamp_error: packet.timestamp_error(),
                    });
                }
                active = true;
                below_frames = 0;
            } else if active {
                below_frames = below_frames.saturating_add(1);
                if below_frames >= release_hold_frames {
                    active = false;
                    below_frames = 0;
                }
            }
        }
    }
    Ok(observations)
}

fn write_pcm_probe(run_dir: &Path, packets: &[AudioPacket]) -> anyhow::Result<()> {
    let path = run_dir.join("audio-calibration.pcm.zst");
    let file =
        File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut encoder = zstd::Encoder::new(file, 3)?;
    for packet in packets {
        encoder.write_all(&packet.pcm)?;
    }
    encoder.finish()?.sync_all()?;
    Ok(())
}

fn distribution(mut values: Vec<f64>) -> CalibrationDistribution {
    if values.is_empty() {
        return CalibrationDistribution::default();
    }
    values.sort_by(f64::total_cmp);
    CalibrationDistribution {
        count: values.len(),
        min: values[0],
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        max: values[values.len() - 1],
    }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    Some(percentile(&values, 0.50))
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    let index = ((values.len() - 1) as f64 * quantile).round() as usize;
    values[index]
}

fn default_limitations() -> Vec<String> {
    vec![
        "This is a non-trainable capture-lab probe; it does not change release capture, RAW schemas, upload or publication."
            .into(),
        "The docs/56 hard gate is not decided by one run: constant offset stability must be demonstrated across runs before hard_gate_passed can become true."
            .into(),
        "An OS/driver access violation while copying a stale GetBuffer pointer cannot be isolated by a Rust thread; the known 22H2 exclusive-fullscreen path remains a release blocker."
            .into(),
    ]
}

fn mm_ok(code: u32, operation: &str) -> anyhow::Result<()> {
    ensure!(code == 0, "{operation} failed with MMRESULT {code}");
    Ok(())
}

struct KernelEvent {
    handle: HANDLE,
}

impl KernelEvent {
    fn auto_reset() -> anyhow::Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, None) }
            .context("failed to create tone playback event")?;
        Ok(Self { handle })
    }
}

impl Drop for KernelEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_schedule_uses_half_second_lead_and_one_second_period() {
        let schedule = tone_schedule(10_000, Duration::from_secs(3), 10_000_000);
        assert_eq!(schedule.first_burst_qpc, 5_010_000);
        assert_eq!(schedule.period_qpc, 10_000_000);
        assert_eq!(schedule.burst_count, 3);
        assert_eq!(schedule.burst_index_at(schedule.first_burst_qpc), Some(0));
        assert_eq!(
            schedule.burst_index_at(schedule.first_burst_qpc + schedule.burst_duration_qpc),
            None
        );
    }

    #[test]
    fn generated_tone_has_silence_then_periodic_bursts() {
        let format = AudioPcmFormat::pcm_s16le_48k_stereo();
        let pcm = generate_tone_pcm(Duration::from_secs(3), format);
        let frame = |index: usize| pcm[index * usize::from(format.channels)];
        assert_eq!(frame(1_000), 0);
        assert_ne!(frame(24_000 + 12), 0);
        assert_eq!(frame(24_000 + 3_000), 0);
        assert_ne!(frame(72_000 + 12), 0);
    }

    #[test]
    fn residual_distribution_removes_constant_offset() {
        let values = [30_000.0, 30_100.0, 29_900.0];
        let offset = median(&values).unwrap();
        let residuals = values
            .iter()
            .map(|value| (value - offset).abs())
            .collect::<Vec<_>>();
        let result = distribution(residuals);
        assert_eq!(offset, 30_000.0);
        assert_eq!(result.max, 100.0);
    }

    #[test]
    fn burst_evidence_requires_completed_render_flashes_and_ordered_video_markers() {
        let markers = [0_u32, 1]
            .into_iter()
            .map(|burst_index| VideoBurstMarker {
                burst_index,
                output_frame: u64::from(burst_index),
                output_frame_qpc: i64::from(burst_index),
                source_qpc: i64::from(burst_index),
            })
            .collect::<Vec<_>>();

        assert!(burst_evidence_complete(2, 2, &markers, 2, 2));
        assert!(!burst_evidence_complete(2, 1, &markers, 2, 2));
        let reversed = [markers[1].clone(), markers[0].clone()];
        assert!(!burst_evidence_complete(2, 2, &reversed, 2, 2));
    }
}
