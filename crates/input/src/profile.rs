//! Профиль управления (версия 1): формат файла `.rdprofile` и проверка его содержимого.
//!
//! Координаты — доли экрана (0–1). Клавиши — имена, как в `docs/controls-format.md`
//! (`"W"`, `"Space"`, `"F1"`, `"LButton"`). Имя переводится в скан-код набора 1 [`scancode`]
//! или сопоставляется с кнопкой мыши; обратно — [`key_name`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Версия формата, которую понимает этот код.
pub const VERSION: u32 = 1;

/// Одновременных касаний, которые отдаёт движок: у панели гостя 10 слотов, слоты 0 и 1 заняты
/// мышью и колесом окна (`rustdroid-app`). Контролов в профиле может быть больше; касание
/// занимает слот только на время нажатия.
pub const MAX_TOUCH_SLOTS: u32 = 8;

/// Кнопки геймпада XInput, которые можно назначить на клавиши профиля.
pub const PAD_BUTTONS: [&str; 14] = [
    "A",
    "B",
    "X",
    "Y",
    "LB",
    "RB",
    "Back",
    "Start",
    "LS",
    "RS",
    "DPadUp",
    "DPadDown",
    "DPadLeft",
    "DPadRight",
];

/// Профиль целиком.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Версия формата.
    pub version: u32,
    /// Название.
    pub name: String,
    /// Пакет приложения, к которому привязан профиль (пока не используется).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Ориентация, для которой нарисованы координаты.
    #[serde(default)]
    pub orientation: Orientation,
    /// Контролы.
    pub controls: Vec<Control>,
    /// Назначение кнопок геймпада на клавиши: `{"A": "Space", …}`. Стики пока не поддержаны.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gamepad: BTreeMap<String, String>,
}

/// Ориентация экрана в профиле.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    /// Альбомная.
    #[default]
    Landscape,
    /// Книжная.
    Portrait,
}

/// Точка в долях экрана.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// X, 0–1.
    pub x: f32,
    /// Y, 0–1.
    pub y: f32,
}

/// Режим удержания.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HoldMode {
    /// Касание, пока нажата клавиша.
    Hold,
    /// Нажатие переключает касание.
    Toggle,
}

/// Клавиши джойстика (WASD и т. п.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickKeys {
    /// Вверх.
    pub up: Key,
    /// Влево.
    pub left: Key,
    /// Вниз.
    pub down: Key,
    /// Вправо.
    pub right: Key,
}

/// Чувствительность прицела по осям.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sensitivity {
    /// По горизонтали.
    pub x: f32,
    /// По вертикали.
    pub y: f32,
}

impl Default for Sensitivity {
    fn default() -> Self {
        Self { x: 1.0, y: 1.0 }
    }
}

/// Шаг макроса: касание или пауза.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MacroStep {
    /// Касание в точке (держится [`crate::engine::TAP_MS`]).
    Tap {
        /// Точка.
        tap: Point,
    },
    /// Пауза, мс.
    Wait {
        /// Длительность.
        wait_ms: u32,
    },
}

