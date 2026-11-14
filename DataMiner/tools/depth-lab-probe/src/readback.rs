//! P1c：把选中的 depth 缓冲从 GPU 拷出来。
//!
//! 这是 probe 里**唯一真正碰 D3D 的回读逻辑**。所有的判断都不在这里：
//!
//! | 判断 | 在哪 | 已测 |
//! |---|---|---|
//! | 解绑快照选择、归属哪一帧 | `probe::snapshot_index` + 真机轮询证据 | ✅ 单测 + E2E |
//! | 槽位轮转、超时回收、丢帧计数 | [`depth_lab::readback`] | ✅ 8 个用例 |
//! | 字节 → 深度、行距、降采样 | [`depth_lab::decode`] | ✅ 14 个用例 |
//!
//! 这里只剩三个调用：`CopyResource`、`Map(DO_NOT_WAIT)`、`Unmap`。
//!
//! # 为什么读取解绑快照
//!
//! `Present` 边界拿不到 DSV（#425 实测），直接读取引擎资源又会遇到帧末清空。
//! probe 因此在每次主 DSV 解绑时写一张私有 GPU 快照；BioShock Infinite 真机轮询
//! 12 个解绑时机后确认，从帧尾倒数第 4 张包含完整主场景。这里仅负责把调用方选好的
//! 快照送入非阻塞 staging 池。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_IEEE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D11::D3D11_UAV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
    D3D11_USAGE_STAGING, ID3D11Buffer, ID3D11ClassInstance, ID3D11ComputeShader, ID3D11Device,
    ID3D11DeviceContext, ID3D11Resource, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R24_UNORM_X8_TYPELESS, DXGI_FORMAT_R24G8_TYPELESS,
    DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS, DXGI_FORMAT_R32_TYPELESS,
    DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAS_STILL_DRAWING, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    IDXGIAdapter3, IDXGIDevice,
};
use windows::core::{Interface, s};

use depth_lab::decode::{self, DepthFormat, ZConvention};
use depth_lab::readback::{Issue, ReadbackPool};

/// 回读配置。字段一一对应 `config/depth-profiles/<game_id>.*.json`
/// （[`depth_lab::profile::DepthProfile`]），probe 启动时从那里读一次即可。
#[derive(Debug, Clone, Copy)]
pub struct CaptureConfig {
    /// 交付分辨率（68 号 §6.4 固定 1280×720）。
    pub dst_w: usize,
    pub dst_h: usize,
    /// Z 约定。**决定降采样取 min 还是 max**，搞反会系统性吃掉前景边缘。
    pub conv: ZConvention,
    /// staging 环形池槽位数（§5.4 要求 ≥4）。
    pub slots: usize,
    /// 槽位超时回收的帧数上限。
    pub deadline_ticks: u64,
}

/// 一帧刚从 GPU 回读出来的**原始字节**，交给 writer 线程去解码。
///
/// # 为什么这里是原始字节而不是解好的深度
///
/// `decode_frame`（2.07M 像素逐字节解码）+ `pool_nearest`（→921K 输出，
/// 每个输出取 2~3×2~3 块的最近值）原本都跑在 `Present` hook 里，
/// 也就是**渲染线程**上。真机实测摊到每次回读约 **11 ms**——
/// 这是 C5 失败最大的一块（完整档比仅水印档多 6.17 ms/帧）。
///
/// 现在 hook 里只剩「memcpy 出来 → `Unmap`」，解码与降采样搬到 writer 线程。
/// 8.3 MB 的 memcpy 约 1 ms，且 `Unmap` 能立刻还给 GPU。
/// Current path: one `Present` performs `Map(DO_NOT_WAIT)`, the raw-byte copy, and `Unmap` as one
/// lifetime. Large copies use cache-bypassing stores so no mapped staging pointer crosses frames.
pub struct RawDepthFrame {
    pub present_seq: u64,
    /// 源几何（回读分辨率，不是交付分辨率）。
    pub src_width: usize,
    pub src_height: usize,
    /// `D3D11_MAPPED_SUBRESOURCE::RowPitch`。**必须带着走**——
    /// 驱动几乎总会把行对齐，按 `width×4` 平铺读会让每行渐进偏移。
    pub row_pitch: usize,
    pub format: DepthFormat,
    /// 交付分辨率与 Z 约定，供 writer 线程做降采样。
    pub dst_width: usize,
    pub dst_height: usize,
    pub conv: ZConvention,
    /// Map 出来的原始字节副本。
    pub bytes: Vec<u8>,
    /// `bytes` 是否已经是 GPU 池化后的 `dst_width×dst_height` R32_FLOAT。
    /// true 时 [`Self::decode`] 只做行距剥离，不再跑 CPU 解码/降采样。
    pub pooled: bool,
    /// VERIFY 影子对拍（`DM_DEPTH_PROBE_GPU_POOL_VERIFY=1`）：GPU 池化结果的原始字节。
    /// 交付数据 100% 来自 CPU 参考路径（`bytes`），这份只用于 writer 线程逐位比对。
    pub verify_pooled: Option<Vec<u8>>,
    pub verify_row_pitch: usize,
}

/// 字节缓冲回收池。
///
/// # 为什么不直接 `to_vec()`
///
/// 1080p 的 staging 一帧是 **8.3 MB**。每次回读新分配一块，意味着
/// 每一页在第一次写入时都要缺页中断（8.3 MB ≈ 2048 页），
/// 而这笔开销**落在渲染线程上**——正是我们要省的那条线。
/// 稳态下复用同一批缓冲，`extend_from_slice` 写的是已经映射好的页。
///
/// 上限取 8：staging 环 + 有界 channel 决定了同时存活的缓冲数远小于这个值，
/// 设上限只是防止分辨率反复变化时旧尺寸的缓冲越攒越多。
const MAX_POOLED_BUFFERS: usize = 8;
const NON_TEMPORAL_COPY_MIN_BYTES: usize = 256 * 1024;

/// 每个 Present 从就绪 staging 分片拷出的默认字节预算（#504 第五轮遥测：
/// 渲染线程一次性 memcpy 整个 720p 槽（≈3.7 MB）中位 0.345 ms、命中 6.23% 的帧，
/// 与正式 B/D 的 p99 增量 0.424 ms 在幅度与频率上都吻合。512 KiB 一片约 50 µs，
/// 240 fps 帧预算的 ~1%；720p 帧 8 片、8 个 Present 拷完，仍远短于 15 Hz 采集间隔）。
const DEFAULT_DRAIN_SLICE_BYTES: usize = 512 * 1024;

/// `DM_DEPTH_PROBE_DRAIN_SLICE_KB`：分片预算（KiB）。`0` 禁用分片（整块拷出，
/// 复测对照用）；其余钳制到 [64, 16384]；未设置或解析失败用默认值。
fn drain_slice_bytes() -> usize {
    static SLICE: OnceLock<usize> = OnceLock::new();
    *SLICE.get_or_init(|| {
        drain_slice_bytes_from(
            std::env::var("DM_DEPTH_PROBE_DRAIN_SLICE_KB")
                .ok()
                .as_deref(),
        )
    })
}

fn drain_slice_bytes_from(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_DRAIN_SLICE_BYTES;
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => usize::MAX,
        Ok(kib) => usize::try_from(kib.clamp(64, 16 * 1024)).expect("钳制后必然可表示") * 1024,
        Err(_) => DEFAULT_DRAIN_SLICE_BYTES,
    }
}

static BUFFERS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Copy a large mapped staging resource without flooding the shared CPU caches.
///
/// # Safety
///
/// `src` must be readable and `dst` writable for `len` bytes. The regions must not overlap.
unsafe fn copy_bytes_into(src: *const u8, dst: *mut u8, len: usize) {
    if len < NON_TEMPORAL_COPY_MIN_BYTES {
        // SAFETY: upheld by the caller contract.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
        return;
    }

    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_sfence, _mm_stream_si128};

        let prefix = ((16 - (dst as usize & 15)) & 15).min(len);
        // SAFETY: upheld by the caller contract; this prefix aligns the streaming destination.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, prefix) };
        let mut offset = prefix;
        while offset + 64 <= len {
            // SAFETY: each load/store stays within the caller-provided regions. The destination is
            // 16-byte aligned after `prefix`; source loads intentionally allow unaligned memory.
            unsafe {
                let a = _mm_loadu_si128(src.add(offset).cast::<__m128i>());
                let b = _mm_loadu_si128(src.add(offset + 16).cast::<__m128i>());
                let c = _mm_loadu_si128(src.add(offset + 32).cast::<__m128i>());
                let d = _mm_loadu_si128(src.add(offset + 48).cast::<__m128i>());
                _mm_stream_si128(dst.add(offset).cast::<__m128i>(), a);
                _mm_stream_si128(dst.add(offset + 16).cast::<__m128i>(), b);
                _mm_stream_si128(dst.add(offset + 32).cast::<__m128i>(), c);
                _mm_stream_si128(dst.add(offset + 48).cast::<__m128i>(), d);
            }
            offset += 64;
        }
        // Streaming stores must be globally visible before the buffer is handed to the writer.
        unsafe { _mm_sfence() };
        // SAFETY: `offset <= len`, and the regions remain valid through the tail.
        unsafe { std::ptr::copy_nonoverlapping(src.add(offset), dst.add(offset), len - offset) };
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        // SAFETY: upheld by the caller contract.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
    }
}

/// Copy mapped bytes into a recycled Vec and finish before the matching D3D `Unmap`.
///
/// # Safety
///
/// `src` must remain readable for `len` bytes until this function returns.
pub(crate) unsafe fn copy_mapped_bytes(src: *const u8, len: usize) -> Vec<u8> {
    let mut bytes = take_buffer(len);
    // SAFETY: capacity is at least `len`; `copy_bytes_into` initializes every byte before return.
    unsafe {
        bytes.set_len(len);
        copy_bytes_into(src, bytes.as_mut_ptr(), len);
    }
    bytes
}

/// 取一个**已清空**、容量至少 `cap` 的缓冲。
///
/// 刻意不用 `resize`——那会把整块 8.3 MB 先 memset 一遍再被 memcpy 覆盖。
/// 返回空 Vec + 足够容量，交给 `extend_from_slice` 只走一次 memcpy。
///
/// 这里只能 `try_lock`：回收者是后台低优先级线程，渲染线程等待它会形成优先级反转。
/// 竞争时临时分配一块，代价可见但有界；绝不能把游戏的 Present 卡在互斥锁上。
fn take_buffer(cap: usize) -> Vec<u8> {
    let mut buf = BUFFERS
        .try_lock()
        .ok()
        .and_then(|mut pool| pool.pop())
        .unwrap_or_default();
    buf.clear();
    if buf.capacity() < cap {
        buf.reserve_exact(cap);
    }
    buf
}

fn recycle_buffer(buf: Vec<u8>) {
    if buf.capacity() == 0 {
        return;
    }
    // best-effort 回收。锁正忙时直接释放 Vec，避免后台线程反过来挡住热路径。
    if let Ok(mut pool) = BUFFERS.try_lock()
        && pool.len() < MAX_POOLED_BUFFERS
    {
        pool.push(buf);
    }
}

