//! 注入热路径的可选计时：全局聚合 + 逐 Present 环形缓冲。
//!
//! 默认完全关闭；只有 StartRun 显式请求 A3 分项计时才读取时钟并累计。
//! 关闭时逐帧路径只做原子准入检查；开启时每帧代价是几次原子 swap + 一次提交锁
//! （Present 尾提交，不在 Draw/Map 粒度），帧环内存预先分配且容量固定。
//! v6 额外保留 Present 入口/原始调用/环提交的 QPC 边界；提交返回和 hook 退出
//! 写回本帧独占的原子槽，跨线程可关联，最后一帧也不依赖后继；不二次抢环锁。
//!
//! # 为什么要逐 Present
//!
//! 1% low 的病要靠「慢帧 × capture 标志 × 分段耗时」的**逐帧相关性**定罪，
//! 全局平均会把 6.25% 的证据帧稀释到看不见。环里每条记录带 `present_seq` 与 QPC，
//! 帧时 = 相邻记录的 QPC 差；`flags` 标出该帧做了什么（capture/drain/audit/latency≥2），
//! StopRun 时 dump 成 jsonl，离线即可与 PresentMon 的慢帧逐一对齐。
//!
//! # 线程归属
//!
//! 普通分段 Scope 与 `mark_frame`/`commit_frame` 只应在渲染线程调用
//! （Present/Draw/Map hook）；控制/worker 线程只能用 aggregate scope 或各自独立的记录环，
//! 不得混入帧累加器。writer 线程的 EXR 编码走独立的 [`record_encode`] 环。

mod present_exit;
mod state;

pub use present_exit::{FrameCommit, PresentExit};

pub use state::*;
pub(crate) use state::{ReadbackCopyRecord, record_readback_copy};

#[cfg(test)]
mod tests;
