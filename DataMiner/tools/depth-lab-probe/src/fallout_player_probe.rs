//! Reference-build Fallout 4 player-root sampler.
//!
//! This is part of the already injected depth probe, not an F4SE plugin. It is
//! strictly gated to an exact executable hash and purpose before initialization.
//! The 1.11.240 layout is validation-only; unknown builds remain rejected. No signature
//! scanning, process traversal, remote-process reads, or last-value fallback is used.

use crate::fallout_build::{FalloutCapturePurpose, resolve_fallout_build};
use crate::fallout_camera_probe::{AUDIT_VERDICT_TTL_MS, RegionProbe};
use crate::player_writer::PlayerRootSample;
use std::sync::Mutex;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
};

// F4SE 0.7.8 source commit cb39721a8155b0ec31640291f2997fa1c43474af,
// runtime 1.11.221: GameReferences.cpp / GameReferences.h / NiObjects.h.
// The same object offsets are declared by F4SE 0.7.9 commit
// 6f6a7caa9aeaebc957a70d934b6ab6b80eb768b2 for 1.11.240; relocations differ.
const TES_OBJECT_REFR_LOADED_DATA: usize = 0xF0;
const LOADED_DATA_ROOT_NODE: usize = 0x08;
const NI_AV_OBJECT_WORLD_TRANSFORM: usize = 0x70;
const NI_TRANSFORM_POSITION: usize = 0x30;
const TRANSFORM_BYTES: usize = NI_TRANSFORM_POSITION + size_of::<[f32; 3]>();
const TRANSFORM_WORDS: usize = TRANSFORM_BYTES / size_of::<u32>();
const TRANSFORM_STABLE_READ_ATTEMPTS: usize = 8;
const METRES_PER_GAME_UNIT: f64 = 0.014_287_5;
type TransformWords = [u32; TRANSFORM_WORDS];
/// 控制线程产出、渲染线程消费的 Player 链审计结论（三级链逐指针钉死）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlayerAuditVerdict {
    pub generation: u64,
    pub player: usize,
    pub loaded_data: usize,
    pub root: usize,
    pub audited_qpc: i64,
}

const PLAYER_AUDIT_PENDING: &str = "Player pointer audit has not run yet";
static PLAYER_VERDICT: Mutex<Result<PlayerAuditVerdict, &'static str>> =
    Mutex::new(Err(PLAYER_AUDIT_PENDING));

pub(crate) fn publish_player_verdict(verdict: Result<PlayerAuditVerdict, &'static str>) {
    if let Ok(mut guard) = PLAYER_VERDICT.lock() {
        *guard = verdict;
    }
}

pub(crate) fn clear_player_verdict() {
    publish_player_verdict(Err(PLAYER_AUDIT_PENDING));
}

/// 控制线程持有的审计任务；production 渲染线程不再执行任何 VirtualQuery。
pub(crate) struct PlayerAuditTask {
    player_slot: *const usize,
}

// SAFETY: 只包含钉死的模块内槽地址；audit() 只做只读查询与只读拷贝。
unsafe impl Send for PlayerAuditTask {}

impl PlayerAuditTask {
    pub(crate) fn audit(
        &self,
        generation: u64,
        now_qpc: i64,
    ) -> Result<PlayerAuditVerdict, &'static str> {
        // SAFETY: slot 在钉死的主模块映像内，构造时审计过，映像整局不卸载。
        let player = unsafe { self.player_slot.read_unaligned() };
        if player == 0 {
            return Err("Player pointer audit: PlayerCharacter is null");
        }
        let mut probe = RegionProbe::new();
        let loaded_slot = player
            .checked_add(TES_OBJECT_REFR_LOADED_DATA)
            .ok_or("Player pointer audit: LoadedData slot address overflow")?
            as *const usize;
        let loaded_data = probe
            .read_copy(loaded_slot)
            .map_err(|_| "Player pointer audit: LoadedData slot is not readable")?;
        if loaded_data == 0 {
            return Err("Player pointer audit: LoadedData is null");
        }
        let root_slot = loaded_data
            .checked_add(LOADED_DATA_ROOT_NODE)
            .ok_or("Player pointer audit: root slot address overflow")?
            as *const usize;
        let root = probe
            .read_copy(root_slot)
            .map_err(|_| "Player pointer audit: root slot is not readable")?;
        if root == 0 {
            return Err("Player pointer audit: root is null");
        }
        let transform = root
            .checked_add(NI_AV_OBJECT_WORLD_TRANSFORM)
            .ok_or("Player pointer audit: world transform address overflow")?;
        if !probe.readable(transform as *const core::ffi::c_void, TRANSFORM_BYTES) {
            return Err("Player pointer audit: world transform is not readable");
        }
        Ok(PlayerAuditVerdict {
            generation,
            player,
            loaded_data,
            root,
            audited_qpc: now_qpc,
        })
    }
}

