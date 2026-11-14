//! 把一次采集转成 68 号（PR #405 归档）要求的交付形态。
//!
//! 一条交付 = **三件**：`video.mp4`、`depth.zip`、`action_camera.json`
//! （外加每款游戏一份的元信息 excel，那是人工件，不在本模块内）。
//!
//! # 这里为什么可以「缺件仍然产出」
//!
//! 三件的输入来自三条不同的链路：video 与 pose 来自 RGB 采集侧，depth 来自探针。
//! 探针可以单独跑通，于是**「只有 depth」是一种真实且常见的中间状态**。
//! 本模块在这种状态下仍然产出 depth.zip，但：
//!
//! - 交付清单里逐件写明**缺了什么、为什么缺**；
//! - CLI 退出码非零。
//!
//! 反过来的做法——缺件就整体失败——会让人无法验证 depth 这条链路是否已经跑通；
//! 而更糟的做法是安静地产出一个只有 depth 的目录，那个目录看起来就像一条完整交付。
//!
//! # 米制不是可选项
//!
//! 68 号要 View-space linear depth（米）。探针落的是未线性化的缓冲值，
//! 所以转换**必须**给投影参数。没有标定就没有米，本模块不接受「先用个默认值凑」——
//! 那会产出一份看起来完全正常、数值全错的交付。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::depth::{Projection, Variant};
use crate::pairing::{AbsentReason, FramePair, depth_name_for};

/// 一件交付物的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ArtifactState {
    /// 已产出。
    Produced { path: String, bytes: u64 },
    /// 缺输入。`reason` 要能直接告诉人去哪儿找这件东西。
    Missing { reason: String },
}

/// 一条交付的清单。它同时是**再生配方**：投影参数记在这里，
/// 否则「这份 depth 是按什么标定转出来的」就只存在于某次命令行历史里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveryManifest {
    pub clip_id: String,
    pub spec: String,
    pub video: ArtifactState,
    pub depth: ArtifactState,
    pub action_camera: ArtifactState,
    /// depth 的帧数与命名范围。
    pub depth_frames: usize,
    pub depth_first_name: Option<String>,
    pub depth_last_name: Option<String>,
    /// 线性化用的参数。**没有它这份 depth 不可复现，也无法判断米制对不对。**
    pub projection: ProjectionRecord,
    /// 每一件缺失的东西。空数组才是完整交付。
    pub blocking: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionRecord {
    pub a: f64,
    pub b: f64,
    pub metric_scale: f64,
    /// 参数的来源，直接来自逐帧 depth 索引，不接受转换时覆盖。
    pub provenance: String,
}

impl DeliveryManifest {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.blocking.is_empty()
    }
}

// --- 最小 ZIP（stored）写入器 ------------------------------------------------
//
// 不引 zip crate：68 号只要求「压缩为 .zip 包，所有文件保存在单个平铺文件夹内」，
// 而里面装的 EXR 已经是 ZIP 压缩过的——再 deflate 一遍是白花 CPU 换不到体积。
// stored 模式的 ZIP 格式很短，自己写比多一份供应链依赖划算。

const CRC32_POLY: u32 = 0xEDB8_8320;

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                CRC32_POLY ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// 平铺的 stored ZIP。
pub struct FlatZip {
    buffer: Vec<u8>,
    entries: Vec<ZipEntry>,
}

/// 中央目录要重放本地头里的这几个数，所以写入时就记下来。
struct ZipEntry {
    name: String,
    crc: u32,
    size: u32,
    local_header_offset: u32,
}

impl Default for FlatZip {
    fn default() -> Self {
        Self::new()
    }
}

impl FlatZip {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            entries: Vec::new(),
        }
    }

    pub fn add(&mut self, name: &str, data: &[u8]) {
        let offset = self.buffer.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        self.buffer.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header
        self.buffer.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // method = stored
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // mod time
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // mod date
        self.buffer.extend_from_slice(&crc.to_le_bytes());
        self.buffer.extend_from_slice(&size.to_le_bytes());
        self.buffer.extend_from_slice(&size.to_le_bytes());
        self.buffer
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.buffer.extend_from_slice(name.as_bytes());
        self.buffer.extend_from_slice(data);
        self.entries.push(ZipEntry {
            name: name.to_string(),
            crc,
            size,
            local_header_offset: offset,
        });
    }

    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        let central_start = self.buffer.len() as u32;
        for ZipEntry {
            name,
            crc,
            size,
            local_header_offset,
        } in &self.entries
        {
            self.buffer.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            self.buffer.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.buffer.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.buffer.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.buffer.extend_from_slice(&0u16.to_le_bytes()); // stored
            self.buffer.extend_from_slice(&0u16.to_le_bytes());
            self.buffer.extend_from_slice(&0u16.to_le_bytes());
            self.buffer.extend_from_slice(&crc.to_le_bytes());
            self.buffer.extend_from_slice(&size.to_le_bytes());
            self.buffer.extend_from_slice(&size.to_le_bytes());
            self.buffer
                .extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.buffer.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.buffer.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.buffer.extend_from_slice(&0u16.to_le_bytes()); // disk
            self.buffer.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.buffer.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.buffer
                .extend_from_slice(&local_header_offset.to_le_bytes());
            self.buffer.extend_from_slice(name.as_bytes());
        }
        let central_size = self.buffer.len() as u32 - central_start;
        let count = self.entries.len() as u16;
        self.buffer.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // disk
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // central disk
        self.buffer.extend_from_slice(&count.to_le_bytes());
        self.buffer.extend_from_slice(&count.to_le_bytes());
        self.buffer.extend_from_slice(&central_size.to_le_bytes());
        self.buffer.extend_from_slice(&central_start.to_le_bytes());
        self.buffer.extend_from_slice(&0u16.to_le_bytes()); // comment
        self.buffer
    }
}

/// 采集索引里我们需要的那几个字段。
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureRow {
    pub present_seq: u64,
    #[serde(default)]
    pub dsv_id: String,
    pub width: u32,
    pub height: u32,
    pub path: String,
    /// 缺失即 v1。v4 起 `storage.transform` 必填，判定见 [`Self::transform`]。
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub storage: Option<BTreeMap<String, String>>,
    /// v3 起的逐帧投影参数。v2 及更早的行解析成 `unknown`——语义正确，
    /// 那些行确实没有参数。
    #[serde(default = "crate::capture_timing::IndexProjection::unknown")]
    pub projection: crate::capture_timing::IndexProjection,
    /// v5 起必填；deliver 入口会把 stored 统计与实际 EXR 再比一遍。
    #[serde(default)]
    pub health: Option<crate::frame_health::FrameHealthEvidence>,
    /// v6 起必填；记录实际落盘的 DSV pass，而不是 profile 里声明的期望值。
    #[serde(default)]
    pub pass: Option<crate::capture_timing::DsvPassEvidence>,
}

/// 一条已经通过逐帧配对表验证的正式交付 depth。
///
/// `present_seq` 只负责回到 probe 索引找原始深度；对外的文件名和 JSON 帧号一律取
/// `session_frame`。这两套编号在锁帧时可能同步，但不能据此把它们当成同一个时钟。
#[derive(Debug, Clone)]
pub struct DeliveryDepthRow {
    session_frame: u64,
    capture: CaptureRow,
}

impl DeliveryDepthRow {
    #[must_use]
    pub fn session_frame(&self) -> u64 {
        self.session_frame
    }

    #[must_use]
    pub fn capture(&self) -> &CaptureRow {
        &self.capture
    }
}

