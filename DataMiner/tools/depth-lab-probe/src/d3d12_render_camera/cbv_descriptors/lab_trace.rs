//! Bounded, intrusive LAB-ONLY ownership observations. Never enabled by the game DLL.
//! Cycle probes perturb holds; cycles are not wall time. Observed intervals are lower
//! bounds: scheduling between lock return/unlock and a clock read is not identifiable here.
use crate::descriptor_timing::CopyPhase;
use crate::execute_prepare::nanos;
use std::cell::RefCell;
use std::time::Instant;
use windows::Win32::System::{
    Threading::GetCurrentThread, WindowsProgramming::QueryThreadCycleTime,
};

#[derive(Clone, serde::Serialize)]
pub(crate) struct Visit {
    worker: usize,
    copy_index: usize,
    shard: usize,
    phase: CopyPhase,
    wait_begin_ns: u64,
    acquired_ns: u64,
    acquire_counter_probe_end_ns: u64,
    unlock_counter_probe_begin_ns: u64,
    unlock_begin_ns: u64,
    unlocked_ns: u64,
    held_cycles: Option<u64>,
}

pub(crate) struct ThreadTrace {
    origin: Instant,
    worker: usize,
    copy_index: usize,
    visits: Vec<Visit>,
    capacity: usize,
    lost: usize,
}

thread_local! {
    static TRACE: RefCell<Option<ThreadTrace>> = const { RefCell::new(None) };
}

pub(crate) fn start(worker: usize, origin: Instant, capacity: usize) {
    assert!(capacity <= 32_000);
    TRACE.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(ThreadTrace {
            origin,
            worker,
            copy_index: 0,
            visits: Vec::with_capacity(capacity),
            capacity,
            lost: 0,
        });
    });
}

pub(crate) fn begin_copy(index: usize) {
    TRACE.with(|slot| {
        if let Some(trace) = slot.borrow_mut().as_mut() {
            trace.copy_index = index;
        }
    });
}

pub(crate) fn finish() -> Option<ThreadTrace> {
    TRACE.with(|slot| slot.borrow_mut().take())
}

fn cycles() -> Option<u64> {
    let mut value = 0;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut value) }.ok()?;
    Some(value)
}

pub(crate) struct Attempt {
    origin: Instant,
    visit: Visit,
    acquired_cycles: Option<u64>,
    acquired: bool,
}

impl Attempt {
    pub(crate) fn begin(shard: usize, phase: CopyPhase) -> Option<Self> {
        TRACE.with(|slot| {
            let trace = slot.borrow();
            let trace = trace.as_ref()?;
            Some(Self {
                origin: trace.origin,
                visit: Visit {
                    worker: trace.worker,
                    copy_index: trace.copy_index,
                    shard,
                    phase,
                    wait_begin_ns: nanos(trace.origin.elapsed()),
                    acquired_ns: 0,
                    acquire_counter_probe_end_ns: 0,
                    unlock_counter_probe_begin_ns: 0,
                    unlock_begin_ns: 0,
                    unlocked_ns: 0,
                    held_cycles: None,
                },
                acquired_cycles: None,
                acquired: false,
            })
        })
    }

    pub(crate) fn acquired(&mut self, acquired: bool) {
        self.acquired = acquired;
        self.visit.acquired_ns = nanos(self.origin.elapsed());
        self.acquired_cycles = cycles();
        self.visit.acquire_counter_probe_end_ns = nanos(self.origin.elapsed());
    }

    pub(crate) fn unlocking(&mut self) {
        self.visit.unlock_counter_probe_begin_ns = nanos(self.origin.elapsed());
        self.visit.held_cycles = cycles()
            .zip(self.acquired_cycles)
            .and_then(|(end, begin)| end.checked_sub(begin));
        self.visit.unlock_begin_ns = nanos(self.origin.elapsed());
    }

