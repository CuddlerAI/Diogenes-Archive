//! Run 级墙钟锚点。
//!
//! 时基契约不变：排序、帧对齐、finalize 单调性硬检查全部只用 QPC
//! （`docs/02-input-jsonl-spec.md`）。本模块只额外记录一条描述性事实——
//! 某个 QPC 值落在哪个 UTC 时刻——好让写完的 run 能回答"这批数据录于何时"。
//! 墙钟会被 NTP 校正、夏令时和睡眠唤醒改写，绝不能进入任何排序或对齐路径。

use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

use crate::qpc::QpcClock;

/// FILETIME 纪元（1601-01-01）到 Unix 纪元（1970-01-01）的 100ns tick 数。
const FILETIME_TO_UNIX_100NS: i64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;
const TICKS_PER_MILLI: i64 = 10_000;
const MILLIS_PER_DAY: i64 = 86_400_000;

/// QPC 与系统 UTC 时钟的一次配对读数。
///
/// 锚点必须是 per-run 的：QPC 跨重启、跨机器不可比，拿别的 run 的样本换算
/// 出来的 UTC 没有意义。
#[derive(Clone, Copy, Debug)]
pub struct WallClockSample {
    qpc: i64,
    unix_100ns: i64,
}

impl WallClockSample {
    /// 背靠背读两个时钟。两次读数之间的偏差在纳秒量级，远低于本模块输出的
    /// 毫秒精度。
    pub fn take(clock: QpcClock) -> anyhow::Result<Self> {
        let filetime = unsafe { GetSystemTimePreciseAsFileTime() };
        let qpc = clock.now()?;
        let ticks = (i64::from(filetime.dwHighDateTime) << 32) | i64::from(filetime.dwLowDateTime);
        Ok(Self {
            qpc,
            unix_100ns: ticks - FILETIME_TO_UNIX_100NS,
        })
    }

    /// 把本次读数投影到 `target_qpc`，返回 ISO-8601 毫秒精度 UTC 字符串。
    ///
    /// 投影距离远小于 run 时长——anchor 侧是毫秒级，收尾侧是最后一段的 finalize
    /// 耗时——所以 QPC 与墙钟的长期漂移不会被放大。run 两端各取一次**独立**样本，
    /// 消费方比较 `endedAtUtc - anchorAtUtc` 与
    /// `(qpcEnd - anchorQpc) / qpcFrequency` 就能界定这一整段的漂移。
    ///
    /// 返回 `None` 表示系统时钟落在可表示范围之外（例如 RTC 没电归零）。锚点是
    /// 描述性元数据，缺它不该把一次录完的 run 变成 failure，所以降级成 `null`
    /// 而不是报错。
    #[must_use]
    pub fn iso8601_at(self, target_qpc: i64, qpc_frequency: i64) -> Option<String> {
        format_iso8601_millis(project_unix_100ns(
            self.unix_100ns,
            self.qpc,
            target_qpc,
            qpc_frequency,
        )?)
    }
}

fn project_unix_100ns(
    sample_unix_100ns: i64,
    sample_qpc: i64,
    target_qpc: i64,
    qpc_frequency: i64,
) -> Option<i64> {
    if qpc_frequency <= 0 {
        return None;
    }
    let delta = i128::from(target_qpc) - i128::from(sample_qpc);
    let offset = delta.checked_mul(i128::from(TICKS_PER_SECOND))? / i128::from(qpc_frequency);
    i64::try_from(i128::from(sample_unix_100ns) + offset).ok()
}

fn format_iso8601_millis(unix_100ns: i64) -> Option<String> {
    let millis = unix_100ns.div_euclid(TICKS_PER_MILLI);
    let (year, month, day) = civil_from_days(millis.div_euclid(MILLIS_PER_DAY))?;
    let ms_of_day = millis.rem_euclid(MILLIS_PER_DAY);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms_of_day / 3_600_000,
        (ms_of_day / 60_000) % 60,
        (ms_of_day / 1_000) % 60,
        ms_of_day % 1_000,
    ))
}

