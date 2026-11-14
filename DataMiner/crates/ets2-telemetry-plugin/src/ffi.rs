//! SCS Telemetry SDK 1.14 的 C ABI 镜像。
//!
//! 逐字对应 `scs_sdk_1_14/include/` 下的头文件。**每一个尺寸断言都抄自头文件自己的
//! `scs_check_size`**,写成 Rust 的 `const` 断言——布局错了就编译不过,
//! 而不是等到游戏里读出一堆看着合理的垃圾数值。
//!
//! # 为什么手写 FFI 而不是 bindgen
//!
//! 这套头文件只有五个结构体和六个函数指针类型,而且**头文件本身带了尺寸断言**,
//! 手写加断言比引一个构建期依赖更可控;更重要的是 bindgen 要装 libclang,
//! 而本 crate 的一条硬要求是「在 macOS 上也能 check 与测试」。
//!
//! # 调用约定
//!
//! 头文件里 `SCSAPIFUNC` 在 MSVC 下是 `__stdcall`,在 GCC 下为空。
//! Rust 的 `extern "system"` 正好是这个语义:x86 Windows 上是 stdcall,
//! x64 Windows 与其余平台上退化为默认 C 约定。**不要写成 `extern "C"`**
//! ——那在 32 位 ETS2(`bin/win_x86/`)上会静默错栈。

use core::ffi::{c_char, c_void};

/// `scs_result_t`。
pub type ScsResult = i32;

pub const SCS_RESULT_OK: ScsResult = 0;
pub const SCS_RESULT_UNSUPPORTED: ScsResult = -1;
pub const SCS_RESULT_GENERIC_ERROR: ScsResult = -7;

/// `SCS_U32_NIL`——非索引通道注册时的 index 取值。
pub const SCS_U32_NIL: u32 = u32::MAX;

/// `SCS_MAKE_VERSION(major, minor)`。
#[must_use]
pub const fn make_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | (minor as u32)
}

/// 本插件支持的两个版本。1.01 相对 1.00 只增不改,参数结构体布局相同
/// (头文件:`typedef scs_telemetry_init_params_v100_t scs_telemetry_init_params_v101_t`)。
pub const SCS_TELEMETRY_VERSION_1_00: u32 = make_version(1, 0);
pub const SCS_TELEMETRY_VERSION_1_01: u32 = make_version(1, 1);

/// `scs_value_type_t` 里我们用到的几个。
pub const SCS_VALUE_TYPE_FLOAT: u32 = 5;
pub const SCS_VALUE_TYPE_FVECTOR: u32 = 7;
pub const SCS_VALUE_TYPE_FPLACEMENT: u32 = 10;
pub const SCS_VALUE_TYPE_DPLACEMENT: u32 = 11;

/// `scs_event_t`。
pub const SCS_TELEMETRY_EVENT_FRAME_START: u32 = 1;
pub const SCS_TELEMETRY_EVENT_FRAME_END: u32 = 2;
pub const SCS_TELEMETRY_EVENT_PAUSED: u32 = 3;
pub const SCS_TELEMETRY_EVENT_STARTED: u32 = 4;
pub const SCS_TELEMETRY_EVENT_CONFIGURATION: u32 = 5;

/// `SCS_TELEMETRY_CHANNEL_FLAG_none`。
///
/// 官方示例用的就是 `none`:值不变时回调不触发,存着的上一个值仍然有效。
/// 代价是**分不清「没变」与「从未上报」**,所以 [`crate::state`] 侧要单独记
/// 「这个通道到底有没有报过」——不记的话,主菜单里也会写出一条看着正常的样本。
pub const SCS_TELEMETRY_CHANNEL_FLAG_NONE: u32 = 0x0000_0000;

/// `scs_log_type_t`。
pub const SCS_LOG_TYPE_MESSAGE: i32 = 0;
pub const SCS_LOG_TYPE_ERROR: i32 = 2;