/// Контрол. Поле `type` выбирает вариант.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Касание в точке, пока нажата клавиша.
    Tap {
        /// Клавиша.
        key: Key,
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Удержание или переключение касания.
    Hold {
        /// Клавиша.
        key: Key,
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Режим.
        mode: HoldMode,
    },
    /// Повтор касаний с интервалом, пока нажата клавиша.
    RepeatTap {
        /// Клавиша.
        key: Key,
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Интервал между касаниями, мс.
        interval_ms: u32,
    },
    /// Свайп от `from` к `to` за `duration_ms` при нажатии клавиши.
    Swipe {
        /// Клавиша.
        key: Key,
        /// Начало.
        from: Point,
        /// Конец.
        to: Point,
        /// Длительность, мс.
        duration_ms: u32,
    },
    /// Джойстик: клавиши задают направление виртуального стика вокруг центра.
    Joystick {
        /// Клавиши направления.
        keys: StickKeys,
        /// Центр стика.
        x: f32,
        /// Центр стика.
        y: f32,
        /// Радиус, доля экрана.
        radius: f32,
    },
    /// Прицел: переключатель клавишей, движение курсора двигает касание.
    Aim {
        /// Клавиша включения.
        toggle_key: Key,
        /// Исходная точка касания.
        x: f32,
        /// Исходная точка касания.
        y: f32,
        /// Чувствительность.
        #[serde(default)]
        sensitivity: Sensitivity,
        /// Инверсия вертикали.
        #[serde(default)]
        invert_y: bool,
    },
    /// Стрельба: касание в точке, пока нажата клавиша (отдельный палец от прицела).
    Shoot {
        /// Клавиша.
        key: Key,
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Колесо мыши: два касания по обе стороны от центра, расстояние меняет щелчок.
    Zoom {
        /// Центр.
        center: Point,
        /// Изменение расстояния за щелчок, доля экрана.
        step: f32,
    },
    /// Макрос: последовательность касаний и пауз по нажатию клавиши.
    Macro {
        /// Клавиша.
        key: Key,
        /// Шаги.
        steps: Vec<MacroStep>,
    },
    /// Скилл MOBA: касание в точке, мышь задаёт направление, отпускание — применить;
    /// `cancel_key` при удержании отменяет.
    MobaSkill {
        /// Клавиша.
        key: Key,
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Радиус направления, доля экрана.
        radius: f32,
        /// Клавиша отмены.
        cancel_key: Key,
    },
}

impl Control {
    /// Клавиши контрола (для проверки и привязки).
    #[must_use]
    pub fn keys(&self) -> Vec<&Key> {
        match self {
            Self::Tap { key, .. }
            | Self::Hold { key, .. }
            | Self::RepeatTap { key, .. }
            | Self::Swipe { key, .. }
            | Self::Shoot { key, .. }
            | Self::Macro { key, .. } => vec![key],
            Self::Joystick { keys, .. } => vec![&keys.up, &keys.left, &keys.down, &keys.right],
            Self::Aim { toggle_key, .. } => vec![toggle_key],
            Self::MobaSkill {
                key, cancel_key, ..
            } => vec![key, cancel_key],
            Self::Zoom { .. } => Vec::new(),
        }
    }

    /// Сколько касаний контрол держит одновременно (зум — два).
    #[must_use]
    pub fn slots(&self) -> u32 {
        match self {
            Self::Zoom { .. } => 2,
            _ => 1,
        }
    }
}

/// Имя клавиши в профиле. Проверяется при [`Profile::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(pub String);

/// Ошибка проверки профиля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// Версия формата не поддерживается.
    Version(u32),
    /// Пустое название.
    EmptyName,
    /// Координата, радиус или чувствительность вне допустимых значений.
    Value {
        /// Номер контрола, с нуля.
        index: usize,
    },
    /// Неизвестное имя клавиши.
    UnknownKey {
        /// Номер контрола, с нуля (для геймпада — `usize::MAX`).
        index: usize,
        /// Имя.
        key: String,
    },
    /// Одна клавиша назначена дважды.
    DuplicateKey(String),
    /// Нулевой интервал, длительность или пустой макрос.
    ZeroTiming {
        /// Номер контрола, с нуля.
        index: usize,
    },
    /// Неизвестная кнопка геймпада.
    UnknownPadButton(String),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(f, "версия профиля {v} не поддерживается (нужна {VERSION})"),
            Self::EmptyName => write!(f, "у профиля нет названия"),
            Self::Value { index } => {
                write!(f, "контрол {index}: значение вне допустимого диапазона")
            }
            Self::UnknownKey { index, key } => {
                write!(f, "контрол {index}: неизвестная клавиша «{key}»")
            }
            Self::DuplicateKey(k) => write!(f, "клавиша «{k}» назначена дважды"),
            Self::ZeroTiming { index } => {
                write!(
                    f,
                    "контрол {index}: нулевой интервал, длительность или пустой макрос"
                )
            }
            Self::UnknownPadButton(b) => write!(f, "неизвестная кнопка геймпада «{b}»"),
        }
    }
}

impl std::error::Error for ProfileError {}

impl Profile {
    /// Разбирает JSON и сразу проверяет профиль.
    ///
    /// # Errors
    /// Ошибка JSON или проверки.
    pub fn from_json(text: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let p: Self = serde_json::from_str(text)?;
        p.validate()?;
        Ok(p)
    }

    /// Проверяет содержимое профиля.
    ///
    /// # Errors
    /// Первая найденная ошибка.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.version != VERSION {
            return Err(ProfileError::Version(self.version));
        }
        if self.name.trim().is_empty() {
            return Err(ProfileError::EmptyName);
        }
        let mut seen: Vec<String> = Vec::new();
        for (index, c) in self.controls.iter().enumerate() {
            for key in c.keys() {
                if !is_key(&key.0) {
                    return Err(ProfileError::UnknownKey {
                        index,
                        key: key.0.clone(),
                    });
                }
                let canon = key.0.to_ascii_uppercase();
                if seen.contains(&canon) {
                    return Err(ProfileError::DuplicateKey(key.0.clone()));
                }
                seen.push(canon);
            }
            check_values(index, c)?;
        }
        for (button, key) in &self.gamepad {
            if !PAD_BUTTONS.contains(&button.as_str()) {
                return Err(ProfileError::UnknownPadButton(button.clone()));
            }
            if !is_key(key) {
                return Err(ProfileError::UnknownKey {
                    index: usize::MAX,
                    key: key.clone(),
                });
            }
        }
        Ok(())
    }
}

