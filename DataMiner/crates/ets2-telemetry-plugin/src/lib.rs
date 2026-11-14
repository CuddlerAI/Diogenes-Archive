//! ETS2 / ATS 官方 Telemetry SDK 插件:逐帧把 `player_*` 写成 JSON Lines。
//!
//! 这是 [98 号 §5.7.1](../../docs/98-steam-wide-map-metric-actioncamera-source-survey.md)
//! 第 ①② 步的实现——`ets2-player-telemetry` 拥有换算与契约,本 crate 拥有
//! **与游戏进程的接口**:注册通道、按帧攒数据、非阻塞落盘。
//!
//! # 结构
//!
//! | 模块 | 职责 | 能不能在 macOS 上验 |
//! |---|---|---|
//! | [`ffi`] | SCS C ABI 镜像 + 布局断言 | ✅ 断言是编译期的,`--target x86_64-pc-windows-msvc` 直接 check |
//! | [`state`] | 逐帧状态机(暂停/缺通道/时间基回跳) | ✅ 平台无关,原生跑测试 |
//! | [`sink`] | 非阻塞写线程 | ✅ 只用 std |
//! | 本文件 | 导出的两个函数与全局状态 | ✅ check;A43/A46 已真机验证,本次上游迁移后仍需冒烟回归 |
//!
//! Windows 构建只额外读取 QPC 起止值，用来与 D3D11 probe 做同帧关联；状态机仍保持
//! 平台无关，所以除这层薄适配外仍可在没有游戏的机器上验证。
//!
//! # 安装
//!
//! 编出 `ets2_telemetry_plugin.dll` 放进 `<游戏目录>/bin/win_x64/plugins/`
//! (32 位游戏对应 `bin/win_x86/`)。构建与安装细节见 README。
//!
//! # 配置(环境变量,游戏启动前设)
//!
//! - `DM_ETS2_TELEMETRY_OUT`:输出 JSONL 路径。未设时落在系统临时目录。
//! - `DM_ETS2_ROLL_CONVENTION`:`unvalidated` / `forward`(默认) / `local_z_backward`。
//!   A46 已用 SDK world/cabin/head 与 GPU 相对旋转交叉标定 `forward`;显式无效值仍
//!   fail closed 到 `unvalidated`。
//!
//! # 验证边界(不要扩大结论)
//!
//! A43/A46 已在迁移前的本地实现上完成真机验证;本次移植到最新上游后的集成分支只有
//! 单元测试证据,还需按 README 的「首次上机检查单」做一次冒烟回归。SDK 回调线程模型
//! 仍按官方文档防御性处理,不要从既有证据外推未测过的线程行为。

pub mod ffi;
pub mod sink;
pub mod state;

use core::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, OnceLock};

use ets2_player_telemetry::RollConvention;

use crate::ffi::{
    SCS_LOG_TYPE_ERROR, SCS_LOG_TYPE_MESSAGE, SCS_RESULT_GENERIC_ERROR, SCS_RESULT_OK,
    SCS_RESULT_UNSUPPORTED, SCS_TELEMETRY_CHANNEL_FLAG_NONE, SCS_TELEMETRY_EVENT_CONFIGURATION,
    SCS_TELEMETRY_EVENT_FRAME_END, SCS_TELEMETRY_EVENT_FRAME_START, SCS_TELEMETRY_EVENT_PAUSED,
    SCS_TELEMETRY_EVENT_STARTED, SCS_TELEMETRY_VERSION_1_00, SCS_TELEMETRY_VERSION_1_01,
    SCS_U32_NIL, SCS_VALUE_TYPE_DPLACEMENT, SCS_VALUE_TYPE_FLOAT, SCS_VALUE_TYPE_FPLACEMENT,
    SCS_VALUE_TYPE_FVECTOR, ScsFvector, ScsLogFn, ScsResult, ScsTelemetryConfiguration,
    ScsTelemetryFrameStart, ScsTelemetryInitParamsV100, ScsValue,
};
use crate::sink::JsonLineSink;
use crate::state::FrameAccumulator;

#[cfg(windows)]
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// 解析 `DM_ETS2_ROLL_CONVENTION`。
///
/// ETS2 A46 实机标定已经确认 `forward`;因此未配置时使用该已验证约定。
/// **显式但无法识别的取值仍退回 `Unvalidated` 而不是报错**:一个拼错的环境变量
/// 不该让整局采集失败,但它也绝不能被当成「已标定」——退回未标定是安全的一侧,
/// 而且该取值会原样写进每一条样本,事后看得见。
#[must_use]
pub fn parse_roll_convention(raw: Option<&str>) -> RollConvention {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None => RollConvention::RollAxisIsForward,
        Some("forward") => RollConvention::RollAxisIsForward,
        Some("local_z_backward") => RollConvention::RollAxisIsLocalZBackward,
        _ => RollConvention::Unvalidated,
    }
}

