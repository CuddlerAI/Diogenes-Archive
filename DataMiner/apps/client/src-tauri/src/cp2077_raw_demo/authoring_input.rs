//! Diagnostic action witness for one camera-authoring window. Reuses the target-bound
//! Raw Input monitor/writer; never captures RGB, arms depth, activates a window or injects input.
use super::*;
use capture_core::input::{EventSegmentSeal, RawEventWriter, wait_input_ack};

const STOP_SCHEMA: &str = "dm-cp2077-authoring-input-stop/v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    schema: String,
    process_launch_id: String,
}

fn stop_requested(path: &Path, launch_id: &str) -> anyhow::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file()
            && metadata.file_attributes() & 0x400 == 0
            && (1..=4_096).contains(&metadata.len()),
        "input stop must be a bounded regular file"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(4_097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4_096, "input stop grew beyond its bound");
    let request: StopRequest = serde_json::from_slice(&bytes)?;
    ensure!(
        request.schema == STOP_SCHEMA && request.process_launch_id == launch_id,
        "input stop schema or launch identity mismatch"
    );
    Ok(true)
}

fn publish(path: &Path, value: &Value) -> anyhow::Result<()> {
    ensure!(!path.exists(), "input witness output already exists");
    let prepared = path.with_extension("prepared.json");
    write_json_create_new(&prepared, value)?;
    fs::rename(prepared, path)?;
    Ok(())
}

fn sealed_evidence(seal: &EventSegmentSeal) -> anyhow::Result<Value> {
    Ok(json!({
        "path": seal.path, "sha256": sha256_file(&seal.path)?,
        "bytes": fs::metadata(&seal.path)?.len(), "event_count": seal.event_count,
        "qpc_start": seal.qpc_start, "qpc_end": seal.qpc_end,
        "queue_overflows": seal.input_queue_overflows,
        "boundary_reorder_drops": seal.input_boundary_reorder_drops,
        "faulted": seal.input_faulted, "fault_reason": seal.input_fault_reason,
    }))
}

pub fn observe_authoring_input(
    pid: u32,
    launch_id: &str,
    seconds: u64,
    output: &Path,
) -> anyhow::Result<()> {
    ensure!(
        cfg!(all(feature = "cp2077-diagnostics", debug_assertions)),
        "authoring input requires a diagnostic build with debug assertions"
    );
    validate_launch_id(launch_id)?;
    ensure!(
        pid > 0 && (10..=60).contains(&seconds),
        "input witness requires a PID and 10..60 seconds"
    );
    ensure!(
        output.is_absolute(),
        "input witness output must be absolute"
    );
    fs::create_dir(output).context("input witness output must be new; never reuse an attempt")?;
    let mut report = json!({
        "schema": "dm-cp2077-authoring-input-witness/v1",
        "process_id": pid, "process_launch_id": launch_id,
        "executable_sha256": CP2077_EXE_SHA256, "maximum_seconds": seconds,
        "non_trainable": true, "recording_ready": false,
        "operator_actions_verified": false, "window_activation_used": false,
        "input_injection_used": false, "rgb_capture_used": false,
        "completed": false, "events": null, "error": null
    });
    let result = capture(pid, launch_id, seconds, output, &mut report);
    report["completed"] = json!(result.is_ok());
    report["error"] = json!(result.as_ref().err().map(|error| format!("{error:#}")));
    publish(&output.join("result.json"), &report)?;
    result
}

