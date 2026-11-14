//! 非阻塞落盘边界。
//!
//! # 为什么不能在回调里直接写文件
//!
//! 通道回调与 `frame_end` **跑在游戏自己的线程上**。在那里做一次同步写,
//! 就是把磁盘延迟直接加进帧时间——轻则掉帧,重则把「采集」变成「制造卡顿的采集」,
//! 而卡顿本身会改变玩家操作,污染的是数据的分布而不只是数据的时间戳。
//!
//! 所以边界是:游戏线程只做一次 `try_send`,**满了就丢并计数,永不阻塞、永不重试**
//! (纪律与 `fallout-player-telemetry` 的 `NonBlockingSinkAbiV1` 一致)。
//! 丢了多少必须能报出来——丢帧本身不可怕,不知道丢了多少才可怕。

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

/// 队列深度。约合 60 fps 下 4 秒的缓冲——足够吸收一次磁盘抖动,
/// 又不至于在真的写不动时攒出几百 MB 常驻内存。
pub const QUEUE_CAPACITY: usize = 256;

/// 落盘统计,关机时写进日志。
#[derive(Debug, Default)]
pub struct SinkStats {
    pub enqueued: AtomicU64,
    pub dropped_queue_full: AtomicU64,
    pub written: AtomicU64,
    pub write_errors: AtomicU64,
}

impl SinkStats {
    /// 人可读的一行摘要。
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "enqueued={} dropped_queue_full={} written={} write_errors={}",
            self.enqueued.load(Ordering::Relaxed),
            self.dropped_queue_full.load(Ordering::Relaxed),
            self.written.load(Ordering::Relaxed),
            self.write_errors.load(Ordering::Relaxed),
        )
    }
}

/// 写线程 + 有界队列。
pub struct JsonLineSink {
    tx: Option<SyncSender<String>>,
    stats: Arc<SinkStats>,
    writer: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl JsonLineSink {
    /// 起一个写线程,输出 JSON Lines。
    ///
    /// # Errors
    ///
    /// 文件建不出来时返回 [`std::io::Error`]——**这是初始化期的硬失败**,
    /// 应当让 `scs_telemetry_init` 直接返回错误,而不是让插件带着一个哑巴 sink 跑一整局。
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(&path)?;
        let (tx, rx) = sync_channel::<String>(QUEUE_CAPACITY);
        let stats = Arc::new(SinkStats::default());
        let writer_stats = Arc::clone(&stats);
        let writer = std::thread::Builder::new()
            .name("ets2-telemetry-writer".to_owned())
            .spawn(move || {
                let mut out = BufWriter::new(file);
                // 发送端全部 drop 后 recv 返回 Err，循环自然结束——这就是关机信号。
                while let Ok(line) = rx.recv() {
                    match out
                        .write_all(line.as_bytes())
                        .and_then(|()| out.write_all(b"\n"))
                    {
                        Ok(()) => {
                            writer_stats.written.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            writer_stats.write_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                let _ = out.flush();
            })?;
        Ok(Self {
            tx: Some(tx),
            stats,
            writer: Some(writer),
            path,
        })
    }

    /// 游戏线程调用。**永不阻塞**:队列满就丢并计数。
    ///
    /// 返回是否入队成功。调用方**不要重试**——重试就是把阻塞换个写法。
    pub fn try_publish(&self, line: String) -> bool {
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        match tx.try_send(line) {
            Ok(()) => {
                self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_)) => {
                self.stats
                    .dropped_queue_full
                    .fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    #[must_use]
    pub fn stats(&self) -> &Arc<SinkStats> {
        &self.stats
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 关闭并等写线程把队列排干。
    ///
    /// 在 `scs_telemetry_shutdown` 里调用——那已经不是渲染线程,可以等。
    pub fn shutdown(&mut self) {
        drop(self.tx.take());
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for JsonLineSink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ets2-sink-test-{name}-{}.jsonl",
            std::process::id()
        ));
        p
    }

    /// 正常路径:入队的每一行都落盘,顺序保持。
    #[test]
    fn every_enqueued_line_reaches_the_file_in_order() {
        let path = temp_path("ordered");
        let mut sink = JsonLineSink::create(&path).expect("可建文件");
        for i in 0..50 {
            assert!(sink.try_publish(format!("{{\"i\":{i}}}")));
        }
        sink.shutdown();

        let content = std::fs::read_to_string(&path).expect("可读回");
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 50);
        assert_eq!(lines[0], "{\"i\":0}");
        assert_eq!(lines[49], "{\"i\":49}");
        assert_eq!(sink.stats().written.load(Ordering::Relaxed), 50);
        let _ = std::fs::remove_file(&path);
    }

    /// 建不出文件是初始化期硬失败,不能悄悄降级成哑巴 sink。
    #[test]
    fn an_uncreatable_path_fails_at_construction() {
        // 用一个已存在文件当作父目录，create_dir_all 必然失败。
        let blocker = temp_path("blocker");
        std::fs::write(&blocker, b"x").expect("可写");
        let bad = blocker.join("nested").join("out.jsonl");
        assert!(JsonLineSink::create(&bad).is_err());
        let _ = std::fs::remove_file(&blocker);
    }

    /// 关机后再发一律失败,不 panic——`shutdown` 可能先于最后一次回调到达。
    #[test]
    fn publishing_after_shutdown_fails_quietly() {
        let path = temp_path("after-shutdown");
        let mut sink = JsonLineSink::create(&path).expect("可建文件");
        assert!(sink.try_publish("{}".to_owned()));
        sink.shutdown();
        assert!(!sink.try_publish("{}".to_owned()), "关机后不该再入队");
        let _ = std::fs::remove_file(&path);
    }

    /// `shutdown` 幂等:`Drop` 里还会再调一次,不能第二次就 panic 或卡住。
    #[test]
    fn shutdown_is_idempotent() {
        let path = temp_path("idempotent");
        let mut sink = JsonLineSink::create(&path).expect("可建文件");
        sink.try_publish("{}".to_owned());
        sink.shutdown();
        sink.shutdown();
        drop(sink);
        let _ = std::fs::remove_file(&path);
    }

    /// 统计摘要把四个数都报出来——丢帧不可怕,不知道丢了多少才可怕。
    #[test]
    fn stats_summary_reports_every_counter() {
        let path = temp_path("stats");
        let mut sink = JsonLineSink::create(&path).expect("可建文件");
        sink.try_publish("{}".to_owned());
        sink.shutdown();
        let summary = sink.stats().summary();
        for key in [
            "enqueued=",
            "dropped_queue_full=",
            "written=",
            "write_errors=",
        ] {
            assert!(summary.contains(key), "摘要缺 {key}：{summary}");
        }
        let _ = std::fs::remove_file(&path);
    }
}
