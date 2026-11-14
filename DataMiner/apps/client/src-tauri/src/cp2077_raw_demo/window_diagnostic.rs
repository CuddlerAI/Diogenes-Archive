//! Bounded, read-only target-window/CPU observations for correlating raw short-run QPCs.
//! No activation, input, capture backend, arm/stop request or performance certification.
use super::*;

#[derive(Debug, Serialize)]
struct Sample {
    qpc: i64,
    target_is_foreground: bool,
    target_revalidated: bool,
    kernel_time_100ns: u64,
    user_time_100ns: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    schema: &'static str,
    process_id: u32,
    process_creation_time_100ns: u64,
    executable_sha256: String,
    qpc_frequency: i64,
    requested_seconds: u64,
    sample_interval_ms: u64,
    samples: Vec<Sample>,
    observation_error: Option<String>,
    foreground_state_is_sampled: bool,
    measures_present_rate: bool,
    window_activation_used: bool,
    input_capture_used: bool,
    non_trainable: bool,
    recording_ready: bool,
}

struct ProcessHandle(HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn process_times(process: HANDLE) -> anyhow::Result<(u64, u64, u64)> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }?;
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Ok((ticks(created), ticks(kernel), ticks(user)))
}

pub fn observe_window(pid: u32, seconds: u64, output: &Path) -> anyhow::Result<()> {
    ensure!(
        pid > 0 && (10..=120).contains(&seconds),
        "window observation requires a PID and 10..120 seconds"
    );
    ensure!(
        output.is_absolute() && !output.exists(),
        "window observation output must be new and absolute"
    );
    let target = bind_exact_launched_window(pid)?;
    validate_recording_target(&target, pid)?;
    let tracker = WindowTracker::new()?;
    let process =
        ProcessHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?);
    let (creation, _, _) = process_times(process.0)?;
    let clock = QpcClock::new()?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    let start = Instant::now();
    let mut report = Report {
        schema: "dm-cp2077-window-cpu-observation/v1",
        process_id: pid,
        process_creation_time_100ns: creation,
        executable_sha256: CP2077_EXE_SHA256.into(),
        qpc_frequency: clock.frequency(),
        requested_seconds: seconds,
        sample_interval_ms: 100,
        samples: Vec::with_capacity(seconds as usize * 10 + 1),
        observation_error: None,
        foreground_state_is_sampled: true,
        measures_present_rate: false,
        window_activation_used: false,
        input_capture_used: false,
        non_trainable: true,
        recording_ready: false,
    };
    eprintln!("Read-only window/CPU observation started; no activation or capture requests.");
    while start.elapsed() < Duration::from_secs(seconds) && report.samples.len() < 1_201 {
        let sample = (|| -> anyhow::Result<Sample> {
            let valid = tracker.revalidate(&target);
            let (_, kernel, user) = process_times(process.0)?;
            let root = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) };
            let sample = Sample {
                qpc: clock.now()?,
                target_is_foreground: root.0 as usize == target.root_owner_value,
                target_revalidated: valid.is_ok(),
                kernel_time_100ns: kernel,
                user_time_100ns: user,
            };
            if let Err(error) = valid {
                report.observation_error = Some(format!("{error:#}"));
            }
            Ok(sample)
        })();
        match sample {
            Ok(sample) => report.samples.push(sample),
            Err(error) => report.observation_error = Some(format!("{error:#}")),
        }
        if report.observation_error.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    ensure!(
        report.observation_error.is_none(),
        "window observation ended early: {:?}",
        report.observation_error
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_times_are_read_only_and_monotonic() {
        let process = ProcessHandle(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, std::process::id()) }
                .unwrap(),
        );
        let a = process_times(process.0).unwrap();
        let b = process_times(process.0).unwrap();
        assert!(a.0 > 0);
        assert_eq!(a.0, b.0);
        assert!(b.1 >= a.1 && b.2 >= a.2);
    }
}