/// 渲染线程的结论准入校验：generation、墙钟时效、链首指针一致，缺一即拒。
pub(crate) fn player_verdict_admits(
    verdict: &PlayerAuditVerdict,
    generation: u64,
    now_qpc: i64,
    ttl_qpc: i64,
    player: usize,
) -> Result<(), String> {
    if verdict.generation != generation {
        return Err(format!(
            "Player audit verdict generation mismatch: verdict={} active={generation}",
            verdict.generation
        ));
    }
    if now_qpc < verdict.audited_qpc || now_qpc - verdict.audited_qpc > ttl_qpc {
        return Err("Player audit verdict expired before this Present".into());
    }
    if verdict.player != player {
        return Err("Fallout PlayerCharacter changed since the last background audit".into());
    }
    Ok(())
}

pub struct FalloutPlayerSampler {
    player_slot: *const usize,
    verdict: Result<PlayerAuditVerdict, &'static str>,
    ttl_qpc: i64,
}

// The address is immutable module storage. Samples happen on the render thread.
unsafe impl Send for FalloutPlayerSampler {}
unsafe impl Sync for FalloutPlayerSampler {}

impl FalloutPlayerSampler {
    pub fn new(
        actual_executable_sha256: &str,
        purpose: FalloutCapturePurpose,
    ) -> Result<Self, String> {
        let build = resolve_fallout_build(actual_executable_sha256, purpose)?;
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("locate Fallout4 module failed: {error}"))?;
        let player_slot = (module.0 as usize)
            .checked_add(build.player_singleton_relocation)
            .ok_or("Fallout player slot address overflow")?
            as *const usize;
        if !readable(player_slot.cast(), size_of::<usize>()) {
            return Err("Fallout g_player relocation is not readable".into());
        }
        let mut frequency = 0i64;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|error| format!("query player audit QPC frequency failed: {error}"))?;
        if frequency <= 0 {
            return Err("player audit QPC frequency is invalid".into());
        }
        Ok(Self {
            player_slot,
            verdict: Err(PLAYER_AUDIT_PENDING),
            ttl_qpc: frequency.saturating_mul(AUDIT_VERDICT_TTL_MS) / 1_000,
        })
    }

    /// 交给控制线程的审计任务；只携带钉死的槽地址。
    #[must_use]
    pub(crate) fn audit_task(&self) -> PlayerAuditTask {
        PlayerAuditTask {
            player_slot: self.player_slot,
        }
    }

    pub fn sample(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        generation: u64,
    ) -> Result<PlayerRootSample, String> {
        if present_qpc <= 0 {
            return Err("Fallout player Present QPC is invalid".into());
        }
        // The relocation slot is immutable module storage and was audited during construction.
        let player = unsafe { self.player_slot.read_unaligned() };
        if player == 0 {
            return Err("Fallout PlayerCharacter is null".into());
        }
        // 渲染线程零 VirtualQuery：可读性由控制线程的周期审计结论担保，
        // 三重校验（generation/时效/逐级指针一致）任一不过即写诊断行。
        if let Ok(guard) = PLAYER_VERDICT.try_lock() {
            self.verdict = *guard;
        }
        let verdict = self.verdict?;
        player_verdict_admits(&verdict, generation, present_qpc, self.ttl_qpc, player)?;
        let loaded_slot = offset(player, TES_OBJECT_REFR_LOADED_DATA)?.cast::<usize>();
        // SAFETY: verdict 担保链上各对象在时效窗口内经过完整可读性审计。
        let loaded_data = unsafe { loaded_slot.read_unaligned() };
        if loaded_data == 0 {
            return Err("Fallout PlayerCharacter LoadedData is null".into());
        }
        if loaded_data != verdict.loaded_data {
            return Err("Fallout player LoadedData changed since the last background audit".into());
        }
        let root_slot = offset(loaded_data, LOADED_DATA_ROOT_NODE)?.cast::<usize>();
        // SAFETY: 同上，loaded_data 与审计结论逐位一致。
        let root = unsafe { root_slot.read_unaligned() };
        if root == 0 {
            return Err("Fallout third-person actor root is null".into());
        }
        if root != verdict.root {
            return Err("Fallout actor root changed since the last background audit".into());
        }
        let transform = offset(root, NI_AV_OBJECT_WORLD_TRANSFORM)?;
        let (source_rotation, raw_position) = read_stable_actor_transform(transform)?;
        let target_rotation = swap_y_z_basis(source_rotation);
        validate_rotation(target_rotation)?;
        let quaternion = depth_lab::camera::mat3_to_quat(&target_rotation);
        let norm = quaternion.iter().map(|value| value * value).sum::<f32>();
        if !norm.is_finite() || (norm - 1.0).abs() > 0.002 {
            return Err("Fallout actor-root quaternion is not normalized".into());
        }
        Ok(PlayerRootSample {
            present_seq,
            present_qpc,
            kcd2_root_source: None,
            cp2077_camera_join: None,
            player_position: [
                f64::from(raw_position[0]) * METRES_PER_GAME_UNIT,
                f64::from(raw_position[2]) * METRES_PER_GAME_UNIT,
                f64::from(raw_position[1]) * METRES_PER_GAME_UNIT,
            ],
            player_rotation_quaternion: quaternion.map(f64::from),
        })
    }
}

