use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde::Serialize;
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, SIZE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMF2DBuffer, IMFActivate, IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult,
    IMFAttributes, IMFByteStream, IMFClockStateSink_Impl, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaSink, IMFMediaType, IMFPresentationClock, IMFSample, IMFSampleGrabberSinkCallback,
    IMFSampleGrabberSinkCallback_Impl, IMFSinkWriter, IMFTransform, MF_LOW_LATENCY,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FIXED_SIZE_SAMPLES, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_GEOMETRIC_APERTURE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_READWRITE_DISABLE_CONVERTERS, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SAMPLEGRABBERSINK_IGNORE_CLOCK, MF_SINK_WRITER_D3D_MANAGER,
    MF_SINK_WRITER_DISABLE_THROTTLING, MF_VERSION, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSampleGrabberSinkActivate, MFCreateSinkWriterFromMediaSink, MFCreateSinkWriterFromURL,
    MFCreateTrackedSample, MFMediaType_Video, MFNominalRange_16_235, MFOffset, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFT_ENUM_HARDWARE_URL_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_TRANSFORM_CLSID_Attribute, MFVideoArea, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_PeakConstrainedVBR,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::IPersist;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PCWSTR, Ref, implement};

use crate::platform::WindowsVersion;
use crate::profile::CaptureProfile;
use crate::qpc::{frame_duration_100ns, frame_pts_100ns};

pub struct MfPlatform;

// Introduced in Windows 11 25H2. This value is from the Microsoft Windows SDK
// 10.0.28000 mfreadwrite.h; windows-rs 0.62 / SDK 26100 do not project it yet.
pub const MF_READWRITE_USE_ONLY_HARDWARE_TRANSFORMS: GUID =
    GUID::from_u128(0xf9074427_bf8b_4f69_bbaf_524969056fb6);

#[implement(IMFAsyncCallback)]
struct SampleReleaseCallback {
    pool_index: u32,
    released: Sender<u32>,
}

impl IMFAsyncCallback_Impl for SampleReleaseCallback_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, _result: windows::core::Ref<'_, IMFAsyncResult>) -> windows::core::Result<()> {
        let _ = self.released.send(self.pool_index);
        Ok(())
    }
}

impl MfPlatform {
    pub fn startup() -> anyhow::Result<Self> {
        WindowsVersion::current()?.require_hardware_only_media_foundation()?;
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .context("Media Foundation startup failed")?;
        Ok(Self)
    }
}

