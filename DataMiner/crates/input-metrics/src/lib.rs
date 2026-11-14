//! Platform-independent input health metrics.
//!
//! Windows may deliver several high-frequency Raw Input reports in one message-loop
//! batch. Those reports have a shared observation timestamp because `RAWINPUT` does
//! not contain a per-report hardware timestamp. Treating equal timestamps as invalid
//! or timing only the message-loop wakeups under-reports a 500/1000 Hz mouse as
//! roughly 125 Hz. This estimator instead divides each delivery interval by the
//! number of reports delivered in that interval.

/// Version committed into calibration evidence whenever this estimator is used.
pub const POLLING_ESTIMATOR_VERSION: &str = "delivery-batch-rate-1";

/// DataMiner supports mouse profiles up to 8 kHz. Keep one extra octave so timing
/// jitter can be represented without an unbounded allocation. Values above the
/// ceiling are conservatively reported as the ceiling.
const MAX_TRACKED_HZ: usize = 16_384;

/// Summary of batch-aware mouse report-rate observations.
#[derive(Clone, Debug, PartialEq)]
pub struct PollingRateSummary {
    pub p50_hz: Option<f64>,
    /// Lower-tail rate corresponding to the slowest five percent of reports.
    pub p05_hz: Option<f64>,
    pub observed_report_count: u64,
    pub estimated_report_count: u64,
    pub delivery_group_count: u64,
    pub max_delivery_batch_size: u64,
    pub non_monotonic_reports: u64,
}

/// Bounded-memory estimator for report rates observed at delivery-batch QPCs.
///
/// All reports read from one `GetRawInputData` + `GetRawInputBuffer` drain carry
/// the same QPC. If four reports arrive every 8 ms, the effective interval is
/// `8 ms / 4`, correctly yielding 500 Hz without inventing four timestamps.
#[derive(Debug)]
pub struct PollingRateEstimator {
    qpc_frequency: i64,
    histogram: Box<[u64]>,
    observed_report_count: u64,
    estimated_report_count: u64,
    delivery_group_count: u64,
    max_delivery_batch_size: u64,
    non_monotonic_reports: u64,
    previous_group_qpc: Option<i64>,
    current_group_qpc: Option<i64>,
    current_group_reports: u64,
}

impl PollingRateEstimator {
    #[must_use]
    pub fn new(qpc_frequency: i64) -> Self {
        Self {
            qpc_frequency,
            histogram: vec![0; MAX_TRACKED_HZ + 1].into_boxed_slice(),
            observed_report_count: 0,
            estimated_report_count: 0,
            delivery_group_count: 0,
            max_delivery_batch_size: 0,
            non_monotonic_reports: 0,
            previous_group_qpc: None,
            current_group_qpc: None,
            current_group_reports: 0,
        }
    }

    #[must_use]
    pub fn qpc_frequency(&self) -> i64 {
        self.qpc_frequency
    }

    pub fn push(&mut self, delivery_qpc: i64) {
        self.observed_report_count = self.observed_report_count.saturating_add(1);
        match self.current_group_qpc {
            None => self.start_group(delivery_qpc),
            Some(current) if delivery_qpc == current => {
                self.current_group_reports = self.current_group_reports.saturating_add(1);
                self.max_delivery_batch_size =
                    self.max_delivery_batch_size.max(self.current_group_reports);
            }
            Some(current) if delivery_qpc > current => {
                self.finish_current_group();
                self.previous_group_qpc = Some(current);
                self.start_group(delivery_qpc);
            }
            Some(_) => {
                // Keep totals/counts auditable, but never let a regressed clock value
                // contaminate the rate histogram or the current delivery group.
                self.non_monotonic_reports = self.non_monotonic_reports.saturating_add(1);
            }
        }
    }

    #[must_use]
    pub fn finish(mut self, min_estimated_reports: u64) -> PollingRateSummary {
        self.finish_current_group();
        let enough = self.estimated_report_count >= min_estimated_reports && self.qpc_frequency > 0;
        PollingRateSummary {
            p50_hz: enough.then(|| self.quantile_hz(50, 100)).flatten(),
            p05_hz: enough.then(|| self.quantile_hz(5, 100)).flatten(),
            observed_report_count: self.observed_report_count,
            estimated_report_count: self.estimated_report_count,
            delivery_group_count: self.delivery_group_count,
            max_delivery_batch_size: self.max_delivery_batch_size,
            non_monotonic_reports: self.non_monotonic_reports,
        }
    }

    fn start_group(&mut self, delivery_qpc: i64) {
        self.current_group_qpc = Some(delivery_qpc);
        self.current_group_reports = 1;
        self.delivery_group_count = self.delivery_group_count.saturating_add(1);
        self.max_delivery_batch_size = self.max_delivery_batch_size.max(1);
    }