/// 用配对表把 probe 的 `present_seq` 映射到视频的 `session_frame`，并执行正式交付门禁。
///
/// 原始 depth 可以因为 30 Hz 时间采样而在奇偶 present 相位之间切换；只要水印配对表能
/// 给每个偶数视频帧找到唯一的真实 depth，这种切换就是合法的。连续 RGB observation
/// 也可以精确 alias 同一个 source Present；身份前进后再引用旧 seq、缺帧、继承帧或不连续
/// 的视频帧必须在写 zip 之前失败。
pub fn select_delivery_depth_rows(
    capture_rows: &[CaptureRow],
    pairs: &[FramePair],
) -> Result<Vec<DeliveryDepthRow>, String> {
    if pairs.is_empty() {
        return Err("frame-pairs.jsonl 为空，无法证明 depth 与 RGB 逐帧对齐".into());
    }

    let mut captures_by_seq = BTreeMap::new();
    for row in capture_rows {
        if captures_by_seq.insert(row.present_seq, row).is_some() {
            return Err(format!(
                "depth-index.jsonl 中 present_seq {} 重复，无法确定该用哪一帧",
                row.present_seq
            ));
        }
    }

    let camera_mode = pairs[0].camera_mode;
    let mut used_present_seq = BTreeSet::new();
    let mut selected = Vec::with_capacity(pairs.len().div_ceil(2));

    for (index, pair) in pairs.iter().enumerate() {
        let expected_frame = index as u64;
        if pair.session_frame != expected_frame {
            return Err(format!(
                "frame-pairs.jsonl 的 session_frame 不连续：第 {index} 行是 {}，应为 {expected_frame}",
                pair.session_frame
            ));
        }
        if pair.camera_mode != camera_mode {
            return Err(format!(
                "同一 clip 混入了 {:?} 与 {:?} 两种相机模式，必须分段交付",
                camera_mode, pair.camera_mode
            ));
        }
        if pair.duplicate
            && index
                .checked_sub(1)
                .and_then(|previous| pairs.get(previous))
                .and_then(|previous| previous.present_seq)
                != pair.present_seq
        {
            return Err(format!(
                "session_frame {} 是 duplicate，但没有连续继承同一个 source present identity",
                pair.session_frame
            ));
        }
        if pair.depth.is_some() && pair.depth_absent_reason.is_some() {
            return Err(format!(
                "session_frame {} 同时声明了 depth 和缺失原因，配对表自相矛盾",
                pair.session_frame
            ));
        }

        if pair.session_frame % 2 == 1 {
            if pair.depth.is_some()
                || pair.depth_absent_reason != Some(AbsentReason::OddFrameBySpec)
            {
                return Err(format!(
                    "session_frame {} 是奇数帧，但配对表没有标为 odd_frame_by_spec",
                    pair.session_frame
                ));
            }
            continue;
        }

        let depth = pair.depth.as_ref().ok_or_else(|| {
            format!(
                "session_frame {} 缺少 depth（原因 {:?}），正式交付要求偶数帧全覆盖",
                pair.session_frame, pair.depth_absent_reason
            )
        })?;
        if depth.inherited != pair.duplicate {
            return Err(format!(
                "session_frame {} 的 duplicate/inherited 标记不一致",
                pair.session_frame,
            ));
        }
        let expected_name = depth_name_for(pair.session_frame);
        if depth.depth_name != expected_name {
            return Err(format!(
                "session_frame {} 的 depth_name 是 `{}`，应为 `{expected_name}`",
                pair.session_frame, depth.depth_name
            ));
        }
        if pair.present_seq != Some(depth.present_seq) {
            return Err(format!(
                "session_frame {} 的 RGB present_seq {:?} 与 depth present_seq {} 不一致",
                pair.session_frame, pair.present_seq, depth.present_seq
            ));
        }
        if !used_present_seq.insert(depth.present_seq)
            && index
                .checked_sub(1)
                .and_then(|previous| pairs.get(previous))
                .and_then(|previous| previous.present_seq)
                != Some(depth.present_seq)
        {
            return Err(format!(
                "present_seq {} 在 source identity 已前进后被再次引用",
                depth.present_seq
            ));
        }
        let capture = captures_by_seq.get(&depth.present_seq).ok_or_else(|| {
            format!(
                "frame-pairs.jsonl 引用了 present_seq {}，但 depth-index.jsonl 中不存在",
                depth.present_seq
            )
        })?;
        if capture.dsv_id.trim().is_empty() {
            return Err(format!(
                "present_seq {} 的 depth 索引没有 dsv_id，无法证明配对的是同一张深度缓冲",
                depth.present_seq
            ));
        }
        if capture.dsv_id != depth.dsv_id {
            return Err(format!(
                "present_seq {} 的 DSV 不一致：索引是 `{}`，配对表是 `{}`",
                depth.present_seq, capture.dsv_id, depth.dsv_id
            ));
        }
        selected.push(DeliveryDepthRow {
            session_frame: pair.session_frame,
            capture: (*capture).clone(),
        });
    }

    if selected.is_empty() {
        return Err("frame-pairs.jsonl 没有可交付的偶数帧 depth".into());
    }
    Ok(selected)
}

/// 从已经验证的逐帧索引取得正式线性化参数及其来源。
///
/// 这里不接收命令行 near/far：索引里的 `(A,B)` 才是采集当时真正使用的投影，
/// 事后重建一组“看起来合理”的参数会产出结构完全正确、米制数值整体错误的 EXR。
pub fn delivery_projection(
    rows: &[DeliveryDepthRow],
) -> Result<(Projection, &'static str), String> {
    let row = rows
        .first()
        .ok_or("没有通过配对验证的 depth 帧，无法取得投影参数")?;
    let index = row.capture().projection;
    let (Some(a), Some(b), Some(metric_scale)) = (index.proj_a, index.proj_b, index.metric_scale)
    else {
        return Err("depth 索引缺少 proj_a/proj_b/metric_scale".into());
    };
    if !index.is_metric_resolvable() {
        return Err("depth 索引里的投影参数或来源不可用于米制交付".into());
    }
    for row in &rows[1..] {
        if row.capture().projection != index {
            return Err(format!(
                "session_frame {} 的投影参数或来源与 clip 首帧不同，必须分段交付",
                row.session_frame()
            ));
        }
    }
    Ok((Projection { a, b, metric_scale }, "constant_buffer"))
}

impl CaptureRow {
    /// 盘上编码。缺 `storage` 即 v1 的裸 f32。
    #[must_use]
    pub fn encoding(&self) -> &str {
        self.storage
            .as_ref()
            .and_then(|s| s.get("encoding"))
            .map_or("raw-f32", String::as_str)
    }

    /// 值域。**只有 `raw_buffer` 才需要线性化**；已经是米的不能再线性化一遍。
    #[must_use]
    pub fn domain(&self) -> &str {
        self.storage
            .as_ref()
            .and_then(|s| s.get("domain"))
            .map_or("raw_buffer", String::as_str)
    }

    /// 盘上的字节要怎么还原成缓冲值 `d`（94 号）。
    ///
    /// **这条路径的赌注最大**：它产出的就是交给对方的那份 EXR。存了 `1−d` 却
    /// 忘了还原，线性化出来的深度会整体反过来——近处变远、远处变近——
    /// 而每一帧看起来都完全正常。所以 v4 缺这个字段时宁可整批拒。
    pub fn transform(&self) -> Result<crate::capture_timing::StorageTransform, String> {
        use crate::capture_timing::StorageTransform;
        match self.storage.as_ref().and_then(|s| s.get("transform")) {
            Some(t) if t == "identity" => Ok(StorageTransform::Identity),
            Some(t) if t == "one_minus_d" => Ok(StorageTransform::OneMinusD),
            Some(other) => Err(format!(
                "seq {} 的 storage.transform 是本版本不认识的 `{other}`",
                self.present_seq
            )),
            // v2/v3 的 writer 只写原始 `d`，这是可查证的事实，不是默认假设
            None if !matches!(
                self.schema.as_deref(),
                Some(
                    crate::capture_timing::INDEX_SCHEMA_V4
                        | crate::capture_timing::INDEX_SCHEMA_V5
                        | crate::capture_timing::INDEX_SCHEMA_V6
                )
            ) =>
            {
                Ok(StorageTransform::Identity)
            }
            None => Err(format!(
                "seq {} 是 {} 却没有 storage.transform——该版本必填。\
                 盘上是否做过 1−d 变换无从判断，猜错会让交付的深度整体反过来（94 号）",
                self.present_seq,
                self.schema.as_deref().unwrap_or("unknown")
            )),
        }
    }

