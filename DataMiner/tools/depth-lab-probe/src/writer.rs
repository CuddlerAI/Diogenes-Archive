//! probe 侧落盘：原始深度帧 + `depth-index.jsonl`。
//!
//! 这一层刻意**只写不判**：格式、几何、归属帧号都由上游给定，
//! writer 唯一的职责是「原子地把它们落到盘上，且索引与文件永远对得上」。
//!
//! # 落的是原始缓冲值，但编码是 fp16 EXR 而不是 f32
//!
//! **「只落原始值、一切解释离线做」这条分工不变**：线性化需要 `(A,B)` 与
//! `metric_scale`，它们来自 P4 的投影矩阵，采集时未必已经标定好；先落归一化原始值，
//! 参数定了随时重新物化，不必重采（77 号 §5.5.5）。交付格式若再变，改的是离线一步。
//!
//! **变的只有数值精度与容器**（[85 号 §2.2](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md)）。
//! 原来落 f32：一帧 3.6 MiB，20 分钟 61 GiB，2 小时 371 GiB——**采不动**。改落 fp16 EXR
//! 之后一帧约 210 KiB，2 小时 21.5 GiB。
//!
//! # 但 fp16 装的是逆深度，不是缓冲原值
//!
//! fp16 保住的是「存进去那个量」的相对精度，不是深度的。传统 Z 的 `d = 1 − n/z`
//! 绝大多数像素挤在 `d → 1` 的一小段里，存原值等于把深度层次扔掉——Fallout 4 实采
//! 一帧只剩 **185 个唯一值**（同场景旧 f32 采集有 547,255 个）。
//!
//! 所以落盘前按 Z 约定做一次可逆变换（[94 号](../../../docs/94-depth-buffer-encoding-decision.md)）：
//! 传统 Z 存 `1 − d ∝ n/z`，reversed-Z 的 `d` 本身已是逆深度、原样存。两条路
//! 都落在「存逆深度」这一个原则上，相对精度 `dz/z = 2⁻¹¹`，
//! **与 68 号 §6.4 交付档要求的 fp16(`z`) 逐位同阶**。
//!
//! 做过哪种变换由索引里的 `storage.transform` 逐行显式携带——**不许靠 Z 约定
//! 反推**：读方拿到的可能是一份 profile 缺失、约定靠数据反推出来的采集，
//! 而反推可能给出 `None`。
//!
//! EXR 编码的成本落在**writer 线程**上，不在 `Present` 钩子里：钩子只做拷贝入队。
//! 编码若慢于帧率，`sync_channel(8)` 可能被填满；渲染侧使用 `try_send`，会显式
//! 丢弃并锁存故障，**不会阻塞 Present**。`write_frame` 仍记录编码耗时，因为 writer
//! 的 CPU / 磁盘竞争可能间接放大尾延迟（85 号 R4）。
//!
//! # 落盘顺序不能颠倒
//!
//! 先写数据文件并 `sync`，再追加索引行。反过来的话，进程被强杀时索引里会留下
//! 一条指向不存在文件的记录——而配对器读到它只会报「文件缺失」，
//! 排障时看起来像是 writer 丢了数据，实际是顺序错了。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use depth_lab::capture_timing::{DepthIndexEntry, DsvPassEvidence};
use depth_lab::decode::DepthFormat;
use depth_lab::frame_health::FrameHealthEvidence;

/// 一帧 30 fps 的预算。编码超过它，writer 线程就是瓶颈（85 号 R4）。
const FRAME_BUDGET_MS: f64 = 1000.0 / 30.0;

/// writer 是跨平台的（测试在 mac 上跑），而 probe 的日志通道只有 Windows 才有。
#[cfg(windows)]
fn warn(msg: &str) {
    crate::probe::log(msg);
}
#[cfg(not(windows))]
fn warn(msg: &str) {
    eprintln!("{msg}");
}

pub struct DepthWriter {
    root: PathBuf,
    raw_dir: PathBuf,
    index_path: PathBuf,
    // Open once per writer generation. Rows are still written immediately to
    // the file; this removes one open/close pair from every successful frame.
    index: Mutex<Option<std::fs::File>>,
}

