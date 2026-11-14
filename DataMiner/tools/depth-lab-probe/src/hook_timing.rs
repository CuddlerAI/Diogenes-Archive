//! Bounded wall timing, no per-call allocation, file IO or capture-clock changes.
//! Opt-in slow-call diagnostics reuse the existing clock; only >=2ms calls sample
//! an extra QPC and try (never wait for) the existing bounded frame-ring lock.
//! Includes time waiting for locks/scheduling; not thread CPU time. Concurrent or nested stages
//! are not additive. Microsecond totals truncate each completed call, and relaxed snapshots are
//! diagnostic observations rather than transactional counters or an admission/quality gate.
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TimingSnapshot {
    pub calls: u64,
    pub total_us: u64,
    pub max_us: u64,
}

#[derive(Default)]
pub struct TimingCounter {
    calls: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
}

impl TimingCounter {
    /// One logical call may visit several shards sequentially. Aggregate its
    /// durations before truncating to microseconds so calls/max still mean calls.
    pub(crate) fn record_duration(&self, duration: Duration) {
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.total_us.fetch_add(micros, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
    }

    pub fn start(&self) -> Timer<'_> {
        Timer {
            counter: self,
            start: Instant::now(),
            trace: None,
            finished: false,
        }
    }
    /// Reuse the existing clock; normal calls do not acquire another timestamp.
    pub fn start_traced(&self, kind: crate::perf::SlowHookKind) -> Timer<'_> {
        let epoch = crate::perf::diagnostic_epoch();
        Timer {
            counter: self,
            start: Instant::now(),
            trace: (epoch != 0).then_some((epoch, kind)),
            finished: false,
        }
    }
    pub fn snapshot(&self) -> TimingSnapshot {
        TimingSnapshot {
            calls: self.calls.load(Ordering::Relaxed),
            total_us: self.total_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
        }
    }
}

pub struct Timer<'a> {
    counter: &'a TimingCounter,
    start: Instant,
    trace: Option<(u64, crate::perf::SlowHookKind)>,
    finished: bool,
}

impl Timer<'_> {
    pub(crate) fn finish_prepare(
        mut self,
        mut sample: crate::execute_prepare::PrepareSample,
    ) -> crate::execute_prepare::PrepareSample {
        let elapsed = self.start.elapsed();
        sample.finish(elapsed);
        record_slow_trace(self.start, elapsed, self.trace, Some(sample), None);
        self.counter.record_duration(elapsed);
        self.finished = true;
        sample
    }

    /// Consume the same timer used by Drop, without another start/end clock pair.
    pub(crate) fn finish(mut self) -> Duration {
        let elapsed = self.start.elapsed();
        record_slow_trace(self.start, elapsed, self.trace, None, None);
        self.counter.record_duration(elapsed);
        self.finished = true;
        elapsed
    }

    pub(crate) fn finish_copy(
        mut self,
        mut sample: Option<crate::descriptor_timing::DescriptorCopySample>,
    ) {
        let elapsed = self.start.elapsed();
        if let Some(sample) = sample.as_mut() {
            sample.finish(elapsed);
        }
        record_slow_trace(self.start, elapsed, self.trace, None, sample);
        self.counter.record_duration(elapsed);
        self.finished = true;
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let elapsed = self.start.elapsed();
        record_slow_trace(self.start, elapsed, self.trace, None, None);
        self.counter.record_duration(elapsed);
    }
}

/// Threshold-only timing for scopes absent from counters (Execute and the Draw
/// redundancy predicate). No clock while A3 is disabled; no all-call atomics.
pub fn slow_scope(kind: crate::perf::SlowHookKind) -> SlowScope {
    let epoch = crate::perf::diagnostic_epoch();
    SlowScope {
        trace: (epoch != 0).then(|| (Instant::now(), epoch, kind)),
    }
}

pub struct SlowScope {
    trace: Option<(Instant, u64, crate::perf::SlowHookKind)>,
}

impl Drop for SlowScope {
    fn drop(&mut self) {
        if let Some((start, epoch, kind)) = self.trace {
            record_slow_trace(start, start.elapsed(), Some((epoch, kind)), None, None);
        }
    }
}

fn record_slow_trace(
    start: Instant,
    elapsed: Duration,
    trace: Option<(u64, crate::perf::SlowHookKind)>,
    execute_prepare: Option<crate::execute_prepare::PrepareSample>,
    descriptor_copy: Option<crate::descriptor_timing::DescriptorCopySample>,
) {
    if let Some((epoch, kind)) = trace
        && elapsed.as_nanos() >= u128::from(crate::perf::SLOW_HOOK_THRESHOLD_NS)
    {
        let mut qpc = 0;
        let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut qpc) };
        let aligned_elapsed = start.elapsed();
        crate::perf::record_slow_hook(
            epoch,
            crate::perf::SlowHookEvent {
                kind,
                thread_id: unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
                observed_end_qpc: qpc,
                duration_ns: elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
                alignment_uncertainty_ns: aligned_elapsed
                    .saturating_sub(elapsed)
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64,
                execute_prepare,
                descriptor_copy,
            },
        );
    }
}

#[test]
fn explicit_prepare_finish_records_exactly_once_and_preserves_residual() {
    let counter = TimingCounter::default();
    let sample = counter.start().finish_prepare(Default::default());
    assert_eq!(counter.snapshot().calls, 1);
    assert_eq!(sample.total_ns, sample.residual_ns);
}
