//! 深度落盘格式的体积实测（[85 号 §1](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md)）。
//!
//! ```text
//! cargo run -p depth-lab --features exr-out --example depth_storage_bench -- <帧.f32>... [--width 1280] [--height 720]
//! ```
//!
//! 输入是 probe v1 落的裸 f32 帧。输出是各候选格式的每帧字节数、编码耗时，
//! 以及按 15 fps 折算的 2 小时总量——**这张表是格式裁决的依据，不是插图**，
//! 所以它必须可复现，且必须用真实编码器而不是估算。

use std::path::PathBuf;
use std::time::Instant;

use depth_lab::depth::{Projection, Variant, truncate_two_decimals};
use depth_lab::half::quantize_to_f16;

const FPS: f64 = 15.0;
const TWO_HOURS_FRAMES: f64 = FPS * 3600.0 * 2.0;

fn main() {
    let mut paths: Vec<PathBuf> = Vec::new();
    let (mut width, mut height) = (1280usize, 720usize);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--width" => width = args.next().and_then(|v| v.parse().ok()).unwrap_or(width),
            "--height" => height = args.next().and_then(|v| v.parse().ok()).unwrap_or(height),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        eprintln!(
            "用法: depth_storage_bench <帧.f32>... [--width W] [--height H]\n\
             帧文件是 probe v1 的裸 f32（w·h·4 字节）。"
        );
        std::process::exit(2);
    }

    let n = width * height;
    let raw_bytes = n * 4;
    println!(
        "几何 {width}×{height}，raw f32 = {} KiB/帧\n",
        raw_bytes / 1024
    );
    println!(
        "{:<40} {:>12} {:>11} {:>10} {:>9}",
        "存储形态", "KiB/帧", "2h @15fps", "相对 raw", "编码 ms"
    );

    let tmp = std::env::temp_dir().join("depth-storage-bench");
    let _ = std::fs::create_dir_all(&tmp);

    for path in &paths {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("读 {} 失败: {e}", path.display());
                continue;
            }
        };
        if bytes.len() != raw_bytes {
            eprintln!(
                "{} 是 {} 字节，与 {width}×{height}×4={raw_bytes} 不符——用 --width/--height 指定几何",
                path.display(),
                bytes.len()
            );
            continue;
        }
        let buffer: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        println!("\n── {} ──", path.display());
        row("raw f32（现状，无容器）", raw_bytes, 0.0, raw_bytes);

        // 探针档：fp16 的原始缓冲值，不线性化
        let out = tmp.join("buffer.exr");
        let t = Instant::now();
        depth_lab::exr_out::write_buffer_frame(&out, &buffer, width, height).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        row(
            "fp16(d) 原始缓冲 EXR ZIP【探针档】",
            file_len(&out),
            ms,
            raw_bytes,
        );

        // 交付档与主档：需要一个投影才能线性化。真实 (A,B_m) 未标定，
        // 所以扫几个量程——体积随量程变化本身就是结论的一部分。
        for (bm, label) in [
            (0.1, "z∈0.11–10 m"),
            (0.6, "z∈0.65–60 m"),
            (6.0, "z∈6.5–600 m"),
        ] {
            let projection = Projection::reversed_z_infinite(bm, 1.0);
            let out_s = tmp.join("spec.exr");
            let t = Instant::now();
            depth_lab::exr_out::write_frame(
                &out_s,
                &buffer,
                width,
                height,
                projection,
                Variant::Spec,
            )
            .unwrap();
            let ms_s = t.elapsed().as_secs_f64() * 1000.0;
            row(
                &format!("fp16(z) 截断 EXR ZIP【交付档】{label}"),
                file_len(&out_s),
                ms_s,
                raw_bytes,
            );

            let out_r = tmp.join("ref.exr");
            let t = Instant::now();
            depth_lab::exr_out::write_frame(
                &out_r,
                &buffer,
                width,
                height,
                projection,
                Variant::Reference,
            )
            .unwrap();
            let ms_r = t.elapsed().as_secs_f64() * 1000.0;
            row(
                &format!("fp32(z) 不截断 EXR ZIP【主档】{label}"),
                file_len(&out_r),
                ms_r,
                raw_bytes,
            );
        }

        // 精度自检：交付档在 fp16 下还能不能守住 1 cm，以及守不住的像素占多少。
        let projection = Projection::reversed_z_infinite(0.6, 1.0);
        let (mut valid, mut beyond, mut worst) = (0u64, 0u64, 0f64);
        for &d in &buffer {
            let Some(z) = projection.linearize(f64::from(d)) else {
                continue;
            };
            valid += 1;
            if z >= 16.0 {
                beyond += 1;
            }
            let s = f64::from(quantize_to_f16(truncate_two_decimals(z) as f32));
            worst = worst.max((s - z).abs());
        }
        println!(
            "   精度自检（z∈0.65–60 m 口径）：有效 {valid} px，z ≥ 16 m 占 {:.2}%（这些像素上 1cm 不成立），max|S−R| = {worst:.4} m",
            beyond as f64 / valid.max(1) as f64 * 100.0
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

fn file_len(p: &std::path::Path) -> usize {
    std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0)
}

fn row(label: &str, bytes: usize, ms: f64, raw_bytes: usize) {
    let two_hours = bytes as f64 * TWO_HOURS_FRAMES / (1u64 << 30) as f64;
    println!(
        "{:<40} {:>9.2} KiB {:>8.1} GiB {:>9.1}× {:>9}",
        label,
        bytes as f64 / 1024.0,
        two_hours,
        raw_bytes as f64 / bytes as f64,
        if ms > 0.0 {
            format!("{ms:.1}")
        } else {
            "—".into()
        }
    );
}
