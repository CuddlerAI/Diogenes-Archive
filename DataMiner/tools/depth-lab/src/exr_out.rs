//! EXR 交付层：把归一化的深度缓冲值物化成 68 号 §6.4 要求的 EXR。
//!
//! ```text
//! raw/*.f32（归一化 d，720p）
//!    └─ Projection::linearize ─→ 米制 z
//!         ├─ Variant R：EXR FLOAT（fp32），不截断        —— 常驻主档
//!         └─ Variant S：EXR HALF（fp16），trunc(×100)/100 —— 按需再生
//! ```
//!
//! # 编码器是 ZIP，这是已决事项
//!
//! 68 号只写了「EXR（OpenEXR，单通道）」，没指定压缩编码器。
//! owner 2026-08-05 按默认定为 **ZIP（无损）**（[75 号 §2.1](../../../docs/75-sfm-depth-spec-clarification-request.md)）。
//! PXR24 体积更小但**有损**，作为体积敏感时的备选保留，启用前需实测损失。
//!
//! # 为什么这个模块在 feature 后面
//!
//! `dm-depth-probe` 依赖本 crate，但它是**注入进游戏进程的 DLL**——
//! 它只需要 decode / watermark / readback / capture_timing，不该背一个 EXR 编码器。
//! 所以 `exr` 是可选依赖，只有 CLI 的 `materialize` 子命令启用。

use std::path::Path;

// **不要 `use exr::prelude::*`** —— 它导出自己的 `Result<T>` 别名，
// 会把 `std::result::Result` 遮蔽掉，于是本文件里所有 `Result<T, E>` 都报
// 「type alias takes 1 generic argument but 2 were supplied」。
// 这些类型只在 prelude 里公开（各自模块内是私有的），所以从 prelude 具名引入。
use exr::prelude::{
    AnyChannel, AnyChannels, Blocks, Compression, Encoding, FlatSamples, Image, Layer,
    LayerAttributes, LineOrder, WritableImage, f16, read_first_flat_layer_from_file,
};
use half::slice::HalfFloatSliceExt;
use rayon::prelude::*;
use smallvec::smallvec;

use crate::depth::{Projection, Variant, encode};

/// 68 号 §6.4 指定的通道名。**单通道，名字就是 `Z`**。
pub use crate::EXR_CHANNEL_NAME as CHANNEL_NAME;

/// 编码专用 rayon 池的默认线程数。
///
/// exr 1.74 的默认 ZIP16 写入为每张图创建自己的线程池，外层 rayon `install`
/// 不能约束那个池。raw HALF 路径直接在本池生成 ZIP16 chunks，再交 exr 写元数据
/// 和偏移表，因此不会逐帧新建压缩线程。worker 经 [`set_encode_thread_init`]
/// 回调设置 BELOW_NORMAL + EcoQoS；离线 metric variant 仍使用 exr 的参考编码器。
///
/// 2 线程的依据：单线程编码曾在 1080p f32 时代跟不上 Fallout 4，但 GPU 池化
/// 后输入已降到 720p（fp16 平面 1.84 MiB），2 线程对 15 Hz 有 2× 以上吞吐余量；
/// `DM_DEPTH_PROBE_ENCODE_THREADS`（1..=8）可在真机上不改代码地校准。
const DEFAULT_ENCODE_THREADS: usize = 2;

static ENCODE_THREAD_INIT: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();
static ENCODE_POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();

/// 注册编码 worker 线程的初始化回调（每个 worker 启动时执行一次）。
///
/// depth-lab 是跨平台 crate，线程降级的 Windows API 由调用方（探针）注入；
/// 必须在**第一次编码之前**注册——池是惰性建的，建成后回调不再补挂。
pub fn set_encode_thread_init(init: fn()) {
    let _ = ENCODE_THREAD_INIT.set(init);
}

pub(crate) fn encode_pool_threads_from(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(DEFAULT_ENCODE_THREADS, |threads| threads.clamp(1, 8))
}

