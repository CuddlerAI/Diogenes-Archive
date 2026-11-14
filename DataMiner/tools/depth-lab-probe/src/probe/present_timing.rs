//! Present-only diagnostics: no Draw work, allocation, extra mutex or hot IO.
//! Keep the historical post-admission QPC anchor and pre/post scopes unchanged.
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{ACTIVE_GENERATION, current_qpc};
use crate::perf::{FrameCommit, PresentCallTiming};

pub(super) struct PresentPerfEntry {
    qpc: i64,
    acquired_qpc: i64,
    epoch: u64,
    generation: u64,
    swapchain: usize,
    thread_id: u32,
}

impl PresentPerfEntry {
    pub(super) fn begin(swapchain: usize) -> Option<Self> {
        crate::perf::enabled().then(|| Self {
            qpc: current_qpc(),
            acquired_qpc: 0,
            epoch: crate::perf::diagnostic_epoch(),
            generation: ACTIVE_GENERATION.load(Ordering::Acquire),
            swapchain,
            thread_id: unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        })
    }

    pub(super) fn acquired(&mut self) {
        self.acquired_qpc = current_qpc();
    }

    fn timing(self, epoch: u64, generation: u64, frame_qpc: i64) -> Option<PresentCallTiming> {
        (self.epoch != 0
            && self.epoch == epoch
            && self.generation == generation
            && self.generation != 0
            && self.swapchain != 0
            && self.qpc > 0
            && self.acquired_qpc >= self.qpc
            && frame_qpc >= self.acquired_qpc)
            .then_some(PresentCallTiming {
                epoch,
                generation,
                swapchain_key: self.swapchain,
                thread_id: self.thread_id,
                entry_qpc: self.qpc,
                entry_lock_acquired_qpc: self.acquired_qpc,
                ..PresentCallTiming::default()
            })
    }
}

pub(super) struct PresentPerfFrame {
    pub(super) frame_qpc: i64,
    pre_started: Option<Instant>,
    timing: Option<PresentCallTiming>,
    committed: Option<(FrameCommit, i64)>,
}

impl PresentPerfFrame {
    pub(super) fn begin(entry: Option<PresentPerfEntry>) -> Self {
        let enabled = crate::perf::enabled();
        // Preserve the ACK FPS anchor after lifecycle admission, not at entry.
        let frame_qpc = if enabled { current_qpc() } else { 0 };
        let pre_started = enabled.then(Instant::now);
        let timing = entry.filter(|_| enabled).and_then(|entry| {
            entry.timing(
                crate::perf::diagnostic_epoch(),
                ACTIVE_GENERATION.load(Ordering::Acquire),
                frame_qpc,
            )
        });
        Self {
            frame_qpc,
            pre_started,
            timing,
            committed: None,
        }
    }

    pub(super) fn pre_ns(&mut self) -> u64 {
        let ns = self.pre_started.map_or(0, |started| {
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
        });
        if let Some(timing) = self.timing.as_mut() {
            timing.original_begin_qpc = current_qpc();
        }
        ns
    }

    pub(super) fn start_post(&mut self) -> Option<Instant> {
        if let Some(timing) = self.timing.as_mut() {
            timing.original_end_qpc = current_qpc();
        }
        self.pre_started.map(|_| Instant::now())
    }

    pub(super) fn begin_commit(&mut self) -> Option<PresentCallTiming> {
        if let Some(timing) = self.timing.as_mut() {
            timing.commit_begin_qpc = current_qpc();
        }
        self.timing
    }

    pub(super) fn committed(&mut self, commit: FrameCommit) {
        if commit.needs_exit() {
            self.committed = Some((commit, current_qpc()));
        }
    }
}

impl Drop for PresentPerfFrame {
    fn drop(&mut self) {
        // Outlives post-Present locals. The exact row owns its endpoint, including
        // the final callback, independent of which thread Presents next.
        if let Some((commit, return_qpc)) = self.committed.take() {
            commit.finish(return_qpc, current_qpc());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_rejects_ready_reset_generation_and_invalid_clock_identity() {
        let entry = || PresentPerfEntry {
            qpc: 10,
            acquired_qpc: 20,
            epoch: 3,
            generation: 4,
            swapchain: 5,
            thread_id: 6,
        };
        assert!(entry().timing(3, 4, 30).is_some());
        assert!(entry().timing(4, 4, 30).is_none());
        assert!(entry().timing(3, 5, 30).is_none());
        assert!(entry().timing(3, 4, 19).is_none());
        assert!(
            PresentPerfEntry {
                epoch: 0,
                ..entry()
            }
            .timing(0, 4, 30)
            .is_none()
        );
        assert!(
            PresentPerfEntry {
                generation: 0,
                ..entry()
            }
            .timing(3, 0, 30)
            .is_none()
        );
    }
}
