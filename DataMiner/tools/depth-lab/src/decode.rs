//! staging 内存 → 深度值：格式解码、行距处理、降采样。
//!
//! 这是 P1c 回读链路里**唯一不需要 D3D 的部分**，也是最容易出静默错的部分。
//! 三个坑各自都能让数据「看起来有、其实全错」，而且都不会报错：
//!
//! 1. **行距（`RowPitch`）**：`Map` 返回的每行字节数**通常大于** `width × 4`（驱动会对齐）。
//!    按 `width × 4` 平铺读会让每一行都比上一行多偏移一点，整帧像被剪切扭曲——
//!    但值域正常、没有异常、不报错。
//! 2. **位序**：`R24G8_TYPELESS` 的**深度在低 24 位**，stencil 在高 8 位。搞反了会读出
//!    一张几乎全 0（stencil 通常是 0）或阶梯状的图。
//! 3. **降采样方向**：深度不连续处不能做线性插值（[77 号 §5.9](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)），
//!    要取「最近的那个表面」。**但「最近」是 min 还是 max 取决于 Z 约定**——见 [`ZConvention`]。

use serde::{Deserialize, Serialize};

/// staging 缓冲的像素格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepthFormat {
    /// `DXGI_FORMAT_R24G8_TYPELESS` / `D24_UNORM_S8_UINT`。
    ///
    /// 32 bit/px：**深度在低 24 位**（R 分量），stencil 在高 8 位（G 分量）。
    /// DXGI 的打包格式里先命名的分量占低位——这也是为什么读深度用
    /// `R24_UNORM_X8_TYPELESS`、读 stencil 用 `X24_TYPELESS_G8_UINT`。
    ///
    /// BioShock Infinite 主场景实测就是这个格式
    /// （[#425](https://github.com/DiogenesModel/Diogenes/pull/425)）。
    #[serde(rename = "d24s8")]
    D24S8,
    /// `DXGI_FORMAT_R32_TYPELESS` / `D32_FLOAT`。32 bit/px 的 f32 深度。
    D32Float,
    /// `DXGI_FORMAT_R32G8X24_TYPELESS` / `D32_FLOAT_S8X24_UINT`。
    ///
    /// 64 bit/px：低 32 位是 f32 深度，高 32 位包含 8-bit stencil 与 24-bit padding。
    /// ETS2 build 23966373 的主场景 DSV 实测使用这个格式。
    D32FloatS8x24,
}

impl DepthFormat {
    #[must_use]
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::D24S8 | Self::D32Float => 4,
            Self::D32FloatS8x24 => 8,
        }
    }
}

/// Z 约定。**决定「最近的表面」是 min 还是 max**。
///
/// - [`Traditional`](Self::Traditional)：`d` 随距离增大（近 0 远 1）→ 最近 = **min**
/// - [`Reversed`](Self::Reversed)：`d = near/z`，随距离减小（近 1 远 0）→ 最近 = **max**
///
/// 77 号 §5.9 写的是「min-pooling（保守取近处）」——**那句话只对传统 Z 成立**。
/// BioShock Infinite（2013 UE3）是传统 Z；WUCHANG（UE5）等现代引擎普遍用 reversed-Z，
/// 那里必须 max-pool。搞反的后果是降采样时**系统性地选中最远的那个表面**，
/// 在物体边缘处把前景吃掉——而画面整体看起来仍然「像深度图」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZConvention {
    Traditional,
    Reversed,
}