fn encode_pool() -> Result<&'static rayon::ThreadPool, String> {
    if let Some(pool) = ENCODE_POOL.get() {
        return Ok(pool);
    }
    let threads = encode_pool_threads_from(
        std::env::var("DM_DEPTH_PROBE_ENCODE_THREADS")
            .ok()
            .as_deref(),
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("dm-exr-encode-{index}"))
        .start_handler(|_| {
            if let Some(init) = ENCODE_THREAD_INIT.get() {
                init();
            }
        })
        .build()
        .map_err(|error| format!("建编码线程池失败: {error}"))?;
    let _ = ENCODE_POOL.set(pool);
    Ok(ENCODE_POOL.get().expect("刚设置过编码池"))
}

/// 写一帧**未线性化**的深度缓冲值，fp16 + ZIP（[85 号 §2.2](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md)）。
///
/// 与 [`write_frame`] 的区别只有一个：这里不做 `linearize`，落进 EXR 的就是归一化的
/// 缓冲原始值。probe 在 `(A, B_m)` 标定完成之前只能这么落——但它**不该再落 f32**：
/// f32 一帧 3.6 MiB，2 小时 371 GiB，采不动。
///
/// 精度不是额外让步：fp16 存 `d` 与 fp16 存 `z` 的相对精度同为 2⁻¹¹
/// （`z = B_m/(d−A)` ⇒ `dz = −z·2⁻¹¹`，推导见 85 号 §2.2），而 68 号本来就要 fp16。
///
/// 索引行必须同时记 `domain: "raw_buffer"`——字节形态和米制交付档一模一样，
/// 只有索引能区分它们。
pub fn write_buffer_frame(
    path: &Path,
    depth_buffer: &[f32],
    width: usize,
    height: usize,
) -> Result<(), String> {
    let pixels = width
        .checked_mul(height)
        .filter(|count| *count > 0)
        .ok_or_else(|| "raw depth dimensions are empty or overflow".to_string())?;
    if depth_buffer.len() != pixels {
        return Err(format!(
            "像素数 {} 与 {width}×{height}={} 不符",
            depth_buffer.len(),
            pixels
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建 {} 失败: {e}", dir.display()))?;
    }
    // Dispatch the hardware-supported slice conversion once for the whole plane,
    // instead of repeating scalar conversion/feature dispatch for every pixel.
    let mut samples = vec![f16::ZERO; depth_buffer.len()];
    samples.convert_from_f32_slice(depth_buffer);
    let channel = AnyChannel::new(CHANNEL_NAME, FlatSamples::F16(samples));
    let layer = Layer::new(
        (width, height),
        LayerAttributes::named("depth"),
        Encoding {
            compression: Compression::ZIP16,
            blocks: Blocks::ScanLines,
            line_order: LineOrder::Increasing,
        },
        AnyChannels::sort(smallvec![channel]),
    );
    // ZIP16 and HALF bits are unchanged; the lossless zlib bitstream may differ.
    // Metadata, offset tables, ordering and file-error cleanup still use exr.
    let image = Image::from_layer(layer);
    let headers = image.write().infer_meta_data();
    let FlatSamples::F16(samples) = &image.layer_data.channel_data.list[0].sample_data else {
        unreachable!("raw depth channel is constructed as HALF");
    };
    let samples_per_block = width
        .checked_mul(16)
        .filter(|count| *count > 0)
        .ok_or_else(|| "ZIP16 block width is invalid".to_string())?;
    encode_pool()?.install(|| {
        exr::io::attempt_delete_file_on_write_error(path, |file| {
            exr::block::write(std::io::BufWriter::new(file), headers, true, |_, writer| {
                use exr::block::writer::ChunksWriter;
                // Compressed blocks never exceed their raw HALF byte length.
                // The retained chunk set is therefore bounded by one HALF plane.
                let chunks = samples
                    .par_chunks(samples_per_block)
                    .enumerate()
                    .map(|(index, samples)| {
                        let y = index
                            .checked_mul(16)
                            .and_then(|y| i32::try_from(y).ok())
                            .ok_or_else(|| {
                                exr::error::Error::Invalid("ZIP16 row coordinate overflow".into())
                            })?;
                        compress_half_zip16(samples, y)
                    })
                    .collect::<exr::error::Result<Vec<_>>>()?;
                for (index, chunk) in chunks.into_iter().enumerate() {
                    writer.write_chunk(index, chunk)?;
                }
                Ok(())
            })
        })
        .map_err(|error| format!("写 {} 失败: {error}", path.display()))
    })
}

thread_local! {
    // One encoder per existing pool worker; no additional threads or global pool.
    // libdeflater::fastest() is level 0 (stored blocks), so pin the measured
    // level 1 explicitly rather than silently turning ZIP16 into raw chunks.
    static ZIP16_ENCODER: std::cell::RefCell<libdeflater::Compressor> =
        std::cell::RefCell::new(libdeflater::Compressor::new(libdeflater::CompressionLvl::new(1).expect("fixed ZIP16 compression level")));
}

fn compress_half_zip16(
    samples: &[f16],
    y_coordinate: i32,
) -> exr::error::Result<exr::block::chunk::Chunk> {
    use exr::block::chunk::{Chunk, CompressedBlock, CompressedScanLineBlock};
    let bytes = samples
        .len()
        .checked_mul(2)
        .ok_or_else(|| exr::error::Error::Invalid("ZIP16 byte length overflow".into()))?;
    // OpenEXR ZIP predictor: even bytes, then odd bytes, followed by a wrapping
    // delta with bias 128. One Z HALF channel makes each sample's byte pair explicit.
    let mut packed = vec![0u8; bytes];
    for (index, sample) in samples.iter().enumerate() {
        let [low, high] = sample.to_bits().to_le_bytes();
        packed[index] = low;
        packed[samples.len() + index] = high;
    }
    for index in (1..packed.len()).rev() {
        packed[index] = packed[index]
            .wrapping_sub(packed[index - 1])
            .wrapping_add(128);
    }
    let compressed = ZIP16_ENCODER.with(|encoder| {
        let mut encoder = encoder.borrow_mut();
        // If zlib would require at least the raw length, ZIP16 stores the original
        // interleaved bytes instead. This is the same EXR fallback as the reference.
        let mut output = vec![0; bytes];
        match encoder.zlib_compress(&packed, &mut output) {
            Ok(length) if length < bytes => {
                output.truncate(length);
                Some(output)
            }
            Ok(_) | Err(libdeflater::CompressionError::InsufficientSpace) => None,
        }
    });
    let compressed_pixels_le = compressed.unwrap_or_else(|| {
        samples
            .iter()
            .flat_map(|sample| sample.to_bits().to_le_bytes())
            .collect()
    });
    Ok(Chunk {
        layer_index: 0,
        compressed_block: CompressedBlock::ScanLine(CompressedScanLineBlock {
            y_coordinate,
            compressed_pixels_le,
        }),
    })
}

/// 写一帧 EXR。
///
/// `depth_buffer` 是**归一化的深度缓冲值**（0..1），不是米制——
/// 线性化在这里做，这样 raw 层保持「probe 只存原始值，一切解释离线做」
/// （[77 号 §5.5.5](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)）。
///
/// 无效像素（`linearize` 返回 `None`）写 `NaN`，这是 68 号对无效像素的要求。
pub fn write_frame(
    path: &Path,
    depth_buffer: &[f32],
    width: usize,
    height: usize,
    projection: Projection,
    variant: Variant,
) -> Result<(), String> {
    if depth_buffer.len() != width * height {
        return Err(format!(
            "像素数 {} 与 {width}×{height}={} 不符",
            depth_buffer.len(),
            width * height
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建 {} 失败: {e}", dir.display()))?;
    }

    // 先算好整帧，再交给编码器——闭包里每像素调一次 linearize 会重复做同样的除法，
    // 而 1280×720 一帧要调 92 万次。
    let metric: Vec<f32> = depth_buffer
        .iter()
        .map(|&d| encode(projection.linearize(f64::from(d)), variant))
        .collect();

    let channel = match variant {
        // Variant S 走 HALF（fp16）——68 号 §6.4 指定的精度
        Variant::Spec => AnyChannel::new(
            CHANNEL_NAME,
            FlatSamples::F16(metric.iter().map(|v| f16::from_f32(*v)).collect()),
        ),
        // Variant R 走 FLOAT（fp32）——主档，不截断
        Variant::Reference => AnyChannel::new(CHANNEL_NAME, FlatSamples::F32(metric)),
    };

    let layer = Layer::new(
        (width, height),
        LayerAttributes::named("depth"),
        // ZIP，已决（75 号 §2.1）
        Encoding {
            compression: Compression::ZIP16,
            blocks: Blocks::ScanLines,
            line_order: LineOrder::Increasing,
        },
        AnyChannels::sort(smallvec![channel]),
    );

    encode_pool()?.install(|| {
        Image::from_layer(layer)
            .write()
            .to_file(path)
            .map_err(|e| format!("写 {} 失败: {e}", path.display()))
    })
}

/// 读回一帧 EXR 的单通道数据（供自检与配对器抽验用）。
pub fn read_frame(path: &Path) -> Result<(Vec<f32>, usize, usize), String> {
    let image = read_first_flat_layer_from_file(path)
        .map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let layer = &image.layer_data;
    let (w, h) = (layer.size.width(), layer.size.height());
    let ch = layer
        .channel_data
        .list
        .iter()
        .find(|c| c.name.to_string() == CHANNEL_NAME)
        .ok_or_else(|| format!("{} 里没有名为 {CHANNEL_NAME} 的通道", path.display()))?;
    let values: Vec<f32> = match &ch.sample_data {
        FlatSamples::F16(v) => v.iter().map(|x| x.to_f32()).collect(),
        FlatSamples::F32(v) => v.clone(),
        FlatSamples::U32(v) => v.iter().map(|x| *x as f32).collect(),
    };
    Ok((values, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_half_conversion_preserves_scalar_exr_bytes_at_rounding_boundaries() {
        let dir = tmp("batch-half");
        std::fs::create_dir_all(&dir).unwrap();
        let mut values = Vec::new();
        for bits in 0..=u16::MAX {
            let value = f16::from_bits(bits).to_f32();
            values.push(value);
            // Include values beyond exact half representability, including signed
            // zeros, subnormals, rounding boundaries, infinities and NaN payloads.
            values.push(f32::from_bits(value.to_bits().wrapping_add(4096)));
        }
        let scalar = AnyChannel::new(
            CHANNEL_NAME,
            FlatSamples::F16(values.iter().map(|&value| f16::from_f32(value)).collect()),
        );
        let layer = Layer::new(
            (1024, 128),
            LayerAttributes::named("depth"),
            Encoding {
                compression: Compression::ZIP16,
                blocks: Blocks::ScanLines,
                line_order: LineOrder::Increasing,
            },
            AnyChannels::sort(smallvec![scalar]),
        );
        let scalar_path = dir.join("scalar.exr");
        let mut batch_layer = layer.clone();
        let mut batch_samples = vec![f16::ZERO; values.len()];
        batch_samples.convert_from_f32_slice(&values);
        batch_layer.channel_data.list[0].sample_data = FlatSamples::F16(batch_samples);
        encode_pool()
            .unwrap()
            .install(|| Image::from_layer(layer).write().to_file(&scalar_path))
            .unwrap();
        let batch_path = dir.join("batch.exr");
        encode_pool()
            .unwrap()
            .install(|| Image::from_layer(batch_layer).write().to_file(&batch_path))
            .unwrap();
        assert_eq!(
            std::fs::read(&batch_path).unwrap(),
            std::fs::read(&scalar_path).unwrap()
        );
        let native_path = dir.join("native.exr");
        write_buffer_frame(&native_path, &values, 1024, 128).unwrap();
        assert_same_half_image(&scalar_path, &native_path);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn assert_same_half_image(reference_path: &Path, actual_path: &Path) {
        let reference = read_first_flat_layer_from_file(reference_path).unwrap();
        let actual = read_first_flat_layer_from_file(actual_path).unwrap();
        assert_eq!(actual.attributes, reference.attributes);
        assert_eq!(
            actual.layer_data.attributes,
            reference.layer_data.attributes
        );
        assert_eq!(actual.layer_data.size, reference.layer_data.size);
        assert_eq!(actual.layer_data.encoding, reference.layer_data.encoding);
        let expected = &reference.layer_data.channel_data.list;
        let observed = &actual.layer_data.channel_data.list;
        assert_eq!(observed.len(), expected.len());
        for (actual, reference) in observed.iter().zip(expected) {
            assert_eq!(actual.name, reference.name);
            assert_eq!(actual.sampling, reference.sampling);
            assert_eq!(actual.quantize_linearly, reference.quantize_linearly);
            let (FlatSamples::F16(actual), FlatSamples::F16(reference)) =
                (&actual.sample_data, &reference.sample_data)
            else {
                panic!("raw depth must stay HALF");
            };
            assert_eq!(
                actual
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                reference
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn native_zip16_handles_partial_blocks_and_uncompressed_chunks() {
        let dir = tmp("native-zip16");
        std::fs::create_dir_all(&dir).unwrap();
        for (width, height) in [(1, 1), (3, 17), (31, 33)] {
            let values: Vec<_> = (0..width * height)
                .map(|index| f16::from_bits((index as u16).wrapping_mul(7919)).to_f32())
                .collect();
            let layer = Layer::new(
                (width, height),
                LayerAttributes::named("depth"),
                Encoding {
                    compression: Compression::ZIP16,
                    blocks: Blocks::ScanLines,
                    line_order: LineOrder::Increasing,
                },
                AnyChannels::sort(smallvec![AnyChannel::new(
                    CHANNEL_NAME,
                    FlatSamples::F16(values.iter().map(|&value| f16::from_f32(value)).collect())
                )]),
            );
            let reference = dir.join(format!("reference-{width}-{height}.exr"));
            let actual = dir.join(format!("actual-{width}-{height}.exr"));
            Image::from_layer(layer)
                .write()
                .to_file(&reference)
                .unwrap();
            write_buffer_frame(&actual, &values, width, height).unwrap();
            assert_same_half_image(&reference, &actual);
        }
        let samples = [f16::from_bits(0x1234)];
        let chunk = compress_half_zip16(&samples, 0).unwrap();
        let exr::block::chunk::CompressedBlock::ScanLine(block) = chunk.compressed_block else {
            panic!("scanline expected")
        };
        assert_eq!(block.compressed_pixels_le, [0x34, 0x12]);
        assert!(write_buffer_frame(&dir.join("empty.exr"), &[], 0, 1).is_err());
        assert!(write_buffer_frame(&dir.join("overflow.exr"), &[], usize::MAX, 2).is_err());
        assert!(write_buffer_frame(&dir.join("short.exr"), &[0.0], 2, 2).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_zip16_compresses_repeated_samples_instead_of_storing_raw() {
        let samples = vec![f16::from_bits(0x1234); 1280 * 16];
        let chunk = compress_half_zip16(&samples, 0).unwrap();
        let exr::block::chunk::CompressedBlock::ScanLine(block) = chunk.compressed_block else {
            panic!("scanline expected")
        };
        assert!(
            block.compressed_pixels_le.len() < 1024,
            "compressible HALF plane must use zlib rather than raw fallback"
        );
    }

    #[test]
    fn encode_pool_size_defaults_and_clamps() {
        assert_eq!(encode_pool_threads_from(None), DEFAULT_ENCODE_THREADS);
        assert_eq!(
            encode_pool_threads_from(Some("garbage")),
            DEFAULT_ENCODE_THREADS
        );
        assert_eq!(encode_pool_threads_from(Some("1")), 1);
        assert_eq!(encode_pool_threads_from(Some(" 3 ")), 3);
        assert_eq!(encode_pool_threads_from(Some("0")), 1, "至少 1 线程");
        assert_eq!(
            encode_pool_threads_from(Some("64")),
            8,
            "上限 8，防手滑重回全核"
        );
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dm-exr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// reversed-Z + 无穷远，near=0.1 → z = 0.1/d，方便手算
    fn proj() -> Projection {
        Projection::reversed_z_infinite(0.1, 1.0)
    }

    #[test]
    fn reference_variant_round_trips_metric_depth() {
        let d = tmp("ref");
        let p = d.join("depth_0.exr");
        // d = 0.1/z → z = 1, 2, 5, 10 m
        let buf: Vec<f32> = [1.0f32, 2.0, 5.0, 10.0].iter().map(|z| 0.1 / z).collect();
        write_frame(&p, &buf, 4, 1, proj(), Variant::Reference).unwrap();

        let (back, w, h) = read_frame(&p).unwrap();
        assert_eq!((w, h), (4, 1));
        for (got, want) in back.iter().zip([1.0f32, 2.0, 5.0, 10.0]) {
            assert!((got - want).abs() < 1e-3, "得到 {got}，期望 {want}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn spec_variant_is_half_precision_and_truncated() {
        let d = tmp("spec");
        let p = d.join("depth_0.exr");
        // 3.678 m → 截断到 3.67 → 再落 fp16 网格
        let z = 3.678f32;
        let buf = vec![0.1 / z];
        write_frame(&p, &buf, 1, 1, proj(), Variant::Spec).unwrap();

        let (back, _, _) = read_frame(&p).unwrap();
        // 截断必须发生（不能是 3.678），且落在 3.67 附近
        assert!(
            (back[0] - 3.67).abs() < 0.01,
            "期望截断到 3.67 附近，实得 {}",
            back[0]
        );
        assert!(
            (back[0] - 3.678).abs() > 1e-4,
            "截断没生效，值仍是 {}",
            back[0]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn invalid_pixels_become_nan_not_zero() {
        // 68 号 §6.4：无效像素写 NaN。写成 0 会被下游当成「贴着相机的表面」。
        let d = tmp("nan");
        let p = d.join("depth_0.exr");
        // d = 0 → z = 0.1/0 → 无效
        let buf = vec![0.0f32, 0.1 / 5.0];
        write_frame(&p, &buf, 2, 1, proj(), Variant::Reference).unwrap();

        let (back, _, _) = read_frame(&p).unwrap();
        assert!(back[0].is_nan(), "无效像素应为 NaN，实得 {}", back[0]);
        assert!((back[1] - 5.0).abs() < 1e-3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pixel_count_mismatch_is_refused_before_writing() {
        let d = tmp("mismatch");
        let p = d.join("depth_0.exr");
        let err = write_frame(&p, &[0.5; 3], 4, 1, proj(), Variant::Reference).unwrap_err();
        assert!(err.contains("像素数"));
        assert!(!p.exists(), "拒绝时不该留下文件");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_channel_is_named_z_as_the_spec_requires() {
        let d = tmp("chan");
        let p = d.join("depth_0.exr");
        write_frame(&p, &[0.02f32], 1, 1, proj(), Variant::Reference).unwrap();
        // read_frame 按名字找 Z，找不到会报错——能读回来就说明名字对
        assert!(read_frame(&p).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn writing_is_deterministic_for_the_recipe_hash() {
        // R-23：Variant S 不常驻、按配方再生，前提是同输入两次生成字节一致
        let d = tmp("determ");
        let buf: Vec<f32> = (1..=64).map(|i| 0.1 / (i as f32 * 0.5)).collect();
        let a = d.join("a.exr");
        let b = d.join("b.exr");
        write_frame(&a, &buf, 8, 8, proj(), Variant::Spec).unwrap();
        write_frame(&b, &buf, 8, 8, proj(), Variant::Spec).unwrap();
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "同一输入两次编码应逐字节一致，否则配方+哈希的再生方案不成立"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_realistic_720p_frame_encodes() {
        // 交付分辨率的实际尺寸，确认没有维度假设写死
        let d = tmp("720p");
        let p = d.join("depth_0.exr");
        let (w, h) = (1280usize, 720usize);
        let buf: Vec<f32> = (0..w * h).map(|i| 0.1 / (1.0 + (i % 100) as f32)).collect();
        write_frame(&p, &buf, w, h, proj(), Variant::Reference).unwrap();
        let (back, bw, bh) = read_frame(&p).unwrap();
        assert_eq!((bw, bh), (w, h));
        assert_eq!(back.len(), w * h);
        let _ = std::fs::remove_dir_all(&d);
    }
}
