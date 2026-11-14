//! Reference-build Fallout 4 PlayerCamera root sampler.
//!
//! The already injected probe reads the authoritative in-process camera root
//! at Present. Object offsets are shared by the audited 1.11.221 layout and the
//! 1.11.240 authoring candidate; singleton relocations are selected by exact EXE
//! hash. Candidate builds require non-deliverable validation mode. Pointer
//! auditing remains fail-closed and stale-value fallback is never permitted.

use crate::fallout_build::{FalloutBuild, FalloutCapturePurpose, resolve_fallout_build};
use std::sync::Mutex;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
};

pub const PROVIDER: &str = "fallout4-player-camera-root-exact-build/v1";
pub const CAMERA_ROOT_OFFSET: usize = 0x20;
pub const WORLD_TRANSFORM_OFFSET: usize = 0x70;
const TRANSFORM_POSITION_OFFSET: usize = 0x30;
const TRANSFORM_BYTES: usize = TRANSFORM_POSITION_OFFSET + size_of::<[f32; 3]>();
/// 后台审计结论的墙钟时效。#504 两轮遥测实证：渲染线程上的 VirtualQuery 会被
/// Fallout 4 自己的内存活动（VAD 锁竞争）拖到毫秒级——错相后单链审计中位仍
/// 1.2–1.7 ms、p95 3.0 ms、慢帧风险 5.4×。因此周期审计整体搬到控制线程
/// （约 100 ms 一轮），渲染线程只消费结论；结论超过本时效即 fail-closed。
///
/// 语义从「≤59 Presents」改为墙钟：审计防的是页面被 decommit（墙钟事件），
/// 350 ms ≈ 3.5 个审计周期，调度抖动不会造成无辜的诊断行。
pub(crate) const AUDIT_VERDICT_TTL_MS: i64 = 350;

/// 控制线程产出、渲染线程消费的 Camera 链审计结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CameraAuditVerdict {
    pub generation: u64,
    pub camera: usize,
    pub root: usize,
    pub audited_qpc: i64,
}

const CAMERA_AUDIT_PENDING: &str = "Camera pointer audit has not run yet";
static CAMERA_VERDICT: Mutex<Result<CameraAuditVerdict, &'static str>> =
    Mutex::new(Err(CAMERA_AUDIT_PENDING));

pub(crate) fn publish_camera_verdict(verdict: Result<CameraAuditVerdict, &'static str>) {
    if let Ok(mut guard) = CAMERA_VERDICT.lock() {
        *guard = verdict;
    }
}

pub(crate) fn clear_camera_verdict() {
    publish_camera_verdict(Err(CAMERA_AUDIT_PENDING));
}

/// 控制线程持有的审计任务——production 期**唯一**执行 VirtualQuery 的地方。
/// slot 地址是钉死的模块内存，跨线程只读安全。
pub(crate) struct CameraAuditTask {
    camera_slot: *const usize,
}

// SAFETY: 只包含模块内静态槽地址；audit() 只做只读查询与只读拷贝。
unsafe impl Send for CameraAuditTask {}

impl CameraAuditTask {
    /// 走一遍链并验证每级可读性。失败原因与结论一起发布，不沿用旧结论，也不
    /// 把不可读/空指针伪装成「后台审计尚未运行」。不在错误里暴露进程地址。
    pub(crate) fn audit(
        &self,
        generation: u64,
        now_qpc: i64,
    ) -> Result<CameraAuditVerdict, &'static str> {
        // SAFETY: slot 在钉死的主模块映像内，构造时审计过，映像整局不卸载。
        let camera = unsafe { self.camera_slot.read_unaligned() };
        if camera == 0 {
            return Err("Camera pointer audit: PlayerCamera is null");
        }
        let mut probe = RegionProbe::new();
        let root_slot = camera
            .checked_add(CAMERA_ROOT_OFFSET)
            .ok_or("Camera pointer audit: cameraRoot slot address overflow")?
            as *const usize;
        let root = probe
            .read_copy(root_slot)
            .map_err(|_| "Camera pointer audit: cameraRoot slot is not readable")?;
        if root == 0 {
            return Err("Camera pointer audit: cameraRoot is null");
        }
        let transform = root
            .checked_add(WORLD_TRANSFORM_OFFSET)
            .ok_or("Camera pointer audit: world transform address overflow")?;
        if !probe.readable(transform as *const core::ffi::c_void, TRANSFORM_BYTES) {
            return Err("Camera pointer audit: world transform is not readable");
        }
        Ok(CameraAuditVerdict {
            generation,
            camera,
            root,
            audited_qpc: now_qpc,
        })
    }
}

