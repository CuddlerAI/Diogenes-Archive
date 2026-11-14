//! Read only typed depth/stencil members. Never copy padding or embedded COM pointers.
//!
//! Parsed for every PSO in every build. The CP2077 holstered camera admission requires the
//! draw-time depth-write mask and DSV format from here; this used to be enabled only in tests and
//! diagnostic builds, so every diagnostic run passed while the first release DLL was refused at
//! warmup with "local camera requires known draw-time depth-write state" (2026-09-18).
use super::*;
use crate::d3d12_render_camera::draw_pass::{PipelineDepthTest, StencilFace};

fn face(value: D3D12_DEPTH_STENCILOP_DESC, read_mask: u8, write_mask: u8) -> StencilFace {
    StencilFace {
        fail_op: value.StencilFailOp.0,
        depth_fail_op: value.StencilDepthFailOp.0,
        pass_op: value.StencilPassOp.0,
        comparison: value.StencilFunc.0,
        read_mask,
        write_mask,
    }
}

pub(super) fn from_desc(value: D3D12_DEPTH_STENCIL_DESC) -> PipelineDepthTest {
    PipelineDepthTest {
        depth_comparison: value.DepthFunc.0,
        depth_write_mask: value.DepthWriteMask.0,
        dsv_format: 0, // filled from the same PSO's format subobject
        stencil_enable: value.StencilEnable.as_bool(),
        front: face(
            value.FrontFace,
            value.StencilReadMask,
            value.StencilWriteMask,
        ),
        back: face(
            value.BackFace,
            value.StencilReadMask,
            value.StencilWriteMask,
        ),
        depth_bounds_enable: false,
    }
}

pub(super) fn stream_default() -> PipelineDepthTest {
    // D3D12 defaults for an omitted depth/stencil subobject, not an unreadable one.
    // https://learn.microsoft.com/windows/win32/api/d3d12/ns-d3d12-d3d12_depth_stencil_desc
    let face = D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP_KEEP,
        StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
        StencilPassOp: D3D12_STENCIL_OP_KEEP,
        StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
    };
    from_desc(D3D12_DEPTH_STENCIL_DESC {
        DepthEnable: true.into(),
        DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
        DepthFunc: D3D12_COMPARISON_FUNC_LESS,
        StencilEnable: false.into(),
        StencilReadMask: D3D12_DEFAULT_STENCIL_READ_MASK as u8,
        StencilWriteMask: D3D12_DEFAULT_STENCIL_WRITE_MASK as u8,
        FrontFace: face,
        BackFace: face,
    })
}

fn value_at<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let data = bytes.get(offset..offset.checked_add(size_of::<T>())?)?;
    // Callers use only plain D3D12 numeric depth/stencil structs, no owned fields.
    Some(unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<T>()) })
}