/// 输出路径:环境变量优先,否则落临时目录。
#[must_use]
pub fn resolve_output_path(configured: Option<&str>) -> std::path::PathBuf {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let mut path = std::env::temp_dir();
            path.push("dm-ets2-telemetry.jsonl");
            path
        }
    }
}

struct Plugin {
    accumulator: FrameAccumulator,
    sink: JsonLineSink,
    roll_convention: RollConvention,
    qpc_frequency_hz: Option<i64>,
    log: ScsLogFn,
    unregister_from_event: ffi::ScsUnregisterFromEventFn,
    unregister_from_channel: ffi::ScsUnregisterFromChannelFn,
    serialize_errors: u64,
}

#[cfg(windows)]
fn qpc_now() -> Option<i64> {
    let mut value = 0_i64;
    unsafe { QueryPerformanceCounter(&mut value) }
        .is_ok()
        .then_some(value)
        .filter(|value| *value > 0)
}

#[cfg(not(windows))]
fn qpc_now() -> Option<i64> {
    None
}

#[cfg(windows)]
fn qpc_frequency_hz() -> Option<i64> {
    let mut value = 0_i64;
    unsafe { QueryPerformanceFrequency(&mut value) }
        .is_ok()
        .then_some(value)
        .filter(|value| *value > 0)
}

#[cfg(not(windows))]
fn qpc_frequency_hz() -> Option<i64> {
    None
}

static PLUGIN: OnceLock<Mutex<Option<Plugin>>> = OnceLock::new();

fn plugin_slot() -> &'static Mutex<Option<Plugin>> {
    PLUGIN.get_or_init(|| Mutex::new(None))
}

/// 往游戏日志写一行。`log` 为 `None` 时静默——绝不 panic。
fn log_line(log: ScsLogFn, log_type: i32, message: &str) {
    let Some(log_fn) = log else { return };
    // 消息里嵌了 NUL 就退回一句固定文本，宁可少说也不要在 FFI 边界上炸。
    let text = std::ffi::CString::new(message)
        .unwrap_or_else(|_| c"dm-ets2: message contained NUL".to_owned());
    // SAFETY: log_fn 由引擎提供且在插件生命周期内有效；text 在调用期间存活且以 NUL 结尾。
    unsafe { log_fn(log_type, text.as_ptr()) }
}

/// 所有跨 FFI 的回调都套这个:**panic 绝不能越过 FFI 边界**(那是 UB)。
fn guard<F: FnOnce()>(body: F) {
    let _ = catch_unwind(AssertUnwindSafe(body));
}

/// 拿到插件并执行;未初始化或锁中毒时静默跳过。
fn with_plugin<F: FnOnce(&mut Plugin)>(body: F) {
    let Ok(mut slot) = plugin_slot().lock() else {
        return;
    };
    if let Some(plugin) = slot.as_mut() {
        body(plugin);
    }
}

// ---------------------------------------------------------------------------
// 通道回调。每个通道各一个函数——比用 context 传判别码更直白，也免去指针整数互转。
// 全部跑在游戏线程上：只做「读联合 + 存值」，不分配、不加锁竞争、不做 IO。
// ---------------------------------------------------------------------------

unsafe extern "system" fn on_world_placement(
    _name: *const c_char,
    _index: u32,
    value: *const ScsValue,
    _context: *mut c_void,
) {
    guard(|| {
        // SAFETY: 引擎保证注册类型匹配时 value 非空且有效；as_dplacement 仍会再核类型标签。
        let Some(placement) = (unsafe { value.as_ref().and_then(|v| v.as_dplacement()) }) else {
            return;
        };
        with_plugin(|plugin| plugin.accumulator.on_world_placement(placement));
    });
}

unsafe extern "system" fn on_local_linear_velocity(
    _name: *const c_char,
    _index: u32,
    value: *const ScsValue,
    _context: *mut c_void,
) {
    guard(|| {
        // SAFETY: 同上。
        let Some(vector) = (unsafe { value.as_ref().and_then(|v| v.as_fvector()) }) else {
            return;
        };
        with_plugin(|plugin| plugin.accumulator.on_local_linear_velocity(vector));
    });
}