fn read_stable_actor_transform(transform: *const u8) -> Result<([[f32; 3]; 3], [f32; 3]), String> {
    if !(transform as usize).is_multiple_of(align_of::<u32>()) {
        return Err("Fallout actor-root transform is not u32-aligned".into());
    }
    read_stable_actor_transform_with(|| {
        // SAFETY: the background pointer audit proves the complete Transform range remains
        // readable for this generation. Volatile word reads are intentional: Fallout updates
        // this memory from another game thread, so two independent snapshots must not be merged
        // by the compiler. A snapshot is accepted only after two consecutive bit-identical reads.
        std::array::from_fn(|index| unsafe {
            transform
                .add(index * size_of::<u32>())
                .cast::<u32>()
                .read_volatile()
        })
    })
}

fn read_stable_actor_transform_with(
    mut read: impl FnMut() -> TransformWords,
) -> Result<([[f32; 3]; 3], [f32; 3]), String> {
    let mut previous = read();
    let mut last_validation_error = decode_actor_transform(&previous).err();
    for _ in 1..TRANSFORM_STABLE_READ_ATTEMPTS {
        let current = read();
        if current == previous {
            match decode_actor_transform(&current) {
                Ok(transform) => return Ok(transform),
                Err(error) => last_validation_error = Some(error),
            }
        }
        previous = current;
    }
    let detail = last_validation_error
        .unwrap_or_else(|| "Fallout actor-root transform changed during every bounded read".into());
    Err(format!(
        "Fallout actor-root transform did not produce a stable valid snapshot after {TRANSFORM_STABLE_READ_ATTEMPTS} reads: {detail}"
    ))
}

fn decode_actor_transform(words: &TransformWords) -> Result<([[f32; 3]; 3], [f32; 3]), String> {
    let values = words.map(f32::from_bits);
    // NiTransform stores three padded columns of the actor-to-world rotation.
    // Reconstruct rows before basis/quaternion conversion; reading blocks as rows
    // would emit the inverse rotation (native evidence: player-rotation-evidence.md).
    let rotation = [
        [values[0], values[4], values[8]],
        [values[1], values[5], values[9]],
        [values[2], values[6], values[10]],
    ];
    validate_rotation(rotation)?;
    let position = [values[12], values[13], values[14]];
    if !position.iter().all(|value| value.is_finite()) {
        return Err("Fallout actor-root position contains a non-finite value".into());
    }
    Ok((rotation, position))
}

fn offset(base: usize, bytes: usize) -> Result<*const u8, String> {
    base.checked_add(bytes)
        .map(|address| address as *const u8)
        .ok_or_else(|| "Fallout object address overflow".into())
}