/// 渲染线程的结论准入校验：generation、墙钟时效、链首指针一致，缺一即拒。
pub(crate) fn camera_verdict_admits(
    verdict: &CameraAuditVerdict,
    generation: u64,
    now_qpc: i64,
    ttl_qpc: i64,
    camera: usize,
) -> Result<(), String> {
    if verdict.generation != generation {
        return Err(format!(
            "Camera audit verdict generation mismatch: verdict={} active={generation}",
            verdict.generation
        ));
    }
    if now_qpc < verdict.audited_qpc || now_qpc - verdict.audited_qpc > ttl_qpc {
        return Err("Camera audit verdict expired before this Present".into());
    }
    if verdict.camera != camera {
        return Err("Fallout PlayerCamera changed since the last background audit".into());
    }
    Ok(())
}

/// 一次指针审计内共享的 region 判定：同一个 `VirtualQuery` region 只查一次。
/// Creation Engine 用自有分配器在大段里细分对象，链上相邻对象落同一 region 的
/// 概率很高。缓存**不跨审计周期**——周期审计的意义就是重新验证。
pub(crate) struct RegionProbe {
    region: Option<(usize, usize)>,
    pub(crate) queries: u32,
}

impl RegionProbe {
    pub(crate) const fn new() -> Self {
        Self {
            region: None,
            queries: 0,
        }
    }

    pub(crate) fn readable(&mut self, ptr: *const core::ffi::c_void, len: usize) -> bool {
        if ptr.is_null() || len == 0 {
            return false;
        }
        let start = ptr as usize;
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        if let Some((base, limit)) = self.region
            && start >= base
            && end <= limit
        {
            return true;
        }
        // 与自由函数 readable() 同一判定（单 region、MEM_COMMIT、非 GUARD/NOACCESS），
        // 只是把 region 边界缓存下来给同一次审计里的后续指针复用。
        self.queries += 1;
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
        let base = info.BaseAddress as usize;
        let Some(limit) = base.checked_add(info.RegionSize) else {
            return false;
        };
        if start < base || end > limit {
            return false;
        }
        self.region = Some((base, limit));
        true
    }

    /// 审计路径的 `read_copy`：可读性判定走共享 region 缓存。
    pub(crate) fn read_copy<T: Copy>(&mut self, ptr: *const T) -> Result<T, String> {
        if !self.readable(ptr.cast(), size_of::<T>()) {
            return Err(format!("Fallout camera memory is not readable at {ptr:p}"));
        }
        // SAFETY: 上面刚确认该跨度落在单个 MEM_COMMIT 可读 region 内。
        Ok(unsafe { ptr.read_unaligned() })
    }
}
pub const METRES_PER_GAME_UNIT: f64 = 0.014_287_5;
pub const AXIS_MAPPING: &str = "x_z_y";

#[derive(Clone, Debug)]
pub struct FalloutCameraSample {
    pub present_seq: u64,
    pub present_qpc: i64,
    pub camera_position: [f64; 3],
    pub camera_rotation_quaternion: [f64; 4],
}

pub struct FalloutCameraSampler {
    camera_slot: *const usize,
    /// 后台结论的渲染线程本地副本：全局槽 try_lock 失败时沿用，
    /// generation/时效/指针一致三重校验兜底，绝不在渲染线程等锁。
    verdict: Result<CameraAuditVerdict, &'static str>,
    ttl_qpc: i64,
}

// The slot address is immutable module storage and samples run on the render thread.
unsafe impl Send for FalloutCameraSampler {}
unsafe impl Sync for FalloutCameraSampler {}

