//! Профиль управления: JSON (`.rdprofile`), формат — `docs/controls-format.md`.
//!
//! Координаты и радиусы — в долях отображаемого экрана (0–1); радиусы — в долях **высоты**
//! экрана, чтобы круг оставался кругом на широком экране.

use serde::{Deserialize, Serialize};

/// Текущая версия формата.
pub const VERSION: u32 = 1;

/// Точка в долях экрана.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Доля ширины.
    pub x: f32,
    /// Доля высоты.
    pub y: f32,
}

/// Ориентация, в которой размечен профиль.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    /// Альбомная.
    #[default]
    Landscape,
    /// Книжная.
    Portrait,
}

/// Клавиши четырёх направлений джойстика.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoystickKeys {
    /// Вверх.
    pub up: String,
    /// Влево.
    pub left: String,
    /// Вниз.
    pub down: String,
    /// Вправо.
    pub right: String,
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

/// Удержание или переключение.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldMode {
    /// Касание, пока клавиша нажата.
    #[default]
    Hold,
    /// Нажатие ставит палец, следующее — убирает.
    Toggle,
}

/// Шаг макроса.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroStep {
    /// Короткое касание.
    Tap(Point),
    /// Пауза, мс.
    WaitMs(u32),
}

/// Контрол.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Касание в точке, пока нажата клавиша.
    Tap {
        /// Клавиша.
        key: String,
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Клавиши направлений → виртуальный стик.
    Joystick {
        /// Клавиши.
        keys: JoystickKeys,
        /// Центр X.
        x: f32,
        /// Центр Y.
        y: f32,
        /// Радиус (доля высоты экрана).
        radius: f32,
    },
    /// Прицел: клавиша включает захват мыши, движение мыши тянет палец.
    Aim {
        /// Клавиша включения/выключения.
        toggle_key: String,
        /// Точка, откуда начинается касание.
        x: f32,
        /// Y.
        y: f32,
        /// Чувствительность.
        #[serde(default)]
        sensitivity: Sensitivity,
        /// Инвертировать вертикаль.
        #[serde(default)]
        invert_y: bool,
    },
    /// Касание, пока клавиша нажата и прицел включён (выстрел).
    Shoot {
        /// Клавиша.
        key: String,
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Свайп от `from` к `to`.
    Swipe {
        /// Клавиша.
        key: String,
        /// Начало.
        from: Point,
        /// Конец.
        to: Point,
        /// Длительность, мс.
        #[serde(default = "default_swipe_ms")]
        duration_ms: u32,
    },
    /// Повтор касаний, пока клавиша нажата.
    RepeatTap {
        /// Клавиша.
        key: String,
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Интервал между касаниями, мс.
        #[serde(default = "default_repeat_ms")]
        interval_ms: u32,
    },
    /// Удержание или переключение.
    Hold {
        /// Клавиша.
        key: String,
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Режим.
        #[serde(default)]
        mode: HoldMode,
    },
    /// Колесо мыши → щипок вокруг `center`.
    Zoom {
        /// Клавиша (обычно `Wheel`).
        key: String,
        /// Центр щипка.
        center: Point,
        /// Насколько разводятся пальцы за щелчок колеса (доля высоты).
        #[serde(default = "default_zoom_step")]
        step: f32,
    },
    /// Последовательность касаний и пауз.
    Macro {
        /// Клавиша.
        key: String,
        /// Шаги.
        steps: Vec<MacroStep>,
    },
    /// Скилл MOBA: нажатие — палец на скилле, курсор задаёт направление, отпускание —
    /// применение; `cancel_key` уводит палец в `cancel` (кнопка отмены игры) и отпускает.
    MobaSkill {
        /// Клавиша.
        key: String,
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Радиус (доля высоты экрана).
        radius: f32,
        /// Клавиша отмены.
        #[serde(default)]
        cancel_key: Option<String>,
        /// Куда увести палец при отмене (без неё — отпустить на месте скилла).
        #[serde(default)]
        cancel: Option<Point>,
    },
}

fn default_swipe_ms() -> u32 {
    80
}
fn default_repeat_ms() -> u32 {
    50
}
fn default_zoom_step() -> f32 {
    0.05
}

/// Профиль.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Версия формата.
    pub version: u32,
    /// Название.
    pub name: String,
    /// Пакет игры: профиль включается, когда она на переднем плане (пусто — вручную).
    #[serde(default)]
    pub package: String,
    /// Ориентация разметки.
    #[serde(default)]
    pub orientation: Orientation,
    /// Контролы.
    pub controls: Vec<Control>,
}

