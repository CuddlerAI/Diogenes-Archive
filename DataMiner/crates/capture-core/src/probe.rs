use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, ensure};
use mp4::{MediaType, Mp4Reader, TrackType};
use serde::Serialize;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, MF_E_ATTRIBUTENOTFOUND, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_GEOMETRIC_APERTURE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG2_PROFILE,
    MF_MT_PAN_SCAN_ENABLED, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MFCreateSourceReaderFromURL, MFNominalRange_16_235,
    MFSampleExtension_CleanPoint, MFVideoArea, MFVideoFormat_H264, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, eAVEncH264VProfile_High,
};
use windows::core::PCWSTR;

use crate::mf::media_foundation_wide_path;
use crate::profile::CaptureProfile;

#[derive(Clone, Debug, Serialize)]
pub struct Mp4Probe {
    pub codec: String,
    pub h264_profile: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub video_primaries: u32,
    pub transfer_function: u32,
    pub yuv_matrix: u32,
    pub nominal_range: u32,
    pub sample_count: u64,
    pub first_pts_100ns: i64,
    pub last_pts_100ns: i64,
    pub first_sample_clean_point: bool,
    pub pts_monotonic: bool,
}

pub fn probe_h264_mp4(
    path: impl AsRef<Path>,
    expected_frames: u64,
    profile: &CaptureProfile,
) -> anyhow::Result<Mp4Probe> {
    let path = path.as_ref();
    let wide = media_foundation_wide_path(path)?;
    let reader = unsafe {
        MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide.as_ptr()), None::<&IMFAttributes>)
    }
    .with_context(|| format!("Media Foundation could not open {}", path.display()))?;

    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let media_type = unsafe { reader.GetNativeMediaType(stream, 0) }
        .context("Media Foundation did not expose the native MP4 video type")?;
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
        .context("MP4 video type has no codec subtype")?;
    ensure!(
        subtype == MFVideoFormat_H264,
        "MP4 video codec is not H.264"
    );
    let h264_profile = unsafe { media_type.GetUINT32(&MF_MT_MPEG2_PROFILE) }
        .context("MP4 video type has no H.264 profile")?;
    ensure!(
        h264_profile == eAVEncH264VProfile_High.0 as u32,
        "MP4 H.264 profile is not High"
    );
    let (coded_width, coded_height) = unpack_pair(
        unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
            .context("MP4 video type has no frame size")?,
    );
    let (container_width, container_height) = mp4_video_dimensions(path)?;
    let display_area = display_area(&media_type)?;
    let (width, height) = validate_video_geometry(
        coded_width,
        coded_height,
        container_width,
        container_height,
        display_area,
        profile.width,
        profile.height,
    )?;
    let (fps_num, fps_den) = unpack_pair(
        unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }
            .context("MP4 video type has no frame rate")?,
    );
    ensure!(
        u64::from(fps_num) * u64::from(profile.fps_den)
            == u64::from(profile.fps_num) * u64::from(fps_den),
        "MP4 frame rate {fps_num}/{fps_den} does not match profile {}/{}",
        profile.fps_num,
        profile.fps_den
    );
    let video_primaries = required_u32(&media_type, &MF_MT_VIDEO_PRIMARIES, "video primaries")?;
    let transfer_function =
        required_u32(&media_type, &MF_MT_TRANSFER_FUNCTION, "transfer function")?;
    let yuv_matrix = required_u32(&media_type, &MF_MT_YUV_MATRIX, "YUV matrix")?;
    let nominal_range = required_u32(&media_type, &MF_MT_VIDEO_NOMINAL_RANGE, "nominal range")?;
    ensure!(
        video_primaries == MFVideoPrimaries_BT709.0 as u32
            && transfer_function == MFVideoTransFunc_709.0 as u32
            && yuv_matrix == MFVideoTransferMatrix_BT709.0 as u32
            && nominal_range == MFNominalRange_16_235.0 as u32,
        "MP4 SDR color metadata does not match BT.709 limited-range contract"
    );
    let mut sample_count = 0_u64;
    let mut first_pts = None;
    let mut last_pts = None;
    let mut clean_point = false;
    let mut monotonic = true;
    loop {
        let mut flags = 0_u32;
        let mut timestamp = 0_i64;
        let mut sample = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .context("Media Foundation MP4 sample probe failed")?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        let Some(sample) = sample else {
            continue;
        };
        let pts = unsafe { sample.GetSampleTime() }.unwrap_or(timestamp);
        if let Some(previous) = last_pts {
            monotonic &= pts > previous;
        }
        if sample_count == 0 {
            first_pts = Some(pts);
            clean_point = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
                .map(|value| value != 0)
                .unwrap_or(false);
        }
        last_pts = Some(pts);
        sample_count += 1;
    }

    let result = Mp4Probe {
        codec: "H.264".into(),
        h264_profile,
        coded_width,
        coded_height,
        width,
        height,
        fps_num,
        fps_den,
        video_primaries,
        transfer_function,
        yuv_matrix,
        nominal_range,
        sample_count,
        first_pts_100ns: first_pts.unwrap_or_default(),
        last_pts_100ns: last_pts.unwrap_or_default(),
        first_sample_clean_point: clean_point,
        pts_monotonic: monotonic,
    };
    ensure!(
        result.sample_count == expected_frames,
        "MP4 sample count {} does not match expected {expected_frames}",
        result.sample_count
    );
    ensure!(
        result.first_pts_100ns == 0,
        "segment PTS does not start at zero"
    );
    ensure!(
        result.first_sample_clean_point,
        "segment first sample is not an IDR/clean point"
    );
    ensure!(
        result.pts_monotonic,
        "segment PTS is not strictly monotonic"
    );
    Ok(result)
}