unsafe extern "system" fn on_speedometer(
    _name: *const c_char,
    _index: u32,
    value: *const ScsValue,
    _context: *mut c_void,
) {
    guard(|| {
        // SAFETY: 同上。
        let Some(speed) = (unsafe { value.as_ref().and_then(|v| v.as_float()) }) else {
            return;
        };
        with_plugin(|plugin| plugin.accumulator.on_speedometer(speed));
    });
}

unsafe extern "system" fn on_cabin_offset(
    _name: *const c_char,
    _index: u32,
    value: *const ScsValue,
    _context: *mut c_void,
) {
    guard(|| {
        // SAFETY: 引擎保证注册类型匹配时 value 非空且有效；as_fplacement 仍会再核类型标签。
        let Some(placement) = (unsafe { value.as_ref().and_then(|v| v.as_fplacement()) }) else {
            return;
        };
        with_plugin(|plugin| plugin.accumulator.on_cabin_offset(placement));
    });
}

unsafe extern "system" fn on_head_offset(
    _name: *const c_char,
    _index: u32,
    value: *const ScsValue,
    _context: *mut c_void,
) {
    guard(|| {
        // SAFETY: 同上。
        let Some(placement) = (unsafe { value.as_ref().and_then(|v| v.as_fplacement()) }) else {
            return;
        };
        with_plugin(|plugin| plugin.accumulator.on_head_offset(placement));
    });
}

/// 在官方以 `name == NULL` 结尾的配置属性数组里找一个 `fvector`。
///
/// # Safety
///
/// `configuration` 及其属性数组必须来自当前 configuration 回调并在本次调用期间有效。
unsafe fn find_fvector_attribute(
    configuration: &ScsTelemetryConfiguration,
    expected_name: &CStr,
) -> Option<ScsFvector> {
    let mut current = configuration.attributes;
    loop {
        // SAFETY: SDK 保证 attributes 指向以 name == NULL 结尾的有效数组。
        let attribute = unsafe { current.as_ref() }?;
        if attribute.name.is_null() {
            return None;
        }
        // SAFETY: SDK 保证属性名是有效、NUL 结尾的 ASCII 子集。
        if attribute.index == SCS_U32_NIL
            && unsafe { CStr::from_ptr(attribute.name) } == expected_name
        {
            // SAFETY: 值来自 SDK；as_fvector 会先检查类型标签。
            return unsafe { attribute.value.as_fvector() };
        }
        // SAFETY: 当前元素不是终止符，前进到数组的下一项。
        current = unsafe { current.add(1) };
    }
}

unsafe extern "system" fn on_event(event: u32, event_info: *const c_void, _context: *mut c_void) {
    guard(|| match event {
        SCS_TELEMETRY_EVENT_FRAME_START => {
            // SAFETY: frame_start 事件的 event_info 指向 scs_telemetry_frame_start_t。
            let Some(frame_start) =
                (unsafe { event_info.cast::<ScsTelemetryFrameStart>().as_ref() })
            else {
                return;
            };
            let sample_qpc = qpc_now();
            with_plugin(|plugin| {
                plugin
                    .accumulator
                    .on_frame_start_with_qpc(frame_start, sample_qpc);
            });
        }
        SCS_TELEMETRY_EVENT_FRAME_END => with_plugin(|plugin| {
            let record = plugin.accumulator.on_frame_end_with_qpc(
                plugin.roll_convention,
                qpc_now(),
                plugin.qpc_frequency_hz,
            );
            match serde_json::to_string(&record) {
                // 满了就丢并计数，绝不重试——重试就是把阻塞换个写法。
                Ok(line) => {
                    let _ = plugin.sink.try_publish(line);
                }
                Err(_) => plugin.serialize_errors += 1,
            }
        }),
        SCS_TELEMETRY_EVENT_PAUSED => with_plugin(|plugin| plugin.accumulator.on_paused()),
        SCS_TELEMETRY_EVENT_STARTED => with_plugin(|plugin| plugin.accumulator.on_started()),
        SCS_TELEMETRY_EVENT_CONFIGURATION => {
            // SAFETY: configuration 事件的 event_info 指向 scs_telemetry_configuration_t。
            let Some(configuration) =
                (unsafe { event_info.cast::<ScsTelemetryConfiguration>().as_ref() })
            else {
                return;
            };
            if configuration.id.is_null()
                // SAFETY: SDK 保证 id 是有效、NUL 结尾的 ASCII 子集。
                || unsafe { CStr::from_ptr(configuration.id) } != c"truck"
            {
                return;
            }
            // SAFETY: configuration 及属性数组在本次回调期间有效。
            let cabin = unsafe { find_fvector_attribute(configuration, c"cabin.position") };
            // SAFETY: 同上。
            let head = unsafe { find_fvector_attribute(configuration, c"head.position") };
            with_plugin(|plugin| plugin.accumulator.on_truck_configuration(cabin, head));
        }
        _ => {}
    });
}