/// Howard Hinnant 的 `civil_from_days`：Unix 纪元起的天数换成 proleptic
/// Gregorian 年月日。`None` 表示年份超出四位数字能表示的范围。
fn civil_from_days(days: i64) -> Option<(i64, i64, i64)> {
    // 原式对负数用 `(z >= 0 ? z : z - 146096) / 146097` 手写地板除；Rust 的
    // div_euclid/rem_euclid 在正除数下正是地板除与非负余数，等价且不必分支。
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (1..=9999).contains(&year).then_some((year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(unix_millis: i64) -> Option<String> {
        format_iso8601_millis(unix_millis * TICKS_PER_MILLI)
    }

    #[test]
    fn formats_epoch_leap_years_and_month_ends() {
        assert_eq!(iso(0).unwrap(), "1970-01-01T00:00:00.000Z");
        // 2026-07-27T14:03:07.250Z，本次改动的实测采集日附近。
        assert_eq!(iso(1_785_160_987_250).unwrap(), "2026-07-27T14:03:07.250Z");
        // 2024 是闰年，2100 不是（百年不闰、四百年再闰）。
        assert_eq!(iso(1_709_164_800_000).unwrap(), "2024-02-29T00:00:00.000Z");
        assert_eq!(iso(4_107_542_400_000).unwrap(), "2100-03-01T00:00:00.000Z");
        assert_eq!(iso(1_767_225_599_999).unwrap(), "2025-12-31T23:59:59.999Z");
    }

    #[test]
    fn every_day_boundary_round_trips_for_two_centuries() {
        // 逐日走 1970..2170，确认 civil_from_days 的月长/闰年分支没有断点：
        // 日期必须严格递增，且每一天的午夜都能格式化。
        let mut previous = String::new();
        for day in 0..73_050_i64 {
            let rendered = iso(day * MILLIS_PER_DAY).expect("day is representable");
            assert!(
                rendered > previous,
                "day {day} went backwards: {previous} -> {rendered}"
            );
            assert!(rendered.ends_with("T00:00:00.000Z"));
            previous = rendered;
        }
    }

    #[test]
    fn projection_reconstructs_absolute_time_from_the_qpc_delta() {
        let sample_unix_100ns = 17_852_473_872_500_000;
        // 3.125MHz 是 08 文档里的实测频率之一：1 tick = 320ns 整。
        let frequency = 3_125_000;
        let sample_qpc = 918_120_243_402;

        // 向后投影 2 秒。
        assert_eq!(
            project_unix_100ns(
                sample_unix_100ns,
                sample_qpc,
                sample_qpc + 2 * frequency,
                frequency
            )
            .unwrap(),
            sample_unix_100ns + 2 * TICKS_PER_SECOND
        );
        // 向前投影 500ms —— anchor 总是落在样本之前，这条路径必须精确。
        assert_eq!(
            project_unix_100ns(
                sample_unix_100ns,
                sample_qpc,
                sample_qpc - frequency / 2,
                frequency
            )
            .unwrap(),
            sample_unix_100ns - TICKS_PER_SECOND / 2
        );
        assert_eq!(
            project_unix_100ns(sample_unix_100ns, sample_qpc, sample_qpc, frequency).unwrap(),
            sample_unix_100ns
        );
    }

    #[test]
    fn unrepresentable_clocks_degrade_to_none_instead_of_a_wrong_string() {
        assert_eq!(project_unix_100ns(0, 0, 1, 0), None);
        assert_eq!(project_unix_100ns(0, 0, 1, -1), None);
        assert_eq!(project_unix_100ns(0, i64::MIN, i64::MAX, 1), None);
        // RTC 归零到 FILETIME 纪元本身仍可表示，如实写出来由消费方做区间校验。
        assert_eq!(
            format_iso8601_millis(-FILETIME_TO_UNIX_100NS).unwrap(),
            "1601-01-01T00:00:00.000Z"
        );
        assert_eq!(format_iso8601_millis(i64::MIN), None);
        assert_eq!(format_iso8601_millis(i64::MAX), None);
    }
}