impl ZConvention {
    /// 两个深度值里哪个离相机更近。NaN 视为无效，被另一个吃掉。
    #[must_use]
    pub fn nearer(self, a: f32, b: f32) -> f32 {
        if a.is_nan() {
            return b;
        }
        if b.is_nan() {
            return a;
        }
        match self {
            Self::Traditional => a.min(b),
            Self::Reversed => a.max(b),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// `RowPitch` 小于一行像素所需的字节数——多半是把 `RowPitch` 和 `DepthPitch` 弄混了。
    RowPitchTooSmall { row_pitch: usize, needed: usize },
    /// 缓冲装不下 `height` 行。
    BufferTooShort { got: usize, needed: usize },
    /// 宽高为零。
    EmptyGeometry,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RowPitchTooSmall { row_pitch, needed } => write!(
                f,
                "RowPitch={row_pitch} 小于一行所需的 {needed} 字节（是不是把 DepthPitch 传进来了？）"
            ),
            Self::BufferTooShort { got, needed } => {
                write!(f, "staging 缓冲只有 {got} 字节，需要 {needed}")
            }
            Self::EmptyGeometry => write!(f, "宽或高为 0"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 单个 D24S8 字的深度分量，归一化到 `[0, 1]`。
#[must_use]
pub fn d24s8_depth(word: u32) -> f32 {
    (word & 0x00FF_FFFF) as f32 / 16_777_215.0
}

/// 单个 D24S8 字的 stencil 分量。
#[must_use]
pub fn d24s8_stencil(word: u32) -> u8 {
    (word >> 24) as u8
}

/// 从 `Map` 出来的 staging 内存解出一整帧深度，按行距逐行走。
///
/// `row_pitch` 直接来自 `D3D11_MAPPED_SUBRESOURCE::RowPitch`（**字节**）。
/// 输出是行优先的 `width × height` 个归一化深度值。
pub fn decode_frame(
    bytes: &[u8],
    width: usize,
    height: usize,
    row_pitch: usize,
    format: DepthFormat,
) -> Result<Vec<f32>, DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::EmptyGeometry);
    }
    let needed_row = width * format.bytes_per_pixel();
    if row_pitch < needed_row {
        return Err(DecodeError::RowPitchTooSmall {
            row_pitch,
            needed: needed_row,
        });
    }
    // 最后一行不需要完整的 row_pitch，只需要 needed_row
    let needed_total = row_pitch * (height - 1) + needed_row;
    if bytes.len() < needed_total {
        return Err(DecodeError::BufferTooShort {
            got: bytes.len(),
            needed: needed_total,
        });
    }

    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = &bytes[y * row_pitch..y * row_pitch + needed_row];
        for x in 0..width {
            let offset = x * format.bytes_per_pixel();
            let w = u32::from_le_bytes([
                row[offset],
                row[offset + 1],
                row[offset + 2],
                row[offset + 3],
            ]);
            out.push(match format {
                DepthFormat::D24S8 => d24s8_depth(w),
                DepthFormat::D32Float | DepthFormat::D32FloatS8x24 => f32::from_bits(w),
            });
        }
    }
    Ok(out)
}

