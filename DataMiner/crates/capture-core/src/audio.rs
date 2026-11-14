//! Process-scoped WASAPI loopback capture primitive.
//!
//! Both capture-lab and the recorder use this module. Profiles opt in explicitly.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, E_UNEXPECTED, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IAgileObject, IAgileObject_Impl,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, Ref, implement, w};

use crate::platform::WindowsVersion;
use crate::qpc::QpcClock;

pub const PROCESS_LOOPBACK_MIN_DOCUMENTED_BUILD: u32 = 20_348;
pub const AUDIO_PACKET_QUEUE_CAPACITY: usize = 200;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const AUDIO_EVENT_POLL_MS: u32 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AudioPcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub block_align: u16,
    pub average_bytes_per_second: u32,
}

impl AudioPcmFormat {
    #[must_use]
    pub const fn pcm_s16le_48k_stereo() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
            block_align: 4,
            average_bytes_per_second: 192_000,
        }
    }

    fn as_wave_format(self) -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: self.channels,
            nSamplesPerSec: self.sample_rate,
            nAvgBytesPerSec: self.average_bytes_per_second,
            nBlockAlign: self.block_align,
            wBitsPerSample: self.bits_per_sample,
            cbSize: 0,
        }
    }
}

#[derive(Debug)]
pub struct AudioPacket {
    pub stream_frame_start: u64,
    pub arrival_qpc: i64,
    pub device_position: u64,
    pub qpc_position_100ns: u64,
    pub frames: u32,
    pub flags: u32,
    pub pcm: Vec<u8>,
}

impl AudioPacket {
    #[must_use]
    pub const fn silent(&self) -> bool {
        self.flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
    }

    #[must_use]
    pub const fn timestamp_error(&self) -> bool {
        self.flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0
    }

    #[must_use]
    pub const fn discontinuity(&self) -> bool {
        self.flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
    }

    pub fn first_frame_qpc(&self, clock: QpcClock) -> anyhow::Result<i64> {
        ensure!(
            self.qpc_position_100ns <= i64::MAX as u64,
            "audio QPC position exceeds i64"
        );
        Ok(clock.relative_100ns_to_qpc(self.qpc_position_100ns as i64))
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioCaptureHealth {
    pub packets: u64,
    pub frames: u64,
    pub bytes: u64,
    pub discontinuities: u64,
    pub silent_packets: u64,
    pub timestamp_errors: u64,
    pub queue_overflows: u64,
    pub nonzero_device_positions: u64,
    pub first_qpc_position_100ns: Option<u64>,
    pub last_qpc_position_100ns: Option<u64>,
    pub first_arrival_qpc: Option<i64>,
    pub last_arrival_qpc: Option<i64>,
    pub mmcss_task: Option<&'static str>,
    pub terminal_error: Option<String>,
}

pub struct ProcessLoopbackCapture {
    packets: Receiver<AudioPacket>,
    stop: Arc<AtomicBool>,
    queue_overflows: Arc<AtomicU64>,
    worker: Option<JoinHandle<AudioCaptureHealth>>,
}

impl ProcessLoopbackCapture {
    pub fn start(target_process_id: u32, clock: QpcClock) -> anyhow::Result<Self> {
        let windows = WindowsVersion::current()?;
        ensure!(
            windows.major >= 10 && windows.build >= PROCESS_LOOPBACK_MIN_DOCUMENTED_BUILD,
            "process loopback requires documented Windows build >= {PROCESS_LOOPBACK_MIN_DOCUMENTED_BUILD}; found {}.{}.{}",
            windows.major,
            windows.minor,
            windows.build,
        );
        ensure!(
            target_process_id != 0,
            "process loopback target PID is zero"
        );

        let (packet_tx, packet_rx) = bounded(AUDIO_PACKET_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_worker = Arc::clone(&stop);
        let queue_overflows = Arc::new(AtomicU64::new(0));
        let queue_overflows_for_worker = Arc::clone(&queue_overflows);
        let worker = std::thread::Builder::new()
            .name("dm-audio-loopback".into())
            .spawn(move || {
                capture_thread(
                    target_process_id,
                    clock,
                    packet_tx,
                    stop_for_worker,
                    queue_overflows_for_worker,
                    ready_tx,
                )
            })
            .context("failed to spawn process-loopback thread")?;

        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                packets: packet_rx,
                stop,
                queue_overflows,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                anyhow::bail!("process-loopback initialization failed: {error}")
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error).context("timed out waiting for process-loopback initialization")
            }
        }
    }

    pub fn drain_packets(&self, destination: &mut Vec<AudioPacket>) {
        destination.extend(self.packets.try_iter());
    }

    #[must_use]
    pub(crate) fn packet_receiver(&self) -> Receiver<AudioPacket> {
        self.packets.clone()
    }

    #[must_use]
    pub(crate) fn queue_overflows(&self) -> u64 {
        self.queue_overflows.load(Ordering::Acquire)
    }

