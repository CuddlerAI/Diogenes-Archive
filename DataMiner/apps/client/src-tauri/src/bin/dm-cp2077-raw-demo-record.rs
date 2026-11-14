#![cfg(windows)]

use std::path::PathBuf;

use anyhow::Context as _;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
};

use dataminer_client_lib::cp2077_raw_demo::{
    RawDemoCoordinatorRequest, audit_existing_session, fixed_view_action_plan_preview,
    observe_authoring_input, observe_window, operator_action_plan_text, run,
};

fn usage() -> &'static str {
    "usage: dm-cp2077-raw-demo-record --pid <Cyberpunk2077 PID> --launch-id <32 lowercase hex> --session-dir <absolute raw demo session> [--contract issue735_local|pr405_local --duration-seconds 64|30] [--manual-stop] [--autonomous-input] [--selected-present-holstered (even-CFR diagnostic)] [--cadence-trace <new absolute path outside session; diagnostic build only>]; preview: --print-action-plan; offline: --audit-session <completed session> --output <new report outside session>; read-only window/CPU: --observe-window --pid <PID> --duration-seconds <10..120> --output <new absolute report>"
}

fn value(args: &[String], name: &str) -> anyhow::Result<String> {
    let index = args
        .iter()
        .position(|argument| argument == name)
        .ok_or_else(|| anyhow::anyhow!("missing {name}; {}", usage()))?;
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} requires a value; {}", usage()))
}

fn main() -> anyhow::Result<()> {
    initialize_physical_pixel_coordinates()?;
    // The coordinated post-capture audit materializes bounded 900-row RGB/pose evidence and
    // several serde values. Windows' default executable main-thread stack is too small for that
    // deterministic audit and previously overflowed only after an otherwise successful capture.
    // Run the unchanged coordinator on an explicitly bounded worker stack so failures remain
    // ordinary fail-closed Results rather than process exceptions.
    std::thread::Builder::new()
        .name("dm-cp2077-raw-demo-main".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(real_main)?
        .join()
        .map_err(|_| anyhow::anyhow!("CP2077 raw-demo coordinator worker panicked"))?
}

fn initialize_physical_pixel_coordinates() -> anyhow::Result<()> {
    // This standalone helper does not initialize Tauri's window runtime. Establish
    // physical-pixel coordinates before spawning the recorder/monitor threads, or
    // Windows can virtualize GetClientRect and reject a native 1080p game at >100% DPI.
    let physical = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
    if !unsafe { AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), physical) }.as_bool()
    {
        unsafe { SetProcessDpiAwarenessContext(physical) }
            .context("raw demo could not establish per-monitor physical-pixel coordinates")?;
    }
    anyhow::ensure!(
        unsafe { AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), physical) }.as_bool(),
        "raw demo thread does not use per-monitor physical-pixel coordinates"
    );
    Ok(())
}

fn take_cadence_trace_path(args: &mut Vec<String>) -> anyhow::Result<Option<PathBuf>> {
    let positions = args
        .iter()
        .enumerate()
        .filter_map(|(i, a)| (a == "--cadence-trace").then_some(i))
        .collect::<Vec<_>>();
    anyhow::ensure!(positions.len() <= 1, "duplicate --cadence-trace");
    let Some(&index) = positions.first() else {
        return Ok(None);
    };
    anyhow::ensure!(
        cfg!(all(feature = "cp2077-diagnostics", debug_assertions)),
        "--cadence-trace requires cp2077-diagnostics and debug assertions; this binary cannot trace"
    );
    let path = args
        .get(index + 1)
        .filter(|s| !s.starts_with("--"))
        .ok_or_else(|| anyhow::anyhow!("--cadence-trace requires a path"))?;
    let path = PathBuf::from(path);
    anyhow::ensure!(
        path.is_absolute(),
        "--cadence-trace requires an absolute path"
    );
    args.drain(index..index + 2);
    Ok(Some(path))
}