/// `scs_telemetry_init`——引擎从最新版本开始逐个尝试,直到我们返回 ok。
///
/// # Safety
///
/// 由游戏引擎按 SCS Telemetry SDK 约定调用:`params` 在本次调用期间指向一个
/// 与 `version` 匹配的合法初始化结构体。不要从别处调用它。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn scs_telemetry_init(version: u32, params: *const c_void) -> ScsResult {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if version != SCS_TELEMETRY_VERSION_1_00 && version != SCS_TELEMETRY_VERSION_1_01 {
            // 必须是 unsupported 而不是 error：引擎靠这个码继续往下试更旧的版本。
            return SCS_RESULT_UNSUPPORTED;
        }
        // SAFETY: 版本已确认，按约定 params 指向 v100 布局（v101 复用同一布局）。
        let Some(params) = (unsafe { params.cast::<ScsTelemetryInitParamsV100>().as_ref() }) else {
            return SCS_RESULT_GENERIC_ERROR;
        };
        let log = params.common.log;

        let out_path = resolve_output_path(std::env::var("DM_ETS2_TELEMETRY_OUT").ok().as_deref());
        let sink = match JsonLineSink::create(&out_path) {
            Ok(sink) => sink,
            Err(error) => {
                log_line(
                    log,
                    SCS_LOG_TYPE_ERROR,
                    &format!("dm-ets2: 无法创建输出文件 {}: {error}", out_path.display()),
                );
                return SCS_RESULT_GENERIC_ERROR;
            }
        };
        let roll_convention =
            parse_roll_convention(std::env::var("DM_ETS2_ROLL_CONVENTION").ok().as_deref());

        let (Some(register_event), Some(register_channel)) =
            (params.register_for_event, params.register_for_channel)
        else {
            return SCS_RESULT_GENERIC_ERROR;
        };

        // 事件：五个都是必需的。frame_start 给对齐键，frame_end 触发产出，
        // paused/started 决定「这一帧算不算数」，configuration 给相机默认位置。
        for event in [
            SCS_TELEMETRY_EVENT_FRAME_START,
            SCS_TELEMETRY_EVENT_FRAME_END,
            SCS_TELEMETRY_EVENT_PAUSED,
            SCS_TELEMETRY_EVENT_STARTED,
            SCS_TELEMETRY_EVENT_CONFIGURATION,
        ] {
            // SAFETY: register_event 由引擎提供，回调是本 crate 的 'static 函数。
            if unsafe { register_event(event, Some(on_event), std::ptr::null_mut()) }
                != SCS_RESULT_OK
            {
                log_line(
                    log,
                    SCS_LOG_TYPE_ERROR,
                    &format!("dm-ets2: 事件 {event} 注册失败"),
                );
                return SCS_RESULT_GENERIC_ERROR;
            }
        }

        // 通道：world.placement 是硬需求；速度两路各自可选，但至少要有一路
        //（state.rs 在两路都没上报时会跳过该帧，而不是拿 0 顶替）。
        let channel_specs: [(&CStr, u32, ffi::ScsTelemetryChannelCallback, bool); 5] = [
            (
                c"truck.world.placement",
                SCS_VALUE_TYPE_DPLACEMENT,
                Some(on_world_placement),
                true,
            ),
            (
                c"truck.local.velocity.linear",
                SCS_VALUE_TYPE_FVECTOR,
                Some(on_local_linear_velocity),
                false,
            ),
            (
                c"truck.speed",
                SCS_VALUE_TYPE_FLOAT,
                Some(on_speedometer),
                false,
            ),
            (
                c"truck.cabin.offset",
                SCS_VALUE_TYPE_FPLACEMENT,
                Some(on_cabin_offset),
                false,
            ),
            (
                c"truck.head.offset",
                SCS_VALUE_TYPE_FPLACEMENT,
                Some(on_head_offset),
                false,
            ),
        ];
        for (name, value_type, callback, required) in channel_specs {
            // SAFETY: name 是 'static NUL 结尾字面量；callback 是本 crate 的 'static 函数。
            let result = unsafe {
                register_channel(
                    name.as_ptr(),
                    SCS_U32_NIL,
                    value_type,
                    SCS_TELEMETRY_CHANNEL_FLAG_NONE,
                    callback,
                    std::ptr::null_mut(),
                )
            };
            if result != SCS_RESULT_OK {
                let name = name.to_string_lossy();
                if required {
                    log_line(
                        log,
                        SCS_LOG_TYPE_ERROR,
                        &format!("dm-ets2: 必需通道 {name} 注册失败({result})"),
                    );
                    return SCS_RESULT_GENERIC_ERROR;
                }
                log_line(
                    log,
                    SCS_LOG_TYPE_ERROR,
                    &format!("dm-ets2: 可选通道 {name} 注册失败({result}),继续"),
                );
            }
        }

        log_line(
            log,
            SCS_LOG_TYPE_MESSAGE,
            &format!(
                "dm-ets2: 已就绪 out={} roll_convention={roll_convention:?} roll_status={}",
                out_path.display(),
                if roll_convention.is_validated() {
                    "validated"
                } else {
                    "unvalidated_recomputable"
                }
            ),
        );

        let Ok(mut slot) = plugin_slot().lock() else {
            return SCS_RESULT_GENERIC_ERROR;
        };
        *slot = Some(Plugin {
            accumulator: FrameAccumulator::new(),
            sink,
            roll_convention,
            qpc_frequency_hz: qpc_frequency_hz(),
            log,
            unregister_from_event: params.unregister_from_event,
            unregister_from_channel: params.unregister_from_channel,
            serialize_errors: 0,
        });
        SCS_RESULT_OK
    }));
    result.unwrap_or(SCS_RESULT_GENERIC_ERROR)
}

