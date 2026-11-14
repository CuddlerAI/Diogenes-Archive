//! Deterministically schedule a callback after an empty wake, before sealing.
//! The function under test is called by record_inner, not a copied recorder.
use super::*;

#[test]
fn cadence_trial_keeps_the_default_and_accepts_only_the_measured_30fps_trial() {
    assert_eq!(cadence_seal_delay_ms(None, 30, 1).unwrap(), 4);
    assert_eq!(cadence_seal_delay_ms(Some(4), 60, 1).unwrap(), 4);
    assert_eq!(cadence_seal_delay_ms(Some(16), 30, 1).unwrap(), 16);
    assert_eq!(cadence_seal_delay_ms(Some(16), 60, 2).unwrap(), 16);
    for (value, num, den) in [
        (0, 30, 1),
        (8, 30, 1),
        (17, 30, 1),
        (255, 30, 1),
        (16, 60, 1),
        (16, 0, 0),
    ] {
        assert!(cadence_seal_delay_ms(Some(value), num, den).is_err());
    }
}

#[test]
fn cadence_override_rejects_mixed_or_unmeasured_budgets() {
    assert_eq!(
        cadence_seal_timing(Some(4), false, 30, 1).unwrap(),
        CfrSealTiming::NORMAL
    );
    let grace = cadence_seal_timing(None, true, 30, 1).unwrap();
    assert_eq!((grace.normal_delay_ms, grace.empty_slot_delay_ms), (4, 16));
    assert!(cadence_seal_timing(Some(16), true, 30, 1).is_err());
    assert!(cadence_seal_timing(None, true, 60, 1).is_err());
    assert!(cadence_seal_timing(None, true, 0, 0).is_err());
}

#[test]
fn regular_entry_uses_grace_only_at_exactly_30_fps() {
    for (num, den) in [(30, 1), (60, 2), (90, 3)] {
        let regular = CfrSealTiming::for_fps(num, den);
        let (decision, at, _) = replay_wait_policy(regular, &[(110, source(2, 99))], 0);
        assert!(!decision.duplicate);
        assert_eq!(at, 110);
        assert_eq!(regular, cadence_seal_timing(None, false, num, den).unwrap());
    }
    for (num, den) in [(60, 1), (120, 2), (30_000, 1_001), (24, 1), (0, 0)] {
        let regular = CfrSealTiming::for_fps(num, den);
        let (decision, at, _) = replay_wait_policy(regular, &[(110, source(2, 99))], 0);
        assert!(decision.duplicate);
        assert_eq!(at, 104);
        assert_eq!(regular, cadence_seal_timing(None, false, num, den).unwrap());
    }
}

#[test]
fn regular_policy_restarts_the_empty_budget_for_each_slot() {
    let policy = CfrSealTiming::for_fps(30, 1);
    // A candidate in one slot must not disable grace in the next empty slot.
    let (_, at, _) = replay_wait_policy(policy, &[(101, source(2, 99))], 0);
    assert_eq!(at, 104);
    let (next, at, _) = replay_wait_policy(policy, &[(112, source(3, 99))], 0);
    assert!(!next.duplicate);
    assert_eq!(at, 112);
}

