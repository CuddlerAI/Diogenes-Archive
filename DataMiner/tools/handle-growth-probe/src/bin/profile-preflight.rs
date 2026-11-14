//! Runs the ordinary hardware preflight with Fallout4's video settings.
//! No game, window capture, input recording, or profile mutation.
use anyhow::{Context, ensure};
use capture_core::{profile::CaptureProfile, recording::probe_hardware_path};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let output = std::path::PathBuf::from(args.next().context("expected a new output directory")?);
    ensure!(args.next().is_none(), "expected only an output directory");
    std::fs::create_dir(&output).context("output must not already exist")?;
    let profile = CaptureProfile {
        profile_id: "fallout4-video-settings-optimus-validation".into(),
        non_trainable: true,
        capture_backend: "native_wgc_window_v1".into(),
        encoder_backend: "mf_nvidia_hardware_v1".into(),
        width: 1920,
        height: 1080,
        fps_num: 30,
        fps_den: 1,
        bitrate_bps: 16_000_000,
        max_bitrate_bps: 24_000_000,
        gop_frames: 120,
        segment_seconds: 60,
        test_segments: 1,
        accepted_encoder_relaxations: None,
        audio: None,
    };
    let facts = probe_hardware_path(&profile, output.join(".preflight-scratch"))?;
    let json = serde_json::to_string_pretty(&facts)?;
    std::fs::write(output.join("hardware-preflight.json"), &json)?;
    println!("{json}");
    Ok(())
}