fn required_u32(
    media_type: &IMFMediaType,
    key: &windows::core::GUID,
    label: &str,
) -> anyhow::Result<u32> {
    unsafe { media_type.GetUINT32(key) }.with_context(|| format!("MP4 video type has no {label}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisplayArea {
    x: i16,
    y: i16,
    width: u32,
    height: u32,
}

fn display_area(media_type: &IMFMediaType) -> anyhow::Result<Option<DisplayArea>> {
    let pan_scan_enabled =
        unsafe { media_type.GetUINT32(&MF_MT_PAN_SCAN_ENABLED) }.unwrap_or_default() != 0;
    ensure!(
        !pan_scan_enabled,
        "MP4 unexpectedly enables pan-and-scan display mode"
    );

    if let Some(area) = optional_video_area(
        media_type,
        &MF_MT_MINIMUM_DISPLAY_APERTURE,
        "display aperture",
    )?
    .or(optional_video_area(
        media_type,
        &MF_MT_GEOMETRIC_APERTURE,
        "geometric aperture",
    )?) {
        ensure!(
            area.OffsetX.fract == 0 && area.OffsetY.fract == 0,
            "MP4 display aperture uses fractional offsets"
        );
        return Ok(Some(DisplayArea {
            x: area.OffsetX.value,
            y: area.OffsetY.value,
            width: u32::try_from(area.Area.cx).context("MP4 display aperture width is negative")?,
            height: u32::try_from(area.Area.cy)
                .context("MP4 display aperture height is negative")?,
        }));
    }

    Ok(None)
}

fn optional_video_area(
    media_type: &IMFMediaType,
    key: &windows::core::GUID,
    label: &str,
) -> anyhow::Result<Option<MFVideoArea>> {
    let size = match unsafe { media_type.GetBlobSize(key) } {
        Ok(size) => size,
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read MP4 {label}")),
    };
    ensure!(
        size as usize == std::mem::size_of::<MFVideoArea>(),
        "MP4 {label} has invalid size {size}"
    );
    let mut area = std::mem::MaybeUninit::<MFVideoArea>::uninit();
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            area.as_mut_ptr().cast::<u8>(),
            std::mem::size_of::<MFVideoArea>(),
        )
    };
    unsafe { media_type.GetBlob(key, bytes, None) }
        .with_context(|| format!("failed to read MP4 {label}"))?;
    Ok(Some(unsafe { area.assume_init() }))
}