/// 落盘（或丢弃）后把缓冲还回池子。放在 `Drop` 里而不是让 writer 显式归还——
/// `try_send` 队列满时帧是被**丢掉**的，那条路径同样要回收。
impl Drop for RawDepthFrame {
    fn drop(&mut self) {
        recycle_buffer(std::mem::take(&mut self.bytes));
    }
}

impl RawDepthFrame {
    /// 在 **writer 线程**上把原始字节解成交付分辨率的归一化深度。
    ///
    /// 刻意不在构造时做——那样就等于又回到渲染线程上了。
    pub fn decode(&self) -> Result<Vec<f32>, String> {
        let mut out = Vec::new();
        self.decode_into(&mut out)?;
        Ok(out)
    }

    /// Reuse the writer's single float plane; failed input never exposes stale pixels.
    pub fn decode_into(&self, out: &mut Vec<f32>) -> Result<(), String> {
        out.clear();
        if self.pooled {
            // GPU 已按 CPU 参考的窗口公式池化到交付分辨率，这里只剥行距。
            return decode_pooled_bytes_into(
                &self.bytes,
                self.dst_width,
                self.dst_height,
                self.row_pitch,
                out,
            )
            .map_err(|e| format!("剥离池化帧 {} 失败: {e}", self.present_seq));
        }
        *out = decode::decode_and_pool_nearest(
            &self.bytes,
            (self.src_width, self.src_height),
            self.row_pitch,
            self.format,
            (self.dst_width, self.dst_height),
            self.conv,
        )
        .map_err(|e| format!("解码帧 {} 失败: {e}", self.present_seq))?;
        Ok(())
    }
}

/// 把带行距的 R32_FLOAT staging 字节剥成密排 f32。位模式原样保留（含 NaN）。
pub fn decode_pooled_bytes(
    bytes: &[u8],
    width: usize,
    height: usize,
    row_pitch: usize,
) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    decode_pooled_bytes_into(bytes, width, height, row_pitch, &mut out)?;
    Ok(out)
}

fn decode_pooled_bytes_into(
    bytes: &[u8],
    width: usize,
    height: usize,
    row_pitch: usize,
    out: &mut Vec<f32>,
) -> Result<(), String> {
    out.clear();
    let needed_row = width * 4;
    if row_pitch < needed_row {
        return Err(format!(
            "池化 RowPitch={row_pitch} 小于一行所需 {needed_row} 字节"
        ));
    }
    let needed_total = row_pitch * (height.saturating_sub(1)) + needed_row;
    if bytes.len() < needed_total {
        return Err(format!(
            "池化缓冲只有 {} 字节，需要 {needed_total}",
            bytes.len()
        ));
    }
    out.reserve(width * height);
    for y in 0..height {
        let row = &bytes[y * row_pitch..y * row_pitch + needed_row];
        out.extend(
            row.as_chunks::<4>()
                .0
                .iter()
                .map(|px| f32::from_le_bytes([px[0], px[1], px[2], px[3]])),
        );
    }
    Ok(())
}

/// VERIFY 影子对拍：CPU 参考与 GPU 池化必须逐位一致。返回 None 表示一致；
/// 否则给出前几处 diff 的位置与位模式，供 Flush 错误定位。
#[must_use]
pub fn verify_bitwise(cpu: &[f32], gpu: &[f32], width: usize) -> Option<String> {
    if cpu.len() != gpu.len() {
        return Some(format!("长度不一致: cpu={} gpu={}", cpu.len(), gpu.len()));
    }
    let mut diffs = 0usize;
    let mut detail = String::new();
    for (index, (a, b)) in cpu.iter().zip(gpu.iter()).enumerate() {
        if a.to_bits() != b.to_bits() {
            diffs += 1;
            if diffs <= 3 && width != 0 {
                detail.push_str(&format!(
                    " [({},{}) cpu={:#010x} gpu={:#010x}]",
                    index % width,
                    index / width,
                    a.to_bits(),
                    b.to_bits()
                ));
            }
        }
    }
    (diffs != 0).then(|| format!("{diffs} 个像素位模式不一致{detail}"))
}

/// 一个就绪 staging 槽的分片拷出进度（#504：渲染线程整块 memcpy → 逐 Present 片拷）。
///
/// 槽位在池里处于 `Draining`；每片拷贝都是「`Map(DO_NOT_WAIT)` → 拷 ≤预算字节 →
/// `Unmap`」的完整闭环，**mapped 指针绝不跨 Present 存活**。
struct PartialDrain {
    slot: usize,
    present_seq: u64,
    /// `take_buffer` + `set_len(len)` 后逐片填充。`done` 之前的字节已初始化；
    /// 交付前必须 `done == len`，丢弃时整块 recycle（回收池只复用容量，不读内容）。
    bytes: Vec<u8>,
    done: usize,
    len: usize,
    /// 首片 Map 观察到的 `RowPitch`。同一 staging 资源的布局不该变；
    /// 后续片一旦漂移即 fail-closed 丢弃——绝不把两种布局拼进同一帧。
    row_pitch: usize,
}

/// staging 资源 + 池状态。整个 probe 只有一份，由 [`STATE`] 持有。
struct ReadbackState {
    staging: Vec<ID3D11Texture2D>,
    pool: ReadbackPool,
    /// 分片拷出队列（FIFO，与池里的 `Draining` 槽位一一对应）。
    /// 稳态最多 1 项；≥2 项说明消费落后于 15 Hz 生产（低帧率），触发泄洪。
    partials: VecDeque<PartialDrain>,
    /// 已完整拷出、但前面还有更老帧未完成的有界重排缓存（#792）。
    ///
    /// key 是 staging 槽位；对应槽位在 `ReadbackPool` 中保持 `Ready`，按序交付前
    /// 不会被 capture 复用。因此这里最多等于池槽位数，不会因一张迟到帧无界增长。
    ready_frames: HashMap<usize, RawDepthFrame>,
    /// 源纹理的几何与格式；用来判断能否复用已建的 staging。
    src_desc: D3D11_TEXTURE2D_DESC,
    format: DepthFormat,
    /// 交付分辨率（68 号 §6.4 固定 720p）。
    dst_w: usize,
    dst_h: usize,
    conv: ZConvention,
    /// GPU 池化管线。正常模式它是**唯一**回读路径：建失败 → `gpu_error`，
    /// capture 一律 Err（fail-closed），绝不静默退回 CPU 全量路径。
    gpu: Option<GpuPool>,
    gpu_error: Option<String>,
    /// `DM_DEPTH_PROBE_GPU_POOL_VERIFY=1`：交付 100% 走 CPU 参考（全量 staging），
    /// GPU 池化只作影子，writer 线程逐位对拍。诊断专用，正常采集必须关。
    verify: bool,
    verify_staging: Option<ID3D11Texture2D>,
    verify_in_flight: Option<u64>,
    verify_skipped: u64,
    /// True when the source is already a 720p R32_FLOAT candidate. This path
    /// performs only candidate -> staging CopyResource and must never dispatch
    /// the pool shader a second time.
    pooled_direct: bool,
}

// SAFETY: 所有对 D3D 对象的访问都在 `STATE` 的 Mutex 之下串行发生；
// probe 的 hook 可能来自不同线程，但同一时刻只有一个持锁者在用这些接口。
unsafe impl Send for ReadbackState {}

impl ReadbackState {
    fn buffer_whole_frame(&mut self, slot: usize, frame: RawDepthFrame) -> Result<(), String> {
        if self.ready_frames.contains_key(&slot) {
            return Err(format!("staging 槽 {slot} 已有等待交付的帧"));
        }
        if !self.pool.buffer_ready(slot) {
            return Err(format!("staging 槽 {slot} 整块拷出后不再处于 InFlight"));
        }
        self.ready_frames.insert(slot, frame);
        Ok(())
    }

    fn buffer_partial_frame(&mut self, slot: usize, frame: RawDepthFrame) -> Result<(), String> {
        if self.ready_frames.contains_key(&slot) {
            return Err(format!("staging 槽 {slot} 已有等待交付的帧"));
        }
        if !self.pool.buffer_finished_drain(slot) {
            return Err(format!("staging 槽 {slot} 分片拷出后不再处于 Draining"));
        }
        self.ready_frames.insert(slot, frame);
        Ok(())
    }

    /// 只释放池中连续 Ready 的最老前缀；遇到更早的 InFlight/Draining 就停。
    fn take_ordered_ready_frames(&mut self) -> Result<Vec<RawDepthFrame>, String> {
        let mut frames = Vec::new();
        while let Some(ready) = self.pool.take_oldest_ready() {
            let frame = self.ready_frames.remove(&ready.slot).ok_or_else(|| {
                format!(
                    "staging 槽 {} 已标记 Ready，但缺少 present_seq={} 的帧",
                    ready.slot, ready.present_seq
                )
            })?;
            if frame.present_seq != ready.present_seq {
                return Err(format!(
                    "staging 槽 {} 的 Ready 归属漂移: pool={} frame={}",
                    ready.slot, ready.present_seq, frame.present_seq
                ));
            }
            frames.push(frame);
        }
        Ok(frames)
    }
}

static STATE: Mutex<Option<ReadbackState>> = Mutex::new(None);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// DXGI 深度格式 → 解码格式。不认识的格式明确拒绝，**不猜**。
fn classify(format: DXGI_FORMAT) -> Option<DepthFormat> {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32G8X24_TYPELESS,
    };
    match format {
        f if f == DXGI_FORMAT_R24G8_TYPELESS || f == DXGI_FORMAT_D24_UNORM_S8_UINT => {
            Some(DepthFormat::D24S8)
        }
        f if f == DXGI_FORMAT_R32_TYPELESS
            || f == DXGI_FORMAT_D32_FLOAT
            || f == DXGI_FORMAT_R32_FLOAT =>
        {
            Some(DepthFormat::D32Float)
        }
        f if f == DXGI_FORMAT_R32G8X24_TYPELESS || f == DXGI_FORMAT_D32_FLOAT_S8X24_UINT => {
            Some(DepthFormat::D32FloatS8x24)
        }
        _ => None,
    }
}

fn make_staging(
    device: &ID3D11Device,
    src: &D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D, String> {
    // §5.3.1：多重采样纹理必须 USAGE_DEFAULT + CPUAccessFlags=0，
    // 所以不存在 MSAA 的 STAGING。命中就应当在 profile 校验阶段被拦下，
    // 这里再兜一次底，免得建出一个永远 Map 不了的资源。
    if src.SampleDesc.Count > 1 {
        return Err(format!(
            "源是 MSAA ×{}，CopyResource→STAGING→Map 在 API 层面不可用（§5.3.1）",
            src.SampleDesc.Count
        ));
    }
    make_staging_sized(device, src.Width, src.Height, src.Format)
}

fn make_staging_sized(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .map_err(|e| format!("建 STAGING 纹理失败: {e}"))?;
    tex.ok_or_else(|| "CreateTexture2D 返回空".to_string())
}

/// VERIFY 影子对拍模式：进程环境变量一次判定，run 中途不切换。
fn verify_mode() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var("DM_DEPTH_PROBE_GPU_POOL_VERIFY").is_ok_and(|value| value == "1")
    })
}