impl Drop for MfPlatform {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

const ENCODED_SAMPLE_QUEUE_CAPACITY: usize = 8;

#[derive(Debug)]
pub struct EncodedH264Sample {
    pub data: Vec<u8>,
    pub pts_100ns: i64,
    pub duration_100ns: i64,
    pub sample_flags: u32,
    pub clean_point: bool,
}

#[implement(IMFSampleGrabberSinkCallback)]
struct EncodedSampleCallback {
    samples: Sender<EncodedH264Sample>,
    failures: Arc<AtomicU64>,
}

impl IMFClockStateSink_Impl for EncodedSampleCallback_Impl {
    fn OnClockStart(
        &self,
        _system_time: i64,
        _clock_start_offset: i64,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnClockStop(&self, _system_time: i64) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnClockPause(&self, _system_time: i64) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnClockRestart(&self, _system_time: i64) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnClockSetRate(&self, _system_time: i64, _rate: f32) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IMFSampleGrabberSinkCallback_Impl for EncodedSampleCallback_Impl {
    fn OnSetPresentationClock(
        &self,
        _presentation_clock: Ref<'_, IMFPresentationClock>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnProcessSample(
        &self,
        major_type: *const GUID,
        sample_flags: u32,
        sample_time: i64,
        sample_duration: i64,
        sample_buffer: *const u8,
        sample_size: u32,
    ) -> windows::core::Result<()> {
        if major_type.is_null()
            || sample_buffer.is_null()
            || unsafe { *major_type } != MFMediaType_Video
        {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(E_FAIL.into());
        }
        let data =
            unsafe { std::slice::from_raw_parts(sample_buffer, sample_size as usize).to_vec() };
        let clean_point = h264_contains_idr(&data);
        let sample = EncodedH264Sample {
            data,
            pts_100ns: sample_time,
            duration_100ns: sample_duration,
            sample_flags,
            clean_point,
        };
        if self.samples.try_send(sample).is_err() {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(E_FAIL.into());
        }
        Ok(())
    }

    fn OnShutdown(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

pub struct HardwareH264Encoder {
    writer: Option<IMFSinkWriter>,
    stream_index: u32,
    codec: Option<ICodecAPI>,
    pending_slots: VecDeque<u32>,
    release_tx: Sender<u32>,
    release_rx: Receiver<u32>,
    fps_num: u32,
    fps_den: u32,
    frames_written: u64,
    finished: bool,
    effective_encoder: EffectiveEncoderConfig,
    encoded_rx: Receiver<EncodedH264Sample>,
    callback_failures: Arc<AtomicU64>,
    _callback: IMFSampleGrabberSinkCallback,
    sink_activate: IMFActivate,
    media_sink: IMFMediaSink,
    _device_manager: IMFDXGIDeviceManager,
}

#[derive(Clone, Debug, Serialize)]
pub struct EffectiveEncoderConfig {
    pub transform_clsid: Option<String>,
    pub friendly_name: Option<String>,
    pub hardware_url: Option<String>,
    pub control_policy: &'static str,
    pub relaxed_controls: Vec<String>,
    pub rate_control_mode: u32,
    pub mean_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub gop_frames: u32,
    pub b_frames: u32,
    pub low_latency: bool,
}

pub struct FinishedHardwareEncoder {
    pub released_slots: Vec<u32>,
    /// Pool slots Media Foundation still held after `Finalize` and the bounded
    /// drain below. Reported separately from `released_slots` so the caller can
    /// assert on a real leak instead of on a count the encoder always satisfies.
    pub retained_slots: Vec<u32>,
    pub encoded_samples: Vec<EncodedH264Sample>,
    pub effective_encoder: EffectiveEncoderConfig,
}

/// `IMFTrackedSample` release callbacks are dispatched asynchronously and some
/// NVIDIA drivers can take noticeably longer on a cold encoder shutdown. Keep
/// the wait bounded, but do not mistake ordinary callback latency for a leaked
/// surface during the one-frame hardware preflight.
const SAMPLE_RELEASE_DRAIN: Duration = Duration::from_secs(2);
const SAMPLE_RELEASE_POLL: Duration = Duration::from_millis(1);

impl HardwareH264Encoder {
    pub fn create(d3d_device: &ID3D11Device, profile: &CaptureProfile) -> anyhow::Result<Self> {
        profile.validate()?;

        let mut reset_token = 0;
        let mut device_manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager) }
            .context("MFCreateDXGIDeviceManager failed")?;
        let device_manager = device_manager.context("Media Foundation returned no DXGI manager")?;
        unsafe { device_manager.ResetDevice(d3d_device, reset_token) }
            .context("failed to bind NVIDIA D3D11 device to Media Foundation")?;

        let attrs = create_attributes(6)?;
        unsafe {
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &device_manager)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_READWRITE_USE_ONLY_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
        }

        let output_type = video_type(
            MFVideoFormat_H264,
            profile,
            Some(profile.bitrate_bps),
            Some(eAVEncH264VProfile_High.0 as u32),
        )?;
        let (encoded_tx, encoded_rx) = bounded(ENCODED_SAMPLE_QUEUE_CAPACITY);
        let callback_failures = Arc::new(AtomicU64::new(0));
        let callback: IMFSampleGrabberSinkCallback = EncodedSampleCallback {
            samples: encoded_tx,
            failures: Arc::clone(&callback_failures),
        }
        .into();
        let sink_activate = unsafe { MFCreateSampleGrabberSinkActivate(&output_type, &callback) }
            .context("failed to create encoded H.264 sample-grabber sink")?;
        unsafe {
            sink_activate.SetUINT32(&MF_SAMPLEGRABBERSINK_IGNORE_CLOCK, 1)?;
        }
        let media_sink: IMFMediaSink = unsafe { sink_activate.ActivateObject() }
            .context("failed to activate encoded H.264 sample-grabber sink")?;
        let writer = unsafe { MFCreateSinkWriterFromMediaSink(&media_sink, &attrs) }
            .context("failed to create persistent hardware-only H.264 sink writer")?;
        // The sample-grabber media sink owns one fixed stream with identifier 0.
        // AddStream would fail with MF_E_STREAMSINKS_FIXED.
        let stream_index = 0;
        let input_type = video_type(MFVideoFormat_NV12, profile, None, None)?;
        // Some hardware MFTs freeze GOP/rate control while negotiating media types. Pass the
        // documented encoder parameter store during negotiation as well as verifying ICodecAPI
        // afterward. Late SetValue success alone did not mean NVIDIA accepted these preferences.
        // https://learn.microsoft.com/windows/win32/api/mfreadwrite/nf-mfreadwrite-imfsinkwriter-setinputmediatype
        let encoding_parameters = create_attributes(5)?;
        unsafe {
            encoding_parameters.SetUINT32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
            )?;
            encoding_parameters.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, profile.bitrate_bps)?;
            encoding_parameters
                .SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, profile.max_bitrate_bps)?;
            encoding_parameters.SetUINT32(&CODECAPI_AVEncMPVGOPSize, profile.gop_frames)?;
            encoding_parameters.SetUINT32(&CODECAPI_AVLowLatencyMode, 1)?;
        }
        let mut relaxed_controls = Vec::new();
        if let Err(error) =
            unsafe { writer.SetInputMediaType(stream_index, &input_type, &encoding_parameters) }
        {
            relaxed_controls.push(format!("initial encoder preferences rejected; hardware-only negotiation without preferences: {error}"));
            unsafe { writer.SetInputMediaType(stream_index, &input_type, None::<&IMFAttributes>) }
                .context("hardware encoder rejected GPU NV12 input type")?;
        }

