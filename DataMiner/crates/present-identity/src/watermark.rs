//! Encoding and decoding for the 192x8 low-16 Present watermark.

pub const BLOCK_PX: u32 = 8;
/// Whitening value for the CRC byte. It guarantees black and white pilot
/// blocks for the all-zero payload while the byte remains payload-dependent.
pub const INTEGRITY_XOR: u8 = 0b1010_1010;
pub const SYNC_BITS: u32 = 8;
pub const PAYLOAD_BITS: u32 = 16;
pub const TOTAL_BLOCKS: u32 = SYNC_BITS + PAYLOAD_BITS;
pub const RECT_W: u32 = TOTAL_BLOCKS * BLOCK_PX;
pub const RECT_H: u32 = BLOCK_PX;

#[must_use]
pub const fn integrity_code(present_seq: u16) -> u8 {
    let bytes = present_seq.to_be_bytes();
    let mut crc = 0u8;
    let mut byte_index = 0;
    while byte_index < bytes.len() {
        crc ^= bytes[byte_index];
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        byte_index += 1;
    }
    crc ^ INTEGRITY_XOR
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatermarkRects {
    pub white: Vec<Rect>,
    pub black: Vec<Rect>,
}

#[must_use]
pub fn encode(present_seq: u16, origin_x: u32, origin_y: u32) -> WatermarkRects {
    let mut white = Vec::new();
    let mut black = Vec::new();
    let integrity = integrity_code(present_seq);
    for i in 0..TOTAL_BLOCKS {
        let bit = if i < SYNC_BITS {
            (integrity >> (SYNC_BITS - 1 - i)) & 1 == 1
        } else {
            let p = i - SYNC_BITS;
            (present_seq >> (PAYLOAD_BITS - 1 - p)) & 1 == 1
        };
        let left = origin_x + i * BLOCK_PX;
        let rect = Rect {
            left,
            top: origin_y,
            right: left + BLOCK_PX,
            bottom: origin_y + BLOCK_PX,
        };
        if bit {
            white.push(rect);
        } else {
            black.push(rect);
        }
    }
    WatermarkRects { white, black }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("watermark sample count is not 24")]
    WrongSampleCount,
    #[error("watermark payload CRC8 does not match")]
    IntegrityMismatch,
    #[error("watermark contains a block inside the ambiguity band")]
    AmbiguousBlock,
}

