//! Backwards-compatible facade for the shared Present watermark contract.

pub use present_identity::watermark::*;

/// Legacy offline unwrapping helper.
///
/// This function preserves the existing artifact reader behavior. New online
/// code must use `present_identity::FullSequenceDecoder`, which requires a full
/// Ready baseline and rejects observations outside a bounded forward interval.
#[must_use]
pub fn unwrap_seq(previous_unwrapped: u64, raw: u16) -> u64 {
    const PERIOD: u64 = 1 << 16;
    let base = previous_unwrapped / PERIOD * PERIOD;
    let candidates = [
        base + u64::from(raw),
        base + PERIOD + u64::from(raw),
        base.wrapping_sub(PERIOD).wrapping_add(u64::from(raw)),
    ];
    candidates
        .into_iter()
        .filter(|candidate| *candidate <= previous_unwrapped + PERIOD / 2)
        .max_by_key(|candidate| {
            let distance = candidate.abs_diff(previous_unwrapped);
            std::cmp::Reverse(distance)
        })
        .unwrap_or(u64::from(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_unwrap_still_crosses_the_low16_boundary() {
        assert_eq!(unwrap_seq(65_535, 0), 65_536);
        assert_eq!(unwrap_seq(65_536, 1), 65_537);
    }
}