/// One virtual slot using the recorder's real deadline policy, channel poll,
/// final queue check and selector. GPU/watermark/Arm are assumed validated here;
/// no game, OS scheduling or hardware performance is simulated.
fn replay_wait_policy(
    timing: CfrSealTiming,
    deliveries: &[(i64, CapturedFrame)],
    wake: i64,
) -> (crate::capture::CfrSlotDecision, i64, Option<CapturedFrame>) {
    const END: i64 = 100;
    const HZ: i64 = 1_000;
    assert!(deliveries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    let (sender, frames) = bounded(deliveries.len().max(1));
    let mut selector = CfrSlotSelector::default();
    let mut now = wake;
    let mut next = 0;
    let mut future = None;
    loop {
        while next < deliveries.len() && deliveries[next].0 <= now {
            sender.try_send(deliveries[next].1).unwrap();
            next += 1;
        }
        let deadline = timing.deadline(END, HZ, selector.has_candidate());
        let frame = match poll_cfr_ingress(&frames, now, deadline).unwrap() {
            CfrIngressPoll::Frame(frame) => frame,
            CfrIngressPoll::DeadlineReached => {
                assert!(finish_empty_cfr_wake(&frames, || Ok(())).unwrap().is_none());
                break;
            }
            CfrIngressPoll::Wait => {
                let next_time = deliveries.get(next).map_or(deadline, |d| d.0.min(deadline));
                assert!(next_time > now, "bounded virtual wait must progress");
                now = next_time;
                continue;
            }
        };
        if let CfrObservation::Future(frame) = selector.observe(frame, END) {
            future = Some(frame);
            break;
        }
    }
    let decision = selector.seal(Some((&source(1, -1)).into())).unwrap();
    (decision, now, future)
}

#[test]
fn empty_slot_grace_does_not_delay_a_slot_that_already_has_a_candidate() {
    let deliveries = [(101, source(2, 98)), (102, source(3, 99))];
    let grace = cadence_seal_timing(None, true, 30, 1).unwrap();
    let fixed16 = cadence_seal_timing(Some(16), false, 30, 1).unwrap();
    for (policy, expected_seal) in [(CfrSealTiming::NORMAL, 104), (grace, 104), (fixed16, 116)] {
        let (decision, sealed, _) = replay_wait_policy(policy, &deliveries, 0);
        assert_eq!(sealed, expected_seal);
        assert!(!decision.duplicate);
        assert_eq!(decision.identity.source_seq, 3);
        assert_eq!(decision.source_superseded, 1);
    }
}

#[test]
fn empty_slot_grace_seals_as_soon_as_a_late_candidate_arrives() {
    let deliveries = [(110, source(2, 99))];
    let grace = cadence_seal_timing(None, true, 30, 1).unwrap();
    let (normal, at, _) = replay_wait_policy(CfrSealTiming::NORMAL, &deliveries, 0);
    assert!(normal.duplicate);
    assert_eq!(at, 104);
    let (recovered, at, _) = replay_wait_policy(grace, &deliveries, 0);
    assert!(!recovered.duplicate);
    assert_eq!(recovered.identity.source_seq, 2);
    assert_eq!(
        at, 110,
        "a recovered candidate must not wait out the 16ms grace"
    );
}

#[test]
fn empty_slot_grace_remains_bounded_and_never_borrows_a_future_source() {
    let grace = cadence_seal_timing(None, true, 30, 1).unwrap();
    for deliveries in [vec![], vec![(117, source(2, 99))]] {
        let (decision, at, future) = replay_wait_policy(grace, &deliveries, 0);
        assert!(decision.duplicate);
        assert_eq!(at, 116);
        assert!(future.is_none());
    }
    let (decision, at, future) = replay_wait_policy(grace, &[(110, source(2, 100))], 0);
    assert!(decision.duplicate);
    assert_eq!(decision.identity.source_seq, 1);
    assert_eq!(at, 110);
    assert_eq!(future.unwrap().source_seq, 2);
}

#[test]
fn empty_slot_grace_preserves_queue_first_recovery_after_a_late_consumer_wake() {
    let grace = cadence_seal_timing(None, true, 30, 1).unwrap();
    let deliveries = [
        (110, source(2, 98)),
        (111, source(3, 99)),
        (112, source(4, 100)),
    ];
    let (decision, at, future) = replay_wait_policy(grace, &deliveries, 120);
    assert!(!decision.duplicate);
    assert_eq!(decision.identity.source_seq, 3);
    assert_eq!(decision.source_superseded, 1);
    assert_eq!(at, 120);
    assert_eq!(future.unwrap().source_seq, 4);
}

fn source(seq: u64, source_qpc: i64) -> CapturedFrame {
    CapturedFrame {
        slot: seq as usize,
        source_seq: seq,
        source_qpc,
        callback_start_qpc: 25,
        callback_ready_qpc: 25,
        queue_depth: 1,
        present_seq: Some(100 + seq),
    }
}

#[test]
fn empty_wake_uses_a_source_published_before_the_recorder_finishes_sealing() {
    let (sender, frames) = bounded(2);
    let (wake_callback, callback_wake) = bounded::<()>(1);
    let (published, publication) = bounded::<()>(1);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            callback_wake.recv().unwrap();
            sender.send(source(2, 19)).unwrap();
            published.send(()).unwrap();
        });
        // The actual channel wait reports timeout while the source queue is
        // empty. Barriers then publish the eligible frame before recorder seal.
        assert!(matches!(
            frames.recv_timeout(Duration::ZERO),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
        let received = finish_empty_cfr_wake(&frames, || {
            wake_callback.send(())?;
            publication.recv()?;
            Ok(())
        })
        .unwrap();
        let mut selector = CfrSlotSelector::default();
        if let Some(frame) = received {
            let _ = selector.observe(frame, 20);
        }
        let decision = selector.seal(Some((&source(1, 5)).into())).unwrap();
        assert!(
            !decision.duplicate,
            "recorder sealed a duplicate with an eligible source already queued"
        );
        assert_eq!(decision.identity.source_seq, 2);
        assert_eq!(decision.identity.source_qpc, 19);
    });
}