    pub fn finish(
        mut self,
        destination: &mut Vec<AudioPacket>,
    ) -> anyhow::Result<AudioCaptureHealth> {
        self.stop.store(true, Ordering::Release);
        let health = self
            .worker
            .take()
            .expect("audio loopback worker missing")
            .join()
            .map_err(|_| anyhow::anyhow!("audio loopback worker panicked"))?;
        self.drain_packets(destination);
        Ok(health)
    }
}

impl Drop for ProcessLoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn capture_thread(
    target_process_id: u32,
    clock: QpcClock,
    packets: Sender<AudioPacket>,
    stop: Arc<AtomicBool>,
    queue_overflows: Arc<AtomicU64>,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> AudioCaptureHealth {
    let mut health = AudioCaptureHealth::default();
    let mut ready = Some(ready);
    if let Err(error) = capture_thread_inner(
        target_process_id,
        clock,
        &packets,
        &stop,
        &queue_overflows,
        &mut ready,
        &mut health,
    ) {
        let message = format!("{error:#}");
        health.terminal_error = Some(message.clone());
        if let Some(ready) = ready.take() {
            let _ = ready.send(Err(message));
        }
    }
    health
}

fn capture_thread_inner(
    target_process_id: u32,
    clock: QpcClock,
    packets: &Sender<AudioPacket>,
    stop: &AtomicBool,
    queue_overflows: &AtomicU64,
    ready: &mut Option<mpsc::SyncSender<Result<(), String>>>,
    health: &mut AudioCaptureHealth,
) -> anyhow::Result<()> {
    let _com = ComGuard::mta()?;
    let _mmcss = MmcssGuard::pro_audio()?;
    health.mmcss_task = Some("Pro Audio");
    let client = activate_process_loopback(target_process_id)?;
    let format = AudioPcmFormat::pcm_s16le_48k_stereo();
    let wave_format = format.as_wave_format();
    let event = KernelEvent::auto_reset()?;

    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            0,
            0,
            &wave_format,
            None,
        )?;
        client.SetEventHandle(event.handle)?;
    }
    let capture: IAudioCaptureClient =
        unsafe { client.GetService() }.context("IAudioClient::GetService failed")?;
    unsafe { client.Start() }.context("IAudioClient::Start failed")?;
    ready
        .take()
        .expect("audio ready sender disappeared")
        .send(Ok(()))
        .ok();

    let mut stream_frame_start = 0_u64;
    while !stop.load(Ordering::Acquire) {
        let wait = unsafe { WaitForSingleObject(event.handle, AUDIO_EVENT_POLL_MS) };
        if wait == WAIT_TIMEOUT {
            continue;
        }
        ensure!(
            wait == WAIT_OBJECT_0,
            "audio sample event wait failed: {wait:?}"
        );
        drain_available_packets(
            &capture,
            clock,
            format,
            packets,
            queue_overflows,
            &mut stream_frame_start,
            health,
        )?;
    }
    let _ = unsafe { client.Stop() };
    Ok(())
}

