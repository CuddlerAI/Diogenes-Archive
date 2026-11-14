//! Rust 侧的构建风味，与前端 `src/build-flavor.ts` 同源同纪律（docs/65 J-13）。
//!
//! 值来自构建脚本注入的编译期环境变量 `DATAMINER_BUILD_FLAVOR`
//! （tools/build-windows-installer.ps1 在整个打包过程中设为进程级 env，rustc 与
//! Vite 因此读到同一个值），打包结束后脚本会断言产物真实的 Authenticode 状态与
//! 风味一致。缺省即 unsigned-dev：忘了设只会低估自己、不会高估——「production
//! 字样 + 未签名产物」这个组合在构造上不可能被发布出去。`unsigned-delivered`
//! 是单独的线上目录口味：仍诚实显示 UNSIGNED，但不再自称本地 DEV。
//!
//! 此前窗口标题与托盘文案把「UNSIGNED DEV」写死在 tauri.conf.json 与
//! lifecycle.rs 里，production 配置又不覆盖 `app.windows`——生产构建会继续自称
//! UNSIGNED DEV。现在四处文案都从这里取值。

// `const fn`：策略的编译期核对靠它（见 services/updater.rs 末尾的 const 断言）。
// `Option<&str>` 的 `==` 不能在 const 上下文里调，所以改成比字节。
pub const fn is_production() -> bool {
    match option_env!("DATAMINER_BUILD_FLAVOR") {
        Some(value) => matches!(value.as_bytes(), b"production"),
        None => false,
    }
}

/// 是否属于固定更新渠道上的、只消费服务端验签目录的发行口味。
///
/// Authenticode 身份与线上目录来源是两件事：`unsigned-delivered` 没有 Windows
/// 代码签名，但与 production 一样不内嵌 dev catalog，允许执行 M-A 版本检查。
pub const fn uses_delivered_catalog() -> bool {
    match option_env!("DATAMINER_BUILD_FLAVOR") {
        Some(value) => matches!(value.as_bytes(), b"production" | b"unsigned-delivered"),
        None => false,
    }
}

const fn is_unsigned_delivered() -> bool {
    match option_env!("DATAMINER_BUILD_FLAVOR") {
        Some(value) => matches!(value.as_bytes(), b"unsigned-delivered"),
        None => false,
    }
}

pub fn main_window_title() -> &'static str {
    if is_production() {
        "DataMiner · Local Collector"
    } else if is_unsigned_delivered() {
        "DataMiner UNSIGNED · Online Collector"
    } else {
        "DataMiner UNSIGNED DEV · Local Collector"
    }
}

pub fn overlay_window_title() -> &'static str {
    if is_production() {
        "DataMiner · Capture Status"
    } else if is_unsigned_delivered() {
        "DataMiner UNSIGNED · Capture Status"
    } else {
        "DataMiner UNSIGNED DEV · Capture Status"
    }
}

pub fn tray_open_label() -> &'static str {
    if is_production() {
        "打开 DataMiner"
    } else if is_unsigned_delivered() {
        "打开 DataMiner UNSIGNED"
    } else {
        "打开 DataMiner UNSIGNED DEV"
    }
}

pub fn tray_tooltip() -> &'static str {
    if is_production() {
        "DataMiner · 本地采集客户端"
    } else if is_unsigned_delivered() {
        "DataMiner UNSIGNED · 线上目录采集客户端"
    } else {
        "DataMiner UNSIGNED DEV · 非生产本地采集客户端"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 常规 cargo test 不设 DATAMINER_BUILD_FLAVOR，必须落在 unsigned-dev 一侧，
    // 且警示字样在场——这是 docs/15 C3 的 UI 面。
    #[test]
    fn defaults_to_unsigned_dev_with_warning_copy() {
        if is_production() {
            // 构建脚本以 production 风味跑测试时，警示字样必须消失。
            assert!(!main_window_title().contains("UNSIGNED"));
            assert!(!tray_tooltip().contains("UNSIGNED"));
        } else if uses_delivered_catalog() {
            assert!(main_window_title().contains("UNSIGNED"));
            assert!(!main_window_title().contains("DEV"));
            assert!(tray_tooltip().contains("线上目录"));
        } else {
            assert!(main_window_title().contains("UNSIGNED DEV"));
            assert!(overlay_window_title().contains("UNSIGNED DEV"));
            assert!(tray_open_label().contains("UNSIGNED DEV"));
            assert!(tray_tooltip().contains("UNSIGNED DEV"));
        }
    }
}
