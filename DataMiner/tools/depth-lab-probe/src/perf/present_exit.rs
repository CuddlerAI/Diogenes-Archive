//! Write-once endpoints owned by a particular frame-ring allocation. A delayed
//! callback can finish after Ready/reset/Stop without touching the next ring.
//! No TLS, allocation, retry loop or second ring lock on the Present exit path.
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Default)]
pub(super) struct ExitSlot {
    commit_return_qpc: AtomicI64,
    hook_exit_qpc: AtomicI64,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct PresentExit {
    pub commit_return_qpc: i64,
    /// Final diagnostic Drop, after local guards. A small return epilogue remains.
    pub hook_exit_qpc: i64,
}

impl ExitSlot {
    pub(super) fn snapshot(&self) -> Option<PresentExit> {
        let hook_exit_qpc = self.hook_exit_qpc.load(Ordering::Acquire);
        (hook_exit_qpc > 0).then(|| PresentExit {
            commit_return_qpc: self.commit_return_qpc.load(Ordering::Relaxed),
            hook_exit_qpc,
        })
    }
}

pub struct FrameCommit {
    // A slot is never recycled, even when frame records wrap. Old callbacks own
    // their old allocation until they finish; they cannot alias a new generation.
    pub(super) exit: Option<(Arc<[ExitSlot]>, usize)>,
}

impl FrameCommit {
    pub fn needs_exit(&self) -> bool {
        self.exit.is_some()
    }

    pub fn finish(self, commit_return_qpc: i64, hook_exit_qpc: i64) {
        if let Some((slots, index)) = self.exit
            && commit_return_qpc > 0
            && hook_exit_qpc >= commit_return_qpc
        {
            // Exactly one writer owns this index. Publish the payload only once
            // complete; a concurrent dump sees either None or the whole endpoint.
            slots[index]
                .commit_return_qpc
                .store(commit_return_qpc, Ordering::Relaxed);
            slots[index]
                .hook_exit_qpc
                .store(hook_exit_qpc, Ordering::Release);
        }
    }
}
