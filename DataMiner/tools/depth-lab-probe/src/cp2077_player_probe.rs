//! Exact-build CP2077 PlayerPuppet world-transform sampler for formal D3D12 production.
//!
//! Initialization reuses the bounded first-party RTTI resolver. The control thread periodically
//! audits the retained chain with VirtualQuery/RPM; the pair-sampler thread consumes only a
//! fresh verdict and performs stable, fallible RPM reads without VirtualQuery. There is no
//! stale-value fallback and no raw pointer dereference in the host process.
//!
//! Which Present an actor root belongs to is decided by camera content, not by when it was
//! read: memory holds the simulation state, which usually leads the rendered frame by one and
//! slips around stalls. The Present path reads no process memory; it asks the sampled ring
//! which state carried the camera it rendered. See `pair_sampler` and
//! `depth_lab::cp2077_pose_join`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use windows::Win32::System::Performance::QueryPerformanceCounter;

use crate::cp2077_pose_wiring::{
    FormalPlayerAddresses, audit_formal_player, locate_formal_player, read_process_value_no_query,
};
use crate::player_writer::PlayerRootSample;

#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub(crate) mod diagnostic;
pub(crate) mod fpp_camera;
mod pair_sampler;
#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub(crate) use pair_sampler::priority_raised as pair_sampler_priority_raised;

pub const PROVIDER: &str = depth_lab::CP2077_PLAYER_POSE_SOURCE;
// CP2077's audit traverses both cpPlayerSystem and PlayerPuppet RTTI chains, so it performs many
// more VirtualQuery-backed checks than the Fallout single-slot audit. The render path still
// re-reads every retained pointer and owner relation with fallible RPM on every Present; this
// one-second structural-verdict ceiling only tolerates a slow control-thread refresh and remains a
// hard fail-closed bound.
const AUDIT_VERDICT_TTL_MS: i64 = 1_000;
// The control thread audits every 100 ms. Keep more than one complete TTL window so a verdict
// which finishes just after Present(N)'s QPC cannot overwrite the still-valid causal verdict for
// that Present. This bounded history is control-plane state only; the render path never appends or
// allocates.
const AUDIT_VERDICT_HISTORY_CAPACITY: usize = 16;
const WORLD_POSITION_DIVISOR: f64 = 131_072.0;
const MAX_ABS_WORLD_UNITS: f64 = 10_000_000.0;
const STABLE_READ_ATTEMPTS: usize = 8;
type WorldTransformWords = [u32; 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlayerAuditVerdict {
    generation: u64,
    addresses: FormalPlayerAddresses,
    audited_qpc: i64,
}

pub(crate) struct PlayerAuditOutcome {
    generation: u64,
    addresses: FormalPlayerAddresses,
    completed_qpc: i64,
    result: Result<(), String>,
}

impl PlayerAuditOutcome {
    fn sampler_verdict(&self) -> Result<PlayerAuditVerdict, String> {
        self.result.as_ref().map_err(Clone::clone)?;
        Ok(PlayerAuditVerdict {
            generation: self.generation,
            addresses: self.addresses,
            audited_qpc: self.completed_qpc,
        })
    }
}

struct PlayerAuditState {
    outcomes: VecDeque<PlayerAuditOutcome>,
    fatal_error: Option<String>,
}

impl PlayerAuditState {
    const fn empty() -> Self {
        Self {
            outcomes: VecDeque::new(),
            fatal_error: None,
        }
    }

    fn publish(&mut self, outcome: Result<PlayerAuditOutcome, String>) {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // Without a completion QPC this failure cannot be ordered against Present. Fail
                // closed instead of pretending that it belongs to any particular frame.
                self.outcomes.clear();
                self.fatal_error = Some(error);
                return;
            }
        };
        if let Some(latest) = self.outcomes.back() {
            if latest.generation == outcome.generation
                && latest.addresses == outcome.addresses
                && latest.completed_qpc >= outcome.completed_qpc
            {
                let previous_qpc = latest.completed_qpc;
                self.outcomes.clear();
                self.fatal_error = Some(format!(
                    "CP2077 player audit completion QPC is not monotonic: previous={} current={}",
                    previous_qpc, outcome.completed_qpc
                ));
                return;
            }
            if latest.generation != outcome.generation || latest.addresses != outcome.addresses {
                self.outcomes.clear();
            }
        }
        if self.outcomes.len() == AUDIT_VERDICT_HISTORY_CAPACITY {
            self.outcomes.pop_front();
        }
        self.outcomes.push_back(outcome);
        self.fatal_error = None;
    }

    fn latest_at_or_before(&self, present_qpc: i64) -> Option<&PlayerAuditOutcome> {
        self.outcomes
            .iter()
            .rev()
            .find(|outcome| outcome.completed_qpc <= present_qpc)
    }

    fn earliest_qpc(&self) -> Option<i64> {
        self.outcomes.front().map(|outcome| outcome.completed_qpc)
    }

    fn clear(&mut self) {
        self.outcomes.clear();
        self.fatal_error = None;
    }
}