/// `scs_telemetry_shutdown`。
///
/// # Safety
///
/// 由游戏引擎在 [`scs_telemetry_init`] 返回成功后调用一次。不要从别处调用它。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn scs_telemetry_shutdown() {
    guard(|| {
        // 取出插件后立刻放锁,再去退订:SDK 的线程模型(全部回调在游戏线程)仍只按
        // 官方文档处理,若引擎真从别的线程回调且 unregister 会等在途回调完成,
        // 「持锁调 unregister + 回调里等锁」就是死锁。放锁之后迟到的回调只会在
        // `with_plugin` 里看到 None 然后跳过——严格更安全。
        let taken = {
            let Ok(mut slot) = plugin_slot().lock() else {
                return;
            };
            slot.take()
        };
        let Some(mut plugin) = taken else {
            return;
        };

        // 先退订：退订之后引擎不会再回调，剩下的排干工作就不会与回调竞争。
        if let Some(unregister_event) = plugin.unregister_from_event {
            for event in [
                SCS_TELEMETRY_EVENT_FRAME_START,
                SCS_TELEMETRY_EVENT_FRAME_END,
                SCS_TELEMETRY_EVENT_PAUSED,
                SCS_TELEMETRY_EVENT_STARTED,
                SCS_TELEMETRY_EVENT_CONFIGURATION,
            ] {
                // SAFETY: 由引擎提供，且我们确实注册过这些事件。
                unsafe { unregister_event(event) };
            }
        }
        if let Some(unregister_channel) = plugin.unregister_from_channel {
            for (name, value_type) in [
                (c"truck.world.placement", SCS_VALUE_TYPE_DPLACEMENT),
                (c"truck.local.velocity.linear", SCS_VALUE_TYPE_FVECTOR),
                (c"truck.speed", SCS_VALUE_TYPE_FLOAT),
                (c"truck.cabin.offset", SCS_VALUE_TYPE_FPLACEMENT),
                (c"truck.head.offset", SCS_VALUE_TYPE_FPLACEMENT),
            ] {
                // SAFETY: 同上。
                unsafe { unregister_channel(name.as_ptr(), SCS_U32_NIL, value_type) };
            }
        }

        let summary = format!(
            "dm-ets2: 收工 {} serialize_errors={} unflagged_time_regressions={}",
            plugin.sink.stats().summary(),
            plugin.serialize_errors,
            plugin.accumulator.unflagged_time_regressions(),
        );
        plugin.sink.shutdown();
        log_line(plugin.log, SCS_LOG_TYPE_MESSAGE, &summary);
    });
}