        let codec = sink_writer_codec_api(&writer, stream_index)
            .context("sink writer did not expose the selected hardware encoder")?;
        let rate_control_mode = configure_u32(
            &codec,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
            "peak-constrained VBR",
            &mut relaxed_controls,
        );
        let mean_bitrate_bps = configure_u32(
            &codec,
            &CODECAPI_AVEncCommonMeanBitRate,
            profile.bitrate_bps,
            "mean bitrate",
            &mut relaxed_controls,
        );
        let max_bitrate_bps = configure_u32(
            &codec,
            &CODECAPI_AVEncCommonMaxBitRate,
            profile.max_bitrate_bps,
            "maximum bitrate",
            &mut relaxed_controls,
        );
        let gop_frames = configure_u32(
            &codec,
            &CODECAPI_AVEncMPVGOPSize,
            profile.gop_frames,
            "GOP size",
            &mut relaxed_controls,
        );
        let b_frames = configure_u32(
            &codec,
            &CODECAPI_AVEncMPVDefaultBPictureCount,
            0,
            "zero B-frame count",
            &mut relaxed_controls,
        );
        let low_latency = configure_bool(
            &codec,
            &CODECAPI_AVLowLatencyMode,
            true,
            "low-latency mode",
            &mut relaxed_controls,
        );
        let identity = encoder_identity(&codec);
        let effective_encoder = EffectiveEncoderConfig {
            transform_clsid: identity.transform_clsid,
            friendly_name: identity.friendly_name,
            hardware_url: identity.hardware_url,
            control_policy: "hardware_capability_validated_best_effort_controls",
            relaxed_controls,
            rate_control_mode,
            mean_bitrate_bps,
            max_bitrate_bps,
            gop_frames,
            b_frames,
            low_latency,
        };
        unsafe { writer.BeginWriting() }
            .context("hardware-only H.264 encoder failed to begin writing")?;

