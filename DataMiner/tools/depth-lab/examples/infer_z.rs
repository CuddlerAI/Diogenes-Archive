//! 对真实采集的帧反推 Z 约定与空帧判定，用于核对配置是否填对。
//!
//! ```text
//! cargo run -p depth-lab --example infer_z -- <帧.f32>...
//! ```

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("用法: infer_z <帧.f32>...");
        std::process::exit(2);
    }
    for path in &paths {
        // 按扩展名分派：.exr 走解码器，.f32 是裸块。**不能把 EXR 当裸 f32 读**——
        // 那样读出来的是文件头与压缩块，判定结果全是噪声却不会报错。
        let depth: Vec<f32> = if path.ends_with(".exr") {
            match depth_lab::exr_out::read_frame(std::path::Path::new(path)) {
                Ok((v, _, _)) => v,
                Err(e) => {
                    eprintln!("解 {path} 失败: {e}");
                    continue;
                }
            }
        } else {
            let Ok(bytes) = std::fs::read(path) else {
                eprintln!("读 {path} 失败");
                continue;
            };
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        };
        let content = depth_lab::capture_timing::classify_frame(&depth);
        let conv = depth_lab::capture_timing::infer_z_convention(&depth);
        let zeros = depth.iter().filter(|v| **v == 0.0).count();
        let ones = depth.iter().filter(|v| **v == 1.0).count();
        println!(
            "{:<28} {:<28} z 约定={:<12} 0.0 占 {:>5.2}%  1.0 占 {:>5.2}%",
            path.rsplit('/').next().unwrap_or(path),
            format!("{content:?}"),
            conv.map_or("判不出".into(), |c| format!("{c:?}")),
            zeros as f64 / depth.len() as f64 * 100.0,
            ones as f64 / depth.len() as f64 * 100.0
        );
    }
}