fn check_values(index: usize, c: &Control) -> Result<(), ProfileError> {
    let bad = || ProfileError::Value { index };
    let zero = || ProfileError::ZeroTiming { index };
    let unit = |v: f32| (0.0..=1.0).contains(&v);
    let point = |p: &Point| unit(p.x) && unit(p.y);
    match c {
        Control::Tap { x, y, .. } | Control::Hold { x, y, .. } | Control::Shoot { x, y, .. } => {
            if !(unit(*x) && unit(*y)) {
                return Err(bad());
            }
        }
        Control::RepeatTap {
            x, y, interval_ms, ..
        } => {
            if !(unit(*x) && unit(*y)) {
                return Err(bad());
            }
            if *interval_ms == 0 {
                return Err(zero());
            }
        }
        Control::Swipe {
            from,
            to,
            duration_ms,
            ..
        } => {
            if !(point(from) && point(to)) {
                return Err(bad());
            }
            if *duration_ms == 0 {
                return Err(zero());
            }
        }
        Control::Joystick { x, y, radius, .. } => {
            if !(unit(*x) && unit(*y) && (0.0..=0.5).contains(radius)) {
                return Err(bad());
            }
        }
        Control::Aim {
            x, y, sensitivity, ..
        } => {
            let pos_ok = |v: f32| v.is_finite() && v > 0.0 && v <= 10.0;
            if !(unit(*x) && unit(*y) && pos_ok(sensitivity.x) && pos_ok(sensitivity.y)) {
                return Err(bad());
            }
        }
        Control::Zoom { center, step } => {
            if !point(center) || !step.is_finite() || *step <= 0.0 || *step > 0.5 {
                return Err(bad());
            }
        }
        Control::Macro { steps, .. } => {
            if steps.is_empty() {
                return Err(zero());
            }
            for s in steps {
                match s {
                    MacroStep::Tap { tap } if !point(tap) => return Err(bad()),
                    MacroStep::Wait { wait_ms } if *wait_ms > 60_000 => return Err(bad()),
                    _ => {}
                }
            }
        }
        Control::MobaSkill { x, y, radius, .. } => {
            if !(unit(*x) && unit(*y) && (0.0..=0.5).contains(radius)) {
                return Err(bad());
            }
        }
    }
    Ok(())
}

/// Клавиши: `(имя, скан-код набора 1)`. Первое имя в строке — каноническое.
const KEYS: &[(&str, u16)] = &[
    ("Escape", 0x01),
    ("1", 0x02),
    ("2", 0x03),
    ("3", 0x04),
    ("4", 0x05),
    ("5", 0x06),
    ("6", 0x07),
    ("7", 0x08),
    ("8", 0x09),
    ("9", 0x0A),
    ("0", 0x0B),
    ("Tab", 0x0F),
    ("Q", 0x10),
    ("W", 0x11),
    ("E", 0x12),
    ("R", 0x13),
    ("T", 0x14),
    ("Y", 0x15),
    ("U", 0x16),
    ("I", 0x17),
    ("O", 0x18),
    ("P", 0x19),
    ("Enter", 0x1C),
    ("LCtrl", 0x1D),
    ("A", 0x1E),
    ("S", 0x1F),
    ("D", 0x20),
    ("F", 0x21),
    ("G", 0x22),
    ("H", 0x23),
    ("J", 0x24),
    ("K", 0x25),
    ("L", 0x26),
    ("LShift", 0x2A),
    ("Z", 0x2C),
    ("X", 0x2D),
    ("C", 0x2E),
    ("V", 0x2F),
    ("B", 0x30),
    ("N", 0x31),
    ("M", 0x32),
    ("LAlt", 0x38),
    ("Space", 0x39),
    ("F1", 0x3B),
    ("F2", 0x3C),
    ("F3", 0x3D),
    ("F4", 0x3E),
    ("F5", 0x3F),
    ("F6", 0x40),
    ("F7", 0x41),
    ("F8", 0x42),
    ("F9", 0x43),
    ("F10", 0x44),
    ("F11", 0x57),
    ("F12", 0x58),
];

/// Кнопки мыши как клавиши профиля (события приходят от окна по имени).
const MOUSE_KEYS: &[&str] = &["LButton", "RButton"];

/// Скан-код набора 1 для имени клавиши профиля; `None` — не клавиатурная клавиша.
#[must_use]
pub fn scancode(name: &str) -> Option<u16> {
    KEYS.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, c)| *c)
}

