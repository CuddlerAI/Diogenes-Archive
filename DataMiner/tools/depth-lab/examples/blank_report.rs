//! 把 [`summarize_blank_frames`] 跑在一份真实采集上。
//!
//! # 为什么需要这个 example
//!
//! [#528](https://github.com/DiogenesModel/Diogenes/pull/528) 定了「长连段是游戏状态、孤立帧才是探针缺陷」这条判据，
//! 有测试，但**没有任何调用者**——于是「29.2% 的帧整帧是 1.0」这个数字反复被
//! 引用，却从来没人拿这条判据去分它到底是哪一种。两者的处置完全相反：
//!
//! - 全在长连段里 ⇒ 采集跑过了读盘/菜单，改的是采集流程，代码没问题；
//! - 有孤立帧 ⇒ 快照时机选错，**必须改代码**，重采多少次都一样。
//!
//! 判据存在但跑不了，等于不存在。
//!
//! ```text
//! cargo run --release -p depth-lab --features exr-out --example blank_report -- <采集目录>
//! ```
//!
//! 目录里要有 `depth-index.jsonl` 与它指向的帧文件。**逐帧解码，不抽样**——
//! 孤立帧按定义就是稀疏的，抽样恰好会把它们漏掉，而那正是要找的东西。

use std::path::{Path, PathBuf};

use depth_lab::capture_timing::{
    BlankFrameReport, DepthIndexEntry, FrameContent, classify_frame, summarize_blank_frames,
};

fn main() {
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("用法: blank_report <采集目录>");
        std::process::exit(2);
    };
    let index_path = dir.join("depth-index.jsonl");
    let text = match std::fs::read_to_string(&index_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读 {} 失败: {e}", index_path.display());
            std::process::exit(2);
        }
    };

    let mut frames: Vec<(u64, bool)> = Vec::new();
    let mut clear_values: std::collections::BTreeMap<u32, usize> =
        std::collections::BTreeMap::new();
    let (mut read_errors, mut parse_errors) = (0usize, 0usize);

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: DepthIndexEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => {
                // 采集还在上传时最后一行可能是截断的，不算错
                parse_errors += 1;
                continue;
            }
        };
        // 变换要按索引声明的来还原。空帧判定对可逆变换不敏感（常量映射成常量），
        // 但走同一条解析路径能顺带验出「v4 缺 transform」这类问题。
        let transform = match entry.resolve_transform() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("⚠️ {e}");
                std::process::exit(1);
            }
        };
        let path = dir.join(&entry.path);
        let Some(values) = read_frame(&path) else {
            read_errors += 1;
            continue;
        };
        let buffer: Vec<f32> = values.iter().map(|&v| transform.invert(v)).collect();
        let blank = match classify_frame(&buffer) {
            FrameContent::Blank { clear_value_bits } => {
                *clear_values.entry(clear_value_bits).or_default() += 1;
                true
            }
            FrameContent::HasGeometry => false,
        };
        frames.push((entry.present_seq, blank));
    }

    if frames.is_empty() {
        eprintln!(
            "一帧都没读到（索引 {} 行解析失败、{read_errors} 个文件读失败）",
            parse_errors
        );
        std::process::exit(1);
    }

    let report = summarize_blank_frames(&frames);
    println!("采集目录  {}", dir.display());
    println!("读到帧数  {}", report.total);
    if parse_errors > 0 || read_errors > 0 {
        println!("跳过      索引 {parse_errors} 行解析失败、{read_errors} 个文件读失败");
    }
    println!();
    println!(
        "空帧总数        {:>6}  ({:.1}%)",
        report.blank,
        100.0 * report.ratio()
    );
    println!(
        "  在长连段里    {:>6}  ← 读盘/菜单/过场，不是缺陷",
        report.in_gameplay_gaps
    );
    println!(
        "  孤立/极短     {:>6}  ({:.3}%)  ← **这才是快照时机选错的形态**",
        report.isolated,
        100.0 * report.isolated_ratio()
    );
    println!("最长连续空白    {:>6} 帧", report.longest_gap);
    println!(
        "判据            连段 ≥{} 帧算游戏状态；孤立占比 ≤{:.1}% 且总空白 ≤{:.0}% 才放行",
        BlankFrameReport::MIN_GAMEPLAY_GAP_FRAMES,
        100.0 * BlankFrameReport::MAX_ISOLATED_RATIO,
        100.0 * BlankFrameReport::MAX_TOTAL_BLANK_RATIO,
    );
    if !clear_values.is_empty() {
        let shown: Vec<String> = clear_values
            .iter()
            .map(|(bits, n)| format!("{:.4}×{n}", f32::from_bits(*bits)))
            .collect();
        println!("清除值分布      {}", shown.join("  "));
    }
    println!();

    if report.isolated > 0 {
        let head: Vec<String> = report
            .isolated_seqs
            .iter()
            .take(20)
            .map(u64::to_string)
            .collect();
        println!("孤立空帧的 present_seq（前 20 个）：{}", head.join(", "));
        println!();
    }

    // 结论要直接说下一步做什么——这正是这个 example 存在的理由
    match (report.isolated > 0, report.passes()) {
        (true, _) => {
            println!("❌ 有孤立空帧 ⇒ **快照时机选错，必须改代码**。重采多少次都一样。");
            std::process::exit(1);
        }
        (false, false) => {
            println!(
                "⚠️ 无孤立空帧，但总空白 {:.1}% 超过 {:.0}% 上限 ⇒ 代码没问题，\n\
                 但这份采集的有效时长不够，采集流程要改（别录读盘/菜单）。",
                100.0 * report.ratio(),
                100.0 * BlankFrameReport::MAX_TOTAL_BLANK_RATIO
            );
            std::process::exit(1);
        }
        (false, true) if report.blank == 0 => {
            println!("✅ 一帧空白都没有。");
        }
        (false, true) => {
            println!("✅ 空帧全在长连段里且总量可接受 ⇒ 是游戏状态，不是探针缺陷。");
        }
    }
}

/// 索引里的 `.exr` 与更早的 `.f32` 都要能读——取证经常是在老数据上做的。
fn read_frame(path: &Path) -> Option<Vec<f32>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("exr") => depth_lab::exr_out::read_frame(path).ok().map(|(v, _, _)| v),
        _ => {
            let bytes = std::fs::read(path).ok()?;
            Some(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        }
    }
}
