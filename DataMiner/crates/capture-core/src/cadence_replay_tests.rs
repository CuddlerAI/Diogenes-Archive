//! Deterministic delivery/consumer timing experiments using the real ingress
//! poll and selector. No sleep, game, GPU, input injection or production knob.
//! These model the observed symptom, not capture 18's missing arrival evidence.

use super::*;

const MS: i64 = 10_000;
const PERIOD: i64 = 333_333; // A 30 FPS interval on a 10 MHz QPC clock.

#[derive(Clone, Copy)]
struct Delivery {
    at: i64,
    frame: CapturedFrame,
}

fn source(seq: u64, timestamp: i64, delivery: i64) -> Delivery {
    Delivery {
        at: delivery,
        frame: CapturedFrame {
            slot: seq as usize,
            source_seq: seq,
            source_qpc: timestamp,
            callback_start_qpc: delivery,
            callback_ready_qpc: delivery,
            queue_depth: 1,
            present_seq: Some(100 + seq),
        },
    }
}

/// Virtual time advances to the next arrival or deadline. Arrivals at a wake
/// are published before polling. Channel operations and slot selection use the
/// production implementations; GPU processing is assumed ready. This does NOT
/// reproduce OS scheduling inside select!/the timeout-to-seal interval.
fn replay(
    deliveries: &[Delivery],
    first_wake: i64,
    seal_delay: i64,
    slots: u64,
) -> Vec<CfrSlotDecision> {
    assert!(deliveries.windows(2).all(|pair| pair[0].at <= pair[1].at));
    let (sender, receiver) = bounded(deliveries.len().max(1));
    let mut now = first_wake;
    let mut cursor = 0;
    let mut future = None;
    let mut previous = SelectedSourceIdentity::from(&source(1, -MS, -MS).frame);
    let mut decisions = Vec::new();
    for slot in 0..slots {
        let end = (slot as i64 + 1) * PERIOD;
        let seal = end + seal_delay;
        let mut selector = CfrSlotSelector::default();
        loop {
            while cursor < deliveries.len() && deliveries[cursor].at <= now {
                sender.try_send(deliveries[cursor].frame).unwrap();
                cursor += 1;
            }
            let next = if let Some(frame) = future.take() {
                frame
            } else {
                match poll_cfr_ingress(&receiver, now, seal).unwrap() {
                    CfrIngressPoll::Frame(frame) => frame,
                    CfrIngressPoll::DeadlineReached => break,
                    CfrIngressPoll::Wait => {
                        let next_time = deliveries
                            .get(cursor)
                            .map_or(seal, |event| event.at.min(seal));
                        assert!(next_time > now, "virtual replay must make progress");
                        now = next_time;
                        continue;
                    }
                }
            };
            if let CfrObservation::Future(frame) = selector.observe(next, end) {
                future = Some(frame);
                break;
            }
        }
        let decision = selector.seal(Some(previous)).unwrap();
        previous = decision.identity;
        decisions.push(decision);
    }
    decisions
}

#[test]
fn cadence_replay_late_delivery_reproduces_duplicate_then_old_timestamp_next_slot() {
    let timestamp = PERIOD - 4 * MS;
    // A 10 ms source-to-delivery delay misses end+4ms by 2ms. This is an
    // explicitly invented arrival time, not recovered capture-18 telemetry.
    let late = source(2, timestamp, timestamp + 10 * MS);
    let decisions = replay(&[late], 0, 4 * MS, 2);
    assert!(decisions[0].duplicate);
    assert_eq!(decisions[0].identity.source_seq, 1);
    assert!(!decisions[1].duplicate);
    assert_eq!(
        decisions[1].identity,
        SelectedSourceIdentity::from(&late.frame)
    );
    assert!(decisions[1].identity.source_qpc < PERIOD);
}

#[test]
fn cadence_replay_sweep_separates_delivery_deadline_from_consumer_wakeup() {
    let seal = PERIOD + 4 * MS;
    let mut cases = 0;
    for phase in [-10 * MS, -MS, -1, 0, 1, MS] {
        for delivery_offset in [-8 * MS, -MS, -1, 0, 1, MS, 8 * MS, 20 * MS] {
            for first_wake in [0, seal - 1, seal, seal + MS, seal + 25 * MS] {
                let event = source(2, PERIOD + phase, seal + delivery_offset);
                let decisions = replay(&[event], first_wake, 4 * MS, 1);
                // The independent oracle asks two questions: does the source
                // belong to this slot, and is it available before this schedule
                // can seal? Waking late must not hide an already queued frame.
                let eligible_and_available = phase < 0 && event.at <= first_wake.max(seal);
                assert_eq!(
                    !decisions[0].duplicate, eligible_and_available,
                    "phase={phase}, delivery_offset={delivery_offset}, first_wake={first_wake}"
                );
                if eligible_and_available {
                    assert_eq!(
                        decisions[0].identity,
                        SelectedSourceIdentity::from(&event.frame)
                    );
                }
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 240);
}

#[test]
fn cadence_replay_more_wait_can_cover_delivery_delay_but_cannot_retime_future_sources() {
    let late = source(2, PERIOD - 4 * MS, PERIOD + 6 * MS);
    let future = source(2, PERIOD + MS, PERIOD - MS);
    for wait in [4 * MS, 8 * MS, 16 * MS] {
        let delivery_result = replay(&[late], 0, wait, 1);
        assert_eq!(delivery_result[0].duplicate, wait < 6 * MS);
        if !delivery_result[0].duplicate {
            assert_eq!(
                delivery_result[0].identity.source_qpc,
                late.frame.source_qpc
            );
        }
        let future_result = replay(&[future], 0, wait, 1);
        assert!(future_result[0].duplicate);
        assert_eq!(future_result[0].identity.source_seq, 1);
    }
}