        let (release_tx, release_rx) = unbounded();
        Ok(Self {
            writer: Some(writer),
            stream_index,
            codec: Some(codec),
            pending_slots: VecDeque::new(),
            release_tx,
            release_rx,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
            frames_written: 0,
            finished: false,
            effective_encoder,
            encoded_rx,
            callback_failures,
            _callback: callback,
            sink_activate,
            media_sink,
            _device_manager: device_manager,
        })
    }

    pub fn write_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        pool_index: u32,
        session_frame: u64,
        force_keyframe: bool,
    ) -> anyhow::Result<()> {
        ensure!(
            !self.finished,
            "cannot write to a finalized hardware encoder"
        );
        ensure!(
            session_frame == self.frames_written,
            "hardware encoder input timeline is discontinuous: expected frame {}, received {session_frame}",
            self.frames_written
        );
        ensure!(
            !self.pending_slots.iter().any(|slot| *slot == pool_index),
            "NV12 pool slot {pool_index} was reused before Media Foundation released it"
        );

        if force_keyframe {
            let codec = self
                .codec
                .as_ref()
                .context("hardware encoder codec service was already released")?;
            let result = try_set_u32(
                codec,
                &CODECAPI_AVEncVideoForceKeyFrame,
                1,
                "segment-boundary IDR",
            );
            if let Err(error) = result {
                // Some NVIDIA Media Foundation drivers reject or fail to
                // advertise ForceKeyFrame even though the encoder naturally
                // emits an IDR at stream/GOP boundaries. Treat the control as
                // a hint: the preflight MP4 probe and every segment muxer still
                // reject a non-IDR first sample, which is the actual minimum
                // capability needed for independently decodable segments.
                let relaxation = format!(
                    "segment-boundary IDR control was rejected; output IDR validation retained: {error:#}"
                );
                if !self
                    .effective_encoder
                    .relaxed_controls
                    .iter()
                    .any(|existing| existing.starts_with("segment-boundary IDR control"))
                {
                    self.effective_encoder
                        .relaxed_controls
                        .push(relaxation.clone());
                }
                tracing::warn!(error = %error, "hardware encoder rejected IDR hint; encoded output remains gated");
            }
        }

        let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false) }
            .context("MFCreateDXGISurfaceBuffer rejected NV12 GPU texture")?;
        let buffer_2d: IMF2DBuffer = buffer
            .cast()
            .context("DXGI surface buffer does not expose IMF2DBuffer")?;
        let contiguous_length = unsafe { buffer_2d.GetContiguousLength() }
            .context("failed to get NV12 surface buffer length")?;
        unsafe { buffer.SetCurrentLength(contiguous_length) }
            .context("failed to set NV12 surface buffer length")?;
        let tracked = unsafe { MFCreateTrackedSample() }.context("MFCreateTrackedSample failed")?;
        let sample: IMFSample = tracked
            .cast()
            .context("tracked sample does not expose IMFSample")?;
        let callback: IMFAsyncCallback = SampleReleaseCallback {
            pool_index,
            released: self.release_tx.clone(),
        }
        .into();
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(frame_pts_100ns(session_frame, self.fps_num, self.fps_den))?;
            sample.SetSampleDuration(frame_duration_100ns(
                session_frame,
                self.fps_num,
                self.fps_den,
            ))?;
            tracked.SetAllocator(&callback, None::<&windows::core::IUnknown>)?;
        }
        self.pending_slots.push_back(pool_index);
        let writer = self
            .writer
            .as_ref()
            .context("hardware encoder sink writer was already released")?;
        unsafe { writer.WriteSample(self.stream_index, &sample) }
            .context("hardware H.264 WriteSample failed")?;
        self.frames_written += 1;
        Ok(())
    }

    pub fn take_released(&mut self) -> anyhow::Result<Vec<u32>> {
        let mut indices = Vec::new();
        while let Ok(index) = self.release_rx.try_recv() {
            let position = self
                .pending_slots
                .iter()
                .position(|pending| *pending == index)
                .with_context(|| {
                    format!("Media Foundation released unknown NV12 pool slot {index}")
                })?;
            self.pending_slots.remove(position);
            indices.push(index);
        }
        Ok(indices)
    }

    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.pending_slots.len()
    }

    pub fn take_encoded(&self) -> anyhow::Result<Vec<EncodedH264Sample>> {
        let failures = self.callback_failures.load(Ordering::Relaxed);
        ensure!(
            failures == 0,
            "encoded H.264 callback dropped or rejected {failures} sample(s); recording stopped"
        );
        Ok(self.encoded_rx.try_iter().collect())
    }

    #[must_use]
    pub fn effective_encoder(&self) -> &EffectiveEncoderConfig {
        &self.effective_encoder
    }

    pub fn finish(mut self) -> anyhow::Result<FinishedHardwareEncoder> {
        let writer = self
            .writer
            .as_ref()
            .context("hardware encoder sink writer was already released")?;
        unsafe { writer.Finalize() }.context("persistent H.264 encoder finalize failed")?;
        self.finished = true;
        let mut encoded_samples = self.take_encoded()?;

        // Finalize completes the encoded output, but the sink writer/encoder can
        // still own the final tracked input sample. Flush any remaining encoder
        // references and shut down the activated sink before deciding that an
        // NV12 surface leaked. The release callback remains alive through this
        // bounded drain and is the only source of truth for pool ownership.
        if let Err(error) = unsafe { writer.Flush(self.stream_index) } {
            tracing::warn!(
                error = %error,
                "hardware encoder rejected post-finalize flush; shutdown release remains required"
            );
        }
        unsafe {
            self.media_sink.Shutdown()?;
            self.sink_activate.ShutdownObject()?;
        }
        // `GetServiceForStream` returns another COM reference to the encoder.
        // Release both it and the sink writer itself before waiting for the
        // tracked input sample's final reference to disappear.
        drop(self.codec.take());
        drop(self.writer.take());

        let mut released = self.take_released()?;
        let deadline = Instant::now() + SAMPLE_RELEASE_DRAIN;
        while !self.pending_slots.is_empty() && Instant::now() < deadline {
            std::thread::sleep(SAMPLE_RELEASE_POLL);
            released.extend(self.take_released()?);
        }
        // Close the race where a callback arrives immediately after the final
        // deadline check but before retained slots are classified.
        released.extend(self.take_released()?);
        let retained_slots = self.pending_slots.drain(..).collect::<Vec<_>>();
        encoded_samples.extend(self.take_encoded()?);
        Ok(FinishedHardwareEncoder {
            released_slots: released,
            retained_slots,
            encoded_samples,
            effective_encoder: self.effective_encoder.clone(),
        })
    }

    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }
}