/// `scs_value_fvector_t`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScsFvector {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// `scs_value_dvector_t`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScsDvector {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// `scs_value_euler_t`,单位是**圈**(见 `ets2_player_telemetry::EulerTurns`)。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScsEuler {
    pub heading: f32,
    pub pitch: f32,
    pub roll: f32,
}

/// `scs_value_fplacement_t`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScsFplacement {
    pub position: ScsFvector,
    pub orientation: ScsEuler,
}

/// `scs_value_dplacement_t`。**末尾那个 `_padding` 是头文件里显式写的**,
/// 漏掉它结构体会短 4 字节,后面所有字段错位。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScsDplacement {
    pub position: ScsDvector,
    pub orientation: ScsEuler,
    pub _padding: u32,
}

/// `scs_value_t` 的载荷联合。最大成员是 `dplacement`(40 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
pub union ScsValuePayload {
    pub value_float: f32,
    pub value_fvector: ScsFvector,
    pub value_dvector: ScsDvector,
    pub value_euler: ScsEuler,
    pub value_fplacement: ScsFplacement,
    pub value_dplacement: ScsDplacement,
}

/// `scs_value_t`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScsValue {
    pub value_type: u32,
    pub _padding: u32,
    pub payload: ScsValuePayload,
}

/// `scs_named_value_t`，配置属性数组的单个元素。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScsNamedValue {
    pub name: *const c_char,
    pub index: u32,
    #[cfg(target_pointer_width = "64")]
    pub _padding: u32,
    pub value: ScsValue,
}

/// `scs_telemetry_configuration_t`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScsTelemetryConfiguration {
    pub id: *const c_char,
    /// 以 `name == NULL` 元素结尾的数组。
    pub attributes: *const ScsNamedValue,
}

/// `scs_telemetry_frame_start_t`。`render_time` 是我们与截帧对齐的键。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScsTelemetryFrameStart {
    pub flags: u32,
    pub _padding: u32,
    /// 微秒。计时器可能因 `SCS_TELEMETRY_FRAME_START_FLAG_timer_restart` 重置。
    pub render_time: u64,
    pub simulation_time: u64,
    pub paused_simulation_time: u64,
}

/// `SCS_TELEMETRY_FRAME_START_FLAG_timer_restart`——置位表示 `render_time` **回跳了**。
///
/// 不处理它的后果:对齐键会突然变小,下游按单调递增假设去 join 就会错配整段。
pub const SCS_TELEMETRY_FRAME_START_FLAG_TIMER_RESTART: u32 = 0x0000_0001;

pub type ScsLogFn = Option<unsafe extern "system" fn(log_type: i32, message: *const c_char)>;

/// `scs_sdk_init_params_v100_t`。
#[repr(C)]
pub struct ScsSdkInitParamsV100 {
    pub game_name: *const c_char,
    pub game_id: *const c_char,
    pub game_version: u32,
    /// 仅 64 位存在(头文件用 `#ifdef SCS_ARCHITECTURE_x64` 括起来)。
    #[cfg(target_pointer_width = "64")]
    pub _padding: u32,
    pub log: ScsLogFn,
}

pub type ScsTelemetryChannelCallback = Option<
    unsafe extern "system" fn(
        name: *const c_char,
        index: u32,
        value: *const ScsValue,
        context: *mut c_void,
    ),
>;

pub type ScsTelemetryEventCallback =
    Option<unsafe extern "system" fn(event: u32, event_info: *const c_void, context: *mut c_void)>;

pub type ScsRegisterForEventFn = Option<
    unsafe extern "system" fn(
        event: u32,
        callback: ScsTelemetryEventCallback,
        context: *mut c_void,
    ) -> ScsResult,
>;

pub type ScsUnregisterFromEventFn = Option<unsafe extern "system" fn(event: u32) -> ScsResult>;

pub type ScsRegisterForChannelFn = Option<
    unsafe extern "system" fn(
        name: *const c_char,
        index: u32,
        value_type: u32,
        flags: u32,
        callback: ScsTelemetryChannelCallback,
        context: *mut c_void,
    ) -> ScsResult,
>;