impl FalloutCameraSampler {
    pub fn new(
        actual_executable_sha256: &str,
        binding: &depth_lab::GameCameraBinding,
        purpose: FalloutCapturePurpose,
    ) -> Result<Self, String> {
        let build = resolve_fallout_build(actual_executable_sha256, purpose)?;
        validate_binding(binding, build)?;
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("locate Fallout4 module failed: {error}"))?;
        let camera_slot = (module.0 as usize)
            .checked_add(build.camera_singleton_relocation)
            .ok_or("Fallout PlayerCamera slot address overflow")?
            as *const usize;
        if !readable(camera_slot.cast(), size_of::<usize>()) {
            return Err("Fallout g_playerCamera relocation is not readable".into());
        }
        let mut frequency = 0i64;
        unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
            .map_err(|error| format!("query camera audit QPC frequency failed: {error}"))?;
        if frequency <= 0 {
            return Err("camera audit QPC frequency is invalid".into());
        }
        Ok(Self {
            camera_slot,
            verdict: Err(CAMERA_AUDIT_PENDING),
            ttl_qpc: frequency.saturating_mul(AUDIT_VERDICT_TTL_MS) / 1_000,
        })
    }

    /// 交给控制线程的审计任务；只携带钉死的槽地址。
    #[must_use]
    pub(crate) fn audit_task(&self) -> CameraAuditTask {
        CameraAuditTask {
            camera_slot: self.camera_slot,
        }
    }

    pub fn sample(
        &mut self,
        present_seq: u64,
        present_qpc: i64,
        generation: u64,
    ) -> Result<FalloutCameraSample, String> {
        if present_qpc <= 0 {
            return Err("Fallout camera Present QPC is invalid".into());
        }
        // The slot lives in the pinned main executable for the process lifetime and was audited
        // during construction. Reading its current value is enough to detect camera replacement.
        let camera = unsafe { self.camera_slot.read_unaligned() };
        if camera == 0 {
            return Err("Fallout PlayerCamera is null".into());
        }
        // 渲染线程零 VirtualQuery：可读性由控制线程的周期审计结论担保。
        // try_lock 失败沿用本地副本——下面的三重校验（generation/时效/指针一致）
        // 保证过期或跨代的副本一律 fail-closed，宁写诊断行不冒读。
        if let Ok(guard) = CAMERA_VERDICT.try_lock() {
            self.verdict = *guard;
        }
        let verdict = self.verdict?;
        camera_verdict_admits(&verdict, generation, present_qpc, self.ttl_qpc, camera)?;
        let root_slot = offset(camera, CAMERA_ROOT_OFFSET)?.cast::<usize>();
        // SAFETY: verdict 担保 camera 对象在时效窗口内经过完整可读性审计。
        let root = unsafe { root_slot.read_unaligned() };
        if root == 0 {
            return Err("Fallout PlayerCamera cameraRoot is null".into());
        }
        if root != verdict.root {
            return Err("Fallout camera root changed since the last background audit".into());
        }
        let transform = offset(root, WORLD_TRANSFORM_OFFSET)?;
        // The complete transform span was audited by the control thread; the verdict above is
        // generation/pointer/TTL checked. Both arrays still come from this exact Present.
        let raw_rotation = unsafe { transform.cast::<[f32; 12]>().read_unaligned() };
        let raw_position = unsafe {
            offset(transform as usize, TRANSFORM_POSITION_OFFSET)?
                .cast::<[f32; 3]>()
                .read_unaligned()
        };
        let source_rotation = [
            [raw_rotation[0], raw_rotation[1], raw_rotation[2]],
            [raw_rotation[4], raw_rotation[5], raw_rotation[6]],
            [raw_rotation[8], raw_rotation[9], raw_rotation[10]],
        ];
        validate_rotation(source_rotation)?;
        let world_to_camera = swap_y_z_basis(source_rotation);
        validate_rotation(world_to_camera)?;
        let camera_to_world = transpose_rotation(world_to_camera);
        let quaternion = depth_lab::camera::mat3_to_quat(&camera_to_world);
        let norm = quaternion.iter().map(|value| value * value).sum::<f32>();
        if !norm.is_finite() || (norm - 1.0).abs() > 0.002 {
            return Err("Fallout camera-root quaternion is not normalized".into());
        }
        Ok(FalloutCameraSample {
            present_seq,
            present_qpc,
            camera_position: [
                f64::from(raw_position[0]) * METRES_PER_GAME_UNIT,
                f64::from(raw_position[2]) * METRES_PER_GAME_UNIT,
                f64::from(raw_position[1]) * METRES_PER_GAME_UNIT,
            ],
            camera_rotation_quaternion: quaternion.map(f64::from),
        })
    }
}

fn validate_binding(
    binding: &depth_lab::GameCameraBinding,
    build: FalloutBuild,
) -> Result<(), String> {
    let expected_scale = METRES_PER_GAME_UNIT.to_string();
    if binding.provider != PROVIDER
        || binding.camera_singleton_relocation != build.camera_singleton_relocation as u64
        || binding.camera_root_offset != CAMERA_ROOT_OFFSET as u32
        || binding.world_transform_offset != WORLD_TRANSFORM_OFFSET as u32
        || binding.metres_per_game_unit != expected_scale
        || binding.axis_mapping != AXIS_MAPPING
    {
        return Err(
            "Fallout game-camera binding does not match the audited exact-build sampler".into(),
        );
    }
    Ok(())
}