impl Drop for HardwareH264Encoder {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(writer) = self.writer.as_ref() {
                let _ = unsafe { writer.Finalize() };
            }
            let _ = unsafe { self.media_sink.Shutdown() };
            let _ = unsafe { self.sink_activate.ShutdownObject() };
        }
    }
}

pub struct H264Mp4Writer {
    writer: IMFSinkWriter,
    stream_index: u32,
    fps_num: u32,
    fps_den: u32,
    frames_written: u64,
    finished: bool,
}

impl H264Mp4Writer {
    pub fn create(output: impl AsRef<Path>, profile: &CaptureProfile) -> anyhow::Result<Self> {
        profile.validate()?;
        let output = output.as_ref();
        let parent = output.parent().context("MP4 output path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let attrs = create_attributes(2)?;
        unsafe {
            attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        }
        let wide = media_foundation_wide_path(output)?;
        let writer = unsafe {
            MFCreateSinkWriterFromURL(
                PCWSTR::from_raw(wide.as_ptr()),
                None::<&IMFByteStream>,
                &attrs,
            )
        }
        .with_context(|| format!("failed to create H.264 MP4 muxer for {}", output.display()))?;
        let h264_type = video_type(
            MFVideoFormat_H264,
            profile,
            Some(profile.bitrate_bps),
            Some(eAVEncH264VProfile_High.0 as u32),
        )?;
        let stream_index = unsafe { writer.AddStream(&h264_type) }
            .context("MP4 muxer rejected H.264 output type")?;
        unsafe { writer.SetInputMediaType(stream_index, &h264_type, None::<&IMFAttributes>) }
            .context("MP4 muxer rejected encoded H.264 input type")?;
        unsafe { writer.BeginWriting() }.context("H.264 MP4 muxer failed to begin writing")?;
        Ok(Self {
            writer,
            stream_index,
            fps_num: profile.fps_num,
            fps_den: profile.fps_den,
            frames_written: 0,
            finished: false,
        })
    }

    pub fn write_sample(&mut self, encoded: &EncodedH264Sample) -> anyhow::Result<()> {
        ensure!(!self.finished, "cannot write to a finalized MP4 muxer");
        ensure!(
            !encoded.data.is_empty(),
            "hardware encoder emitted an empty H.264 sample"
        );
        ensure!(
            h264_is_annex_b(&encoded.data),
            "hardware encoder emitted non-Annex-B H.264; MP4 muxing stopped"
        );
        if self.frames_written == 0 {
            ensure!(
                encoded.clean_point,
                "MP4 segment did not start with an IDR sample; recording stopped"
            );
        }
        let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(encoded.data.len() as u32) }
            .context("failed to allocate compressed H.264 media buffer")?;
        let mut destination = std::ptr::null_mut();
        unsafe { buffer.Lock(&mut destination, None, None) }
            .context("failed to lock compressed H.264 media buffer")?;
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.data.as_ptr(), destination, encoded.data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(encoded.data.len() as u32)?;
        }
        let sample =
            unsafe { MFCreateSample() }.context("failed to create compressed H.264 sample")?;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(frame_pts_100ns(
                self.frames_written,
                self.fps_num,
                self.fps_den,
            ))?;
            sample.SetSampleDuration(frame_duration_100ns(
                self.frames_written,
                self.fps_num,
                self.fps_den,
            ))?;
            sample.SetSampleFlags(encoded.sample_flags)?;
            if encoded.clean_point {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.writer.WriteSample(self.stream_index, &sample)?;
        }
        self.frames_written += 1;
        Ok(())
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        unsafe { self.writer.Finalize() }.context("MP4 muxer finalize failed")?;
        self.finished = true;
        Ok(())
    }

    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }
}