pub fn decode(block_means: &[u8], margin: u8) -> Result<u16, DecodeError> {
    if block_means.len() != TOTAL_BLOCKS as usize {
        return Err(DecodeError::WrongSampleCount);
    }
    let mut bits = [false; TOTAL_BLOCKS as usize];
    for (i, &mean) in block_means.iter().enumerate() {
        let lo = 128u16.saturating_sub(u16::from(margin));
        let hi = 128u16 + u16::from(margin);
        let mean = u16::from(mean);
        if mean > lo && mean < hi {
            return Err(DecodeError::AmbiguousBlock);
        }
        bits[i] = mean >= hi;
    }
    let mut integrity = 0u8;
    for &bit in bits.iter().take(SYNC_BITS as usize) {
        integrity = (integrity << 1) | u8::from(bit);
    }
    let mut seq = 0u16;
    for &bit in bits.iter().skip(SYNC_BITS as usize) {
        seq = (seq << 1) | u16::from(bit);
    }
    if integrity != integrity_code(seq) {
        return Err(DecodeError::IntegrityMismatch);
    }
    Ok(seq)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BgraRoiError {
    #[error("scaled watermark ROI dimensions {width}x{height} are too small")]
    DimensionsTooSmall { width: usize, height: usize },
    #[error("watermark ROI row pitch {actual} is smaller than {minimum}")]
    RowPitchTooSmall { actual: usize, minimum: usize },
    #[error("watermark ROI has {actual} bytes, expected at least {minimum}")]
    BufferTooSmall { actual: usize, minimum: usize },
}

/// Produces one mean per block from the center 4x4 BGRA pixels of a 192x8 ROI.
pub fn block_means_from_bgra(
    roi: &[u8],
    row_pitch: usize,
) -> Result<[u8; TOTAL_BLOCKS as usize], BgraRoiError> {
    scaled_block_means_from_bgra(roi, row_pitch, RECT_W as usize, RECT_H as usize)
}

/// Produces one mean per block after the compositor has scaled the watermark
/// together with the game backbuffer.
///
/// The caller copies the complete scaled watermark ROI. Sampling the center
/// half of every scaled block avoids bilinear filtering at block boundaries;
/// at the native 192x8 geometry this is exactly the historical center 4x4
/// sample.
pub fn scaled_block_means_from_bgra(
    roi: &[u8],
    row_pitch: usize,
    roi_width: usize,
    roi_height: usize,
) -> Result<[u8; TOTAL_BLOCKS as usize], BgraRoiError> {
    if roi_width < TOTAL_BLOCKS as usize || roi_height == 0 {
        return Err(BgraRoiError::DimensionsTooSmall {
            width: roi_width,
            height: roi_height,
        });
    }
    let minimum_pitch = roi_width
        .checked_mul(4)
        .ok_or(BgraRoiError::DimensionsTooSmall {
            width: roi_width,
            height: roi_height,
        })?;
    if row_pitch < minimum_pitch {
        return Err(BgraRoiError::RowPitchTooSmall {
            actual: row_pitch,
            minimum: minimum_pitch,
        });
    }
    let minimum_len =
        row_pitch
            .checked_mul(roi_height)
            .ok_or(BgraRoiError::DimensionsTooSmall {
                width: roi_width,
                height: roi_height,
            })?;
    if roi.len() < minimum_len {
        return Err(BgraRoiError::BufferTooSmall {
            actual: roi.len(),
            minimum: minimum_len,
        });
    }
    let mut means = [0u8; TOTAL_BLOCKS as usize];
    for (block, mean) in means.iter_mut().enumerate() {
        let block_left = block * roi_width / TOTAL_BLOCKS as usize;
        let block_right = (block + 1) * roi_width / TOTAL_BLOCKS as usize;
        let block_width = block_right - block_left;
        let sample_left = block_left + block_width / 4;
        let sample_right = block_right - block_width / 4;
        let sample_top = roi_height / 4;
        let sample_bottom = roi_height - roi_height / 4;
        let mut sum = 0u64;
        let mut samples = 0u64;
        for y in sample_top..sample_bottom {
            for x in sample_left..sample_right {
                let pixel = y * row_pitch + x * 4;
                sum += u64::from(roi[pixel]);
                sum += u64::from(roi[pixel + 1]);
                sum += u64::from(roi[pixel + 2]);
                samples += 3;
            }
        }
        *mean = (sum / samples) as u8;
    }
    Ok(means)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects_to_means(rects: &WatermarkRects, origin_x: u32) -> Vec<u8> {
        let mut means = vec![0u8; TOTAL_BLOCKS as usize];
        for rect in &rects.white {
            means[((rect.left - origin_x) / BLOCK_PX) as usize] = 255;
        }
        means
    }

    #[test]
    fn geometry_is_192_by_8() {
        assert_eq!((RECT_W, RECT_H, TOTAL_BLOCKS), (192, 8, 24));
    }

    #[test]
    fn all_low16_values_roundtrip() {
        for seq in 0..=u16::MAX {
            let means = rects_to_means(&encode(seq, 0, 0), 0);
            assert_eq!(decode(&means, 32).unwrap(), seq);
        }
    }

    #[test]
    fn every_single_block_bit_flip_is_rejected_by_crc8() {
        let encoded = encode(0x5aa5, 0, 0);
        let original = rects_to_means(&encoded, 0);
        for bit in 0..TOTAL_BLOCKS as usize {
            let mut corrupted = original.clone();
            corrupted[bit] = 255 - corrupted[bit];
            assert_eq!(decode(&corrupted, 32), Err(DecodeError::IntegrityMismatch));
        }
    }

    #[test]
    fn cursor_like_block_obstruction_and_low_contrast_fail_closed() {
        let encoded = encode(42, 0, 0);
        let mut obstructed = rects_to_means(&encoded, 0);
        obstructed[12] = 255 - obstructed[12];
        assert_eq!(decode(&obstructed, 32), Err(DecodeError::IntegrityMismatch));

        let mut low_contrast = rects_to_means(&encoded, 0);
        low_contrast[4] = 128;
        assert_eq!(decode(&low_contrast, 32), Err(DecodeError::AmbiguousBlock));
    }

    #[test]
    fn bgra_roi_sampling_ignores_row_padding() {
        let row_pitch = RECT_W as usize * 4 + 32;
        let encoded = encode(0xa55a, 0, 0);
        let mut roi = vec![77u8; row_pitch * RECT_H as usize];
        for y in 0..RECT_H as usize {
            for block in 0..TOTAL_BLOCKS as usize {
                let white = encoded
                    .white
                    .iter()
                    .any(|rect| rect.left as usize / BLOCK_PX as usize == block);
                let value = if white { 240 } else { 10 };
                for x in 0..BLOCK_PX as usize {
                    let pixel = y * row_pitch + (block * BLOCK_PX as usize + x) * 4;
                    roi[pixel..pixel + 3].fill(value);
                    roi[pixel + 3] = 255;
                }
            }
        }
        let means = block_means_from_bgra(&roi, row_pitch).unwrap();
        assert_eq!(decode(&means, 32).unwrap(), 0xa55a);
    }

    #[test]
    fn scaled_bgra_roi_sampling_uses_block_centers() {
        let width = 288usize;
        let height = 12usize;
        let row_pitch = width * 4 + 64;
        let encoded = encode(0x3c5a, 0, 0);
        // Model bilinear ambiguity around every scaled block boundary. Only
        // each block's center half is guaranteed to retain its source value.
        let mut roi = vec![128u8; row_pitch * height];
        for block in 0..TOTAL_BLOCKS as usize {
            let white = encoded
                .white
                .iter()
                .any(|rect| rect.left as usize / BLOCK_PX as usize == block);
            let value = if white { 240 } else { 10 };
            let block_left = block * width / TOTAL_BLOCKS as usize;
            let block_right = (block + 1) * width / TOTAL_BLOCKS as usize;
            let block_width = block_right - block_left;
            for y in height / 4..height - height / 4 {
                for x in block_left + block_width / 4..block_right - block_width / 4 {
                    let pixel = y * row_pitch + x * 4;
                    roi[pixel..pixel + 3].fill(value);
                    roi[pixel + 3] = 255;
                }
            }
        }
        let means = scaled_block_means_from_bgra(&roi, row_pitch, width, height).unwrap();
        assert_eq!(decode(&means, 32).unwrap(), 0x3c5a);
    }
}
