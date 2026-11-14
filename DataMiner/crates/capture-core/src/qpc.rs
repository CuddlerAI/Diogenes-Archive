use anyhow::{Context, ensure};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

#[derive(Clone, Copy, Debug)]
pub struct QpcClock {
    frequency: i64,
}

impl QpcClock {
    pub fn new() -> anyhow::Result<Self> {
        let mut frequency = 0_i64;
        unsafe { QueryPerformanceFrequency(&mut frequency) }
            .context("QueryPerformanceFrequency failed")?;
        ensure!(frequency > 0, "QPC frequency is not positive");
        Ok(Self { frequency })
    }

    #[must_use]
    pub const fn frequency(self) -> i64 {
        self.frequency
    }

    pub fn now(self) -> anyhow::Result<i64> {
        let mut value = 0_i64;
        unsafe { QueryPerformanceCounter(&mut value) }.context("QueryPerformanceCounter failed")?;
        Ok(value)
    }

    #[must_use]
    pub fn relative_100ns_to_qpc(self, ticks_100ns: i64) -> i64 {
        round_ratio(
            i128::from(ticks_100ns) * i128::from(self.frequency),
            10_000_000,
        ) as i64
    }

    #[must_use]
    pub fn elapsed_micros(self, start: i64, end: i64) -> f64 {
        (end - start) as f64 * 1_000_000.0 / self.frequency as f64
    }
}

#[must_use]
pub fn frame_pts_100ns(frame_index: u64, fps_num: u32, fps_den: u32) -> i64 {
    round_ratio(
        i128::from(frame_index) * 10_000_000 * i128::from(fps_den),
        i128::from(fps_num),
    ) as i64
}

#[must_use]
pub fn frame_duration_100ns(frame_index: u64, fps_num: u32, fps_den: u32) -> i64 {
    frame_pts_100ns(frame_index + 1, fps_num, fps_den)
        - frame_pts_100ns(frame_index, fps_num, fps_den)
}

#[must_use]
pub fn frame_qpc(
    anchor_qpc: i64,
    frame_index: u64,
    qpc_frequency: i64,
    fps_num: u32,
    fps_den: u32,
) -> i64 {
    anchor_qpc
        + round_ratio(
            i128::from(frame_index) * i128::from(qpc_frequency) * i128::from(fps_den),
            i128::from(fps_num),
        ) as i64
}

pub fn frame_index_at_qpc(
    anchor_qpc: i64,
    qpc: i64,
    qpc_frequency: i64,
    fps_num: u32,
    fps_den: u32,
) -> anyhow::Result<u64> {
    ensure!(qpc_frequency > 0, "QPC frequency must be positive");
    ensure!(fps_num > 0 && fps_den > 0, "frame rate must be positive");
    ensure!(qpc >= anchor_qpc, "QPC precedes the run anchor");

    let delta = i128::from(qpc - anchor_qpc);
    let denominator = i128::from(qpc_frequency) * i128::from(fps_den);
    let estimate = delta
        .checked_mul(i128::from(fps_num))
        .context("QPC-to-frame multiplication overflowed")?
        / denominator;
    let mut frame_index = u64::try_from(estimate).context("QPC-to-frame index exceeds u64")?;

    while frame_index > 0
        && frame_qpc(anchor_qpc, frame_index, qpc_frequency, fps_num, fps_den) > qpc
    {
        frame_index -= 1;
    }
    while frame_index < u64::MAX
        && frame_qpc(anchor_qpc, frame_index + 1, qpc_frequency, fps_num, fps_den) <= qpc
    {
        frame_index += 1;
    }
    Ok(frame_index)
}

pub fn qpc_to_monotonic_ns(qpc: i64, qpc_frequency: i64) -> anyhow::Result<u64> {
    ensure!(qpc >= 0, "QPC must not be negative");
    ensure!(qpc_frequency > 0, "QPC frequency must be positive");
    let nanos = i128::from(qpc)
        .checked_mul(1_000_000_000)
        .context("QPC-to-nanoseconds multiplication overflowed")?
        / i128::from(qpc_frequency);
    u64::try_from(nanos).context("QPC nanoseconds value exceeds u64")
}

fn round_ratio(numerator: i128, denominator: i128) -> i128 {
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        (numerator - denominator / 2) / denominator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_pts_has_no_accumulated_drift() {
        assert_eq!(frame_pts_100ns(0, 60_000, 1001), 0);
        assert_eq!(frame_pts_100ns(600, 60_000, 1001), 100_100_000);
        let total: i64 = (0..600)
            .map(|i| frame_duration_100ns(i, 60_000, 1001))
            .sum();
        assert_eq!(total, frame_pts_100ns(600, 60_000, 1001));
    }

    #[test]
    fn qpc_frame_lookup_uses_half_open_cfr_boundaries() {
        let anchor = 10_000;
        let frequency = 10_000_000;
        let boundary = frame_qpc(anchor, 1, frequency, 30, 1);

        assert_eq!(
            frame_index_at_qpc(anchor, anchor, frequency, 30, 1).unwrap(),
            0
        );
        assert_eq!(
            frame_index_at_qpc(anchor, boundary - 1, frequency, 30, 1).unwrap(),
            0
        );
        assert_eq!(
            frame_index_at_qpc(anchor, boundary, frequency, 30, 1).unwrap(),
            1
        );
        assert_eq!(frame_pts_100ns(1, 30, 1) / 10, 33_333);
    }

    #[test]
    fn qpc_to_monotonic_nanoseconds_uses_integer_floor_without_drift() {
        assert_eq!(
            qpc_to_monotonic_ns(12_345_678, 10_000_000).unwrap(),
            1_234_567_800
        );
        assert_eq!(qpc_to_monotonic_ns(1, 3).unwrap(), 333_333_333);
        assert!(qpc_to_monotonic_ns(-1, 10_000_000).is_err());
        assert!(qpc_to_monotonic_ns(1, 0).is_err());
    }
}