/// GPU 池化 CS。**窗口公式必须与 [`decode::decode_and_pool_nearest`] 逐字符等价**
/// （uint 整除 + div_ceil + min/max 钳制，见 decode.rs），坐标用 `Load` 而不是采样器，
/// 杜绝一切滤波/舍入自由度。HLSL 的 `min/max` 对 NaN 返回另一个操作数（D3D11 规范），
/// 与 `ZConvention::nearer` 的显式 NaN 检查逐位一致；acc 以 quiet NaN 起步同 CPU 参考。
///
/// D24 源经 `R24_UNORM_X8_TYPELESS` SRV 读出 UNORM24→f32（correctly rounded），与 CPU 的
/// `u24 as f32 / 16777215.0`（u24 精确表示 + correctly-rounded 除法）是同一实数的同一舍入。
/// D32 源经 `R32_FLOAT` SRV 位模式原样。两条都以 VERIFY 影子对拍与 WARP 测试钉死。
const POOL_SHADER_HLSL: &str = r#"
Texture2D<float> src : register(t0);
RWTexture2D<float> dst : register(u0);
cbuffer PoolParams : register(b0) {
    uint src_w; uint src_h; uint dst_w; uint dst_h;
    uint take_max; uint3 _pad;
};
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= dst_w || id.y >= dst_h) { return; }
    uint y0 = id.y * src_h / dst_h;
    uint y1 = max(y0 + 1u, min(src_h, ((id.y + 1u) * src_h + dst_h - 1u) / dst_h));
    uint x0 = id.x * src_w / dst_w;
    uint x1 = max(x0 + 1u, min(src_w, ((id.x + 1u) * src_w + dst_w - 1u) / dst_w));
    float acc = asfloat(0x7fc00000u);
    for (uint y = y0; y < y1; ++y) {
        for (uint x = x0; x < x1; ++x) {
            float v = src.Load(int3(int(x), int(y), 0));
            acc = (take_max != 0u) ? max(acc, v) : min(acc, v);
        }
    }
    dst[id.xy] = acc;
}
"#;

/// 编译一次、全 run 复用的 DXBC。d3dcompiler_47 是 Win8+ 系统组件，加载发生在
/// StartRun 后的首个证据帧（控制/渲染稳态），不在 DllMain。失败即 fail-closed——
/// **没有 CPU 静默回退**：#502 的教训是两条数据路径分叉时没有任何东西会报警。
static POOL_SHADER_DXBC: Mutex<Option<Result<Vec<u8>, String>>> = Mutex::new(None);

fn pool_shader_dxbc() -> Result<Vec<u8>, String> {
    let mut guard = POOL_SHADER_DXBC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        *guard = Some(compile_pool_shader());
    }
    guard.as_ref().expect("刚写入").clone()
}

fn compile_pool_shader() -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // IEEE strictness 关掉 fast-math：NaN 起点与 min/max 语义是位等同验证的一部分。
    let result = unsafe {
        D3DCompile(
            POOL_SHADER_HLSL.as_ptr().cast(),
            POOL_SHADER_HLSL.len(),
            None,
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_IEEE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let detail = errors
            .and_then(|blob| unsafe {
                let ptr = blob.GetBufferPointer().cast::<u8>();
                let len = blob.GetBufferSize();
                (!ptr.is_null()).then(|| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
                })
            })
            .unwrap_or_default();
        return Err(format!("池化 CS 编译失败: {error}; {detail}"));
    }
    let code = code.ok_or("D3DCompile 返回空 blob")?;
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

/// GPU 池化管线。所有对象与 staging 池同生命周期（几何/格式变化整体重建）。
struct GpuPool {
    shader: ID3D11ComputeShader,
    params: ID3D11Buffer,
    pool_tex: ID3D11Texture2D,
    pool_uav: ID3D11UnorderedAccessView,
    srv_format: DXGI_FORMAT,
    /// 快照纹理指针 → SRV。快照环最多 12 张；超限清空重建，防指针复用无限增长。
    srvs: HashMap<usize, ID3D11ShaderResourceView>,
}

const MAX_CACHED_POOL_SRVS: usize = 16;

impl GpuPool {
    fn srv_for(
        &mut self,
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
    ) -> Result<ID3D11ShaderResourceView, String> {
        let key = source.as_raw() as usize;
        if let Some(srv) = self.srvs.get(&key) {
            return Ok(srv.clone());
        }
        if self.srvs.len() >= MAX_CACHED_POOL_SRVS {
            self.srvs.clear();
        }
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: self.srv_format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv = None;
        unsafe { device.CreateShaderResourceView(source, Some(&desc), Some(&mut srv)) }
            .map_err(|e| format!("建快照 SRV 失败: {e}"))?;
        let srv = srv.ok_or("CreateShaderResourceView 返回空")?;
        self.srvs.insert(key, srv.clone());
        Ok(srv)
    }
}

/// TYPELESS 深度家族 → 可读 float 的 SRV 格式。fully-typed 深度资源建不了 SRV，
/// 明确拒绝（fail-closed），不猜。Fallout 4 实测是 R24G8_TYPELESS（#583 证据）。
fn pool_srv_format(source: DXGI_FORMAT) -> Result<DXGI_FORMAT, String> {
    match source {
        f if f == DXGI_FORMAT_R24G8_TYPELESS => Ok(DXGI_FORMAT_R24_UNORM_X8_TYPELESS),
        f if f == DXGI_FORMAT_R32_TYPELESS => Ok(DXGI_FORMAT_R32_FLOAT),
        f if f == DXGI_FORMAT_R32G8X24_TYPELESS => Ok(DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS),
        other => Err(format!(
            "快照格式 {other:?} 不是 TYPELESS 深度家族，无法建 SRV 做 GPU 池化"
        )),
    }
}

fn make_gpu_pool(
    device: &ID3D11Device,
    src_desc: &D3D11_TEXTURE2D_DESC,
    dst_w: usize,
    dst_h: usize,
    conv: ZConvention,
) -> Result<GpuPool, String> {
    let srv_format = pool_srv_format(src_desc.Format)?;
    let dxbc = pool_shader_dxbc()?;
    let mut shader = None;
    unsafe { device.CreateComputeShader(&dxbc, None, Some(&mut shader)) }
        .map_err(|e| format!("建池化 CS 失败: {e}"))?;
    let shader = shader.ok_or("CreateComputeShader 返回空")?;

    let params_data: [u32; 8] = [
        src_desc.Width,
        src_desc.Height,
        u32::try_from(dst_w).map_err(|_| "dst_w 溢出")?,
        u32::try_from(dst_h).map_err(|_| "dst_h 溢出")?,
        u32::from(matches!(conv, ZConvention::Reversed)),
        0,
        0,
        0,
    ];
    let params_desc = D3D11_BUFFER_DESC {
        ByteWidth: (size_of::<[u32; 8]>()) as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let params_init = D3D11_SUBRESOURCE_DATA {
        pSysMem: params_data.as_ptr().cast(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut params = None;
    unsafe { device.CreateBuffer(&params_desc, Some(&params_init), Some(&mut params)) }
        .map_err(|e| format!("建池化参数 CB 失败: {e}"))?;
    let params = params.ok_or("CreateBuffer 返回空")?;

    let pool_desc = D3D11_TEXTURE2D_DESC {
        Width: u32::try_from(dst_w).map_err(|_| "dst_w 溢出")?,
        Height: u32::try_from(dst_h).map_err(|_| "dst_h 溢出")?,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R32_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // R32_FLOAT typed UAV store 是 FL11_0 的保证能力，不需要 cap 查询。
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut pool_tex = None;
    unsafe { device.CreateTexture2D(&pool_desc, None, Some(&mut pool_tex)) }
        .map_err(|e| format!("建池化输出纹理失败: {e}"))?;
    let pool_tex = pool_tex.ok_or("CreateTexture2D 返回空池化纹理")?;
    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_FLOAT,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut pool_uav = None;
    unsafe { device.CreateUnorderedAccessView(&pool_tex, Some(&uav_desc), Some(&mut pool_uav)) }
        .map_err(|e| format!("建池化 UAV 失败: {e}"))?;
    let pool_uav = pool_uav.ok_or("CreateUnorderedAccessView 返回空")?;

    Ok(GpuPool {
        shader,
        params,
        pool_tex,
        pool_uav,
        srv_format,
        srvs: HashMap::new(),
    })
}

/// Dispatch 期间借用 immediate context 的 CS 绑定，Drop 时无条件还原。
/// 游戏在 Present 边界的 CS 状态通常是空，但「通常」不配进正确性论证——
/// Get/Set 各 4 对，代价 µs 级，换来对引擎状态零假设。
struct CsStateGuard<'a> {
    context: &'a ID3D11DeviceContext,
    shader: Option<ID3D11ComputeShader>,
    class_instances: [Option<ID3D11ClassInstance>; 0],
    srv: [Option<ID3D11ShaderResourceView>; 1],
    uav: [Option<ID3D11UnorderedAccessView>; 1],
    cb: [Option<ID3D11Buffer>; 1],
}

impl<'a> CsStateGuard<'a> {
    fn save(context: &'a ID3D11DeviceContext) -> Self {
        let mut shader = None;
        let mut srv = [None];
        let mut uav = [None];
        let mut cb = [None];
        unsafe {
            context.CSGetShader(&mut shader, None, None);
            context.CSGetShaderResources(0, Some(&mut srv));
            context.CSGetUnorderedAccessViews(0, Some(&mut uav));
            context.CSGetConstantBuffers(0, Some(&mut cb));
        }
        Self {
            context,
            shader,
            class_instances: [],
            srv,
            uav,
            cb,
        }
    }
}

impl Drop for CsStateGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            self.context
                .CSSetShader(self.shader.as_ref(), Some(&self.class_instances));
            self.context.CSSetShaderResources(0, Some(&self.srv));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(self.uav.as_ptr()), Some(&u32::MAX));
            self.context.CSSetConstantBuffers(0, Some(&self.cb));
        }
    }
}

/// 在渲染线程上把快照池化到 `pool_tex`（720p R32_FLOAT）。
/// 调用方随后把 `pool_tex` CopyResource 进小 staging；GPU 侧总量 ~0.05 ms@5070Ti。
fn dispatch_pool(
    gpu: &mut GpuPool,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    dst_w: usize,
    dst_h: usize,
) -> Result<(), String> {
    let srv = gpu.srv_for(device, source)?;
    let _guard = CsStateGuard::save(context);
    unsafe {
        context.CSSetShader(&gpu.shader, None);
        context.CSSetShaderResources(0, Some(&[Some(srv)]));
        context.CSSetConstantBuffers(0, Some(&[Some(gpu.params.clone())]));
        let uav = [Some(gpu.pool_uav.clone())];
        context.CSSetUnorderedAccessViews(0, 1, Some(uav.as_ptr()), Some(&u32::MAX));
        let groups_x = u32::try_from(dst_w.div_ceil(8)).map_err(|_| "dispatch x 溢出")?;
        let groups_y = u32::try_from(dst_h.div_ceil(8)).map_err(|_| "dispatch y 溢出")?;
        context.Dispatch(groups_x, groups_y, 1);
    }
    // guard drop 还原 CS 绑定（含把 u0 还给引擎原值），pool_tex 随之脱离 UAV 绑定，
    // 后续 CopyResource 以它为 src 不构成 hazard。
    Ok(())
}

