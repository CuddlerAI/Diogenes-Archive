//! Diagnostic camera read: the production FPP camera chain plus FOV/zoom fields that are
//! recorded for analysis only and are not evidence of the final render FOV.
//! Named layouts: RED4ext.SDK ad7277714ad30d6885d7050c5ba24fa0102f6920:
//! Generated/ent/BaseCameraComponent.hpp (fov 128, zoom 15C),
//! Generated/game/CameraComponent.hpp (zoom overrides 278/27C, FOV overrides 2AC/2B0).
//! The component itself is resolved by `cp2077_player_probe::fpp_camera`.

use crate::cp2077_player_probe::fpp_camera::{FppCameraChain, address};
use crate::cp2077_pose_wiring::{FormalPlayerAddresses, read_process_value_no_query as rpm};
use serde::Serialize;

pub(super) use crate::cp2077_player_probe::fpp_camera::CHAIN_ID;
const FOV: usize = 0x128;
const ZOOM: usize = 0x15c;
const ZOOM_OVERRIDE: usize = 0x278;
const OVERRIDE: usize = 0x2ac;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Payload {
    transform: [u32; 8],
    fov: u32,
    zoom: u32,
    zoom_overrides: [u32; 2],
    overrides: [u32; 2],
}

fn stable_payload(
    mut read: impl FnMut() -> Result<Payload, String>,
) -> Result<(Payload, u32), String> {
    let mut previous = read()?;
    for attempt in 2..=4 {
        let current = read()?;
        if current == previous {
            return Ok((current, attempt));
        }
        previous = current;
    }
    Err("camera transform/FOV changed during bounded stable read".into())
}

fn decode_payload(
    current: Payload,
    axes: depth_lab::D3d12AxisMapping,
    scale: f64,
    attempts: u32,
) -> Result<MemorySample, String> {
    let (position, rotation) =
        crate::cp2077_player_probe::decode_world_transform(current.transform, axes, scale)?;
    let finite = |bits| {
        let value = f32::from_bits(bits);
        value.is_finite().then_some(value)
    };
    let fov = finite(current.fov)
        .filter(|v| *v > 0.0 && *v < 180.0)
        .map(f64::from);
    Ok(MemorySample {
        status: "candidate",
        reason: None,
        position: Some(position),
        rotation: Some(rotation),
        fov_deg: fov,
        fov_axis: "unverified_engine_parameter",
        fov_semantics: "component_base_fov_not_final_render_fov",
        component_zoom: finite(current.zoom),
        zoom_override_weight: finite(current.zoom_overrides[0]),
        zoom_override_value: finite(current.zoom_overrides[1]),
        fov_override_weight: finite(current.overrides[0]),
        fov_override_value: finite(current.overrides[1]),
        stable_read_attempts: attempts,
    })
}

#[derive(Debug, Serialize)]
pub(super) struct MemorySample {
    pub status: &'static str,
    pub reason: Option<String>,
    pub position: Option<[f64; 3]>,
    pub rotation: Option<[f64; 4]>,
    pub fov_deg: Option<f64>,
    pub fov_axis: &'static str,
    pub fov_semantics: &'static str,
    pub component_zoom: Option<f32>,
    pub zoom_override_weight: Option<f32>,
    pub zoom_override_value: Option<f32>,
    pub fov_override_weight: Option<f32>,
    pub fov_override_value: Option<f32>,
    pub stable_read_attempts: u32,
}

impl MemorySample {
    pub(super) fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            status: "unavailable",
            reason: Some(reason.into()),
            position: None,
            rotation: None,
            fov_deg: None,
            fov_axis: "unverified_engine_parameter",
            fov_semantics: "component_base_fov_not_final_render_fov",
            component_zoom: None,
            zoom_override_weight: None,
            zoom_override_value: None,
            fov_override_weight: None,
            fov_override_value: None,
            stable_read_attempts: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Sampler {
    chain: FppCameraChain,
    axes: depth_lab::D3d12AxisMapping,
    scale: f64,
}

impl Sampler {
    pub(super) fn locate(
        player: FormalPlayerAddresses,
        axes: depth_lab::D3d12AxisMapping,
        scale: f64,
    ) -> Result<Self, String> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err("invalid diagnostic metric scale".into());
        }
        Ok(Self {
            chain: FppCameraChain::locate(player)?,
            axes,
            scale,
        })
    }

    fn payload(&self) -> Result<Payload, String> {
        let component = self.chain.component();
        Ok(Payload {
            transform: self.chain.read_transform()?,
            fov: rpm(address(component, FOV)?)?,
            zoom: rpm(address(component, ZOOM)?)?,
            zoom_overrides: rpm(address(component, ZOOM_OVERRIDE)?)?,
            overrides: rpm(address(component, OVERRIDE)?)?,
        })
    }

    pub(super) fn sample(&self) -> Result<MemorySample, String> {
        self.sample_with_words().map(|(sample, _)| sample)
    }

    /// Same read as `sample`, also returning the native WorldTransform words as content identity.
    pub(super) fn sample_with_words(&self) -> Result<(MemorySample, [u32; 8]), String> {
        self.chain.check_identity()?;
        let (payload, attempts) = stable_payload(|| self.payload())?;
        self.chain.check_identity()?;
        Ok((
            decode_payload(payload, self.axes, self.scale, attempts)?,
            payload.transform,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn payload() -> Payload {
        Payload {
            transform: [
                131072,
                (-262144i32) as u32,
                65536,
                0,
                0,
                0,
                0,
                1.0f32.to_bits(),
            ],
            fov: 75.0f32.to_bits(),
            zoom: 1.0f32.to_bits(),
            zoom_overrides: [0, 1.0f32.to_bits()],
            overrides: [0, 90.0f32.to_bits()],
        }
    }
    fn axes() -> depth_lab::D3d12AxisMapping {
        use depth_lab::D3d12SignedAxis::*;
        depth_lab::D3d12AxisMapping {
            delivery_x_from: PositiveX,
            delivery_y_from: PositiveZ,
            delivery_z_from: PositiveY,
        }
    }
    #[test]
    fn diagnostic_transform_uses_the_formal_player_coordinate_conversion() {
        let value = decode_payload(payload(), axes(), 1.0, 2).unwrap();
        assert_eq!(value.position, Some([1.0, 0.5, -2.0]));
        assert_eq!(value.rotation, Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(value.fov_deg, Some(75.0));
        assert_eq!(value.fov_override_value, Some(90.0));
        assert_eq!(value.fov_axis, "unverified_engine_parameter");
    }
    #[test]
    fn stable_payload_includes_fov_and_zoom_and_does_not_reuse_last_frame() {
        let p = payload();
        assert_eq!(stable_payload(|| Ok(p)).unwrap().1, 2);
        let mut calls = 0;
        assert!(
            stable_payload(|| {
                calls += 1;
                let mut next = p;
                next.fov += calls;
                Ok(next)
            })
            .is_err()
        );
        assert_eq!(calls, 4);
        assert!(stable_payload(|| Err("RPM failed".into())).is_err());
    }
    #[test]
    fn malformed_quaternion_refuses_and_nonfinite_fov_is_null() {
        let mut p = payload();
        p.fov = f32::NAN.to_bits();
        p.zoom = f32::INFINITY.to_bits();
        let decoded = decode_payload(p, axes(), 1.0, 2).unwrap();
        assert_eq!(decoded.fov_deg, None);
        assert_eq!(decoded.component_zoom, None);
        p.transform[7] = 0;
        assert!(decode_payload(p, axes(), 1.0, 2).is_err());
    }
}