impl Drop for H264Mp4Writer {
    fn drop(&mut self) {
        if !self.finished {
            let _ = unsafe { self.writer.Finalize() };
        }
    }
}

fn h264_is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

fn h264_contains_idr(data: &[u8]) -> bool {
    let mut offset = 0;
    while offset + 3 < data.len() {
        let start_code = if data[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        let nal = offset + start_code;
        if nal < data.len() && data[nal] & 0x1f == 5 {
            return true;
        }
        offset = nal.saturating_add(1);
    }
    false
}

fn create_attributes(capacity: u32) -> anyhow::Result<IMFAttributes> {
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, capacity) }.context("MFCreateAttributes failed")?;
    attrs.context("Media Foundation returned no attributes")
}

fn video_type(
    subtype: GUID,
    profile: &CaptureProfile,
    bitrate: Option<u32>,
    h264_profile: Option<u32>,
) -> anyhow::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType failed")?;
    let display_area = MFVideoArea {
        OffsetX: MFOffset { fract: 0, value: 0 },
        OffsetY: MFOffset { fract: 0, value: 0 },
        Area: SIZE {
            cx: i32::try_from(profile.width).context("profile width exceeds i32")?,
            cy: i32::try_from(profile.height).context("profile height exceeds i32")?,
        },
    };
    let display_area_bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(&display_area).cast::<u8>(),
            std::mem::size_of::<MFVideoArea>(),
        )
    };
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_pair(profile.width, profile.height))?;
        media_type.SetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, display_area_bytes)?;
        media_type.SetBlob(&MF_MT_GEOMETRIC_APERTURE, display_area_bytes)?;
        media_type.SetUINT64(
            &MF_MT_FRAME_RATE,
            pack_pair(profile.fps_num, profile.fps_den),
        )?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        if subtype == MFVideoFormat_NV12 {
            media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            media_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
            media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, profile.width)?;
            media_type.SetUINT32(&MF_MT_SAMPLE_SIZE, profile.width * profile.height * 3 / 2)?;
        }
        if subtype == MFVideoFormat_H264 {
            media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 0)?;
        }
        if let Some(bitrate) = bitrate {
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        }
        if let Some(profile) = h264_profile {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
        }
    }
    Ok(media_type)
}