/// 把调用方已经选定时机的深度资源拷进一个空闲 staging 槽。
///
/// `present_seq` 必须与该资源快照所属的水印序号一致。
///
/// 返回 `Ok(false)` 表示池满、这一帧的 depth 被主动丢弃（§5.4「宁可丢 depth，不可卡游戏」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateGpuAllocationStats {
    pub capacity: u32,
    pub texture_bytes: u64,
    pub estimated_bytes: u64,
    pub configured_limit_bytes: u64,
    pub budget_before: u64,
    pub usage_before: u64,
    pub headroom_before: u64,
    pub budget_after: u64,
    pub usage_after: u64,
    pub headroom_after: u64,
    pub actual_usage_delta: u64,
    pub configured_reserve_bytes: u64,
    pub budget_at_flush: Option<u64>,
    pub usage_at_flush: Option<u64>,
    pub headroom_at_flush: Option<u64>,
}

/// GPU-only R32_FLOAT candidate resources. Materialization never allocates a
/// staging resource and never reads data back to the CPU.
pub struct GpuCandidateRing {
    gpu: GpuPool,
    textures: Vec<ID3D11Texture2D>,
    src_desc: D3D11_TEXTURE2D_DESC,
    dst_w: usize,
    dst_h: usize,
    stats: CandidateGpuAllocationStats,
}

// SAFETY: the owner serializes access under the Present-run lock.
unsafe impl Send for GpuCandidateRing {}

fn candidate_allocation_estimate(
    texture_bytes: u64,
    capacity: u32,
    staging_slots: usize,
) -> Option<u64> {
    u64::from(capacity)
        .checked_add(1)
        .and_then(|count| count.checked_add(staging_slots as u64))
        .and_then(|count| texture_bytes.checked_mul(count))
}

/// Force every candidate and staging allocation through the game's immediate context before the
/// post-allocation DXGI snapshot is taken.
///
/// `CreateTexture2D` is lazy on the NVIDIA driver used by the certification machine: immediately
/// querying `CurrentUsage` after creating 96 textures repeatedly reported a zero-byte delta.  The
/// old code therefore waited until the five-minute Flush and attributed every allocation the game
/// made during the route to the depth ring.  Prime the exact resources here and block on the last
/// staging copy once, so `actual_usage_delta` covers a millisecond-scale, depth-owned allocation
/// window instead of the whole run.
// `dst_w`/`dst_h` 收成一个元组：它们永远成对出现，拆成两个参数除了让签名
// 越过 clippy 的 7 参上限，也让调用点多一次「别把宽高写反」的机会。
fn prime_candidate_allocations(
    gpu: &mut GpuPool,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    textures: &[ID3D11Texture2D],
    staging: &[ID3D11Texture2D],
    (dst_w, dst_h): (usize, usize),
) -> Result<(), String> {
    dispatch_pool(gpu, device, context, source, dst_w, dst_h)?;
    let src: ID3D11Resource = gpu.pool_tex.clone().into();
    for texture in textures {
        let dst: ID3D11Resource = texture.clone().into();
        unsafe { context.CopyResource(&dst, &src) };
    }
    for texture in staging {
        let dst: ID3D11Resource = texture.clone().into();
        unsafe { context.CopyResource(&dst, &src) };
    }

    // A blocking Map is deliberately restricted to this one-time allocation bootstrap.  Mapping
    // the last staging texture proves all preceding copies completed in immediate-context order;
    // the per-frame capture path remains DO_NOT_WAIT and never blocks the render thread.
    let last_staging: ID3D11Resource = staging
        .last()
        .ok_or("candidate allocation priming has no staging texture")?
        .clone()
        .into();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&last_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|error| format!("candidate allocation priming Map: {error}"))?;
    unsafe { context.Unmap(&last_staging, 0) };
    Ok(())
}

impl GpuCandidateRing {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
        capacity: u32,
        dst_w: usize,
        dst_h: usize,
        conv: ZConvention,
        vram_limit_bytes: u64,
        reserve_bytes: u64,
        staging_slots: usize,
        readback_deadline_ticks: u64,
    ) -> Result<Self, String> {
        if capacity == 0 || dst_w == 0 || dst_h == 0 {
            return Err("candidate ring geometry/capacity is zero".into());
        }
        let texture_bytes = u64::try_from(dst_w)
            .ok()
            .and_then(|w| u64::try_from(dst_h).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("candidate ring byte size overflow")?;
        let staging_slots = staging_slots.max(ReadbackPool::MIN_SLOTS);
        // Include N candidate slots, the shared pool-shader output, and every selected-readback
        // staging texture. The exact full-resolution scratch already exists at `memory` and is
        // therefore part of usage_before rather than double-counted here.
        let estimated_bytes = candidate_allocation_estimate(texture_bytes, capacity, staging_slots)
            .ok_or("candidate ring allocation estimate overflow")?;
        if estimated_bytes > vram_limit_bytes {
            return Err(format!(
                "candidate allocation {estimated_bytes} exceeds configured VRAM limit {vram_limit_bytes}"
            ));
        }

        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|e| format!("D3D device has no IDXGIDevice: {e}"))?;
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|e| format!("IDXGIDevice::GetAdapter: {e}"))?;
        let adapter3: IDXGIAdapter3 = adapter
            .cast()
            .map_err(|e| format!("DXGI adapter has no video-memory budget interface: {e}"))?;
        let mut memory = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe { adapter3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut memory) }
            .map_err(|e| format!("QueryVideoMemoryInfo(local): {e}"))?;
        let required_headroom = estimated_bytes
            .checked_add(reserve_bytes)
            .ok_or("candidate allocation + reserve overflow")?;
        let headroom = memory.Budget.saturating_sub(memory.CurrentUsage);
        if required_headroom > headroom {
            return Err(format!(
                "candidate allocation needs {required_headroom} bytes headroom (allocation={estimated_bytes} reserve={reserve_bytes}), DXGI reports {headroom} (budget={} usage={})",
                memory.Budget, memory.CurrentUsage
            ));
        }

        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut src_desc) };
        let mut gpu = make_gpu_pool(device, &src_desc, dst_w, dst_h, conv)?;
        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: u32::try_from(dst_w).map_err(|_| "candidate width overflow")?,
            Height: u32::try_from(dst_h).map_err(|_| "candidate height overflow")?,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut textures = Vec::with_capacity(capacity as usize);
        for slot in 0..capacity {
            let mut texture = None;
            unsafe { device.CreateTexture2D(&texture_desc, None, Some(&mut texture)) }
                .map_err(|e| format!("candidate slot {slot} allocation failed: {e}"))?;
            textures.push(
                texture.ok_or_else(|| format!("candidate slot {slot} allocation returned null"))?,
            );
        }
        // Eager creation gives the allocation snapshot a deterministic boundary and prevents the
        // first Select from hiding four staging allocations outside the VRAM evidence.
        let mut staging = Vec::with_capacity(staging_slots);
        for _ in 0..staging_slots {
            staging.push(make_staging_sized(
                device,
                texture_desc.Width,
                texture_desc.Height,
                texture_desc.Format,
            )?);
        }
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|error| format!("candidate allocation GetImmediateContext: {error}"))?;
        prime_candidate_allocations(
            &mut gpu,
            device,
            &context,
            source,
            &textures,
            &staging,
            (dst_w, dst_h),
        )?;
        let mut memory_after = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe {
            adapter3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut memory_after)
        }
        .map_err(|e| format!("QueryVideoMemoryInfo(local) after allocation: {e}"))?;
        let headroom_after = memory_after
            .Budget
            .saturating_sub(memory_after.CurrentUsage);
        let actual_usage_delta = memory_after
            .CurrentUsage
            .saturating_sub(memory.CurrentUsage);
        if headroom_after < reserve_bytes {
            return Err(format!(
                "candidate allocation left {headroom_after} bytes DXGI headroom, below configured reserve {reserve_bytes} (budget={} usage={})",
                memory_after.Budget, memory_after.CurrentUsage
            ));
        }
        if actual_usage_delta > vram_limit_bytes {
            return Err(format!(
                "candidate allocation increased DXGI usage by {actual_usage_delta} bytes, above configured VRAM limit {vram_limit_bytes}"
            ));
        }
        let mut readback_state = STATE
            .lock()
            .map_err(|_| "readback state lock poisoned during eager allocation")?;
        if readback_state
            .as_ref()
            .is_some_and(|state| state.pool.in_flight() != 0)
        {
            return Err("selected readback state still has frames in flight".into());
        }
        if readback_state.is_some() {
            return Err("selected readback state was not reset before candidate allocation".into());
        }
        *readback_state = Some(ReadbackState {
            staging,
            pool: ReadbackPool::new(staging_slots, readback_deadline_ticks),
            partials: VecDeque::new(),
            ready_frames: HashMap::new(),
            src_desc: texture_desc,
            format: DepthFormat::D32Float,
            dst_w,
            dst_h,
            conv,
            gpu: None,
            gpu_error: None,
            verify: false,
            verify_staging: None,
            verify_in_flight: None,
            verify_skipped: 0,
            pooled_direct: true,
        });
        Ok(Self {
            gpu,
            textures,
            src_desc,
            dst_w,
            dst_h,
            stats: CandidateGpuAllocationStats {
                capacity,
                texture_bytes,
                estimated_bytes,
                configured_limit_bytes: vram_limit_bytes,
                budget_before: memory.Budget,
                usage_before: memory.CurrentUsage,
                headroom_before: headroom,
                budget_after: memory_after.Budget,
                usage_after: memory_after.CurrentUsage,
                headroom_after,
                actual_usage_delta,
                configured_reserve_bytes: reserve_bytes,
                budget_at_flush: None,
                usage_at_flush: None,
                headroom_at_flush: None,
            },
        })
    }

    pub fn materialize(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        slot: usize,
    ) -> Result<(), String> {
        let destination = self
            .textures
            .get(slot)
            .ok_or_else(|| format!("candidate slot {slot} is outside the ring"))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Width != self.src_desc.Width
            || desc.Height != self.src_desc.Height
            || desc.Format != self.src_desc.Format
            || desc.SampleDesc.Count != self.src_desc.SampleDesc.Count
            || desc.SampleDesc.Quality != self.src_desc.SampleDesc.Quality
        {
            return Err("depth source geometry/resource format changed after Ready".into());
        }
        dispatch_pool(
            &mut self.gpu,
            device,
            context,
            source,
            self.dst_w,
            self.dst_h,
        )?;
        let dst: ID3D11Resource = destination.clone().into();
        let src: ID3D11Resource = self.gpu.pool_tex.clone().into();
        unsafe { context.CopyResource(&dst, &src) };
        Ok(())
    }

    pub fn texture(&self, slot: usize) -> Result<ID3D11Texture2D, String> {
        self.textures
            .get(slot)
            .cloned()
            .ok_or_else(|| format!("candidate slot {slot} is outside the ring"))
    }

    #[must_use]
    pub const fn stats(&self) -> CandidateGpuAllocationStats {
        self.stats
    }

    /// Record run-end headroom without re-attributing unrelated game allocations to the depth ring.
    /// Lazy commitment was already forced and measured by `prime_candidate_allocations`.
    pub fn refresh_final_stats(&mut self) -> Result<(), String> {
        let texture = self
            .textures
            .first()
            .ok_or("candidate ring has no texture for final VRAM query")?;
        let device = unsafe { texture.GetDevice() }
            .map_err(|error| format!("candidate texture GetDevice: {error}"))?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("D3D device has no IDXGIDevice: {error}"))?;
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|error| format!("IDXGIDevice::GetAdapter: {error}"))?;
        let adapter3: IDXGIAdapter3 = adapter.cast().map_err(|error| {
            format!("DXGI adapter has no video-memory budget interface: {error}")
        })?;
        let mut memory = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe { adapter3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut memory) }
            .map_err(|error| format!("final QueryVideoMemoryInfo(local): {error}"))?;
        let headroom = memory.Budget.saturating_sub(memory.CurrentUsage);
        self.stats.budget_at_flush = Some(memory.Budget);
        self.stats.usage_at_flush = Some(memory.CurrentUsage);
        self.stats.headroom_at_flush = Some(headroom);
        if headroom < self.stats.configured_reserve_bytes {
            return Err(format!(
                "final GPU headroom {headroom} bytes is below reserve {} (budget={} usage={})",
                self.stats.configured_reserve_bytes, memory.Budget, memory.CurrentUsage
            ));
        }
        Ok(())
    }
}

