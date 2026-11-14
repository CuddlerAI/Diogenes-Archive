//! `dm-depth-extract` —— 从 `video.mp4` 量出每帧水印的 24 个块灰度，产出
//! `watermark-samples.jsonl`，供 `depth-lab pair` 消费。
//!
//! ```text
//! dm-depth-extract --video video.mp4 --out watermark-samples.jsonl [--origin-x 0] [--origin-y 0]
//! ```
//!
//! # 为什么这个工具必须是 Windows-only
//!
//! 配对需要知道每个 RGB 帧的 `present_seq`，而它写在画面左上角的水印里——
//! 要读它就得**解 H.264**。仓库里没有解码器：`mp4` crate 只解容器不解码，
//! 而 FFmpeg 被 `AGENTS.md` 明令禁止。剩下的唯一正路是 **Media Foundation**——
//! 它本来就是 DataMiner 编码链路用的那套（[01 §2](../../../docs/01-recording-pipeline.md)），
//! 用它的解码器不引入任何新的依赖面。
//!
//! 判决逻辑本身（同步字、位序、绕回展开）在 [`depth_lab::watermark`] 里，
//! 已在任意平台把 16 位序号空间**全跑一遍**验证过。这里只负责「把像素量出来」。

#![cfg(windows)]

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::core::HSTRING;

use depth_lab::watermark::{BLOCK_PX, RECT_H, RECT_W, TOTAL_BLOCKS};

fn usage() -> &'static str {
    "用法：dm-depth-extract --video <mp4> --out <jsonl> [--origin-x N] [--origin-y N] [--session-frame-offset N] [--png-frames 12,34,56 --png-dir <session>]"
}

/// 取一个块中心 4×4 的平均亮度。
///
/// **取中心而不是整块**：H.264 在块边界最容易出振铃，中心最稳。
/// 这与 77 号 §5.6.1 定的判决方式一致。
fn block_mean(px: &[u8], stride: usize, height: usize, bx: usize, by: usize, flip: bool) -> u8 {
    let mut sum = 0u32;
    let mut n = 0u32;
    for dy in 0..4usize {
        for dx in 0..4usize {
            let x = bx + BLOCK_PX as usize / 2 - 2 + dx;
            let y = by + BLOCK_PX as usize / 2 - 2 + dy;
            // MF 的负 stride 表示自底向上，行序要翻过来
            let row = if flip { height - 1 - y } else { y };
            let off = row * stride + x * 4;
            if off + 3 >= px.len() {
                continue;
            }
            // RGB32 实际是 BGRA；用亮度而不是单通道，避免纯色水印在某通道饱和
            let b = u32::from(px[off]);
            let g = u32::from(px[off + 1]);
            let r = u32::from(px[off + 2]);
            sum += (r * 299 + g * 587 + b * 114) / 1000;
            n += 1;
        }
    }
    sum.checked_div(n).unwrap_or(0) as u8
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_png_frames(value: Option<String>) -> Result<BTreeSet<u64>, String> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    let mut frames = BTreeSet::new();
    for token in value.split(',') {
        let frame = token
            .trim()
            .parse::<u64>()
            .map_err(|error| format!("--png-frames 含非法帧号 {token:?}: {error}"))?;
        if !frames.insert(frame) {
            return Err(format!("--png-frames 重复帧号 {frame}"));
        }
    }
    if frames.is_empty() || frames.len() > 32 {
        return Err("--png-frames 必须包含 1..=32 个唯一帧号".into());
    }
    Ok(frames)
}

fn metric_png_name(sample_index: usize, session_frame: u64) -> String {
    format!("metric-hud-{sample_index:02}-frame-{session_frame:010}.png")
}

fn write_png(
    path: &Path,
    pixels: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    flip: bool,
) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("创建标定截图 {} 失败: {error}", path.display()))?;
    let result = (|| {
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            let source_y = if flip { height - 1 - y } else { y };
            let row = source_y * stride;
            for x in 0..width {
                let offset = row + x * 4;
                if offset + 3 >= pixels.len() {
                    return Err("解码帧 stride/长度不足，拒绝输出截断 PNG".into());
                }
                rgba.extend_from_slice(&[
                    pixels[offset + 2],
                    pixels[offset + 1],
                    pixels[offset],
                    pixels[offset + 3],
                ]);
            }
        }
        let mut encoder = png::Encoder::new(BufWriter::new(file), width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&rgba))
            .map_err(|error| format!("编码标定 PNG {} 失败: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let video = arg(&args, "--video").ok_or_else(|| format!("缺少 --video\n{}", usage()))?;
    let out = arg(&args, "--out").ok_or_else(|| format!("缺少 --out\n{}", usage()))?;
    let ox: usize = arg(&args, "--origin-x")
        .unwrap_or_else(|| "0".into())
        .parse()
        .map_err(|e| format!("--origin-x 不是数字: {e}"))?;
    let oy: usize = arg(&args, "--origin-y")
        .unwrap_or_else(|| "0".into())
        .parse()
        .map_err(|e| format!("--origin-y 不是数字: {e}"))?;
    let session_frame_offset: u64 = arg(&args, "--session-frame-offset")
        .unwrap_or_else(|| "0".into())
        .parse()
        .map_err(|e| format!("--session-frame-offset 不是数字: {e}"))?;
    let png_frames = parse_png_frames(arg(&args, "--png-frames"))?;
    let png_dir = arg(&args, "--png-dir").map(PathBuf::from);
    if png_frames.is_empty() != png_dir.is_none() {
        return Err("--png-frames 与 --png-dir 必须同时提供".into());
    }

    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| format!("MFStartup: {e}"))?;

    let result = extract(
        &video,
        &out,
        ox,
        oy,
        session_frame_offset,
        &png_frames,
        png_dir.as_deref(),
    );
    unsafe {
        let _ = MFShutdown();
    }
    result
}

