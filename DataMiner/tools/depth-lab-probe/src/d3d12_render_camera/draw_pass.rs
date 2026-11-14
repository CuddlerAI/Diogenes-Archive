//! Bounded draw-time state, not proof of pixel contribution. Local holstered v2 uses
//! known PSO write state to distinguish depth-camera sources from non-writing witnesses.
use super::{CapturedViewport, RenderCameraTracker};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct StencilFace {
    pub fail_op: i32,
    pub depth_fail_op: i32,
    pub pass_op: i32,
    pub comparison: i32,
    pub read_mask: u8,
    pub write_mask: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct PipelineDepthTest {
    pub depth_comparison: i32,
    pub depth_write_mask: i32,
    pub dsv_format: i32,
    pub stencil_enable: bool,
    pub front: StencilFace,
    pub back: StencilFace,
    pub depth_bounds_enable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ViewportObservation {
    pub count: u32,
    /// x, y, width, height, minDepth, maxDepth; exact f32 bits, no NaN conversion.
    /// Only the single-viewport case is retained. Multiple viewports stay explicit.
    pub single_bits: Option<[u32; 6]>,
}

impl From<CapturedViewport> for ViewportObservation {
    fn from(value: CapturedViewport) -> Self {
        let v = value.first;
        Self {
            count: value.count,
            single_bits: (value.count == 1).then(|| {
                [
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth,
                ]
                .map(f32::to_bits)
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct ScissorObservation {
    pub count: u32,
    /// left, top, right, bottom. None when not exactly one readable rectangle.
    pub single_rect: Option<[i32; 4]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DrawPassContext {
    pub schema: &'static str,
    pub non_trainable: bool,
    pub pipeline: Option<PipelineDepthTest>,
    /// From the exact bound CPU DSV descriptor, never a resource-level candidate.
    pub dsv_flags: Option<i32>,
    pub viewport: Option<ViewportObservation>,
    pub scissor: Option<ScissorObservation>,
}

impl RenderCameraTracker {
    pub fn set_scissor(&mut self, raw: usize, scissor: ScissorObservation) {
        if let Some(list) = self.lists.get_mut(&raw) {
            list.scissor = Some(scissor);
        }
    }

    pub fn draw_pass_context(&self, raw: usize, dsv_flags: Option<i32>) -> DrawPassContext {
        let list = self.lists.get(&raw);
        DrawPassContext {
            schema: "dm-d3d12-draw-pass/v1",
            non_trainable: true,
            pipeline: None,
            dsv_flags,
            viewport: list.and_then(|list| list.viewport).map(Into::into),
            scissor: list.and_then(|list| list.scissor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

    #[test]
    fn draw_pass_range_is_frozen_and_reset_or_reused_lists_are_unknown() {
        let mut tracker = RenderCameraTracker::new(1920, 1080, 75.0);
        tracker.register_list(7, 1);
        tracker.set_viewport(
            7,
            CapturedViewport {
                count: 1,
                first: D3D12_VIEWPORT {
                    Width: 1920.0,
                    Height: 1080.0,
                    MaxDepth: 1.0,
                    ..Default::default()
                },
            },
        );
        tracker.set_scissor(
            7,
            ScissorObservation {
                count: 1,
                single_rect: Some([0, 0, 1920, 1080]),
            },
        );
        let frozen = tracker.draw_pass_context(7, Some(1));
        tracker.set_scissor(
            7,
            ScissorObservation {
                count: 1,
                single_rect: Some([0, 0, 0, 0]),
            },
        );
        assert_eq!(
            frozen.scissor.unwrap().single_rect,
            Some([0, 0, 1920, 1080])
        );
        assert_eq!(
            frozen.viewport.unwrap().single_bits.unwrap()[2],
            1920f32.to_bits()
        );
        assert_eq!(frozen.dsv_flags, Some(1));
        tracker.reset_list(7);
        assert_eq!(tracker.draw_pass_context(7, None).viewport, None);
        assert_eq!(tracker.draw_pass_context(7, None).scissor, None);
        tracker.set_scissor(7, frozen.scissor.unwrap());
        tracker.release_list(7);
        tracker.register_list(7, 2);
        assert_eq!(tracker.draw_pass_context(7, None).scissor, None);
        assert_eq!(tracker.draw_pass_context(999, None).dsv_flags, None);
    }

    #[test]
    fn multiple_or_nonfinite_viewports_are_not_fabricated_into_full_screen() {
        let v = CapturedViewport {
            count: 2,
            first: D3D12_VIEWPORT::default(),
        };
        assert_eq!(ViewportObservation::from(v).single_bits, None);
        let v = CapturedViewport {
            count: 1,
            first: D3D12_VIEWPORT {
                Width: f32::NAN,
                ..Default::default()
            },
        };
        let observed = ViewportObservation::from(v);
        assert_eq!(observed.single_bits.unwrap()[2], f32::NAN.to_bits());
        assert!(serde_json::to_value(observed).unwrap()["single_bits"].is_array());
    }
}