#[test]
fn empty_wake_keeps_a_recovered_future_source_out_of_the_current_slot() {
    let (sender, frames) = bounded(1);
    let frame = finish_empty_cfr_wake(&frames, || {
        sender.send(source(2, 20))?;
        Ok(())
    })
    .unwrap()
    .unwrap();
    let mut selector = CfrSlotSelector::default();
    let CfrObservation::Future(future) = selector.observe(frame, 20) else {
        panic!("the extra queue check retimed a future source");
    };
    let decision = selector.seal(Some((&source(1, 5)).into())).unwrap();
    assert!(decision.duplicate);
    assert_eq!(decision.identity.source_seq, 1);
    assert_eq!(future.source_qpc, 20);
    assert_eq!(future.present_seq, Some(102));
}

#[test]
fn empty_wake_recovery_preserves_latest_candidate_superseding_and_future_hold() {
    let (sender, frames) = bounded(3);
    let first = finish_empty_cfr_wake(&frames, || {
        for frame in [source(2, 17), source(3, 19), source(4, 20)] {
            sender.send(frame)?;
        }
        Ok(())
    })
    .unwrap()
    .unwrap();
    let mut selector = CfrSlotSelector::default();
    let _ = selector.observe(first, 20);
    let future = loop {
        let CfrIngressPoll::Frame(frame) = poll_cfr_ingress(&frames, 30, 24).unwrap() else {
            panic!("queued source lost after recovering the first observation");
        };
        if let CfrObservation::Future(frame) = selector.observe(frame, 20) {
            break frame;
        }
    };
    let decision = selector.seal(Some((&source(1, 5)).into())).unwrap();
    assert!(!decision.duplicate);
    assert_eq!(decision.identity.source_seq, 3);
    assert_eq!(decision.source_superseded, 1);
    assert_eq!(future.source_seq, 4);
}

#[test]
fn empty_wake_still_returns_empty_without_waiting_for_a_future_delivery() {
    let (sender, frames) = bounded(1);
    let checks = std::cell::Cell::new(0);
    assert!(
        finish_empty_cfr_wake(&frames, || {
            checks.set(checks.get() + 1);
            Ok(())
        })
        .unwrap()
        .is_none()
    );
    assert_eq!(checks.get(), 1);
    sender.send(source(2, 19)).unwrap();
    assert_eq!(frames.try_recv().unwrap().source_seq, 2);
}

#[test]
fn empty_wake_source_fault_wins_over_an_arriving_frame() {
    let (sender, frames) = bounded(1);
    let error = finish_empty_cfr_wake(&frames, || {
        sender.send(source(2, 19))?;
        anyhow::bail!("injected source health fault")
    })
    .unwrap_err();
    assert!(error.to_string().contains("source health fault"));
    assert_eq!(frames.try_recv().unwrap().source_seq, 2);
}

#[test]
fn empty_wake_disconnect_is_a_fault_instead_of_a_duplicate() {
    let (sender, frames) = bounded(1);
    let error = finish_empty_cfr_wake(&frames, || {
        drop(sender);
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("disconnected before CFR seal"));
}