impl DepthWriter {
    /// `root` 下会生成 `raw/` 与 `depth-index.jsonl`。
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        let raw_dir = root.join("raw");
        fs::create_dir_all(&raw_dir)
            .map_err(|e| format!("建目录 {} 失败: {e}", raw_dir.display()))?;
        Ok(Self {
            index_path: root.join("depth-index.jsonl"),
            root,
            raw_dir,
            index: Mutex::new(None),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 落一帧。
    ///
    /// `present_seq` 必须是 [`depth_lab::capture_timing::CaptureDecision::belongs_to_present_seq`]
    /// ——**上一帧**的序号。用错就是整份数据错位一帧。
    #[allow(clippy::too_many_arguments)]
    pub fn write_frame(
        &self,
        present_seq: u64,
        width: u32,
        height: u32,
        format: DepthFormat,
        dsv_id: &str,
        pass: DsvPassEvidence,
        depth: &[f32],
        readback_latency_frames: u64,
        projection: depth_lab::capture_timing::IndexProjection,
        transform: depth_lab::capture_timing::StorageTransform,
    ) -> Result<FrameHealthEvidence, String> {
        let expected = width as usize * height as usize;
        if depth.len() != expected {
            return Err(format!(
                "帧 {present_seq} 的像素数 {} 与 {width}×{height}={expected} 不符",
                depth.len()
            ));
        }

        // 落盘前的逆深度变换。**不就地改**：入参是解码出来的缓冲值，
        // 调用方在同一个 buffer 上还要做 Z 约定反推，那必须看原值。
        // identity 时连拷贝都不做——reversed-Z 采集走的是这条。
        let transformed: Option<Vec<f32>> = match transform {
            depth_lab::capture_timing::StorageTransform::Identity => None,
            other => Some(depth.iter().map(|&d| other.apply(d)).collect()),
        };
        let payload: &[f32] = transformed.as_deref().unwrap_or(depth);
        // Inspect both the decoded f32 source and the exact fp16 grid that is about to be
        // encoded. This runs on the background writer thread, never in Present.
        let convention = projection.reverse_z.map(|reverse_z| {
            if reverse_z {
                depth_lab::ZConvention::Reversed
            } else {
                depth_lab::ZConvention::Traditional
            }
        });
        let health = FrameHealthEvidence::inspect_with_convention(
            depth,
            width as usize,
            transform,
            convention,
        );

        // 固定宽度补零，`ls` 出来天然有序，配对器也不必排序。**不用 68 号的
        // `depth_<帧>` 命名**：那是相对每条 clip 从 0 重编号的交付名，探针只有全局
        // present_seq，叫成那样会被误当作交付文件。
        let name = format!("{present_seq:012}.exr");
        let path = self.raw_dir.join(&name);

        // 先写临时文件再原子改名：被强杀时不会留下半截文件被索引指着
        let tmp = path.with_extension("exr.tmp");
        let started = Instant::now();
        depth_lab::exr_out::write_buffer_frame(&tmp, payload, width as usize, height as usize)?;
        // exr crate 自己关闭文件，但不 fsync；崩溃时我们宁可丢这一帧也不要半截字节
        // 被索引指着，所以显式 sync 之后再改名。
        {
            // Windows 上 `File::open` 只有读权限，对它调用 `sync_all` 会报
            // `Access denied`；这里同步的是刚写完的文件，句柄必须带写权限。
            let f = OpenOptions::new()
                .write(true)
                .open(&tmp)
                .map_err(|e| format!("开 {} 失败: {e}", tmp.display()))?;
            f.sync_all()
                .map_err(|e| format!("sync {} 失败: {e}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).map_err(|e| format!("改名 {} 失败: {e}", path.display()))?;
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        if encode_ms > FRAME_BUDGET_MS {
            warn(&format!(
                "深度编码耗时 {encode_ms:.1} ms 超过单帧预算 {FRAME_BUDGET_MS:.1} ms（seq {present_seq}）——\
                 writer 出现尾延迟；队列满时会主动丢弃并报错，不会阻塞 Present，\
                 但 CPU / 磁盘竞争仍需用 writer-bypass A/B 归因"
            ));
        }

        // 数据落定之后才追加索引 —— 顺序反了会留下指向不存在文件的索引行
        let entry = DepthIndexEntry {
            schema: depth_lab::capture_timing::INDEX_SCHEMA_V6.to_string(),
            present_seq,
            // 探针路径上深度就采自这一 present 帧，两者同号；引擎路径低频采样时才分叉。
            source_frame: present_seq,
            depth_name: None,
            width,
            height,
            format,
            dsv_id: dsv_id.to_string(),
            storage: depth_lab::capture_timing::DepthStorage::raw_buffer_exr(transform),
            path: format!("raw/{name}"),
            readback_latency_frames,
            // 逐帧写，即使是 `unknown`。**显式的「没有参数」比没有这个键强**——
            // #490 的 61 GiB 就是因为索引里根本看不出缺了什么，
            // 转换阶段才发现，而那时已经无法补救。
            projection,
            health: Some(health.clone()),
            pass: Some(pass),
        };
        let line = serde_json::to_string(&entry).map_err(|e| format!("序列化索引失败: {e}"))?;
        let mut index = self
            .index
            .lock()
            .map_err(|_| "索引 writer lock poisoned".to_string())?;
        if index.is_none() {
            *index = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.index_path)
                    .map_err(|e| format!("打开 {} 失败: {e}", self.index_path.display()))?,
            );
        }
        writeln!(index.as_mut().expect("index initialized"), "{line}")
            .map_err(|e| format!("写索引失败: {e}"))?;
        Ok(health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth_lab::capture_timing::{IndexProjection, IndexProjectionSource, StorageTransform};
    use depth_lab::depth::Projection;

    fn known_projection() -> IndexProjection {
        IndexProjection::known(
            Projection::reversed_z_infinite(0.1, 1.0),
            Some(1.0),
            IndexProjectionSource::Calibrated,
        )
    }

    /// 这些测试只验落盘原子性、命名或像素往返，不对投影参数作断言。
    /// 用显式 `unknown` 表达「本夹具没有提供投影证据」，不伪造可交付参数。
    fn unknown_projection() -> IndexProjection {
        IndexProjection::unknown()
    }

    fn sample_pass() -> DsvPassEvidence {
        DsvPassEvidence {
            unbind_ordinal_from_end: 4,
            unbind_total: 12,
            draw_calls: 3_184,
            selection: depth_lab::DsvSelectionAudit {
                selected_id: "dsv-0001".into(),
                ranked_top_id: Some("dsv-0001".into()),
                score_margin: Some(42.0),
                selected_matches_top: true,
                ranking_confident: true,
                any_msaa: false,
                operator_exact_pin: false,
            },
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dm-depth-writer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn writes_frame_and_index_that_agree() {
        let d = tmpdir("agree");
        let w = DepthWriter::new(&d).unwrap();
        let depth = vec![0.25f32; 4 * 3];
        w.write_frame(
            42,
            4,
            3,
            DepthFormat::D24S8,
            "main-scene-v1",
            sample_pass(),
            &depth,
            2,
            known_projection(),
            StorageTransform::Identity,
        )
        .unwrap();

        let idx = fs::read_to_string(d.join("depth-index.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(idx.trim()).unwrap();
        assert_eq!(row["present_seq"], 42);
        assert_eq!(row["path"], "raw/000000000042.exr");
        assert_eq!(row["schema"], "depth-index/v6");
        assert_eq!(row["storage"]["domain"], "raw_buffer");
        assert_eq!(row["storage"]["encoding"], "exr-half-zip");
        assert_eq!(row["storage"]["transform"], "identity");
        assert_eq!(row["source_frame"], 42);
        assert!(row.get("depth_name").is_none(), "探针层不该编造交付命名");
        assert_eq!(row["buffer_format"], "d24s8");
        assert_eq!(row["projection"]["proj_a"], 0.0);
        assert_eq!(row["projection"]["proj_b"], 0.1);
        assert_eq!(row["projection"]["reverse_z"], true);
        assert_eq!(row["projection"]["metric_scale"], 1.0);
        assert_eq!(row["projection"]["proj_source"], "calibrated");
        assert_eq!(row["projection"]["proj_scope"], "assumed_constant_for_run");
        assert_eq!(row["pass"]["unbind_ordinal_from_end"], 4);
        assert_eq!(row["pass"]["unbind_total"], 12);
        assert_eq!(row["pass"]["draw_calls"], 3_184);
        assert_eq!(row["health"]["schema"], "depth-frame-health/v1");
        assert_eq!(row["health"]["raw"]["pixels"], 12);
        assert_eq!(row["health"]["stored"]["pixels"], 12);
        assert_eq!(row["health"]["defects"][0]["code"], "raw_uniform");

        // 索引指的文件必须真的在，且能读回同样的像素
        let raw = d.join(row["path"].as_str().unwrap());
        let (back, w2, h2) = depth_lab::exr_out::read_frame(&raw).unwrap();
        assert_eq!((w2, h2), (4, 3));
        assert!(
            back.iter().all(|v| (*v - 0.25).abs() < 1e-3),
            "读回的像素与写入不符: {back:?}"
        );
        // 压缩必须真的发生：12 像素的 fp16 EXR 不该比 f32 裸写还大
        assert!(fs::metadata(&raw).unwrap().len() < 4096);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pixel_count_mismatch_is_refused_before_anything_is_written() {
        let d = tmpdir("mismatch");
        let w = DepthWriter::new(&d).unwrap();
        let err = w
            .write_frame(
                1,
                4,
                3,
                DepthFormat::D24S8,
                "m",
                sample_pass(),
                &[0.0; 5],
                0,
                unknown_projection(),
                StorageTransform::Identity,
            )
            .unwrap_err();
        assert!(err.contains("像素数"));
        // 关键：拒绝时不得留下任何痕迹
        assert!(!d.join("depth-index.jsonl").exists());
        assert_eq!(fs::read_dir(d.join("raw")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_temp_files_survive_a_successful_write() {
        let d = tmpdir("notmp");
        let w = DepthWriter::new(&d).unwrap();
        w.write_frame(
            7,
            2,
            2,
            DepthFormat::D32Float,
            "m",
            sample_pass(),
            &[1.0; 4],
            0,
            unknown_projection(),
            StorageTransform::Identity,
        )
        .unwrap();
        let leftovers: Vec<_> = fs::read_dir(d.join("raw"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn frames_are_appended_in_order_and_names_sort_naturally() {
        let d = tmpdir("order");
        let w = DepthWriter::new(&d).unwrap();
        for seq in [0u64, 2, 4, 100, 65536] {
            w.write_frame(
                seq,
                1,
                1,
                DepthFormat::D24S8,
                "m",
                sample_pass(),
                &[0.0],
                0,
                unknown_projection(),
                StorageTransform::Identity,
            )
            .unwrap();
        }
        let idx = fs::read_to_string(d.join("depth-index.jsonl")).unwrap();
        let seqs: Vec<u64> = idx
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["present_seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, vec![0, 2, 4, 100, 65536]);

        // 补零宽度足够，字典序 == 数值序（配对器因此不必排序）
        let mut names: Vec<String> = fs::read_dir(d.join("raw"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.first().unwrap(), "000000000000.exr");
        assert_eq!(names.last().unwrap(), "000000065536.exr");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn round_trips_the_payload_at_half_precision() {
        let d = tmpdir("roundtrip");
        let w = DepthWriter::new(&d).unwrap();
        let depth = vec![0.0f32, 0.5, 1.0, f32::NAN];
        w.write_frame(
            0,
            4,
            1,
            DepthFormat::D32Float,
            "m",
            sample_pass(),
            &depth,
            0,
            unknown_projection(),
            StorageTransform::Identity,
        )
        .unwrap();
        let index = fs::read_to_string(d.join("depth-index.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(index.trim()).unwrap();
        assert_eq!(row["buffer_format"], "d32_float");
        let (back, w2, h2) =
            depth_lab::exr_out::read_frame(&d.join("raw/000000000000.exr")).unwrap();
        assert_eq!((w2, h2), (4, 1));
        // 这三个在 fp16 里都是精确可表示的，所以往返必须逐位相等——
        // 「fp16 有损」不该被用来掩盖一个真实的往返 bug。
        assert_eq!(back[0], 0.0);
        assert_eq!(back[1], 0.5);
        assert_eq!(back[2], 1.0);
        // NaN 是 68 号标记无效像素的方式，过 fp16 也必须还是 NaN
        assert!(back[3].is_nan());
        let _ = fs::remove_dir_all(&d);
    }

    // --- 逆深度落盘（94 号裁决）---

    /// 传统 Z 采集：盘上必须是 `1−d`，索引必须说自己做了什么。
    ///
    /// **这个测试要同时钉住两件事**——只钉字节会漏掉「做了变换但索引没记」，
    /// 只钉索引会漏掉「记了 one_minus_d 但字节没变」。两种失效都会让下游
    /// 得到整体反过来、却看起来完全正常的深度。
    #[test]
    fn a_traditional_z_frame_is_stored_as_inverse_depth_and_says_so() {
        let d = tmpdir("onemind");
        let w = DepthWriter::new(&d).unwrap();
        // 传统 Z 的典型值域：贴着 1 的一小段
        let depth = vec![0.75f32, 0.875, 0.9375, 1.0];
        w.write_frame(
            8,
            4,
            1,
            DepthFormat::D24S8,
            "m",
            sample_pass(),
            &depth,
            0,
            unknown_projection(),
            StorageTransform::OneMinusD,
        )
        .unwrap();

        let row: serde_json::Value = serde_json::from_str(
            fs::read_to_string(d.join("depth-index.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["storage"]["transform"], "one_minus_d");
        assert_eq!(row["health"]["schema"], "depth-frame-health/v1");

        let (stored, _, _) =
            depth_lab::exr_out::read_frame(&d.join("raw/000000000008.exr")).unwrap();
        // 盘上是 1−d，不是 d。这几个值在 fp16 里都精确可表示，所以逐位相等。
        assert_eq!(stored, vec![0.25, 0.125, 0.0625, 0.0]);

        let entry: depth_lab::capture_timing::DepthIndexEntry =
            serde_json::from_value(row).unwrap();
        entry
            .health
            .as_ref()
            .unwrap()
            .validate_stored_values(&stored, 4)
            .unwrap();
        // 索引说的变换必须真能把它还原回缓冲值
        let t = entry.resolve_transform().unwrap();
        let back: Vec<f32> = stored.iter().map(|&v| t.invert(v)).collect();
        assert_eq!(back, depth);
        let _ = fs::remove_dir_all(&d);
    }

    /// reversed-Z 走 identity：`d` 本身已是逆深度，再变换只会把精度挪到远端。
    /// 盘上必须与入参逐位相同。
    #[test]
    fn a_reversed_z_frame_is_stored_untouched() {
        let d = tmpdir("identity");
        let w = DepthWriter::new(&d).unwrap();
        let depth = vec![0.0f32, 0.25, 0.5, 1.0];
        w.write_frame(
            9,
            4,
            1,
            DepthFormat::D32Float,
            "m",
            sample_pass(),
            &depth,
            0,
            unknown_projection(),
            StorageTransform::Identity,
        )
        .unwrap();
        let (stored, _, _) =
            depth_lab::exr_out::read_frame(&d.join("raw/000000000009.exr")).unwrap();
        assert_eq!(stored, depth);
        let _ = fs::remove_dir_all(&d);
    }

    /// 变换不得吃掉无效像素标记。`1.0 - NaN` 恰好也是 NaN，但那是巧合，
    /// 值得钉住——变成 0 会被读成「贴着镜头的表面」。
    #[test]
    fn nan_survives_the_inverse_depth_transform_on_disk() {
        let d = tmpdir("nan-onemind");
        let w = DepthWriter::new(&d).unwrap();
        w.write_frame(
            0,
            2,
            1,
            DepthFormat::D24S8,
            "m",
            sample_pass(),
            &[f32::NAN, 0.5],
            0,
            unknown_projection(),
            StorageTransform::OneMinusD,
        )
        .unwrap();
        let (stored, _, _) =
            depth_lab::exr_out::read_frame(&d.join("raw/000000000000.exr")).unwrap();
        assert!(stored[0].is_nan(), "无效像素被变换吃掉了: {stored:?}");
        assert_eq!(stored[1], 0.5);
        let _ = fs::remove_dir_all(&d);
    }

    /// 写入端不许就地改调用方的 buffer：probe 在同一份数据上还要反推 Z 约定，
    /// 那必须看缓冲原值。就地改会让反推结果静默翻面。
    #[test]
    fn the_callers_buffer_is_not_modified_by_the_transform() {
        let d = tmpdir("nomutate");
        let w = DepthWriter::new(&d).unwrap();
        let depth = vec![0.9f32, 1.0, 0.8, 1.0];
        let before = depth.clone();
        w.write_frame(
            2,
            4,
            1,
            DepthFormat::D24S8,
            "m",
            sample_pass(),
            &depth,
            0,
            unknown_projection(),
            StorageTransform::OneMinusD,
        )
        .unwrap();
        assert_eq!(depth, before, "调用方的缓冲值被就地改掉了");
        let _ = fs::remove_dir_all(&d);
    }
}