static PLAYER_AUDITS: Mutex<PlayerAuditState> = Mutex::new(PlayerAuditState::empty());

pub(crate) fn publish_player_verdict(outcome: Result<PlayerAuditOutcome, String>) {
    if let Ok(mut guard) = PLAYER_AUDITS.lock() {
        guard.publish(outcome);
    }
}

pub(crate) fn clear_player_verdict() {
    if let Ok(mut guard) = PLAYER_AUDITS.lock() {
        guard.clear();
    }
}

pub(crate) struct PlayerAuditTask {
    addresses: FormalPlayerAddresses,
}

// SAFETY: the task contains current-process addresses only. audit() performs bounded read-only
// validation and is owned by the serialized control thread.
unsafe impl Send for PlayerAuditTask {}

impl PlayerAuditTask {
    pub(crate) fn audit(&self, generation: u64) -> Result<PlayerAuditOutcome, String> {
        complete_player_audit(
            generation,
            self.addresses,
            || audit_formal_player(self.addresses),
            player_audit_qpc,
        )
    }
}

fn complete_player_audit(
    generation: u64,
    addresses: FormalPlayerAddresses,
    audit: impl FnOnce() -> Result<(), String>,
    clock: impl FnOnce() -> Result<i64, String>,
) -> Result<PlayerAuditOutcome, String> {
    let result = audit();
    // The verdict's TTL starts when the structural audit finishes. CP2077's several
    // VirtualQuery/RPM checks can contend with the game's VAD activity; stamping the verdict with
    // the control-loop time from before those checks made a newly published verdict already stale.
    let audited_qpc = clock()?;
    if audited_qpc <= 0 {
        return Err("CP2077 player audit completion QPC is invalid".into());
    }
    Ok(PlayerAuditOutcome {
        generation,
        addresses,
        completed_qpc: audited_qpc,
        result,
    })
}

fn player_audit_qpc() -> Result<i64, String> {
    let mut qpc = 0i64;
    unsafe { QueryPerformanceCounter(&mut qpc) }
        .map_err(|error| format!("query CP2077 player audit completion QPC failed: {error}"))?;
    Ok(qpc)
}

pub struct Cp2077PlayerSampler {
    addresses: FormalPlayerAddresses,
    generation: u64,
    pairs: pair_sampler::PairSampler,
}

// SAFETY: the stored addresses are never dereferenced. Only the pair-sampler thread reads them,
// through fallible ReadProcessMemory under a fresh control-thread verdict; the render thread
// reads no process memory and only scans the sampled ring.
unsafe impl Send for Cp2077PlayerSampler {}
unsafe impl Sync for Cp2077PlayerSampler {}

impl Cp2077PlayerSampler {
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(crate) fn diagnostic_addresses(&self) -> FormalPlayerAddresses {
        self.addresses
    }

