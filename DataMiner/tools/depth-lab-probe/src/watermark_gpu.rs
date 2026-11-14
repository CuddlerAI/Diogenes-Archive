//! 水印写入的 GPU 侧，**带缓存**。
//!
//! # 为什么单独一个模块：每帧新建 RTV 是 C5 失败的原因之一
//!
//! 原实现每帧都做一遍
//! `GetBuffer(0)` → `GetDevice` → `GetImmediateContext` → `QueryInterface` →
//! **`CreateRenderTargetView`** → 两次 `ClearView`。
//!
//! `CreateRenderTargetView` 是驱动侧的资源创建，每帧一次很贵。真机实测
//! 仅水印一档就让平均 FPS 掉 **13.73%**（187.33 → 161.60，即 +0.850 ms/帧），
//! 而门限是 2%。
//!
//! [77 号 §5.6.2](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)
//! 当初写的就是「**缓存一个后缓冲 RTV，`ResizeBuffers` 时失效重建**」——
//! 实现没照做。本模块补上。
//!
//! # 为什么缓存要用「指针 → RTV」的小表，而不是单个 RTV
//!
//! **未在硬件上验证过**：D3D11 的 flip 模型（`FLIP_DISCARD` / `FLIP_SEQUENTIAL`）下，
//! 运行时自己做后缓冲轮转，`GetBuffer(0)` 大概率每次返回同一个纹理对象
//! （逐缓冲索引是 D3D12 的模型）。若如此，这张表只会有一条记录，等同于缓存单个 RTV。
//!
//! 但反过来若假设错了——`GetBuffer(0)` 真的换了对象——单 RTV 缓存会**每帧判失效再重建**，
//! 白缓存一场，而这正是本模块要消除的开销。小表在两种情况下都对，代价是一次 HashMap 查找。
//! 所以按指针建表，不去赌那个未验证的前提。
//!
//! # 缓存 RTV 会持有后缓冲引用，因此 `ResizeBuffers` 必须一起 hook
//!
//! DXGI 要求调用 `ResizeBuffers` 前释放掉**所有**后缓冲引用，否则直接返回
//! `DXGI_ERROR_INVALID_CALL`。也就是说，光缓存不挂 hook，症状不是「水印丢了」，
//! 而是「装了 probe 之后游戏改不了分辨率」，且失败的 HRESULT 由游戏收到，
//! 我们的日志里一片干净。见 `probe::hooked_resize_buffers`。
//!
//! 另注：缓存持有 `ID3D11DeviceContext1`，也就顺带把设备钉住了。
//! 设备丢失后游戏会重建 swapchain，`swapchain` 字段比对不上即整表作废，
//! 旧引用随之释放——这条路径覆盖了设备重建。

use std::collections::HashMap;
use std::sync::{Mutex, TryLockError};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11DeviceContext1, ID3D11RenderTargetView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::core::Interface;

use depth_lab::watermark;

/// 缓存的 RTV 上限。交换链的 BufferCount 极少超过 4；
/// 超了就整表清空重来，宁可偶尔重建也不无限增长。
const MAX_CACHED_RTVS: usize = 8;

struct Cache {
    /// 该缓存属于哪个 swapchain。换了 swapchain（设备丢失、分辨率切换重建）就整体作废。
    swapchain: usize,
    context: ID3D11DeviceContext1,
    /// 后缓冲纹理指针 → 对应的 RTV。flip 模型下会有多条。
    rtvs: HashMap<usize, ID3D11RenderTargetView>,
    /// 矩形只与 `present_seq` 的 24 个 bit 有关，几何固定，可预分配复用。
    white: Vec<RECT>,
    black: Vec<RECT>,
}

