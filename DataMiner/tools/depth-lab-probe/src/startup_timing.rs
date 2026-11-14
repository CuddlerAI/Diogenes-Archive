//! Control-thread-only startup diagnostics. Emit one summary, never per memory read.
use std::cell::RefCell;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Default, Serialize)]
struct Measurements {
    phases: Vec<Phase>,
    virtual_query_calls: u64,
    virtual_query_ms: f64,
    /// `RegionMemo` 实际发出的查询数。与 `virtual_query_calls` 的差额就是不经 memo 的
    /// 查询点（例如 vtable 扫描里的可执行页判定），可据此判断还有没有剩余的优化空间。
    region_memo_queries: u64,
    /// 被 `RegionMemo` 的 region 裁定挡下、因而没有发出去的查询数。
    /// `region_memo_queries + region_memo_avoided_queries` 就是这些游走在引入
    /// `RegionMemo` 之前会发出的查询数——单次运行即可自带对照组。
    region_memo_avoided_queries: u64,
    scan_slots: usize,
    candidate_pointers: usize,
    unique_candidate_pointers: usize,
    rtti_objects: usize,
}

#[derive(Serialize)]
struct Phase {
    name: &'static str,
    elapsed_ms: f64,
    success: bool,
}

thread_local! {
    static ACTIVE: RefCell<Option<Measurements>> = const { RefCell::new(None) };
}

pub(crate) struct StartupTiming {
    generation: u64,
    observed_unix_ms: u128,
    started: Instant,
    started_emitted: bool,
}

impl StartupTiming {
    pub(crate) fn begin(generation: u64) -> Self {
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(Measurements::default()));
        Self {
            generation,
            observed_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            started: Instant::now(),
            started_emitted: false,
        }
    }

    pub(crate) fn mark_started(&mut self) {
        self.started_emitted = true;
    }
}

impl Drop for StartupTiming {
    fn drop(&mut self) {
        let total_ms = self.started.elapsed().as_secs_f64() * 1_000.0;
        if let Some(measurements) = ACTIVE.with(|slot| slot.borrow_mut().take()) {
            crate::probe::log(&format!(
                "startup_timing {}",
                serde_json::json!({
                    "generation": self.generation,
                    "command_observed_unix_ms": self.observed_unix_ms,
                    "started_emitted": self.started_emitted,
                    "total_ms": total_ms,
                    "measurements": measurements,
                })
            ));
        }
    }
}

pub(crate) fn active() -> bool {
    ACTIVE.with(|slot| slot.borrow().is_some())
}

pub(crate) fn phase<T, E>(name: &'static str, task: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let started = active().then(Instant::now);
    let result = task();
    if let Some(started) = started {
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        ACTIVE.with(|slot| {
            if let Some(stats) = slot.borrow_mut().as_mut() {
                stats.phases.push(Phase {
                    name,
                    elapsed_ms,
                    success: result.is_ok(),
                });
            }
        });
    }
    result
}

pub(crate) fn virtual_query<T>(query: impl FnOnce() -> T) -> T {
    let started = active().then(Instant::now);
    let result = query();
    if let Some(started) = started {
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        ACTIVE.with(|slot| {
            if let Some(stats) = slot.borrow_mut().as_mut() {
                stats.virtual_query_calls += 1;
                stats.virtual_query_ms += elapsed_ms;
            }
        });
    }
    result
}

/// 由 `RegionMemo` 在一次结构游走结束时汇报。与 `virtual_query` 一样按线程局部作用域
/// 生效：启动之外的周期审计、渲染线程上的读取都落在 `ACTIVE` 之外，不会进这份报告。
pub(crate) fn region_memo_walk(queries: u64, avoided: u64) {
    if queries == 0 && avoided == 0 {
        return;
    }
    ACTIVE.with(|slot| {
        if let Some(stats) = slot.borrow_mut().as_mut() {
            stats.region_memo_queries += queries;
            stats.region_memo_avoided_queries += avoided;
        }
    });
}

pub(crate) fn scan_counts(slots: usize, candidates: usize, unique: usize, rtti_objects: usize) {
    ACTIVE.with(|slot| {
        if let Some(stats) = slot.borrow_mut().as_mut() {
            stats.scan_slots = slots;
            stats.candidate_pointers = candidates;
            stats.unique_candidate_pointers = unique;
            stats.rtti_objects = rtti_objects;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_scope_is_thread_local_and_cleared_after_failure() {
        assert!(!active());
        {
            let _scope = StartupTiming::begin(1);
            let result = phase("failed_phase", || {
                virtual_query(|| 42);
                Err::<(), _>("expected failure")
            });
            assert_eq!(result, Err("expected failure"));
            std::thread::spawn(|| {
                assert!(!active());
                virtual_query(|| 99);
            })
            .join()
            .unwrap();
            ACTIVE.with(|slot| {
                let stats = slot.borrow();
                let stats = stats.as_ref().unwrap();
                assert_eq!(stats.virtual_query_calls, 1);
                assert_eq!(stats.phases.len(), 1);
                assert!(!stats.phases[0].success);
            });
        }
        assert!(!active());
        // Later steady-state queries cannot enter the completed startup report.
        assert_eq!(virtual_query(|| 7), 7);
        assert!(!active());
        let _next = StartupTiming::begin(2);
        ACTIVE.with(|slot| assert_eq!(slot.borrow().as_ref().unwrap().virtual_query_calls, 0));
    }
}
