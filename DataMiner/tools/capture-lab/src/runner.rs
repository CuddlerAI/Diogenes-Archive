use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use capture_core::capture::{
    CaptureIngress, CapturedFrame, ENCODER_DRAIN_CFR_FRAMES, GpuCopyHandler,
    WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME, copy_queries_after_start, gpu_copy_wait_limit_qpc,
    textures_after_start, wait_for_gpu_copy_completion,
};
use capture_core::crop::{CropRect, capture_surface_size, client_area_crop};
use capture_core::d3d::{AdapterIdentity, D3dContext};
use capture_core::gpu::{
    BgraToNv12, VideoConversionSpec, create_bgra_copy_texture, create_nv12_texture,
};
use capture_core::mf::{
    EffectiveEncoderConfig, EncodedH264Sample, H264Mp4Writer, HardwareH264Encoder, MfPlatform,
};
use capture_core::platform::WindowsVersion;
use capture_core::probe::{Mp4Probe, probe_h264_mp4};
use capture_core::profile::CaptureProfile;
use capture_core::qpc::{QpcClock, frame_pts_100ns};
use capture_core::recording::validate_encoded_sample_timing;
use crossbeam_channel::{Receiver, Sender, bounded};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess, GetProcessTimes,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
use windows::core::PCWSTR;
use windows_capture_wgc::{CaptureControl, WgcSettings, Window};

use crate::audio_calibration::{
    ActiveAudioCalibration, AudioCalibrationReport, AudioCalibrationState, VideoBurstMarker,
};
use crate::nvml::{GpuReport, NvmlSampler};
use crate::synthetic::{RenderTelemetry, RenderTelemetrySnapshot};

const NV12_POOL_SIZE: usize = 6;
const NV12_CANONICAL_SLOT: usize = 0;
const SEAL_DELAY_MS: i64 = 4;

