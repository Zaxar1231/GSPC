//! Геймпад (раскладка XInput) → события движка: кнопки — клавиши `Pad*`, стики — `PadLeft` и
//! `PadRight`. Хост опрашивает геймпад и передаёт сюда состояние; здесь только чистая логика.

/// Состояние геймпада (поля как у `XINPUT_GAMEPAD`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadState {
    /// Кнопки (биты `XINPUT_GAMEPAD_*`).
    pub buttons: u16,
    /// Левый курок, 0–255.
    pub lt: u8,
    /// Правый курок, 0–255.
    pub rt: u8,
    /// Левый стик X (вправо — плюс).
    pub lx: i16,
    /// Левый стик Y (вверх — плюс).
    pub ly: i16,
    /// Правый стик X.
    pub rx: i16,
    /// Правый стик Y.
    pub ry: i16,
}

/// Кнопки XInput и их имена в профиле.
const BUTTONS: [(u16, &str); 14] = [
    (0x0001, "PadUp"),
    (0x0002, "PadDown"),
    (0x0004, "PadLeft"),
    (0x0008, "PadRight"),
    (0x0010, "PadStart"),
    (0x0020, "PadBack"),
    (0x0040, "PadLS"),
    (0x0080, "PadRS"),
    (0x0100, "PadLB"),
    (0x0200, "PadRB"),
    (0x1000, "PadA"),
    (0x2000, "PadB"),
    (0x4000, "PadX"),
    (0x8000, "PadY"),
];

/// Курок нажат выше этого значения (`XINPUT_GAMEPAD_TRIGGER_THRESHOLD`).
const TRIGGER: u8 = 30;
/// Мёртвая зона стика (доля полного хода).
pub const DEADZONE: f32 = 0.15;

/// Имя левого стика в профиле (поле `stick`).
pub const LEFT_STICK: &str = "PadLeftStick";
/// Имя правого стика в профиле.
pub const RIGHT_STICK: &str = "PadRightStick";

/// Кнопки и курки, изменившиеся между `prev` и `cur`: (имя, нажата).
#[must_use]
pub fn buttons(prev: PadState, cur: PadState) -> Vec<(&'static str, bool)> {
    let mut out: Vec<(&'static str, bool)> = BUTTONS
        .iter()
        .filter(|(bit, _)| (prev.buttons ^ cur.buttons) & bit != 0)
        .map(|&(bit, name)| (name, cur.buttons & bit != 0))
        .collect();
    for (p, c, name) in [(prev.lt, cur.lt, "PadLT"), (prev.rt, cur.rt, "PadRT")] {
        if (p > TRIGGER) != (c > TRIGGER) {
            out.push((name, c > TRIGGER));
        }
    }
    out
}

/// Положение стика в долях хода, ось Y — вниз (как у экрана); в мёртвой зоне — `(0, 0)`.
#[must_use]
pub fn stick(x: i16, y: i16) -> (f32, f32) {
    let (fx, fy) = (f32::from(x) / 32767.0, -f32::from(y) / 32767.0);
    let m = fx.hypot(fy);
    if m < DEADZONE {
        return (0.0, 0.0);
    }
    // За пределами мёртвой зоны — плавно от 0 до 1, чтобы не было скачка.
    let k = ((m - DEADZONE) / (1.0 - DEADZONE)).min(1.0) / m;
    (fx * k, fy * k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_and_trigger_edges() {
        let idle = PadState::default();
        let a = PadState {
            buttons: 0x1000,
            rt: 200,
            ..idle
        };
        assert_eq!(buttons(idle, a), vec![("PadA", true), ("PadRT", true)]);
        assert_eq!(buttons(a, a), vec![]);
        let b = PadState {
            buttons: 0x2000,
            rt: 10,
            ..idle
        };
        assert_eq!(
            buttons(a, b),
            vec![("PadA", false), ("PadB", true), ("PadRT", false)]
        );
    }

    #[test]
    fn stick_deadzone_and_axes() {
        assert_eq!(stick(1000, -1000), (0.0, 0.0));
        let (x, y) = stick(32767, 0);
        assert!((x - 1.0).abs() < 1e-4 && y == 0.0);
        // Вверх у XInput — плюс, на экране — минус.
        let (_, y) = stick(0, 32767);
        assert!((y + 1.0).abs() < 1e-4);
        // Сразу за мёртвой зоной — почти ноль, без скачка.
        let (x, _) = stick(5300, 0);
        assert!(x > 0.0 && x < 0.02);
    }
}
