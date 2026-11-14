use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
use data_demo_converter::inspect_sync_samples;
use mp4::Mp4Reader;
use serde::{Deserialize, Serialize};

use crate::hash::sha256_file;
use crate::model::FramePlanRow;
use crate::plan::PreparedRun;
use crate::spec::{LoadedSpec, RationalRate};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaIdentity {
    pub implementation: String,
    pub version: String,
    pub binary_sha256: String,
    pub settings_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VideoProbe {
    pub sample_count: u32,
    pub width: u16,
    pub height: u16,
    pub fps_num: u32,
    pub fps_den: u32,
    pub track_timescale: u32,
    pub duration_ticks: u64,
    pub duration_us: u64,
    pub pts_start: u64,
    pub pts_end: u64,
    pub gop_frames: u32,
    pub sync_samples: Vec<u32>,
    pub first_frame_idr: bool,
}

pub trait MediaBackend {
    fn identity(&self, spec: &LoadedSpec) -> anyhow::Result<MediaIdentity>;

    fn normalize_run(
        &self,
        spec: &LoadedSpec,
        run: &PreparedRun,
        plan: &[FramePlanRow],
        source_width: u32,
        source_height: u32,
        output: &Path,
    ) -> anyhow::Result<VideoProbe>;
}

#[derive(Clone, Debug)]
pub struct FfmpegBackend {
    executable: PathBuf,
    binary_sha256: String,
    version_line: String,
}

impl FfmpegBackend {
    pub fn pinned(executable: impl AsRef<Path>, expected_sha256: &str) -> anyhow::Result<Self> {
        let executable = executable.as_ref();
        ensure!(
            executable.is_file(),
            "pinned FFmpeg executable {} is missing",
            executable.display()
        );
        ensure!(
            expected_sha256.starts_with("sha256:")
                && expected_sha256.len() == 71
                && expected_sha256[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
            "expected FFmpeg hash must be sha256:<64 lowercase hex>"
        );
        let binary_sha256 = sha256_file(executable)?;
        ensure!(
            binary_sha256 == expected_sha256,
            "FFmpeg binary SHA-256 does not match the caller-pinned value"
        );
        let output = Command::new(executable)
            .arg("-version")
            .output()
            .with_context(|| format!("failed to execute {}", executable.display()))?;
        ensure!(output.status.success(), "pinned FFmpeg -version failed");
        let stdout =
            String::from_utf8(output.stdout).context("FFmpeg version output is not UTF-8")?;
        let version_line = stdout
            .lines()
            .next()
            .context("FFmpeg version output is empty")?
            .trim()
            .to_owned();
        ensure!(
            version_line.starts_with("ffmpeg version "),
            "executable does not identify itself as FFmpeg"
        );
        Ok(Self {
            executable: executable.to_path_buf(),
            binary_sha256,
            version_line,
        })
    }
}

impl MediaBackend for FfmpegBackend {
    fn identity(&self, spec: &LoadedSpec) -> anyhow::Result<MediaIdentity> {
        ensure!(
            spec.spec.encoder.binary_policy == "sha256_exact_v1",
            "FFmpeg backend requires the SHA-256 exact binary policy"
        );
        Ok(MediaIdentity {
            implementation: spec.spec.encoder.implementation.clone(),
            version: self.version_line.clone(),
            binary_sha256: self.binary_sha256.clone(),
            settings_id: spec.spec.encoder.settings_id.clone(),
        })
    }

    fn normalize_run(
        &self,
        spec: &LoadedSpec,
        run: &PreparedRun,
        plan: &[FramePlanRow],
        source_width: u32,
        source_height: u32,
        output: &Path,
    ) -> anyhow::Result<VideoProbe> {
        ensure!(
            plan.len() == run.plan_len,
            "media worker plan range mismatch"
        );
        ensure!(
            run.source_videos.len() == run.source_video_frames.len(),
            "source video/frame-count list mismatch"
        );
        let factor = u64::from(spec.spec.downsample_factor());
        for (index, row) in plan.iter().enumerate() {
            ensure!(row.run_id == run.run_id, "media plan crosses run boundary");
            ensure!(
                row.source_frame == (index as u64 + 1) * factor - 1,
                "media worker received a frame-plan that is not the frozen integer selector"
            );
        }
        for (path, expected_frames) in run.source_videos.iter().zip(&run.source_video_frames) {
            validate_source_video(
                path,
                *expected_frames,
                source_width,
                source_height,
                spec.spec.source,
            )
            .with_context(|| format!("invalid source video {}", path.display()))?;
        }
        let parent = output
            .parent()
            .context("normalized video has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let concat_path = parent.join("source.ffconcat");
        write_concat(&concat_path, &run.source_videos)?;
        let select = format!(
            "select=not(mod(n+1\\,{})),setpts=N*{}/({}*TB)",
            spec.spec.downsample_factor(),
            spec.spec.target.fps_den,
            spec.spec.target.fps_num
        );
        let status = Command::new(&self.executable)
            .args([
                "-hide_banner",
                "-nostdin",
                "-y",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ])
            .arg(&concat_path)
            .args(["-an", "-vf"])
            .arg(select)
            .args([
                "-frames:v",
                &plan.len().to_string(),
                "-fps_mode",
                "cfr",
                "-r",
                &format!("{}/{}", spec.spec.target.fps_num, spec.spec.target.fps_den),
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-preset",
                "medium",
                "-crf",
                "18",
                "-threads",
                "1",
                "-g",
                &spec.spec.gop_frames().to_string(),
                "-keyint_min",
                &spec.spec.gop_frames().to_string(),
                "-sc_threshold",
                "0",
                "-bf",
                "0",
                "-video_track_timescale",
                "90000",
                "-map_metadata",
                "-1",
                "-map_chapters",
                "-1",
                "-fflags",
                "+bitexact",
                "-flags:v",
                "+bitexact",
                "-movflags",
                "+faststart",
            ])
            .arg(output)
            .output()
            .with_context(|| format!("failed to start pinned FFmpeg for run {}", run.run_id));
        let _ = fs::remove_file(&concat_path);
        let status = status?;
        ensure!(
            status.status.success(),
            "pinned FFmpeg failed for run {}: {}",
            run.run_id,
            String::from_utf8_lossy(&status.stderr).trim()
        );
        let probe = probe_video(output, spec.spec.target_rate(), spec.spec.gop_frames())?;
        ensure!(
            probe.sample_count as usize == plan.len(),
            "normalized video frame count does not match frame-plan"
        );
        ensure!(
            u32::from(probe.width) == source_width && u32::from(probe.height) == source_height,
            "preserve-source geometry policy was not honored"
        );
        Ok(probe)
    }
}

pub(crate) fn validate_source_video(
    path: &Path,
    expected_frames: u32,
    expected_width: u32,
    expected_height: u32,
    expected_rate: RationalRate,
) -> anyhow::Result<()> {
    let sync = inspect_sync_samples(path)
        .with_context(|| format!("invalid source video {}", path.display()))?;
    ensure!(
        sync.sample_count == expected_frames,
        "source video sample count does not match source timing rows"
    );
    ensure!(
        u32::from(sync.width) == expected_width && u32::from(sync.height) == expected_height,
        "source video dimensions do not match the request"
    );

    // `inspect_sync_samples` closes the H.264/IDR/no-B-frame contract, but it
    // deliberately has no expected native rate. Re-read the sample table here
    // so a request cannot merely declare 60/1 while pointing at 59.94, 30fps,
    // or VFR media with the same frame count and geometry.
    let file = File::open(path)
        .with_context(|| format!("failed to open source video {}", path.display()))?;
    let size = file.metadata()?.len();
    let mut reader = Mp4Reader::read_header(BufReader::new(file), size)
        .with_context(|| format!("invalid source MP4 {}", path.display()))?;
    ensure!(
        reader.tracks().len() == 1,
        "source MP4 must contain exactly one track"
    );
    let track_id = *reader
        .tracks()
        .keys()
        .next()
        .context("source MP4 has no track")?;
    let track = reader.tracks().get(&track_id).unwrap();
    ensure!(
        track.timescale() == sync.track_timescale,
        "source MP4 probe timescale disagreement"
    );
    let mut expected_start = 0_u64;
    for sample_id in 1..=sync.sample_count {
        let sample = reader
            .read_sample(track_id, sample_id)?
            .with_context(|| format!("source MP4 sample {sample_id} is missing"))?;
        ensure!(
            sample.start_time == expected_start,
            "source MP4 PTS has a gap"
        );
        ensure!(
            u64::from(sample.duration) * u64::from(expected_rate.fps_num)
                == u64::from(sync.track_timescale) * u64::from(expected_rate.fps_den),
            "source MP4 sample duration does not match exact source fps {}/{}",
            expected_rate.fps_num,
            expected_rate.fps_den
        );
        expected_start = expected_start
            .checked_add(u64::from(sample.duration))
            .context("source MP4 duration overflow")?;
    }
    Ok(())
}

pub fn probe_video(
    path: &Path,
    expected_rate: RationalRate,
    gop_frames: u32,
) -> anyhow::Result<VideoProbe> {
    ensure!(gop_frames > 0, "expected GOP must be positive");
    let sync = inspect_sync_samples(path)?;
    let file = File::open(path)
        .with_context(|| format!("failed to open normalized video {}", path.display()))?;
    let size = file.metadata()?.len();
    let mut reader = Mp4Reader::read_header(BufReader::new(file), size)?;
    let track_id = *reader
        .tracks()
        .keys()
        .next()
        .context("normalized MP4 has no track")?;
    ensure!(
        reader.tracks().len() == 1,
        "normalized MP4 must have one video track"
    );
    let track = reader.tracks().get(&track_id).unwrap();
    ensure!(
        track.timescale() == sync.track_timescale,
        "normalized MP4 probe timescale disagreement"
    );
    let mut expected_start = 0_u64;
    for sample_id in 1..=sync.sample_count {
        let sample = reader
            .read_sample(track_id, sample_id)?
            .with_context(|| format!("normalized MP4 sample {sample_id} is missing"))?;
        ensure!(
            sample.start_time == expected_start,
            "normalized MP4 PTS has a gap"
        );
        ensure!(
            sample.rendering_offset == 0,
            "normalized MP4 contains B-frame reordering"
        );
        ensure!(
            u64::from(sample.duration) * u64::from(expected_rate.fps_num)
                == u64::from(sync.track_timescale) * u64::from(expected_rate.fps_den),
            "normalized MP4 sample duration does not match exact target fps"
        );
        expected_start = expected_start
            .checked_add(u64::from(sample.duration))
            .context("normalized MP4 duration overflow")?;
    }
    let expected_sync = (0..sync.sample_count)
        .step_by(gop_frames as usize)
        .collect::<Vec<_>>();
    ensure!(
        sync.sync_samples == expected_sync,
        "normalized MP4 GOP/IDR schedule does not match the frozen encoder policy"
    );
    let duration_us = expected_start
        .checked_mul(1_000_000)
        .context("normalized MP4 microsecond duration overflow")?
        / u64::from(sync.track_timescale);
    Ok(VideoProbe {
        sample_count: sync.sample_count,
        width: sync.width,
        height: sync.height,
        fps_num: expected_rate.fps_num,
        fps_den: expected_rate.fps_den,
        track_timescale: sync.track_timescale,
        duration_ticks: expected_start,
        duration_us,
        pts_start: 0,
        pts_end: expected_start,
        gop_frames,
        sync_samples: sync.sync_samples,
        first_frame_idr: true,
    })
}

fn write_concat(path: &Path, videos: &[PathBuf]) -> anyhow::Result<()> {
    ensure!(!videos.is_empty(), "run has no source videos");
    let mut file = File::create(path)
        .with_context(|| format!("failed to create FFmpeg concat file {}", path.display()))?;
    file.write_all(b"ffconcat version 1.0\n")?;
    for video in videos {
        let path = video
            .canonicalize()
            .with_context(|| format!("failed to resolve source video {}", video.display()))?
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''");
        writeln!(file, "file '{path}'")?;
    }
    file.flush()?;
    Ok(())
}