    pub fn new(
        actual_executable_sha256: &str,
        binding: &depth_lab::D3d12CameraBinding,
        generation: u64,
        output_dir: &Path,
    ) -> Result<Self, String> {
        if actual_executable_sha256 != depth_lab::CP2077_BUILD_20383525_EXE_SHA256
            || binding.executable_sha256 != actual_executable_sha256
            || binding.holstered_camera.is_none()
        {
            return Err("CP2077 formal player sampler requires the exact holstered build".into());
        }
        let metres_per_unit = binding
            .view_metres_per_unit
            .parse::<f64>()
            .map_err(|_| "CP2077 formal player metric scale is invalid".to_string())?;
        if !metres_per_unit.is_finite() || metres_per_unit <= 0.0 {
            return Err("CP2077 formal player metric scale is invalid".into());
        }
        let addresses = locate_formal_player(
            actual_executable_sha256,
            format!("formal-{generation}"),
            output_dir,
        )?;
        let mut frequency = 0i64;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|error| format!("query CP2077 player audit QPC frequency failed: {error}"))?;
        if frequency <= 0 {
            return Err("CP2077 player audit QPC frequency is invalid".into());
        }
        let ttl_qpc = frequency.saturating_mul(AUDIT_VERDICT_TTL_MS) / 1_000;
        // Without the simulation-side camera there is no evidence of which frame an actor
        // root belongs to, so a failed resolution refuses the run instead of degrading to the
        // Present-time read.
        let source = pair_sampler::PairSource {
            addresses,
            camera: crate::startup_timing::phase("fpp_camera_locate", || {
                fpp_camera::FppCameraChain::locate(addresses)
            })?,
            axis_mapping: binding.axis_mapping,
            metres_per_unit,
        };
        Ok(Self {
            addresses,
            generation,
            pairs: pair_sampler::PairSampler::start(source, generation, frequency, ttl_qpc)?,
        })
    }

    #[must_use]
    pub(crate) fn audit_task(&self) -> PlayerAuditTask {
        PlayerAuditTask {
            addresses: self.addresses,
        }
    }

    /// `camera_position`/`camera_rotation` are this Present's rendered camera from the GPU
    /// constants; the returned actor root is the one read together with that camera content.
    ///
    /// No process memory is read here and the audit lock is not touched: every state in the
    /// ring was read by the sampler thread under a verdict admitted at that moment. A stale
    /// audit, a replaced pointer or a dead sampler stop new states from arriving, so the next
    /// Present finds no state carrying its camera and the run stops with the sampler's reason.
    pub fn sample(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        generation: u64,
        camera_position: [f64; 3],
        camera_rotation: [f64; 4],
    ) -> Result<PlayerRootSample, String> {
        if present_qpc <= 0 {
            return Err("CP2077 player Present QPC is invalid".into());
        }
        if generation != self.generation {
            return Err(format!(
                "CP2077 player sampler generation mismatch: sampler={} active={generation}",
                self.generation
            ));
        }
        let joined = self
            .pairs
            .join(present_qpc, camera_position, camera_rotation)?;
        let clamp = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
        Ok(PlayerRootSample {
            present_seq,
            present_qpc,
            kcd2_root_source: None,
            cp2077_camera_join: Some(depth_lab::Cp2077CameraJoinReceipt {
                state_first_qpc: joined.pair.first_qpc,
                state_last_qpc: joined.pair.last_qpc,
                state_reads: joined.pair.reads,
                position_residual_metres: joined.position_residual_metres,
                rotation_residual_degrees: joined.rotation_residual_degrees,
                candidates: clamp(joined.candidates),
                torn_candidates: clamp(joined.torn_candidates),
            }),
            player_position: joined.pair.player_position,
            player_rotation_quaternion: joined.pair.player_rotation,
        })
    }
}

/// Identity-checked stable read of the actor root under an admitted audit verdict.
///
/// A fresh audit admits the exact chain, but the game may still destroy an object between audit
/// ticks. Reads stay free of VirtualQuery and fallible, so death/loading races become an error
/// instead of access-violating the host process.
fn read_actor_root(
    addresses: FormalPlayerAddresses,
    verdict: &PlayerAuditVerdict,
) -> Result<WorldTransformWords, String> {
    let player = read_process_value_no_query::<usize>(addresses.player_slot as usize)?;
    if player != verdict.addresses.player_object {
        return Err("CP2077 PlayerPuppet changed since the last background audit".into());
    }
    let component =
        read_process_value_no_query::<usize>(addresses.transform_component_slot as usize)?;
    if component != verdict.addresses.transform_component {
        return Err(
            "CP2077 PlayerPuppet transform component changed since the last background audit"
                .into(),
        );
    }
    let owner = read_process_value_no_query::<usize>(addresses.component_owner_slot as usize)?;
    if owner != player {
        return Err("CP2077 transform component owner no longer points to PlayerPuppet".into());
    }
    read_stable_transform(addresses.world_transform)
}

fn player_verdict_admits(
    verdict: &PlayerAuditVerdict,
    generation: u64,
    now_qpc: i64,
    ttl_qpc: i64,
    addresses: FormalPlayerAddresses,
) -> Result<(), String> {
    if verdict.generation != generation {
        return Err(format!(
            "CP2077 player audit verdict generation mismatch: verdict={} active={generation}",
            verdict.generation
        ));
    }
    if now_qpc < verdict.audited_qpc {
        return Err(format!(
            "CP2077 player audit verdict is newer than this Present: lead_qpc={}",
            verdict.audited_qpc.saturating_sub(now_qpc)
        ));
    }
    if now_qpc - verdict.audited_qpc > ttl_qpc {
        return Err(format!(
            "CP2077 player audit verdict expired before this Present: age_qpc={} ttl_qpc={ttl_qpc}",
            now_qpc - verdict.audited_qpc
        ));
    }
    if verdict.addresses != addresses {
        return Err("CP2077 player audit verdict belongs to a different pointer chain".into());
    }
    Ok(())
}

