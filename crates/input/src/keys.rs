//! Перевод скан-кодов клавиатуры Windows (набор 1, `WM_KEYDOWN`/Raw Input) в коды Linux
//! `KEY_*`, которые понимает Android.
//!
//! Для основных клавиш набор 1 совпадает с кодами Linux, расширенным (`E0`) нужна таблица.

/// Код Linux `KEY_*` для скан-кода `scancode` (младшие 8 бит) и флага `extended` (префикс E0).
#[must_use]
pub fn linux_keycode(scancode: u16, extended: bool) -> Option<u16> {
    let sc = scancode & 0xFF;
    if !extended {
        // Набор 1 до 0x58 совпадает с KEY_* (ESC=1 … F12=88), кроме дыр 0x54..0x56.
        return match sc {
            0x01..=0x53 => Some(sc),
            0x56 => Some(86), // KEY_102ND
            0x57 => Some(87), // KEY_F11
            0x58 => Some(88), // KEY_F12
            _ => None,
        };
    }
    Some(match sc {
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x35 => 98,  // KEY_KPSLASH
        0x37 => 99,  // KEY_SYSRQ
        0x38 => 100, // KEY_RIGHTALT
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_keys_match_linux() {
        assert_eq!(linux_keycode(0x01, false), Some(1)); // ESC
        assert_eq!(linux_keycode(0x1E, false), Some(30)); // A
        assert_eq!(linux_keycode(0x39, false), Some(57)); // SPACE
        assert_eq!(linux_keycode(0x58, false), Some(88)); // F12
        assert_eq!(linux_keycode(0x54, false), None);
    }

    #[test]
    fn extended_keys() {
        assert_eq!(linux_keycode(0x48, true), Some(103)); // UP
        assert_eq!(linux_keycode(0x1D, true), Some(97)); // RIGHTCTRL
        assert_eq!(linux_keycode(0x48, false), Some(0x48)); // KP8 без E0
        assert_eq!(linux_keycode(0x11, true), None);
    }
}