/// Submit one already-pooled candidate to the asynchronous staging pool. No
/// pool shader dispatch occurs in this function.
pub fn capture_pooled(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    present_seq: u64,
    cfg: CaptureConfig,
) -> Result<bool, String> {
    let _perf = crate::perf::scope(crate::perf::Section::SelectedReadback);
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_R32_FLOAT
        || desc.Width != u32::try_from(cfg.dst_w).map_err(|_| "dst_w overflow")?
        || desc.Height != u32::try_from(cfg.dst_h).map_err(|_| "dst_h overflow")?
        || desc.SampleDesc.Count != 1
    {
        return Err(
            "selected candidate is not the configured single-sample R32_FLOAT texture".into(),
        );
    }

    let mut guard = STATE.lock().map_err(|_| "readback state lock poisoned")?;
    let needs_rebuild = guard.as_ref().is_none_or(|state| {
        !state.pooled_direct
            || state.src_desc.Width != desc.Width
            || state.src_desc.Height != desc.Height
            || state.src_desc.Format != desc.Format
            || state.dst_w != cfg.dst_w
            || state.dst_h != cfg.dst_h
            || state.conv != cfg.conv
    });
    if needs_rebuild {
        if guard
            .as_ref()
            .is_some_and(|state| state.pool.in_flight() != 0)
        {
            return Err("readback resources changed with selected frames in flight".into());
        }
        let n = cfg.slots.max(ReadbackPool::MIN_SLOTS);
        let mut staging = Vec::with_capacity(n);
        for _ in 0..n {
            staging.push(make_staging_sized(
                device,
                desc.Width,
                desc.Height,
                desc.Format,
            )?);
        }
        *guard = Some(ReadbackState {
            staging,
            pool: ReadbackPool::new(n, cfg.deadline_ticks),
            partials: VecDeque::new(),
            ready_frames: HashMap::new(),
            src_desc: desc,
            format: DepthFormat::D32Float,
            dst_w: cfg.dst_w,
            dst_h: cfg.dst_h,
            conv: cfg.conv,
            gpu: None,
            gpu_error: None,
            verify: false,
            verify_staging: None,
            verify_in_flight: None,
            verify_skipped: 0,
            pooled_direct: true,
        });
    }

    let state = guard
        .as_mut()
        .expect("direct readback state was initialized");
    let accepted = match state.pool.issue(present_seq) {
        Issue::Accepted { slot } => {
            let dst: ID3D11Resource = state.staging[slot].clone().into();
            let src: ID3D11Resource = source.clone().into();
            unsafe { context.CopyResource(&dst, &src) };
            true
        }
        Issue::DroppedPoolFull => false,
    };
    IN_FLIGHT.store(state.pool.in_flight(), Ordering::Release);
    Ok(accepted)
}

pub fn capture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    present_seq: u64,
    cfg: CaptureConfig,
) -> Result<bool, String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    let format = classify(desc.Format)
        .ok_or_else(|| format!("不认识的深度格式 {:?}，拒绝回读", desc.Format))?;

    let mut guard = STATE.lock().map_err(|_| "回读状态锁已中毒")?;

    // 几何或格式变了（分辨率切换、alt-tab 后重建）→ 整池重建。
    let needs_rebuild = match guard.as_ref() {
        None => true,
        Some(st) => {
            st.pooled_direct
                || st.src_desc.Width != desc.Width
                || st.src_desc.Height != desc.Height
                || st.src_desc.Format != desc.Format
        }
    };
    if needs_rebuild {
        if guard
            .as_ref()
            .is_some_and(|state| state.pool.in_flight() != 0)
        {
            return Err("readback geometry changed with frames in flight".into());
        }
        let verify = verify_mode();
        // GPU 池化是正常模式的**唯一**路径：建失败只记录，不静默退回全量回读；
        // 后续每个证据帧的 capture 都会 Err，帧被丢弃、run 以 0 rows 判废（fail-closed）。
        let (gpu, gpu_error) = match make_gpu_pool(device, &desc, cfg.dst_w, cfg.dst_h, cfg.conv) {
            Ok(pool) => (Some(pool), None),
            Err(error) => (None, Some(error)),
        };
        let dst_w_u32 = u32::try_from(cfg.dst_w).map_err(|_| "dst_w 溢出".to_string())?;
        let dst_h_u32 = u32::try_from(cfg.dst_h).map_err(|_| "dst_h 溢出".to_string())?;
        let n = cfg.slots.max(ReadbackPool::MIN_SLOTS);
        let mut staging = Vec::with_capacity(n);
        for _ in 0..n {
            if verify {
                // VERIFY：交付走 CPU 参考，池里是全量源尺寸 staging。
                staging.push(make_staging(device, &desc)?);
            } else {
                // 正常：池里只剩 720p R32_FLOAT，渲染线程每证据帧的 copy 从 8.3 MB 降到 3.7 MB。
                staging.push(make_staging_sized(
                    device,
                    dst_w_u32,
                    dst_h_u32,
                    DXGI_FORMAT_R32_FLOAT,
                )?);
            }
        }
        let verify_staging = if verify && gpu.is_some() {
            Some(make_staging_sized(
                device,
                dst_w_u32,
                dst_h_u32,
                DXGI_FORMAT_R32_FLOAT,
            )?)
        } else {
            None
        };
        *guard = Some(ReadbackState {
            staging,
            pool: ReadbackPool::new(n, cfg.deadline_ticks),
            partials: VecDeque::new(),
            ready_frames: HashMap::new(),
            src_desc: desc,
            format,
            dst_w: cfg.dst_w,
            dst_h: cfg.dst_h,
            conv: cfg.conv,
            gpu,
            gpu_error,
            verify,
            verify_staging,
            verify_in_flight: None,
            verify_skipped: 0,
            pooled_direct: false,
        });
    }

    let st = guard.as_mut().expect("刚建过");
    if !st.verify && st.gpu.is_none() {
        return Err(st
            .gpu_error
            .clone()
            .unwrap_or_else(|| "GPU 池化未初始化".to_string()));
    }
    let accepted = match st.pool.issue(present_seq) {
        Issue::Accepted { slot } => {
            if st.verify {
                // 交付：全量拷贝，writer 走 CPU 参考解码（黄金路径）。
                let dst: ID3D11Resource = st.staging[slot].clone().into();
                let src: ID3D11Resource = source.clone().into();
                unsafe { context.CopyResource(&dst, &src) };
                // 影子：GPU 池化到独立单槽。影子失败绝不能污染交付——记录后停用影子。
                if st.verify_in_flight.is_some() {
                    st.verify_skipped = st.verify_skipped.saturating_add(1);
                } else if let (Some(gpu), Some(shadow)) =
                    (st.gpu.as_mut(), st.verify_staging.clone())
                {
                    match dispatch_pool(gpu, device, context, source, st.dst_w, st.dst_h) {
                        Ok(()) => {
                            let dst: ID3D11Resource = shadow.into();
                            let src: ID3D11Resource = gpu.pool_tex.clone().into();
                            unsafe { context.CopyResource(&dst, &src) };
                            st.verify_in_flight = Some(present_seq);
                        }
                        Err(error) => {
                            st.gpu_error = Some(error);
                            st.verify_staging = None;
                        }
                    }
                }
                true
            } else {
                let _perf = crate::perf::scope(crate::perf::Section::PoolDispatch);
                let gpu = st.gpu.as_mut().expect("正常模式上方已判 gpu 存在");
                dispatch_pool(gpu, device, context, source, st.dst_w, st.dst_h)?;
                let dst: ID3D11Resource = st.staging[slot].clone().into();
                let src: ID3D11Resource = gpu.pool_tex.clone().into();
                unsafe { context.CopyResource(&dst, &src) };
                true
            }
        }
        Issue::DroppedPoolFull => false,
    };
    IN_FLIGHT.store(st.pool.in_flight(), Ordering::Release);
    Ok(accepted)
}

/// Non-blockingly advance the readback: probe in-flight slots, slice out ready ones.
///
/// `Map(DO_NOT_WAIT)` is the readiness query. A busy slot remains in flight for a later Present.
/// A ready slot bigger than the slice budget moves to `Draining` and is copied out **one slice
/// per Present**（#504：整块 3.7 MB memcpy 的 0.3 ms+ 定额税正是 1% low 的缺口来源）；
/// each slice is a complete `Map` → copy → `Unmap` lifetime——no mapped pointer crosses frames.
/// Smaller (or VERIFY) slots are still copied whole. When the drain queue backs up (≥2 slots,
/// low-frame-rate presents with ample budget), the head is flushed in one go so the pool
/// never starves.
pub fn drain_ready(context: &ID3D11DeviceContext) -> Result<Vec<RawDepthFrame>, String> {
    drain_ready_with_slice(context, drain_slice_bytes())
}