fn extract(
    video: &str,
    out: &str,
    ox: usize,
    oy: usize,
    session_frame_offset: u64,
    png_frames: &BTreeSet<u64>,
    png_dir: Option<&Path>,
) -> Result<(), String> {
    // Source Reader 默认只公开解码器的原生输出（本机 NVIDIA H.264 解码器是
    // NV12）。请求 RGB32 之前必须显式启用 MF 的视频处理转换器；否则某些
    // 合法 H.264 文件会以 MF_E_INVALIDMEDIATYPE 拒绝 SetCurrentMediaType。
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1) }
        .map_err(|e| format!("MFCreateAttributes: {e}"))?;
    let attributes =
        attributes.ok_or_else(|| "MF 没有返回 Source Reader attributes".to_string())?;
    unsafe {
        attributes
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|e| format!("启用 Source Reader 视频处理失败: {e}"))?;
    }
    let reader: IMFSourceReader =
        unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(video), &attributes) }
            .map_err(|e| format!("打开 {video} 失败: {e}"))?;

    // 让 MF 把 H.264 解成 RGB32（BGRA）。这一步就是「用 MF 的解码器」——
    // 不引入任何新的第三方解码依赖。
    let want = unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType: {e}"))?;
    unsafe {
        want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("设 MAJOR_TYPE: {e}"))?;
        want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| format!("设 SUBTYPE: {e}"))?;
        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &want)
            .map_err(|e| format!("SetCurrentMediaType(RGB32) 失败: {e}"))?;
    }

    let actual =
        unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) }
            .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
    let frame_size = unsafe { actual.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|e| format!("读 FRAME_SIZE: {e}"))?;
    let width = (frame_size >> 32) as usize;
    let height = (frame_size & 0xFFFF_FFFF) as usize;

    if !png_frames.is_empty() && (width != 1920 || height != 1080) {
        return Err(format!(
            "米制标定截图必须来自完整 1920×1080 RGB 帧，实际为 {width}×{height}"
        ));
    }

    if ox + RECT_W as usize > width || oy + RECT_H as usize > height {
        return Err(format!(
            "水印矩形 {RECT_W}×{RECT_H} @({ox},{oy}) 超出画面 {width}×{height}"
        ));
    }

    // 负 stride = 自底向上。拿不到就按紧凑正向处理。
    let declared_stride = unsafe { actual.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|v| v as i32)
        .unwrap_or((width * 4) as i32);
    let flip = declared_stride < 0;
    let stride = declared_stride.unsigned_abs() as usize;

    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|e| format!("建 {out} 失败: {e}"))
        .map(std::io::BufWriter::new)?;
    let mut session_frame = session_frame_offset;
    let mut written_pngs = 0usize;

    loop {
        let mut flags = 0u32;
        let mut sample = None;
        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample 第 {session_frame} 帧失败: {e}"))?;

        const MF_SOURCE_READERF_ENDOFSTREAM: u32 = 0x2;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM != 0 {
            break;
        }
        let Some(sample) = sample else { continue };

        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }
            .map_err(|e| format!("Lock buffer: {e}"))?;

        // SAFETY: Lock 成功后 ptr 指向 len 字节，且只在 Unlock 之前使用。
        let px = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        if png_frames.contains(&session_frame) {
            let directory = png_dir.expect("validated PNG directory exists");
            if !directory.is_dir() {
                unsafe {
                    let _ = buffer.Unlock();
                }
                return Err(format!("PNG 输出目录不存在: {}", directory.display()));
            }
            let sample_index = png_frames
                .iter()
                .position(|frame| *frame == session_frame)
                .expect("selected frame has a stable index");
            let path = directory.join(metric_png_name(sample_index, session_frame));
            if let Err(error) = write_png(&path, px, width, height, stride, flip) {
                unsafe {
                    let _ = buffer.Unlock();
                }
                return Err(error);
            }
            written_pngs += 1;
        }
        let mut means = Vec::with_capacity(TOTAL_BLOCKS as usize);
        for i in 0..TOTAL_BLOCKS as usize {
            means.push(block_mean(
                px,
                stride,
                height,
                ox + i * BLOCK_PX as usize,
                oy,
                flip,
            ));
        }
        unsafe {
            let _ = buffer.Unlock();
        }

        writeln!(
            f,
            "{}",
            serde_json::json!({ "session_frame": session_frame, "block_means": means })
        )
        .map_err(|e| format!("写 {out} 失败: {e}"))?;
        session_frame += 1;
    }

    f.flush().map_err(|e| format!("flush 失败: {e}"))?;
    if written_pngs != png_frames.len() {
        return Err(format!(
            "请求导出 {} 张标定截图，实际只找到 {written_pngs} 张",
            png_frames.len()
        ));
    }
    eprintln!(
        "提取 {} 帧 → {out}（session_frame {}..{}，画面 {width}×{height}，stride {declared_stride}）",
        session_frame - session_frame_offset,
        session_frame_offset,
        session_frame.saturating_sub(1)
    );
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::ExitCode::from(2)
        }
    }
}
