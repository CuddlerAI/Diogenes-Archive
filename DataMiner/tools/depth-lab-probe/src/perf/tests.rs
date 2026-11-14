// Compile the exact production implementation with fixture-owned statics. The
// parallel probe/writer tests keep using crate::perf, so their instrumentation
// cannot become samples in this fixture. Worker threads inside this fixture
// still share its atomic counters and rings, just as they do in the DLL.
#![allow(dead_code)] // The private fixture does not call every public entry point.

include!("state.rs");

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn full_v3_slow_ring_fits_the_offline_reader_budget() {
    let mut event = serde_json::to_value(SlowHookEvent {
        kind: SlowHookKind::ExecutePrepare,
        thread_id: u32::MAX,
        observed_end_qpc: i64::MAX,
        duration_ns: u64::MAX,
        alignment_uncertainty_ns: u64::MAX,
        execute_prepare: Some(crate::execute_prepare::PrepareSample::default()),
        descriptor_copy: None,
    })
    .unwrap();
    for value in event["execute_prepare"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        *value = serde_json::json!(u64::MAX);
    }
    let max_line_bytes = serde_json::to_vec(&event).unwrap().len() + 1;
    assert!(max_line_bytes * SLOW_HOOK_CAPACITY + 1024 <= 16 * 1024 * 1024);
}

fn timed_commit(seq: u64, thread_id: u32) -> FrameCommit {
    let qpc = 100 + seq as i64 * 100;
    commit_frame_with_clock(
        seq,
        qpc,
        0,
        0,
        Some(PresentCallTiming {
            epoch: diagnostic_epoch(),
            generation: 1,
            swapchain_key: 2,
            thread_id,
            ..PresentCallTiming::default()
        }),
        || qpc + 10,
    )
    .unwrap()
}

#[test]
fn serial_23_thread_rotation_dump_is_readable_end_to_end() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let epoch = diagnostic_epoch();
    let workers = (0..23)
        .map(|_| {
            let (tx, rx) = std::sync::mpsc::channel::<u64>();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                for seq in rx {
                    let qpc = 1000 + seq as i64 * 100;
                    let mut clocks = [qpc + 4, qpc + 5].into_iter();
                    let commit = commit_frame_with_clock(
                        seq,
                        qpc,
                        1,
                        1,
                        Some(PresentCallTiming {
                            epoch,
                            generation: 4,
                            swapchain_key: 5,
                            thread_id: unsafe {
                                windows::Win32::System::Threading::GetCurrentThreadId()
                            },
                            entry_qpc: qpc - 10,
                            entry_lock_acquired_qpc: qpc - 5,
                            original_begin_qpc: qpc + 1,
                            original_end_qpc: qpc + 2,
                            commit_begin_qpc: qpc + 3,
                            ..Default::default()
                        }),
                        || clocks.next().unwrap(),
                    )
                    .unwrap();
                    commit.finish(qpc + 6, qpc + 7);
                    done_tx.send(()).unwrap();
                }
            });
            (tx, done_rx, worker)
        })
        .collect::<Vec<_>>();
    for seq in 0..1068 {
        let (tx, done, _) = &workers[seq % workers.len()];
        tx.send(seq as u64).unwrap();
        done.recv().unwrap();
    }
    for (tx, _, worker) in workers {
        drop(tx);
        worker.join().unwrap();
    }
    record_slow_hook(
        epoch,
        SlowHookEvent {
            kind: SlowHookKind::ExecutePrepare,
            thread_id: 7,
            observed_end_qpc: 1080,
            duration_ns: 20_000_000,
            alignment_uncertainty_ns: 1000,
            execute_prepare: Some(crate::execute_prepare::PrepareSample {
                total_ns: 20_000_000,
                memory_read_ns: 10_000_000,
                residual_ns: 10_000_000,
                requests: 2,
                live_requests: 2,
                read_attempts: 1,
                reused_snapshots: 1,
                read_bytes: 256,
                reused_bytes: 256,
                wide_loads: 32,
                unflagged_reads: 1,
                memory_query_calls: 1,
                query_cache_inserts: 1,
                ..Default::default()
            }),
            descriptor_copy: None,
        },
    );
    let explicit = std::env::var_os("DM_PRESENT_TIMING_FIXTURE_DIR");
    let dir = explicit
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("dm-present-rotation-{}", std::process::id()))
        });
    std::fs::create_dir_all(&dir).unwrap();
    dump_rings(&dir, 1000).unwrap();
    let lines = std::fs::read_to_string(dir.join("perf-frames.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
    assert_eq!(last["present_call"]["exit"]["hook_exit_qpc"], 107707);
    let slow = std::fs::read_to_string(dir.join("perf-slow-hooks.jsonl")).unwrap();
    assert!(slow.contains("execute_prepare"));
    let rows: Vec<serde_json::Value> = slow
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["schema"], "dm-perf-slow-hooks-v5");
    assert_eq!(rows[1]["execute_prepare"]["memory_read_ns"], 10_000_000);
    assert_eq!(rows[1]["execute_prepare"]["reused_snapshots"], 1);
    assert_eq!(rows[1]["execute_prepare"]["memory_query_calls"], 1);
    assert_eq!(rows[1]["execute_prepare"]["query_cache_inserts"], 1);
    if explicit.is_none() {
        std::fs::remove_dir_all(dir).unwrap();
    }
    finish_run();
}