fn drain_ready_with_slice(
    context: &ID3D11DeviceContext,
    slice_bytes: usize,
) -> Result<Vec<RawDepthFrame>, String> {
    let mut guard = STATE.lock().map_err(|_| "回读状态锁已中毒")?;
    let Some(st) = guard.as_mut() else {
        return Ok(Vec::new());
    };
    st.pool.advance_tick();

    // —— 第一步：推进分片队头（每个 Present 至多拷一片）。 ——
    // 宽限回收过的队头直接丢弃：池已计 drain_aborted，半成品绝不交付。
    while let Some(front) = st.partials.front() {
        if st.pool.is_draining(front.slot, front.present_seq) {
            break;
        }
        let dead = st.partials.pop_front().expect("front 刚检查过存在");
        recycle_buffer(dead.bytes);
    }
    let queue_depth = st.partials.len();
    let mut copied_this_present = false;
    if let Some(front) = st.partials.front_mut() {
        let res: ID3D11Resource = st.staging[front.slot].clone().into();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let hr = {
            let _perf = crate::perf::scope(crate::perf::Section::DrainMap);
            unsafe {
                context.Map(
                    &res,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            }
        };
        match hr {
            // 进得了 Draining 说明该槽曾 Map 成功过，规范下不会再 STILL_DRAWING；
            // 万一驱动这么答，就等下一个 Present 重试——数据没坏，只是没就绪。
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {}
            Err(error) => {
                let dead = st.partials.pop_front().expect("front 仍在");
                st.pool.abort_drain(dead.slot);
                let failed_slot = dead.slot;
                recycle_buffer(dead.bytes);
                return Err(format!("分片重 Map staging 槽 {failed_slot} 失败: {error}"));
            }
            Ok(()) => {
                let observed_pitch = mapped.RowPitch as usize;
                if observed_pitch != front.row_pitch {
                    unsafe { context.Unmap(&res, 0) };
                    let dead = st.partials.pop_front().expect("front 仍在");
                    st.pool.abort_drain(dead.slot);
                    let (failed_slot, first_pitch) = (dead.slot, dead.row_pitch);
                    recycle_buffer(dead.bytes);
                    return Err(format!(
                        "staging 槽 {failed_slot} 的 RowPitch 跨 Map 漂移: 首片 {first_pitch} 本片 {observed_pitch}"
                    ));
                }
                // 泄洪：队列积压说明分片消费落后于 15 Hz 生产（低帧率场景，
                // Present 预算充裕），一口气拷完队头，池子不挨饿。
                let budget = if queue_depth >= 2 {
                    front.len - front.done
                } else {
                    slice_bytes
                };
                let n = budget.min(front.len - front.done);
                {
                    let _perf = crate::perf::scope(crate::perf::Section::ReadbackMemcpy);
                    // SAFETY: Map 成功后 `pData` 至少 `len` 字节有效；目标缓冲容量为
                    // `len`，`[done, done+n)` 在界内；拷贝在下方 `Unmap` 之前完成。
                    unsafe {
                        copy_bytes_into(
                            mapped.pData.cast::<u8>().add(front.done),
                            front.bytes.as_mut_ptr().add(front.done),
                            n,
                        );
                    }
                }
                unsafe { context.Unmap(&res, 0) };
                front.done += n;
                copied_this_present = true;
                if front.done == front.len {
                    let part = st.partials.pop_front().expect("front 仍在");
                    let slot = part.slot;
                    st.buffer_partial_frame(
                        slot,
                        RawDepthFrame {
                            present_seq: part.present_seq,
                            src_width: st.src_desc.Width as usize,
                            src_height: st.src_desc.Height as usize,
                            row_pitch: part.row_pitch,
                            format: st.format,
                            dst_width: st.dst_w,
                            dst_height: st.dst_h,
                            conv: st.conv,
                            bytes: part.bytes,
                            // 分片只发生在正常 GPU 池化路径（VERIFY 恒走整块拷出）。
                            pooled: true,
                            verify_pooled: None,
                            verify_row_pitch: 0,
                        },
                    )?;
                }
            }
        }
    }

    // —— 第二步：探测在飞槽位；就绪的大帧转入分片，小帧/VERIFY 帧整块拷出。 ——
    for (slot, present_seq) in st.pool.in_flight_slots() {
        let res: ID3D11Resource = st.staging[slot].clone().into();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let hr = {
            let _perf = crate::perf::scope(crate::perf::Section::DrainMap);
            unsafe {
                context.Map(
                    &res,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            }
        };
        match hr {
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => continue,
            Err(error) => return Err(format!("Map staging 槽 {slot} 失败: {error}")),
            Ok(()) => {}
        }

        // 池里的字节：正常模式是 GPU 池化后的 720p R32_FLOAT，VERIFY 模式是全量源尺寸。
        let (row_width, row_height, bytes_per_pixel) = if st.verify {
            (
                st.src_desc.Width as usize,
                st.src_desc.Height as usize,
                st.format.bytes_per_pixel(),
            )
        } else {
            (st.dst_w, st.dst_h, 4)
        };
        let row_pitch = mapped.RowPitch as usize;
        let len = row_pitch * (row_height - 1) + row_width * bytes_per_pixel;

        // 大帧转入分片拷出：槽位改挂 `Draining`（脱离 InFlight deadline、不可被
        // issue 复用、不再参与就绪探测），之后每个 Present 拷一片。
        // VERIFY 恒走整块路径——影子配对逻辑绑定在「大帧就绪的同一 Present」上。
        if !st.verify && len > slice_bytes {
            if !st.pool.begin_drain(slot) {
                // 槽位恰在本 tick 被回收（advance_tick 已过，理论上不会）——放弃这帧。
                unsafe { context.Unmap(&res, 0) };
                continue;
            }
            let mut bytes = take_buffer(len);
            // SAFETY: 容量至少 `len`。`[0, done)` 由本片与后续 Present 的分片逐段
            // 初始化；交付前 `done == len`（全初始化）；未完成的缓冲只会被
            // recycle（只复用容量），绝不会被读。
            unsafe { bytes.set_len(len) };
            let mut done = 0usize;
            if !copied_this_present {
                let n = slice_bytes.min(len);
                let _perf = crate::perf::scope(crate::perf::Section::ReadbackMemcpy);
                // SAFETY: Map 成功后 `pData` 至少 `len` 字节有效；拷贝在下方 `Unmap` 前完成。
                unsafe { copy_bytes_into(mapped.pData.cast::<u8>(), bytes.as_mut_ptr(), n) };
                done = n;
                copied_this_present = true;
            }
            unsafe { context.Unmap(&res, 0) };
            st.partials.push_back(PartialDrain {
                slot,
                present_seq,
                bytes,
                done,
                len,
                row_pitch,
            });
            continue;
        }

        let bytes = {
            let _perf = crate::perf::scope(crate::perf::Section::ReadbackMemcpy);
            // SAFETY: the staging resource remains mapped until the copy returns immediately below.
            unsafe { copy_mapped_bytes(mapped.pData.cast(), len) }
        };
        unsafe { context.Unmap(&res, 0) };

        // VERIFY 影子回读：大帧与影子同帧按序提交（大 copy → dispatch → 小 copy），
        // 大帧就绪时影子几乎必然就绪；万一 STILL_DRAWING，放弃本帧影子而不是拖交付
        // ——配对一旦跨帧就散了，宁可 skip 计数也不引入等待。
        let mut verify_pooled = None;
        let mut verify_row_pitch = 0usize;
        if st.verify && st.verify_in_flight == Some(present_seq) {
            st.verify_in_flight = None;
            if let Some(shadow_tex) = st.verify_staging.clone() {
                let shadow_res: ID3D11Resource = shadow_tex.into();
                let mut shadow_mapped = D3D11_MAPPED_SUBRESOURCE::default();
                let shadow_hr = unsafe {
                    context.Map(
                        &shadow_res,
                        0,
                        D3D11_MAP_READ,
                        D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                        Some(&mut shadow_mapped),
                    )
                };
                match shadow_hr {
                    Ok(()) => {
                        let pitch = shadow_mapped.RowPitch as usize;
                        let shadow_len = pitch * (st.dst_h - 1) + st.dst_w * 4;
                        // SAFETY: Map 成功后 pData 至少 shadow_len 字节，Unmap 前使用。
                        let shadow_bytes = unsafe {
                            std::slice::from_raw_parts(shadow_mapped.pData.cast::<u8>(), shadow_len)
                        }
                        .to_vec();
                        unsafe { context.Unmap(&shadow_res, 0) };
                        verify_pooled = Some(shadow_bytes);
                        verify_row_pitch = pitch;
                    }
                    Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                        st.verify_skipped = st.verify_skipped.saturating_add(1);
                    }
                    Err(error) => {
                        st.gpu_error = Some(format!("影子 staging Map 失败: {error}"));
                        st.verify_staging = None;
                    }
                }
            }
        }

        st.buffer_whole_frame(
            slot,
            RawDepthFrame {
                present_seq,
                src_width: st.src_desc.Width as usize,
                src_height: st.src_desc.Height as usize,
                row_pitch,
                format: st.format,
                dst_width: st.dst_w,
                dst_height: st.dst_h,
                conv: st.conv,
                bytes,
                pooled: !st.verify,
                verify_pooled,
                verify_row_pitch,
            },
        )?;
    }

    let frames = st.take_ordered_ready_frames()?;
    IN_FLIGHT.store(st.pool.in_flight(), Ordering::Release);
    Ok(frames)
}

/// GPU 回读层统计。
///
/// **writer 队列的丢弃发生在本层之后**，所以这里的 `completed` 只说明
/// 「GPU 回读成功了」，不代表帧真的落了盘。合并见 `probe::delivery_stats`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadbackStats {
    pub drops: depth_lab::readback::DropStats,
    pub completed: u64,
}

/// 丢帧与回读完成数，供 Overlay 实时显示（§6.3）。
pub fn stats() -> Option<ReadbackStats> {
    let guard = STATE.lock().ok()?;
    let st = guard.as_ref()?;
    Some(ReadbackStats {
        drops: st.pool.stats(),
        completed: st.pool.completed(),
    })
}

#[must_use]
pub fn in_flight() -> usize {
    IN_FLIGHT.load(Ordering::Acquire)
}

#[must_use]
pub fn has_in_flight() -> bool {
    IN_FLIGHT.load(Ordering::Acquire) != 0
}

/// 当前仍由回读池持有的帧号——**含分片拷出和等待按序交付的槽位**：这些帧尚未
/// 交给 writer，pass 证据必须活到真正交付。调用方用它清理与 readback 同生命周期的
/// 旁路证据；已超时或因池重建丢弃的帧不会出现在这里。
#[must_use]
pub fn in_flight_present_seqs() -> Vec<u64> {
    STATE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|state| state.pool.held_present_seqs()))
        .unwrap_or_default()
}

/// A new generation may only discard the old D3D resources after the previous
/// generation has drained. Returning false keeps StartRun fail-closed.
pub fn reset_if_drained() -> bool {
    let Ok(mut guard) = STATE.lock() else {
        return false;
    };
    if guard
        .as_ref()
        .is_some_and(|state| state.pool.in_flight() != 0)
    {
        return false;
    }
    *guard = None;
    IN_FLIGHT.store(0, Ordering::Release);
    true
}

