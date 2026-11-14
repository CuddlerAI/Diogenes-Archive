use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, ensure};
use capture_core::audio::ProcessLoopbackCapture;
use capture_core::platform::WindowsVersion;
use capture_core::qpc::QpcClock;

fn main() -> anyhow::Result<()> {
    let mut pid = None;
    let mut seconds = 60_u64;
    let mut output = PathBuf::from("tools/capture-lab/out/process-audio-probe.json");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pid" => {
                pid = Some(
                    args.next()
                        .context("--pid requires a process id")?
                        .parse::<u32>()?,
                );
            }
            "--seconds" => {
                seconds = args
                    .next()
                    .context("--seconds requires a duration")?
                    .parse::<u64>()?;
            }
            "--out" => output = args.next().context("--out requires a path")?.into(),
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    let pid = pid.context("--pid is required")?;
    ensure!(
        (2..=3_600).contains(&seconds),
        "--seconds must be within 2..=3600"
    );

    let windows = WindowsVersion::current()?;
    let clock = QpcClock::new()?;
    let capture = ProcessLoopbackCapture::start(pid, clock)?;
    let started_qpc = clock.now()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut packets = Vec::new();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        capture.drain_packets(&mut packets);
    }
    let health = capture.finish(&mut packets)?;
    let ended_qpc = clock.now()?;
    let passed = health.packets > 0
        && health.frames > 0
        && health.discontinuities == 0
        && health.timestamp_errors == 0
        && health.queue_overflows == 0
        && health.nonzero_device_positions == 0
        && health.terminal_error.is_none();
    let report = serde_json::json!({
        "schema_version": "dm-process-audio-probe-1",
        "passed": passed,
        "target_process_id": pid,
        "duration_seconds": seconds,
        "windows": windows,
        "qpc_frequency": clock.frequency(),
        "started_qpc": started_qpc,
        "ended_qpc": ended_qpc,
        "drained_packet_count": packets.len(),
        "health": health,
        "operator_actions": [
            "Record fullscreen/window-mode and Alt+Tab actions beside this file; the probe cannot infer them."
        ],
    });
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&report)?;
    std::fs::write(&output, [&bytes[..], b"\n"].concat())
        .with_context(|| format!("failed to write {}", output.display()))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(
        passed,
        "process audio health gate failed; see {}",
        output.display()
    );
    Ok(())
}