    fn health(&self) -> Result<Option<&crate::frame_health::FrameHealthEvidence>, String> {
        if let Some(health) = self.health.as_ref() {
            health.validate().map_err(|error| {
                format!("seq {} 的 frame health 证据无效：{error}", self.present_seq)
            })?;
            return Ok(Some(health));
        }
        if matches!(
            self.schema.as_deref(),
            Some(crate::capture_timing::INDEX_SCHEMA_V5 | crate::capture_timing::INDEX_SCHEMA_V6)
        ) {
            return Err(format!(
                "seq {} 是 {} 却没有 frame health 证据——该版本必填",
                self.present_seq,
                self.schema.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(None)
    }
}

/// 读一帧采集数据（v1 f32 或 v2+ EXR），返回**归一化缓冲值 `d`**。
///
/// 盘上可能存的是 `1−d`（94 号），这里按索引声明的变换还原回去——
/// 于是调用方拿到的永远是 `d`，线性化那一步不必知道变换存在。
pub fn read_capture_frame(capture_dir: &Path, row: &CaptureRow) -> Result<Vec<f32>, String> {
    let transform = row.transform()?;
    let health = row.health()?;
    let stored = read_stored_frame(capture_dir, row)?;
    if let Some(health) = health {
        health.validate_stored_values(&stored, row.width as usize)?;
    }
    Ok(match transform {
        crate::capture_timing::StorageTransform::Identity => stored,
        other => stored.into_iter().map(|v| other.invert(v)).collect(),
    })
}

/// 盘上的原样字节，不做变换还原。只给取证用——交付路径一律走
/// [`read_capture_frame`]。
fn read_stored_frame(capture_dir: &Path, row: &CaptureRow) -> Result<Vec<f32>, String> {
    let path = capture_dir.join(&row.path);
    let expected = row.width as usize * row.height as usize;
    match row.encoding() {
        "exr-half-zip" => {
            let (values, w, h) = crate::exr_out::read_frame(&path)?;
            if w * h != expected {
                return Err(format!(
                    "{} 是 {w}×{h}，索引说 {}×{}",
                    path.display(),
                    row.width,
                    row.height
                ));
            }
            Ok(values)
        }
        "raw-f32" => {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
            if bytes.len() != expected * 4 {
                return Err(format!(
                    "{} 是 {} 字节，与 {}×{}×4 不符",
                    path.display(),
                    bytes.len(),
                    row.width,
                    row.height
                ));
            }
            Ok(bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect())
        }
        other => Err(format!("不认识的盘上编码 `{other}`")),
    }
}

/// 打包好的 depth 交付件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepthPackage {
    pub zip_bytes: Vec<u8>,
    pub frames: usize,
    /// 交付命名的首末。核对 depth.zip 与 action_camera.json 的 `depth_name`
    /// 是否覆盖同一段时，看的就是这两个。
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

/// 把采集的 depth 转成 68 号交付档，打进一个平铺 zip。
pub fn build_depth_zip(
    capture_dir: &Path,
    rows: &[DeliveryDepthRow],
) -> Result<DepthPackage, String> {
    if rows.is_empty() {
        return Err("没有通过配对验证的 depth 帧".into());
    }
    let (projection, _) = delivery_projection(rows)?;
    let mut zip = FlatZip::new();
    let (mut first_name, mut last_name) = (None, None);

    for delivery_row in rows {
        let row = delivery_row.capture();
        let buffer = read_capture_frame(capture_dir, row)?;
        if row.domain() != "raw_buffer" {
            return Err(format!(
                "seq {} 的 domain 是 `{}`，不是 raw_buffer——它已经是米制了，再线性化一次会得到错的数",
                row.present_seq,
                row.domain()
            ));
        }
        let name = depth_name_for(delivery_row.session_frame());
        // 交付层写 Variant S：米制 + trunc(z×100)/100 + fp16 + 单通道 EXR + 无效像素 NaN
        let tmp = std::env::temp_dir().join(format!(
            "dm-deliver-{}-{}.exr",
            std::process::id(),
            row.present_seq
        ));
        crate::exr_out::write_frame(
            &tmp,
            &buffer,
            row.width as usize,
            row.height as usize,
            projection,
            Variant::Spec,
        )?;
        let data = std::fs::read(&tmp).map_err(|e| format!("读回 {} 失败: {e}", tmp.display()))?;
        let _ = std::fs::remove_file(&tmp);
        zip.add(&format!("{name}.exr"), &data);
        if first_name.is_none() {
            first_name = Some(name.clone());
        }
        last_name = Some(name);
    }
    Ok(DepthPackage {
        zip_bytes: zip.finish(),
        frames: rows.len(),
        first_name,
        last_name,
    })
}

/// 读采集索引。
pub fn read_capture_index(capture_dir: &Path) -> Result<Vec<CaptureRow>, String> {
    let path = capture_dir.join("depth-index.jsonl");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str::<CaptureRow>(line)
                .map_err(|e| format!("{}:{} 解析失败: {e}", path.display(), i + 1))?,
        );
    }
    rows.sort_by_key(|r| r.present_seq);
    Ok(rows)
}

/// 采集侧的阻塞项：**在转换之前**就该拦下的东西。
///
/// 正式转换直接使用采集索引里的逐帧 `(A,B,metric_scale)`，不再存在命令行补值或覆盖。
/// 因此这里缺参数或可信来源就意味着**必须重新采集**，已有的字节事后补不回来。
/// [#490](https://github.com/DiogenesModel/Diogenes/issues/490) 的 61 GiB 属于后者，
/// 而当时没有任何一道门看得见它。
#[must_use]
pub fn capture_blockers(rows: &[CaptureRow]) -> Vec<String> {
    let mut out = Vec::new();
    if rows.is_empty() {
        out.push("采集索引为空".into());
        return out;
    }

    // 1) 逐帧投影参数
    let unknown = rows
        .iter()
        .filter(|r| !r.projection.is_metric_resolvable())
        .count();
    if unknown > 0 {
        out.push(format!(
            "{unknown}/{} 帧没有逐帧可证、可线性化成米的投影参数，因此无法线性化并交付（必须是 constant_buffer + \
             observed_this_frame，且 proj_a/proj_b/metric_scale 完整）；run 级配置常量不能证明\
             切视角/开镜后仍有效；当前必须按逐帧观测重采，单视角具结通道落地前不得放行（#575）",
            rows.len()
        ));
    }

    // 2) 参数是否全程恒定。**逐帧记的意义就在这里**——不是「已经证明它会变」，
    //    而是没记就「变没变都无法验证」。
    let mut distinct: Vec<(Option<f64>, Option<f64>, Option<f64>)> = Vec::new();
    for r in rows {
        let key = (
            r.projection.proj_a,
            r.projection.proj_b,
            r.projection.metric_scale,
        );
        if !distinct.contains(&key) {
            distinct.push(key);
        }
    }
    if distinct.len() > 1 {
        out.push(format!(
            "这一段里出现了 {} 组不同的投影/尺度参数——不能按单一 (A,B,scale) 交付，须按参数分段",
            distinct.len()
        ));
    }

    // 3) v6 的 DSV pass 生产事实。profile 里的 `unbind_from_end` 只是人的选择，
    //    只有这里三个逐帧值能让画质/渲染通道漂移在数据中可见（#577）。
    let mut invalid_pass = Vec::new();
    let mut unbind_totals = Vec::new();
    let mut ranking_failures = Vec::new();
    for row in rows {
        match row.pass.as_ref() {
            Some(pass) => {
                if let Err(error) = pass.validate() {
                    invalid_pass.push(format!("seq {}: {error}", row.present_seq));
                }
                if !unbind_totals.contains(&pass.unbind_total) {
                    unbind_totals.push(pass.unbind_total);
                }
                if !pass.selection.is_deliverable() {
                    ranking_failures.push(format!(
                        "seq {}: margin={:?} any_msaa={}",
                        row.present_seq, pass.selection.score_margin, pass.selection.any_msaa
                    ));
                }
            }
            None if row.schema.as_deref() == Some(crate::capture_timing::INDEX_SCHEMA_V6) => {
                invalid_pass.push(format!("seq {}: v6 缺 pass", row.present_seq));
            }
            None => {}
        }
    }
    if !invalid_pass.is_empty() {
        out.push(format!(
            "{} 帧的 DSV pass 证据缺失或无效（首项：{}）",
            invalid_pass.len(),
            invalid_pass[0]
        ));
    }
    if unbind_totals.len() > 1 {
        unbind_totals.sort_unstable();
        out.push(format!(
            "同一段出现不同的 DSV unbind_total {:?}——渲染 pass 数已漂移，\
             `unbind_from_end` 可能指向另一条通道，必须人工复核后重采（#577）",
            unbind_totals
        ));
    }
    if !ranking_failures.is_empty() {
        out.push(format!(
            "{} 帧的 DSV ranking 不可放行（首项：{}）；低 margin 必须人工介入，\
             MSAA 必须先接 typeless 路线（#577）",
            ranking_failures.len(),
            ranking_failures[0]
        ));
    }

    // 4) 落盘变换（[94 号](../../../docs/94-depth-buffer-encoding-decision.md)）。
    //
    //    **这一条必须在这里再挡一遍**，即使 segment_finalize 已经挡过：
    //    deliver 是**另一个入口**（CLI 的交付子命令直接读采集目录），
    //    不保证走过收口。少了它，一份精度已毁的采集照样能打成 zip 发出去。
    let mut broken = Vec::new();
    let mut traditional_identity = 0usize;
    for r in rows {
        match r.transform() {
            Err(e) => broken.push(e),
            // 传统 Z 存缓冲原值 = fp16 之后 20 m 处只剩 0.95 m 分辨率
            Ok(crate::capture_timing::StorageTransform::Identity)
                if r.projection.reverse_z == Some(false) =>
            {
                traditional_identity += 1;
            }
            Ok(_) => {}
        }
    }
    if !broken.is_empty() {
        out.push(format!(
            "{}/{} 帧的 storage.transform 读不出来，第一条：{}",
            broken.len(),
            rows.len(),
            broken[0]
        ));
    }
    if traditional_identity > 0 {
        out.push(format!(
            "{traditional_identity}/{} 帧是传统 Z 却按 identity 落盘：fp16 存 `d` 在 20 m 处\
             只剩 0.95 m 分辨率（94 号）。必须按逆深度重采，重跑转换救不回来",
            rows.len()
        ));
    }

    out
}

/// 采集目录里的 present 帧率裁决（#801）：violated → 采集侧阻塞（必须重采）。
///
/// - 文件不存在 → `Ok(None)`：旧采集/闸未启用，其余闸照常兜底，不追溯旧数据。
/// - 文件存在但解析失败 → `Err`：**响亮失败**。一份写坏的裁决静默放行，
///   等于闸没存在过——这正是 #801 之前「假设裸奔」的形态，不能重演。
/// - `violated=true` → `Ok(Some(阻塞说明))`，与 [`capture_blockers`] 同渠道上报。
pub fn present_rate_blocker(capture_dir: &Path) -> Result<Option<String>, String> {
    let path = capture_dir.join(crate::present_rate::VERDICT_FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读不了 {}: {error}", path.display())),
    };
    let verdict: crate::present_rate::PresentRateVerdict = serde_json::from_str(&text)
        .map_err(|error| format!("解析 {} 失败: {error}", path.display()))?;
    if !verdict.violated {
        return Ok(None);
    }
    Ok(Some(format!(
        "采集期间游戏未锁车道帧率：观测 {:.2} fps vs 车道 {} fps（最大偏差 {:.1}%，容差 {:.0}%，\
         第 {} 个 present 起持续越界）。深度与 15fps 交付网格已错位，重跑转换救不回来，必须重采（#801）",
        verdict.observed_fps_at_worst,
        verdict.lane_fps,
        verdict.worst_deviation_pct,
        verdict.tolerance_pct,
        verdict
            .first_violation_present
            .map_or_else(|| "?".into(), |n| n.to_string()),
    )))
}

/// 交付目录的布局。
#[must_use]
pub fn clip_dir(out_root: &Path, clip_id: &str) -> PathBuf {
    out_root.join(clip_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_timing::{DsvPassEvidence, IndexProjection, IndexProjectionSource};
    use crate::depth::Projection;

    /// present 帧率裁决 → 采集侧阻塞的四种形态（#801）。
    mod present_rate_blocker_gate {
        use super::super::present_rate_blocker;
        use crate::present_rate::{PresentRateVerdict, VERDICT_FILE_NAME, VERDICT_SCHEMA};

        fn dir(tag: &str) -> std::path::PathBuf {
            let d = std::env::temp_dir().join(format!(
                "dm-present-rate-blocker-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn verdict(violated: bool) -> PresentRateVerdict {
            PresentRateVerdict {
                schema: VERDICT_SCHEMA.into(),
                lane_fps: 30,
                tolerance_pct: 4.0,
                presents_observed: 1800,
                violated,
                first_violation_present: violated.then_some(75),
                worst_deviation_pct: if violated { 6.7 } else { 1.2 },
                observed_fps_at_worst: if violated { 32.0 } else { 30.3 },
            }
        }

        #[test]
        fn absent_file_is_not_a_blocker() {
            // 旧采集没有裁决文件：不追溯，其余闸照常兜底。
            let d = dir("absent");
            assert_eq!(present_rate_blocker(&d).unwrap(), None);
        }

        #[test]
        fn clean_verdict_is_not_a_blocker() {
            let d = dir("clean");
            std::fs::write(
                d.join(VERDICT_FILE_NAME),
                serde_json::to_vec(&verdict(false)).unwrap(),
            )
            .unwrap();
            assert_eq!(present_rate_blocker(&d).unwrap(), None);
        }

        #[test]
        fn violated_verdict_blocks_with_recapture_message() {
            let d = dir("violated");
            std::fs::write(
                d.join(VERDICT_FILE_NAME),
                serde_json::to_vec(&verdict(true)).unwrap(),
            )
            .unwrap();
            let msg = present_rate_blocker(&d).unwrap().expect("必须给出阻塞");
            assert!(msg.contains("32.00 fps"), "{msg}");
            assert!(msg.contains("重采"), "{msg}");
        }

        #[test]
        fn corrupt_verdict_fails_loud() {
            // 写坏的裁决静默放行 = 闸不存在。必须响亮失败。
            let d = dir("corrupt");
            std::fs::write(d.join(VERDICT_FILE_NAME), b"{not json").unwrap();
            assert!(present_rate_blocker(&d).is_err());
        }
    }

    fn row(seq: u64, proj: IndexProjection) -> CaptureRow {
        CaptureRow {
            present_seq: seq,
            dsv_id: "main-scene-v1".into(),
            width: 1280,
            height: 720,
            path: format!("raw/{seq:012}.exr"),
            schema: None,
            storage: None,
            projection: proj,
            health: None,
            pass: None,
        }
    }

    fn pair(session_frame: u64, present_seq: Option<u64>) -> FramePair {
        let even = session_frame.is_multiple_of(2);
        FramePair {
            session_frame,
            segment_index: 0,
            segment_frame: session_frame as u32,
            source_qpc: session_frame as i64 * 1_000,
            duplicate: false,
            camera_mode: crate::pairing::CameraMode::Player,
            present_seq,
            depth: present_seq.map(|present_seq| crate::pairing::PairedDepth {
                present_seq,
                depth_name: depth_name_for(session_frame),
                dsv_id: "main-scene-v1".into(),
                inherited: false,
            }),
            depth_absent_reason: (!even).then_some(AbsentReason::OddFrameBySpec),
        }
    }

    fn delivery_row(session_frame: u64, capture: CaptureRow) -> DeliveryDepthRow {
        DeliveryDepthRow {
            session_frame,
            capture,
        }
    }

    /// 交付路径的赌注最大：存了 `1−d` 却忘了还原，出来的深度整体反过来，
    /// 而每一帧看起来都正常。所以 v4 缺 transform 必须拒整批。
    #[test]
    fn a_v4_row_without_a_transform_refuses_to_be_read() {
        let mut r = row(0, resolvable());
        r.schema = Some(crate::capture_timing::INDEX_SCHEMA_V4.into());
        r.storage = Some(BTreeMap::from([
            ("encoding".to_string(), "exr-half-zip".to_string()),
            ("domain".to_string(), "raw_buffer".to_string()),
        ]));
        let err = r.transform().unwrap_err();
        assert!(err.contains("transform"), "{err}");

        // 但 v3 缺它是 identity——那一版的 writer 只写原始 `d`，可查证
        r.schema = Some(crate::capture_timing::INDEX_SCHEMA_V3.into());
        assert_eq!(
            r.transform().unwrap(),
            crate::capture_timing::StorageTransform::Identity
        );
    }

    #[test]
    fn a_v5_row_without_health_is_refused_at_the_direct_deliver_entry() {
        let mut r = row(0, resolvable());
        r.schema = Some(crate::capture_timing::INDEX_SCHEMA_V5.into());
        let error = r.health().unwrap_err();
        assert!(error.contains("health"), "{error}");
        assert!(
            error.contains(crate::capture_timing::INDEX_SCHEMA_V5),
            "{error}"
        );
    }

    /// deliver 是**另一个入口**，不保证走过 segment_finalize 的收口。
    /// 少了这道门，一份精度已毁的采集照样能打成 zip 发出去。
    #[test]
    fn traditional_z_stored_as_raw_d_blocks_delivery_at_this_entry_too() {
        let identity_storage = BTreeMap::from([
            ("encoding".to_string(), "exr-half-zip".to_string()),
            ("domain".to_string(), "raw_buffer".to_string()),
            ("transform".to_string(), "identity".to_string()),
        ]);
        let traditional =
            IndexProjection::observed(Projection::traditional_z(0.2143, 1000.0, 1.0), Some(1.0));
        let rows: Vec<_> = (0..5)
            .map(|i| {
                let mut r = row(i * 2, traditional);
                r.schema = Some(crate::capture_timing::INDEX_SCHEMA_V4.into());
                r.storage = Some(identity_storage.clone());
                r
            })
            .collect();
        let blockers = capture_blockers(&rows);
        assert!(
            blockers
                .iter()
                .any(|b| b.contains("传统 Z") && b.contains("94")),
            "传统 Z + identity 必须挡住：{blockers:?}"
        );

        // 同一批数据改存逆深度就该放行——挡的是编码，不是这份采集本身
        let mut inverse = identity_storage;
        inverse.insert("transform".to_string(), "one_minus_d".to_string());
        let ok: Vec<_> = (0..5)
            .map(|i| {
                let mut r = row(i * 2, traditional);
                r.schema = Some(crate::capture_timing::INDEX_SCHEMA_V4.into());
                r.storage = Some(inverse.clone());
                r
            })
            .collect();
        assert_eq!(capture_blockers(&ok), Vec::<String>::new());
    }

    /// reversed-Z 存 identity 是**对的**，不能连它一起挡——
    /// 那样会把唯一一份已经采对的数据也拒掉。
    #[test]
    fn reversed_z_stored_as_identity_is_not_blocked() {
        let rows: Vec<_> = (0..5)
            .map(|i| {
                let mut r = row(i * 2, resolvable());
                r.storage = Some(BTreeMap::from([(
                    "transform".to_string(),
                    "identity".to_string(),
                )]));
                r
            })
            .collect();
        assert_eq!(capture_blockers(&rows), Vec::<String>::new());
    }

    #[test]
    fn an_unknown_transform_is_refused_rather_than_ignored() {
        let mut r = row(0, resolvable());
        r.storage = Some(BTreeMap::from([(
            "transform".to_string(),
            "log_z".to_string(),
        )]));
        assert!(r.transform().unwrap_err().contains("log_z"));
    }

    /// `read_capture_frame` 的契约是「返回缓冲值 `d`」。盘上存的是 `1−d` 时
    /// 它必须自己还原——下游的线性化按 `d` 写的，多一处知道变换就多一处会漏掉。
    #[test]
    fn reading_a_one_minus_d_frame_returns_buffer_values() {
        let dir = std::env::temp_dir().join(format!("dm-deliver-inv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        // 传统 Z 的缓冲值 0.75，落盘存成 1−0.75 = 0.25
        crate::exr_out::write_buffer_frame(&dir.join("raw/000000000000.exr"), &[0.25; 4], 2, 2)
            .unwrap();

        let mut r = row(0, resolvable());
        r.width = 2;
        r.height = 2;
        r.schema = Some(crate::capture_timing::INDEX_SCHEMA_V4.into());
        r.storage = Some(BTreeMap::from([
            ("encoding".to_string(), "exr-half-zip".to_string()),
            ("domain".to_string(), "raw_buffer".to_string()),
            ("transform".to_string(), "one_minus_d".to_string()),
        ]));
        let values = read_capture_frame(&dir, &r).unwrap();
        assert_eq!(
            values,
            vec![0.75f32; 4],
            "读出来的必须是缓冲值，不是盘上的字节"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn resolvable() -> IndexProjection {
        IndexProjection::observed(Projection::reversed_z_infinite(0.1, 1.0), Some(1.0))
    }

    fn pass(unbind_total: u32) -> DsvPassEvidence {
        DsvPassEvidence {
            unbind_ordinal_from_end: 4,
            unbind_total,
            draw_calls: 3_184,
            selection: crate::dsv::DsvSelectionAudit {
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

    #[test]
    fn a_clean_capture_has_no_blockers() {
        let rows: Vec<_> = (0..10).map(|i| row(i * 2, resolvable())).collect();
        assert_eq!(capture_blockers(&rows), Vec::<String>::new());
    }

    #[test]
    fn a_v6_pass_count_drift_is_visible_and_blocks_delivery() {
        let mut rows: Vec<_> = (0..4).map(|i| row(i * 2, resolvable())).collect();
        for row in &mut rows {
            row.schema = Some(crate::capture_timing::INDEX_SCHEMA_V6.into());
            row.storage = Some(BTreeMap::from([(
                "transform".to_string(),
                "identity".to_string(),
            )]));
            row.pass = Some(pass(12));
        }
        rows[2].pass = Some(pass(11));
        let blockers = capture_blockers(&rows);
        assert!(
            blockers.iter().any(|item| item.contains("unbind_total")),
            "{blockers:?}"
        );
    }

    #[test]
    fn low_margin_and_msaa_ranking_fail_closed_but_a_non_top_operator_pick_is_evidence_only() {
        let mut rows = vec![row(0, resolvable())];
        rows[0].pass = Some(pass(12));
        rows[0]
            .pass
            .as_mut()
            .unwrap()
            .selection
            .selected_matches_top = false;
        assert_eq!(capture_blockers(&rows), Vec::<String>::new());

        rows[0].pass.as_mut().unwrap().selection.ranking_confident = false;
        let blockers = capture_blockers(&rows);
        assert!(blockers.iter().any(|item| item.contains("ranking")));

        rows[0].pass.as_mut().unwrap().selection.ranking_confident = true;
        rows[0].pass.as_mut().unwrap().selection.any_msaa = true;
        let blockers = capture_blockers(&rows);
        assert!(blockers.iter().any(|item| item.contains("MSAA")));
    }

    #[test]
    fn the_61gib_shape_is_caught_before_any_conversion_runs() {
        // #490 的那批：v2 索引、全部 unknown。这是「必须重采」，不是「重跑转换」。
        let rows: Vec<_> = (0..100)
            .map(|i| row(i * 2, IndexProjection::unknown()))
            .collect();
        let b = capture_blockers(&rows);
        assert!(
            b.iter().any(|x| x.contains("无法线性化")),
            "应当拦下没有投影参数的采集：{b:?}"
        );
    }

    #[test]
    fn present_phase_changes_are_valid_when_frame_pairs_stay_contiguous() {
        // 时间采样可能从奇数 present 相位切到偶数相位；正式编号仍由视频帧决定。
        let captures: Vec<_> = [1, 3, 6, 8]
            .into_iter()
            .map(|seq| row(seq, resolvable()))
            .collect();
        let pairs = vec![
            pair(0, Some(1)),
            pair(1, None),
            pair(2, Some(3)),
            pair(3, None),
            pair(4, Some(6)),
            pair(5, None),
            pair(6, Some(8)),
            pair(7, None),
        ];

        let selected = select_delivery_depth_rows(&captures, &pairs).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(DeliveryDepthRow::session_frame)
                .collect::<Vec<_>>(),
            vec![0, 2, 4, 6]
        );
        let selected_capture: Vec<_> = selected.iter().map(|r| r.capture().clone()).collect();
        assert_eq!(capture_blockers(&selected_capture), Vec::<String>::new());
    }

    #[test]
    fn delivery_uses_the_exact_projection_recorded_during_capture() {
        let expected = Projection {
            a: -0.000_125_015_626_953_369,
            b: 0.200_025_003_125_391,
            metric_scale: 1.0,
        };
        let projection = IndexProjection::observed(expected, Some(expected.metric_scale));
        let selected =
            select_delivery_depth_rows(&[row(101, projection)], &[pair(0, Some(101))]).unwrap();
        let (actual, provenance) = delivery_projection(&selected).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(provenance, "constant_buffer");
    }

    #[test]
    fn a_projection_that_changed_mid_run_cannot_be_delivered_as_one_clip() {
        // ETS2 内/外视角 near plane 不同就是这个情形。按单一 (A,B) 交付
        // 会让一半的帧数值全错，而结构完全合规。
        let mut rows: Vec<_> = (0..5).map(|i| row(i * 2, resolvable())).collect();
        let other = IndexProjection::observed(Projection::reversed_z_infinite(0.5, 1.0), Some(1.0));
        rows.extend((5..10).map(|i| row(i * 2, other)));
        let b = capture_blockers(&rows);
        assert!(b.iter().any(|x| x.contains("不同的投影/尺度参数")), "{b:?}");
    }

    #[test]
    fn a_metric_scale_that_changed_mid_run_cannot_be_delivered_as_one_clip() {
        let mut rows: Vec<_> = (0..5).map(|i| row(i * 2, resolvable())).collect();
        let other_scale =
            IndexProjection::observed(Projection::reversed_z_infinite(0.1, 0.02), Some(0.02));
        rows.extend((5..10).map(|i| row(i * 2, other_scale)));
        let b = capture_blockers(&rows);
        assert!(b.iter().any(|x| x.contains("不同的投影/尺度参数")), "{b:?}");
    }

    #[test]
    fn missing_metric_scale_alone_is_enough_to_block() {
        // 有 a/b 没 scale 时，z = b/(d−a)·scale 仍然算不出米。
        let no_scale = IndexProjection::known(
            Projection::reversed_z_infinite(0.1, 1.0),
            None,
            IndexProjectionSource::ConstantBuffer,
        );
        let rows: Vec<_> = (0..10).map(|i| row(i * 2, no_scale)).collect();
        assert!(!capture_blockers(&rows).is_empty());
    }

    #[test]
    fn a_missing_even_frame_is_rejected_before_packaging() {
        let captures = vec![row(101, resolvable())];
        let pairs = vec![pair(0, Some(101)), pair(1, None), pair(2, None)];
        let error = select_delivery_depth_rows(&captures, &pairs).unwrap_err();
        assert!(error.contains("session_frame 2") && error.contains("缺少 depth"));
    }

    #[test]
    fn duplicate_present_mapping_is_rejected_before_packaging() {
        let captures = vec![row(101, resolvable())];
        let pairs = vec![pair(0, Some(101)), pair(1, None), pair(2, Some(101))];
        let error = select_delivery_depth_rows(&captures, &pairs).unwrap_err();
        assert!(error.contains("identity 已前进"), "{error}");
    }

    #[test]
    fn consecutive_source_identity_alias_is_delivered_for_both_even_frames() {
        let captures = vec![row(101, resolvable())];
        let mut odd_alias = pair(1, Some(101));
        odd_alias.depth = None;
        odd_alias.depth_absent_reason = Some(AbsentReason::OddFrameBySpec);
        let selected = select_delivery_depth_rows(
            &captures,
            &[pair(0, Some(101)), odd_alias, pair(2, Some(101))],
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(DeliveryDepthRow::session_frame)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(selected.iter().all(|row| row.capture().present_seq == 101));
    }

    #[test]
    fn duplicate_even_frame_delivers_the_exact_inherited_source_depth() {
        let captures = vec![row(101, resolvable())];
        let mut odd_alias = pair(1, Some(101));
        odd_alias.depth = None;
        odd_alias.depth_absent_reason = Some(AbsentReason::OddFrameBySpec);
        let mut duplicate_even = pair(2, Some(101));
        duplicate_even.duplicate = true;
        duplicate_even.depth.as_mut().unwrap().inherited = true;
        let selected =
            select_delivery_depth_rows(&captures, &[pair(0, Some(101)), odd_alias, duplicate_even])
                .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(DeliveryDepthRow::session_frame)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(selected.iter().all(|row| row.capture().present_seq == 101));
    }

    #[test]
    fn out_of_order_or_missing_pair_rows_are_rejected() {
        let captures = vec![row(101, resolvable()), row(103, resolvable())];
        let pairs = vec![pair(0, Some(101)), pair(2, Some(103))];
        let error = select_delivery_depth_rows(&captures, &pairs).unwrap_err();
        assert!(error.contains("session_frame 不连续"), "{error}");
    }

    #[test]
    fn inherited_depth_is_rejected_for_formal_delivery() {
        let captures = vec![row(101, resolvable())];
        let mut inherited = pair(0, Some(101));
        inherited.depth.as_mut().unwrap().inherited = true;
        let error = select_delivery_depth_rows(&captures, &[inherited]).unwrap_err();
        assert!(error.contains("duplicate/inherited"), "{error}");
    }

    #[test]
    fn missing_dsv_identity_is_rejected_for_formal_delivery() {
        let mut capture = row(101, resolvable());
        capture.dsv_id.clear();
        let error = select_delivery_depth_rows(&[capture], &[pair(0, Some(101))]).unwrap_err();
        assert!(error.contains("没有 dsv_id"), "{error}");
    }

    #[test]
    fn a_tampered_delivery_depth_name_is_rejected() {
        let capture = row(101, resolvable());
        let mut pair = pair(0, Some(101));
        pair.depth.as_mut().unwrap().depth_name = "depth_101".into();
        let error = select_delivery_depth_rows(&[capture], &[pair]).unwrap_err();
        assert!(error.contains("应为 `depth_0`"), "{error}");
    }

    #[test]
    fn zip_is_a_valid_stored_archive_with_flat_names() {
        let mut z = FlatZip::new();
        z.add("depth_0.exr", b"hello");
        z.add("depth_2.exr", b"world!");
        let bytes = z.finish();
        // 本地头 + 中央目录 + EOCD 的魔数都要在
        assert_eq!(&bytes[0..4], &0x0403_4b50u32.to_le_bytes());
        assert!(
            bytes.windows(4).any(|w| w == 0x0201_4b50u32.to_le_bytes()),
            "缺中央目录"
        );
        assert!(
            bytes.windows(4).any(|w| w == 0x0605_4b50u32.to_le_bytes()),
            "缺 EOCD"
        );
        // 平铺：名字里不许有目录分隔符（68 号要求单个平铺文件夹）
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("depth_0.exr"));
        assert!(!text.contains("depth_0.exr/"));
        assert!(!text.contains("/depth_0.exr"));
    }

    /// CRC 算错的 zip 在很多解压器上仍能打开，只在校验时报错——
    /// 也就是说它会一路走到需求方那边才炸。用已知向量钉住。
    #[test]
    fn crc32_matches_the_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn v1_rows_without_storage_are_read_as_raw_f32() {
        let row: CaptureRow =
            serde_json::from_str(r#"{"present_seq":2,"width":2,"height":1,"path":"raw/x.f32"}"#)
                .unwrap();
        assert_eq!(row.encoding(), "raw-f32");
        assert_eq!(row.domain(), "raw_buffer");
    }

    #[test]
    fn v2_rows_declare_their_encoding_and_domain() {
        let row: CaptureRow = serde_json::from_str(
            r#"{"present_seq":2,"width":2,"height":1,"path":"raw/x.exr",
                "storage":{"encoding":"exr-half-zip","domain":"raw_buffer","channel":"Z"}}"#,
        )
        .unwrap();
        assert_eq!(row.encoding(), "exr-half-zip");
        assert_eq!(row.domain(), "raw_buffer");
    }

    /// 已经是米制的帧再线性化一次会得到完全错的数，而结果看起来仍然像深度图。
    #[test]
    fn already_metric_frames_are_refused_rather_than_linearized_twice() {
        let d = std::env::temp_dir().join(format!("dm-deliver-metric-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("raw")).unwrap();
        std::fs::write(d.join("raw/x.f32"), 1.0f32.to_le_bytes()).unwrap();
        let mut row: CaptureRow = serde_json::from_str(
            r#"{"present_seq":0,"width":1,"height":1,"path":"raw/x.f32",
                "storage":{"encoding":"raw-f32","domain":"view_space_meters","channel":"Z"}}"#,
        )
        .unwrap();
        row.projection = resolvable();
        let err = build_depth_zip(&d, &[delivery_row(0, row)]).unwrap_err();
        assert!(err.contains("view_space_meters"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_capture_is_an_error_not_an_empty_zip() {
        let err = build_depth_zip(Path::new("/nonexistent"), &[]).unwrap_err();
        assert!(err.contains("没有通过配对验证"));
    }
}

// --- action_camera.json（68 号 §6.3 / schema/action_camera_15fps.sample.json）---

/// 采集侧为每个 present_seq 写下的位姿旁车。
///
/// 探针从 P4 的常量缓冲取 view/projection 矩阵还原位姿与内参（[`crate::camera`]），
/// 或由游戏自身的遥测给（ETS2 的 SDK 直接出米制位置与速度）。两条路都写成这一种行，
/// **转换器不关心它是怎么来的，只关心 `present_seq` 对得上**。
pub type PoseRow = crate::FinalPoseRow;

fn num(value: f64) -> String {
    // 需求方样本里 1.0 写作 `1.0`、整数速度写作 `0.0`——统一按最短往返表示输出，
    // 但整数必须带 `.0`，否则 `metric_scale: 1` 与样本的 `1.0` 不一致。
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        let mut s = format!("{value}");
        if !s.contains('.') && !s.contains('e') {
            s.push_str(".0");
        }
        s
    }
}

fn vec3(v: [f64; 3]) -> String {
    format!("[{}, {}, {}]", num(v[0]), num(v[1]), num(v[2]))
}

fn vec4(v: [f64; 4]) -> String {
    format!(
        "[{}, {}, {}, {}]",
        num(v[0]),
        num(v[1]),
        num(v[2]),
        num(v[3])
    )
}

/// 生成 68 号的 `action_camera.json`。
///
/// **键序是手写的，不是 serde 派生的。** 需求方的样本有固定键序，而 `serde_json`
/// 的 `Map` 默认按插入序、开了 `preserve_order` 之外的构建又会变字典序——
/// 一个键序漂移的交付在肉眼比对时极难发现，却可能让对方的解析脚本挑不出字段。
///
/// 同样刻意逐字保留的怪癖（见 `engine-data/README` 的键名怪癖表）：
/// 68 号侧 follow offset 是**小写下划线** `camera_follow_offset`，
/// 鼠标绝对坐标是**驼峰** `mouseX`/`mouseY`。别「修正」它们。
pub fn build_action_camera(
    depth_rows: &[DeliveryDepthRow],
    poses: &BTreeMap<u64, PoseRow>,
    _fps: f64,
) -> Result<String, String> {
    if depth_rows.is_empty() {
        return Err("没有 depth 帧，生不出 action_camera.json".into());
    }
    // 位姿和 depth 在进入交付层前都已经换算为米。索引里的 metric_scale 是
    // 游戏单位 → 米的内部线性化因子，不能再次泄漏为交付坐标的单位比例。
    // 业务核验锚点（docs/102 §2）要求此处固定声明米制 1.0。
    let metric_scale = 1.0;
    let mut missing = Vec::new();
    let mut out = String::from("[\n");

    for (i, delivery_row) in depth_rows.iter().enumerate() {
        let row = delivery_row.capture();
        let Some(pose) = poses.get(&row.present_seq) else {
            missing.push(row.present_seq);
            continue;
        };
        let frame = delivery_row.session_frame();
        let depth_name = depth_name_for(frame);
        if i > 0 && !out.ends_with("[\n") {
            out.push_str(",\n");
        }
        out.push_str("  {\n");
        out.push_str(&format!("    \"time\": {:?},\n", pose.time));
        out.push_str(&format!("    \"fps\": {},\n", num(pose.fps)));
        out.push_str(&format!("    \"depth_name\": {depth_name:?},\n"));
        out.push_str(&format!("    \"frame\": {frame},\n"));
        out.push_str(&format!(
            "    \"camera_position\": {},\n",
            vec3(pose.camera_position)
        ));
        out.push_str(&format!(
            "    \"camera_rotation_quaternion\": {},\n",
            vec4(pose.camera_rotation_quaternion)
        ));
        out.push_str(&format!(
            "    \"camera_follow_offset\": {},\n",
            vec3(pose.camera_follow_offset)
        ));
        let k = &pose.camera_intrinsics;
        out.push_str(&format!(
            "    \"camera_intrinsics\": {{\"fx\": {}, \"fy\": {}, \"cx\": {}, \"cy\": {}}},\n",
            num(f64::from(k.fx)),
            num(f64::from(k.fy)),
            num(f64::from(k.cx)),
            num(f64::from(k.cy))
        ));
        out.push_str(&format!(
            "    \"camera_speed\": {},\n",
            vec3(pose.camera_speed)
        ));
        out.push_str(&format!(
            "    \"player_position\": {},\n",
            vec3(pose.player_position)
        ));
        out.push_str(&format!("    \"mouseX\": {},\n", num(pose.mouse_x)));
        out.push_str(&format!("    \"mouseY\": {},\n", num(pose.mouse_y)));
        out.push_str(&format!("    \"mouse_dx\": {},\n", num(pose.mouse_dx)));
        out.push_str(&format!("    \"mouse_dy\": {},\n", num(pose.mouse_dy)));
        out.push_str(&format!(
            "    \"keyCode\": [{}],\n",
            pose.key_code
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.push_str(&format!(
            "    \"player_rotation_quaternion\": {},\n",
            vec4(pose.player_rotation_quaternion)
        ));
        out.push_str(&format!(
            "    \"player_speed\": {},\n",
            vec3(pose.player_speed)
        ));
        out.push_str(&format!("    \"metric_scale\": {}\n", num(metric_scale)));
        out.push_str("  }");
    }
    out.push_str("\n]\n");

    // 缺位姿的帧不能悄悄跳过：68 号要求 depth 与 pose 严格对齐，
    // 少一行就是「有 depth 没 pose」，那条 clip 整体不合格。
    if !missing.is_empty() {
        return Err(format!(
            "{} 个 depth 帧没有对应位姿（首个 present_seq={}）——68 号要求逐帧对齐，缺一行整条不合格",
            missing.len(),
            missing[0]
        ));
    }
    Ok(out)
}

/// Read a full-rate pose sidecar without collapsing duplicate RGB frames that
/// legitimately share one source Present.
pub fn read_pose_rows(path: &Path) -> Result<Vec<PoseRow>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(line)
                .map_err(|e| format!("{}:{} 解析失败: {e}", path.display(), i + 1))?,
        );
    }
    if rows.is_empty() {
        return Err(format!("{} 没有位姿行", path.display()));
    }
    Ok(rows)
}

/// Generate the independent 30 fps RGB action-camera file. It intentionally has
/// no `depth_name`; that key belongs only to the 15 fps depth-related JSON.
///
/// **不收 metric_scale。** 位姿和 depth 在进入交付层前都已经换算为米，索引里的
/// `metric_scale` 是业务核验锚点（docs/102 §2）要求此处固定声明米制 1.0；把探针
/// 内部的游戏单位换算因子再传进来只会被重复应用一次。签名里不留这个参数，
/// 是为了让「这里不需要它」在类型上成立——留成 `_source_metric_scale` 的话，
/// 后来人把下划线去掉再用起来，正好复现被修掉的那个 bug。
pub fn build_action_camera_30fps(poses: &[PoseRow]) -> Result<String, String> {
    if poses.is_empty() {
        return Err("没有 RGB 位姿，生不出 action_camera_30fps.json".into());
    }
    let metric_scale = 1.0;
    let first_frame = poses[0].session_frame;
    let mut previous = None;
    let mut out = String::from("[\n");
    for (index, pose) in poses.iter().enumerate() {
        if let Some(previous) = previous
            && pose.session_frame != previous + 1
        {
            return Err(format!(
                "30fps 位姿 session_frame 不连续：{previous} 后是 {}",
                pose.session_frame
            ));
        }
        previous = Some(pose.session_frame);
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str("  {\n");
        out.push_str(&format!("    \"time\": {:?},\n", pose.time));
        out.push_str(&format!("    \"fps\": {},\n", num(pose.fps)));
        out.push_str(&format!(
            "    \"frame\": {},\n",
            pose.session_frame.saturating_sub(first_frame)
        ));
        out.push_str(&format!(
            "    \"camera_position\": {},\n",
            vec3(pose.camera_position)
        ));
        out.push_str(&format!(
            "    \"camera_rotation_quaternion\": {},\n",
            vec4(pose.camera_rotation_quaternion)
        ));
        out.push_str(&format!(
            "    \"camera_follow_offset\": {},\n",
            vec3(pose.camera_follow_offset)
        ));
        let k = &pose.camera_intrinsics;
        out.push_str(&format!(
            "    \"camera_intrinsics\": {{\"fx\": {}, \"fy\": {}, \"cx\": {}, \"cy\": {}}},\n",
            num(f64::from(k.fx)),
            num(f64::from(k.fy)),
            num(f64::from(k.cx)),
            num(f64::from(k.cy))
        ));
        out.push_str(&format!(
            "    \"camera_speed\": {},\n",
            vec3(pose.camera_speed)
        ));
        out.push_str(&format!(
            "    \"player_position\": {},\n",
            vec3(pose.player_position)
        ));
        out.push_str(&format!("    \"mouseX\": {},\n", num(pose.mouse_x)));
        out.push_str(&format!("    \"mouseY\": {},\n", num(pose.mouse_y)));
        out.push_str(&format!("    \"mouse_dx\": {},\n", num(pose.mouse_dx)));
        out.push_str(&format!("    \"mouse_dy\": {},\n", num(pose.mouse_dy)));
        out.push_str(&format!(
            "    \"keyCode\": [{}],\n",
            pose.key_code
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.push_str(&format!(
            "    \"player_rotation_quaternion\": {},\n",
            vec4(pose.player_rotation_quaternion)
        ));
        out.push_str(&format!(
            "    \"player_speed\": {},\n",
            vec3(pose.player_speed)
        ));
        out.push_str(&format!("    \"metric_scale\": {}\n", num(metric_scale)));
        out.push_str("  }");
    }
    out.push_str("\n]\n");
    Ok(out)
}

/// 读位姿旁车。
pub fn read_pose_index(path: &Path) -> Result<BTreeMap<u64, PoseRow>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
    let mut map = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: PoseRow = serde_json::from_str(line)
            .map_err(|e| format!("{}:{} 解析失败: {e}", path.display(), i + 1))?;
        let present_seq = row.present_seq;
        if map.insert(present_seq, row).is_some() {
            return Err(format!(
                "{}:{} 的 present_seq {present_seq} 重复，无法确定该用哪条位姿",
                path.display(),
                i + 1
            ));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod action_camera_tests {
    use super::*;

    fn pose(seq: u64) -> PoseRow {
        PoseRow {
            present_seq: seq,
            session_frame: seq,
            camera_identity: crate::FinalCameraIdentity::GameTelemetry {
                provider: "test-telemetry".into(),
            },
            camera_source: crate::CameraPoseSource::GameTelemetry,
            time: "2026-07-31 15:50:19.354".into(),
            fps: 30.0,
            camera_position: [4991.432404, 1.986775, -5507.211924],
            camera_rotation_quaternion: [0.019822, -0.374512, 0.008008, 0.926976],
            camera_intrinsics: crate::camera::Intrinsics {
                fx: 960.0,
                fy: 960.0,
                cx: 960.0,
                cy: 540.0,
            },
            camera_follow_offset: [0.069464, 0.6, -0.071935],
            camera_speed: [-6.591536, -0.280702, 6.829726],
            player_position: [4991.36294, 1.386775, -5507.139989],
            player_rotation_quaternion: [-0.0, -0.374598, -0.0, 0.927187],
            player_speed: [-4.166878, 0.0, 4.317074],
            mouse_x: 0.35625,
            mouse_y: 0.45675,
            mouse_dx: 0.0,
            mouse_dy: -0.0,
            key_code: vec![87],
        }
    }

    fn rows(seqs: &[u64]) -> Vec<DeliveryDepthRow> {
        seqs.iter()
            .enumerate()
            .map(|(index, s)| {
                let mut capture = serde_json::from_str::<CaptureRow>(&format!(
                    r#"{{"present_seq":{s},"width":1280,"height":720,"path":"raw/x.exr"}}"#
                ))
                .unwrap();
                capture.projection = crate::capture_timing::IndexProjection::observed(
                    Projection::reversed_z_infinite(0.1, 0.014_287_5),
                    Some(0.014_287_5),
                );
                DeliveryDepthRow {
                    session_frame: index as u64 * 2,
                    capture,
                }
            })
            .collect()
    }

    /// 键序漂移的交付在肉眼比对时极难发现，却可能让对方的解析脚本挑不出字段。
    /// 这条按需求方样本逐字钉住顺序。
    #[test]
    fn keys_appear_in_the_exact_order_of_the_customer_sample() {
        let mut poses = BTreeMap::new();
        poses.insert(100, pose(100));
        let json = build_action_camera(&rows(&[100]), &poses, 30.0).unwrap();
        let order: Vec<&str> = [
            "\"time\"",
            "\"fps\"",
            "\"depth_name\"",
            "\"frame\"",
            "\"camera_position\"",
            "\"camera_rotation_quaternion\"",
            "\"camera_follow_offset\"",
            "\"camera_intrinsics\"",
            "\"camera_speed\"",
            "\"player_position\"",
            "\"mouseX\"",
            "\"mouseY\"",
            "\"mouse_dx\"",
            "\"mouse_dy\"",
            "\"keyCode\"",
            "\"player_rotation_quaternion\"",
            "\"player_speed\"",
            "\"metric_scale\"",
        ]
        .to_vec();
        let mut cursor = 0;
        for key in order {
            let found = json[cursor..]
                .find(key)
                .unwrap_or_else(|| panic!("{key} 不在预期位置，键序漂了:\n{json}"));
            cursor += found + key.len();
        }
    }

    /// 68 号侧 follow offset 是小写下划线、鼠标绝对坐标是驼峰。
    /// 这两个怪癖来自需求方样本，**不许「修正」**。
    #[test]
    fn the_key_name_quirks_are_preserved_verbatim() {
        let mut poses = BTreeMap::new();
        poses.insert(0, pose(0));
        let json = build_action_camera(&rows(&[0]), &poses, 30.0).unwrap();
        assert!(json.contains("\"camera_follow_offset\""));
        assert!(
            !json.contains("camera_Follow Offset"),
            "那是 69 号 MC 的怪癖，不是 68 号的"
        );
        assert!(json.contains("\"mouseX\""));
        assert!(json.contains("\"mouseY\""));
        assert!(!json.contains("\"mouse_x\""));
    }

    /// depth_name 与 frame 都从 clip 的 0 数起，且必须彼此一致。
    #[test]
    fn frame_numbers_and_depth_names_agree_and_restart_at_zero() {
        let mut poses = BTreeMap::new();
        for s in [16712, 16714, 16716] {
            poses.insert(s, pose(s));
        }
        let json = build_action_camera(&rows(&[16712, 16714, 16716]), &poses, 30.0).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        for (i, expected) in [0u64, 2, 4].iter().enumerate() {
            assert_eq!(arr[i]["frame"].as_u64().unwrap(), *expected);
            assert_eq!(
                arr[i]["depth_name"].as_str().unwrap(),
                format!("depth_{expected}")
            );
        }
        // 全局 present_seq 绝不能泄漏进交付
        assert!(!json.contains("16712"));
    }

    /// 缺一帧位姿就是「有 depth 没 pose」，68 号要求逐帧严格对齐——
    /// 悄悄少写一行会让 depth.zip 与 json 的帧数对不上，而对方是逐文件验收的。
    #[test]
    fn a_depth_frame_without_a_pose_fails_instead_of_being_skipped() {
        let mut poses = BTreeMap::new();
        poses.insert(0, pose(0));
        poses.insert(4, pose(4));
        let err = build_action_camera(&rows(&[0, 2, 4]), &poses, 30.0).unwrap_err();
        assert!(err.contains("没有对应位姿"), "{err}");
    }

    /// 整数必须带 .0——需求方样本里 metric_scale 是 `1.0` 而不是 `1`。
    #[test]
    fn integers_keep_their_decimal_point_like_the_sample() {
        let mut poses = BTreeMap::new();
        poses.insert(0, pose(0));
        let json = build_action_camera(&rows(&[0]), &poses, 30.0).unwrap();
        assert!(json.contains("\"metric_scale\": 1.0"), "{json}");
        assert!(json.contains("\"fps\": 30.0"));
        assert!(json.contains("\"fx\": 960.0"));
    }

    #[test]
    fn the_output_is_valid_json() {
        let mut poses = BTreeMap::new();
        for s in [0, 2] {
            poses.insert(s, pose(s));
        }
        let json = build_action_camera(&rows(&[0, 2]), &poses, 30.0).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn duplicate_pose_keys_are_rejected_instead_of_overwritten() {
        let path =
            std::env::temp_dir().join(format!("dm-duplicate-pose-{}.jsonl", std::process::id()));
        let line = serde_json::to_string(&pose(7)).unwrap();
        std::fs::write(&path, format!("{line}\n{line}\n")).unwrap();
        let error = read_pose_index(&path).unwrap_err();
        assert!(error.contains("present_seq 7 重复"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn final_pose_round_trip_keeps_camera_identity_and_source() {
        let expected = pose(7);
        let path =
            std::env::temp_dir().join(format!("dm-pose-provenance-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&expected).unwrap()),
        )
        .unwrap();
        let actual = read_pose_rows(&path).unwrap();
        assert_eq!(actual, vec![expected]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_final_pose_without_camera_identity_is_fail_closed() {
        let mut legacy = serde_json::to_value(pose(7)).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("camera_identity");
        object.remove("camera_source");
        let path = std::env::temp_dir().join(format!(
            "dm-pose-legacy-provenance-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .unwrap();
        let error = read_pose_rows(&path).unwrap_err();
        assert!(error.contains("camera_identity"), "{error}");
        let _ = std::fs::remove_file(path);
    }
}

/// 从 MP4 sample table 读出来的视频事实。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub frames: u64,
}

/// 探测视频的分辨率/帧率/帧数，并验证 sample 时间轴连续且没有 B-frame 重排。
pub fn probe_video(path: &Path) -> Result<VideoInfo, String> {
    use mp4::{Mp4Reader, TrackType};

    let file = std::fs::File::open(path)
        .map_err(|error| format!("打开 {} 失败: {error}", path.display()))?;
    let size = file
        .metadata()
        .map_err(|error| format!("读取 {} 元数据失败: {error}", path.display()))?
        .len();
    let mut reader = Mp4Reader::read_header(std::io::BufReader::new(file), size)
        .map_err(|error| format!("解析 {} 失败: {error}", path.display()))?;
    if reader.is_fragmented() || reader.tracks().len() != 1 {
        return Err("video.mp4 必须是单轨、非分片 MP4".into());
    }
    let track = reader
        .tracks()
        .values()
        .next()
        .expect("track count checked");
    if track
        .track_type()
        .map_err(|error| format!("读取视频轨类型失败: {error}"))?
        != TrackType::Video
    {
        return Err("video.mp4 的唯一轨道不是视频".into());
    }
    let (track_id, timescale, frames, width, height) = (
        track.track_id(),
        track.timescale(),
        track.sample_count(),
        u32::from(track.width()),
        u32::from(track.height()),
    );
    if frames == 0 || timescale == 0 {
        return Err("video.mp4 没有帧或 timescale 为 0".into());
    }
    let mut expected_start = 0_u64;
    for sample_id in 1..=frames {
        let sample = reader
            .read_sample(track_id, sample_id)
            .map_err(|error| format!("读取视频第 {sample_id} 帧失败: {error}"))?
            .ok_or_else(|| format!("video.mp4 缺少第 {sample_id} 帧"))?;
        if sample.duration == 0
            || sample.rendering_offset != 0
            || sample.start_time != expected_start
        {
            return Err(format!(
                "video.mp4 第 {sample_id} 帧时间轴不连续、时长为 0 或含 B-frame 重排"
            ));
        }
        expected_start = expected_start
            .checked_add(u64::from(sample.duration))
            .ok_or_else(|| "video.mp4 时间轴溢出".to_string())?;
    }
    let fps = f64::from(frames) * f64::from(timescale) / expected_start as f64;
    Ok(VideoInfo {
        width,
        height,
        fps,
        frames: u64::from(frames),
    })
}
