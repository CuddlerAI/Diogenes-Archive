//! Windows pagefile-backed transport for the two selected-Present SPSC queues.

#[cfg(test)]
mod even_cfr_tests;

use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};
use windows::Win32::System::Threading::{CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent};
use windows::core::PCWSTR;

use crate::feedback::{
    Ack, Decision, Envelope, FEEDBACK_SCHEMA_V3, FEEDBACK_SCHEMA_V4, FaultReason, ProtocolError,
    RUN_ID_CAPACITY, SESSION_ID_CAPACITY, StreamEpoch, WireIdentity,
};

const QUEUE_MAGIC: [u8; 8] = *b"DMSELP03";
const QUEUE_ABI: u16 = 3;
const SLOT_SIZE: usize = 256;
const DIRECTION_DECISION: u16 = 1;
const DIRECTION_ACK: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportNames {
    pub decision_mapping: String,
    pub decision_event: String,
    pub ack_mapping: String,
    pub ack_event: String,
}

impl TransportNames {
    pub fn new(
        client_pid: u32,
        game_pid: u32,
        generation: u64,
        nonce: &str,
    ) -> Result<Self, TransportError> {
        if nonce.is_empty()
            || nonce.len() > 64
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(TransportError::InvalidName);
        }
        let base = format!(
            "Local\\Diogenes.SelectedPresent.v1.{client_pid}.{game_pid}.{generation}.{nonce}"
        );
        Ok(Self {
            decision_mapping: format!("{base}.decision.map"),
            decision_event: format!("{base}.decision.event"),
            ack_mapping: format!("{base}.ack.map"),
            ack_event: format!("{base}.ack.event"),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("selected-Present transport name is invalid")]
    InvalidName,
    #[error("selected-Present queue capacity is invalid")]
    InvalidCapacity,
    #[error("selected-Present named object already exists: {0}")]
    AlreadyExists(String),
    #[error("selected-Present Windows transport error: {0}")]
    Windows(String),
    #[error("selected-Present mapping header is corrupt: {0}")]
    Corrupt(&'static str),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

#[repr(C, align(64))]
struct CacheLineU64 {
    value: AtomicU64,
    _padding: [u8; 56],
}

impl CacheLineU64 {
    fn new(value: u64) -> Self {
        Self {
            value: AtomicU64::new(value),
            _padding: [0; 56],
        }
    }
}

#[repr(C, align(64))]
struct QueueHeader {
    magic: [u8; 8],
    abi: u16,
    direction: u16,
    header_size: u32,
    slot_size: u32,
    capacity: u32,
    total_size: u32,
    session_len: u8,
    run_len: u8,
    _reserved0: [u8; 6],
    session_id: [u8; SESSION_ID_CAPACITY],
    run_id: [u8; RUN_ID_CAPACITY],
    generation: u64,
    resource_epoch: u64,
    head: CacheLineU64,
    tail: CacheLineU64,
    max_depth: CacheLineU64,
    full_count: CacheLineU64,
    fatal_reason: AtomicU32,
    _reserved1: [u8; 60],
}

#[repr(C, align(64))]
struct QueueSlot {
    commit_ordinal: AtomicU64,
    payload: UnsafeCell<[u8; SLOT_SIZE - 8]>,
}

#[derive(Clone, Copy)]
struct QueueLayout {
    direction: u16,
    header_size: u32,
    slot_size: u32,
    capacity: u32,
    total_size: u32,
}

struct Mapping {
    handle: HANDLE,
    event: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    header: NonNull<QueueHeader>,
    slots: NonNull<QueueSlot>,
    stream: StreamEpoch,
    layout: QueueLayout,
}

struct OpenMappingGuard {
    handle: HANDLE,
    event: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl OpenMappingGuard {
    fn into_mapping(
        self,
        header: NonNull<QueueHeader>,
        slots: NonNull<QueueSlot>,
        stream: StreamEpoch,
        layout: QueueLayout,
    ) -> Mapping {
        let mapping = Mapping {
            handle: self.handle,
            event: self.event,
            view: self.view,
            header,
            slots,
            stream,
            layout,
        };
        std::mem::forget(self);
        mapping
    }
}

impl Drop for OpenMappingGuard {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.view) };
        let _ = unsafe { CloseHandle(self.event) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueueStats {
    pub depth: u64,
    pub max_depth: u64,
    pub full_count: u64,
    pub fatal_reason: u32,
}

impl Mapping {
    fn create(
        mapping_name: &str,
        event_name: &str,
        direction: u16,
        stream: &StreamEpoch,
        capacity: usize,
    ) -> Result<Self, TransportError> {
        let capacity_u32 = u32::try_from(capacity).map_err(|_| TransportError::InvalidCapacity)?;
        if capacity_u32 == 0 {
            return Err(TransportError::InvalidCapacity);
        }
        let header_size = align_up(std::mem::size_of::<QueueHeader>(), 64);
        let total_size = header_size
            .checked_add(
                capacity
                    .checked_mul(std::mem::size_of::<QueueSlot>())
                    .ok_or(TransportError::InvalidCapacity)?,
            )
            .ok_or(TransportError::InvalidCapacity)?;
        let total_u32 = u32::try_from(total_size).map_err(|_| TransportError::InvalidCapacity)?;
        let mapping_wide = wide(mapping_name);
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                total_u32,
                PCWSTR(mapping_wide.as_ptr()),
            )
        }
        .map_err(|error| TransportError::Windows(error.to_string()))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let _ = unsafe { CloseHandle(handle) };
            return Err(TransportError::AlreadyExists(mapping_name.to_owned()));
        }
        let event_wide = wide(event_name);
        let event = match unsafe { CreateEventW(None, false, false, PCWSTR(event_wide.as_ptr())) } {
            Ok(event) if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS => event,
            Ok(event) => {
                let _ = unsafe { CloseHandle(event) };
                let _ = unsafe { CloseHandle(handle) };
                return Err(TransportError::AlreadyExists(event_name.to_owned()));
            }
            Err(error) => {
                let _ = unsafe { CloseHandle(handle) };
                return Err(TransportError::Windows(error.to_string()));
            }
        };
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, total_size) };
        if view.Value.is_null() {
            let _ = unsafe { CloseHandle(event) };
            let _ = unsafe { CloseHandle(handle) };
            return Err(TransportError::Windows(
                "MapViewOfFile returned null".into(),
            ));
        }
        let header = NonNull::new(view.Value.cast::<QueueHeader>())
            .ok_or(TransportError::Windows("mapping header is null".into()))?;
        let slots: NonNull<QueueSlot> =
            NonNull::new(unsafe { view.Value.cast::<u8>().add(header_size) }.cast())
                .ok_or(TransportError::Windows("mapping slots are null".into()))?;
        let (session_len, session_id) =
            fixed_bytes::<SESSION_ID_CAPACITY>(stream.identity.session_id());
        let (run_len, run_id) = fixed_bytes::<RUN_ID_CAPACITY>(stream.identity.run_id());
        unsafe {
            header.as_ptr().write(QueueHeader {
                magic: QUEUE_MAGIC,
                abi: QUEUE_ABI,
                direction,
                header_size: header_size as u32,
                slot_size: SLOT_SIZE as u32,
                capacity: capacity_u32,
                total_size: total_u32,
                session_len,
                run_len,
                _reserved0: [0; 6],
                session_id,
                run_id,
                generation: stream.generation,
                resource_epoch: stream.resource_epoch,
                head: CacheLineU64::new(0),
                tail: CacheLineU64::new(0),
                max_depth: CacheLineU64::new(0),
                full_count: CacheLineU64::new(0),
                fatal_reason: AtomicU32::new(0),
                _reserved1: [0; 60],
            });
            for index in 0..capacity {
                slots.as_ptr().add(index).write(QueueSlot {
                    commit_ordinal: AtomicU64::new(0),
                    payload: UnsafeCell::new([0; SLOT_SIZE - 8]),
                });
            }
        }
        Ok(Self {
            handle,
            event,
            view,
            header,
            slots,
            stream: stream.clone(),
            layout: QueueLayout {
                direction,
                header_size: header_size as u32,
                slot_size: SLOT_SIZE as u32,
                capacity: capacity_u32,
                total_size: total_u32,
            },
        })
    }

    fn open(
        mapping_name: &str,
        event_name: &str,
        direction: u16,
        stream: &StreamEpoch,
    ) -> Result<Self, TransportError> {
        let mapping_wide = wide(mapping_name);
        let handle = unsafe {
            OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(mapping_wide.as_ptr()))
        }
        .map_err(|error| TransportError::Windows(error.to_string()))?;
        let event_wide = wide(event_name);
        let event =
            match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_wide.as_ptr())) } {
                Ok(event) => event,
                Err(error) => {
                    let _ = unsafe { CloseHandle(handle) };
                    return Err(TransportError::Windows(error.to_string()));
                }
            };
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            let _ = unsafe { CloseHandle(event) };
            let _ = unsafe { CloseHandle(handle) };
            return Err(TransportError::Windows(
                "MapViewOfFile returned null".into(),
            ));
        }
        let resources = OpenMappingGuard {
            handle,
            event,
            view,
        };
        let mut memory = MEMORY_BASIC_INFORMATION::default();
        let queried = unsafe {
            VirtualQuery(
                Some(view.Value.cast_const()),
                &mut memory,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 || memory.RegionSize < std::mem::size_of::<QueueHeader>() {
            return Err(TransportError::Corrupt(
                "mapping view is smaller than header",
            ));
        }
        let header = NonNull::new(view.Value.cast::<QueueHeader>())
            .ok_or(TransportError::Windows("mapping header is null".into()))?;
        let header_ref = unsafe { header.as_ref() };
        validate_header(header_ref, direction, stream, memory.RegionSize)?;
        let layout = QueueLayout {
            direction: header_ref.direction,
            header_size: header_ref.header_size,
            slot_size: header_ref.slot_size,
            capacity: header_ref.capacity,
            total_size: header_ref.total_size,
        };
        let slots: NonNull<QueueSlot> = NonNull::new(
            unsafe { view.Value.cast::<u8>().add(layout.header_size as usize) }.cast(),
        )
        .ok_or(TransportError::Corrupt("slot pointer"))?;
        Ok(resources.into_mapping(header, slots, stream.clone(), layout))
    }

    fn header(&self) -> &QueueHeader {
        unsafe { self.header.as_ref() }
    }

    fn capacity(&self) -> usize {
        self.layout.capacity as usize
    }

    fn validate_runtime_layout(&self) -> Result<(), TransportError> {
        let header = self.header();
        if header.magic != QUEUE_MAGIC
            || header.abi != QUEUE_ABI
            || header.direction != self.layout.direction
            || header.header_size != self.layout.header_size
            || header.slot_size != self.layout.slot_size
            || header.capacity != self.layout.capacity
            || header.total_size != self.layout.total_size
        {
            self.latch_fault(FaultReason::TransportCorrupt);
            return Err(TransportError::Corrupt("runtime layout mutation"));
        }
        Ok(())
    }

    fn latch_fault(&self, reason: FaultReason) {
        let _ = self.header().fatal_reason.compare_exchange(
            0,
            reason as u32,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn fault_reason(&self) -> Result<Option<FaultReason>, TransportError> {
        self.validate_runtime_layout()?;
        let reason = self.header().fatal_reason.load(Ordering::Acquire);
        if reason == 0 {
            return Ok(None);
        }
        let reason = u16::try_from(reason)
            .map_err(|_| TransportError::Corrupt("fault reason exceeds wire range"))?;
        decode_reason(reason).map(Some)
    }

    fn stats(&self) -> Result<QueueStats, TransportError> {
        self.validate_runtime_layout()?;
        let header = self.header();
        let head = header.head.value.load(Ordering::Acquire);
        let tail = header.tail.value.load(Ordering::Acquire);
        let depth = head
            .checked_sub(tail)
            .ok_or(TransportError::Corrupt("head precedes tail"))?;
        if depth > u64::from(self.layout.capacity) {
            return Err(TransportError::Corrupt("queue depth exceeds capacity"));
        }
        Ok(QueueStats {
            depth,
            max_depth: header.max_depth.value.load(Ordering::Acquire),
            full_count: header.full_count.value.load(Ordering::Acquire),
            fatal_reason: header.fatal_reason.load(Ordering::Acquire),
        })
    }

    fn push_bytes(&self, ordinal: u64, bytes: &[u8]) -> Result<(), TransportError> {
        if ordinal == 0 || bytes.len() != SLOT_SIZE - 8 {
            self.latch_fault(FaultReason::TransportCorrupt);
            return Err(TransportError::Corrupt("slot encoding"));
        }
        self.validate_runtime_layout()?;
        let header = self.header();
        let head = header.head.value.load(Ordering::Relaxed);
        let tail = header.tail.value.load(Ordering::Acquire);
        let depth = head.checked_sub(tail).ok_or_else(|| {
            self.latch_fault(FaultReason::TransportCorrupt);
            TransportError::Corrupt("head precedes tail")
        })?;
        if depth >= u64::from(self.layout.capacity) {
            header.full_count.value.fetch_add(1, Ordering::Relaxed);
            let reason = if self.layout.direction == DIRECTION_DECISION {
                FaultReason::FeedbackQueueFull
            } else {
                FaultReason::AckQueueFull
            };
            self.latch_fault(reason);
            return Err(TransportError::Protocol(ProtocolError::Fault(reason)));
        }
        let index = usize::try_from(head % u64::from(self.layout.capacity))
            .map_err(|_| TransportError::Corrupt("slot index"))?;
        let slot = unsafe { &*self.slots.as_ptr().add(index) };
        unsafe { (*slot.payload.get()).copy_from_slice(bytes) };
        slot.commit_ordinal.store(ordinal, Ordering::Release);
        header.head.value.store(head + 1, Ordering::Release);
        header
            .max_depth
            .value
            .fetch_max(depth + 1, Ordering::Relaxed);
        unsafe { SetEvent(self.event) }
            .map_err(|error| TransportError::Windows(error.to_string()))?;
        Ok(())
    }

    fn pop_bytes(&self) -> Result<Option<[u8; SLOT_SIZE - 8]>, TransportError> {
        self.validate_runtime_layout()?;
        let header = self.header();
        let tail = header.tail.value.load(Ordering::Relaxed);
        let head = header.head.value.load(Ordering::Acquire);
        let depth = head
            .checked_sub(tail)
            .ok_or(TransportError::Corrupt("head precedes tail"))?;
        if depth == 0 {
            return Ok(None);
        }
        if depth > u64::from(self.layout.capacity) {
            self.latch_fault(FaultReason::TransportCorrupt);
            return Err(TransportError::Corrupt("queue depth exceeds capacity"));
        }
        let index = usize::try_from(tail % u64::from(self.layout.capacity))
            .map_err(|_| TransportError::Corrupt("slot index"))?;
        let slot = unsafe { &*self.slots.as_ptr().add(index) };
        let committed = slot.commit_ordinal.load(Ordering::Acquire);
        if committed == 0 {
            self.latch_fault(FaultReason::TransportCorrupt);
            return Err(TransportError::Corrupt("published slot is uncommitted"));
        }
        let bytes = unsafe { *slot.payload.get() };
        if slot.commit_ordinal.load(Ordering::Acquire) != committed {
            self.latch_fault(FaultReason::TransportCorrupt);
            return Err(TransportError::Corrupt("slot changed during read"));
        }
        header.tail.value.store(tail + 1, Ordering::Release);
        Ok(Some(bytes))
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.view) };
        let _ = unsafe { CloseHandle(self.event) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

pub struct DecisionProducer {
    mapping: Mapping,
    _not_sync: PhantomData<Cell<()>>,
}

pub struct DecisionConsumer {
    mapping: Mapping,
    _not_sync: PhantomData<Cell<()>>,
}

pub struct AckProducer {
    mapping: Mapping,
    _not_sync: PhantomData<Cell<()>>,
}

pub struct AckConsumer {
    mapping: Mapping,
    _not_sync: PhantomData<Cell<()>>,
}

unsafe impl Send for DecisionProducer {}
unsafe impl Send for DecisionConsumer {}
unsafe impl Send for AckProducer {}
unsafe impl Send for AckConsumer {}

pub struct ClientTransport {
    pub decisions: DecisionProducer,
    pub acks: AckConsumer,
}

pub struct ProbeTransport {
    pub decisions: DecisionConsumer,
    pub acks: AckProducer,
}

impl ClientTransport {
    pub fn create(
        names: &TransportNames,
        stream: &StreamEpoch,
        capacity: usize,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            decisions: DecisionProducer {
                mapping: Mapping::create(
                    &names.decision_mapping,
                    &names.decision_event,
                    DIRECTION_DECISION,
                    stream,
                    capacity,
                )?,
                _not_sync: PhantomData,
            },
            acks: AckConsumer {
                mapping: Mapping::create(
                    &names.ack_mapping,
                    &names.ack_event,
                    DIRECTION_ACK,
                    stream,
                    capacity,
                )?,
                _not_sync: PhantomData,
            },
        })
    }

    #[must_use]
    pub fn stream(&self) -> &StreamEpoch {
        &self.decisions.mapping.stream
    }

    #[must_use]
    pub fn queue_capacity(&self) -> usize {
        self.decisions.mapping.capacity()
    }

    /// Observe a terminal peer fault even when no ACK can be queued. Callers
    /// drain already published ACKs first so a specific Fault ACK is not hidden
    /// by a later queue-full condition.
    pub fn check_fault(&self) -> Result<(), TransportError> {
        let reason = match self.acks.mapping.fault_reason()? {
            Some(reason) => Some(reason),
            None => self.decisions.mapping.fault_reason()?,
        };
        match reason {
            Some(reason) => Err(TransportError::Protocol(ProtocolError::Fault(reason))),
            None => Ok(()),
        }
    }
}

impl ProbeTransport {
    pub fn open(names: &TransportNames, stream: &StreamEpoch) -> Result<Self, TransportError> {
        Ok(Self {
            decisions: DecisionConsumer {
                mapping: Mapping::open(
                    &names.decision_mapping,
                    &names.decision_event,
                    DIRECTION_DECISION,
                    stream,
                )?,
                _not_sync: PhantomData,
            },
            acks: AckProducer {
                mapping: Mapping::open(
                    &names.ack_mapping,
                    &names.ack_event,
                    DIRECTION_ACK,
                    stream,
                )?,
                _not_sync: PhantomData,
            },
        })
    }

    /// Publish a terminal fault without becoming another producer of either
    /// SPSC queue or inventing a decision ordinal. The existing ABI-3 header
    /// latch outlives the probe endpoint while the client owns its mappings.
    /// A later cleanup error must not replace an already stored fault.
    pub fn publish_fault(&self, reason: FaultReason) -> Result<(), TransportError> {
        let reason = self
            .acks
            .mapping
            .fault_reason()?
            .or(self.decisions.mapping.fault_reason()?)
            .unwrap_or(reason);
        self.acks.mapping.latch_fault(reason);
        self.decisions.mapping.latch_fault(reason);
        // Attempt both wakes even if one fails. The durable header latches are
        // already visible to the next client poll; no queue slot was consumed.
        let ack_wake = unsafe { SetEvent(self.acks.mapping.event) };
        let decision_wake = unsafe { SetEvent(self.decisions.mapping.event) };
        ack_wake
            .and(decision_wake)
            .map_err(|error| TransportError::Windows(error.to_string()))
    }
}

impl DecisionProducer {
    pub fn try_push(&mut self, message: &Envelope<Decision>) -> Result<(), TransportError> {
        validate_stream(&self.mapping.stream, message)?;
        self.mapping
            .push_bytes(message.ordinal, &encode_decision(message)?)
    }
}

impl DecisionConsumer {
    pub fn try_pop(&mut self) -> Result<Option<Envelope<Decision>>, TransportError> {
        self.mapping
            .pop_bytes()?
            .map(|bytes| {
                let message = decode_decision(&bytes)?;
                validate_stream(&self.mapping.stream, &message)?;
                Ok(message)
            })
            .transpose()
    }

    pub fn stats(&self) -> Result<QueueStats, TransportError> {
        self.mapping.stats()
    }
}

impl AckProducer {
    pub fn try_push(&mut self, message: &Envelope<Ack>) -> Result<(), TransportError> {
        validate_stream(&self.mapping.stream, message)?;
        self.mapping
            .push_bytes(message.ordinal, &encode_ack(message)?)
    }

    pub fn stats(&self) -> Result<QueueStats, TransportError> {
        self.mapping.stats()
    }
}

impl AckConsumer {
    pub fn try_pop(&mut self) -> Result<Option<Envelope<Ack>>, TransportError> {
        self.mapping
            .pop_bytes()?
            .map(|bytes| {
                let message = decode_ack(&bytes)?;
                validate_stream(&self.mapping.stream, &message)?;
                Ok(message)
            })
            .transpose()
    }
}

fn validate_header(
    header: &QueueHeader,
    direction: u16,
    stream: &StreamEpoch,
    mapped_region_size: usize,
) -> Result<(), TransportError> {
    if header.magic != QUEUE_MAGIC || header.abi != QUEUE_ABI {
        return Err(TransportError::Corrupt("magic/ABI"));
    }
    let header_size = align_up(std::mem::size_of::<QueueHeader>(), 64);
    let capacity = usize::try_from(header.capacity)
        .map_err(|_| TransportError::Corrupt("capacity does not fit address space"))?;
    let expected_total = capacity
        .checked_mul(SLOT_SIZE)
        .and_then(|slots| header_size.checked_add(slots))
        .ok_or(TransportError::Corrupt("mapping layout overflow"))?;
    if header.direction != direction
        || header.slot_size as usize != SLOT_SIZE
        || header.capacity == 0
        || header.header_size as usize != header_size
        || usize::try_from(header.total_size).ok() != Some(expected_total)
        || mapped_region_size < expected_total
    {
        return Err(TransportError::Corrupt("layout/direction"));
    }
    if header.session_len == 0
        || usize::from(header.session_len) > SESSION_ID_CAPACITY
        || header.run_len == 0
        || usize::from(header.run_len) > RUN_ID_CAPACITY
    {
        return Err(TransportError::Corrupt("mapping identity length"));
    }
    let identity = WireIdentity::new(
        std::str::from_utf8(&header.session_id[..usize::from(header.session_len)])
            .map_err(|_| TransportError::Corrupt("session identity UTF-8"))?,
        std::str::from_utf8(&header.run_id[..usize::from(header.run_len)])
            .map_err(|_| TransportError::Corrupt("run identity UTF-8"))?,
    )?;
    if identity != stream.identity
        || header.generation != stream.generation
        || header.resource_epoch != stream.resource_epoch
    {
        return Err(TransportError::Protocol(ProtocolError::StreamMismatch(
            "mapping header",
        )));
    }
    Ok(())
}

fn validate_stream<T>(expected: &StreamEpoch, message: &Envelope<T>) -> Result<(), TransportError> {
    if !matches!(message.schema, FEEDBACK_SCHEMA_V3 | FEEDBACK_SCHEMA_V4) {
        return Err(TransportError::Protocol(ProtocolError::SchemaMismatch));
    }
    if &message.stream != expected {
        return Err(TransportError::Protocol(ProtocolError::StreamMismatch(
            "message",
        )));
    }
    Ok(())
}

fn encode_decision(message: &Envelope<Decision>) -> Result<[u8; SLOT_SIZE - 8], TransportError> {
    let mut bytes = [0u8; SLOT_SIZE - 8];
    encode_common(&mut bytes, message, decision_kind(&message.payload))?;
    match message.payload {
        Decision::ArmGeneration => {}
        Decision::Select {
            session_frame,
            present_seq,
            depth_required,
            duplicate,
            source_superseded,
            source_qpc,
            callback_start_qpc,
            callback_ready_qpc,
            decision_qpc,
        } => {
            put_u64(&mut bytes, 136, session_frame);
            put_u64(&mut bytes, 144, present_seq);
            bytes[152] = u8::from(duplicate);
            encode_depth_required(&mut bytes, 153, message.schema, depth_required)?;
            put_u32(&mut bytes, 156, source_superseded);
            put_i64(&mut bytes, 160, source_qpc);
            put_i64(&mut bytes, 168, callback_start_qpc);
            put_i64(&mut bytes, 176, callback_ready_qpc);
            put_i64(&mut bytes, 184, decision_qpc);
        }
        Decision::RetireThrough { present_seq } => put_u64(&mut bytes, 136, present_seq),
        Decision::SealGeneration {
            final_session_frame,
            expected_select_count,
            expected_distinct_depth_count,
        } => {
            bytes[136] = u8::from(final_session_frame.is_some());
            put_u64(&mut bytes, 144, final_session_frame.unwrap_or_default());
            put_u64(&mut bytes, 152, expected_select_count);
            put_u64(&mut bytes, 160, expected_distinct_depth_count);
        }
    }
    Ok(bytes)
}

fn decode_decision(bytes: &[u8; SLOT_SIZE - 8]) -> Result<Envelope<Decision>, TransportError> {
    let (schema, stream, ordinal, kind) = decode_common(bytes)?;
    let payload = match kind {
        4 => Decision::ArmGeneration,
        1 => Decision::Select {
            session_frame: get_u64(bytes, 136),
            present_seq: get_u64(bytes, 144),
            depth_required: decode_depth_required(bytes, 153, schema)?,
            duplicate: match bytes[152] {
                0 => false,
                1 => true,
                _ => return Err(TransportError::Corrupt("duplicate flag")),
            },
            source_superseded: get_u32(bytes, 156),
            source_qpc: get_i64(bytes, 160),
            callback_start_qpc: get_i64(bytes, 168),
            callback_ready_qpc: get_i64(bytes, 176),
            decision_qpc: get_i64(bytes, 184),
        },
        2 => Decision::RetireThrough {
            present_seq: get_u64(bytes, 136),
        },
        3 => Decision::SealGeneration {
            final_session_frame: match bytes[136] {
                0 => None,
                1 => Some(get_u64(bytes, 144)),
                _ => return Err(TransportError::Corrupt("seal frame flag")),
            },
            expected_select_count: get_u64(bytes, 152),
            expected_distinct_depth_count: get_u64(bytes, 160),
        },
        _ => return Err(TransportError::Corrupt("decision kind")),
    };
    Ok(Envelope {
        schema,
        stream,
        ordinal,
        payload,
    })
}

fn encode_ack(message: &Envelope<Ack>) -> Result<[u8; SLOT_SIZE - 8], TransportError> {
    let mut bytes = [0u8; SLOT_SIZE - 8];
    encode_common(&mut bytes, message, ack_kind(&message.payload))?;
    match message.payload {
        Ack::GenerationArmed {
            decision_ordinal,
            first_candidate_present_seq,
        } => {
            put_u64(&mut bytes, 136, decision_ordinal);
            put_u64(&mut bytes, 144, first_candidate_present_seq);
        }
        Ack::SelectAccepted {
            decision_ordinal,
            session_frame,
            present_seq,
            depth_required,
            depth_already_selected,
        } => {
            put_u64(&mut bytes, 136, decision_ordinal);
            put_u64(&mut bytes, 144, session_frame);
            put_u64(&mut bytes, 152, present_seq);
            bytes[160] = u8::from(depth_already_selected);
            encode_depth_required(&mut bytes, 161, message.schema, depth_required)?;
        }
        Ack::Retired {
            decision_ordinal,
            present_seq,
        } => {
            put_u64(&mut bytes, 136, decision_ordinal);
            put_u64(&mut bytes, 144, present_seq);
        }
        Ack::GenerationSealed {
            decision_ordinal,
            select_count,
            distinct_depth_count,
        } => {
            put_u64(&mut bytes, 136, decision_ordinal);
            put_u64(&mut bytes, 144, select_count);
            put_u64(&mut bytes, 152, distinct_depth_count);
        }
        Ack::Fault {
            decision_ordinal,
            reason,
        } => {
            put_u64(&mut bytes, 136, decision_ordinal);
            put_u16(&mut bytes, 144, reason as u16);
        }
    }
    Ok(bytes)
}

fn decode_ack(bytes: &[u8; SLOT_SIZE - 8]) -> Result<Envelope<Ack>, TransportError> {
    let (schema, stream, ordinal, kind) = decode_common(bytes)?;
    let payload = match kind {
        105 => Ack::GenerationArmed {
            decision_ordinal: get_u64(bytes, 136),
            first_candidate_present_seq: get_u64(bytes, 144),
        },
        101 => Ack::SelectAccepted {
            decision_ordinal: get_u64(bytes, 136),
            session_frame: get_u64(bytes, 144),
            present_seq: get_u64(bytes, 152),
            depth_required: decode_depth_required(bytes, 161, schema)?,
            depth_already_selected: match bytes[160] {
                0 => false,
                1 => true,
                _ => return Err(TransportError::Corrupt("depth alias flag")),
            },
        },
        102 => Ack::Retired {
            decision_ordinal: get_u64(bytes, 136),
            present_seq: get_u64(bytes, 144),
        },
        103 => Ack::GenerationSealed {
            decision_ordinal: get_u64(bytes, 136),
            select_count: get_u64(bytes, 144),
            distinct_depth_count: get_u64(bytes, 152),
        },
        104 => Ack::Fault {
            decision_ordinal: get_u64(bytes, 136),
            reason: decode_reason(get_u16(bytes, 144))?,
        },
        _ => return Err(TransportError::Corrupt("ACK kind")),
    };
    Ok(Envelope {
        schema,
        stream,
        ordinal,
        payload,
    })
}

fn encode_depth_required(
    bytes: &mut [u8; SLOT_SIZE - 8],
    offset: usize,
    schema: u16,
    required: bool,
) -> Result<(), TransportError> {
    match schema {
        FEEDBACK_SCHEMA_V3 if required => Ok(()), // Preserve V3 reserved bytes.
        FEEDBACK_SCHEMA_V4 => {
            bytes[offset] = u8::from(required);
            Ok(())
        }
        _ => Err(TransportError::Protocol(ProtocolError::SchemaMismatch)),
    }
}

fn decode_depth_required(
    bytes: &[u8; SLOT_SIZE - 8],
    offset: usize,
    schema: u16,
) -> Result<bool, TransportError> {
    match (schema, bytes[offset]) {
        (FEEDBACK_SCHEMA_V3, 0) => Ok(true),
        (FEEDBACK_SCHEMA_V4, 0) => Ok(false),
        (FEEDBACK_SCHEMA_V4, 1) => Ok(true),
        _ => Err(TransportError::Corrupt("depth required flag/schema")),
    }
}

fn encode_common<T>(
    bytes: &mut [u8; SLOT_SIZE - 8],
    message: &Envelope<T>,
    kind: u16,
) -> Result<(), TransportError> {
    put_u16(bytes, 0, message.schema);
    put_u16(bytes, 2, kind);
    let session = message.stream.identity.session_id().as_bytes();
    let run = message.stream.identity.run_id().as_bytes();
    bytes[4] = u8::try_from(session.len()).map_err(|_| TransportError::InvalidName)?;
    bytes[5] = u8::try_from(run.len()).map_err(|_| TransportError::InvalidName)?;
    bytes[8..8 + session.len()].copy_from_slice(session);
    bytes[72..72 + run.len()].copy_from_slice(run);
    put_u64(bytes, 104, message.stream.generation);
    put_u64(bytes, 112, message.stream.resource_epoch);
    put_u64(bytes, 120, message.ordinal);
    Ok(())
}

fn decode_common(
    bytes: &[u8; SLOT_SIZE - 8],
) -> Result<(u16, StreamEpoch, u64, u16), TransportError> {
    let session_len = usize::from(bytes[4]);
    let run_len = usize::from(bytes[5]);
    if session_len == 0
        || session_len > SESSION_ID_CAPACITY
        || run_len == 0
        || run_len > RUN_ID_CAPACITY
    {
        return Err(TransportError::Corrupt("message identity length"));
    }
    let session = std::str::from_utf8(&bytes[8..8 + session_len])
        .map_err(|_| TransportError::Corrupt("message session UTF-8"))?;
    let run = std::str::from_utf8(&bytes[72..72 + run_len])
        .map_err(|_| TransportError::Corrupt("message run UTF-8"))?;
    Ok((
        get_u16(bytes, 0),
        StreamEpoch {
            identity: WireIdentity::new(session, run)?,
            generation: get_u64(bytes, 104),
            resource_epoch: get_u64(bytes, 112),
        },
        get_u64(bytes, 120),
        get_u16(bytes, 2),
    ))
}

fn decision_kind(decision: &Decision) -> u16 {
    match decision {
        Decision::ArmGeneration => 4,
        Decision::Select { .. } => 1,
        Decision::RetireThrough { .. } => 2,
        Decision::SealGeneration { .. } => 3,
    }
}

fn ack_kind(ack: &Ack) -> u16 {
    match ack {
        Ack::GenerationArmed { .. } => 105,
        Ack::SelectAccepted { .. } => 101,
        Ack::Retired { .. } => 102,
        Ack::GenerationSealed { .. } => 103,
        Ack::Fault { .. } => 104,
    }
}

fn decode_reason(value: u16) -> Result<FaultReason, TransportError> {
    use FaultReason::*;
    Ok(match value {
        1 => FeedbackQueueFull,
        2 => AckQueueFull,
        3 => IdentityMismatch,
        4 => GenerationMismatch,
        5 => ResourceEpochMismatch,
        6 => DecisionOrdinalGap,
        7 => SessionFrameConflict,
        8 => DuplicateSourceMismatch,
        9 => RetireFrontierRegressed,
        10 => SelectAfterRetire,
        11 => SelectAfterSeal,
        12 => SealCountMismatch,
        13 => StopBeforeGenerationSealed,
        14 => SelectedFeedbackLate,
        15 => RingOverwriteUnresolved,
        16 => RingIdentityMismatch,
        17 => ResizeAfterReady,
        18 => ReadbackPoolFull,
        19 => ReadbackTimeout,
        20 => ReadbackMapFailed,
        21 => WriterQueueFull,
        22 => WatermarkWriteFailed,
        23 => VramBudgetInsufficient,
        24 => CandidateAllocationFailed,
        25 => FlushCountMismatch,
        26 => PeerExited,
        27 => TransportCorrupt,
        28 => PresentSeqRegressed,
        29 => FeedbackTimestampInvalid,
        30 => FeedbackTelemetryOverflow,
        31 => SwapchainAdapterMismatch,
        32 => ArmRepeated,
        33 => ArmAfterSeal,
        34 => CandidateBeforeArm,
        35 => FeedbackBeforeArm,
        36 => ArmBoundaryMissing,
        _ => return Err(TransportError::Corrupt("fault reason")),
    })
}

fn fixed_bytes<const N: usize>(value: &str) -> (u8, [u8; N]) {
    let mut bytes = [0; N];
    bytes[..value.len()].copy_from_slice(value.as_bytes());
    (value.len() as u8, bytes)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

const fn align_up(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("fixed wire range"),
    )
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed wire range"),
    )
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed wire range"),
    )
}