/// 编译期确认导出的两个函数签名与 SDK 头文件一致。
///
/// 签名错了(比如把 `extern "system"` 写成 `extern "C"`)在 x64 上不会立刻报错,
/// 却会在 32 位游戏上静默错栈。这两行让签名错误变成编译错误。
const _: () = {
    const _INIT: unsafe extern "system" fn(u32, *const c_void) -> ScsResult = scs_telemetry_init;
    const _SHUTDOWN: unsafe extern "system" fn() = scs_telemetry_shutdown;
};

#[cfg(test)]
mod tests {
    use super::*;
    use ets2_player_telemetry::channels;

    fn named_fvector(name: &'static CStr, value: ScsFvector) -> ffi::ScsNamedValue {
        ffi::ScsNamedValue {
            name: name.as_ptr(),
            index: SCS_U32_NIL,
            #[cfg(target_pointer_width = "64")]
            _padding: 0,
            value: ScsValue {
                value_type: SCS_VALUE_TYPE_FVECTOR,
                _padding: 0,
                payload: ffi::ScsValuePayload {
                    value_fvector: value,
                },
            },
        }
    }

    /// 未配置使用 A46 已标定的 ETS2 forward 约定；拼错的显式取值仍退回未标定。
    #[test]
    fn ets2_defaults_to_calibrated_forward_but_bad_values_remain_unvalidated() {
        assert_eq!(
            parse_roll_convention(None),
            RollConvention::RollAxisIsForward
        );
        assert_eq!(
            parse_roll_convention(Some("forward")),
            RollConvention::RollAxisIsForward
        );
        assert_eq!(
            parse_roll_convention(Some("  LOCAL_Z_BACKWARD  ")),
            RollConvention::RollAxisIsLocalZBackward
        );
        for bad in [Some(""), Some("typo"), Some("true")] {
            assert_eq!(
                parse_roll_convention(bad),
                RollConvention::Unvalidated,
                "{bad:?} 应退回未标定"
            );
            assert!(!parse_roll_convention(bad).is_validated());
        }
    }

    /// 输出路径:给了就用,空白/未给退回临时目录。
    #[test]
    fn output_path_falls_back_to_a_temp_file() {
        assert_eq!(
            resolve_output_path(Some("/data/out.jsonl")),
            std::path::PathBuf::from("/data/out.jsonl")
        );
        for empty in [None, Some(""), Some("   ")] {
            let path = resolve_output_path(empty);
            assert!(path.starts_with(std::env::temp_dir()), "{path:?}");
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("jsonl"));
        }
    }

    /// 通道名必须与 `ets2-player-telemetry` 导出的常量逐字一致——
    /// 两处各写一遍字符串,写错一个字母的表现是「通道注册失败」而不是编译错误。
    #[test]
    fn channel_names_match_the_shared_constants() {
        assert_eq!(
            c"truck.world.placement".to_str().unwrap(),
            channels::WORLD_PLACEMENT
        );
        assert_eq!(
            c"truck.local.velocity.linear".to_str().unwrap(),
            channels::LOCAL_LINEAR_VELOCITY
        );
        assert_eq!(c"truck.speed".to_str().unwrap(), channels::SPEEDOMETER);
        assert_eq!(
            c"truck.cabin.offset".to_str().unwrap(),
            channels::CABIN_OFFSET
        );
        assert_eq!(
            c"truck.head.offset".to_str().unwrap(),
            channels::HEAD_OFFSET
        );
    }

    #[test]
    fn camera_base_positions_are_read_from_the_terminated_configuration_array() {
        let cabin = ScsFvector {
            x: 0.25,
            y: 1.5,
            z: -0.75,
        };
        let attributes = [
            named_fvector(c"cabin.position", cabin),
            ffi::ScsNamedValue {
                name: std::ptr::null(),
                index: 0,
                #[cfg(target_pointer_width = "64")]
                _padding: 0,
                value: ScsValue {
                    value_type: 0,
                    _padding: 0,
                    payload: ffi::ScsValuePayload { value_float: 0.0 },
                },
            },
        ];
        let configuration = ScsTelemetryConfiguration {
            id: c"truck".as_ptr(),
            attributes: attributes.as_ptr(),
        };

        // SAFETY: attributes 是本测试构造的合法终止数组，调用期间一直存活。
        unsafe {
            assert_eq!(
                find_fvector_attribute(&configuration, c"cabin.position"),
                Some(cabin)
            );
            assert_eq!(
                find_fvector_attribute(&configuration, c"head.position"),
                None
            );
        }
    }
}