fn drain_available_packets(
    capture: &IAudioCaptureClient,
    clock: QpcClock,
    format: AudioPcmFormat,
    packets: &Sender<AudioPacket>,
    queue_overflows: &AtomicU64,
    stream_frame_start: &mut u64,
    health: &mut AudioCaptureHealth,
) -> anyhow::Result<()> {
    loop {
        let next_frames = unsafe { capture.GetNextPacketSize() }
            .context("IAudioCaptureClient::GetNextPacketSize failed")?;
        if next_frames == 0 {
            return Ok(());
        }

        let arrival_qpc = clock.now()?;
        let mut data = std::ptr::null_mut();
        let mut frames = 0_u32;
        let mut flags = 0_u32;
        let mut device_position = 0_u64;
        let mut qpc_position_100ns = 0_u64;
        unsafe {
            capture.GetBuffer(
                &mut data,
                &mut frames,
                &mut flags,
                Some(&mut device_position),
                Some(&mut qpc_position_100ns),
            )?;
        }

        let bytes = usize::try_from(frames)?
            .checked_mul(usize::from(format.block_align))
            .context("audio packet byte count overflowed")?;
        // Copy immediately while the WASAPI buffer is valid. This probe still
        // cannot contain an OS access violation caused by a driver invalidating
        // the pointer mid-copy; docs/56 therefore keeps release wiring blocked.
        let pcm = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
            vec![0_u8; bytes]
        } else {
            ensure!(!data.is_null(), "WASAPI returned a null non-silent buffer");
            unsafe { std::slice::from_raw_parts(data.cast_const(), bytes).to_vec() }
        };
        unsafe { capture.ReleaseBuffer(frames) }
            .context("IAudioCaptureClient::ReleaseBuffer failed")?;

        let packet = AudioPacket {
            stream_frame_start: *stream_frame_start,
            arrival_qpc,
            device_position,
            qpc_position_100ns,
            frames,
            flags,
            pcm,
        };
        *stream_frame_start = stream_frame_start.saturating_add(u64::from(frames));
        update_health(health, &packet);
        if packets.try_send(packet).is_err() {
            health.queue_overflows = health.queue_overflows.saturating_add(1);
            queue_overflows.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn update_health(health: &mut AudioCaptureHealth, packet: &AudioPacket) {
    health.packets = health.packets.saturating_add(1);
    health.frames = health.frames.saturating_add(u64::from(packet.frames));
    health.bytes = health
        .bytes
        .saturating_add(u64::try_from(packet.pcm.len()).unwrap_or(u64::MAX));
    if packet.flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
        health.discontinuities = health.discontinuities.saturating_add(1);
    }
    if packet.silent() {
        health.silent_packets = health.silent_packets.saturating_add(1);
    }
    if packet.timestamp_error() {
        health.timestamp_errors = health.timestamp_errors.saturating_add(1);
    }
    if packet.device_position != 0 {
        health.nonzero_device_positions = health.nonzero_device_positions.saturating_add(1);
    }
    health
        .first_qpc_position_100ns
        .get_or_insert(packet.qpc_position_100ns);
    health.last_qpc_position_100ns = Some(packet.qpc_position_100ns);
    health.first_arrival_qpc.get_or_insert(packet.arrival_qpc);
    health.last_arrival_qpc = Some(packet.arrival_qpc);
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationHandler {
    sender: Mutex<Option<mpsc::SyncSender<windows::core::Result<IAudioClient>>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| {
            let operation = operation.ok()?;
            let mut activation_result = E_UNEXPECTED;
            let mut unknown = None;
            unsafe { operation.GetActivateResult(&mut activation_result, &mut unknown) }?;
            activation_result.ok()?;
            unknown
                .ok_or_else(|| windows::core::Error::from_hresult(E_UNEXPECTED))?
                .cast::<IAudioClient>()
        })();
        if let Some(sender) = self
            .sender
            .lock()
            .expect("activation mutex poisoned")
            .take()
        {
            let _ = sender.send(result);
        }
        Ok(())
    }
}

impl IAgileObject_Impl for ActivationHandler_Impl {}

fn activate_process_loopback(target_process_id: u32) -> anyhow::Result<IAudioClient> {
    let mut activation = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: target_process_id,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let blob = BLOB {
        cbSize: u32::try_from(std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>())?,
        pBlobData: (&mut activation as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast::<u8>(),
    };
    // This VT_BLOB borrows stack storage. The C++ sample treats its PROPVARIANT
    // as a non-owning wrapper and never calls PropVariantClear; the Rust
    // binding's Drop implementation would otherwise try to free pBlobData and
    // corrupt the process heap after activation completes.
    let parameters = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { blob },
            }),
        },
    });
    let (sender, receiver) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
        sender: Mutex::new(Some(sender)),
    }
    .into();
    let _operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*parameters),
            &handler,
        )
    }
    .context("ActivateAudioInterfaceAsync failed")?;
    receiver
        .recv_timeout(READY_TIMEOUT)
        .context("process-loopback activation callback timed out")?
        .context("process-loopback activation result failed")
}

struct ComGuard;

impl ComGuard {
    fn mta() -> anyhow::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("CoInitializeEx(MTA) failed for audio loopback")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct MmcssGuard(HANDLE);

impl MmcssGuard {
    fn pro_audio() -> anyhow::Result<Self> {
        let mut task_index = 0_u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }
            .context("failed to enter MMCSS Pro Audio task")?;
        Ok(Self(handle))
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

struct KernelEvent {
    handle: HANDLE,
}

impl KernelEvent {
    fn auto_reset() -> anyhow::Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, None) }
            .context("failed to create WASAPI sample event")?;
        Ok(Self { handle })
    }
}

impl Drop for KernelEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_probe_format_is_pcm_s16le_48k_stereo() {
        let format = AudioPcmFormat::pcm_s16le_48k_stereo();
        assert_eq!(format.block_align, 4);
        assert_eq!(format.average_bytes_per_second, 192_000);
        let wave = format.as_wave_format();
        let format_tag = wave.wFormatTag;
        let extra_size = wave.cbSize;
        assert_eq!(format_tag, WAVE_FORMAT_PCM as u16);
        assert_eq!(extra_size, 0);
    }

    #[test]
    fn packet_qpc_uses_shared_qpc_frequency_conversion() {
        let packet = AudioPacket {
            stream_frame_start: 0,
            arrival_qpc: 0,
            device_position: 0,
            qpc_position_100ns: 12_345_678,
            frames: 1,
            flags: 0,
            pcm: vec![0; 4],
        };
        let clock = QpcClock::new().unwrap();
        let expected = clock.relative_100ns_to_qpc(12_345_678);
        assert_eq!(packet.first_frame_qpc(clock).unwrap(), expected);
    }
}