fn offset(base: usize, bytes: usize) -> Result<*const u8, String> {
    base.checked_add(bytes)
        .map(|address| address as *const u8)
        .ok_or_else(|| "Fallout camera object address overflow".into())
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
    let order = [0usize, 2, 1];
    std::array::from_fn(|row| std::array::from_fn(|column| source[order[row]][order[column]]))
}

fn transpose_rotation(source: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    std::array::from_fn(|row| std::array::from_fn(|column| source[column][row]))
}

fn validate_rotation(rotation: [[f32; 3]; 3]) -> Result<(), String> {
    if !rotation.iter().flatten().all(|value| value.is_finite()) {
        return Err("Fallout camera-root rotation contains a non-finite value".into());
    }
    for row in &rotation {
        let norm = row.iter().map(|value| value * value).sum::<f32>();
        if (norm - 1.0).abs() > 0.01 {
            return Err("Fallout camera-root rotation row is not unit length".into());
        }
    }
    for a in 0..3 {
        for b in (a + 1)..3 {
            let dot = (0..3)
                .map(|column| rotation[a][column] * rotation[b][column])
                .sum::<f32>();
            if dot.abs() > 0.01 {
                return Err("Fallout camera-root rotation rows are not orthogonal".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_build_is_rejected_before_binding_or_module_access() {
        let mut binding: depth_lab::CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/fallout4-steam-release-test/camera-binding.json"
        ))
        .unwrap();
        let game_camera = binding.game_camera.as_mut().unwrap();
        game_camera.camera_singleton_relocation = u64::MAX;
        let error = FalloutCameraSampler::new(
            &format!("sha256:{}", "f".repeat(64)),
            game_camera,
            FalloutCapturePurpose::Production,
        )
        .err()
        .expect("unknown build must fail before accessing game memory");
        assert!(error.contains("exact-build mismatch"), "{error}");
    }

    #[test]
    fn pointer_audit_reports_null_unreadable_and_successful_camera_chain() {
        let mut slot = Box::new(0usize);
        let task = CameraAuditTask {
            camera_slot: &*slot,
        };
        assert_eq!(
            task.audit(7, 100),
            Err("Camera pointer audit: PlayerCamera is null")
        );
        *slot = 1;
        assert_eq!(
            task.audit(7, 100),
            Err("Camera pointer audit: cameraRoot slot is not readable")
        );
        let mut camera = [0usize; 8];
        *slot = camera.as_ptr() as usize;
        assert_eq!(
            task.audit(7, 100),
            Err("Camera pointer audit: cameraRoot is null")
        );
        camera[CAMERA_ROOT_OFFSET / size_of::<usize>()] = 1;
        assert_eq!(
            task.audit(7, 100),
            Err("Camera pointer audit: world transform is not readable")
        );
        let root = [0u8; WORLD_TRANSFORM_OFFSET + TRANSFORM_BYTES];
        camera[CAMERA_ROOT_OFFSET / size_of::<usize>()] = root.as_ptr() as usize;
        let verdict = task.audit(7, 100).unwrap();
        assert_eq!(
            verdict,
            CameraAuditVerdict {
                generation: 7,
                camera: camera.as_ptr() as usize,
                root: root.as_ptr() as usize,
                audited_qpc: 100,
            }
        );
        camera[CAMERA_ROOT_OFFSET / size_of::<usize>()] = 0;
        assert_eq!(camera[CAMERA_ROOT_OFFSET / size_of::<usize>()], 0);
        assert!(
            task.audit(7, 200).is_err(),
            "a good audit must not mask a later broken chain"
        );
        *slot = usize::MAX;
        assert_eq!(
            task.audit(7, 300),
            Err("Camera pointer audit: cameraRoot slot address overflow")
        );
    }

    #[test]
    fn exact_binding_constants_are_stable() {
        let build = crate::fallout_build::FALLOUT4_1_11_221;
        validate_binding(
            &depth_lab::GameCameraBinding {
                provider: PROVIDER.into(),
                camera_singleton_relocation: build.camera_singleton_relocation as u64,
                camera_root_offset: CAMERA_ROOT_OFFSET as u32,
                world_transform_offset: WORLD_TRANSFORM_OFFSET as u32,
                metres_per_game_unit: METRES_PER_GAME_UNIT.to_string(),
                axis_mapping: AXIS_MAPPING.into(),
            },
            build,
        )
        .unwrap();
    }

    #[test]
    fn candidate_binding_cannot_mix_old_and_new_relocations() {
        let mut binding: depth_lab::CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/fallout4-steam-release-test/camera-binding.json"
        ))
        .unwrap();
        let camera = binding.game_camera.as_mut().unwrap();
        let old = crate::fallout_build::FALLOUT4_1_11_221;
        let candidate = crate::fallout_build::FALLOUT4_1_11_240;
        assert!(validate_binding(camera, old).is_ok());
        assert!(validate_binding(camera, candidate).is_err());
        camera.camera_singleton_relocation = candidate.camera_singleton_relocation as u64;
        assert!(validate_binding(camera, candidate).is_ok());
        assert!(validate_binding(camera, old).is_err());
        camera.camera_root_offset += 8;
        assert!(validate_binding(camera, candidate).is_err());
    }

    #[test]
    fn candidate_reference_matches_layout_but_is_not_a_production_binding() {
        let json = include_str!("../../pr405-fallout4/authoring/1.11.240.0/camera-reference.json");
        let reference: serde_json::Value = serde_json::from_str(json).unwrap();
        let candidate = crate::fallout_build::FALLOUT4_1_11_240;
        assert_eq!(reference["executable_sha256"], candidate.executable_sha256);
        assert_eq!(reference["deliverable"], false);
        let camera: depth_lab::GameCameraBinding =
            serde_json::from_value(reference["game_camera"].clone()).unwrap();
        validate_binding(&camera, candidate).unwrap();
        assert!(serde_json::from_str::<depth_lab::CameraBindingDocument>(json).is_err());
    }

    #[test]
    fn world_to_camera_basis_is_transposed_for_delivery() {
        let world_to_camera = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
        let camera_to_world = transpose_rotation(world_to_camera);
        assert_eq!(
            camera_to_world,
            [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]
        );
        let quaternion = depth_lab::camera::mat3_to_quat(&camera_to_world);
        let root_half = 0.5_f32.sqrt();
        assert!((quaternion[1] + root_half).abs() < 1e-6, "{quaternion:?}");
        assert!((quaternion[3] - root_half).abs() < 1e-6, "{quaternion:?}");
    }

    #[test]
    fn verdict_admission_is_fail_closed_on_generation_ttl_and_pointer_identity() {
        let verdict = CameraAuditVerdict {
            generation: 7,
            camera: 0x1000,
            root: 0x2000,
            audited_qpc: 1_000_000,
        };
        let ttl = 3_500_000i64; // 350 ms @ 10 MHz
        // 在时效内、代次与指针一致：放行。
        assert!(camera_verdict_admits(&verdict, 7, 1_000_000 + ttl, ttl, 0x1000).is_ok());
        // generation 不匹配（StartRun 换代 / Stop 后迟到结论）：拒。
        assert!(camera_verdict_admits(&verdict, 8, 1_500_000, ttl, 0x1000).is_err());
        // 结论过期一个 tick：拒。
        assert!(camera_verdict_admits(&verdict, 7, 1_000_001 + ttl, ttl, 0x1000).is_err());
        // QPC 回绕（结论来自未来）：拒。
        assert!(camera_verdict_admits(&verdict, 7, 999_999, ttl, 0x1000).is_err());
        // 链首指针变了（相机被替换）：拒，等控制线程追上。
        assert!(camera_verdict_admits(&verdict, 7, 1_500_000, ttl, 0x3000).is_err());
    }

    #[test]
    fn region_probe_reuses_the_same_region_and_rejects_null() {
        static SPAN: [u8; 4096] = [0u8; 4096];
        let mut probe = RegionProbe::new();
        assert!(probe.readable(SPAN.as_ptr().cast(), 64));
        assert_eq!(probe.queries, 1);
        // 同一 region 内的第二个跨度：命中缓存，不再发起 VirtualQuery。
        assert!(probe.readable(SPAN.as_ptr().wrapping_add(1024).cast(), 64));
        assert_eq!(probe.queries, 1);
        assert!(!probe.readable(std::ptr::null(), 8));
        let value = probe.read_copy(SPAN.as_ptr().cast::<u64>()).unwrap();
        assert_eq!(value, 0);
        assert_eq!(probe.queries, 1, "read_copy 也必须走共享缓存");
    }
}