fn sink_writer_codec_api(writer: &IMFSinkWriter, stream_index: u32) -> anyhow::Result<ICodecAPI> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    unsafe {
        writer.GetServiceForStream(stream_index, &GUID::zeroed(), &ICodecAPI::IID, &mut raw)?;
        ensure!(
            !raw.is_null(),
            "GetServiceForStream returned a null ICodecAPI"
        );
        Ok(ICodecAPI::from_raw(raw))
    }
}

fn try_set_u32(codec: &ICodecAPI, key: &GUID, value: u32, label: &str) -> anyhow::Result<()> {
    let value = VARIANT::from(value);
    unsafe {
        codec
            .SetValue(key, &value)
            .with_context(|| format!("hardware encoder rejected {label}"))
    }
}

fn configure_u32(
    codec: &ICodecAPI,
    key: &GUID,
    requested: u32,
    label: &str,
    relaxed_controls: &mut Vec<String>,
) -> u32 {
    // Driver capability reporting is inconsistent across NVIDIA generations.
    // Treat controls as preferences rather than capabilities: attempt SetValue
    // even when IsSupported lies, then prefer the value reported by the
    // encoder. If neither can be verified, retain the request as an explicitly
    // unverified fact. Actual H.264 samples, timestamps and segment-leading
    // IDRs remain the hard capability gates.
    let value = VARIANT::from(requested);
    let set_error = unsafe { codec.SetValue(key, &value) }.err();
    let effective = unsafe { codec.GetValue(key) }
        .ok()
        .and_then(|value| u32::try_from(&value).ok());

    match (set_error, effective) {
        (None, Some(effective)) if effective == requested => effective,
        (None, Some(effective)) => {
            relaxed_controls.push(format!(
                "{label} normalized by driver: requested {requested}, effective {effective}"
            ));
            effective
        }
        (None, None) => {
            relaxed_controls.push(format!(
                "{label} accepted but effective value was unavailable; requested {requested} retained"
            ));
            requested
        }
        (Some(error), Some(effective)) => {
            relaxed_controls.push(format!(
                "{label} SetValue rejected; driver default {effective} accepted: {error}"
            ));
            effective
        }
        (Some(error), None) => {
            relaxed_controls.push(format!(
                "{label} unavailable; encoder default accepted and requested value {requested} recorded as unverified: {error}"
            ));
            requested
        }
    }
}

fn configure_bool(
    codec: &ICodecAPI,
    key: &GUID,
    requested: bool,
    label: &str,
    relaxed_controls: &mut Vec<String>,
) -> bool {
    let value = VARIANT::from(requested);
    let set_error = unsafe { codec.SetValue(key, &value) }.err();
    let effective = unsafe { codec.GetValue(key) }
        .ok()
        .and_then(|value| bool::try_from(&value).ok());

    match (set_error, effective) {
        (None, Some(effective)) if effective == requested => effective,
        (None, Some(effective)) => {
            relaxed_controls.push(format!(
                "{label} normalized by driver: requested {requested}, effective {effective}"
            ));
            effective
        }
        (None, None) => {
            relaxed_controls.push(format!(
                "{label} accepted but effective value was unavailable; requested {requested} retained"
            ));
            requested
        }
        (Some(error), Some(effective)) => {
            relaxed_controls.push(format!(
                "{label} SetValue rejected; driver default {effective} accepted: {error}"
            ));
            effective
        }
        (Some(error), None) => {
            relaxed_controls.push(format!(
                "{label} unavailable; encoder default accepted and requested value {requested} recorded as unverified: {error}"
            ));
            requested
        }
    }
}

struct EncoderIdentity {
    transform_clsid: Option<String>,
    friendly_name: Option<String>,
    hardware_url: Option<String>,
}