/// Имя клавиши профиля по скан-коду набора 1 (для событий окна).
#[must_use]
pub fn key_name(code: u16) -> Option<&'static str> {
    KEYS.iter().find(|(_, c)| *c == code).map(|(n, _)| *n)
}

/// Известно ли имя клавиши профиля (клавиатура или кнопка мыши).
#[must_use]
pub fn is_key(name: &str) -> bool {
    scancode(name).is_some() || MOUSE_KEYS.iter().any(|m| m.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": 1,
        "name": "Example Shooter — по умолчанию",
        "package": "com.example.shooter",
        "orientation": "landscape",
        "controls": [
            { "type": "tap", "key": "F", "x": 0.82, "y": 0.64 },
            { "type": "joystick", "keys": { "up": "W", "left": "A", "down": "S", "right": "D" },
              "x": 0.15, "y": 0.72, "radius": 0.08, "smoothing": 0.0 },
            { "type": "aim", "toggle_key": "LAlt", "x": 0.65, "y": 0.45,
              "sensitivity": { "x": 1.0, "y": 1.0 }, "invert_y": false, "smoothing": 0.0 },
            { "type": "shoot", "key": "LButton", "x": 0.88, "y": 0.52 },
            { "type": "swipe", "key": "Space", "from": { "x": 0.5, "y": 0.8 }, "to": { "x": 0.5, "y": 0.4 }, "duration_ms": 80 },
            { "type": "repeat_tap", "key": "E", "x": 0.7, "y": 0.8, "interval_ms": 50 },
            { "type": "hold", "key": "C", "x": 0.9, "y": 0.85, "mode": "toggle" },
            { "type": "zoom", "key": "Wheel", "center": { "x": 0.5, "y": 0.5 }, "step": 0.05 },
            { "type": "macro", "key": "Q", "steps": [
                { "tap": { "x": 0.6, "y": 0.6 } }, { "wait_ms": 120 }, { "tap": { "x": 0.7, "y": 0.6 } } ] },
            { "type": "moba_skill", "key": "1", "x": 0.78, "y": 0.82, "radius": 0.07, "cancel_key": "Escape" }
        ],
        "gamepad": { "A": "Space", "LB": "LShift" }
    }"#;

    #[test]
    fn documented_sample_parses() {
        let p = Profile::from_json(SAMPLE).expect("профиль");
        assert_eq!(p.controls.len(), 10);
        assert_eq!(p.gamepad.get("A").map(String::as_str), Some("Space"));
    }

    #[test]
    fn rejects_bad_profiles() {
        let base = || Profile::from_json(SAMPLE).expect("профиль");
        let mut p = base();
        p.version = 2;
        assert_eq!(p.validate(), Err(ProfileError::Version(2)));

        let mut p = base();
        p.controls[0] = Control::Tap {
            key: Key("Foo".into()),
            x: 0.1,
            y: 0.1,
        };
        assert!(matches!(
            p.validate(),
            Err(ProfileError::UnknownKey { index: 0, .. })
        ));

        let mut p = base();
        p.controls[0] = Control::Tap {
            key: Key("F".into()),
            x: 1.5,
            y: 0.1,
        };
        assert_eq!(p.validate(), Err(ProfileError::Value { index: 0 }));

        let mut p = base();
        p.controls[0] = Control::Tap {
            key: Key("W".into()),
            x: 0.1,
            y: 0.1,
        };
        assert_eq!(p.validate(), Err(ProfileError::DuplicateKey("W".into())));

        let mut p = base();
        p.controls[8] = Control::Macro {
            key: Key("Q".into()),
            steps: vec![],
        };
        assert_eq!(p.validate(), Err(ProfileError::ZeroTiming { index: 8 }));

        let mut p = base();
        p.gamepad.insert("Z".into(), "Space".into());
        assert_eq!(
            p.validate(),
            Err(ProfileError::UnknownPadButton("Z".into()))
        );
    }

    #[test]
    fn unknown_control_type_is_an_error() {
        let text = SAMPLE.replace("\"tap\"", "\"laser\"");
        assert!(Profile::from_json(&text).is_err());
    }

    #[test]
    fn key_names_round_trip() {
        assert_eq!(scancode("a"), Some(0x1E));
        assert_eq!(scancode("Space"), Some(0x39));
        assert_eq!(scancode("F10"), Some(0x44));
        assert_eq!(key_name(0x1E), Some("A"));
        assert_eq!(key_name(0x39), Some("Space"));
        assert!(is_key("LButton"));
        assert!(!is_key("Wheel"));
        assert!(!is_key("F13"));
    }
}
