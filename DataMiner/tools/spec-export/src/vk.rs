//! Windows Virtual-Key → spec 键名的固定映射。
//!
//! 数据侧 spec 对 `code` 只要求“系统虚拟键名、全会话一致”。这里维护一张
//! 固定表覆盖常用键；表外 VK 统一格式化为 `VK_0xNN`（两位十六进制大写，
//! 超过 0xFF 时自然扩展位数），保证同一 VK 在全会话得到同一名字。

/// 键盘事件的 `vk` → spec `code`。
pub fn key_code_name(vk: u16) -> String {
    match vk {
        // 字母与数字直接用字符本身（W → "W"）。
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        0x08 => "BACKSPACE".into(),
        0x09 => "TAB".into(),
        0x0D => "ENTER".into(),
        0x10 => "SHIFT".into(),
        0x11 => "CTRL".into(),
        0x12 => "ALT".into(),
        0x13 => "PAUSE".into(),
        0x14 => "CAPSLOCK".into(),
        0x1B => "ESCAPE".into(),
        0x20 => "SPACE".into(),
        0x21 => "PAGEUP".into(),
        0x22 => "PAGEDOWN".into(),
        0x23 => "END".into(),
        0x24 => "HOME".into(),
        0x25 => "LEFT".into(),
        0x26 => "UP".into(),
        0x27 => "RIGHT".into(),
        0x28 => "DOWN".into(),
        0x2C => "PRINTSCREEN".into(),
        0x2D => "INSERT".into(),
        0x2E => "DELETE".into(),
        0x5B => "LWIN".into(),
        0x5C => "RWIN".into(),
        0x5D => "APPS".into(),
        0x60..=0x69 => format!("NUMPAD{}", vk - 0x60),
        0x6A => "NUMPAD_MULTIPLY".into(),
        0x6B => "NUMPAD_ADD".into(),
        0x6D => "NUMPAD_SUBTRACT".into(),
        0x6E => "NUMPAD_DECIMAL".into(),
        0x6F => "NUMPAD_DIVIDE".into(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x90 => "NUMLOCK".into(),
        0x91 => "SCROLLLOCK".into(),
        0xA0 => "LSHIFT".into(),
        0xA1 => "RSHIFT".into(),
        0xA2 => "LCTRL".into(),
        0xA3 => "RCTRL".into(),
        0xA4 => "LALT".into(),
        0xA5 => "RALT".into(),
        0xBA => "SEMICOLON".into(),
        0xBB => "EQUALS".into(),
        0xBC => "COMMA".into(),
        0xBD => "MINUS".into(),
        0xBE => "PERIOD".into(),
        0xBF => "SLASH".into(),
        0xC0 => "GRAVE".into(),
        0xDB => "LBRACKET".into(),
        0xDC => "BACKSLASH".into(),
        0xDD => "RBRACKET".into(),
        0xDE => "APOSTROPHE".into(),
        other => format!("VK_0x{other:02X}"),
    }
}

/// 鼠标按键事件的 `vk`（VK_LBUTTON 等）→ spec `button`。
pub fn mouse_button_name(vk: u16) -> String {
    match vk {
        0x01 => "left".into(),
        0x02 => "right".into(),
        0x04 => "middle".into(),
        0x05 => "x1".into(),
        0x06 => "x2".into(),
        other => format!("VK_0x{other:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{key_code_name, mouse_button_name};

    #[test]
    fn letters_and_digits_use_the_character() {
        assert_eq!(key_code_name(0x57), "W");
        assert_eq!(key_code_name(0x41), "A");
        assert_eq!(key_code_name(0x30), "0");
        assert_eq!(key_code_name(0x39), "9");
    }

    #[test]
    fn common_named_keys() {
        assert_eq!(key_code_name(0x20), "SPACE");
        assert_eq!(key_code_name(0xA0), "LSHIFT");
        assert_eq!(key_code_name(0x70), "F1");
        assert_eq!(key_code_name(0x87), "F24");
        assert_eq!(key_code_name(0x60), "NUMPAD0");
    }

    #[test]
    fn unknown_vk_is_stable_hex() {
        assert_eq!(key_code_name(0x07), "VK_0x07");
        assert_eq!(key_code_name(0xE7), "VK_0xE7");
        assert_eq!(key_code_name(0x1FF), "VK_0x1FF");
    }

    #[test]
    fn mouse_buttons() {
        assert_eq!(mouse_button_name(1), "left");
        assert_eq!(mouse_button_name(2), "right");
        assert_eq!(mouse_button_name(4), "middle");
        assert_eq!(mouse_button_name(5), "x1");
        assert_eq!(mouse_button_name(6), "x2");
        assert_eq!(mouse_button_name(0x0A), "VK_0x0A");
    }
}