fn real_main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("--observe-authoring-input") {
        anyhow::ensure!(
            args.len() == 9
                && args[1] == "--pid"
                && args[3] == "--launch-id"
                && args[5] == "--duration-seconds"
                && args[7] == "--output-dir",
            "usage: --observe-authoring-input --pid <PID> --launch-id <32 lowercase hex> --duration-seconds <10..60 maximum> --output-dir <new absolute directory>"
        );
        observe_authoring_input(
            args[2].parse()?,
            &args[4],
            args[6].parse()?,
            &PathBuf::from(&args[8]),
        )?;
        println!("Non-trainable authoring input witness saved: {}", args[8]);
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("--observe-window") {
        anyhow::ensure!(
            args.len() == 7
                && args[1] == "--pid"
                && args[3] == "--duration-seconds"
                && args[5] == "--output",
            "{}",
            usage()
        );
        observe_window(args[2].parse()?, args[4].parse()?, &PathBuf::from(&args[6]))?;
        println!("Read-only window/CPU observations saved: {}", args[6]);
        return Ok(());
    }
    if args == ["--print-action-plan"] {
        println!(
            "{}",
            serde_json::to_string_pretty(&fixed_view_action_plan_preview())?
        );
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("--audit-session") {
        anyhow::ensure!(args.len() == 4 && args[2] == "--output", "{}", usage());
        audit_existing_session(&PathBuf::from(&args[1]), &PathBuf::from(&args[3]))?;
        println!(
            "Offline diagnostic saved: {} (original capture result unchanged)",
            args[3]
        );
        return Ok(());
    }
    let cadence_trace_path = take_cadence_trace_path(&mut args)?;
    let selected_count = args
        .iter()
        .filter(|a| a.as_str() == "--selected-present-holstered")
        .count();
    anyhow::ensure!(
        selected_count <= 1,
        "duplicate --selected-present-holstered"
    );
    let selected_present = selected_count == 1;
    args.retain(|a| a != "--selected-present-holstered");
    anyhow::ensure!(
        !selected_present
            || (cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
                && cadence_trace_path.is_none()),
        "--selected-present-holstered requires a diagnostic build and cannot combine with --cadence-trace"
    );
    let autonomous_count = args
        .iter()
        .filter(|argument| argument.as_str() == "--autonomous-input")
        .count();
    anyhow::ensure!(autonomous_count <= 1, "{}", usage());
    let autonomous_input = autonomous_count == 1;
    args.retain(|argument| argument != "--autonomous-input");
    let manual_stop_count = args
        .iter()
        .filter(|argument| argument.as_str() == "--manual-stop")
        .count();
    anyhow::ensure!(manual_stop_count <= 1, "{}", usage());
    let manual_stop = manual_stop_count == 1;
    anyhow::ensure!(
        !selected_present || !autonomous_input,
        "--selected-present-holstered requires holstered weapons and operator input"
    );
    args.retain(|argument| argument != "--manual-stop");
    anyhow::ensure!(args.len() % 2 == 0, "{}", usage());
    let flags = args
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| pair[0].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let base = std::collections::BTreeSet::from(["--pid", "--launch-id", "--session-dir"]);
    let pr405 = std::collections::BTreeSet::from([
        "--pid",
        "--launch-id",
        "--session-dir",
        "--contract",
        "--duration-seconds",
    ]);
    anyhow::ensure!(flags == base || flags == pr405, "{}", usage());
    let pid = value(&args, "--pid")?
        .parse::<u32>()
        .map_err(|error| anyhow::anyhow!("--pid is invalid: {error}"))?;
    let process_launch_id = value(&args, "--launch-id")?;
    let session_dir = PathBuf::from(value(&args, "--session-dir")?);
    let (capture_contract, duration_ms) = if args.iter().any(|arg| arg == "--contract")
        || args.iter().any(|arg| arg == "--duration-seconds")
    {
        let contract = value(&args, "--contract")?;
        let seconds = value(&args, "--duration-seconds")?
            .parse::<u64>()
            .map_err(|error| anyhow::anyhow!("--duration-seconds is invalid: {error}"))?;
        (contract, seconds.saturating_mul(1_000))
    } else {
        (
            depth_lab::ISSUE735_LOCAL_CONTRACT.into(),
            depth_lab::ISSUE735_LOCAL_DURATION_MS,
        )
    };
    anyhow::ensure!(matches!(args.len(), 6 | 10), "{}", usage());
    anyhow::ensure!(
        !manual_stop
            || (capture_contract == depth_lab::ISSUE735_LOCAL_CONTRACT
                && duration_ms == depth_lab::ISSUE735_LOCAL_DURATION_MS
                && !autonomous_input),
        "--manual-stop requires the operator-driven issue735_local/64s contract"
    );
    anyhow::ensure!(
        !autonomous_input
            || matches!(
                (capture_contract.as_str(), duration_ms),
                (
                    depth_lab::PR405_LOCAL_CONTRACT,
                    depth_lab::PR405_LOCAL_DURATION_MS
                ) | (
                    depth_lab::ISSUE735_LOCAL_CONTRACT,
                    depth_lab::ISSUE735_LOCAL_DURATION_MS
                )
            ),
        "--autonomous-input requires an exact supported contract/duration pair"
    );

    eprintln!(
        "{}",
        operator_action_plan_text(&capture_contract, duration_ms)?
    );
    eprintln!(
        "{}",
        if autonomous_input {
            "Authorized autonomous mode will emit only W/S SendInput transitions at exact QPC cues. It records a separate provenance ledger and does not claim hardware Raw Input; same-launch pose, render-camera, depth, RGB pairing, and Loop QC remain mandatory."
        } else {
            "The recorder emits timestamped cues. Follow each cue exactly; successful completion still requires Raw Input, same-launch pose, render-camera, depth, RGB pairing, and Loop QC."
        }
    );

    let request = RawDemoCoordinatorRequest {
        pid,
        process_launch_id,
        session_dir,
        capture_contract,
        duration_ms,
        manual_stop,
        autonomous_input,
        settings_path: None,
    };
    let result = if selected_present {
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        {
            dataminer_client_lib::cp2077_raw_demo::run_with_selected_present(request)?
        }
        #[cfg(not(all(feature = "cp2077-diagnostics", debug_assertions)))]
        {
            unreachable!("unsupported selected-Present mode rejected during parsing")
        }
    } else if let Some(path) = cadence_trace_path {
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        {
            dataminer_client_lib::cp2077_raw_demo::run_with_cadence_trace(request, path)?
        }
        #[cfg(not(all(feature = "cp2077-diagnostics", debug_assertions)))]
        {
            let _ = path;
            unreachable!("unsupported trace rejected during argument parsing")
        }
    } else {
        run(request)?
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_flag_is_explicit_and_unsupported_builds_refuse_it() {
        let mut absent = vec!["--pid".into(), "1".into()];
        assert!(take_cadence_trace_path(&mut absent).unwrap().is_none());
        let path = std::env::temp_dir().join("cadence-test.jsonl");
        let mut args = vec![
            "--cadence-trace".into(),
            path.display().to_string(),
            "--pid".into(),
            "1".into(),
        ];
        let result = take_cadence_trace_path(&mut args);
        if cfg!(all(feature = "cp2077-diagnostics", debug_assertions)) {
            assert_eq!(result.unwrap(), Some(path));
            assert_eq!(args, ["--pid", "1"]);
        } else {
            assert!(result.is_err());
        }
        assert!(take_cadence_trace_path(&mut vec!["--cadence-trace".into()]).is_err());
        assert!(
            take_cadence_trace_path(&mut vec!["--cadence-trace".into(), "relative.jsonl".into()])
                .is_err()
        );
        assert!(
            take_cadence_trace_path(&mut vec![
                "--cadence-trace".into(),
                "--cadence-trace".into()
            ])
            .is_err()
        );
    }

    #[test]
    fn recorder_worker_inherits_physical_pixel_coordinates() {
        initialize_physical_pixel_coordinates().unwrap();
        // A repeated initialization must also accept an already-aware process.
        initialize_physical_pixel_coordinates().unwrap();
        assert!(
            std::thread::spawn(|| unsafe {
                AreDpiAwarenessContextsEqual(
                    GetThreadDpiAwarenessContext(),
                    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                )
                .as_bool()
            })
            .join()
            .unwrap()
        );
    }
}