fn get_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed wire range"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NONCE: AtomicU64 = AtomicU64::new(1);

    fn fixture() -> (TransportNames, StreamEpoch) {
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let names = TransportNames::new(
            std::process::id(),
            std::process::id(),
            70,
            &format!("test-{nonce}"),
        )
        .unwrap();
        let stream = StreamEpoch {
            identity: WireIdentity::new("s-transport", "r0001").unwrap(),
            generation: 70,
            resource_epoch: 2,
        };
        (names, stream)
    }

    #[test]
    fn independent_decision_and_ack_mappings_roundtrip() {
        let (names, stream) = fixture();
        let mut client = ClientTransport::create(&names, &stream, 4).unwrap();
        let mut probe = ProbeTransport::open(&names, &stream).unwrap();
        let decision = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream: stream.clone(),
            ordinal: 1,
            payload: Decision::Select {
                depth_required: true,
                session_frame: 80,
                present_seq: 900,
                duplicate: false,
                source_superseded: 3,
                source_qpc: 10_000,
                callback_start_qpc: 10_010,
                callback_ready_qpc: 10_015,
                decision_qpc: 10_020,
            },
        };
        client.decisions.try_push(&decision).unwrap();
        assert_eq!(probe.decisions.stats().unwrap().depth, 1);
        assert_eq!(probe.decisions.try_pop().unwrap(), Some(decision));
        assert_eq!(probe.decisions.try_pop().unwrap(), None);
        assert_eq!(probe.decisions.stats().unwrap().depth, 0);

        let ack = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream: stream.clone(),
            ordinal: 1,
            payload: Ack::SelectAccepted {
                depth_required: true,
                decision_ordinal: 1,
                session_frame: 80,
                present_seq: 900,
                depth_already_selected: false,
            },
        };
        probe.acks.try_push(&ack).unwrap();
        assert_eq!(probe.acks.stats().unwrap().depth, 1);
        assert_eq!(client.acks.try_pop().unwrap(), Some(ack));
        assert_eq!(probe.acks.stats().unwrap().depth, 0);
    }

    #[test]
    fn arm_and_actual_first_candidate_boundary_roundtrip() {
        let (_, stream) = fixture();
        let arm = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream: stream.clone(),
            ordinal: 1,
            payload: Decision::ArmGeneration,
        };
        assert_eq!(
            decode_decision(&encode_decision(&arm).unwrap()).unwrap(),
            arm
        );

        let armed = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream,
            ordinal: 1,
            payload: Ack::GenerationArmed {
                decision_ordinal: 1,
                first_candidate_present_seq: 65_537,
            },
        };
        assert_eq!(decode_ack(&encode_ack(&armed).unwrap()).unwrap(), armed);
    }

    #[test]
    fn full_queue_never_overwrites_an_unread_message() {
        let (names, stream) = fixture();
        let mut client = ClientTransport::create(&names, &stream, 1).unwrap();
        let mut probe = ProbeTransport::open(&names, &stream).unwrap();
        let decision = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream: stream.clone(),
            ordinal: 1,
            payload: Decision::RetireThrough { present_seq: 50 },
        };
        client.decisions.try_push(&decision).unwrap();
        let mut second = decision.clone();
        second.ordinal = 2;
        assert!(matches!(
            client.decisions.try_push(&second),
            Err(TransportError::Protocol(ProtocolError::Fault(
                FaultReason::FeedbackQueueFull
            )))
        ));
        assert_eq!(probe.decisions.try_pop().unwrap(), Some(decision));
    }

    #[test]
    fn peer_fault_is_visible_without_an_ack_and_survives_endpoint_drop() {
        let (names, stream) = fixture();
        let mut client = ClientTransport::create(&names, &stream, 256).unwrap();
        let probe = ProbeTransport::open(&names, &stream).unwrap();
        client.check_fault().unwrap();
        probe.publish_fault(FaultReason::ReadbackMapFailed).unwrap();
        assert_eq!(probe.decisions.stats().unwrap().depth, 0);
        assert_eq!(probe.acks.stats().unwrap().depth, 0);
        drop(probe);
        assert_eq!(client.acks.try_pop().unwrap(), None);
        assert!(matches!(
            client.check_fault(),
            Err(TransportError::Protocol(ProtocolError::Fault(
                FaultReason::ReadbackMapFailed
            )))
        ));
    }

    #[test]
    fn cleanup_fault_cannot_replace_the_published_cause_or_touch_queue_slots() {
        let (names, stream) = fixture();
        let mut client = ClientTransport::create(&names, &stream, 1).unwrap();
        let mut probe = ProbeTransport::open(&names, &stream).unwrap();
        let decision = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream,
            ordinal: 1,
            payload: Decision::RetireThrough { present_seq: 50 },
        };
        client.decisions.try_push(&decision).unwrap();
        probe.publish_fault(FaultReason::ReadbackMapFailed).unwrap();
        probe.publish_fault(FaultReason::WriterQueueFull).unwrap();
        for stats in [
            probe.decisions.stats().unwrap(),
            probe.acks.stats().unwrap(),
        ] {
            assert_eq!(stats.fatal_reason, FaultReason::ReadbackMapFailed as u32);
            assert_eq!(stats.full_count, 0);
        }
        assert_eq!(probe.decisions.try_pop().unwrap(), Some(decision));
    }

    #[test]
    fn peer_fault_preserves_an_already_latched_queue_failure() {
        let (names, stream) = fixture();
        let mut client = ClientTransport::create(&names, &stream, 1).unwrap();
        let probe = ProbeTransport::open(&names, &stream).unwrap();
        let mut decision = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream,
            ordinal: 1,
            payload: Decision::RetireThrough { present_seq: 50 },
        };
        client.decisions.try_push(&decision).unwrap();
        decision.ordinal = 2;
        assert!(client.decisions.try_push(&decision).is_err());
        probe.publish_fault(FaultReason::ReadbackMapFailed).unwrap();
        assert!(matches!(
            client.check_fault(),
            Err(TransportError::Protocol(ProtocolError::Fault(
                FaultReason::FeedbackQueueFull
            )))
        ));
    }

    #[test]
    fn unknown_shared_fault_code_fails_closed_without_narrowing() {
        let (names, stream) = fixture();
        let client = ClientTransport::create(&names, &stream, 1).unwrap();
        client
            .acks
            .mapping
            .header()
            .fatal_reason
            .store(u32::MAX, Ordering::Release);
        assert!(matches!(
            client.check_fault(),
            Err(TransportError::Corrupt("fault reason exceeds wire range"))
        ));
    }

    #[test]
    fn opening_with_the_wrong_generation_is_rejected() {
        let (names, stream) = fixture();
        let _client = ClientTransport::create(&names, &stream, 2).unwrap();
        let mut wrong = stream;
        wrong.generation += 1;
        assert!(matches!(
            ProbeTransport::open(&names, &wrong),
            Err(TransportError::Protocol(ProtocolError::StreamMismatch(_)))
        ));
    }

    #[test]
    fn older_transport_abis_are_rejected() {
        for (magic, abi) in [(*b"DMSELP01", 1), (*b"DMSELP02", 2)] {
            let (names, stream) = fixture();
            let client = ClientTransport::create(&names, &stream, 2).unwrap();
            unsafe {
                (*client.decisions.mapping.header.as_ptr()).magic = magic;
                (*client.decisions.mapping.header.as_ptr()).abi = abi;
            }
            assert!(matches!(
                ProbeTransport::open(&names, &stream),
                Err(TransportError::Corrupt("magic/ABI"))
            ));
        }
    }

    #[test]
    fn corrupt_mapping_identity_lengths_fail_closed_without_panicking() {
        let (names, stream) = fixture();
        let client = ClientTransport::create(&names, &stream, 2).unwrap();
        unsafe {
            (*client.decisions.mapping.header.as_ptr()).session_len = u8::MAX;
        }
        assert!(matches!(
            ProbeTransport::open(&names, &stream),
            Err(TransportError::Corrupt("mapping identity length"))
        ));
    }

    #[test]
    fn runtime_layout_mutation_is_rejected_before_queue_indexing() {
        let (names, stream) = fixture();
        let mut client = ClientTransport::create(&names, &stream, 2).unwrap();
        let mut probe = ProbeTransport::open(&names, &stream).unwrap();
        unsafe {
            (*client.decisions.mapping.header.as_ptr()).capacity = u32::MAX;
            (*client.decisions.mapping.header.as_ptr()).direction = DIRECTION_ACK;
        }
        let decision = Envelope {
            schema: FEEDBACK_SCHEMA_V3,
            stream,
            ordinal: 1,
            payload: Decision::RetireThrough { present_seq: 50 },
        };
        assert!(matches!(
            client.decisions.try_push(&decision),
            Err(TransportError::Corrupt("runtime layout mutation"))
        ));
        assert!(matches!(
            probe.decisions.try_pop(),
            Err(TransportError::Corrupt("runtime layout mutation"))
        ));
        assert_eq!(client.queue_capacity(), 2);
    }

    #[test]
    fn decision_queue_survives_threaded_spsc_pressure_without_torn_payloads() {
        const COUNT: u64 = 4_096;
        const CAPACITY: usize = 8;
        let (names, stream) = fixture();
        let client = ClientTransport::create(&names, &stream, CAPACITY).unwrap();
        let probe = ProbeTransport::open(&names, &stream).unwrap();
        let ClientTransport {
            mut decisions,
            acks: _client_acks,
        } = client;
        let ProbeTransport {
            decisions: mut consumer,
            acks: _probe_acks,
        } = probe;
        let consumed = std::sync::Arc::new(AtomicU64::new(0));
        let producer_consumed = consumed.clone();
        let producer_stream = stream.clone();
        let producer = std::thread::spawn(move || {
            for ordinal in 1..=COUNT {
                while ordinal.saturating_sub(producer_consumed.load(Ordering::Acquire))
                    >= CAPACITY as u64
                {
                    std::thread::yield_now();
                }
                decisions
                    .try_push(&Envelope {
                        schema: FEEDBACK_SCHEMA_V3,
                        stream: producer_stream.clone(),
                        ordinal,
                        payload: Decision::RetireThrough {
                            present_seq: ordinal * 17,
                        },
                    })
                    .unwrap();
            }
        });
        let consumer_progress = consumed;
        let consumer_thread = std::thread::spawn(move || {
            let mut expected = 1;
            while expected <= COUNT {
                if let Some(message) = consumer.try_pop().unwrap() {
                    assert_eq!(message.ordinal, expected);
                    assert_eq!(
                        message.payload,
                        Decision::RetireThrough {
                            present_seq: expected * 17
                        }
                    );
                    consumer_progress.store(expected, Ordering::Release);
                    expected += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        producer.join().unwrap();
        consumer_thread.join().unwrap();
    }

    #[test]
    fn two_named_queues_roundtrip_across_processes() {
        const CHILD_MARKER: &str = "DM_PRESENT_IDENTITY_TRANSPORT_CHILD";
        const COUNT: u64 = 1_024;
        const CAPACITY: usize = 8;
        let stream = StreamEpoch {
            identity: WireIdentity::new("s-cross-process", "r0001").unwrap(),
            generation: 71,
            resource_epoch: 3,
        };

        if std::env::var_os(CHILD_MARKER).is_some() {
            let names = TransportNames {
                decision_mapping: std::env::var("DM_PI_DECISION_MAPPING").unwrap(),
                decision_event: std::env::var("DM_PI_DECISION_EVENT").unwrap(),
                ack_mapping: std::env::var("DM_PI_ACK_MAPPING").unwrap(),
                ack_event: std::env::var("DM_PI_ACK_EVENT").unwrap(),
            };
            let mut probe = ProbeTransport::open(&names, &stream).unwrap();
            for expected in 1..=COUNT {
                let decision = loop {
                    if let Some(message) = probe.decisions.try_pop().unwrap() {
                        break message;
                    }
                    std::thread::yield_now();
                };
                assert_eq!(decision.ordinal, expected);
                assert_eq!(
                    decision.payload,
                    Decision::RetireThrough {
                        present_seq: expected * 19
                    }
                );
                probe
                    .acks
                    .try_push(&Envelope {
                        schema: FEEDBACK_SCHEMA_V3,
                        stream: stream.clone(),
                        ordinal: expected,
                        payload: Ack::Retired {
                            decision_ordinal: expected,
                            present_seq: expected * 19,
                        },
                    })
                    .unwrap();
            }
            return;
        }

        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let names = TransportNames::new(
            std::process::id(),
            std::process::id(),
            stream.generation,
            &format!("cross-process-{nonce}"),
        )
        .unwrap();
        let mut client = ClientTransport::create(&names, &stream, CAPACITY).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .creation_flags(0x0800_0000)
            .arg("--exact")
            .arg("transport::tests::two_named_queues_roundtrip_across_processes")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env("DM_PI_DECISION_MAPPING", &names.decision_mapping)
            .env("DM_PI_DECISION_EVENT", &names.decision_event)
            .env("DM_PI_ACK_MAPPING", &names.ack_mapping)
            .env("DM_PI_ACK_EVENT", &names.ack_event);
        let mut child = command.spawn().unwrap();

        for ordinal in 1..=COUNT {
            client
                .decisions
                .try_push(&Envelope {
                    schema: FEEDBACK_SCHEMA_V3,
                    stream: stream.clone(),
                    ordinal,
                    payload: Decision::RetireThrough {
                        present_seq: ordinal * 19,
                    },
                })
                .unwrap();
            let ack = loop {
                if let Some(message) = client.acks.try_pop().unwrap() {
                    break message;
                }
                std::thread::yield_now();
            };
            assert_eq!(ack.ordinal, ordinal);
            assert_eq!(
                ack.payload,
                Ack::Retired {
                    decision_ordinal: ordinal,
                    present_seq: ordinal * 19,
                }
            );
        }
        assert!(child.wait().unwrap().success());
        assert!(client.acks.try_pop().unwrap().is_none());
    }
}