fn readable(ptr: *const core::ffi::c_void, len: usize) -> bool {
    if ptr.is_null() || len == 0 {
        return false;
    }
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let queried =
        unsafe { VirtualQuery(Some(ptr), &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    if queried != size_of::<MEMORY_BASIC_INFORMATION>()
        || info.State != MEM_COMMIT
        || info.Protect.contains(PAGE_GUARD)
        || info.Protect.contains(PAGE_NOACCESS)
    {
        return false;
    }
    let start = ptr as usize;
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let region_start = info.BaseAddress as usize;
    region_start
        .checked_add(info.RegionSize)
        .is_some_and(|region_end| start >= region_start && end <= region_end)
}

fn swap_y_z_basis(source: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    // S * R * S^T, with S swapping Y/Z between Fallout and DataMiner axes.
    let order = [0usize, 2, 1];
    std::array::from_fn(|row| std::array::from_fn(|column| source[order[row]][order[column]]))
}

fn validate_rotation(rotation: [[f32; 3]; 3]) -> Result<(), String> {
    if !rotation.iter().flatten().all(|value| value.is_finite()) {
        return Err("Fallout actor-root rotation contains a non-finite value".into());
    }
    for row in &rotation {
        let norm = row.iter().map(|value| value * value).sum::<f32>();
        if (norm - 1.0).abs() > 0.01 {
            return Err("Fallout actor-root rotation row is not unit length".into());
        }
    }
    for a in 0..3 {
        for b in (a + 1)..3 {
            let dot = (0..3)
                .map(|column| rotation[a][column] * rotation[b][column])
                .sum::<f32>();
            if dot.abs() > 0.01 {
                return Err("Fallout actor-root rotation rows are not orthogonal".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_build_is_rejected_before_module_access() {
        let error = FalloutPlayerSampler::new(
            &format!("sha256:{}", "f".repeat(64)),
            FalloutCapturePurpose::Production,
        )
        .err()
        .expect("unknown build must fail before accessing game memory");
        assert!(error.contains("exact-build mismatch"), "{error}");
    }

    #[test]
    fn pointer_audit_reports_the_failed_player_chain_stage_and_recovers() {
        let mut slot = Box::new(0usize);
        let task = PlayerAuditTask {
            player_slot: &*slot,
        };
        assert_eq!(
            task.audit(7, 100),
            Err("Player pointer audit: PlayerCharacter is null")
        );
        *slot = 1;
        assert_eq!(
            task.audit(7, 100),
            Err("Player pointer audit: LoadedData slot is not readable")
        );
        let mut player = [0usize; 32];
        *slot = player.as_ptr() as usize;
        assert_eq!(
            task.audit(7, 100),
            Err("Player pointer audit: LoadedData is null")
        );
        player[TES_OBJECT_REFR_LOADED_DATA / size_of::<usize>()] = 1;
        assert_eq!(
            task.audit(7, 100),
            Err("Player pointer audit: root slot is not readable")
        );
        let mut loaded = [0usize; 2];
        player[TES_OBJECT_REFR_LOADED_DATA / size_of::<usize>()] = loaded.as_ptr() as usize;
        assert_eq!(
            task.audit(7, 100),
            Err("Player pointer audit: root is null")
        );
        loaded[LOADED_DATA_ROOT_NODE / size_of::<usize>()] = 1;
        assert_eq!(
            task.audit(7, 100),
            Err("Player pointer audit: world transform is not readable")
        );
        let root = [0u8; NI_AV_OBJECT_WORLD_TRANSFORM + TRANSFORM_BYTES];
        loaded[LOADED_DATA_ROOT_NODE / size_of::<usize>()] = root.as_ptr() as usize;
        let verdict = task.audit(7, 100).unwrap();
        assert_eq!(
            verdict,
            PlayerAuditVerdict {
                generation: 7,
                player: player.as_ptr() as usize,
                loaded_data: loaded.as_ptr() as usize,
                root: root.as_ptr() as usize,
                audited_qpc: 100,
            }
        );
        player[TES_OBJECT_REFR_LOADED_DATA / size_of::<usize>()] = 0;
        assert_eq!(player[TES_OBJECT_REFR_LOADED_DATA / size_of::<usize>()], 0);
        assert!(
            task.audit(7, 200).is_err(),
            "a good audit must not mask a later broken chain"
        );
        *slot = usize::MAX;
        assert_eq!(
            task.audit(7, 300),
            Err("Player pointer audit: LoadedData slot address overflow")
        );
    }

    fn transform_words(columns: [[f32; 3]; 3], position: [f32; 3]) -> TransformWords {
        let values = [
            columns[0][0],
            columns[0][1],
            columns[0][2],
            0.0,
            columns[1][0],
            columns[1][1],
            columns[1][2],
            0.0,
            columns[2][0],
            columns[2][1],
            columns[2][2],
            0.0,
            position[0],
            position[1],
            position[2],
        ];
        values.map(f32::to_bits)
    }

    fn rotate_vector(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let xyz = [q[0], q[1], q[2]];
        let uv = cross(xyz, v);
        let uuv = cross(xyz, uv);
        std::array::from_fn(|i| v[i] + 2.0 * (q[3] * uv[i] + uuv[i]))
    }

    #[test]
    fn actor_column_blocks_match_native_transform_for_each_axis_and_compound_rotation() {
        // Native 1.11.240 composition transforms a vector as x*block0 + y*block1 +
        // z*block2 (see authoring/1.11.240.0/player-rotation-evidence.md). This oracle
        // is independent of the decoder's matrix indexing and quaternion conversion.
        let cases = [
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [[0.0, 1.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]],
            [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
            [[0.36, 0.48, -0.8], [-0.8, 0.6, 0.0], [0.48, 0.64, 0.6]],
        ];
        let swap = |v: [f32; 3]| [v[0], v[2], v[1]];
        for columns in cases {
            let position = [12.0, -3.0, 4.0];
            let mut words = transform_words(columns, position);
            // Padding is not a fourth rotation component. Ignore it even when non-finite.
            for i in [3, 7, 11] {
                words[i] = f32::NAN.to_bits();
            }
            let (rotation, decoded_position) = read_stable_actor_transform_with(|| words).unwrap();
            assert_eq!(decoded_position, position);
            let q = depth_lab::camera::mat3_to_quat(&swap_y_z_basis(rotation));
            for local in [
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 0.0, -1.0],
                [0.2, -0.7, 1.3],
            ] {
                let game_local = swap(local);
                let native_world =
                    std::array::from_fn(|i| (0..3).map(|k| columns[k][i] * game_local[k]).sum());
                let expected = swap(native_world);
                let actual = rotate_vector(q, local);
                for i in 0..3 {
                    assert!(
                        (actual[i] - expected[i]).abs() < 1e-5,
                        "columns={columns:?}, local={local:?}, actual={actual:?}, expected={expected:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn basis_conversion_preserves_identity() {
        let identity = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert_eq!(swap_y_z_basis(identity), identity);
        validate_rotation(identity).unwrap();
    }

    #[test]
    fn basis_conversion_swaps_y_and_z_rotation_axes() {
        let source = [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        let target = swap_y_z_basis(source);
        assert_eq!(target, [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]);
        validate_rotation(target).unwrap();
    }

    #[test]
    fn player_verdict_admission_is_fail_closed() {
        let verdict = PlayerAuditVerdict {
            generation: 3,
            player: 0x100,
            loaded_data: 0x200,
            root: 0x300,
            audited_qpc: 5_000,
        };
        let ttl = 1_000i64;
        assert!(player_verdict_admits(&verdict, 3, 5_500, ttl, 0x100).is_ok());
        assert!(player_verdict_admits(&verdict, 4, 5_500, ttl, 0x100).is_err());
        assert!(player_verdict_admits(&verdict, 3, 6_001, ttl, 0x100).is_err());
        assert!(player_verdict_admits(&verdict, 3, 4_999, ttl, 0x100).is_err());
        assert!(player_verdict_admits(&verdict, 3, 5_500, ttl, 0x999).is_err());
    }

    #[test]
    fn stable_transform_read_recovers_from_one_torn_matrix() {
        let torn = transform_words(
            [[1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            [1.0, 2.0, 3.0],
        );
        let stable = transform_words(
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [4.0, 5.0, 6.0],
        );
        let mut reads = [torn, stable, stable].into_iter();

        let (rotation, position) =
            read_stable_actor_transform_with(|| reads.next().unwrap_or(stable)).unwrap();

        assert_eq!(
            rotation,
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
        assert_eq!(position, [4.0, 5.0, 6.0]);
    }

    #[test]
    fn stable_transform_read_still_rejects_a_persistently_invalid_matrix() {
        let invalid = transform_words(
            [[1.0, 1.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            [1.0, 2.0, 3.0],
        );

        let error = read_stable_actor_transform_with(|| invalid).unwrap_err();

        assert!(error.contains("after 8 reads"), "{error}");
        assert!(error.contains("rows are not orthogonal"), "{error}");
    }
}