fn validate_video_geometry(
    coded_width: u32,
    coded_height: u32,
    container_width: u32,
    container_height: u32,
    display: Option<DisplayArea>,
    expected_width: u32,
    expected_height: u32,
) -> anyhow::Result<(u32, u32)> {
    ensure!(
        container_width == expected_width && container_height == expected_height,
        "MP4 container display size {container_width}x{container_height} does not match profile \
         {expected_width}x{expected_height}"
    );
    ensure!(
        matches!(
            coded_width,
            value if value == expected_width || value == expected_width.div_ceil(16) * 16
        ) && matches!(
            coded_height,
            value if value == expected_height || value == expected_height.div_ceil(16) * 16
        ),
        "MP4 coded frame {coded_width}x{coded_height} is not the profile size or its H.264 \
         macroblock alignment for {expected_width}x{expected_height}"
    );
    if let Some(display) = display {
        ensure!(
            display.x == 0 && display.y == 0,
            "MP4 display aperture starts at {},{} instead of 0,0",
            display.x,
            display.y
        );
        ensure!(
            display.width == expected_width && display.height == expected_height,
            "MP4 display aperture {}x{} does not match profile {expected_width}x{expected_height}",
            display.width,
            display.height
        );
    }
    Ok((container_width, container_height))
}

fn mp4_video_dimensions(path: &Path) -> anyhow::Result<(u32, u32)> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let size = file.metadata()?.len();
    let reader = Mp4Reader::read_header(BufReader::new(file), size)
        .with_context(|| format!("invalid MP4 {}", path.display()))?;
    ensure!(
        reader.tracks().len() == 1,
        "MP4 must contain exactly one track"
    );
    let track = reader
        .tracks()
        .values()
        .next()
        .expect("track count checked");
    ensure!(
        track.track_type()? == TrackType::Video,
        "MP4 track is not video"
    );
    ensure!(
        track.media_type()? == MediaType::H264,
        "MP4 video is not H.264"
    );
    Ok((u32::from(track.width()), u32::from(track.height())))
}

const fn unpack_pair(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

#[cfg(test)]
mod tests {
    use super::{DisplayArea, validate_video_geometry};

    #[test]
    fn exact_coded_frame_is_accepted() {
        let geometry = validate_video_geometry(
            1920,
            1080,
            1920,
            1080,
            Some(DisplayArea {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            1920,
            1080,
        );

        assert_eq!(geometry.unwrap(), (1920, 1080));
    }

    #[test]
    fn aligned_coded_frame_requires_exact_display_aperture() {
        let geometry = validate_video_geometry(
            1920,
            1088,
            1920,
            1080,
            Some(DisplayArea {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            1920,
            1080,
        );

        assert_eq!(geometry.unwrap(), (1920, 1080));
    }

    #[test]
    fn aligned_frame_uses_exact_container_display_size_without_aperture() {
        let geometry = validate_video_geometry(1920, 1088, 1920, 1080, None, 1920, 1080);

        assert_eq!(geometry.unwrap(), (1920, 1080));
    }

    #[test]
    fn mismatched_container_display_size_is_rejected() {
        let geometry = validate_video_geometry(1920, 1088, 1920, 1088, None, 1920, 1080);

        assert!(geometry.is_err());
    }

    #[test]
    fn shifted_display_aperture_is_rejected() {
        let geometry = validate_video_geometry(
            1920,
            1088,
            1920,
            1080,
            Some(DisplayArea {
                x: 1,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            1920,
            1080,
        );

        assert!(geometry.is_err());
    }
}
