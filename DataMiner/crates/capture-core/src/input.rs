use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use input_metrics::{POLLING_ESTIMATOR_VERSION, PollingRateEstimator};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

pub const RAW_INPUT_QUEUE_CAPACITY: usize = 32_768;
pub const RAW_INPUT_COMMAND_CAPACITY: usize = 16;
pub const RAW_INPUT_CONTROL_MESSAGE: u32 = WM_APP + 0x41;
pub const RAW_INPUT_ARM_MAX_SECONDS: i64 = 3;
const INPUT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub enum RawInputKind {
    Key {
        down: bool,
        vk: u16,
        scan_code: u16,
        flags: u16,
        repeat: bool,
    },
    MouseMove {
        dx: i32,
        dy: i32,
        absolute: bool,
    },
    MouseButton {
        down: bool,
        vk: u16,
    },
    Wheel {
        delta: i16,
        horizontal: bool,
    },
}

#[derive(Clone, Debug)]
pub struct RawInputPacket {
    pub qpc: i64,
    pub device: String,
    pub control: bool,
    pub kind: RawInputKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawInputEvent {
    pub seq: u64,
    pub run_id: String,
    pub qpc: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vk: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dx: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dy: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absolute: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub horizontal: Option<bool>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub control: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl RawInputPacket {
    fn into_event(self, seq: u64, run_id: &str) -> RawInputEvent {
        let mut event = RawInputEvent {
            seq,
            run_id: run_id.to_owned(),
            qpc: self.qpc,
            event_type: String::new(),
            device: self.device,
            vk: None,
            scan_code: None,
            flags: None,
            repeat: None,
            dx: None,
            dy: None,
            absolute: None,
            delta: None,
            horizontal: None,
            control: self.control,
        };
        match self.kind {
            RawInputKind::Key {
                down,
                vk,
                scan_code,
                flags,
                repeat,
            } => {
                event.event_type = if down { "key_down" } else { "key_up" }.into();
                event.vk = Some(vk);
                event.scan_code = Some(scan_code);
                event.flags = Some(flags);
                event.repeat = repeat.then_some(true);
            }
            RawInputKind::MouseMove { dx, dy, absolute } => {
                event.event_type = "mouse_move".into();
                event.dx = Some(dx);
                event.dy = Some(dy);
                event.absolute = Some(absolute);
            }
            RawInputKind::MouseButton { down, vk } => {
                event.event_type = if down { "mouse_down" } else { "mouse_up" }.into();
                event.vk = Some(vk);
            }
            RawInputKind::Wheel { delta, horizontal } => {
                event.event_type = "wheel".into();
                event.delta = Some(delta);
                event.horizontal = horizontal.then_some(true);
            }
        }
        event
    }
}

#[derive(Clone, Debug)]
pub struct EventSegmentSeal {
    pub segment_index: u32,
    pub qpc_start: i64,
    pub qpc_end: i64,
    pub event_count: u64,
    pub path: PathBuf,
    pub input_queue_overflows: u64,
    pub input_boundary_reorder_drops: u64,
    pub input_faulted: bool,
    pub input_fault_reason: Option<String>,
    pub input_raw_reports: u64,
    pub input_buffered_raw_reports: u64,
    pub input_max_raw_read_batch_size: u64,
    /// 本段按完整报告数修正 delivery batch 后的 p50 回报率（Hz）；移动
    /// 报告不足时为 None。docs/45 §4.4：随段自动实测，不问用户。
    pub input_polling_hz_p50: Option<f64>,
    pub input_polling_hz_p05: Option<f64>,
    pub input_polling_delivery_groups: u64,
    pub input_polling_max_delivery_batch_size: u64,
    pub input_polling_estimator: &'static str,
}

pub type InputAck<T> = Result<T, String>;

#[derive(Debug)]
pub enum RawInputThreadCommand {
    Arm {
        anchor_qpc: i64,
        first_segment_end_qpc: i64,
        response: Sender<InputAck<()>>,
    },
    Seal {
        segment_index: u32,
        qpc_end: i64,
        next_qpc_end: i64,
        response: Sender<InputAck<EventSegmentSeal>>,
    },
    Finish {
        segment_index: u32,
        effective_end_qpc: i64,
        response: Sender<InputAck<EventSegmentSeal>>,
    },
    Abort {
        response: Sender<InputAck<()>>,
    },
    ClearPressed,
}

#[derive(Debug)]
pub enum RawInputRecord {
    Event(RawInputPacket),
    Arm {
        anchor_qpc: i64,
        first_segment_end_qpc: i64,
        neutral_at_anchor: bool,
        arming_start_qpc: Option<i64>,
        response: Sender<InputAck<()>>,
    },
    Seal {
        segment_index: u32,
        qpc_end: i64,
        next_qpc_end: i64,
        response: Sender<InputAck<EventSegmentSeal>>,
    },
    Finish {
        segment_index: u32,
        effective_end_qpc: i64,
        response: Sender<InputAck<EventSegmentSeal>>,
    },
    Abort {
        response: Sender<InputAck<()>>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct RawInputStatus {
    inner: Arc<RawInputStatusInner>,
}

#[derive(Debug)]
struct RawInputStatusInner {
    pressed_keys: [AtomicU64; 4],
    mouse_buttons: AtomicU8,
    accepting: AtomicBool,
    faulted: AtomicBool,
    received_events: AtomicU64,
    persisted_events: AtomicU64,
    raw_reports: AtomicU64,
    buffered_raw_reports: AtomicU64,
    max_raw_read_batch_size: AtomicU64,
    queue_overflows: AtomicU64,
    boundary_reorder_drops: AtomicU64,
    queue_depth: AtomicUsize,
    max_queue_depth: AtomicUsize,
    callback_max_qpc: AtomicU64,
    fault_reason: Mutex<Option<String>>,
}

impl Default for RawInputStatusInner {
    fn default() -> Self {
        Self {
            pressed_keys: std::array::from_fn(|_| AtomicU64::new(0)),
            mouse_buttons: AtomicU8::new(0),
            accepting: AtomicBool::new(true),
            faulted: AtomicBool::new(false),
            received_events: AtomicU64::new(0),
            persisted_events: AtomicU64::new(0),
            raw_reports: AtomicU64::new(0),
            buffered_raw_reports: AtomicU64::new(0),
            max_raw_read_batch_size: AtomicU64::new(0),
            queue_overflows: AtomicU64::new(0),
            boundary_reorder_drops: AtomicU64::new(0),
            queue_depth: AtomicUsize::new(0),
            max_queue_depth: AtomicUsize::new(0),
            callback_max_qpc: AtomicU64::new(0),
            fault_reason: Mutex::new(None),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RawInputStatusSnapshot {
    pub pressed_contract_keys: Vec<u16>,
    pub mouse_buttons: Vec<u8>,
    pub received_events: u64,
    pub persisted_events: u64,
    pub raw_reports: u64,
    pub buffered_raw_reports: u64,
    pub max_raw_read_batch_size: u64,
    pub queue_overflows: u64,
    pub boundary_reorder_drops: u64,
    pub queue_depth: usize,
    pub max_queue_depth: usize,
    pub callback_max_qpc: u64,
    pub faulted: bool,
    pub fault_reason: Option<String>,
}

impl RawInputStatus {
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        self.inner.accepting.load(Ordering::Acquire)
    }

    pub fn close_gate(&self) {
        self.inner.accepting.store(false, Ordering::Release);
        self.clear_pressed();
    }

    /// Opens the event gate only while the input path is healthy. A faulted
    /// gate is terminal for the current run and cannot be reopened.
    pub fn open_gate(&self) -> bool {
        if self.faulted() {
            return false;
        }
        self.clear_pressed();
        self.inner.accepting.store(true, Ordering::Release);
        if self.faulted() {
            self.close_gate();
            return false;
        }
        true
    }

    pub fn mark_fault(&self, overflow: bool, reason: impl Into<String>) {
        if overflow {
            self.inner.queue_overflows.fetch_add(1, Ordering::Relaxed);
        }
        let mut fault_reason = self
            .inner
            .fault_reason
            .lock()
            .expect("Raw Input fault reason lock poisoned");
        if fault_reason.is_none() {
            *fault_reason = Some(reason.into());
        }
        self.inner.faulted.store(true, Ordering::Release);
        self.close_gate();
    }

    #[must_use]
    pub fn faulted(&self) -> bool {
        self.inner.faulted.load(Ordering::Acquire)
    }

    pub fn set_key(&self, vk: u16, down: bool) -> bool {
        let slot = usize::from(vk / 64);
        let bit = 1_u64 << (vk % 64);
        if down {
            self.inner.pressed_keys[slot].fetch_or(bit, Ordering::AcqRel) & bit != 0
        } else {
            self.inner.pressed_keys[slot].fetch_and(!bit, Ordering::AcqRel) & bit != 0
        }
    }

    pub fn set_mouse_button(&self, vk: u16, down: bool) -> bool {
        let Some(bit) = mouse_button_bit(vk) else {
            return false;
        };
        if down {
            self.inner.mouse_buttons.fetch_or(bit, Ordering::AcqRel) & bit != 0
        } else {
            self.inner.mouse_buttons.fetch_and(!bit, Ordering::AcqRel) & bit != 0
        }
    }

    pub fn clear_pressed(&self) {
        for slot in &self.inner.pressed_keys {
            slot.store(0, Ordering::Release);
        }
        self.inner.mouse_buttons.store(0, Ordering::Release);
    }

    pub fn note_received(&self, queue_depth: usize, callback_qpc: u64) {
        self.inner.received_events.fetch_add(1, Ordering::Relaxed);
        self.inner.queue_depth.store(queue_depth, Ordering::Relaxed);
        self.inner
            .max_queue_depth
            .fetch_max(queue_depth, Ordering::Relaxed);
        self.inner
            .callback_max_qpc
            .fetch_max(callback_qpc, Ordering::Relaxed);
    }

    /// Records one Win32 hybrid read (`GetRawInputData` current packet plus the
    /// `GetRawInputBuffer` tail) before catalog filtering. This is cumulative
    /// health evidence and proves whether high-frequency buffered reports were
    /// actually recovered.
    pub fn note_raw_batch(&self, report_count: usize, buffered_report_count: usize) {
        let reports = u64::try_from(report_count).unwrap_or(u64::MAX);
        let buffered = u64::try_from(buffered_report_count).unwrap_or(u64::MAX);
        self.inner.raw_reports.fetch_add(reports, Ordering::Relaxed);
        self.inner
            .buffered_raw_reports
            .fetch_add(buffered, Ordering::Relaxed);
        self.inner
            .max_raw_read_batch_size
            .fetch_max(reports, Ordering::Relaxed);
    }

    pub fn note_persisted(&self) {
        self.inner.persisted_events.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a raw-input event that reached the append writer after the
    /// segment boundary had already advanced past its QPC. Non-fatal by design:
    /// the straddling event is dropped and counted so a long recording survives
    /// the input/Seal ordering race instead of failing the whole run.
    pub fn note_boundary_reorder_drop(&self) {
        self.inner
            .boundary_reorder_drops
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_queue_depth(&self, depth: usize) {
        self.inner.queue_depth.store(depth, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> RawInputStatusSnapshot {
        let mut pressed_contract_keys = Vec::new();
        for (slot_index, slot) in self.inner.pressed_keys.iter().enumerate() {
            let mut bits = slot.load(Ordering::Acquire);
            while bits != 0 {
                let offset = bits.trailing_zeros() as usize;
                pressed_contract_keys.push((slot_index * 64 + offset) as u16);
                bits &= bits - 1;
            }
        }
        let button_bits = self.inner.mouse_buttons.load(Ordering::Acquire);
        let mouse_buttons = [1_u16, 2, 4, 5, 6]
            .into_iter()
            .filter(|vk| mouse_button_bit(*vk).is_some_and(|bit| button_bits & bit != 0))
            .map(|vk| vk as u8)
            .collect();
        RawInputStatusSnapshot {
            pressed_contract_keys,
            mouse_buttons,
            received_events: self.inner.received_events.load(Ordering::Relaxed),
            persisted_events: self.inner.persisted_events.load(Ordering::Relaxed),
            raw_reports: self.inner.raw_reports.load(Ordering::Relaxed),
            buffered_raw_reports: self.inner.buffered_raw_reports.load(Ordering::Relaxed),
            max_raw_read_batch_size: self.inner.max_raw_read_batch_size.load(Ordering::Relaxed),
            queue_overflows: self.inner.queue_overflows.load(Ordering::Relaxed),
            boundary_reorder_drops: self.inner.boundary_reorder_drops.load(Ordering::Relaxed),
            queue_depth: self.inner.queue_depth.load(Ordering::Relaxed),
            max_queue_depth: self.inner.max_queue_depth.load(Ordering::Relaxed),
            callback_max_qpc: self.inner.callback_max_qpc.load(Ordering::Relaxed),
            faulted: self.inner.faulted.load(Ordering::Acquire),
            fault_reason: self
                .inner
                .fault_reason
                .lock()
                .expect("Raw Input fault reason lock poisoned")
                .clone(),
        }
    }
}

fn mouse_button_bit(vk: u16) -> Option<u8> {
    match vk {
        1 => Some(1 << 0),
        2 => Some(1 << 1),
        4 => Some(1 << 2),
        5 => Some(1 << 3),
        6 => Some(1 << 4),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct RawInputControl {
    thread_id: u32,
    commands: Sender<RawInputThreadCommand>,
    emergency_records: Sender<RawInputRecord>,
    status: RawInputStatus,
}

impl RawInputControl {
    #[must_use]
    pub fn new(
        thread_id: u32,
        commands: Sender<RawInputThreadCommand>,
        emergency_records: Sender<RawInputRecord>,
        status: RawInputStatus,
    ) -> Self {
        Self {
            thread_id,
            commands,
            emergency_records,
            status,
        }
    }

    pub fn request_arm(
        &self,
        anchor_qpc: i64,
        first_segment_end_qpc: i64,
    ) -> anyhow::Result<Receiver<InputAck<()>>> {
        let (response, receiver) = bounded(1);
        self.post(RawInputThreadCommand::Arm {
            anchor_qpc,
            first_segment_end_qpc,
            response,
        })?;
        Ok(receiver)
    }

    pub fn request_seal(
        &self,
        segment_index: u32,
        qpc_end: i64,
        next_qpc_end: i64,
    ) -> anyhow::Result<Receiver<InputAck<EventSegmentSeal>>> {
        let (response, receiver) = bounded(1);
        self.post(RawInputThreadCommand::Seal {
            segment_index,
            qpc_end,
            next_qpc_end,
            response,
        })?;
        Ok(receiver)
    }

    pub fn request_finish(
        &self,
        segment_index: u32,
        effective_end_qpc: i64,
    ) -> anyhow::Result<Receiver<InputAck<EventSegmentSeal>>> {
        let (response, receiver) = bounded(1);
        self.post(RawInputThreadCommand::Finish {
            segment_index,
            effective_end_qpc,
            response,
        })?;
        Ok(receiver)
    }

    pub fn request_abort(&self) -> anyhow::Result<Receiver<InputAck<()>>> {
        let (response, receiver) = bounded(1);
        self.post(RawInputThreadCommand::Abort { response })?;
        Ok(receiver)
    }

    pub fn close_gate(&self) -> anyhow::Result<()> {
        self.status.close_gate();
        self.post(RawInputThreadCommand::ClearPressed)
    }

    fn post(&self, command: RawInputThreadCommand) -> anyhow::Result<()> {
        self.commands
            .try_send(command)
            .context("Raw Input command queue is full")?;
        unsafe {
            PostThreadMessageW(
                self.thread_id,
                RAW_INPUT_CONTROL_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .context("failed to wake Raw Input message thread")
    }

    pub fn emergency_abort(&self) {
        self.status.close_gate();
        let (response, _receiver) = bounded(1);
        let _ = self
            .emergency_records
            .send_timeout(RawInputRecord::Abort { response }, INPUT_ACK_TIMEOUT);
    }
}

pub struct RawEventWriter {
    join: Option<JoinHandle<anyhow::Result<()>>>,
    failures: Receiver<String>,
    control: RawInputControl,
}

impl RawEventWriter {
    pub fn start(
        records: Receiver<RawInputRecord>,
        control: RawInputControl,
        status: RawInputStatus,
        spool_dir: PathBuf,
        run_id: String,
        qpc_frequency: i64,
    ) -> anyhow::Result<Self> {
        let (failure_tx, failures) = bounded(1);
        let join = thread::Builder::new()
            .name("dm-append-writer".into())
            .spawn(move || {
                let result =
                    run_event_writer(&records, &status, &spool_dir, &run_id, qpc_frequency);
                if let Err(error) = &result {
                    let _ = failure_tx.try_send(format!("{error:#}"));
                }
                result
            })
            .context("failed to spawn Raw Input append writer")?;
        Ok(Self {
            join: Some(join),
            failures,
            control,
        })
    }

    pub fn check(&self) -> anyhow::Result<()> {
        if let Ok(error) = self.failures.try_recv() {
            anyhow::bail!("Raw Input append writer failed: {error}");
        }
        Ok(())
    }

    pub fn stop(mut self) -> anyhow::Result<()> {
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| anyhow::anyhow!("Raw Input append writer panicked"))??;
        }
        Ok(())
    }
}

impl Drop for RawEventWriter {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.control.emergency_abort();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct EventFile {
    index: u32,
    qpc_start: i64,
    qpc_end: i64,
    active_path: PathBuf,
    writer: BufWriter<File>,
    offsets: Vec<(i64, u64)>,
    event_count: u64,
    bytes_written: u64,
    last_checkpoint: Instant,
    polling_rate: PollingRateEstimator,
}

impl EventFile {
    fn create(
        spool_dir: &Path,
        index: u32,
        qpc_start: i64,
        qpc_end: i64,
        qpc_frequency: i64,
    ) -> anyhow::Result<Self> {
        ensure!(qpc_end > qpc_start, "input segment boundary is empty");
        fs::create_dir_all(spool_dir)?;
        let active_path = spool_dir.join(format!("{index:05}.events.jsonl.active"));
        let writer = BufWriter::with_capacity(64 * 1024, File::create(&active_path)?);
        Ok(Self {
            index,
            qpc_start,
            qpc_end,
            active_path,
            writer,
            offsets: Vec::new(),
            event_count: 0,
            bytes_written: 0,
            last_checkpoint: Instant::now(),
            polling_rate: PollingRateEstimator::new(qpc_frequency),
        })
    }

    fn append(&mut self, event: &RawInputEvent) -> anyhow::Result<()> {
        ensure!(
            event.qpc >= self.qpc_start && event.qpc < self.qpc_end,
            "Raw Input event is outside its active segment"
        );
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        self.offsets.push((event.qpc, self.bytes_written));
        self.writer.write_all(&line)?;
        self.bytes_written += line.len() as u64;
        self.event_count += 1;
        // 相同 QPC 的移动报告属于一次 delivery batch；估算器按该批报告数
        // 修正相邻批次间隔，不能把 message-loop 唤醒率冒充硬件回报率。
        if event.dx.is_some() {
            self.polling_rate.push(event.qpc);
        }
        if self.last_checkpoint.elapsed() >= Duration::from_secs(1) {
            self.writer.flush()?;
            self.writer.get_ref().sync_data()?;
            self.last_checkpoint = Instant::now();
        }
        Ok(())
    }

    fn seal(
        mut self,
        effective_end_qpc: i64,
        status: &RawInputStatus,
        _qpc_frequency: i64,
    ) -> anyhow::Result<EventSegmentSeal> {
        ensure!(
            effective_end_qpc > self.qpc_start && effective_end_qpc <= self.qpc_end,
            "invalid Raw Input effective segment end"
        );
        let keep = self
            .offsets
            .partition_point(|(qpc, _)| *qpc < effective_end_qpc);
        let truncate_at = self
            .offsets
            .get(keep)
            .map_or(self.bytes_written, |(_, offset)| *offset);
        self.writer.flush()?;
        let mut file = self.writer.into_inner()?;
        file.set_len(truncate_at)?;
        file.seek(SeekFrom::End(0))?;
        file.sync_data()?;
        drop(file);
        let sealed_path = self
            .active_path
            .with_file_name(format!("{:05}.events.jsonl.sealed", self.index));
        fs::rename(&self.active_path, &sealed_path)?;
        let input = status.snapshot();
        let polling = self.polling_rate.finish(POLLING_MIN_REPORTS);
        Ok(EventSegmentSeal {
            segment_index: self.index,
            qpc_start: self.qpc_start,
            qpc_end: effective_end_qpc,
            event_count: keep as u64,
            path: sealed_path,
            input_queue_overflows: input.queue_overflows,
            input_boundary_reorder_drops: input.boundary_reorder_drops,
            input_faulted: input.faulted,
            input_fault_reason: input.fault_reason,
            input_raw_reports: input.raw_reports,
            input_buffered_raw_reports: input.buffered_raw_reports,
            input_max_raw_read_batch_size: input.max_raw_read_batch_size,
            // 未截断口径：含 seal 截尾之前累计的全部移动报告 delivery group。
            input_polling_hz_p50: polling.p50_hz,
            input_polling_hz_p05: polling.p05_hz,
            input_polling_delivery_groups: polling.delivery_group_count,
            input_polling_max_delivery_batch_size: polling.max_delivery_batch_size,
            input_polling_estimator: POLLING_ESTIMATOR_VERSION,
        })
    }
}

/// p50 至少需要的可估算移动报告数——太少不出数。
const POLLING_MIN_REPORTS: u64 = 64;

fn run_event_writer(
    records: &Receiver<RawInputRecord>,
    status: &RawInputStatus,
    spool_dir: &Path,
    run_id: &str,
    qpc_frequency: i64,
) -> anyhow::Result<()> {
    let mut arming: VecDeque<RawInputPacket> = VecDeque::new();
    let mut future: VecDeque<RawInputPacket> = VecDeque::new();
    let mut current: Option<EventFile> = None;
    let mut next_seq = 0_u64;
    while let Ok(record) = records.recv() {
        status.set_queue_depth(records.len());
        match record {
            RawInputRecord::Event(packet) => {
                if let Some(file) = &mut current {
                    route_packet(packet, file, &mut future, &mut next_seq, run_id, status)?;
                } else {
                    if let Some(first) = arming.front()
                        && packet.qpc - first.qpc > qpc_frequency * RAW_INPUT_ARM_MAX_SECONDS
                    {
                        status.mark_fault(false, "Raw Input arming exceeded three seconds");
                        anyhow::bail!("Raw Input arming exceeded three seconds");
                    }
                    arming.push_back(packet);
                }
            }
            RawInputRecord::Arm {
                anchor_qpc,
                first_segment_end_qpc,
                neutral_at_anchor,
                arming_start_qpc,
                response,
            } => {
                let result = (|| {
                    ensure!(current.is_none(), "Raw Input writer was armed twice");
                    ensure!(neutral_at_anchor, "INPUT_NOT_NEUTRAL at first video frame");
                    if let Some(start) = arming_start_qpc {
                        ensure!(
                            anchor_qpc - start <= qpc_frequency * RAW_INPUT_ARM_MAX_SECONDS,
                            "Raw Input arming exceeded three seconds"
                        );
                    }
                    let mut file = EventFile::create(
                        spool_dir,
                        0,
                        anchor_qpc,
                        first_segment_end_qpc,
                        qpc_frequency,
                    )?;
                    while let Some(packet) = arming.pop_front() {
                        if packet.qpc >= anchor_qpc {
                            route_packet(
                                packet,
                                &mut file,
                                &mut future,
                                &mut next_seq,
                                run_id,
                                status,
                            )?;
                        }
                    }
                    current = Some(file);
                    Ok(())
                })();
                send_ack(response, result)?;
            }
            RawInputRecord::Seal {
                segment_index,
                qpc_end,
                next_qpc_end,
                response,
            } => {
                let result = (|| {
                    let file = current.take().context("Raw Input writer is not armed")?;
                    ensure!(
                        file.index == segment_index,
                        "Raw Input segment index mismatch"
                    );
                    ensure!(file.qpc_end == qpc_end, "Raw Input segment end mismatch");
                    let sealed = file.seal(qpc_end, status, qpc_frequency)?;
                    let mut next = EventFile::create(
                        spool_dir,
                        segment_index + 1,
                        qpc_end,
                        next_qpc_end,
                        qpc_frequency,
                    )?;
                    let pending = std::mem::take(&mut future);
                    for packet in pending {
                        route_packet(
                            packet,
                            &mut next,
                            &mut future,
                            &mut next_seq,
                            run_id,
                            status,
                        )?;
                    }
                    current = Some(next);
                    Ok(sealed)
                })();
                send_ack(response, result)?;
            }
            RawInputRecord::Finish {
                segment_index,
                effective_end_qpc,
                response,
            } => {
                let result = (|| {
                    let file = current.take().context("Raw Input writer is not armed")?;
                    ensure!(
                        file.index == segment_index,
                        "Raw Input final segment mismatch"
                    );
                    file.seal(effective_end_qpc, status, qpc_frequency)
                })();
                send_ack(response, result)?;
                return Ok(());
            }
            RawInputRecord::Abort { response } => {
                let active_path = current.as_ref().map(|file| file.active_path.clone());
                drop(current.take());
                if let Some(path) = active_path
                    && path.exists()
                {
                    fs::remove_file(path)?;
                }
                if spool_dir.exists() {
                    for entry in fs::read_dir(spool_dir)? {
                        let path = entry?.path();
                        if path.is_file()
                            && path
                                .file_name()
                                .is_some_and(|name| name.to_string_lossy().ends_with(".active"))
                        {
                            fs::remove_file(path)?;
                        }
                    }
                }
                send_ack(response, Ok(()))?;
                return Ok(());
            }
        }
    }
    anyhow::bail!("Raw Input record channel disconnected")
}

fn route_packet(
    packet: RawInputPacket,
    current: &mut EventFile,
    future: &mut VecDeque<RawInputPacket>,
    next_seq: &mut u64,
    run_id: &str,
    status: &RawInputStatus,
) -> anyhow::Result<()> {
    if packet.qpc >= current.qpc_end {
        future.push_back(packet);
        return Ok(());
    }
    if packet.qpc < current.qpc_start {
        // The recorder advances segments off the video-frame QPC timeline
        // (RawInputRecord::Seal) while input events are stamped on a separate
        // thread; both are multiplexed onto one channel and processed in arrival
        // order, not QPC order. An input event that occurred microseconds before
        // a boundary can therefore reach the writer just after the Seal that
        // advanced `current`. That is a cross-thread ordering race, not a
        // monotonic-clock regression (the retained per-segment QPC stays strictly
        // increasing), so drop and count the single straddling event instead of
        // failing the entire run.
        status.note_boundary_reorder_drop();
        tracing::warn!(
            packet_qpc = packet.qpc,
            segment_qpc_start = current.qpc_start,
            "dropped a raw-input event delivered before the current segment boundary"
        );
        return Ok(());
    }
    let event = packet.into_event(*next_seq, run_id);
    current.append(&event)?;
    *next_seq += 1;
    status.note_persisted();
    Ok(())
}

fn send_ack<T>(response: Sender<InputAck<T>>, result: anyhow::Result<T>) -> anyhow::Result<()> {
    response
        .send(result.map_err(|error| format!("{error:#}")))
        .map_err(|_| anyhow::anyhow!("Raw Input acknowledgement receiver disappeared"))
}

pub fn wait_input_ack<T>(receiver: &Receiver<InputAck<T>>, operation: &str) -> anyhow::Result<T> {
    receiver
        .recv_timeout(INPUT_ACK_TIMEOUT)
        .with_context(|| format!("timed out waiting for Raw Input {operation}"))?
        .map_err(|error| anyhow::anyhow!("Raw Input {operation} failed: {error}"))
}

pub fn open_sealed_event_file(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open sealed Raw Input file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn mouse_move(qpc: i64) -> RawInputPacket {
        RawInputPacket {
            qpc,
            device: "mouse:test".into(),
            control: false,
            kind: RawInputKind::MouseMove {
                dx: 1,
                dy: -1,
                absolute: false,
            },
        }
    }

    #[test]
    fn raw_event_schema_keeps_physical_key_facts() {
        let event = RawInputPacket {
            qpc: 42,
            device: "kbd:test".into(),
            control: false,
            kind: RawInputKind::Key {
                down: true,
                vk: 87,
                scan_code: 17,
                flags: 2,
                repeat: false,
            },
        }
        .into_event(7, "run-test");
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["type"], "key_down");
        assert_eq!(json["vk"], 87);
        assert_eq!(json["scan_code"], 17);
        assert!(json.get("control").is_none());
        assert!(json.get("repeat").is_none());
        let decoded: RawInputEvent = serde_json::from_value(json).unwrap();
        assert!(!decoded.control);
    }

    #[test]
    fn pressed_snapshot_is_sorted_and_cleared_with_gate() {
        let status = RawInputStatus::default();
        status.set_key(87, true);
        status.set_key(32, true);
        status.set_mouse_button(2, true);
        assert_eq!(status.snapshot().pressed_contract_keys, vec![32, 87]);
        assert_eq!(status.snapshot().mouse_buttons, vec![2]);
        status.close_gate();
        assert!(status.snapshot().pressed_contract_keys.is_empty());
        assert!(status.snapshot().mouse_buttons.is_empty());
    }

    #[test]
    fn closed_gate_reopens_only_while_healthy() {
        let healthy = RawInputStatus::default();
        healthy.close_gate();
        assert!(!healthy.is_accepting());
        assert!(healthy.open_gate());
        assert!(healthy.is_accepting());

        let faulted = RawInputStatus::default();
        faulted.close_gate();
        faulted.mark_fault(false, "test fault");
        assert!(!faulted.open_gate());
        assert!(!faulted.is_accepting());
    }

    #[test]
    fn event_segment_health_recovers_batched_five_hundred_hz() {
        const QPC_FREQUENCY: i64 = 10_000_000;
        let root = std::env::temp_dir().join(format!(
            "dataminer-input-polling-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let status = RawInputStatus::default();
        let mut file = EventFile::create(&root, 0, 0, QPC_FREQUENCY * 3, QPC_FREQUENCY)
            .expect("creates event file");
        let mut seq = 0_u64;
        for batch in 1..=50_i64 {
            let qpc = batch * (QPC_FREQUENCY / 125);
            status.note_raw_batch(4, 3);
            for _ in 0..4 {
                let event = mouse_move(qpc).into_event(seq, "run-test");
                file.append(&event).expect("appends report");
                seq += 1;
            }
        }
        let seal = file
            .seal(QPC_FREQUENCY * 3, &status, QPC_FREQUENCY)
            .expect("seals event file");
        assert_eq!(seal.event_count, 200);
        assert_eq!(seal.input_raw_reports, 200);
        assert_eq!(seal.input_buffered_raw_reports, 150);
        assert_eq!(seal.input_max_raw_read_batch_size, 4);
        assert_eq!(seal.input_polling_hz_p50, Some(500.0));
        assert_eq!(seal.input_polling_hz_p05, Some(500.0));
        assert_eq!(seal.input_polling_delivery_groups, 50);
        assert_eq!(seal.input_polling_max_delivery_batch_size, 4);
        assert_eq!(seal.input_polling_estimator, POLLING_ESTIMATOR_VERSION);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn append_writer_seals_half_open_segments_and_truncates_stop_tail() {
        let root = std::env::temp_dir().join(format!(
            "dataminer-input-test-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let spool = root.join("spool");
        let (records, receiver) = bounded(RAW_INPUT_QUEUE_CAPACITY);
        let (commands, _command_receiver) = bounded(RAW_INPUT_COMMAND_CAPACITY);
        let status = RawInputStatus::default();
        let status_handle = status.clone();
        let control = RawInputControl::new(0, commands, records.clone(), status.clone());
        let writer =
            RawEventWriter::start(receiver, control, status, spool, "run-test".into(), 1_000)
                .unwrap();

        records.send(RawInputRecord::Event(mouse_move(10))).unwrap();
        let (arm_tx, arm_rx) = bounded(1);
        records
            .send(RawInputRecord::Arm {
                anchor_qpc: 0,
                first_segment_end_qpc: 100,
                neutral_at_anchor: true,
                arming_start_qpc: Some(10),
                response: arm_tx,
            })
            .unwrap();
        wait_input_ack(&arm_rx, "test arm").unwrap();
        records
            .send(RawInputRecord::Event(mouse_move(110)))
            .unwrap();
        records
            .send(RawInputRecord::Event(mouse_move(130)))
            .unwrap();

        let (seal_tx, seal_rx) = bounded(1);
        records
            .send(RawInputRecord::Seal {
                segment_index: 0,
                qpc_end: 100,
                next_qpc_end: 200,
                response: seal_tx,
            })
            .unwrap();
        let first = wait_input_ack(&seal_rx, "test seal").unwrap();
        assert_eq!(first.event_count, 1);
        assert_eq!(first.input_queue_overflows, 0);
        assert!(!first.input_faulted);

        status_handle.mark_fault(true, "test overflow after seal request");
        let (finish_tx, finish_rx) = bounded(1);
        records
            .send(RawInputRecord::Finish {
                segment_index: 1,
                effective_end_qpc: 120,
                response: finish_tx,
            })
            .unwrap();
        let second = wait_input_ack(&finish_rx, "test finish").unwrap();
        assert_eq!(second.event_count, 1);
        assert_eq!(second.input_queue_overflows, 1);
        assert!(second.input_faulted);
        assert_eq!(
            second.input_fault_reason.as_deref(),
            Some("test overflow after seal request")
        );
        writer.stop().unwrap();

        let first_text = fs::read_to_string(first.path).unwrap();
        let second_text = fs::read_to_string(second.path).unwrap();
        assert_eq!(first_text.lines().count(), 1);
        assert_eq!(second_text.lines().count(), 1);
        assert!(second_text.contains("\"qpc\":110"));
        assert!(!second_text.contains("\"qpc\":130"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn append_writer_tolerates_boundary_reordered_events_without_failing() {
        let root = std::env::temp_dir().join(format!(
            "dataminer-input-reorder-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let spool = root.join("spool");
        let (records, receiver) = bounded(RAW_INPUT_QUEUE_CAPACITY);
        let (commands, _command_receiver) = bounded(RAW_INPUT_COMMAND_CAPACITY);
        let status = RawInputStatus::default();
        let status_handle = status.clone();
        let control = RawInputControl::new(0, commands, records.clone(), status.clone());
        let writer =
            RawEventWriter::start(receiver, control, status, spool, "run-test".into(), 1_000)
                .unwrap();

        // Segment 0 = [0, 100) with one in-range event.
        let (arm_tx, arm_rx) = bounded(1);
        records
            .send(RawInputRecord::Arm {
                anchor_qpc: 0,
                first_segment_end_qpc: 100,
                neutral_at_anchor: true,
                arming_start_qpc: None,
                response: arm_tx,
            })
            .unwrap();
        wait_input_ack(&arm_rx, "test arm").unwrap();
        records.send(RawInputRecord::Event(mouse_move(40))).unwrap();

        // Advance to segment 1 = [100, 200).
        let (seal_tx, seal_rx) = bounded(1);
        records
            .send(RawInputRecord::Seal {
                segment_index: 0,
                qpc_end: 100,
                next_qpc_end: 200,
                response: seal_tx,
            })
            .unwrap();
        let first = wait_input_ack(&seal_rx, "test seal").unwrap();
        assert_eq!(first.event_count, 1);
        assert_eq!(first.input_boundary_reorder_drops, 0);

        // A raw-input event stamped before the boundary is delivered *after* the
        // Seal advanced `current` to segment 1. It must be dropped and counted,
        // not fail the writer (the historic run-killing regression).
        records.send(RawInputRecord::Event(mouse_move(60))).unwrap();

        let (finish_tx, finish_rx) = bounded(1);
        records
            .send(RawInputRecord::Finish {
                segment_index: 1,
                effective_end_qpc: 150,
                response: finish_tx,
            })
            .unwrap();
        let second = wait_input_ack(&finish_rx, "test finish").unwrap();
        assert_eq!(second.event_count, 0);
        assert_eq!(second.input_boundary_reorder_drops, 1);
        assert!(!second.input_faulted);
        assert_eq!(status_handle.snapshot().boundary_reorder_drops, 1);
        writer.stop().unwrap();

        // The dropped straddling event reached neither sealed segment.
        let first_text = fs::read_to_string(first.path).unwrap();
        let second_text = fs::read_to_string(second.path).unwrap();
        assert_eq!(first_text.lines().count(), 1);
        assert_eq!(second_text.lines().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