/// Explicitly discard every outstanding readback after the owning generation
/// has already become terminally faulted.
///
/// This is deliberately separate from [`reset_if_drained`]: normal StopRun
/// must drain and deliver every selected frame, while a faulted run is
/// non-deliverable and must not leave queued D3D resources pinning the global
/// state forever. Dropping the COM resources is safe even when CopyResource is
/// queued; the D3D11 runtime retains command-list references until GPU use is
/// complete. Callers must exclude the Present hook while invoking this.
pub fn abort_faulted_generation() -> Result<Vec<u64>, String> {
    let mut guard = STATE
        .lock()
        .map_err(|_| "readback state lock poisoned during fault abort".to_string())?;
    let held_present_seqs = guard
        .as_ref()
        .map(|state| state.pool.held_present_seqs())
        .unwrap_or_default();
    *guard = None;
    IN_FLIGHT.store(0, Ordering::Release);
    Ok(held_present_seqs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_vram_estimate_includes_pool_and_all_staging_textures() {
        let texture_bytes = 1280 * 720 * 4;
        assert_eq!(
            candidate_allocation_estimate(texture_bytes, 96, 4),
            Some(texture_bytes * 101)
        );
        assert_eq!(
            candidate_allocation_estimate(texture_bytes, 96, 6),
            Some(texture_bytes * 103),
            "the configured six-slot pool must be included in the VRAM gate"
        );
        assert_eq!(candidate_allocation_estimate(u64::MAX, 96, 6), None);
    }
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R32G8X24_TYPELESS,
    };

    #[test]
    fn classifies_ets2_d32s8_resource_and_view_formats_as_64_bit_depth() {
        assert_eq!(
            classify(DXGI_FORMAT_R32G8X24_TYPELESS),
            Some(DepthFormat::D32FloatS8x24)
        );
        assert_eq!(
            classify(DXGI_FORMAT_D32_FLOAT_S8X24_UINT),
            Some(DepthFormat::D32FloatS8x24)
        );
    }

    static GLOBAL_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// WARP（软件光栅）设备：CI/开发机上无 GPU 也能执行真实的 D3D11 编译与 dispatch，
    /// min/max/NaN 与 UNORM→float 语义和硬件同一规范。
    fn warp_device() -> (ID3D11Device, ID3D11DeviceContext) {
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        }
        .expect("WARP 设备创建失败");
        (device.expect("device"), context.expect("context"))
    }

    /// 确定性伪随机字节（LCG）；probe 代码禁不了随机源，测试也保持可复现。
    fn fill_deterministic(bytes: &mut [u8], seed: u32) {
        let mut state = seed | 1;
        for byte in bytes.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
    }

    struct PoolCase<'a> {
        src_w: usize,
        src_h: usize,
        row_pitch: usize,
        dst_w: usize,
        dst_h: usize,
        source_format: DXGI_FORMAT,
        decode_format: DepthFormat,
        conv: ZConvention,
        bytes: &'a [u8],
    }

    /// GPU 池化 vs CPU 参考逐位对拍。bytes 带行距 padding，两条路吃同一份输入。
    fn assert_gpu_pool_matches_cpu(case: &PoolCase<'_>) {
        let PoolCase {
            src_w,
            src_h,
            row_pitch,
            dst_w,
            dst_h,
            source_format,
            decode_format,
            conv,
            bytes,
        } = *case;
        let cpu = decode::decode_and_pool_nearest(
            bytes,
            (src_w, src_h),
            row_pitch,
            decode_format,
            (dst_w, dst_h),
            conv,
        )
        .expect("CPU 参考解码失败");

        let (device, context) = warp_device();
        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: src_w as u32,
            Height: src_h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: source_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: row_pitch as u32,
            SysMemSlicePitch: 0,
        };
        let mut source = None;
        unsafe { device.CreateTexture2D(&src_desc, Some(&init), Some(&mut source)) }
            .expect("建源纹理失败");
        let source = source.expect("源纹理");

        let mut gpu = make_gpu_pool(&device, &src_desc, dst_w, dst_h, conv).expect("建 GPU 池化");
        dispatch_pool(&mut gpu, &device, &context, &source, dst_w, dst_h).expect("dispatch 失败");
        let staging =
            make_staging_sized(&device, dst_w as u32, dst_h as u32, DXGI_FORMAT_R32_FLOAT)
                .expect("建回读 staging");
        let dst_res: ID3D11Resource = staging.clone().into();
        let src_res: ID3D11Resource = gpu.pool_tex.clone().into();
        unsafe { context.CopyResource(&dst_res, &src_res) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // 测试里允许阻塞 Map：没有渲染线程要保护。
        unsafe { context.Map(&dst_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .expect("Map 失败");
        let pitch = mapped.RowPitch as usize;
        let len = pitch * (dst_h - 1) + dst_w * 4;
        // SAFETY: Map 成功后 pData 至少 len 字节，Unmap 前使用完毕。
        let shadow = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) }.to_vec();
        unsafe { context.Unmap(&dst_res, 0) };

        let gpu_depth =
            decode_pooled_bytes(&shadow, dst_w, dst_h, pitch).expect("池化字节剥离失败");
        assert_eq!(cpu.len(), gpu_depth.len());
        assert!(
            verify_bitwise(&cpu, &gpu_depth, dst_w).is_none(),
            "GPU 池化与 CPU 参考位模式不一致: {:?}",
            verify_bitwise(&cpu, &gpu_depth, dst_w)
        );
    }

    #[test]
    fn warp_pool_matches_cpu_r32_pathological_ratio_traditional_with_nans() {
        let (src_w, src_h, dst_w, dst_h) = (43usize, 25usize, 17usize, 11usize);
        let row_pitch = src_w * 4 + 12; // 故意非整齐，钉 RowPitch 处理
        let mut bytes = vec![0u8; row_pitch * src_h];
        fill_deterministic(&mut bytes, 0x1234_5678);
        // 把每个 f32 归一到 [0,1)（深度域），并注入正/负 NaN——但保证不会出现全 NaN 窗口
        // （真实深度缓冲无 NaN；全 NaN 窗口的位模式在 IEEE 与 D3D 规范下均不保证唯一）。
        for y in 0..src_h {
            for x in 0..src_w {
                let offset = y * row_pitch + x * 4;
                let word = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                let value = (word as f32) / (u32::MAX as f32);
                let encoded = if (x + y * src_w) % 97 == 13 && x % 2 == 0 {
                    if x % 4 == 0 {
                        0x7FC0_0000u32
                    } else {
                        0xFFC0_0001u32
                    }
                } else {
                    value.to_bits()
                };
                bytes[offset..offset + 4].copy_from_slice(&encoded.to_le_bytes());
            }
        }
        assert_gpu_pool_matches_cpu(&PoolCase {
            src_w,
            src_h,
            row_pitch,
            dst_w,
            dst_h,
            source_format: DXGI_FORMAT_R32_TYPELESS,
            decode_format: DepthFormat::D32Float,
            conv: ZConvention::Traditional,
            bytes: &bytes,
        });
    }

    #[test]
    fn warp_pool_matches_cpu_r32_reversed_z() {
        let (src_w, src_h, dst_w, dst_h) = (43usize, 25usize, 17usize, 11usize);
        let row_pitch = src_w * 4;
        let mut bytes = vec![0u8; row_pitch * src_h];
        fill_deterministic(&mut bytes, 0x0BAD_F00D);
        for chunk in bytes.as_chunks_mut::<4>().0 {
            let word = u32::from_le_bytes(*chunk);
            let value = (word as f32) / (u32::MAX as f32);
            chunk.copy_from_slice(&value.to_bits().to_le_bytes());
        }
        assert_gpu_pool_matches_cpu(&PoolCase {
            src_w,
            src_h,
            row_pitch,
            dst_w,
            dst_h,
            source_format: DXGI_FORMAT_R32_TYPELESS,
            decode_format: DepthFormat::D32Float,
            conv: ZConvention::Reversed,
            bytes: &bytes,
        });
    }

    #[test]
    fn warp_pool_matches_cpu_ets2_d32s8_reversed_z() {
        let (src_w, src_h, dst_w, dst_h) = (43usize, 25usize, 17usize, 11usize);
        let row_pitch = src_w * 8 + 24;
        let mut bytes = vec![0u8; row_pitch * src_h];
        for y in 0..src_h {
            for x in 0..src_w {
                let offset = y * row_pitch + x * 8;
                let seed = (x as u32)
                    .wrapping_mul(0x9E37_79B9)
                    .wrapping_add((y as u32).wrapping_mul(0x85EB_CA6B));
                let depth = (seed as f32) / (u32::MAX as f32);
                bytes[offset..offset + 4].copy_from_slice(&depth.to_bits().to_le_bytes());
                bytes[offset + 4..offset + 8]
                    .copy_from_slice(&(0xA500_0000u32 | seed & 0xff).to_le_bytes());
            }
        }
        assert_gpu_pool_matches_cpu(&PoolCase {
            src_w,
            src_h,
            row_pitch,
            dst_w,
            dst_h,
            source_format: DXGI_FORMAT_R32G8X24_TYPELESS,
            decode_format: DepthFormat::D32FloatS8x24,
            conv: ZConvention::Reversed,
            bytes: &bytes,
        });
    }

    #[test]
    fn warp_pool_matches_cpu_d24s8_delivery_geometry() {
        // 真实交付几何：1920×1080 → 1280×720，1.5× 非整数窗口 + D24 UNORM 转换。
        let (src_w, src_h, dst_w, dst_h) = (1920usize, 1080usize, 1280usize, 720usize);
        let row_pitch = src_w * 4 + 128;
        let mut bytes = vec![0u8; row_pitch * src_h];
        fill_deterministic(&mut bytes, 0xDEAD_BEEF);
        assert_gpu_pool_matches_cpu(&PoolCase {
            src_w,
            src_h,
            row_pitch,
            dst_w,
            dst_h,
            source_format: DXGI_FORMAT_R24G8_TYPELESS,
            decode_format: DepthFormat::D24S8,
            conv: ZConvention::Traditional,
            bytes: &bytes,
        });
    }

    #[test]
    fn pool_srv_format_rejects_fully_typed_depth() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        };
        assert!(pool_srv_format(DXGI_FORMAT_R24G8_TYPELESS).is_ok());
        assert!(pool_srv_format(DXGI_FORMAT_R32_TYPELESS).is_ok());
        assert_eq!(
            pool_srv_format(DXGI_FORMAT_R32G8X24_TYPELESS).unwrap(),
            DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS
        );
        assert!(pool_srv_format(DXGI_FORMAT_D32_FLOAT).is_err());
        assert!(pool_srv_format(DXGI_FORMAT_D32_FLOAT_S8X24_UINT).is_err());
    }

    #[test]
    fn pooled_byte_stripping_and_bitwise_verify_reject_mismatch() {
        let width = 3usize;
        let height = 2usize;
        let pitch = width * 4 + 8;
        let mut bytes = vec![0u8; pitch * height];
        let values = [0.25f32, 0.5, f32::NAN, 0.75, 1.0, 0.0];
        for (index, value) in values.iter().enumerate() {
            let offset = (index / width) * pitch + (index % width) * 4;
            bytes[offset..offset + 4].copy_from_slice(&value.to_bits().to_le_bytes());
        }
        let decoded = decode_pooled_bytes(&bytes, width, height, pitch).unwrap();
        assert_eq!(decoded.len(), values.len());
        assert_eq!(decoded[0].to_bits(), values[0].to_bits());
        assert_eq!(decoded[2].to_bits(), values[2].to_bits(), "NaN 位模式原样");
        assert!(verify_bitwise(&decoded, &decoded.clone(), width).is_none());
        let mut tampered = decoded.clone();
        tampered[4] = f32::from_bits(tampered[4].to_bits() ^ 1);
        let mismatch = verify_bitwise(&decoded, &tampered, width).expect("必须检出单 bit 翻转");
        assert!(mismatch.contains("1 个像素"));
    }

    #[test]
    fn mapped_copy_preserves_small_and_streaming_store_payloads() {
        for len in [127, NON_TEMPORAL_COPY_MIN_BYTES + 37] {
            let source = (0..len + 3)
                .map(|index| (index.wrapping_mul(31) & 0xff) as u8)
                .collect::<Vec<_>>();
            let expected = &source[3..];

            // SAFETY: the deliberately unaligned source slice stays alive through each copy.
            let copied = unsafe { copy_mapped_bytes(expected.as_ptr(), expected.len()) };

            assert_eq!(copied, expected);
        }
    }

    #[test]
    fn pooled_decode_reuses_capacity_preserves_bits_and_clears_failed_frames() {
        let bits = [
            0x7fc0_0001u32,
            0xffa0_0003,
            0x8000_0000,
            0x7f80_0000,
            1,
            0x3f00_0000,
        ];
        let mut bytes = vec![0x5a; 40];
        for (index, value) in bits.iter().enumerate() {
            let offset = index / 3 * 20 + index % 3 * 4;
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut frame = RawDepthFrame {
            present_seq: 1,
            src_width: 3,
            src_height: 2,
            row_pitch: 20,
            format: DepthFormat::D32Float,
            dst_width: 3,
            dst_height: 2,
            conv: ZConvention::Reversed,
            bytes,
            pooled: true,
            verify_pooled: None,
            verify_row_pitch: 0,
        };
        let mut out = Vec::with_capacity(16);
        let allocation = out.as_ptr();
        for _ in 0..2 {
            frame.decode_into(&mut out).unwrap();
            assert_eq!(out.as_ptr(), allocation);
            assert_eq!(out.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), bits);
        }
        frame.row_pitch = 4;
        assert!(frame.decode_into(&mut out).is_err());
        assert!(out.is_empty());
        frame.row_pitch = 20;
        frame.bytes.truncate(24);
        assert!(frame.decode_into(&mut out).is_err());
        assert!(out.is_empty());
        frame.dst_width = 2;
        frame.dst_height = 1;
        frame.decode_into(&mut out).unwrap();
        assert_eq!(out.as_ptr(), allocation);
        assert_eq!(
            out.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            bits[..2]
        );
    }

    #[test]
    fn drain_slice_budget_parses_env_shapes() {
        assert_eq!(drain_slice_bytes_from(None), DEFAULT_DRAIN_SLICE_BYTES);
        assert_eq!(
            drain_slice_bytes_from(Some("0")),
            usize::MAX,
            "0 = 禁用分片"
        );
        assert_eq!(drain_slice_bytes_from(Some("512")), 512 * 1024);
        assert_eq!(drain_slice_bytes_from(Some(" 256 ")), 256 * 1024);
        assert_eq!(drain_slice_bytes_from(Some("1")), 64 * 1024, "下钳 64 KiB");
        assert_eq!(
            drain_slice_bytes_from(Some("999999")),
            16 * 1024 * 1024,
            "上钳 16 MiB"
        );
        assert_eq!(
            drain_slice_bytes_from(Some("garbage")),
            DEFAULT_DRAIN_SLICE_BYTES
        );
    }

    /// 分片重组必须与整块拷出逐位一致，交付语义（present_seq、pooled、几何）不变。
    ///
    /// 注意：这是本文件**唯一**触碰全局 [`STATE`] 的测试（其余 WARP 测试全用局部
    /// 对象）。将来若再加第二个，必须串行化，否则两个测试会互踩池状态。
    #[test]
    fn warp_sliced_drain_reassembles_bitwise_identical_bytes() {
        let _serial = GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let (device, context) = warp_device();

        let (src_w, src_h, dst_w, dst_h) = (64usize, 48usize, 32usize, 24usize);
        let row_pitch = src_w * 4;
        let mut bytes = vec![0u8; row_pitch * src_h];
        fill_deterministic(&mut bytes, 0xC0FF_EE11);
        for chunk in bytes.as_chunks_mut::<4>().0 {
            let word = u32::from_le_bytes(*chunk);
            let value = (word as f32) / (u32::MAX as f32);
            chunk.copy_from_slice(&value.to_bits().to_le_bytes());
        }
        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: src_w as u32,
            Height: src_h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_TYPELESS,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: row_pitch as u32,
            SysMemSlicePitch: 0,
        };
        let mut source = None;
        unsafe { device.CreateTexture2D(&src_desc, Some(&init), Some(&mut source)) }
            .expect("建源纹理失败");
        let source = source.expect("源纹理");

        // 池化输出 32×24 R32F：len ≥ 3072 字节，1 KiB 片 → 至少 3 轮才拷完。
        // deadline_ticks 给大：测试逐轮 sleep 等 WARP，InFlight 阶段不能被误回收。
        let cfg = CaptureConfig {
            dst_w,
            dst_h,
            conv: ZConvention::Traditional,
            slots: 4,
            deadline_ticks: 4096,
        };
        let drain_until_frame = |slice: usize, label: &str| -> (RawDepthFrame, usize) {
            let mut rounds = 0usize;
            while rounds < 4096 {
                rounds += 1;
                let mut got = drain_ready_with_slice(&context, slice)
                    .unwrap_or_else(|error| panic!("{label}: drain 失败: {error}"));
                assert!(got.len() <= 1, "{label}: 单帧在飞不可能一轮多帧");
                if let Some(frame) = got.pop() {
                    return (frame, rounds);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            panic!("{label}: 4096 轮内未交付");
        };

        // 帧 1：1 KiB 分片路径。
        assert!(capture(&device, &context, &source, 1, cfg).expect("capture 1"));
        unsafe { context.Flush() };
        let (sliced, sliced_rounds) = drain_until_frame(1024, "sliced");
        assert!(
            sliced_rounds >= 3,
            "≥3 KiB 的帧按 1 KiB 片至少 3 轮，实际 {sliced_rounds}"
        );

        // 帧 2：整块对照（预算无穷大 → 恒走原路径）。
        assert!(capture(&device, &context, &source, 2, cfg).expect("capture 2"));
        unsafe { context.Flush() };
        let (whole, _) = drain_until_frame(usize::MAX, "whole");

        assert_eq!(sliced.present_seq, 1);
        assert_eq!(whole.present_seq, 2);
        assert!(sliced.pooled && whole.pooled);
        assert_eq!(sliced.row_pitch, whole.row_pitch);
        assert_eq!(sliced.bytes, whole.bytes, "分片重组必须与整块拷出逐位一致");

        // 清空全局 STATE，不给将来的测试留残留。
        assert!(reset_if_drained(), "池已排空，reset 必须成功");
    }

    #[test]
    fn warp_candidate_survives_slot_reuse_after_selected_copy_submission() {
        use depth_lab::selected_present_candidate_ring::{
            CandidateRingConfig, CandidateRingState, SelectPlan,
        };

        let _serial = GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        assert!(reset_if_drained());
        let (device, context) = warp_device();
        let (src_w, src_h, dst_w, dst_h) = (16usize, 16usize, 4usize, 4usize);
        let source = |value: f32| {
            let words = vec![value; src_w * src_h];
            let desc = D3D11_TEXTURE2D_DESC {
                Width: src_w as u32,
                Height: src_h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R32_TYPELESS,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: words.as_ptr().cast(),
                SysMemPitch: (src_w * 4) as u32,
                SysMemSlicePitch: 0,
            };
            let mut texture = None;
            unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
                .expect("create candidate test source");
            texture.expect("candidate test source")
        };
        let first_source = source(1.0);
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { first_source.GetDesc(&mut src_desc) };
        let gpu =
            make_gpu_pool(&device, &src_desc, dst_w, dst_h, ZConvention::Traditional).unwrap();
        let candidate_desc = D3D11_TEXTURE2D_DESC {
            Width: dst_w as u32,
            Height: dst_h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut textures = Vec::new();
        for _ in 0..4 {
            let mut texture = None;
            unsafe { device.CreateTexture2D(&candidate_desc, None, Some(&mut texture)) }.unwrap();
            textures.push(texture.unwrap());
        }
        let mut gpu_ring = GpuCandidateRing {
            gpu,
            textures,
            src_desc,
            dst_w,
            dst_h,
            stats: CandidateGpuAllocationStats {
                capacity: 4,
                texture_bytes: 64,
                estimated_bytes: 320,
                configured_limit_bytes: 320,
                budget_before: 0,
                usage_before: 0,
                headroom_before: 0,
                budget_after: 0,
                usage_after: 0,
                headroom_after: 0,
                actual_usage_delta: 0,
                configured_reserve_bytes: 0,
                budget_at_flush: None,
                usage_at_flush: None,
                headroom_at_flush: None,
            },
        };
        let mut state = CandidateRingState::new(
            CandidateRingConfig {
                capacity: 4,
                max_feedback_lag_presents: 3,
                guard_slots: 1,
            },
            1,
            1,
        )
        .unwrap();
        state.arm().unwrap();

        for (seq, texture) in [
            (1, first_source),
            (2, source(2.0)),
            (3, source(3.0)),
            (4, source(4.0)),
        ] {
            let slot = state.materialize(seq).unwrap();
            gpu_ring
                .materialize(&device, &context, &texture, slot)
                .unwrap();
        }
        let SelectPlan::Submit(plan) = state.select(1, 1, 1, 4).unwrap() else {
            unreachable!()
        };
        let selected = gpu_ring.texture(plan.slot).unwrap();
        let cfg = CaptureConfig {
            dst_w,
            dst_h,
            conv: ZConvention::Traditional,
            slots: 4,
            deadline_ticks: 4096,
        };
        assert!(capture_pooled(&device, &context, &selected, 1, cfg).unwrap());
        state.commit_submitted(plan).unwrap();
        state.retire_through(4).unwrap();

        // Reuse the same physical candidate slot after the selected copy was
        // submitted. D3D11 command ordering must preserve the old value in staging.
        let slot = state.materialize(5).unwrap();
        gpu_ring
            .materialize(&device, &context, &source(5.0), slot)
            .unwrap();
        unsafe { context.Flush() };
        let frame = (0..4096)
            .find_map(|_| {
                let mut frames = drain_ready_with_slice(&context, usize::MAX).unwrap();
                if frames.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    None
                } else {
                    Some(frames.remove(0))
                }
            })
            .expect("selected candidate readback did not complete");
        assert_eq!(frame.present_seq, 1);
        assert!(
            frame
                .decode()
                .unwrap()
                .iter()
                .all(|value| value.to_bits() == 1.0f32.to_bits())
        );
        assert!(reset_if_drained());
    }

    #[test]
    fn warp_fault_abort_releases_in_flight_state_for_next_generation() {
        let _serial = GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        assert!(reset_if_drained());
        let (device, context) = warp_device();
        let words = [7.0f32; 16];
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 4,
            Height: 4,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: words.as_ptr().cast(),
            SysMemPitch: 16,
            SysMemSlicePitch: 0,
        };
        let mut source = None;
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut source)) }
            .expect("create selected candidate");
        let source = source.expect("selected candidate");
        let cfg = CaptureConfig {
            dst_w: 4,
            dst_h: 4,
            conv: ZConvention::Traditional,
            slots: 4,
            deadline_ticks: 4096,
        };

        // Generation 1 faults immediately after queueing a GPU copy. The
        // fault path must explicitly abandon this non-deliverable work.
        assert!(capture_pooled(&device, &context, &source, 101, cfg).unwrap());
        assert_eq!(in_flight_present_seqs(), vec![101]);
        assert_eq!(abort_faulted_generation().unwrap(), vec![101]);
        assert_eq!(in_flight(), 0);
        assert!(reset_if_drained());

        // Generation 2 can immediately build a fresh staging pool in the same
        // game process; no DLL/game restart is required.
        assert!(capture_pooled(&device, &context, &source, 202, cfg).unwrap());
        unsafe { context.Flush() };
        let frame = (0..4096)
            .find_map(|_| {
                let mut frames = drain_ready_with_slice(&context, usize::MAX).unwrap();
                if frames.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    None
                } else {
                    Some(frames.remove(0))
                }
            })
            .expect("next-generation selected readback did not complete");
        assert_eq!(frame.present_seq, 202);
        assert!(reset_if_drained());
    }
}