    pub(crate) fn released(mut self) {
        self.visit.unlocked_ns = nanos(self.origin.elapsed());
        if !self.acquired {
            return;
        }
        // Record only after releasing the real shard. Capacity is allocated before start gates.
        TRACE.with(|slot| {
            if let Some(trace) = slot.borrow_mut().as_mut() {
                if trace.visits.len() < trace.capacity {
                    trace.visits.push(self.visit);
                } else {
                    trace.lost += 1;
                }
            }
        });
    }
}

#[derive(serde::Serialize)]
pub(crate) struct Correlation {
    scope: &'static str,
    intrusive: bool,
    visits: usize,
    lost_visits: usize,
    missing_cycle_samples: usize,
    longest_wait: Visit,
    wait_ns: u64,
    observed_held_union_ns: u64,
    not_covered_by_observed_holds_ns: u64,
    overlapping_holder_count: usize,
    holder_sample_selection: &'static str,
    holder_samples: Vec<Visit>,
}

pub(crate) fn correlate(
    traces: Vec<ThreadTrace>,
    expected_visits: usize,
) -> Result<Correlation, String> {
    let lost = traces.iter().map(|t| t.lost).sum::<usize>();
    let visits: Vec<_> = traces.into_iter().flat_map(|t| t.visits).collect();
    if lost != 0 || visits.len() != expected_visits || visits.is_empty() {
        return Err(format!(
            "ownership trace incomplete: {} visits, expected {expected_visits}, lost {lost}",
            visits.len()
        ));
    }
    if visits.iter().any(|v| {
        !(v.wait_begin_ns <= v.acquired_ns
            && v.acquired_ns <= v.acquire_counter_probe_end_ns
            && v.acquire_counter_probe_end_ns <= v.unlock_counter_probe_begin_ns
            && v.unlock_counter_probe_begin_ns <= v.unlock_begin_ns
            && v.unlock_begin_ns <= v.unlocked_ns)
    }) {
        return Err("ownership trace clock order invalid".into());
    }
    let (worst_index, worst) = visits
        .iter()
        .enumerate()
        .max_by_key(|(_, v)| v.acquired_ns - v.wait_begin_ns)
        .unwrap();
    let mut holders: Vec<_> = visits
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let start = v.acquired_ns.max(worst.wait_begin_ns);
            let end = v.unlock_begin_ns.min(worst.acquired_ns);
            (i != worst_index && v.shard == worst.shard && end > start).then_some((start, end, v))
        })
        .collect();
    holders.sort_unstable_by_key(|v| v.0);
    let (mut covered, mut previous_end) = (0u64, 0u64);
    for &(start, end, _) in &holders {
        covered += end.saturating_sub(start.max(previous_end));
        previous_end = previous_end.max(end);
    }
    let holder_count = holders.len();
    // Summaries include ALL observed holders; only the JSON event sample is bounded.
    holders.sort_unstable_by_key(|(start, end, _)| std::cmp::Reverse(end - start));
    let mut samples: Vec<_> = holders
        .iter()
        .take(32)
        .map(|(_, _, v)| (*v).clone())
        .collect();
    samples.sort_unstable_by_key(|v| v.acquired_ns);
    let wait_ns = worst.acquired_ns - worst.wait_begin_ns;
    Ok(Correlation {
        scope: "private_store_copy_phase_locks; setup_and_final_validation_outside_workers; observed_hold_lower_bounds_not_kernel_scheduler_trace",
        intrusive: true,
        visits: visits.len(),
        lost_visits: lost,
        missing_cycle_samples: visits.iter().filter(|v| v.held_cycles.is_none()).count(),
        longest_wait: worst.clone(),
        wait_ns,
        observed_held_union_ns: covered,
        not_covered_by_observed_holds_ns: wait_ns.saturating_sub(covered),
        overlapping_holder_count: holder_count,
        holder_sample_selection: "top_32_by_overlap_then_chronological; all_holders_in_union",
        holder_samples: samples,
    })
}