#[test]
fn exits_follow_exact_rows_across_23_threads_and_reverse_completion() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    std::thread::scope(|scope| {
        for worker in 0..23 {
            scope.spawn(move || {
                let pending = (0..20)
                    .map(|i| {
                        let seq = worker * 20 + i;
                        (seq, timed_commit(seq, worker as u32 + 1))
                    })
                    .collect::<Vec<_>>();
                for (seq, commit) in pending.into_iter().rev() {
                    let qpc = 100 + seq as i64 * 100;
                    commit.finish(qpc + 20, qpc + 30);
                }
            });
        }
    });
    {
        let guard = FRAME_RING.lock().unwrap();
        let ring = guard.as_ref().unwrap();
        assert_eq!(ring.committed, 460);
        for row in &ring.records[..460] {
            let exit = ring.exits[row.exit_index.unwrap()].snapshot().unwrap();
            assert_eq!(exit.commit_return_qpc, row.qpc + 20);
            assert_eq!(exit.hook_exit_qpc, row.qpc + 30);
        }
    }
    finish_run();
}

#[test]
fn reset_and_stop_do_not_alias_delayed_exits_and_last_frame_needs_no_successor() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let old_epoch = diagnostic_epoch();
    let old = timed_commit(0, 1);
    let old_slots = Arc::clone(&FRAME_RING.lock().unwrap().as_ref().unwrap().exits);
    begin_certification_window(100, 1, 0, 10).unwrap();
    assert_ne!(old_epoch, diagnostic_epoch());
    let fresh = timed_commit(1, 2); // closes window
    old.finish(120, 130);
    assert!(
        FRAME_RING.lock().unwrap().as_ref().unwrap().exits[0]
            .snapshot()
            .is_none()
    );
    fresh.finish(220, 230);
    let slots = Arc::clone(&FRAME_RING.lock().unwrap().as_ref().unwrap().exits);
    assert_eq!(slots[0].snapshot().unwrap().hook_exit_qpc, 230);
    assert!(commit_frame(2, 300, 0, 0, None).is_none());
    finish_run();
    assert_eq!(old_slots[0].snapshot().unwrap().hook_exit_qpc, 130);
    assert_eq!(slots[0].snapshot().unwrap().hook_exit_qpc, 230);
}