/// 直接把 staging 字节降采样成交付分辨率，避免先分配一张完整的 `f32` 深度图。
///
/// 语义与 `pool_nearest(decode_frame(...))` 完全相同：每个输出像素仍覆盖同一片源区域，
/// 并按 [`ZConvention`] 选择最近表面。不同之处只是只解码这些覆盖区域里的像素，省掉
/// 1920×1080 中间图的约 8 MiB 分配和一次完整内存遍历。
pub fn decode_and_pool_nearest(
    bytes: &[u8],
    src_size: (usize, usize),
    row_pitch: usize,
    format: DepthFormat,
    dst_size: (usize, usize),
    conv: ZConvention,
) -> Result<Vec<f32>, DecodeError> {
    let (src_w, src_h) = src_size;
    let (dst_w, dst_h) = dst_size;
    if src_w == 0 || src_h == 0 {
        return Err(DecodeError::EmptyGeometry);
    }
    let needed_row = src_w * format.bytes_per_pixel();
    if row_pitch < needed_row {
        return Err(DecodeError::RowPitchTooSmall {
            row_pitch,
            needed: needed_row,
        });
    }
    let needed_total = row_pitch * (src_h - 1) + needed_row;
    if bytes.len() < needed_total {
        return Err(DecodeError::BufferTooShort {
            got: bytes.len(),
            needed: needed_total,
        });
    }
    if dst_w == 0 || dst_h == 0 {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(dst_w * dst_h);
    for dy in 0..dst_h {
        let y0 = dy * src_h / dst_h;
        let y1 = (((dy + 1) * src_h).div_ceil(dst_h)).min(src_h).max(y0 + 1);
        for dx in 0..dst_w {
            let x0 = dx * src_w / dst_w;
            let x1 = (((dx + 1) * src_w).div_ceil(dst_w)).min(src_w).max(x0 + 1);
            let mut acc = f32::NAN;
            for y in y0..y1 {
                let row = &bytes[y * row_pitch..y * row_pitch + needed_row];
                for x in x0..x1 {
                    let offset = x * format.bytes_per_pixel();
                    let word = u32::from_le_bytes([
                        row[offset],
                        row[offset + 1],
                        row[offset + 2],
                        row[offset + 3],
                    ]);
                    let value = match format {
                        DepthFormat::D24S8 => d24s8_depth(word),
                        DepthFormat::D32Float | DepthFormat::D32FloatS8x24 => f32::from_bits(word),
                    };
                    acc = conv.nearer(acc, value);
                }
            }
            out.push(acc);
        }
    }
    Ok(out)
}

/// 按「取最近表面」降采样。**不做任何插值**（77 号 §5.9）。
///
/// 支持非整数倍率：1920×1080 → 1280×720 是 1.5×，每个输出像素覆盖 2 或 3 个输入像素，
/// 本函数按实际覆盖区间取最近值，不假设整数倍。
///
/// 为什么不能双线性：深度不连续处做线性插值会**造出现实中不存在的中间深度**，
/// 一个前景边缘和背景之间会凭空长出一圈「半空中的表面」，直接污染边界指标。
#[must_use]
pub fn pool_nearest(
    src: &[f32],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    conv: ZConvention,
) -> Vec<f32> {
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(dst_w * dst_h);
    for dy in 0..dst_h {
        let y0 = dy * src_h / dst_h;
        let y1 = (((dy + 1) * src_h).div_ceil(dst_h)).min(src_h).max(y0 + 1);
        for dx in 0..dst_w {
            let x0 = dx * src_w / dst_w;
            let x1 = (((dx + 1) * src_w).div_ceil(dst_w)).min(src_w).max(x0 + 1);
            let mut acc = f32::NAN;
            for y in y0..y1 {
                for x in x0..x1 {
                    acc = conv.nearer(acc, src[y * src_w + x]);
                }
            }
            out.push(acc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// 融合路径与两步路径必须**逐值相同**。
    ///
    /// 融合的意义只在省掉那张 1920×1080 的中间 f32 图（约 8 MiB/帧）与一次完整遍历，
    /// **不是**换一种降采样语义。一旦这两条路的结果开始分叉，
    /// 交付数据就取决于「当天走的是哪条代码路径」——而那种差异没有任何东西会报警。
    #[test]
    fn the_fused_path_is_bit_identical_to_decode_then_pool() {
        use super::*;
        let (w, h) = (64usize, 36usize);
        let pitch = w * 4 + 64; // 故意带行对齐填充，顺便钉住 RowPitch 处理
        let mut bytes = vec![0u8; pitch * (h - 1) + w * 4];
        for y in 0..h {
            for x in 0..w {
                // 造一点非平凡的结构，别让所有像素同值把差异掩盖掉
                let v = ((x * 37 + y * 91) % 16_777_216) as u32;
                let k = y * pitch + x * 4;
                bytes[k..k + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        for conv in [ZConvention::Traditional, ZConvention::Reversed] {
            for (dw, dh) in [(64, 36), (32, 18), (43, 25)] {
                let full = decode_frame(&bytes, w, h, pitch, DepthFormat::D24S8).unwrap();
                let two_step = pool_nearest(&full, w, h, dw, dh, conv);
                let fused = decode_and_pool_nearest(
                    &bytes,
                    (w, h),
                    pitch,
                    DepthFormat::D24S8,
                    (dw, dh),
                    conv,
                )
                .unwrap();
                assert_eq!(fused, two_step, "{conv:?} {dw}x{dh} 两条路不一致");
            }
        }
    }

    #[test]
    fn the_fused_path_rejects_the_same_bad_geometry_as_the_two_step_one() {
        use super::*;
        // 校验必须同样严：融合路径若放松了检查，坏输入会在这条路上静默通过
        let bytes = vec![0u8; 64];
        assert!(
            decode_and_pool_nearest(
                &bytes,
                (0, 4),
                16,
                DepthFormat::D24S8,
                (2, 2),
                ZConvention::Traditional
            )
            .is_err()
        );
        assert!(
            decode_and_pool_nearest(
                &bytes,
                (4, 4),
                4,
                DepthFormat::D24S8,
                (2, 2),
                ZConvention::Traditional
            )
            .is_err()
        );
        assert!(
            decode_and_pool_nearest(
                &[],
                (4, 4),
                16,
                DepthFormat::D24S8,
                (2, 2),
                ZConvention::Traditional
            )
            .is_err()
        );
    }

    use super::*;

    #[test]
    fn d24s8_puts_depth_in_the_low_bits() {
        // 这是本模块最重要的一条：位序搞反会读出一张几乎全 0 的图。
        assert_eq!(d24s8_depth(0x0000_0000), 0.0);
        assert_eq!(d24s8_depth(0x00FF_FFFF), 1.0);
        // stencil 位（高 8 位）不得污染深度
        assert_eq!(d24s8_depth(0xFF00_0000), 0.0);
        assert_eq!(d24s8_depth(0xABFF_FFFF), 1.0);
        // 反过来，stencil 读的是高 8 位
        assert_eq!(d24s8_stencil(0xAB00_0000), 0xAB);
        assert_eq!(d24s8_stencil(0x00FF_FFFF), 0);
    }

    #[test]
    fn row_pitch_larger_than_the_row_is_handled() {
        // GPU 驱动几乎总会把行对齐。按 width*4 平铺读会让每行渐进偏移——
        // 值域正常、不报错、图是斜的。这个用例就是防它。
        let (w, h) = (3usize, 2usize);
        let pitch = 16usize; // 3*4=12，多 4 字节填充
        let mut bytes = vec![0xAAu8; pitch * h];
        // 第 0 行像素值 1,2,3；第 1 行 4,5,6（放在低 24 位）
        for (y, base) in [(0usize, 1u32), (1, 4)] {
            for x in 0..w {
                let v = base + x as u32;
                let off = y * pitch + x * 4;
                bytes[off..off + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        let got = decode_frame(&bytes, w, h, pitch, DepthFormat::D24S8).unwrap();
        let expect: Vec<f32> = (1..=6).map(|v| v as f32 / 16_777_215.0).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn tight_packing_also_works() {
        let (w, h) = (2usize, 2usize);
        let pitch = w * 4;
        let mut bytes = vec![0u8; pitch * h];
        for i in 0..4u32 {
            bytes[i as usize * 4..i as usize * 4 + 4].copy_from_slice(&i.to_le_bytes());
        }
        let got = decode_frame(&bytes, w, h, pitch, DepthFormat::D24S8).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], 0.0);
    }

    #[test]
    fn last_row_does_not_need_full_pitch() {
        // 常见的现实情况：驱动给的缓冲长度只到最后一行的有效字节
        let (w, h, pitch) = (2usize, 2usize, 16usize);
        let bytes = vec![0u8; pitch * (h - 1) + w * 4];
        assert!(decode_frame(&bytes, w, h, pitch, DepthFormat::D24S8).is_ok());
    }

    #[test]
    fn a_pitch_smaller_than_a_row_is_rejected_loudly() {
        // 把 DepthPitch 当 RowPitch 传是个真实会犯的错
        let bytes = vec![0u8; 1024];
        let err = decode_frame(&bytes, 100, 2, 8, DepthFormat::D24S8).unwrap_err();
        assert!(matches!(err, DecodeError::RowPitchTooSmall { .. }));
    }

    #[test]
    fn a_short_buffer_is_rejected_instead_of_read_out_of_bounds() {
        let bytes = vec![0u8; 8];
        let err = decode_frame(&bytes, 100, 100, 400, DepthFormat::D24S8).unwrap_err();
        assert!(matches!(err, DecodeError::BufferTooShort { .. }));
    }

    #[test]
    fn d32_float_round_trips() {
        let vals = [0.0f32, 0.25, 1.0, 0.987];
        let mut bytes = Vec::new();
        for v in vals {
            bytes.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        let got = decode_frame(&bytes, 4, 1, 16, DepthFormat::D32Float).unwrap();
        assert_eq!(got, vals);
    }

    #[test]
    fn d32_float_s8x24_reads_depth_and_ignores_stencil_padding() {
        let vals = [0.0f32, 0.25, 1.0, 0.987];
        let mut bytes = Vec::new();
        for (index, value) in vals.into_iter().enumerate() {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
            bytes.extend_from_slice(&(0xA500_0000u32 | index as u32).to_le_bytes());
        }
        let got = decode_frame(&bytes, 4, 1, 32, DepthFormat::D32FloatS8x24).unwrap();
        assert_eq!(got, vals);
    }

    #[test]
    fn pooling_direction_depends_on_the_z_convention() {
        // 这条是本模块的第二个重点。77 号 §5.9 写的「min-pooling」只对传统 Z 成立。
        let src = vec![0.2f32, 0.9, 0.9, 0.9]; // 2×2，左上角有个近处前景（传统 Z 下 0.2 更近）
        let trad = pool_nearest(&src, 2, 2, 1, 1, ZConvention::Traditional);
        assert_eq!(trad, vec![0.2], "传统 Z 下最近 = min");

        // reversed-Z：d = near/z，近处 d 大。同一组数里 0.9 才是近处。
        let rev = pool_nearest(&src, 2, 2, 1, 1, ZConvention::Reversed);
        assert_eq!(rev, vec![0.9], "reversed-Z 下最近 = max");
    }

    #[test]
    fn pooling_never_invents_an_intermediate_depth() {
        // §5.9 的核心要求：深度不连续处不能凭空长出「半空中的表面」
        let src = vec![0.1f32, 0.1, 0.9, 0.9, 0.1, 0.1, 0.9, 0.9];
        let out = pool_nearest(&src, 4, 2, 2, 1, ZConvention::Traditional);
        for v in &out {
            assert!(
                *v == 0.1 || *v == 0.9,
                "输出 {v} 不是任何一个输入值——发生了插值"
            );
        }
    }

    #[test]
    fn handles_the_real_1920x1080_to_1280x720_ratio() {
        // 1.5× 不是整数倍：每个输出像素覆盖 2 或 3 个输入像素
        let (sw, sh) = (1920usize, 1080usize);
        let src = vec![0.5f32; sw * sh];
        let out = pool_nearest(&src, sw, sh, 1280, 720, ZConvention::Traditional);
        assert_eq!(out.len(), 1280 * 720);
        assert!(out.iter().all(|v| (*v - 0.5).abs() < f32::EPSILON));
    }

    #[test]
    fn every_source_pixel_is_covered_by_some_output_pixel() {
        // 覆盖不全 = 有些前景表面永远不会被选中。用「唯一最近值」探测。
        let (sw, sh, dw, dh) = (6usize, 4usize, 4usize, 3usize);
        for probe in 0..sw * sh {
            let mut src = vec![0.9f32; sw * sh];
            src[probe] = 0.1; // 唯一的近处像素
            let out = pool_nearest(&src, sw, sh, dw, dh, ZConvention::Traditional);
            assert!(
                out.iter().any(|v| (*v - 0.1).abs() < f32::EPSILON),
                "源像素 {probe} 没有被任何输出像素覆盖"
            );
        }
    }

    #[test]
    fn nan_is_absorbed_rather_than_poisoning_the_block() {
        // 68 号用 NaN 标记无效像素；一个 NaN 不该把整块吃成 NaN
        let src = vec![f32::NAN, 0.3, 0.7, f32::NAN];
        let out = pool_nearest(&src, 2, 2, 1, 1, ZConvention::Traditional);
        assert_eq!(out, vec![0.3]);
        // 但全是 NaN 时结果仍是 NaN（该块确实没有有效深度）
        let all_nan = vec![f32::NAN; 4];
        assert!(pool_nearest(&all_nan, 2, 2, 1, 1, ZConvention::Traditional)[0].is_nan());
    }

    #[test]
    fn upscaling_is_not_silently_wrong() {
        // 目标比源大时不该 panic、也不该越界；每个输出至少覆盖一个源像素
        let src = vec![0.1f32, 0.9];
        let out = pool_nearest(&src, 2, 1, 4, 1, ZConvention::Traditional);
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|v| *v == 0.1 || *v == 0.9));
    }

    #[test]
    fn decode_then_pool_is_the_real_p1c_path() {
        // 端到端：staging 字节 → 归一化深度 → 720p
        let (w, h, pitch) = (8usize, 4usize, 8 * 4 + 16);
        let mut bytes = vec![0u8; pitch * h];
        for y in 0..h {
            for x in 0..w {
                let word = if x < 4 { 0x0010_0000u32 } else { 0x00E0_0000 };
                let off = y * pitch + x * 4;
                bytes[off..off + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        let depth = decode_frame(&bytes, w, h, pitch, DepthFormat::D24S8).unwrap();
        let small = pool_nearest(&depth, w, h, 4, 2, ZConvention::Traditional);
        assert_eq!(small.len(), 8);
        // 左半应保留近处值，右半保留远处值，且都不是插值出来的第三个数
        let near = 0x0010_0000 as f32 / 16_777_215.0;
        let far = 0x00E0_0000 as f32 / 16_777_215.0;
        assert!(small.iter().all(|v| *v == near || *v == far));
        assert!(small.contains(&near) && small.contains(&far));
    }
}
