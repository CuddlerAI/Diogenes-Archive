mod audio_calibration;
mod nvml;
mod runner;
mod synthetic;
mod viewscale_scene;

const SYNTHETIC_SOURCE_RATE_MULTIPLIER: u32 = 4;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut profile_path = std::path::PathBuf::from("config/capture-profiles.json");
    let mut output_root = std::path::PathBuf::from("tools/capture-lab/out");
    let mut smoke = false;
    let mut encoder_preflight = false;
    let mut segment_override = None;
    let mut seconds_override = None;
    let mut window_only = false;
    let mut audio_loopback = false;
    let mut view_scale_counts: Option<f64> = None;
    let mut view_scale_hfov = 90.0_f64;
    let mut simulate_acceleration: Option<f64> = None;
    let mut simulate_smoothing: Option<f64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                profile_path = args
                    .next()
                    .map(Into::into)
                    .ok_or_else(|| anyhow::anyhow!("--profile requires a path"))?
            }
            "--out" => {
                output_root = args
                    .next()
                    .map(Into::into)
                    .ok_or_else(|| anyhow::anyhow!("--out requires a path"))?
            }
            "--smoke" => smoke = true,
            "--encoder-preflight" => encoder_preflight = true,
            "--window-only" => window_only = true,
            "--audio-loopback" => audio_loopback = true,
            "--segments" => {
                segment_override = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--segments requires a number"))?
                        .parse::<u32>()?,
                )
            }
            "--seconds" => {
                seconds_override = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--seconds requires a number"))?
                        .parse::<u32>()?,
                )
            }
            "--status" => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&capture_core::P0Status::current())?
                );
                return Ok(());
            }
            "--view-scale" => {
                view_scale_counts = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--view-scale requires counts_per_360"))?
                        .parse::<f64>()?,
                )
            }
            "--view-scale-hfov" => {
                view_scale_hfov = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--view-scale-hfov requires degrees"))?
                    .parse::<f64>()?
            }
            "--simulate-acceleration" => {
                simulate_acceleration = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--simulate-acceleration requires a gain"))?
                        .parse::<f64>()?,
                )
            }
            "--simulate-smoothing" => {
                simulate_smoothing = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--simulate-smoothing requires an alpha"))?
                        .parse::<f64>()?,
                )
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    // --view-scale: known-truth calibration scene (docs/46 C3). Only meaningful
    // in the manual/wizard --window-only mode; the capture pipeline path keeps
    // the plain P0 stress scene. Degradation switches require the scene.
    let view_scale = match view_scale_counts {
        Some(counts) => {
            anyhow::ensure!(
                window_only,
                "--view-scale requires --window-only (calibration scene is for manual/wizard use)"
            );
            anyhow::ensure!(
                counts.is_finite() && counts > 0.0,
                "--view-scale counts_per_360 must be positive"
            );
            anyhow::ensure!(
                (10.0..=170.0).contains(&view_scale_hfov),
                "--view-scale-hfov must be within [10, 170] degrees"
            );
            let accel_gain = simulate_acceleration.unwrap_or(0.0);
            anyhow::ensure!(
                accel_gain >= 0.0 && accel_gain.is_finite(),
                "--simulate-acceleration gain must be finite and >= 0"
            );
            if let Some(alpha) = simulate_smoothing {
                anyhow::ensure!(
                    alpha > 0.0 && alpha <= 1.0,
                    "--simulate-smoothing alpha must be in (0, 1]"
                );
            }
            Some(viewscale_scene::ViewScaleConfig {
                counts_per_360: counts,
                accel_gain,
                smoothing_alpha: simulate_smoothing,
                hfov_deg: view_scale_hfov,
            })
        }
        None => {
            anyhow::ensure!(
                simulate_acceleration.is_none() && simulate_smoothing.is_none(),
                "--simulate-* switches require --view-scale"
            );
            None
        }
    };

    let validation_mode = if smoke {
        runner::ValidationMode::Smoke
    } else if segment_override.is_some() || seconds_override.is_some() {
        runner::ValidationMode::Custom
    } else {
        runner::ValidationMode::P0Endurance
    };
    let baseline_seconds = if validation_mode == runner::ValidationMode::P0Endurance {
        30
    } else {
        5
    };

    let mut profile = capture_core::profile::CaptureProfile::load(&profile_path)?;
    if smoke {
        profile.segment_seconds = 2;
        profile.test_segments = 1;
    }
    if let Some(segments) = segment_override {
        profile.test_segments = segments;
    }
    if let Some(seconds) = seconds_override {
        profile.segment_seconds = seconds;
    }
    runner::validate_lab_profile(&profile)?;
    if encoder_preflight {
        anyhow::ensure!(
            !window_only && !audio_loopback && view_scale.is_none() && !smoke,
            "--encoder-preflight only probes GPU conversion and hardware encoding; no window/audio/scene"
        );
        let output = output_root.join("encoder-preflight.json");
        std::fs::create_dir_all(&output_root)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?;
        let evidence = capture_core::recording::probe_hardware_path(
            &profile,
            output_root.join(".preflight-scratch"),
        )?;
        serde_json::to_writer_pretty(&mut file, &evidence)?;
        file.sync_all()?;
        println!("{}", output.display());
        return Ok(());
    }
    if audio_loopback {
        anyhow::ensure!(
            !window_only,
            "--audio-loopback runs with the capture pipeline and cannot be combined with --window-only"
        );
        let total_seconds = u64::from(profile.segment_seconds)
            .checked_mul(u64::from(profile.test_segments))
            .ok_or_else(|| anyhow::anyhow!("audio calibration duration overflowed"))?;
        anyhow::ensure!(
            (2..=60).contains(&total_seconds),
            "--audio-loopback is a short lab probe and requires a total duration within 2..=60 seconds"
        );
    }
    // The non-trainable P0 source refreshes at 4x the capture cadence so the
    // lab measures collector-added duplicates rather than phase races between
    // two independent clocks at exactly the same nominal rate. Output remains
    // the unmodified profile CFR and every superseded source is reported. Game
    // performance telemetry samples every fourth repaint at profile cadence.
    let window = synthetic::SyntheticWindow::create(
        profile.width,
        profile.height,
        profile.fps_num,
        profile.fps_den,
        SYNTHETIC_SOURCE_RATE_MULTIPLIER,
        view_scale.clone(),
    )?;
    if window_only {
        window.allow_normal_z_order()?;
        println!(
            "synthetic_game=ready class=DataMinerCaptureLabSyntheticWindow non_trainable=true"
        );
        if let Some(config) = &view_scale {
            // Machine-readable truth line for wizard E2E scripts.
            println!(
                "view_scale=ready counts_per_360={} hfov_deg={} accel_gain={} smoothing_alpha={}",
                config.counts_per_360,
                config.hfov_deg,
                config.accel_gain,
                config
                    .smoothing_alpha
                    .map_or_else(|| "off".to_string(), |alpha| alpha.to_string()),
            );
        }
        window.run_message_loop()?;
        return Ok(());
    }
    let hwnd_value = window.hwnd.0 as usize;
    let telemetry = std::sync::Arc::clone(&window.telemetry);
    let telemetry_for_worker = std::sync::Arc::clone(&window.telemetry);
    let clock = capture_core::qpc::QpcClock::new()?;
    let output_for_thread = output_root.clone();
    let worker = std::thread::Builder::new()
        .name("dm-capture-pipeline".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(baseline_seconds));
            telemetry_for_worker.begin_preflight();
            runner::run(
                hwnd_value,
                runner::RunConfig {
                    profile,
                    output_root: output_for_thread,
                    validation_mode,
                    render_telemetry: telemetry_for_worker,
                    synthetic_source_rate_multiplier: SYNTHETIC_SOURCE_RATE_MULTIPLIER,
                    audio_loopback,
                },
            )
        })?;
    window.run_message_loop()?;
    let mut report = worker
        .join()
        .map_err(|_| anyhow::anyhow!("capture pipeline panicked"))??;
    report.apply_game_performance(runner::game_performance(
        &telemetry.snapshot(),
        clock.frequency(),
    ));
    let report_path = runner::write_report(&output_root, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    println!("report={}", report_path.display());
    if report.p0_acceptance_evaluated {
        anyhow::ensure!(
            report.p0_acceptance_passed,
            "P0 endurance completed but acceptance gates failed; see {}",
            report_path.display()
        );
    } else if audio_loopback {
        anyhow::ensure!(
            report.passed,
            "audio-loopback probe completed but its timing, health, or performance gates failed; see {}",
            report_path.display()
        );
    }
    Ok(())
}