#[test]
fn late_finish_after_teardown_and_wrap_cannot_rewrite_another_row() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let old = timed_commit(0, 1);
    let slots = Arc::clone(&FRAME_RING.lock().unwrap().as_ref().unwrap().exits);
    {
        let mut guard = FRAME_RING.lock().unwrap();
        let ring = guard.as_mut().unwrap();
        ring.wrapped = true;
        ring.next = 0;
    }
    let wrapped = timed_commit(1000, 2);
    assert!(!wrapped.needs_exit());
    assert!(
        FRAME_RING.lock().unwrap().as_ref().unwrap().records[0]
            .exit_index
            .is_none()
    );
    finish_run();
    begin_run(true);
    let current = timed_commit(0, 3);
    old.finish(120, 130);
    assert_eq!(slots[0].snapshot().unwrap().hook_exit_qpc, 130);
    assert!(
        FRAME_RING.lock().unwrap().as_ref().unwrap().exits[0]
            .snapshot()
            .is_none()
    );
    drop(current); // abandoned/unfinished must not masquerade as zero duration
    assert!(
        FRAME_RING.lock().unwrap().as_ref().unwrap().exits[0]
            .snapshot()
            .is_none()
    );
    finish_run();
}

#[test]
fn stale_entry_epoch_cannot_acquire_a_new_endpoint_slot() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let epoch = diagnostic_epoch();
    prepare_rings();
    let commit = commit_frame_with_clock(
        1,
        100,
        0,
        0,
        Some(PresentCallTiming {
            epoch,
            ..Default::default()
        }),
        || panic!("stale diagnostic must not sample the clock"),
    )
    .unwrap();
    assert!(!commit.needs_exit());
    assert!(
        FRAME_RING.lock().unwrap().as_ref().unwrap().records[0]
            .present_call
            .is_none()
    );
    finish_run();
}

#[test]
fn slow_hook_trace_is_bounded_nonblocking_and_generation_scoped() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let epoch = diagnostic_epoch();
    let event = SlowHookEvent {
        kind: SlowHookKind::DescriptorCopy,
        thread_id: 7,
        observed_end_qpc: 1000,
        duration_ns: SLOW_HOOK_THRESHOLD_NS,
        alignment_uncertainty_ns: 100,
        execute_prepare: None,
        descriptor_copy: None,
    };
    record_slow_hook(
        epoch,
        SlowHookEvent {
            duration_ns: 1,
            ..event
        },
    );
    let dropped = SLOW_HOOK_DROPPED.load(Ordering::Relaxed);
    {
        let guard = FRAME_RING.lock().unwrap();
        record_slow_hook(epoch, event); // would deadlock if this ever used lock()
        assert!(guard.as_ref().unwrap().slow_hooks.is_empty());
    }
    assert_eq!(SLOW_HOOK_DROPPED.load(Ordering::Relaxed), dropped + 1);
    for _ in 0..SLOW_HOOK_CAPACITY + 2 {
        record_slow_hook(epoch, event);
    }
    assert_eq!(
        FRAME_RING
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .slow_hooks
            .len(),
        SLOW_HOOK_CAPACITY
    );
    assert_eq!(SLOW_HOOK_DROPPED.load(Ordering::Relaxed), dropped + 3);
    prepare_rings();
    record_slow_hook(epoch, event);
    assert!(
        FRAME_RING
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .slow_hooks
            .is_empty()
    );
    finish_run();
    record_slow_hook(epoch, event);
    assert!(FRAME_RING.lock().unwrap().is_none());
}