#[derive(Clone, Debug, Serialize)]
pub struct FrameTiming {
    pub frame: u64,
    pub session_frame: u64,
    pub output_qpc_start: i64,
    pub output_qpc_end: i64,
    pub source_qpc: i64,
    pub source_seq: u64,
    pub duplicate: bool,
    pub source_superseded: u32,
    pub queue_drop: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SegmentSummary {
    pub index: u32,
    pub frame_count: u64,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub video_bytes: u64,
    pub video_sha256: String,
    pub frames_sha256: String,
    pub events_sha256: String,
    pub meta_sha256: String,
    pub effective_encoder: EffectiveEncoderConfig,
    pub probe: Mp4Probe,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Distribution {
    pub count: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GameFpsReport {
    pub frames: usize,
    pub average_fps: f64,
    pub one_percent_low_fps: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GamePerformanceReport {
    pub baseline: GameFpsReport,
    pub capture: GameFpsReport,
    pub average_fps_drop_percent: f64,
    pub one_percent_low_drop_percent: f64,
    pub passed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    Smoke,
    Custom,
    P0Endurance,
}

#[derive(Clone, Debug, Serialize)]
pub struct LabReport {
    pub schema_version: &'static str,
    pub run_id: String,
    pub non_trainable: bool,
    pub production_certified: bool,
    pub passed: bool,
    pub validation_mode: ValidationMode,
    pub integrity_passed: bool,
    pub p0_acceptance_evaluated: bool,
    pub p0_acceptance_passed: bool,
    pub acceptance_failures: Vec<String>,
    pub fork_id: &'static str,
    pub windows: WindowsVersion,
    pub contract: PipelineContract,
    pub profile: CaptureProfile,
    pub synthetic_source_rate_multiplier: u32,
    pub adapter: AdapterIdentity,
    pub crop: CropRect,
    pub surface_width: u32,
    pub surface_height: u32,
    pub qpc_frequency: i64,
    pub callback_latency_us: Distribution,
    pub gpu_copy_completion_latency_us: Distribution,
    pub pipeline_submit_latency_us: Distribution,
    pub gpu_conversion_latency_us: Distribution,
    pub encoder_progress_latency_us: Distribution,
    pub encoder_surface_wait_latency_us: Distribution,
    pub encoder_write_latency_us: Distribution,
    pub max_capture_queue_depth: usize,
    pub capture_queue_drops: u64,
    pub duplicate_frames: u64,
    pub superseded_source_frames: u64,
    pub nv12_surface_pool: usize,
    pub max_encoder_surfaces_in_flight: usize,
    pub boundary_gap_qpc_ticks: Vec<i64>,
    pub process_cpu_percent_total_capacity: f64,
    pub gpu: GpuReport,
    pub game_performance: GamePerformanceReport,
    pub audio_calibration: Option<AudioCalibrationReport>,
    pub segments: Vec<SegmentSummary>,
}

impl LabReport {
    pub fn apply_game_performance(&mut self, performance: GamePerformanceReport) {
        self.game_performance = performance;
        let run_surfaces_passed = run_surfaces_passed(
            self.integrity_passed,
            self.audio_calibration.as_ref(),
            self.game_performance.passed,
        );
        self.passed = run_surfaces_passed;
        if self.validation_mode != ValidationMode::P0Endurance {
            return;
        }

        self.p0_acceptance_evaluated = true;
        if self.profile.segment_seconds != 10 || self.profile.test_segments < 30 {
            self.acceptance_failures
                .push("P0 endurance requires at least 30 consecutive 10-second segments".into());
        }
        if self.segments.len() != self.profile.test_segments as usize {
            self.acceptance_failures.push(format!(
                "expected {} completed segments, found {}",
                self.profile.test_segments,
                self.segments.len()
            ));
        }
        if self.duplicate_frames != 0 {
            self.acceptance_failures.push(format!(
                "synthetic CFR run emitted {} duplicate frame(s)",
                self.duplicate_frames
            ));
        }
        if !self.game_performance.passed {
            self.acceptance_failures.push(format!(
                "synthetic game performance exceeded limits: average drop {:.3}%, 1% low drop {:.3}%",
                self.game_performance.average_fps_drop_percent,
                self.game_performance.one_percent_low_drop_percent
            ));
        }
        self.p0_acceptance_passed = run_surfaces_passed && self.acceptance_failures.is_empty();
        self.passed = self.p0_acceptance_passed;
    }
}

fn audio_calibration_passed(audio: Option<&AudioCalibrationReport>) -> bool {
    audio.is_none_or(|audio| {
        matches!(audio.state, AudioCalibrationState::Ready) && audio.single_run_timing_passed
    })
}

fn run_surfaces_passed(
    integrity_passed: bool,
    audio: Option<&AudioCalibrationReport>,
    game_performance_passed: bool,
) -> bool {
    integrity_passed
        && audio_calibration_passed(audio)
        && (audio.is_none() || game_performance_passed)
}

#[derive(Debug, Serialize)]
struct FailureReport {
    schema_version: &'static str,
    run_id: String,
    non_trainable: bool,
    production_certified: bool,
    passed: bool,
    error: String,
    completed_segments_retained: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PipelineContract {
    pub source: &'static str,
    pub callback_operation: &'static str,
    pub callback_cpu_readback_calls: u32,
    pub crop_and_color_conversion: &'static str,
    pub encoder: &'static str,
    pub encoder_hardware_only: bool,
    pub explicit_dedicated_threads: u32,
}

pub struct RunConfig {
    pub profile: CaptureProfile,
    pub output_root: PathBuf,
    pub validation_mode: ValidationMode,
    pub render_telemetry: Arc<RenderTelemetry>,
    pub synthetic_source_rate_multiplier: u32,
    pub audio_loopback: bool,
}

struct ComGuard;

impl ComGuard {
    fn mta() -> anyhow::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("CoInitializeEx(MTA) failed")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct FinalizeJob {
    index: u32,
    writer: H264Mp4Writer,
    timing: Vec<FrameTiming>,
    staging_dir: PathBuf,
    final_dir: PathBuf,
    prepare: Option<(u32, PathBuf)>,
    effective_encoder: EffectiveEncoderConfig,
}

// The MP4 muxer is created for MTA use. windows-rs conservatively marks its COM
// wrappers !Send, so the dedicated, joinable finalizer handoff is explicit.
unsafe impl Send for FinalizeJob {}

struct FinalizeResult {
    summary: SegmentSummary,
    prepared: Option<(u32, H264Mp4Writer)>,
}

unsafe impl Send for FinalizeResult {}

struct Finalizer {
    jobs: Sender<FinalizeJob>,
    results: Receiver<anyhow::Result<FinalizeResult>>,
    join: Option<JoinHandle<()>>,
}

impl Finalizer {
    fn start(profile: CaptureProfile) -> anyhow::Result<Self> {
        let (jobs, job_rx) = bounded::<FinalizeJob>(1);
        let (result_tx, results) = bounded::<anyhow::Result<FinalizeResult>>(1);
        let join = thread::Builder::new()
            .name("dm-finalize".into())
            .spawn(move || {
                let _com = match ComGuard::mta() {
                    Ok(com) => com,
                    Err(error) => {
                        let _ = result_tx.send(Err(error));
                        return;
                    }
                };
                while let Ok(job) = job_rx.recv() {
                    let result = finalize_segment(job, &profile);
                    if result_tx.send(result).is_err() {
                        return;
                    }
                }
            })
            .context("failed to spawn finalize worker")?;
        Ok(Self {
            jobs,
            results,
            join: Some(join),
        })
    }

    fn submit(&self, job: FinalizeJob) -> anyhow::Result<()> {
        self.jobs
            .send(job)
            .map_err(|_| anyhow::anyhow!("finalize worker stopped before accepting segment"))
    }

    fn try_result(&self) -> anyhow::Result<Option<FinalizeResult>> {
        match self.results.try_recv() {
            Ok(result) => result.map(Some),
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                anyhow::bail!("finalize worker exited unexpectedly")
            }
        }
    }

    fn recv_result(&self) -> anyhow::Result<FinalizeResult> {
        self.results
            .recv()
            .context("finalize worker exited unexpectedly")?
    }

    fn stop(mut self) -> anyhow::Result<()> {
        self.close_jobs();
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| anyhow::anyhow!("finalize worker panicked"))?;
        }
        Ok(())
    }

    fn close_jobs(&mut self) {
        let (replacement, receiver) = bounded(0);
        drop(receiver);
        drop(std::mem::replace(&mut self.jobs, replacement));
    }
}

impl Drop for Finalizer {
    fn drop(&mut self) {
        self.close_jobs();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn run(hwnd_value: usize, config: RunConfig) -> anyhow::Result<LabReport> {
    validate_lab_profile(&config.profile)?;
    let _com = ComGuard::mta()?;
    let clock = QpcClock::new()?;
    let hwnd = HWND(hwnd_value as *mut _);
    let run_id = format!(
        "p0-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    );
    let run_dir = config.output_root.join(&run_id);
    fs::create_dir_all(run_dir.join("segments"))
        .with_context(|| format!("failed to create {}", run_dir.display()))?;

    let result = run_inner(hwnd, &config, &run_id, &run_dir, clock);
    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            let completed_segments_retained = count_completed_segments(&run_dir);
            let failure_path = run_dir.join("failure.json");
            let failure = FailureReport {
                schema_version: "dm-p0-lab-1",
                run_id,
                non_trainable: true,
                production_certified: false,
                passed: false,
                error: format!("{error:#}"),
                completed_segments_retained,
            };
            write_json(&failure_path, &failure)?;
            return Err(error.context(format!(
                "P0 run failed; diagnostic report written to {}",
                failure_path.display()
            )));
        }
    };
    Ok(report)
}

pub fn validate_lab_profile(profile: &CaptureProfile) -> anyhow::Result<()> {
    profile.validate()?;
    ensure!(
        profile.non_trainable,
        "capture-lab accepts only explicitly non-trainable profiles"
    );
    Ok(())
}

fn count_completed_segments(run_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(run_dir.join("segments")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            completed_segment_manifest_valid(&path)
        })
        .count()
}

fn completed_segment_manifest_valid(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path.join("sha256.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
    else {
        return false;
    };
    [
        "video.mp4",
        "events.jsonl.zst",
        "frames.jsonl.zst",
        "meta.json",
    ]
    .iter()
    .all(|name| {
        let Some(expected) = manifest.get(*name).and_then(serde_json::Value::as_str) else {
            return false;
        };
        sha256_file(&path.join(name)).is_ok_and(|actual| actual == expected)
    })
}

fn run_inner(
    hwnd: HWND,
    config: &RunConfig,
    run_id: &str,
    run_dir: &Path,
    clock: QpcClock,
) -> anyhow::Result<LabReport> {
    let profile = &config.profile;
    let validation_mode = config.validation_mode;
    let render_telemetry = &config.render_telemetry;
    let synthetic_source_rate_multiplier = config.synthetic_source_rate_multiplier;
    validate_lab_profile(profile)?;
    let windows = WindowsVersion::current()?;
    windows.require_hardware_only_media_foundation()?;
    let d3d = D3dContext::create_nvidia_hardware_only()?;
    let crop = client_area_crop(hwnd)?;
    ensure!(
        crop.width() == profile.width && crop.height() == profile.height,
        "synthetic client area {}x{} does not match profile {}x{}",
        crop.width(),
        crop.height(),
        profile.width,
        profile.height
    );
    let (surface_width, surface_height) = capture_surface_size(hwnd)?;
    crop.validate_for_surface(surface_width, surface_height)?;

    let converter = BgraToNv12::new(
        &d3d.device,
        &d3d.immediate_context,
        VideoConversionSpec {
            surface_width,
            surface_height,
            crop,
            output_width: profile.width,
            output_height: profile.height,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
        },
    )?;
    let nv12_textures = (0..NV12_POOL_SIZE)
        .map(|_| create_nv12_texture(&d3d.device, profile.width, profile.height))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut free_nv12: VecDeque<usize> = (1..NV12_POOL_SIZE).collect();

    let _mf = MfPlatform::startup()?;
    preflight_hardware_path(
        &converter,
        &d3d.device,
        &d3d.immediate_context,
        &nv12_textures,
        surface_width,
        surface_height,
        profile,
        run_dir,
    )?;
    let final_segment_dir = |index: u32| run_dir.join("segments").join(format!("{index:05}"));
    let staging_segment_dir =
        |index: u32| run_dir.join("segments").join(format!(".{index:05}.tmp"));
    fs::create_dir_all(staging_segment_dir(0))?;
    let mut encoder = HardwareH264Encoder::create(&d3d.device, profile)?;
    let effective_encoder = encoder.effective_encoder().clone();
    let mut current = H264Mp4Writer::create(staging_segment_dir(0).join("video.mp4"), profile)?;
    let mut next = if profile.test_segments > 1 {
        fs::create_dir_all(staging_segment_dir(1))?;
        Some((
            1,
            H264Mp4Writer::create(staging_segment_dir(1).join("video.mp4"), profile)?,
        ))
    } else {
        None
    };
    let finalizer = Finalizer::start(profile.clone())?;
    let capture_duration = Duration::from_secs(
        u64::from(profile.segment_seconds)
            .checked_mul(u64::from(profile.test_segments))
            .context("capture duration overflowed")?,
    );
    let mut audio_failure = None;
    let mut audio_calibration = if config.audio_loopback {
        match ActiveAudioCalibration::prepare(clock) {
            Ok(calibration) => Some(calibration),
            Err(error) => {
                audio_failure = Some(AudioCalibrationReport::failed(format!("{error:#}")));
                None
            }
        }
    } else {
        None
    };

    // StartCapture is intentionally last: device, converter, NV12 pool and both
    // persistent encoder and the first two muxers are ready before the
    // bounded callback queue can fill.
    let mut ingress = CaptureIngress::new(clock, None);
    let flags = ingress.take_flags()?;
    let settings = WgcSettings {
        window: Window::from_raw_hwnd(hwnd.0),
        device: d3d.device.clone(),
        device_context: d3d.immediate_context.clone(),
        cursor_capture: true,
        draw_border: false,
        include_secondary_windows: false,
        // Ask WGC often enough that an otherwise on-rate native frame is not
        // coalesced merely because compositor and game clocks have different
        // phases. This remains profile-derived and does not change output
        // cadence; the QPC FramePacer below is still the sole CFR clock.
        minimum_update_interval: Duration::from_nanos(
            (1_000_000_000_u64 * u64::from(profile.fps_den))
                / (u64::from(profile.fps_num)
                    * u64::try_from(WGC_SOURCE_OBSERVATIONS_PER_CFR_FRAME)?),
        ),
        flags,
    };
    let mut capture = Some(
        CaptureControl::<GpuCopyHandler>::start(settings)
            .map_err(|error| anyhow::anyhow!("WGC HWND capture failed: {error}"))?,
    );
    let bgra_textures = textures_after_start(&ingress)?.to_vec();
    let copy_queries = copy_queries_after_start(&ingress)?.to_vec();
    ensure!(
        copy_queries.len() == bgra_textures.len(),
        "GPU-copy texture/query ring length mismatch"
    );
    let video_surfaces = converter.prepare_surfaces(&bgra_textures, &nv12_textures)?;
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { bgra_textures[0].GetDesc(&mut source_desc) };
    ensure!(
        source_desc.Width == surface_width && source_desc.Height == surface_height,
        "WGC surface geometry changed between preflight and StartCapture"
    );

    let expected_per_segment = profile.expected_frames_per_segment();
    let total_frames = profile.expected_frames_for_test();
    let segment_duration_qpc = clock.frequency() * i64::from(profile.segment_seconds);
    let first = ingress
        .frames
        .recv_timeout(Duration::from_secs(5))
        .context("timed out waiting for first WGC frame")?;
    let anchor_qpc = first.source_qpc;
    let mut future = Some(first);
    if let Some(calibration) = audio_calibration.as_mut() {
        calibration.drain();
        match calibration.start_playback(capture_duration, clock) {
            Ok(schedule) => render_telemetry.configure_audio_schedule(schedule),
            Err(error) => {
                audio_failure = Some(AudioCalibrationReport::failed(format!("{error:#}")));
            }
        }
    }
    let mut last_source_qpc = anchor_qpc;
    let mut last_source_seq = 0_u64;
    let mut have_canonical = false;
    let mut callback_latencies = Vec::with_capacity(total_frames as usize + 16);
    let mut gpu_copy_completion_latencies = Vec::with_capacity(total_frames as usize + 16);
    let mut pipeline_submit_latencies = Vec::with_capacity(total_frames as usize);
    let mut gpu_conversion_latencies = Vec::with_capacity(total_frames as usize);
    let mut encoder_progress_latencies = Vec::with_capacity(total_frames as usize);
    let mut encoder_surface_wait_latencies = Vec::with_capacity(total_frames as usize);
    let mut encoder_write_latencies = Vec::with_capacity(total_frames as usize);
    let mut timing = Vec::with_capacity(expected_per_segment as usize);
    let mut summaries = Vec::with_capacity(profile.test_segments as usize);
    let mut duplicates = 0_u64;
    let mut superseded_total = 0_u64;
    let mut max_encoder_surfaces_in_flight = 0_usize;
    let mut active_segment = 0_u32;
    let mut encoded_frames_written = 0_u64;
    let mut encoded_pts_offset_100ns = None;
    let mut nvml = NvmlSampler::load()?;
    ensure!(
        nvml.device_name() == d3d.adapter.description,
        "NVML device '{}' does not match D3D11 adapter '{}'; recording stopped",
        nvml.device_name(),
        d3d.adapter.description,
    );
    let capture_phase = render_telemetry.begin_capture();
    let process_start = process_times()?;
    let wall_start = Instant::now();
    let mut audio_video_markers = Vec::new();
    let mut last_audio_video_burst = None;

    for session_frame in 0..total_frames {
        while let Some(result) = finalizer.try_result()? {
            accept_finalize_result(result, &mut summaries, &mut next)?;
        }
        drain_encoded_samples(
            &encoder,
            &mut current,
            &mut encoded_frames_written,
            &mut encoded_pts_offset_100ns,
            profile,
        )?;

        let slot_start = slot_qpc(anchor_qpc, session_frame, clock.frequency(), profile);
        let slot_end = slot_qpc(anchor_qpc, session_frame + 1, clock.frequency(), profile);
        let segment_index = segment_index_for_slot(
            anchor_qpc,
            slot_start,
            segment_duration_qpc,
            profile.test_segments,
        )?;
        if segment_index != active_segment {
            let drain_start = clock.now()?;
            let drain_limit = encoder_progress_limit_qpc(profile, clock.frequency());
            while encoded_frames_written < session_frame {
                drain_encoded_samples(
                    &encoder,
                    &mut current,
                    &mut encoded_frames_written,
                    &mut encoded_pts_offset_100ns,
                    profile,
                )?;
                ensure!(
                    clock.now()? - drain_start < drain_limit,
                    "persistent hardware encoder did not emit segment {} tail before the next boundary; recording stopped",
                    active_segment
                );
                std::thread::sleep(Duration::from_micros(200));
            }
            ensure!(
                current.frames_written() == expected_per_segment,
                "segment {} muxed {} frames, expected {expected_per_segment}",
                active_segment,
                current.frames_written()
            );
            ensure!(
                segment_index == active_segment + 1,
                "output timeline skipped a segment boundary"
            );
            let (prepared_index, prepared) = next
                .take()
                .context("next MP4 muxer was not prepared before the segment boundary")?;
            ensure!(
                prepared_index == segment_index,
                "prepared MP4 muxer index mismatch"
            );
            let old = std::mem::replace(&mut current, prepared);
            let prepare_index = segment_index + 1;
            let prepare = (prepare_index < profile.test_segments).then(|| {
                (
                    prepare_index,
                    staging_segment_dir(prepare_index).join("video.mp4"),
                )
            });
            if let Some((index, _)) = &prepare {
                fs::create_dir_all(staging_segment_dir(*index))?;
            }
            finalizer.submit(FinalizeJob {
                index: active_segment,
                writer: old,
                timing: std::mem::take(&mut timing),
                staging_dir: staging_segment_dir(active_segment),
                final_dir: final_segment_dir(active_segment),
                prepare,
                effective_encoder: effective_encoder.clone(),
            })?;
            active_segment = segment_index;
        }
        let local_frame = timing.len() as u64;

        let seal_qpc = slot_end + clock.frequency() * SEAL_DELAY_MS / 1_000;
        let mut selected = None;
        let mut superseded = 0_u32;
        loop {
            let (frame, newly_received) = if let Some(frame) = future.take() {
                (Some(frame), false)
            } else {
                let now = clock.now()?;
                if now >= seal_qpc {
                    (None, false)
                } else {
                    let wait =
                        Duration::from_secs_f64((seal_qpc - now) as f64 / clock.frequency() as f64);
                    match ingress.frames.recv_timeout(wait) {
                        Ok(frame) => (Some(frame), true),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => (None, false),
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            anyhow::bail!("WGC callback channel disconnected")
                        }
                    }
                }
            };
            let Some(frame) = frame else {
                let drops = ingress.metrics.drops.load(Ordering::Relaxed);
                if drops > 0 {
                    let max_submit_us = pipeline_submit_latencies
                        .iter()
                        .copied()
                        .fold(0.0_f64, f64::max);
                    let max_gpu_us = gpu_conversion_latencies
                        .iter()
                        .copied()
                        .fold(0.0_f64, f64::max);
                    let max_progress_us = encoder_progress_latencies
                        .iter()
                        .copied()
                        .fold(0.0_f64, f64::max);
                    let max_surface_wait_us = encoder_surface_wait_latencies
                        .iter()
                        .copied()
                        .fold(0.0_f64, f64::max);
                    let max_write_us = encoder_write_latencies
                        .iter()
                        .copied()
                        .fold(0.0_f64, f64::max);
                    anyhow::bail!(
                        "WGC callback queue dropped {drops} frame(s) at output frame {session_frame}; prior max submit {max_submit_us:.3} us (GPU {max_gpu_us:.3}, encoder-progress {max_progress_us:.3}, surface-wait {max_surface_wait_us:.3}, WriteSample {max_write_us:.3}); recording stopped"
                    );
                }
                if capture.as_ref().is_some_and(CaptureControl::is_finished) {
                    let stopped = capture.take().expect("capture control disappeared").wait();
                    match stopped {
                        Ok(()) => anyhow::bail!("WGC control thread stopped unexpectedly"),
                        Err(error) => anyhow::bail!("WGC stopped with a fatal error: {error}"),
                    }
                }
                break;
            };
            if newly_received || frame.source_seq == 1 {
                callback_latencies
                    .push(clock.elapsed_micros(frame.callback_start_qpc, frame.callback_ready_qpc));
                gpu_copy_completion_latencies.push(wait_for_gpu_copy_completion(
                    &d3d.immediate_context,
                    &copy_queries[frame.slot],
                    &clock,
                    &ingress.metrics,
                    frame.callback_start_qpc,
                    gpu_copy_wait_limit_qpc(profile.fps_num, profile.fps_den, clock.frequency()),
                )?);
            }
            if frame.source_qpc >= slot_end {
                future = Some(frame);
                break;
            }
            if let Some(previous) = selected.replace(frame) {
                ingress.recycle.send(previous.slot)?;
                superseded += 1;
            }
        }

        let pipeline_submit_start = clock.now()?;
        if let Some(source) = selected {
            let gpu_start = clock.now()?;
            converter.convert(
                &video_surfaces,
                source.slot,
                NV12_CANONICAL_SLOT,
                session_frame as u32,
            )?;
            gpu_conversion_latencies.push(clock.elapsed_micros(gpu_start, clock.now()?));
            ingress.recycle.send(source.slot)?;
            last_source_qpc = source.source_qpc;
            last_source_seq = source.source_seq;
            have_canonical = true;
        } else {
            ensure!(have_canonical, "first CFR slot had no source frame");
            duplicates += 1;
        }
        superseded_total += u64::from(superseded);

        let progress_start = clock.now()?;
        for released in encoder.take_released()? {
            free_nv12.push_back(released as usize);
        }
        drain_encoded_samples(
            &encoder,
            &mut current,
            &mut encoded_frames_written,
            &mut encoded_pts_offset_100ns,
            profile,
        )?;
        encoder_progress_latencies.push(clock.elapsed_micros(progress_start, clock.now()?));
        let surface_wait_start = clock.now()?;
        let surface_wait_limit = encoder_progress_limit_qpc(profile, clock.frequency());
        let output_slot = loop {
            for released in encoder.take_released()? {
                free_nv12.push_back(released as usize);
            }
            drain_encoded_samples(
                &encoder,
                &mut current,
                &mut encoded_frames_written,
                &mut encoded_pts_offset_100ns,
                profile,
            )?;
            if let Some(slot) = free_nv12.pop_front() {
                break slot;
            }
            let drops = ingress.metrics.drops.load(Ordering::Relaxed);
            if drops > 0 {
                let max_write_us = encoder_write_latencies
                    .iter()
                    .copied()
                    .fold(0.0_f64, f64::max);
                let max_gpu_copy_us = gpu_copy_completion_latencies
                    .iter()
                    .copied()
                    .fold(0.0_f64, f64::max);
                anyhow::bail!(
                    "source queue dropped {drops} frame(s) while waiting for an encoder surface at output frame {session_frame}; current writer has {} surface(s) in flight, prior max WriteSample {max_write_us:.3} us, max GPU-copy completion {max_gpu_copy_us:.3} us",
                    encoder.in_flight()
                );
            }
            let elapsed = clock.now()? - surface_wait_start;
            ensure!(
                elapsed < surface_wait_limit,
                "NV12 backpressure exceeded {ENCODER_DRAIN_CFR_FRAMES} output frame(s) with {} current-writer surfaces and {} total surfaces in flight; recording stopped",
                encoder.in_flight(),
                NV12_POOL_SIZE - 1,
            );
            std::thread::sleep(Duration::from_micros(200));
        };
        encoder_surface_wait_latencies.push(clock.elapsed_micros(surface_wait_start, clock.now()?));
        unsafe {
            d3d.immediate_context.CopyResource(
                &nv12_textures[output_slot],
                &nv12_textures[NV12_CANONICAL_SLOT],
            );
        }
        let write_start = clock.now()?;
        encoder.write_texture(
            &nv12_textures[output_slot],
            output_slot as u32,
            session_frame,
            local_frame == 0,
        )?;
        encoder_write_latencies.push(clock.elapsed_micros(write_start, clock.now()?));
        drain_encoded_samples(
            &encoder,
            &mut current,
            &mut encoded_frames_written,
            &mut encoded_pts_offset_100ns,
            profile,
        )?;
        pipeline_submit_latencies.push(clock.elapsed_micros(pipeline_submit_start, clock.now()?));
        max_encoder_surfaces_in_flight =
            max_encoder_surfaces_in_flight.max((NV12_POOL_SIZE - 1) - free_nv12.len());
        if let Some(calibration) = audio_calibration.as_mut() {
            calibration.drain();
            if let Some(burst_index) = render_telemetry.painted_audio_burst_at(last_source_qpc)
                && last_audio_video_burst != Some(burst_index)
            {
                audio_video_markers.push(VideoBurstMarker {
                    burst_index,
                    output_frame: session_frame,
                    output_frame_qpc: slot_start,
                    source_qpc: last_source_qpc,
                });
                last_audio_video_burst = Some(burst_index);
            }
        }
        if session_frame % u64::from(profile.fps_num / profile.fps_den.max(1)).max(1) == 0 {
            nvml.sample()?;
        }

        timing.push(FrameTiming {
            frame: local_frame,
            session_frame,
            output_qpc_start: slot_start,
            output_qpc_end: slot_end,
            source_qpc: last_source_qpc,
            source_seq: last_source_seq,
            duplicate: selected.is_none(),
            source_superseded: superseded,
            queue_drop: ingress.metrics.drops.load(Ordering::Relaxed),
        });
    }

    if let Some(future) = future.take() {
        ingress.recycle.send(future.slot)?;
    }
    let capture_result = capture
        .take()
        .expect("capture control disappeared before shutdown")
        .stop();
    if let Err(error) = capture_result {
        anyhow::bail!("WGC stopped with a fatal error: {error}");
    }
    let finished_encoder = encoder.finish()?;
    ensure!(
        finished_encoder.retained_slots.is_empty(),
        "hardware encoder retained {} NV12 surface(s) after finalize",
        finished_encoder.retained_slots.len()
    );
    for released in finished_encoder.released_slots {
        free_nv12.push_back(released as usize);
    }
    write_encoded_samples(
        finished_encoder.encoded_samples,
        &mut current,
        &mut encoded_frames_written,
        &mut encoded_pts_offset_100ns,
        profile,
    )?;
    ensure!(
        encoded_frames_written == total_frames,
        "persistent hardware encoder emitted {encoded_frames_written} frames, expected {total_frames}"
    );
    ensure!(
        current.frames_written() == expected_per_segment,
        "final segment muxed {} frames, expected {expected_per_segment}",
        current.frames_written()
    );
    ensure!(
        free_nv12.len() == NV12_POOL_SIZE - 1,
        "hardware encoder retained {} NV12 surface(s) after finalize",
        (NV12_POOL_SIZE - 1).saturating_sub(free_nv12.len())
    );
    let audio_calibration = if let Some(calibration) = audio_calibration {
        let render_flash_qpc_ticks = render_telemetry.snapshot().audio_flash_qpc_ticks;
        let frame_duration_us =
            1_000_000.0 * f64::from(profile.fps_den) / f64::from(profile.fps_num);
        let finish_result = calibration.finish(
            clock,
            audio_video_markers,
            render_flash_qpc_ticks,
            frame_duration_us,
            run_dir,
        );
        if let Some(failure) = audio_failure {
            Some(failure)
        } else {
            match finish_result {
                Ok(report) => Some(report),
                Err(error) => Some(AudioCalibrationReport::failed(format!("{error:#}"))),
            }
        }
    } else {
        audio_failure
    };
    drop(capture_phase);
    let process_end = process_times()?;
    let logical_processors = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1);
    let cpu_100ns = process_end.saturating_sub(process_start);
    let process_cpu_percent_total_capacity = cpu_100ns as f64
        / 10_000_000.0
        / wall_start.elapsed().as_secs_f64()
        / f64::from(logical_processors)
        * 100.0;

    finalizer.submit(FinalizeJob {
        index: active_segment,
        writer: current,
        timing,
        staging_dir: staging_segment_dir(active_segment),
        final_dir: final_segment_dir(active_segment),
        prepare: None,
        effective_encoder,
    })?;
    while summaries.len() < profile.test_segments as usize {
        let result = finalizer.recv_result()?;
        accept_finalize_result(result, &mut summaries, &mut next)?;
    }
    finalizer.stop()?;
    summaries.sort_by_key(|segment| segment.index);

    let boundary_gap_qpc_ticks = summaries
        .windows(2)
        .map(|pair| pair[1].qpc_start - pair[0].qpc_end)
        .collect::<Vec<_>>();
    ensure!(
        boundary_gap_qpc_ticks.iter().all(|gap| *gap == 0),
        "segment timing contains a boundary gap"
    );
    ensure!(
        ingress.metrics.drops.load(Ordering::Relaxed) == 0,
        "capture queue dropped a frame"
    );

    Ok(LabReport {
        schema_version: "dm-p0-lab-1",
        run_id: run_id.to_owned(),
        non_trainable: true,
        production_certified: false,
        passed: false,
        validation_mode,
        integrity_passed: true,
        p0_acceptance_evaluated: false,
        p0_acceptance_passed: false,
        acceptance_failures: Vec::new(),
        fork_id: windows_capture_wgc::FORK_ID,
        windows,
        contract: PipelineContract {
            source: "HWND/CreateForWindow",
            callback_operation: "D3D11 CopyResource",
            callback_cpu_readback_calls: 0,
            crop_and_color_conversion: "D3D11 VideoProcessor RGB full-range to BT.709 limited NV12, no scaling",
            encoder: "Media Foundation H.264 High hardware MFT",
            encoder_hardware_only: true,
            // The lab probe adds one process-loopback capture thread and one
            // short-lived tone-render thread while calibration is active.
            explicit_dedicated_threads: 3 + 2 * u32::from(config.audio_loopback),
        },
        profile: profile.clone(),
        synthetic_source_rate_multiplier,
        adapter: d3d.adapter,
        crop,
        surface_width: source_desc.Width,
        surface_height: source_desc.Height,
        qpc_frequency: clock.frequency(),
        callback_latency_us: distribution(callback_latencies),
        gpu_copy_completion_latency_us: distribution(gpu_copy_completion_latencies),
        pipeline_submit_latency_us: distribution(pipeline_submit_latencies),
        gpu_conversion_latency_us: distribution(gpu_conversion_latencies),
        encoder_progress_latency_us: distribution(encoder_progress_latencies),
        encoder_surface_wait_latency_us: distribution(encoder_surface_wait_latencies),
        encoder_write_latency_us: distribution(encoder_write_latencies),
        max_capture_queue_depth: ingress.metrics.max_queue_depth.load(Ordering::Relaxed),
        capture_queue_drops: ingress.metrics.drops.load(Ordering::Relaxed),
        duplicate_frames: duplicates,
        superseded_source_frames: superseded_total,
        nv12_surface_pool: NV12_POOL_SIZE,
        max_encoder_surfaces_in_flight,
        boundary_gap_qpc_ticks,
        process_cpu_percent_total_capacity,
        gpu: nvml.report(),
        game_performance: GamePerformanceReport::default(),
        audio_calibration,
        segments: summaries,
    })
}

fn drain_encoded_samples(
    encoder: &HardwareH264Encoder,
    muxer: &mut H264Mp4Writer,
    encoded_frames_written: &mut u64,
    encoded_pts_offset_100ns: &mut Option<i64>,
    profile: &CaptureProfile,
) -> anyhow::Result<()> {
    write_encoded_samples(
        encoder.take_encoded()?,
        muxer,
        encoded_frames_written,
        encoded_pts_offset_100ns,
        profile,
    )
}

fn write_encoded_samples(
    samples: impl IntoIterator<Item = EncodedH264Sample>,
    muxer: &mut H264Mp4Writer,
    encoded_frames_written: &mut u64,
    encoded_pts_offset_100ns: &mut Option<i64>,
    profile: &CaptureProfile,
) -> anyhow::Result<()> {
    for sample in samples {
        let frame = *encoded_frames_written;
        validate_encoded_sample_timing(&sample, frame, encoded_pts_offset_100ns, profile)?;
        muxer.write_sample(&sample)?;
        *encoded_frames_written += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn preflight_hardware_path(
    converter: &BgraToNv12,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    nv12_textures: &[ID3D11Texture2D],
    surface_width: u32,
    surface_height: u32,
    profile: &CaptureProfile,
    run_dir: &Path,
) -> anyhow::Result<()> {
    ensure!(
        nv12_textures.len() >= 2,
        "hardware preflight requires two NV12 textures"
    );
    let scratch_bgra = create_bgra_copy_texture(device, surface_width, surface_height)?;
    let scratch_surfaces = converter.prepare_surfaces(
        std::slice::from_ref(&scratch_bgra),
        std::slice::from_ref(&nv12_textures[NV12_CANONICAL_SLOT]),
    )?;
    converter.convert(&scratch_surfaces, 0, 0, 0)?;
    unsafe {
        context.CopyResource(&nv12_textures[1], &nv12_textures[NV12_CANONICAL_SLOT]);
        context.Flush();
    }

    let path = run_dir.join("_hardware-preflight.mp4");
    let mut encoder = HardwareH264Encoder::create(device, profile)?;
    encoder.write_texture(&nv12_textures[1], 1, 0, true)?;
    let finished = encoder.finish()?;
    ensure!(
        finished.released_slots == [1] && finished.retained_slots.is_empty(),
        "hardware preflight did not release its NV12 surface"
    );
    ensure!(
        finished.encoded_samples.len() == 1,
        "hardware preflight emitted {} H.264 samples for one input frame",
        finished.encoded_samples.len()
    );
    validate_encoded_sample_timing(&finished.encoded_samples[0], 0, &mut None, profile)?;
    let mut muxer = H264Mp4Writer::create(&path, profile)?;
    muxer.write_sample(&finished.encoded_samples[0])?;
    muxer.finish()?;
    let probe = probe_h264_mp4(&path, 1, profile)?;
    ensure!(
        probe.first_sample_clean_point && probe.pts_monotonic,
        "hardware preflight did not produce a valid first IDR sample"
    );
    fs::remove_file(&path).context("failed to remove completed hardware preflight file")?;
    Ok(())
}

fn accept_finalize_result(
    result: FinalizeResult,
    summaries: &mut Vec<SegmentSummary>,
    next: &mut Option<(u32, H264Mp4Writer)>,
) -> anyhow::Result<()> {
    if let Some(prepared) = result.prepared {
        ensure!(
            next.replace(prepared).is_none(),
            "finalizer prepared two standby MP4 muxers"
        );
    }
    summaries.push(result.summary);
    Ok(())
}

fn finalize_segment(job: FinalizeJob, profile: &CaptureProfile) -> anyhow::Result<FinalizeResult> {
    let finalize_start = Instant::now();
    let writer_finish_start = Instant::now();
    job.writer.finish()?;
    tracing::info!(
        segment_index = job.index,
        elapsed_ms = writer_finish_start.elapsed().as_secs_f64() * 1_000.0,
        "segment writer finalized"
    );
    let video_path = job.staging_dir.join("video.mp4");
    let probe_start = Instant::now();
    let probe = probe_h264_mp4(&video_path, job.timing.len() as u64, profile)?;
    tracing::info!(
        segment_index = job.index,
        elapsed_ms = probe_start.elapsed().as_secs_f64() * 1_000.0,
        "segment MP4 probed"
    );
    let artifact_start = Instant::now();
    let frames_path = job.staging_dir.join("frames.jsonl.zst");
    write_jsonl_zstd(&frames_path, &job.timing)?;
    let events_path = job.staging_dir.join("events.jsonl.zst");
    write_jsonl_zstd::<serde_json::Value>(&events_path, &[])?;

    let video_sha256 = sha256_file(&video_path)?;
    let frames_sha256 = sha256_file(&frames_path)?;
    let events_sha256 = sha256_file(&events_path)?;
    let qpc_start = job
        .timing
        .first()
        .context("segment timing is empty")?
        .output_qpc_start;
    let qpc_end = job
        .timing
        .last()
        .context("segment timing is empty")?
        .output_qpc_end;
    let video_bytes = fs::metadata(&video_path)?.len();
    let meta_path = job.staging_dir.join("meta.json");
    let meta = serde_json::json!({
        "schema_version": "dm-p0-lab-1",
        "segment_index": job.index,
        "state": "p0_lab_complete",
        "non_trainable": true,
        "production_certified": false,
        "qpc_start": qpc_start,
        "qpc_end": qpc_end,
        "video": {"path": "video.mp4", "bytes": video_bytes, "sha256": video_sha256},
        "frames": {"path": "frames.jsonl.zst", "lines": job.timing.len(), "sha256": frames_sha256},
        "events": {"path": "events.jsonl.zst", "lines": 0, "sha256": events_sha256},
        "effective_encoder": &job.effective_encoder,
        "probe": &probe
    });
    write_json(&meta_path, &meta)?;
    let meta_sha256 = sha256_file(&meta_path)?;
    let manifest = serde_json::json!({
        "video.mp4": video_sha256,
        "frames.jsonl.zst": frames_sha256,
        "events.jsonl.zst": events_sha256,
        "meta.json": meta_sha256
    });
    let manifest_path = job.staging_dir.join("sha256.json");
    write_json(&manifest_path, &manifest)?;
    for path in [
        &video_path,
        &frames_path,
        &events_path,
        &meta_path,
        &manifest_path,
    ] {
        sync_file(path)?;
    }
    ensure!(
        !job.final_dir.exists(),
        "refusing to overwrite an immutable completed segment: {}",
        job.final_dir.display()
    );
    durable_rename(&job.staging_dir, &job.final_dir).with_context(|| {
        format!(
            "failed to atomically publish {} as {}",
            job.staging_dir.display(),
            job.final_dir.display()
        )
    })?;
    tracing::info!(
        segment_index = job.index,
        elapsed_ms = artifact_start.elapsed().as_secs_f64() * 1_000.0,
        "segment artifacts published"
    );

    let prepared = match job.prepare {
        Some((index, path)) => {
            let create_start = Instant::now();
            let writer = H264Mp4Writer::create(path, profile)?;
            tracing::info!(
                segment_index = job.index,
                prepared_segment_index = index,
                elapsed_ms = create_start.elapsed().as_secs_f64() * 1_000.0,
                "standby MP4 muxer prepared"
            );
            Some((index, writer))
        }
        None => None,
    };
    tracing::info!(
        segment_index = job.index,
        elapsed_ms = finalize_start.elapsed().as_secs_f64() * 1_000.0,
        "segment finalizer job completed"
    );
    Ok(FinalizeResult {
        summary: SegmentSummary {
            index: job.index,
            frame_count: job.timing.len() as u64,
            qpc_start,
            qpc_end,
            video_bytes,
            video_sha256,
            frames_sha256,
            events_sha256,
            meta_sha256,
            effective_encoder: job.effective_encoder,
            probe,
        },
        prepared,
    })
}

fn write_jsonl_zstd<T: Serialize>(path: &Path, rows: &[T]) -> anyhow::Result<()> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut encoder = zstd::Encoder::new(BufWriter::new(file), 3)?;
    for row in rows {
        serde_json::to_writer(&mut encoder, row)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()?.flush()?;
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn sync_file(path: &Path) -> anyhow::Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to reopen {} for durable sync", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to durable-sync {}", path.display()))
}

fn durable_rename(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source_wide.as_ptr()),
            PCWSTR::from_raw(destination_wide.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .context("MoveFileExW(MOVEFILE_WRITE_THROUGH) failed")
}

pub fn write_report(root: &Path, report: &LabReport) -> anyhow::Result<PathBuf> {
    let run_dir = root.join(&report.run_id);
    let json_path = run_dir.join("report.json");
    write_json(&json_path, report)?;
    let markdown_path = run_dir.join("report.md");
    let mut markdown = BufWriter::new(File::create(&markdown_path)?);
    writeln!(markdown, "# DataMiner P0 Native Capture Lab")?;
    writeln!(markdown)?;
    writeln!(
        markdown,
        "- Run result: {}",
        if report.passed { "PASS" } else { "FAIL" }
    )?;
    writeln!(markdown, "- Validation mode: {:?}", report.validation_mode)?;
    writeln!(
        markdown,
        "- P0 acceptance: {}",
        if !report.p0_acceptance_evaluated {
            "NOT EVALUATED"
        } else if report.p0_acceptance_passed {
            "PASS"
        } else {
            "FAIL"
        }
    )?;
    for failure in &report.acceptance_failures {
        writeln!(markdown, "  - Acceptance failure: {failure}")?;
    }
    writeln!(markdown, "- NON-TRAINABLE: true")?;
    writeln!(markdown, "- Production certified: false")?;
    writeln!(markdown, "- Adapter: {}", report.adapter.description)?;
    writeln!(
        markdown,
        "- Windows build: {}.{}.{}",
        report.windows.major, report.windows.minor, report.windows.build
    )?;
    writeln!(
        markdown,
        "- Profile: {}x{} @ {}/{} fps",
        report.profile.width, report.profile.height, report.profile.fps_num, report.profile.fps_den
    )?;
    writeln!(markdown, "- Segments: {}", report.segments.len())?;
    writeln!(
        markdown,
        "- Capture queue drops: {}",
        report.capture_queue_drops
    )?;
    writeln!(
        markdown,
        "- Encoder surfaces in flight: max {} / pool {}",
        report.max_encoder_surfaces_in_flight, report.nv12_surface_pool
    )?;
    writeln!(
        markdown,
        "- Callback: {}; CPU readback calls: {}",
        report.contract.callback_operation, report.contract.callback_cpu_readback_calls
    )?;
    writeln!(
        markdown,
        "- Encoder: {}; hardware-only: {}",
        report.contract.encoder, report.contract.encoder_hardware_only
    )?;
    writeln!(
        markdown,
        "- Explicit dedicated threads: {} / 5",
        report.contract.explicit_dedicated_threads
    )?;
    if let Some(audio) = &report.audio_calibration {
        writeln!(markdown, "- Audio loopback lab state: {:?}", audio.state)?;
        writeln!(
            markdown,
            "- Audio packets / frames / queue overflows: {} / {} / {}",
            audio.health.packets, audio.health.frames, audio.health.queue_overflows
        )?;
        writeln!(
            markdown,
            "- Completed render flashes / video markers / audio bursts: {} / {} / {}; single-run timing: {}; docs/56 hard gate: NOT EVALUATED",
            audio.render_flash_qpc_ticks.len(),
            audio.video_markers.len(),
            audio.audio_bursts.len(),
            if audio.single_run_timing_passed {
                "PASS"
            } else {
                "FAIL"
            }
        )?;
        if let Some(offset) = audio.constant_offset_us {
            writeln!(
                markdown,
                "- Audio constant offset: {:.3} us; residual p95/max: {:.3} / {:.3} us",
                offset, audio.residual_absolute_error_us.p95, audio.residual_absolute_error_us.max
            )?;
        }
        for limitation in &audio.limitations {
            writeln!(markdown, "  - Audio limitation: {limitation}")?;
        }
    }
    writeln!(
        markdown,
        "- Callback latency p95: {:.3} us",
        report.callback_latency_us.p95
    )?;
    writeln!(
        markdown,
        "- GPU copy completion p95: {:.3} us",
        report.gpu_copy_completion_latency_us.p95
    )?;
    writeln!(
        markdown,
        "- GPU/encoder submit latency p95: {:.3} us",
        report.pipeline_submit_latency_us.p95
    )?;
    writeln!(
        markdown,
        "- Stage p95 (GPU/progress/surface-wait/write): {:.3} / {:.3} / {:.3} / {:.3} us",
        report.gpu_conversion_latency_us.p95,
        report.encoder_progress_latency_us.p95,
        report.encoder_surface_wait_latency_us.p95,
        report.encoder_write_latency_us.p95
    )?;
    writeln!(
        markdown,
        "- Process CPU: {:.2}% of total capacity",
        report.process_cpu_percent_total_capacity
    )?;
    writeln!(
        markdown,
        "- Synthetic game baseline FPS: {:.2}; 1% low: {:.2}",
        report.game_performance.baseline.average_fps,
        report.game_performance.baseline.one_percent_low_fps
    )?;
    writeln!(
        markdown,
        "- Synthetic game capture FPS: {:.2}; 1% low: {:.2}",
        report.game_performance.capture.average_fps,
        report.game_performance.capture.one_percent_low_fps
    )?;
    writeln!(
        markdown,
        "- Synthetic game FPS drop: average {:.3}%; 1% low {:.3}%",
        report.game_performance.average_fps_drop_percent,
        report.game_performance.one_percent_low_drop_percent
    )?;
    writeln!(
        markdown,
        "- GPU: {} avg {:.2}% / max {}%; encoder avg {:.2}% / max {}%",
        report.gpu.device_name,
        report.gpu.gpu_average_percent,
        report.gpu.gpu_max_percent,
        report.gpu.encoder_average_percent,
        report.gpu.encoder_max_percent
    )?;
    markdown.flush()?;
    Ok(json_path)
}

pub fn game_fps(ticks: &[i64], frequency: i64) -> GameFpsReport {
    let intervals = ticks
        .windows(2)
        .filter_map(|pair| {
            let delta = pair[1] - pair[0];
            (delta > 0).then_some(delta as f64 / frequency as f64)
        })
        .collect::<Vec<_>>();
    if intervals.is_empty() {
        return GameFpsReport::default();
    }
    let average = intervals.iter().sum::<f64>() / intervals.len() as f64;
    let mut slowest = intervals.clone();
    slowest.sort_by(f64::total_cmp);
    slowest.reverse();
    let count = (slowest.len() as f64 * 0.01).ceil().max(1.0) as usize;
    let slow_average = slowest[..count].iter().sum::<f64>() / count as f64;
    GameFpsReport {
        frames: ticks.len(),
        average_fps: 1.0 / average,
        one_percent_low_fps: 1.0 / slow_average,
    }
}

pub fn game_performance(
    snapshot: &RenderTelemetrySnapshot,
    frequency: i64,
) -> GamePerformanceReport {
    let baseline = game_fps(&snapshot.baseline_qpc_ticks, frequency);
    let capture = game_fps(&snapshot.capture_qpc_ticks, frequency);
    let average_fps_drop_percent = percent_drop(baseline.average_fps, capture.average_fps);
    let one_percent_low_drop_percent =
        percent_drop(baseline.one_percent_low_fps, capture.one_percent_low_fps);
    let enough_samples = baseline.frames >= 30 && capture.frames >= 30;
    GamePerformanceReport {
        baseline,
        capture,
        average_fps_drop_percent,
        one_percent_low_drop_percent,
        passed: enough_samples
            && average_fps_drop_percent < 2.0
            && one_percent_low_drop_percent < 3.0,
    }
}

fn percent_drop(baseline: f64, measured: f64) -> f64 {
    if baseline <= 0.0 || measured <= 0.0 {
        return 100.0;
    }
    (baseline - measured) / baseline * 100.0
}

fn distribution(mut values: Vec<f64>) -> Distribution {
    if values.is_empty() {
        return Distribution::default();
    }
    values.sort_by(f64::total_cmp);
    Distribution {
        count: values.len(),
        min: values[0],
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        p99: percentile(&values, 0.99),
        max: *values.last().unwrap_or(&0.0),
    }
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    let index = ((values.len() - 1) as f64 * quantile).round() as usize;
    values[index]
}

fn slot_qpc(anchor: i64, index: u64, frequency: i64, profile: &CaptureProfile) -> i64 {
    anchor
        + (i128::from(index) * i128::from(frequency) * i128::from(profile.fps_den)
            / i128::from(profile.fps_num)) as i64
}

fn segment_index_for_slot(
    anchor_qpc: i64,
    slot_start_qpc: i64,
    segment_duration_qpc: i64,
    segment_count: u32,
) -> anyhow::Result<u32> {
    ensure!(
        segment_duration_qpc > 0 && slot_start_qpc >= anchor_qpc,
        "invalid QPC segment timeline"
    );
    let index = u32::try_from((slot_start_qpc - anchor_qpc) / segment_duration_qpc)
        .context("segment index overflowed u32")?;
    ensure!(
        index < segment_count,
        "output slot started beyond the configured test duration"
    );
    Ok(index)
}

fn frame_period_qpc(profile: &CaptureProfile, frequency: i64) -> i64 {
    frequency * i64::from(profile.fps_den) / i64::from(profile.fps_num)
}

/// Mirrors `capture_core::recording`: waiting on the encoder pipeline is not
/// one frame of work, because the driver keeps several frames in flight (issue
/// #266). capture-lab must use the same budget as the production recorder or it
/// stops reproducing what the client actually does.
fn encoder_progress_limit_qpc(profile: &CaptureProfile, frequency: i64) -> i64 {
    frame_period_qpc(profile, frequency)
        .saturating_mul(i64::try_from(ENCODER_DRAIN_CFR_FRAMES).unwrap_or(1))
        .max(1)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn process_times() -> anyhow::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .context("GetProcessTimes failed")?;
    Ok(filetime(kernel) + filetime(user))
}

const fn filetime(value: FILETIME) -> u64 {
    (value.dwHighDateTime as u64) << 32 | value.dwLowDateTime as u64
}

#[allow(dead_code)]
fn _keep_context_in_data_plane(_: &ID3D11DeviceContext, _: &ID3D11Texture2D, _: CapturedFrame) {
    let _ = frame_pts_100ns(0, 30, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rational_profile() -> CaptureProfile {
        CaptureProfile {
            profile_id: "test".into(),
            non_trainable: true,
            capture_backend: "native_wgc_window_v1".into(),
            encoder_backend: "mf_nvidia_hardware_v1".into(),
            width: 1920,
            height: 1080,
            fps_num: 60_000,
            fps_den: 1001,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 18_000_000,
            gop_frames: 240,
            segment_seconds: 10,
            test_segments: 30,
            accepted_encoder_relaxations: None,
            audio: None,
        }
    }

    #[test]
    fn rational_slots_follow_qpc_segment_boundaries_without_drift() {
        let profile = rational_profile();
        let frequency = 10_000_000;
        let anchor = 123_456_789;
        let segment_ticks = frequency * i64::from(profile.segment_seconds);
        let mut counts = vec![0_u64; profile.test_segments as usize];

        for frame in 0..profile.expected_frames_for_test() {
            let start = slot_qpc(anchor, frame, frequency, &profile);
            let segment =
                segment_index_for_slot(anchor, start, segment_ticks, profile.test_segments)
                    .unwrap();
            counts[segment as usize] += 1;
        }

        assert_eq!(counts.iter().sum::<u64>(), 17_983);
        assert_eq!(counts[0], 600);
        assert!(counts[1..].iter().all(|count| (599..=600).contains(count)));
        let last_start = slot_qpc(
            anchor,
            profile.expected_frames_for_test() - 1,
            frequency,
            &profile,
        );
        assert!(last_start < anchor + segment_ticks * i64::from(profile.test_segments));
    }

    #[test]
    fn capture_lab_rejects_a_trainable_profile_before_capture() {
        let mut profile = rational_profile();
        profile.non_trainable = false;

        let error = validate_lab_profile(&profile).unwrap_err();

        assert!(format!("{error:#}").contains("only explicitly non-trainable profiles"));
    }

    #[test]
    fn requested_audio_probe_controls_the_top_level_result() {
        assert!(audio_calibration_passed(None));

        let mut report = AudioCalibrationReport::failed("activation failed");
        assert!(!audio_calibration_passed(Some(&report)));

        report.state = AudioCalibrationState::Ready;
        report.single_run_timing_passed = true;
        assert!(audio_calibration_passed(Some(&report)));
    }

    #[test]
    fn requested_audio_probe_also_gates_on_game_performance() {
        let mut report = AudioCalibrationReport::failed("activation failed");
        report.state = AudioCalibrationState::Ready;
        report.single_run_timing_passed = true;

        assert!(!run_surfaces_passed(true, Some(&report), false));
        assert!(run_surfaces_passed(true, Some(&report), true));
        assert!(run_surfaces_passed(true, None, false));
    }

    #[test]
    fn h1_60fps_profile_is_non_trainable_and_time_closed() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/capture-profiles-60fps-h1.json");
        let profile = CaptureProfile::load(path).unwrap();

        validate_lab_profile(&profile).unwrap();
        assert!(profile.non_trainable);
        assert_eq!((profile.fps_num, profile.fps_den), (60, 1));
        assert_eq!(profile.gop_frames, 240);
        assert_eq!(profile.expected_frames_per_segment(), 600);
        assert_eq!(profile.expected_frames_for_test(), 36_000);
        assert!(profile.accepted_encoder_relaxations.is_none());
    }
}