pub type ScsUnregisterFromChannelFn = Option<
    unsafe extern "system" fn(name: *const c_char, index: u32, value_type: u32) -> ScsResult,
>;

/// `scs_telemetry_init_params_v100_t`(1.01 复用同一布局)。
///
/// 头文件里它继承自一个只有成员函数、没有数据成员的空基类,
/// 按空基类优化不占空间——所以 Rust 侧直接平铺即可,尺寸断言会替我们兜住。
#[repr(C)]
pub struct ScsTelemetryInitParamsV100 {
    pub common: ScsSdkInitParamsV100,
    pub register_for_event: ScsRegisterForEventFn,
    pub unregister_from_event: ScsUnregisterFromEventFn,
    pub register_for_channel: ScsRegisterForChannelFn,
    pub unregister_from_channel: ScsUnregisterFromChannelFn,
}

// ---------------------------------------------------------------------------
// 布局断言：逐条抄自头文件自己的 scs_check_size(结构体, 32 位期望, 64 位期望)。
//
// 这些是编译期断言：布局一旦对不上，**编译直接失败**。
// 没有它们，ABI 错位的表现是「游戏里读到一堆有限但错误的数字」——
// 那正是本仓一再吃亏的那类静默错误。
// ---------------------------------------------------------------------------

const _: () = assert!(size_of::<ScsFvector>() == 12);
const _: () = assert!(size_of::<ScsDvector>() == 24);
const _: () = assert!(size_of::<ScsEuler>() == 12);
const _: () = assert!(size_of::<ScsFplacement>() == 24);
const _: () = assert!(size_of::<ScsDplacement>() == 40);
/// `scs_check_size(scs_value_t, 48, 48)`——两种架构下都是 48。
const _: () = assert!(size_of::<ScsValue>() == 48);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<ScsNamedValue>() == 64);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<ScsNamedValue>() == 56);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<ScsTelemetryConfiguration>() == 16);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<ScsTelemetryConfiguration>() == 8);
/// `scs_check_size(scs_telemetry_frame_start_t, 32, 32)`。
const _: () = assert!(size_of::<ScsTelemetryFrameStart>() == 32);

#[cfg(target_pointer_width = "64")]
const _: () = {
    // scs_check_size(scs_sdk_init_params_v100_t, 16, 32)
    assert!(size_of::<ScsSdkInitParamsV100>() == 32);
    // scs_check_size(scs_telemetry_init_params_v100_t, 32, 64)
    assert!(size_of::<ScsTelemetryInitParamsV100>() == 64);
};

#[cfg(target_pointer_width = "32")]
const _: () = {
    assert!(size_of::<ScsSdkInitParamsV100>() == 16);
    assert!(size_of::<ScsTelemetryInitParamsV100>() == 32);
};

impl ScsValue {
    /// 按类型读 `fplacement`。
    ///
    /// # Safety
    ///
    /// 同 [`Self::as_dplacement`]。
    #[must_use]
    pub unsafe fn as_fplacement(&self) -> Option<ScsFplacement> {
        if self.value_type == SCS_VALUE_TYPE_FPLACEMENT {
            Some(unsafe { self.payload.value_fplacement })
        } else {
            None
        }
    }

    /// 按类型读 `dplacement`。**类型不符返回 `None`,绝不强读联合。**
    ///
    /// # Safety
    ///
    /// 调用方必须保证 `self` 来自引擎的回调参数(即联合成员与 `value_type` 一致)。
    /// 本函数只负责在类型标签对不上时拒绝,不能挽救一个本身就撒谎的 `value_type`。
    #[must_use]
    pub unsafe fn as_dplacement(&self) -> Option<ScsDplacement> {
        // 刻意写成 if 而不是 `.then_some(...)`：后者会**先**读联合再判类型，
        // 恰好破坏这三个函数存在的唯一理由——先验标签，再碰载荷。
        if self.value_type == SCS_VALUE_TYPE_DPLACEMENT {
            Some(unsafe { self.payload.value_dplacement })
        } else {
            None
        }
    }

