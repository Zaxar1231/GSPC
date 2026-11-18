//! Профиль управления (версия 1): формат файла `.rdprofile` и проверка его содержимого.
//!
//! Координаты — доли экрана (0–1). Клавиши — имена, как в `docs/controls-format.md`
//! (`"W"`, `"Space"`, `"F1"`); [`scancode`] переводит имя в скан-код набора 1.

use serde::{Deserialize, Serialize};

/// Версия формата, которую понимает этот код.
pub const VERSION: u32 = 1;

/// Наибольшее число контролов с касанием: слоты 0 и 1 заняты мышью и колесом.
pub const MAX_TOUCH_CONTROLS: usize = 8;

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

/// Контрол. Поле `type` выбирает вариант.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Касание в точке, пока нажата клавиша.
    Tap {
        /// Клавиша.
        key: Key,
        /// Точка касания.
        x: f32,
        /// Точка касания.
        y: f32,
    },
    /// Удержание или переключение касания.
    Hold {
        /// Клавиша.
        key: Key,
        /// Точка касания.
        x: f32,
        /// Точка касания.
        y: f32,
        /// Режим.
        mode: HoldMode,
    },
    /// Повтор касаний с интервалом, пока нажата клавиша.
    RepeatTap {
        /// Клавиша.
        key: Key,
        /// Точка касания.
        x: f32,
        /// Точка касания.
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
}

impl Control {
    /// Клавиша контрола.
    #[must_use]
    pub fn key(&self) -> &Key {
        match self {
            Self::Tap { key, .. }
            | Self::Hold { key, .. }
            | Self::RepeatTap { key, .. }
            | Self::Swipe { key, .. } => key,
        }
    }
}

/// Имя клавиши в профиле. Хранится как строка, проверяется при [`Profile::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(pub String);

/// Ошибка проверки профиля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// Версия формата не поддерживается.
    Version(u32),
    /// Пустое название.
    EmptyName,
    /// Координата вне 0–1 (или не число).
    Coordinate {
        /// Номер контрола, с нуля.
        index: usize,
    },
    /// Неизвестное имя клавиши.
    UnknownKey {
        /// Номер контрола, с нуля.
        index: usize,
        /// Имя.
        key: String,
    },
    /// Две записи назначены на одну клавишу.
    DuplicateKey(String),
    /// Нулевой интервал или длительность.
    ZeroTiming {
        /// Номер контрола, с нуля.
        index: usize,
    },
    /// Контролов с касанием больше, чем слотов.
    TooManyControls(usize),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(f, "версия профиля {v} не поддерживается (нужна {VERSION})"),
            Self::EmptyName => write!(f, "у профиля нет названия"),
            Self::Coordinate { index } => write!(f, "контрол {index}: координата вне 0–1"),
            Self::UnknownKey { index, key } => {
                write!(f, "контрол {index}: неизвестная клавиша «{key}»")
            }
            Self::DuplicateKey(k) => write!(f, "клавиша «{k}» назначена дважды"),
            Self::ZeroTiming { index } => {
                write!(f, "контрол {index}: нулевой интервал или длительность")
            }
            Self::TooManyControls(n) => {
                write!(f, "контролов с касанием {n}, больше {MAX_TOUCH_CONTROLS}")
            }
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
        let mut keys: Vec<&str> = Vec::new();
        for (index, c) in self.controls.iter().enumerate() {
            let key = c.key().0.as_str();
            if scancode(key).is_none() {
                return Err(ProfileError::UnknownKey {
                    index,
                    key: key.to_owned(),
                });
            }
            if keys.contains(&key) {
                return Err(ProfileError::DuplicateKey(key.to_owned()));
            }
            keys.push(key);
            let points = match c {
                Control::Tap { x, y, .. }
                | Control::Hold { x, y, .. }
                | Control::RepeatTap { x, y, .. } => vec![Point { x: *x, y: *y }],
                Control::Swipe { from, to, .. } => vec![*from, *to],
            };
            if points.iter().any(|p| !in_unit(p.x) || !in_unit(p.y)) {
                return Err(ProfileError::Coordinate { index });
            }
            let zero = match c {
                Control::RepeatTap { interval_ms, .. } => *interval_ms == 0,
                Control::Swipe { duration_ms, .. } => *duration_ms == 0,
                _ => false,
            };
            if zero {
                return Err(ProfileError::ZeroTiming { index });
            }
        }
        let touch = self.controls.len();
        if touch > MAX_TOUCH_CONTROLS {
            return Err(ProfileError::TooManyControls(touch));
        }
        Ok(())
    }
}

fn in_unit(v: f32) -> bool {
    (0.0..=1.0).contains(&v)
}

