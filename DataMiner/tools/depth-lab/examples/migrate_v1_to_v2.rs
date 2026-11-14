//! 把 v1 采集目录（裸 f32 + 无 schema 的索引）转成 v2（fp16 EXR + 索引 v2）。
//!
//! ```text
//! cargo run --release -p depth-lab --features exr-out --example migrate_v1_to_v2 -- <v1目录> <v2目录>
//! ```
//!
//! 用于 [85 号 R3](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md)：
//! `diogenes-tempbucket` 里已经有 43 GiB 的 v1 数据，重采成本比转换高得多。
//!
//! **转换是有损的**（f32 → fp16），但损失与规格自带的 fp16 完全相同（相对精度 2⁻¹¹），
//! 而这些数据的 domain 是 `raw_buffer`、尚未标定，也就还没有任何交付价值——
//! 现在转和采集时就这么存，结果一样。

use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法: migrate_v1_to_v2 <v1目录> <v2目录>");
        std::process::exit(2);
    }
    let (src, dst) = (PathBuf::from(&args[0]), PathBuf::from(&args[1]));
    match run(&src, &dst) {
        Ok((n, before, after)) => {
            println!(
                "转换 {n} 帧：{:.2} GiB → {:.2} GiB（{:.1}×）",
                before as f64 / (1u64 << 30) as f64,
                after as f64 / (1u64 << 30) as f64,
                before as f64 / after.max(1) as f64
            );
        }
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    }
}

fn run(src: &Path, dst: &Path) -> Result<(usize, u64, u64), String> {
    let index_text = std::fs::read_to_string(src.join("depth-index.jsonl"))
        .map_err(|e| format!("读索引失败: {e}"))?;
    std::fs::create_dir_all(dst.join("raw")).map_err(|e| format!("建目录失败: {e}"))?;

    let mut out_index = String::new();
    let (mut count, mut before, mut after) = (0usize, 0u64, 0u64);

    for line in index_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut row: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("索引行解析失败: {e}"))?;

        // v2 行原样带过——重复跑一次不该把已转好的又转一遍
        if row.get("schema").is_some() {
            out_index.push_str(line);
            out_index.push('\n');
            continue;
        }

        let seq = row["present_seq"].as_u64().ok_or("索引行缺 present_seq")?;
        let width = row["width"].as_u64().ok_or("索引行缺 width")? as usize;
        let height = row["height"].as_u64().ok_or("索引行缺 height")? as usize;
        let rel = row["path"].as_str().ok_or("索引行缺 path")?;

        let f32_path = src.join(rel);
        // 源帧缺失时跳过并继续：一次采集几万帧，为一帧缺失整批失败没有意义，
        // 但**必须报出来**——静默跳过会让转换后的索引与实际帧数悄悄分叉。
        let Ok(bytes) = std::fs::read(&f32_path) else {
            eprintln!("跳过 seq {seq}：读不到 {}", f32_path.display());
            continue;
        };
        if bytes.len() != width * height * 4 {
            eprintln!(
                "跳过 seq {seq}：{} 字节与 {width}×{height}×4 不符",
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

        let name = format!("{seq:012}.exr");
        let out_path = dst.join("raw").join(&name);
        depth_lab::exr_out::write_buffer_frame(&out_path, &buffer, width, height)?;

        before += bytes.len() as u64;
        after += std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
        count += 1;

        // 就地升级索引行：保留来源事实（buffer_format / dsv_id / 延迟），换掉存储描述
        let obj = row.as_object_mut().ok_or("索引行不是对象")?;
        let format = obj.remove("format").unwrap_or(serde_json::Value::Null);
        obj.insert(
            "schema".into(),
            depth_lab::capture_timing::INDEX_SCHEMA_V2.into(),
        );
        obj.insert("source_frame".into(), seq.into());
        obj.insert("buffer_format".into(), format);
        obj.insert(
            "storage".into(),
            serde_json::to_value(depth_lab::capture_timing::DepthStorage::raw_buffer_exr(
                // v1/v2 的 writer 只写原始 `d`，这是可查证的事实、不是默认假设
                depth_lab::capture_timing::StorageTransform::Identity,
            ))
            .map_err(|e| format!("序列化 storage 失败: {e}"))?,
        );
        obj.insert("path".into(), format!("raw/{name}").into());

        out_index.push_str(&serde_json::to_string(&row).map_err(|e| e.to_string())?);
        out_index.push('\n');
    }

    std::fs::write(dst.join("depth-index.jsonl"), out_index)
        .map_err(|e| format!("写索引失败: {e}"))?;
    Ok((count, before, after))
}