/// Ошибка чтения профиля.
#[derive(Debug)]
pub enum ProfileError {
    /// Неверный JSON или поля.
    Json(serde_json::Error),
    /// Профиль из более новой версии программы.
    TooNew(u32),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "профиль не прочитан: {e}"),
            Self::TooNew(v) => write!(
                f,
                "профиль версии {v} создан более новой программой (поддерживается до {VERSION})"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

impl Profile {
    /// Читает профиль из JSON.
    ///
    /// # Errors
    /// Неверный JSON или версия новее [`VERSION`].
    pub fn from_json(text: &str) -> Result<Self, ProfileError> {
        // Блокнот и PowerShell 5.1 сохраняют UTF-8 с BOM, serde_json его не принимает.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let p: Self = serde_json::from_str(text).map_err(ProfileError::Json)?;
        if p.version > VERSION {
            return Err(ProfileError::TooNew(p.version));
        }
        // Миграции vN → vN+1 появятся вместе с версией 2.
        Ok(p)
    }

    /// JSON профиля (с отступами — его правят руками).
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_from_docs_parses_and_roundtrips() {
        let text = r#"{
          "version": 1,
          "name": "Example Shooter",
          "package": "com.example.shooter",
          "orientation": "landscape",
          "controls": [
            { "type": "tap", "key": "F", "x": 0.82, "y": 0.64 },
            { "type": "joystick", "keys": { "up": "W", "left": "A", "down": "S", "right": "D" },
              "x": 0.15, "y": 0.72, "radius": 0.08 },
            { "type": "aim", "toggle_key": "LAlt", "x": 0.65, "y": 0.45,
              "sensitivity": { "x": 1.0, "y": 1.0 }, "invert_y": false },
            { "type": "shoot", "key": "LButton", "x": 0.88, "y": 0.52 },
            { "type": "swipe", "key": "Space", "from": { "x": 0.5, "y": 0.8 }, "to": { "x": 0.5, "y": 0.4 }, "duration_ms": 80 },
            { "type": "repeat_tap", "key": "E", "x": 0.7, "y": 0.8, "interval_ms": 50 },
            { "type": "hold", "key": "C", "x": 0.9, "y": 0.85, "mode": "toggle" },
            { "type": "zoom", "key": "Wheel", "center": { "x": 0.5, "y": 0.5 }, "step": 0.05 },
            { "type": "macro", "key": "Q", "steps": [
                { "tap": { "x": 0.6, "y": 0.6 } }, { "wait_ms": 120 }, { "tap": { "x": 0.7, "y": 0.6 } } ] },
            { "type": "moba_skill", "key": "1", "x": 0.78, "y": 0.82, "radius": 0.07, "cancel_key": "Escape" }
          ]
        }"#;
        let p = Profile::from_json(text).unwrap();
        assert_eq!(p.controls.len(), 10);
        assert_eq!(Profile::from_json(&p.to_json()).unwrap(), p);
    }

    #[test]
    fn defaults_and_version_check() {
        let p = Profile::from_json(
            r#"{"version":1,"name":"x","controls":[{"type":"swipe","key":"Space","from":{"x":0,"y":0},"to":{"x":1,"y":1}}]}"#,
        )
        .unwrap();
        assert_eq!(p.package, "");
        assert!(matches!(
            p.controls[0],
            Control::Swipe {
                duration_ms: 80,
                ..
            }
        ));
        assert!(matches!(
            Profile::from_json(r#"{"version":2,"name":"x","controls":[]}"#),
            Err(ProfileError::TooNew(2))
        ));
        assert!(Profile::from_json("{").is_err());
        assert!(Profile::from_json("\u{feff}{\"version\":1,\"name\":\"x\",\"controls\":[]}").is_ok());
    }
}