/// Скан-код набора 1 для имени клавиши профиля, `None` — неизвестное имя.
#[must_use]
pub fn scancode(name: &str) -> Option<u16> {
    const LETTERS: [(char, u16); 26] = [
        ('Q', 0x10),
        ('W', 0x11),
        ('E', 0x12),
        ('R', 0x13),
        ('T', 0x14),
        ('Y', 0x15),
        ('U', 0x16),
        ('I', 0x17),
        ('O', 0x18),
        ('P', 0x19),
        ('A', 0x1E),
        ('S', 0x1F),
        ('D', 0x20),
        ('F', 0x21),
        ('G', 0x22),
        ('H', 0x23),
        ('J', 0x24),
        ('K', 0x25),
        ('L', 0x26),
        ('Z', 0x2C),
        ('X', 0x2D),
        ('C', 0x2E),
        ('V', 0x2F),
        ('B', 0x30),
        ('N', 0x31),
        ('M', 0x32),
    ];
    let upper = name.to_ascii_uppercase();
    if let [c] = upper.as_bytes() {
        let c = char::from(*c);
        if c.is_ascii_digit() {
            // «1» … «9» → 0x02 … 0x0A, «0» → 0x0B.
            let d = u16::from(c as u8 - b'0');
            return Some(if d == 0 { 0x0B } else { d + 1 });
        }
        return LETTERS.iter().find(|(l, _)| *l == c).map(|(_, s)| *s);
    }
    Some(match upper.as_str() {
        "ESCAPE" | "ESC" => 0x01,
        "TAB" => 0x0F,
        "ENTER" => 0x1C,
        "LCTRL" => 0x1D,
        "LSHIFT" => 0x2A,
        "LALT" => 0x38,
        "SPACE" => 0x39,
        "F11" => 0x57,
        "F12" => 0x58,
        f if f.len() >= 2 && f.starts_with('F') => {
            let n: u16 = f[1..].parse().ok()?;
            if (1..=10).contains(&n) {
                0x3A + n
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": 1,
        "name": "Тест",
        "orientation": "landscape",
        "controls": [
            { "type": "tap", "key": "F", "x": 0.82, "y": 0.64 },
            { "type": "hold", "key": "C", "x": 0.9, "y": 0.85, "mode": "toggle" },
            { "type": "repeat_tap", "key": "E", "x": 0.7, "y": 0.8, "interval_ms": 50 },
            { "type": "swipe", "key": "Space", "from": {"x": 0.5, "y": 0.8},
              "to": {"x": 0.5, "y": 0.4}, "duration_ms": 80 }
        ]
    }"#;

    #[test]
    fn parses_sample() {
        let p = Profile::from_json(SAMPLE).expect("профиль");
        assert_eq!(p.controls.len(), 4);
        assert_eq!(p.orientation, Orientation::Landscape);
        assert!(matches!(
            p.controls[1],
            Control::Hold {
                mode: HoldMode::Toggle,
                ..
            }
        ));
    }

    #[test]
    fn rejects_bad_profiles() {
        let mut p = Profile::from_json(SAMPLE).expect("профиль");
        p.version = 2;
        assert_eq!(p.validate(), Err(ProfileError::Version(2)));

        let mut p = Profile::from_json(SAMPLE).expect("профиль");
        p.controls[0] = Control::Tap {
            key: Key("Foo".into()),
            x: 0.1,
            y: 0.1,
        };
        assert!(matches!(
            p.validate(),
            Err(ProfileError::UnknownKey { index: 0, .. })
        ));

        let mut p = Profile::from_json(SAMPLE).expect("профиль");
        p.controls[0] = Control::Tap {
            key: Key("F".into()),
            x: 1.5,
            y: 0.1,
        };
        assert_eq!(p.validate(), Err(ProfileError::Coordinate { index: 0 }));

        let mut p = Profile::from_json(SAMPLE).expect("профиль");
        p.controls[1] = Control::Tap {
            key: Key("F".into()),
            x: 0.1,
            y: 0.1,
        };
        assert_eq!(p.validate(), Err(ProfileError::DuplicateKey("F".into())));

        let mut p = Profile::from_json(SAMPLE).expect("профиль");
        p.controls[2] = Control::RepeatTap {
            key: Key("E".into()),
            x: 0.1,
            y: 0.1,
            interval_ms: 0,
        };
        assert_eq!(p.validate(), Err(ProfileError::ZeroTiming { index: 2 }));
    }

    #[test]
    fn unknown_control_type_is_an_error() {
        let text = SAMPLE.replace("\"tap\"", "\"joystick\"");
        assert!(Profile::from_json(&text).is_err());
    }

    #[test]
    fn scancodes() {
        assert_eq!(scancode("A"), Some(0x1E));
        assert_eq!(scancode("f"), Some(0x21));
        assert_eq!(scancode("1"), Some(0x02));
        assert_eq!(scancode("0"), Some(0x0B));
        assert_eq!(scancode("Space"), Some(0x39));
        assert_eq!(scancode("F1"), Some(0x3B));
        assert_eq!(scancode("F10"), Some(0x44));
        assert_eq!(scancode("F12"), Some(0x58));
        assert_eq!(scancode("LButton"), None);
        assert_eq!(scancode("F13"), None);
    }
}
