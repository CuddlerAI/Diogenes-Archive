use std::mem::ManuallyDrop;

use anyhow::{Context, ensure};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::crop::CropRect;

pub fn create_bgra_copy_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    create_texture(
        device,
        D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        },
        "BGRA GPU-copy texture",
    )
}

pub fn create_nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "NV12 dimensions must be even"
    );
    create_texture(
        device,
        D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        },
        "NV12 video-processor output texture",
    )
}

fn create_texture(
    device: &ID3D11Device,
    desc: D3D11_TEXTURE2D_DESC,
    label: &str,
) -> anyhow::Result<ID3D11Texture2D> {
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .with_context(|| format!("failed to create {label}"))?;
    texture.with_context(|| format!("D3D11 returned no {label}"))
}

pub struct BgraToNv12 {
    device: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    crop: RECT,
    output: RECT,
}

pub struct BoundVideoSurfaces {
    inputs: Vec<ID3D11VideoProcessorInputView>,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
}

#[derive(Clone, Copy, Debug)]
pub struct VideoConversionSpec {
    pub surface_width: u32,
    pub surface_height: u32,
    pub crop: CropRect,
    pub output_width: u32,
    pub output_height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl BgraToNv12 {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        spec: VideoConversionSpec,
    ) -> anyhow::Result<Self> {
        let VideoConversionSpec {
            surface_width,
            surface_height,
            crop,
            output_width,
            output_height,
            fps_num,
            fps_den,
        } = spec;
        crop.validate_for_surface(surface_width, surface_height)?;
        ensure!(
            output_width > 0 && output_height > 0,
            "video output dimensions must be positive"
        );
        ensure!(
            output_width.is_multiple_of(2) && output_height.is_multiple_of(2),
            "NV12 output dimensions must be even"
        );

        let video_device: ID3D11VideoDevice =
            device.cast().context("D3D11 video device is unavailable")?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .context("D3D11 video context is unavailable")?;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps_num,
                Denominator: fps_den,
            },
            InputWidth: surface_width,
            InputHeight: surface_height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps_num,
                Denominator: fps_den,
            },
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("D3D11 video processor does not support the requested BGRA/NV12 geometry")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("failed to create D3D11 video processor")?;
        let output = RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        };

        unsafe {
            // Explicit SDR contract: WGC BGRA is full-range RGB; encoder NV12 is
            // BT.709 with studio/limited nominal range. Leaving these at the
            // D3D11 defaults would silently select a BT.601 YCbCr matrix.
            let input_color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: color_space_bits(true, false, false, 0),
            };
            let output_color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: color_space_bits(true, false, true, 1),
            };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &raw const input_color);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &raw const output_color);
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&output));
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamOutputRate(
                &processor,
                0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL,
                false,
                None,
            );
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&RECT {
                    left: crop.left,
                    top: crop.top,
                    right: crop.right,
                    bottom: crop.bottom,
                }),
            );
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&output));
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }

        Ok(Self {
            device: video_device,
            context: video_context,
            enumerator,
            processor,
            crop: RECT {
                left: crop.left,
                top: crop.top,
                right: crop.right,
                bottom: crop.bottom,
            },
            output,
        })
    }

    pub fn prepare_surfaces(
        &self,
        inputs: &[ID3D11Texture2D],
        outputs: &[ID3D11Texture2D],
    ) -> anyhow::Result<BoundVideoSurfaces> {
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let inputs = inputs
            .iter()
            .map(|texture| {
                let mut view = None;
                unsafe {
                    self.device.CreateVideoProcessorInputView(
                        texture,
                        &self.enumerator,
                        &input_desc,
                        Some(&mut view),
                    )
                }
                .context("failed to pre-create BGRA video-processor input view")?;
                view.context("D3D11 returned no input view")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let outputs = outputs
            .iter()
            .map(|texture| {
                let mut view = None;
                unsafe {
                    self.device.CreateVideoProcessorOutputView(
                        texture,
                        &self.enumerator,
                        &output_desc,
                        Some(&mut view),
                    )
                }
                .context("failed to pre-create NV12 video-processor output view")?;
                view.context("D3D11 returned no output view")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(BoundVideoSurfaces { inputs, outputs })
    }

    pub fn convert(
        &self,
        surfaces: &BoundVideoSurfaces,
        input_slot: usize,
        output_slot: usize,
        frame_index: u32,
    ) -> anyhow::Result<()> {
        let input_view = surfaces
            .inputs
            .get(input_slot)
            .context("BGRA input view slot is out of range")?;
        let output_view = surfaces
            .outputs
            .get(output_slot)
            .context("NV12 output view slot is out of range")?;

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            InputFrameOrField: frame_index,
            pInputSurface: ManuallyDrop::new(Some(input_view.clone())),
            ..Default::default()
        };
        let result = unsafe {
            self.context.VideoProcessorBlt(
                &self.processor,
                output_view,
                frame_index,
                std::slice::from_ref(&stream),
            )
        }
        .context("GPU BGRA-to-NV12 VideoProcessorBlt failed");
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result
    }

    #[must_use]
    pub fn crop_rect(&self) -> RECT {
        self.crop
    }

    #[must_use]
    pub fn output_rect(&self) -> RECT {
        self.output
    }
}

const fn color_space_bits(
    processing_usage: bool,
    limited_rgb: bool,
    bt709_ycbcr: bool,
    nominal_range: u32,
) -> u32 {
    (processing_usage as u32)
        | ((limited_rgb as u32) << 1)
        | ((bt709_ycbcr as u32) << 2)
        | ((nominal_range & 0b11) << 4)
}