#[test]
fn present_call_boundaries_include_ring_wait_without_changing_certification_anchor() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let call = PresentCallTiming {
        epoch: diagnostic_epoch(),
        generation: 1,
        swapchain_key: 2,
        thread_id: 3,
        entry_qpc: 50,
        entry_lock_acquired_qpc: 90,
        original_begin_qpc: 110,
        // More than the old u32-nanosecond range: retain full QPC evidence.
        original_end_qpc: 60_000_110,
        commit_begin_qpc: 60_000_120,
        ..PresentCallTiming::default()
    };
    let mut clocks = [60_000_200, 60_000_210].into_iter();
    let commit =
        commit_frame_with_clock(9, 100, 5, 7, Some(call), || clocks.next().unwrap()).unwrap();
    commit.finish(60_000_220, 60_000_230);
    assert!(clocks.next().is_none());
    assert!(
        (std::mem::size_of::<FrameRecord>() + std::mem::size_of::<ExitSlot>())
            * FRAME_RING_CAPACITY
            + std::mem::size_of::<SlowHookEvent>() * SLOW_HOOK_CAPACITY
            <= 40 * 1024 * 1024
    );
    let dir = std::env::temp_dir().join(format!("dm-present-call-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dump_rings(&dir, 10_000_000).unwrap();
    let dumped = std::fs::read_to_string(dir.join("perf-frames.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(dumped.lines().nth(1).unwrap()).unwrap();
    assert_eq!(row["qpc"], 100);
    assert_eq!(row["present_call"]["original_end_qpc"], 60_000_110);
    assert_eq!(row["present_call"]["commit_recorded_qpc"], 60_000_210);
    assert_eq!(row["present_call"]["exit"]["commit_return_qpc"], 60_000_220);
    assert_eq!(row["present_call"]["exit"]["hook_exit_qpc"], 60_000_230);
    std::fs::remove_dir_all(&dir).unwrap();
    {
        let ring = FRAME_RING.lock().unwrap();
        let row = ring.as_ref().unwrap().records[0];
        assert_eq!(row.qpc, 100);
        assert_eq!(row.hook_pre_ns, 5);
        assert_eq!(row.hook_post_ns, 7);
        let timing = row.present_call.unwrap();
        assert_eq!(timing.original_end_qpc, 60_000_110);
        assert_eq!(timing.commit_lock_acquired_qpc, 60_000_200);
        assert_eq!(timing.commit_recorded_qpc, 60_000_210);
        assert!(timing.exit.is_none()); // merged only at dump
        let encoded = serde_json::to_value(timing).unwrap();
        assert_eq!(encoded["epoch"], diagnostic_epoch());
    }
    let mut rows = present_records(&[1_000, 1_000, 1_000], 100);
    let before = summarize_present_records(&rows, false, 100, 3_100, 3_100, 1_000).unwrap();
    for row in &mut rows {
        row.present_call = Some(call);
    }
    let after = summarize_present_records(&rows, false, 100, 3_100, 3_100, 1_000).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    finish_run();
}

#[test]
fn diagnostic_clock_is_unused_when_disabled_absent_or_window_closed() {
    let _guard = TEST_LOCK.lock().unwrap();
    finish_run();
    assert!(
        commit_frame_with_clock(1, 100, 0, 0, Some(PresentCallTiming::default()), || panic!(
            "disabled clock"
        ))
        .is_none()
    );
    begin_run(true);
    begin_certification_window(100, 1, 0, 1).unwrap();
    assert!(commit_frame_with_clock(1, 100, 0, 0, None, || panic!("absent clock")).is_some());
    assert!(commit_frame_with_clock(2, 101, 0, 0, None, || panic!("absent clock")).is_some());
    assert!(
        commit_frame_with_clock(3, 102, 0, 0, Some(PresentCallTiming::default()), || panic!(
            "closed clock"
        ))
        .is_none()
    );
    assert_eq!(FRAME_RING.lock().unwrap().as_ref().unwrap().committed, 2);
    finish_run();
}

#[test]
fn fixture_keeps_its_worker_samples_separate_from_parallel_probe_calls() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    std::thread::spawn(|| {
        // These calls represent unrelated tests of instrumented probe helpers.
        let _scope = crate::perf::scope(crate::perf::Section::Draw);
        crate::perf::count(crate::perf::Counter::Draw);
        crate::perf::mark_frame(crate::perf::FrameFlag::Capture);
        crate::perf::commit_frame(999, 999_000, 0, 0, None);
        crate::perf::record_encode(999, 999_000, 4_925_000, None);
        crate::perf::record_readback_copy(crate::perf::ReadbackCopyRecord::default());
    })
    .join()
    .unwrap();
    // Isolation is per fixture, not per thread: its real writer worker must
    // still contribute to the same ring read by the controlling test thread.
    std::thread::spawn(|| record_encode(42, 1_000_000, 34_000_000, None))
        .join()
        .unwrap();
    commit_frame(42, 1_000_000, 0, 0, None);
    let timing = encode_timing_summary().unwrap();
    assert_eq!(timing.sample_count, 1);
    assert_eq!(timing.total_committed, 1);
    assert_eq!(timing.duration_us.p50, 34_000);
    {
        let ring = FRAME_RING.lock().unwrap();
        let ring = ring.as_ref().unwrap();
        assert_eq!(ring.committed, 1);
        assert_eq!(ring.records[0].present_seq, 42);
        assert_eq!(ring.records[0].flags, 0);
        assert_eq!(ring.records[0].counts, [0; COUNTER_COUNT]);
        assert_eq!(ring.records[0].sections_ns, [0; SECTION_COUNT]);
    }
    assert_eq!(
        READBACK_COPY_RING
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .committed,
        0
    );
    finish_run();
}

#[test]
fn disabled_scope_does_not_touch_aggregates() {
    let _guard = TEST_LOCK.lock().unwrap();
    ENABLED.store(false, Ordering::Release);
    {
        let _scope = scope(Section::Draw);
    }
    assert_eq!(COUNTS[Section::Draw as usize].load(Ordering::Acquire), 0);
    assert!(take_summary().is_none());
}

#[test]
fn aggregate_scope_updates_totals_without_leaking_into_a_present() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    let before = COUNTS[Section::PoseAudit as usize].load(Ordering::Acquire);
    {
        let _scope = aggregate_scope(Section::PoseAudit);
    }
    assert_eq!(
        COUNTS[Section::PoseAudit as usize].load(Ordering::Acquire),
        before + 1
    );
    commit_frame(1, 100, 0, 0, None);
    let ring = FRAME_RING.lock().unwrap();
    assert_eq!(
        ring.as_ref().unwrap().records[0].sections_ns,
        [0; SECTION_COUNT]
    );
    drop(ring);
    finish_run();
}

#[test]
fn disabled_commit_and_marks_are_no_ops() {
    let _guard = TEST_LOCK.lock().unwrap();
    ENABLED.store(false, Ordering::Release);
    mark_frame(FrameFlag::Capture);
    commit_frame(1, 100, 5, 7, None);
    record_encode(1, 100, 42, None);
    assert_eq!(FRAME_FLAGS.load(Ordering::Acquire), 0);
    assert!(FRAME_RING.lock().unwrap().is_none());
}

#[test]
fn frame_ring_records_flags_and_sections_then_dumps() {
    let _guard = TEST_LOCK.lock().unwrap();
    ENABLED.store(true, Ordering::Release);
    prepare_rings();
    FRAME_NANOS[Section::PoolDispatch as usize].store(1_234, Ordering::Relaxed);
    mark_frame(FrameFlag::Capture);
    mark_frame(FrameFlag::Latency2);
    for _ in 0..7 {
        count(Counter::Draw);
    }
    count(Counter::Om);
    commit_frame(42, 1_000_000, 250, 750, None);
    record_encode(42, 1_000_500, 34_000_000, Some((1_000_000, 50_000)));
    record_readback_copy(ReadbackCopyRecord {
        present_seq: 41,
        start_qpc: 900_000,
        duration_ns: 400_000,
        map_ns: 2_000,
        copy_ns: 390_000,
        queue_wait_ns: 10_000,
    });
    let timing = encode_timing_summary().unwrap();
    assert_eq!(timing.sample_count, 1);
    assert_eq!(timing.total_committed, 1);
    assert_eq!(timing.over_budget_count, 1);
    assert_eq!(timing.duration_us.p99, 34_000);

    let dir = std::env::temp_dir().join(format!("dm-perf-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dump_rings(&dir, 10_000_000).unwrap();
    let frames = std::fs::read_to_string(dir.join("perf-frames.jsonl")).unwrap();
    assert!(frames.starts_with("{\"schema\":\"dm-perf-frames-v6\",\"qpf\":10000000"));
    assert!(frames.contains("\"present_call\":null"));
    assert!(
        frames.contains(
            "\"counters\":[\"draw_entries\",\"map_entries\",\"vs_entries\",\"om_entries\"]"
        )
    );
    assert!(frames.contains("\"seq\":42"));
    assert!(frames.contains("capture|latency2"));
    assert!(frames.contains("1234"));
    assert!(frames.contains("\"counts\":[7,0,0,1]"));
    assert_eq!(FRAME_FLAGS.load(Ordering::Acquire), 0);
    for count in &FRAME_COUNTS {
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
    let encodes = std::fs::read_to_string(dir.join("perf-encode.jsonl")).unwrap();
    assert!(encodes.contains("\"duration_ns\":34000000"));
    let rows: Vec<serde_json::Value> = encodes
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["schema"], "dm-perf-encode-v2");
    assert_eq!(rows[0]["duration_scope"], "exr_write");
    assert_eq!(rows[0]["preparation_scope"], "decode_verify_convention");
    assert_eq!(rows[1]["seq"], 42);
    assert_eq!(rows[1]["preparation"]["start_qpc"], 1_000_000);
    assert_eq!(rows[1]["preparation"]["duration_ns"], 50_000);
    // The UI/ACK EXR timing retains its prior scope rather than silently including preparation.
    assert_eq!(encode_timing_summary().unwrap().duration_us.p50, 34_000);
    let copies = std::fs::read_to_string(dir.join("perf-readback-copy.jsonl")).unwrap();
    let copies: Vec<serde_json::Value> = copies
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(copies[0]["schema"], "dm-perf-readback-copy-v1");
    assert_eq!(copies[0]["execution_thread"], "worker");
    assert_eq!(copies[1]["present_seq"], 41);
    assert_eq!(copies[1]["copy_ns"], 390_000);
    std::fs::remove_dir_all(&dir).ok();

    // 收尾清环，避免污染同进程其他测试。
    ENABLED.store(false, Ordering::Release);
    prepare_rings();
}

#[test]
fn frame_flags_reset_after_commit() {
    let _guard = TEST_LOCK.lock().unwrap();
    ENABLED.store(true, Ordering::Release);
    prepare_rings();
    mark_frame(FrameFlag::Drain);
    commit_frame(7, 500, 0, 0, None);
    assert_eq!(FRAME_FLAGS.load(Ordering::Acquire), 0);
    ENABLED.store(false, Ordering::Release);
    prepare_rings();
}

fn present_records(durations: &[i64], start_qpc: i64) -> Vec<FrameRecord> {
    let mut qpc = start_qpc;
    let mut records = vec![FrameRecord {
        present_seq: 1,
        qpc,
        ..FrameRecord::default()
    }];
    for (index, duration) in durations.iter().copied().enumerate() {
        qpc += duration;
        records.push(FrameRecord {
            present_seq: u64::try_from(index + 2).unwrap(),
            qpc,
            ..FrameRecord::default()
        });
    }
    records
}

fn assert_protocol_valid(summary: &depth_lab::PresentTimingSummary) {
    let flush = depth_lab::ProbeFlushSummary {
        writer_queue_depth: 0,
        last_present_seq: 1,
        cutoff_present_seq: 1,
        depth_row_count: 0,
        depth_file_count: 0,
        camera_pose_row_count: 0,
        camera_diagnostic_row_count: 0,
        player_pose_row_count: None,
        selected_present: None,
        writer_bypass: false,
        writer_bypassed_frames: 0,
        writer_timing: None,
        performance_metrics_enabled: true,
        present_timing: Some(summary.clone()),
        errors: Vec::new(),
    };
    flush.validate().unwrap();
}

#[test]
fn five_minute_certification_keeps_high_fps_present_evidence() {
    let _guard = TEST_LOCK.lock().unwrap();
    let frequency = 7_200_000;
    let ready = 1_000_000;
    for fps in [60, 120, 144, 240] {
        begin_run(true);
        begin_certification_window(ready, frequency, 5, 300).unwrap();
        for frame in 0..=fps * 310 {
            commit_frame(
                frame as u64 + 1,
                ready + frame * frequency / fps,
                0,
                0,
                None,
            );
        }
        let summary = present_timing_summary(ready + 310 * frequency, frequency)
            .unwrap()
            .unwrap();
        assert!(!summary.wrapped, "{fps} FPS lost five-minute evidence");
        assert_eq!(summary.sample_count, (fps * 300 + 1) as u64);
        assert_eq!(summary.avg_millifps, fps as u64 * 1_000);
        assert_eq!(summary.low1_millifps, fps as u64 * 1_000);
        assert_protocol_valid(&summary);
        finish_run();
    }
}

#[test]
fn completed_window_survives_delayed_stop_without_disabling_other_metrics() {
    let _guard = TEST_LOCK.lock().unwrap();
    let frequency = 240_000;
    let ready = 1_000_000;
    begin_run(true);
    begin_certification_window(ready, frequency, 5, 300).unwrap();
    for frame in 0..=240 * 600 {
        mark_frame(FrameFlag::Capture);
        count(Counter::Draw);
        commit_frame(frame + 1, ready + frame as i64 * 1_000, 1, 2, None);
    }
    let summary = present_timing_summary(ready + 600 * frequency, frequency)
        .unwrap()
        .unwrap();
    assert!(!summary.wrapped);
    assert_eq!(summary.sample_count, 72_001);
    assert_eq!(summary.avg_millifps, 240_000);
    assert_protocol_valid(&summary);
    assert_eq!(
        FRAME_RING.lock().unwrap().as_ref().unwrap().committed,
        73_201
    );
    assert_eq!(FRAME_FLAGS.load(Ordering::Acquire), 0);
    assert_eq!(
        FRAME_COUNTS[Counter::Draw as usize].load(Ordering::Acquire),
        0
    );
    assert!(enabled());
    record_encode(144_001, ready + 600 * frequency, 1_000, None);
    record_readback_copy(ReadbackCopyRecord {
        present_seq: 144_001,
        ..Default::default()
    });
    assert_eq!(encode_timing_summary().unwrap().total_committed, 1);
    assert_eq!(
        READBACK_COPY_RING
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .committed,
        1
    );
    finish_run();
}

#[test]
fn window_closes_on_exact_or_first_late_present_and_resets_for_next_run() {
    let _guard = TEST_LOCK.lock().unwrap();
    for closing_qpc in [306_000, 306_007] {
        begin_run(true);
        begin_certification_window(1_000, 1_000, 5, 300).unwrap();
        for (index, qpc) in [6_000, 305_999, closing_qpc, 306_100]
            .into_iter()
            .enumerate()
        {
            commit_frame(index as u64 + 1, qpc, 0, 0, None);
        }
        let summary = present_timing_summary(307_000, 1_000).unwrap().unwrap();
        assert_eq!(summary.sample_count, 3);
        assert_eq!(summary.qpc_end, closing_qpc);
        assert!(FRAME_RING.lock().unwrap().as_ref().unwrap().window_closed);
    }
    begin_run(true);
    assert_eq!(CERTIFICATION_WINDOW_END_QPC.load(Ordering::Acquire), 0);
    commit_frame(10, 500_000, 0, 0, None);
    commit_frame(11, 501_000, 0, 0, None);
    assert_eq!(FRAME_RING.lock().unwrap().as_ref().unwrap().committed, 2);
    begin_certification_window(500_000, 1_000, 5, 300).unwrap();
    commit_frame(12, 505_000, 0, 0, None);
    commit_frame(13, 506_000, 0, 0, None);
    let early = present_timing_summary(506_000, 1_000).unwrap().unwrap();
    assert_eq!(early.sample_count, 2);
    assert!(!FRAME_RING.lock().unwrap().as_ref().unwrap().window_closed);
    finish_run();
}

#[test]
fn real_frame_capacity_overflow_stays_ineligible_after_window_closes() {
    let _guard = TEST_LOCK.lock().unwrap();
    begin_run(true);
    begin_certification_window(1_000, 1_000_000, 0, 300).unwrap();
    for frame in 0..FRAME_RING_CAPACITY + 10 {
        commit_frame(frame as u64 + 1, 1_000 + frame as i64, 0, 0, None);
    }
    commit_frame(FRAME_RING_CAPACITY as u64 + 11, 300_001_000, 0, 0, None);
    let summary = present_timing_summary(301_001_000, 1_000_000)
        .unwrap()
        .unwrap();
    assert!(summary.wrapped);
    assert!(FRAME_RING.lock().unwrap().as_ref().unwrap().window_closed);
    finish_run();
}

#[test]
fn present_summary_reports_exact_sixty_fps() {
    let records = present_records(&vec![1_000; 300], 10_000);
    let summary =
        summarize_present_records(&records, false, 10_000, 310_000, 400_000, 60_000).unwrap();
    assert_eq!(summary.sample_count, 301);
    assert_eq!(summary.avg_millifps, 60_000);
    assert_eq!(summary.low1_millifps, 60_000);
    assert!(!summary.wrapped);
    assert_protocol_valid(&summary);
}

#[test]
fn present_summary_low1_uses_slowest_frame_times() {
    let mut durations = vec![1_000; 100];
    durations[42] = 4_000;
    let records = present_records(&durations, 10_000);
    let summary =
        summarize_present_records(&records, false, 10_000, 113_000, 200_000, 60_000).unwrap();
    assert_eq!(summary.low1_millifps, 15_000);
    assert!(summary.low1_millifps < summary.avg_millifps);
}

#[test]
fn present_summary_fractionally_weights_the_one_percent_boundary() {
    let mut durations = vec![1_000; 250];
    durations[0] = 4_000;
    durations[1] = 4_000;
    durations[2] = 2_000;
    let records = present_records(&durations, 10_000);
    let summary =
        summarize_present_records(&records, false, 10_000, 267_000, 400_000, 60_000).unwrap();
    assert_eq!(summary.low1_millifps, 16_666);
}

#[test]
fn present_summary_filters_settle_and_post_cutoff_records() {
    let records = present_records(&[1_000, 1_000, 1_000, 1_000, 1_000], 10_000);
    let summary =
        summarize_present_records(&records, false, 12_000, 14_000, 15_000, 60_000).unwrap();
    assert_eq!(summary.sample_count, 3);
    assert_eq!(summary.qpc_start, 12_000);
    assert_eq!(summary.qpc_end, 14_000);
    assert_eq!(summary.measurement_cutoff_qpc, 14_000);
    assert_eq!(summary.stop_cutoff_qpc, 15_000);
    assert_eq!(summary.avg_millifps, 60_000);
}

#[test]
fn present_summary_uses_only_the_first_tail_present_to_close_the_boundary_interval() {
    let records = present_records(&[1_000, 1_000, 1_000, 2_000, 1_000, 1_000], 10_000);
    let summary =
        summarize_present_records(&records, false, 12_000, 14_000, 18_000, 60_000).unwrap();
    assert_eq!(summary.qpc_start, 12_000);
    assert_eq!(summary.qpc_end, 15_000);
    assert_eq!(summary.sample_count, 3);
    assert_eq!(summary.slowest_one_percent[0].start_qpc, 13_000);
    assert_eq!(summary.slowest_one_percent[0].end_qpc, 15_000);
    assert_protocol_valid(&summary);
}

#[test]
fn present_summary_rejects_a_measurement_that_never_closes_before_stop() {
    let records = present_records(&[1_000, 1_000, 1_000], 10_000);
    let error =
        summarize_present_records(&records, false, 10_000, 14_000, 18_000, 60_000).unwrap_err();
    assert!(error.contains("did not reach scheduled measurement cutoff"));
}

#[test]
fn present_summary_keeps_early_pause_as_a_short_measurement() {
    let records = present_records(&[1_000, 1_000, 1_000, 1_000, 1_000], 10_000);
    let summary =
        summarize_present_records(&records, false, 20_000, 30_000, 14_000, 60_000).unwrap();
    assert_eq!(summary.sample_count, 5);
    assert_eq!(summary.qpc_start, 10_000);
    assert_eq!(summary.qpc_end, 14_000);
    assert_eq!(summary.avg_millifps, 60_000);
}