// SAFETY: 所有访问都在下面这个 Mutex 之下串行发生。
unsafe impl Send for Cache {}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// 把 `present_seq` 写进后缓冲左上角。
///
/// 稳态下每帧只剩：一次 `GetBuffer`、一次 HashMap 查找、两次 `ClearView`。
/// `GetDevice` / `GetImmediateContext` / `QueryInterface` / `CreateRenderTargetView`
/// 都只在缓存失效时发生。
pub fn write(
    swapchain_ptr: *mut core::ffi::c_void,
    seq: u16,
    origin_x: u32,
    origin_y: u32,
) -> Result<(), String> {
    let sc =
        unsafe { IDXGISwapChain::from_raw_borrowed(&swapchain_ptr) }.ok_or("swapchain 指针无效")?;
    let sc_key = swapchain_ptr as usize;

    // Present 热路径绝不等锁。ResizeBuffers 正在清缓存时宁可跳过一帧水印，
    // 也不能把游戏的渲染线程堵在互斥锁上。
    //
    // 锁必须在 GetBuffer **之前**拿：ResizeBuffers 侧拿到同一把锁后，才能证明
    // 本函数持有的临时 backbuffer 引用也已经释放，而不只是缓存表清空了。
    let mut guard = match CACHE.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => return Err("水印缓存正忙，本帧跳过".into()),
        Err(TryLockError::Poisoned(_)) => return Err("水印缓存锁已中毒".into()),
    };
    let backbuffer: ID3D11Texture2D =
        unsafe { sc.GetBuffer(0) }.map_err(|e| format!("GetBuffer(0) 失败: {e}"))?;
    let bb_key = backbuffer.as_raw() as usize;

    // swapchain 变了（设备丢失 / 分辨率切换后重建）→ 整表作废
    if guard.as_ref().is_some_and(|c| c.swapchain != sc_key) {
        *guard = None;
    }
    if guard.is_none() {
        let device: ID3D11Device =
            unsafe { sc.GetDevice() }.map_err(|e| format!("GetDevice: {e}"))?;
        let ctx: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
            .map_err(|e| format!("拿不到 immediate context: {e}"))?;
        let ctx1: ID3D11DeviceContext1 = ctx
            .cast()
            .map_err(|e| format!("ID3D11DeviceContext1 不可用（需要 D3D11.1）: {e}"))?;
        *guard = Some(Cache {
            swapchain: sc_key,
            context: ctx1,
            rtvs: HashMap::new(),
            white: Vec::with_capacity(watermark::TOTAL_BLOCKS as usize),
            black: Vec::with_capacity(watermark::TOTAL_BLOCKS as usize),
        });
    }
    let cache = guard.as_mut().expect("刚建过");

    if !cache.rtvs.contains_key(&bb_key) {
        // 表长到不像话说明假设错了（BufferCount 极少超过 4）——清空重来，
        // 而不是让它无限增长把显存吃掉。
        if cache.rtvs.len() >= MAX_CACHED_RTVS {
            cache.rtvs.clear();
        }
        let device: ID3D11Device =
            unsafe { sc.GetDevice() }.map_err(|e| format!("GetDevice: {e}"))?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        unsafe { device.CreateRenderTargetView(&backbuffer, None, Some(&mut rtv)) }
            .map_err(|e| format!("CreateRenderTargetView: {e}"))?;
        cache
            .rtvs
            .insert(bb_key, rtv.ok_or("CreateRenderTargetView 返回空")?);
    }

    encode_into(seq, origin_x, origin_y, &mut cache.white, &mut cache.black);

    // 借用检查：ClearView 需要 &cache.context 与 &cache.white 同时存在，
    // 而两者都在 cache 里——先把 RTV 取出来再调。
    let rtv = cache.rtvs.get(&bb_key).expect("刚插入").clone();
    unsafe {
        cache.context.ClearView(&rtv, &WHITE, Some(&cache.white));
        cache.context.ClearView(&rtv, &BLACK, Some(&cache.black));
    }
    Ok(())
}

fn encode_into(
    seq: u16,
    origin_x: u32,
    origin_y: u32,
    white: &mut Vec<RECT>,
    black: &mut Vec<RECT>,
) {
    white.clear();
    black.clear();
    for index in 0..watermark::TOTAL_BLOCKS {
        let bit = if index < watermark::SYNC_BITS {
            (watermark::integrity_code(seq) >> (watermark::SYNC_BITS - 1 - index)) & 1 == 1
        } else {
            let payload_index = index - watermark::SYNC_BITS;
            (seq >> (watermark::PAYLOAD_BITS - 1 - payload_index)) & 1 == 1
        };
        let left = origin_x + index * watermark::BLOCK_PX;
        let rect = RECT {
            left: left as i32,
            top: origin_y as i32,
            right: (left + watermark::BLOCK_PX) as i32,
            bottom: (origin_y + watermark::BLOCK_PX) as i32,
        };
        if bit {
            white.push(rect);
        } else {
            black.push(rect);
        }
    }
}

/// `ResizeBuffers` **之前**必须调：释放缓存与在途水印写入持有的全部后缓冲引用。
///
/// 这里允许稀有的 ResizeBuffers 路径等待正在结束的水印写入；Present 侧用
/// `try_lock`，不会反向等待本函数。拿到锁后，write 里的临时 backbuffer 已先释放，
/// 再清缓存 RTV，随后调用原 ResizeBuffers 才满足 DXGI 的引用规则。
pub fn invalidate() {
    let mut guard = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_free_encoder_matches_the_shared_contract() {
        let mut white = Vec::with_capacity(watermark::TOTAL_BLOCKS as usize);
        let mut black = Vec::with_capacity(watermark::TOTAL_BLOCKS as usize);
        for seq in [0, 1, 0x5555, 0xaaaa, 0xabcd, u16::MAX] {
            encode_into(seq, 17, 23, &mut white, &mut black);
            let expected = watermark::encode(seq, 17, 23);
            assert_eq!(
                white
                    .iter()
                    .map(|rect| (rect.left, rect.top, rect.right, rect.bottom))
                    .collect::<Vec<_>>(),
                expected
                    .white
                    .iter()
                    .map(|rect| {
                        (
                            rect.left as i32,
                            rect.top as i32,
                            rect.right as i32,
                            rect.bottom as i32,
                        )
                    })
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                black
                    .iter()
                    .map(|rect| (rect.left, rect.top, rect.right, rect.bottom))
                    .collect::<Vec<_>>(),
                expected
                    .black
                    .iter()
                    .map(|rect| {
                        (
                            rect.left as i32,
                            rect.top as i32,
                            rect.right as i32,
                            rect.bottom as i32,
                        )
                    })
                    .collect::<Vec<_>>()
            );
        }
    }
}