fn encoder_identity(codec: &ICodecAPI) -> EncoderIdentity {
    if let Ok(persist) = codec.cast::<IPersist>()
        && let Ok(clsid) = unsafe { persist.GetClassID() }
    {
        return EncoderIdentity {
            transform_clsid: Some(format!("{clsid:?}")),
            friendly_name: None,
            hardware_url: None,
        };
    }
    let attributes = codec
        .cast::<IMFTransform>()
        .ok()
        .and_then(|transform| unsafe { transform.GetAttributes() }.ok());
    let transform_clsid = attributes
        .as_ref()
        .and_then(|attrs| unsafe { attrs.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok())
        .map(|clsid| format!("{clsid:?}"));
    let friendly_name = attributes
        .as_ref()
        .and_then(|attrs| optional_attribute_string(attrs, &MFT_FRIENDLY_NAME_Attribute));
    let hardware_url = attributes
        .as_ref()
        .and_then(|attrs| optional_attribute_string(attrs, &MFT_ENUM_HARDWARE_URL_Attribute));
    EncoderIdentity {
        transform_clsid,
        friendly_name,
        hardware_url,
    }
}

fn optional_attribute_string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    let length = unsafe { attributes.GetStringLength(key) }.ok()?;
    let mut buffer = vec![0_u16; length as usize + 1];
    unsafe { attributes.GetString(key, &mut buffer, None) }.ok()?;
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

const fn pack_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

pub(crate) fn media_foundation_wide_path(path: &Path) -> anyhow::Result<Vec<u16>> {
    let path = media_foundation_output_path(path)?;
    Ok(path.encode_utf16().chain(std::iter::once(0)).collect())
}

fn media_foundation_output_path(path: &Path) -> anyhow::Result<String> {
    let path = path
        .to_str()
        .context("MP4 output path is not valid Unicode")?;
    let Some(local_path) = path.strip_prefix(r"\\?\") else {
        return Ok(path.to_owned());
    };

    let bytes = local_path.as_bytes();
    ensure!(
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\',
        "Media Foundation MP4 output must use a local disk path, not a verbatim UNC or device path"
    );
    ensure!(
        local_path[3..]
            .split('\\')
            .all(|component| !component.is_empty()
                && component != "."
                && component != ".."
                && !component.ends_with(['.', ' '])),
        "Media Foundation MP4 output cannot safely remove the verbatim prefix from this path"
    );

    // Capture storage is canonicalized to a fixed local disk before this point. Windows returns
    // that path with the `\\?\` prefix, but MFCreateSinkWriterFromURL treats the same prefix as
    // a network URL and fails with ERROR_BAD_NETPATH. Passing the equivalent DOS path keeps the
    // sink local while retaining fail-closed rejection for UNC and device namespaces.
    Ok(local_path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::media_foundation_output_path;
    use std::path::Path;

    #[test]
    fn media_foundation_path_keeps_regular_local_disk_path() {
        let path = Path::new(r"D:\DiogenesDataMiner\.preflight-scratch\probe.mp4");

        assert_eq!(
            media_foundation_output_path(path).expect("regular local path should be accepted"),
            path.to_string_lossy()
        );
    }

    #[test]
    fn media_foundation_path_removes_verbatim_local_disk_prefix() {
        let path = Path::new(r"\\?\D:\DiogenesDataMiner\.preflight-scratch\probe.mp4");

        assert_eq!(
            media_foundation_output_path(path).expect("verbatim local path should be accepted"),
            r"D:\DiogenesDataMiner\.preflight-scratch\probe.mp4"
        );
    }

    #[test]
    fn media_foundation_path_rejects_verbatim_unc_path() {
        let path = Path::new(r"\\?\UNC\server\share\probe.mp4");

        assert!(media_foundation_output_path(path).is_err());
    }

    #[test]
    fn media_foundation_path_rejects_lossy_verbatim_component() {
        let path = Path::new(r"\\?\D:\capture.\probe.mp4");

        assert!(media_foundation_output_path(path).is_err());
    }
}