pub(super) fn from_stream(
    bytes: &[u8],
    payload: usize,
    kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE,
) -> Option<PipelineDepthTest> {
    match kind {
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL => {
            Some(from_desc(value_at(bytes, payload)?))
        }
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1 => {
            let value: D3D12_DEPTH_STENCIL_DESC1 = value_at(bytes, payload)?;
            let mut state = from_desc(D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: value.DepthEnable,
                DepthWriteMask: value.DepthWriteMask,
                DepthFunc: value.DepthFunc,
                StencilEnable: value.StencilEnable,
                StencilReadMask: value.StencilReadMask,
                StencilWriteMask: value.StencilWriteMask,
                FrontFace: value.FrontFace,
                BackFace: value.BackFace,
            });
            state.depth_bounds_enable = value.DepthBoundsTestEnable.as_bool();
            Some(state)
        }
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => {
            let value: D3D12_DEPTH_STENCIL_DESC2 = value_at(bytes, payload)?;
            let face = |v: D3D12_DEPTH_STENCILOP_DESC1| StencilFace {
                fail_op: v.StencilFailOp.0,
                depth_fail_op: v.StencilDepthFailOp.0,
                pass_op: v.StencilPassOp.0,
                comparison: v.StencilFunc.0,
                read_mask: v.StencilReadMask,
                write_mask: v.StencilWriteMask,
            };
            Some(PipelineDepthTest {
                depth_comparison: value.DepthFunc.0,
                depth_write_mask: value.DepthWriteMask.0,
                dsv_format: 0,
                stencil_enable: value.StencilEnable.as_bool(),
                front: face(value.FrontFace),
                back: face(value.BackFace),
                depth_bounds_enable: value.DepthBoundsTestEnable.as_bool(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::append;
    use super::*;

    fn put(bytes: &mut [u8], at: usize, value: i32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn depth_test_stream_variants_preserve_comparison_and_face_masks() {
        for variant in 0..3 {
            let mut bytes = Vec::new();
            let (front, back, bounds) = match variant {
                0 => {
                    append::<D3D12_DEPTH_STENCIL_DESC>(
                        &mut bytes,
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
                        &[1, 1, 7, 1],
                    );
                    (
                        offset_of!(D3D12_DEPTH_STENCIL_DESC, FrontFace),
                        offset_of!(D3D12_DEPTH_STENCIL_DESC, BackFace),
                        None,
                    )
                }
                1 => {
                    append::<D3D12_DEPTH_STENCIL_DESC1>(
                        &mut bytes,
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1,
                        &[1, 1, 7, 1],
                    );
                    (
                        offset_of!(D3D12_DEPTH_STENCIL_DESC1, FrontFace),
                        offset_of!(D3D12_DEPTH_STENCIL_DESC1, BackFace),
                        Some(offset_of!(D3D12_DEPTH_STENCIL_DESC1, DepthBoundsTestEnable)),
                    )
                }
                _ => {
                    append::<D3D12_DEPTH_STENCIL_DESC2>(
                        &mut bytes,
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2,
                        &[1, 1, 7, 1],
                    );
                    (
                        offset_of!(D3D12_DEPTH_STENCIL_DESC2, FrontFace),
                        offset_of!(D3D12_DEPTH_STENCIL_DESC2, BackFace),
                        Some(offset_of!(D3D12_DEPTH_STENCIL_DESC2, DepthBoundsTestEnable)),
                    )
                }
            };
            let payload = 4; // enum and all three typed payloads have alignment 4
            for at in [front, back] {
                for (offset, value) in [(0, 1), (4, 2), (8, 3), (12, 4)] {
                    put(&mut bytes, payload + at + offset, value);
                }
            }
            if variant == 2 {
                bytes[payload + front + offset_of!(D3D12_DEPTH_STENCILOP_DESC1, StencilReadMask)] =
                    0x11;
                bytes
                    [payload + front + offset_of!(D3D12_DEPTH_STENCILOP_DESC1, StencilWriteMask)] =
                    0x22;
                bytes[payload + back + offset_of!(D3D12_DEPTH_STENCILOP_DESC1, StencilReadMask)] =
                    0x33;
                bytes[payload + back + offset_of!(D3D12_DEPTH_STENCILOP_DESC1, StencilWriteMask)] =
                    0x44;
            } else {
                bytes[payload + offset_of!(D3D12_DEPTH_STENCIL_DESC, StencilReadMask)] = 0x11;
                bytes[payload + offset_of!(D3D12_DEPTH_STENCIL_DESC, StencilWriteMask)] = 0x22;
            }
            if let Some(at) = bounds {
                put(&mut bytes, payload + at, 1);
            }
            append::<DXGI_FORMAT>(
                &mut bytes,
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
                &[20],
            );
            let observed = parse_stream(&bytes).unwrap();
            let test = observed.test.unwrap();
            assert!(observed.depth.write);
            assert_eq!(test.depth_comparison, 7);
            assert_eq!(test.dsv_format, 20);
            assert!(test.stencil_enable);
            assert_eq!(test.front.read_mask, 0x11);
            assert_eq!(test.front.write_mask, 0x22);
            assert_eq!(test.back.read_mask, if variant == 2 { 0x33 } else { 0x11 });
            assert_eq!(test.back.write_mask, if variant == 2 { 0x44 } else { 0x22 });
            assert_eq!(
                (
                    test.front.fail_op,
                    test.front.depth_fail_op,
                    test.front.pass_op,
                    test.front.comparison
                ),
                (1, 2, 3, 4)
            );
            assert_eq!(test.depth_bounds_enable, variant != 0);
            // Never fall back to defaults when a declared subobject is malformed.
            assert_eq!(parse_stream(&bytes[..bytes.len() - 1]), None);
        }
    }

    #[test]
    fn depth_test_graphics_fields_and_omitted_stream_defaults_are_distinct() {
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            DSVFormat: DXGI_FORMAT(20),
            ..Default::default()
        };
        desc.DepthStencilState.DepthEnable = true.into();
        desc.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_GREATER_EQUAL;
        desc.DepthStencilState.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ZERO;
        desc.DepthStencilState.FrontFace.StencilFunc = D3D12_COMPARISON_FUNC_NEVER;
        desc.DepthStencilState.StencilReadMask = 0x12;
        let actual = graphics_state(&desc).unwrap();
        assert!(!actual.depth.write);
        assert_eq!(
            actual.test.unwrap().depth_comparison,
            D3D12_COMPARISON_FUNC_GREATER_EQUAL.0
        );
        assert_eq!(actual.test.unwrap().front.read_mask, 0x12);
        let mut bytes = Vec::new();
        append::<DXGI_FORMAT>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
            &[20],
        );
        let default = parse_stream(&bytes).unwrap().test.unwrap();
        assert_eq!(default.depth_comparison, D3D12_COMPARISON_FUNC_LESS.0);
        assert_eq!(default.front.comparison, D3D12_COMPARISON_FUNC_ALWAYS.0);
        assert_eq!(default.front.read_mask, 255);
        assert!(!default.stencil_enable);
        assert!(!default.depth_bounds_enable);
    }
}