fn capture(
    pid: u32,
    launch_id: &str,
    seconds: u64,
    output: &Path,
    report: &mut Value,
) -> anyhow::Result<()> {
    let target = bind_exact_launched_window(pid)?;
    validate_recording_target(&target, pid)?;
    let tracker = WindowTracker::new()?;
    tracker.revalidate(&target)?;
    ensure!(
        unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) }.0 as usize
            == target.root_owner_value,
        "input witness target is not foreground"
    );
    let clock = QpcClock::new()?;
    let cutoff = Arc::new(AtomicI64::new(i64::MAX));
    let (stop_tx, stop_rx) = bounded::<RecordingStopSignal>(8);
    let (input_tx, input_rx) = bounded::<RawInputRecord>(RAW_INPUT_QUEUE_CAPACITY);
    let monitor = TargetWindowMonitor::start(
        &target,
        format!("authoring:{launch_id}"),
        Arc::clone(&cutoff),
        stop_tx,
        input_tx,
        Arc::new(AtomicBool::new(false)),
    )?;
    let control = monitor.input_control();
    let status = monitor.input_status();
    let writer = RawEventWriter::start(
        input_rx,
        control.clone(),
        status.clone(),
        output.join("events"),
        format!("authoring:{launch_id}"),
        clock.frequency(),
    )?;
    ensure!(
        monitor.prepare_commit()?.activate_and_open(),
        "input witness gate faulted"
    );
    // Same existing neutral-input barrier, without a video/first-frame dependency.
    // Give Raw Input a small observation interval, inside its three-second arm bound.
    std::thread::sleep(Duration::from_millis(100));
    let start = clock.now()?;
    let deadline = start
        .checked_add(
            clock
                .frequency()
                .checked_mul(seconds as i64)
                .context("input duration overflow")?,
        )
        .context("input deadline overflow")?;
    wait_input_ack(&control.request_arm(start, deadline)?, "authoring arm")?;
    report["qpc_frequency"] = json!(clock.frequency());
    report["qpc_start"] = json!(start);
    // Every failure after arm finishes the partial witness rather than deleting its events.
    let observed = (|| -> anyhow::Result<i64> {
        publish(
            &output.join("ready.json"),
            &json!({
                "schema": "dm-cp2077-authoring-input-ready/v1", "process_id": pid,
                "process_launch_id": launch_id, "qpc_start": start,
                "qpc_deadline": deadline, "qpc_frequency": clock.frequency(), "non_trainable": true
            }),
        )?;
        loop {
            writer.check()?;
            ensure!(
                !status.faulted(),
                "input witness fault: {:?}",
                status.snapshot().fault_reason
            );
            if let Ok(signal) = stop_rx.try_recv() {
                anyhow::bail!(
                    "input witness target invalidated: {:?} at {}",
                    signal.reason,
                    signal.cutoff_qpc
                );
            }
            tracker.revalidate(&target)?;
            ensure!(
                unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) }.0 as usize
                    == target.root_owner_value,
                "input witness foreground lost"
            );
            let now = clock.now()?;
            ensure!(
                now < deadline,
                "input witness timed out without a controller stop"
            );
            if stop_requested(&output.join("stop-request.json"), launch_id)? {
                return Ok(now);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    let end = observed
        .as_ref()
        .copied()
        .unwrap_or_else(|_| {
            clock
                .now()
                .unwrap_or(deadline)
                .min(cutoff.load(Ordering::Acquire))
        })
        .clamp(start + 1, deadline);
    let finish = control
        .request_finish(0, end)
        .and_then(|ack| wait_input_ack(&ack, "authoring finish"));
    // A failed Finish must not enter an unbounded join while the writer still awaits commands.
    let seal = match finish {
        Ok(seal) => {
            writer.stop()?;
            seal
        }
        Err(error) => {
            drop(writer);
            return Err(error.context(format!("observation: {observed:?}")));
        }
    };
    drop(monitor);
    report["events"] = sealed_evidence(&seal)?;
    observed?;
    ensure!(
        !seal.input_faulted
            && seal.input_queue_overflows == 0
            && seal.input_boundary_reorder_drops == 0,
        "input witness is incomplete; preserve this attempt"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_core::input::{RawInputControl, RawInputKind, RawInputPacket, RawInputStatus};
    const LAUNCH: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn partial_witness_preserves_original_key_times_and_exposes_input_fault() {
        let dir = tempfile::tempdir().unwrap();
        let (records, receiver) = bounded(16);
        let (commands, _command_rx) = bounded(4);
        let status = RawInputStatus::default();
        let control = RawInputControl::new(0, commands, records.clone(), status.clone());
        let writer = RawEventWriter::start(
            receiver,
            control,
            status.clone(),
            dir.path().to_owned(),
            format!("authoring:{LAUNCH}"),
            1_000,
        )
        .unwrap();
        let (arm_tx, arm_rx) = bounded(1);
        records
            .send(RawInputRecord::Arm {
                anchor_qpc: 100,
                first_segment_end_qpc: 1_000,
                neutral_at_anchor: true,
                arming_start_qpc: None,
                response: arm_tx,
            })
            .unwrap();
        wait_input_ack(&arm_rx, "synthetic authoring arm").unwrap();
        for (qpc, down) in [(123, true), (456, false)] {
            records
                .send(RawInputRecord::Event(RawInputPacket {
                    qpc,
                    device: "synthetic".into(),
                    control: false,
                    kind: RawInputKind::Key {
                        down,
                        vk: 87,
                        scan_code: 17,
                        flags: 0,
                        repeat: false,
                    },
                }))
                .unwrap();
        }
        status.mark_fault(false, "synthetic foreground loss");
        let (finish_tx, finish_rx) = bounded(1);
        records
            .send(RawInputRecord::Finish {
                segment_index: 0,
                effective_end_qpc: 600,
                response: finish_tx,
            })
            .unwrap();
        let seal = wait_input_ack(&finish_rx, "synthetic partial finish").unwrap();
        writer.stop().unwrap();
        let evidence = sealed_evidence(&seal).unwrap();
        assert_eq!(evidence["event_count"], 2);
        assert_eq!(evidence["faulted"], true);
        assert_eq!(evidence["qpc_end"], 600);
        let events: Vec<RawInputEvent> = fs::read_to_string(&seal.path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events.iter().map(|event| event.qpc).collect::<Vec<_>>(),
            [123, 456]
        );
        assert_eq!(events[0].event_type, "key_down");
        assert_eq!(events[1].event_type, "key_up");
        assert_eq!(evidence["sha256"], sha256_file(&seal.path).unwrap());
    }

    #[test]
    fn stop_rejects_partial_foreign_unknown_and_oversize_requests() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("stop.json");
        assert!(!stop_requested(&stop, LAUNCH).unwrap());
        for value in [
            String::new(),
            "{".into(),
            " ".repeat(4_097),
            json!({"schema": STOP_SCHEMA, "process_launch_id": "other"}).to_string(),
            json!({"schema": "other", "process_launch_id": LAUNCH}).to_string(),
            json!({"schema": STOP_SCHEMA, "process_launch_id": LAUNCH, "extra": true}).to_string(),
        ] {
            fs::write(&stop, value).unwrap();
            assert!(stop_requested(&stop, LAUNCH).is_err());
        }
        fs::remove_file(&stop).unwrap();
        publish(
            &stop,
            &json!({"schema": STOP_SCHEMA, "process_launch_id": LAUNCH}),
        )
        .unwrap();
        assert!(stop_requested(&stop, LAUNCH).unwrap());
        assert!(publish(&stop, &json!({})).is_err());
        assert!(stop_requested(dir.path(), LAUNCH).is_err());
    }

    #[test]
    fn output_attempt_cannot_be_reused_or_started_with_invalid_arguments() {
        let dir = tempfile::tempdir().unwrap();
        assert!(observe_authoring_input(1, LAUNCH, 60, dir.path()).is_err());
        let new = dir.path().join("new");
        for (pid, launch, seconds) in [
            (0, LAUNCH, 60),
            (1, "bad", 60),
            (1, LAUNCH, 61),
            (1, LAUNCH, 0),
        ] {
            assert!(observe_authoring_input(pid, launch, seconds, &new).is_err());
            assert!(!new.exists());
        }
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