    /// 按类型读 `fvector`。
    ///
    /// # Safety
    ///
    /// 同 [`Self::as_dplacement`]。
    #[must_use]
    pub unsafe fn as_fvector(&self) -> Option<ScsFvector> {
        if self.value_type == SCS_VALUE_TYPE_FVECTOR {
            Some(unsafe { self.payload.value_fvector })
        } else {
            None
        }
    }

    /// 按类型读 `float`。
    ///
    /// # Safety
    ///
    /// 同 [`Self::as_dplacement`]。
    #[must_use]
    pub unsafe fn as_float(&self) -> Option<f32> {
        if self.value_type == SCS_VALUE_TYPE_FLOAT {
            Some(unsafe { self.payload.value_float })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本号编码必须与头文件的 `SCS_MAKE_VERSION` 一致。
    /// 弄错的表现是 `scs_telemetry_init` 永远走 unsupported 分支,插件静默不工作。
    #[test]
    fn version_encoding_matches_the_header_macro() {
        assert_eq!(SCS_TELEMETRY_VERSION_1_00, 0x0001_0000);
        assert_eq!(SCS_TELEMETRY_VERSION_1_01, 0x0001_0001);
        assert_eq!(make_version(2, 3), 0x0002_0003);
    }

    /// 类型标签对不上时一律返回 `None`,不去强读联合。
    #[test]
    fn reading_a_value_checks_the_type_tag_first() {
        let value = ScsValue {
            value_type: SCS_VALUE_TYPE_FLOAT,
            _padding: 0,
            payload: ScsValuePayload { value_float: 42.0 },
        };
        // SAFETY: 联合成员与 value_type 一致。
        unsafe {
            assert_eq!(value.as_float(), Some(42.0));
            assert_eq!(value.as_dplacement(), None, "类型不符必须拒读");
            assert_eq!(value.as_fvector(), None, "类型不符必须拒读");
        }
    }

    #[test]
    fn reading_an_fplacement_checks_the_official_type_tag() {
        assert_eq!(SCS_VALUE_TYPE_FPLACEMENT, 10);
        let placement = ScsFplacement {
            position: ScsFvector {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            orientation: ScsEuler {
                heading: 0.25,
                pitch: -0.125,
                roll: 0.5,
            },
        };
        let value = ScsValue {
            value_type: SCS_VALUE_TYPE_FPLACEMENT,
            _padding: 0,
            payload: ScsValuePayload {
                value_fplacement: placement,
            },
        };
        // SAFETY: 联合成员与 value_type 一致。
        unsafe {
            assert_eq!(value.as_fplacement(), Some(placement));
            assert_eq!(value.as_dplacement(), None, "类型不符必须拒读");
        }
    }

    /// `dplacement` 的字段偏移必须与 C 一致:position(24) + euler(12) + padding(4)。
    /// 这条和尺寸断言互补——尺寸对了字段顺序仍可能错。
    #[test]
    fn dplacement_field_offsets_match_the_c_layout() {
        let p = ScsDplacement {
            position: ScsDvector {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            orientation: ScsEuler {
                heading: 0.25,
                pitch: 0.1,
                roll: -0.1,
            },
            _padding: 0,
        };
        let base = std::ptr::from_ref(&p).cast::<u8>();
        let offset_of = |field: *const u8| unsafe { field.offset_from(base) };
        assert_eq!(offset_of(std::ptr::from_ref(&p.position).cast()), 0);
        assert_eq!(offset_of(std::ptr::from_ref(&p.orientation).cast()), 24);
        assert_eq!(offset_of(std::ptr::from_ref(&p._padding).cast()), 36);
    }

    #[test]
    fn configuration_structs_match_the_official_layout() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<ScsNamedValue>(), 64);
            assert_eq!(size_of::<ScsTelemetryConfiguration>(), 16);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(size_of::<ScsNamedValue>(), 56);
            assert_eq!(size_of::<ScsTelemetryConfiguration>(), 8);
        }
        assert_eq!(SCS_TELEMETRY_EVENT_CONFIGURATION, 5);
    }
}