fn read_stable_transform(
    transform: *const WorldTransformWords,
) -> Result<WorldTransformWords, String> {
    let read = || read_process_value_no_query::<WorldTransformWords>(transform as usize);
    let mut previous = read()?;
    for _ in 1..STABLE_READ_ATTEMPTS {
        let current = read()?;
        if current == previous {
            return Ok(current);
        }
        previous = current;
    }
    Err("CP2077 PlayerPuppet WorldTransform changed during the Present read".into())
}

pub(crate) fn decode_world_transform(
    words: WorldTransformWords,
    axis_mapping: depth_lab::D3d12AxisMapping,
    metres_per_unit: f64,
) -> Result<([f64; 3], [f64; 4]), String> {
    let source_position = std::array::from_fn(|axis| {
        f64::from(i32::from_le_bytes(words[axis].to_le_bytes())) / WORLD_POSITION_DIVISOR
    });
    if !source_position
        .iter()
        .all(|value| value.is_finite() && value.abs() <= MAX_ABS_WORLD_UNITS)
    {
        return Err("CP2077 PlayerPuppet position is outside the structural bound".into());
    }
    let source_quaternion = std::array::from_fn(|axis| f32::from_bits(words[4 + axis]) as f64);
    depth_lab::d3d12_camera::transform_world_pose(
        axis_mapping,
        metres_per_unit,
        source_position,
        source_quaternion,
    )
    .map_err(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use depth_lab::{D3d12AxisMapping, D3d12SignedAxis};
    use std::cell::Cell;

    fn cp_mapping() -> D3d12AxisMapping {
        D3d12AxisMapping {
            delivery_x_from: D3d12SignedAxis::PositiveX,
            delivery_y_from: D3d12SignedAxis::PositiveZ,
            delivery_z_from: D3d12SignedAxis::PositiveY,
        }
    }

    #[test]
    fn decodes_fixed_point_position_and_ijkr_rotation() {
        let half = 0.5_f32.sqrt();
        let words = [
            131_072u32,
            (-262_144i32) as u32,
            65_536u32,
            0,
            0.0f32.to_bits(),
            half.to_bits(),
            0.0f32.to_bits(),
            half.to_bits(),
        ];
        let (position, quaternion) = decode_world_transform(words, cp_mapping(), 1.0).unwrap();
        assert_eq!(position, [1.0, 0.5, -2.0]);
        assert!(quaternion[0].abs() < 1e-6, "{quaternion:?}");
        assert!(quaternion[1].abs() < 1e-6, "{quaternion:?}");
        assert!(
            (quaternion[2] + f64::from(half)).abs() < 1e-6,
            "{quaternion:?}"
        );
        assert!(
            (quaternion[3] - f64::from(half)).abs() < 1e-6,
            "{quaternion:?}"
        );
    }

    #[test]
    fn stable_reader_returns_one_complete_transform() {
        let words = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(read_stable_transform(&words), Ok(words));
    }

    #[test]
    fn stable_reader_rejects_a_missing_transform_without_dereferencing_it() {
        assert!(read_stable_transform(std::ptr::null()).is_err());
        assert!(read_stable_transform(std::ptr::dangling()).is_err());
    }

    #[test]
    fn verdict_rejects_cross_generation_expiry_and_chain_drift() {
        let words = [0u32; 8];
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: (&0usize) as *const usize,
            player_object: 2,
            transform_component_slot: (&0usize) as *const usize,
            transform_component: 3,
            component_owner_slot: (&0usize) as *const usize,
            world_transform: &words,
        };
        let verdict = PlayerAuditVerdict {
            generation: 7,
            addresses,
            audited_qpc: 100,
        };
        assert!(player_verdict_admits(&verdict, 7, 110, 20, addresses).is_ok());
        assert!(player_verdict_admits(&verdict, 8, 110, 20, addresses).is_err());
        assert!(player_verdict_admits(&verdict, 7, 121, 20, addresses).is_err());
        let mut drifted = addresses;
        drifted.player_object = 9;
        assert!(player_verdict_admits(&verdict, 7, 110, 20, drifted).is_err());
    }

    #[test]
    fn audit_history_uses_the_newest_verdict_not_later_than_the_present() {
        let words = [0u32; 8];
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: (&0usize) as *const usize,
            player_object: 2,
            transform_component_slot: (&0usize) as *const usize,
            transform_component: 3,
            component_owner_slot: (&0usize) as *const usize,
            world_transform: &words,
        };
        let mut state = PlayerAuditState::empty();
        for completed_qpc in [100, 200, 300] {
            state.publish(Ok(PlayerAuditOutcome {
                generation: 7,
                addresses,
                completed_qpc,
                result: Ok(()),
            }));
        }

        assert_eq!(
            state
                .latest_at_or_before(250)
                .map(|outcome| outcome.completed_qpc),
            Some(200)
        );
        assert_eq!(
            state
                .latest_at_or_before(300)
                .map(|outcome| outcome.completed_qpc),
            Some(300)
        );
        assert!(state.latest_at_or_before(99).is_none());
    }

    #[test]
    fn future_audit_error_does_not_reject_an_earlier_present() {
        let words = [0u32; 8];
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: (&0usize) as *const usize,
            player_object: 2,
            transform_component_slot: (&0usize) as *const usize,
            transform_component: 3,
            component_owner_slot: (&0usize) as *const usize,
            world_transform: &words,
        };
        let outcome = |completed_qpc, result| PlayerAuditOutcome {
            generation: 7,
            addresses,
            completed_qpc,
            result,
        };
        let mut state = PlayerAuditState::empty();
        state.publish(Ok(outcome(100, Ok(()))));
        state.publish(Ok(outcome(300, Err("chain drift".into()))));

        assert!(
            state
                .latest_at_or_before(250)
                .unwrap()
                .sampler_verdict()
                .is_ok(),
            "an audit error completed after Present must not reject that earlier Present"
        );
        assert_eq!(
            state
                .latest_at_or_before(300)
                .unwrap()
                .sampler_verdict()
                .unwrap_err(),
            "chain drift"
        );

        state.publish(Ok(outcome(400, Ok(()))));
        assert_eq!(
            state
                .latest_at_or_before(350)
                .unwrap()
                .sampler_verdict()
                .unwrap_err(),
            "chain drift"
        );
        assert!(
            state
                .latest_at_or_before(400)
                .unwrap()
                .sampler_verdict()
                .is_ok()
        );
        assert_eq!(state.fatal_error, None);
    }

    #[test]
    fn audit_ttl_starts_after_the_structural_checks_finish() {
        let words = [0u32; 8];
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: (&0usize) as *const usize,
            player_object: 2,
            transform_component_slot: (&0usize) as *const usize,
            transform_component: 3,
            component_owner_slot: (&0usize) as *const usize,
            world_transform: &words,
        };
        let order = Cell::new(0);
        let outcome = complete_player_audit(
            7,
            addresses,
            || {
                assert_eq!(order.get(), 0);
                order.set(1);
                Ok(())
            },
            || {
                assert_eq!(order.get(), 1, "the completion clock ran before the audit");
                order.set(2);
                Ok(900)
            },
        )
        .unwrap();

        assert_eq!(order.get(), 2);
        assert_eq!(outcome.completed_qpc, 900);
        let verdict = outcome.sampler_verdict().unwrap();
        assert_eq!(verdict.audited_qpc, 900);
        assert!(player_verdict_admits(&verdict, 7, 920, 20, addresses).is_ok());
    }

    #[test]
    fn failed_audit_is_also_timestamped_after_the_structural_checks_finish() {
        let words = [0u32; 8];
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: (&0usize) as *const usize,
            player_object: 2,
            transform_component_slot: (&0usize) as *const usize,
            transform_component: 3,
            component_owner_slot: (&0usize) as *const usize,
            world_transform: &words,
        };
        let order = Cell::new(0);
        let outcome = complete_player_audit(
            7,
            addresses,
            || {
                order.set(1);
                Err("chain drift".into())
            },
            || {
                assert_eq!(
                    order.get(),
                    1,
                    "the failure was published before its completion QPC"
                );
                Ok(900)
            },
        )
        .unwrap();

        assert_eq!(outcome.completed_qpc, 900);
        assert_eq!(outcome.sampler_verdict().unwrap_err(), "chain drift");
    }

    #[test]
    fn exact_cp2077_build_cannot_fall_back_to_a_camera_only_binding() {
        let mut binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
        ))
        .unwrap();
        binding.holstered_camera = None;
        let error = Cp2077PlayerSampler::new(
            depth_lab::CP2077_BUILD_20383525_EXE_SHA256,
            &binding,
            1,
            Path::new(""),
        )
        .err()
        .expect("camera-only CP2077 binding must be rejected");
        assert!(error.contains("exact holstered build"), "{error}");
    }
}