    fn finish_current_group(&mut self) {
        let (Some(previous), Some(current)) = (self.previous_group_qpc, self.current_group_qpc)
        else {
            return;
        };
        let delta = current - previous;
        if delta <= 0 || self.current_group_reports == 0 || self.qpc_frequency <= 0 {
            return;
        }
        let numerator =
            (self.qpc_frequency as u128).saturating_mul(self.current_group_reports as u128);
        let rounded_hz = numerator
            .saturating_add(delta as u128 / 2)
            .checked_div(delta as u128)
            .unwrap_or(0);
        if rounded_hz == 0 {
            return;
        }
        let bin = usize::try_from(rounded_hz)
            .unwrap_or(MAX_TRACKED_HZ)
            .min(MAX_TRACKED_HZ);
        self.histogram[bin] = self.histogram[bin].saturating_add(self.current_group_reports);
        self.estimated_report_count = self
            .estimated_report_count
            .saturating_add(self.current_group_reports);
    }

    fn quantile_hz(&self, numerator: u64, denominator: u64) -> Option<f64> {
        if self.estimated_report_count == 0 || denominator == 0 {
            return None;
        }
        let rank = self
            .estimated_report_count
            .saturating_mul(numerator)
            .div_ceil(denominator)
            .max(1);
        let mut accumulated = 0_u64;
        for (hz, count) in self.histogram.iter().copied().enumerate() {
            accumulated = accumulated.saturating_add(count);
            if accumulated >= rank {
                return Some(hz as f64);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QPC_FREQUENCY: i64 = 10_000_000;

    fn batched(batch_hz: i64, reports_per_batch: usize, batches: usize) -> PollingRateSummary {
        let mut estimator = PollingRateEstimator::new(QPC_FREQUENCY);
        let step = QPC_FREQUENCY / batch_hz;
        for batch in 0..batches {
            let qpc = (batch as i64 + 1) * step;
            for _ in 0..reports_per_batch {
                estimator.push(qpc);
            }
        }
        estimator.finish(2)
    }

    #[test]
    fn unbatched_stream_keeps_native_rate() {
        let summary = batched(1_000, 1, 1_000);
        assert_eq!(summary.p50_hz, Some(1_000.0));
        assert_eq!(summary.max_delivery_batch_size, 1);
        assert_eq!(summary.delivery_group_count, 1_000);
    }

    #[test]
    fn four_reports_every_eight_ms_estimate_five_hundred_hz() {
        let summary = batched(125, 4, 250);
        assert_eq!(summary.p50_hz, Some(500.0));
        assert_eq!(summary.p05_hz, Some(500.0));
        assert_eq!(summary.max_delivery_batch_size, 4);
    }

    #[test]
    fn eight_reports_every_eight_ms_estimate_one_thousand_hz() {
        let summary = batched(125, 8, 250);
        assert_eq!(summary.p50_hz, Some(1_000.0));
        assert_eq!(summary.max_delivery_batch_size, 8);
    }

    #[test]
    fn one_slow_delivery_does_not_drag_down_the_median() {
        let mut estimator = PollingRateEstimator::new(QPC_FREQUENCY);
        for batch in 0..100_i64 {
            let qpc = if batch < 50 {
                (batch + 1) * 80_000
            } else {
                (batch + 1) * 80_000 + 1_000_000
            };
            for _ in 0..8 {
                estimator.push(qpc);
            }
        }
        let summary = estimator.finish(2);
        assert_eq!(summary.p50_hz, Some(1_000.0));
        assert!(summary.p05_hz.is_some_and(|hz| hz <= 1_000.0));
    }

    #[test]
    fn regressed_qpc_is_counted_but_not_estimated() {
        let mut estimator = PollingRateEstimator::new(QPC_FREQUENCY);
        estimator.push(100);
        estimator.push(90);
        estimator.push(200);
        estimator.push(300);
        let summary = estimator.finish(2);
        assert_eq!(summary.observed_report_count, 4);
        assert_eq!(summary.non_monotonic_reports, 1);
        assert_eq!(summary.estimated_report_count, 2);
    }

    #[test]
    fn one_delivery_group_is_not_enough_to_invent_a_rate() {
        let mut estimator = PollingRateEstimator::new(QPC_FREQUENCY);
        for _ in 0..8 {
            estimator.push(100);
        }
        let summary = estimator.finish(2);
        assert_eq!(summary.p50_hz, None);
        assert_eq!(summary.delivery_group_count, 1);
        assert_eq!(summary.max_delivery_batch_size, 8);
    }
}
