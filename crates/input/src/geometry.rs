//! Координаты в долях экрана и их перевод в координаты сенсорной панели гостя.
//!
//! Соглашения:
//! - [`NormPoint`] — точка в долях **отображаемого** экрана (то, что видит пользователь):
//!   `(0, 0)` — левый верхний угол, `(1, 1)` — правый нижний;
//! - сенсорная панель гостя всегда работает в своей «естественной» ориентации, Android сам
//!   поворачивает касания согласно ориентации дисплея;
//! - [`Rotation`] — на сколько градусов **по часовой стрелке** изображение повёрнуто
//!   относительно естественной ориентации панели.
//!
//! Соответствие с `Surface.ROTATION_*` гостя проверяется на этапе 3 на реальном образе.

/// Точка в долях отображаемого экрана. Компоненты всегда в диапазоне `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormPoint {
    x: f32,
    y: f32,
}

impl NormPoint {
    /// Создаёт точку, приводя компоненты к `0.0..=1.0`. `NaN` превращается в `0.0`.
    #[must_use]
    pub fn new(x: f32, y: f32) -> Self {
        Self {
            x: clamp01(x),
            y: clamp01(y),
        }
    }

    /// Горизонтальная доля.
    #[must_use]
    pub fn x(self) -> f32 {
        self.x
    }

    /// Вертикальная доля.
    #[must_use]
    pub fn y(self) -> f32 {
        self.y
    }

    /// Переводит точку из системы отображаемого экрана в систему панели
    /// (тоже в долях, но в естественной ориентации панели).
    #[must_use]
    pub fn to_panel(self, rotation: Rotation) -> Self {
        let Self { x, y } = self;
        match rotation {
            Rotation::Deg0 => Self { x, y },
            // Поворот изображения на 90° по часовой: панель (u, v) → экран (1 - v, u).
            Rotation::Deg90 => Self { x: y, y: 1.0 - x },
            Rotation::Deg180 => Self {
                x: 1.0 - x,
                y: 1.0 - y,
            },
            // Поворот на 270° по часовой: панель (u, v) → экран (v, 1 - u).
            Rotation::Deg270 => Self { x: 1.0 - y, y: x },
        }
    }

    /// Переводит точку в абсолютные координаты сенсорной панели (значения `ABS_MT_POSITION_*`).
    #[must_use]
    pub fn to_touch(self, rotation: Rotation, panel: PanelSize) -> TouchPoint {
        let p = self.to_panel(rotation);
        TouchPoint {
            x: scale(p.x, panel.width),
            y: scale(p.y, panel.height),
        }
    }
}

/// Поворот изображения относительно естественной ориентации панели (по часовой стрелке).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Rotation {
    /// Естественная ориентация.
    #[default]
    Deg0,
    /// 90° по часовой.
    Deg90,
    /// 180°.
    Deg180,
    /// 270° по часовой.
    Deg270,
}

impl Rotation {
    /// `true`, если ширина и высота отображаемого экрана поменяны местами относительно панели.
    #[must_use]
    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::Deg90 | Self::Deg270)
    }
}

/// Размер сенсорной панели гостя в её естественной ориентации (диапазоны `ABS_MT_POSITION_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PanelSize {
    /// Число точек по горизонтали.
    pub width: u32,
    /// Число точек по вертикали.
    pub height: u32,
}

/// Абсолютная точка касания на панели.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TouchPoint {
    /// Координата X (`0..width`).
    pub x: u32,
    /// Координата Y (`0..height`).
    pub y: u32,
}

#[inline]
fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

#[inline]
fn scale(frac: f32, size: u32) -> u32 {
    let max = size.saturating_sub(1);
    #[allow(
        clippy::cast_precision_loss,
        reason = "размеры панели много меньше 2^24"
    )]
    let scaled = (frac * max as f32).round();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "frac в 0..=1, значит scaled в 0..=max"
    )]
    let value = scaled as u32;
    value.min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANEL: PanelSize = PanelSize {
        width: 1080,
        height: 2400,
    };

    #[test]
    fn clamps_and_handles_nan() {
        let p = NormPoint::new(-0.5, f32::NAN);
        assert_eq!((p.x(), p.y()), (0.0, 0.0));
        let p = NormPoint::new(1.5, 0.25);
        assert_eq!((p.x(), p.y()), (1.0, 0.25));
    }

    #[test]
    fn identity_rotation() {
        let t = NormPoint::new(0.5, 0.5).to_touch(Rotation::Deg0, PANEL);
        assert_eq!(t, TouchPoint { x: 540, y: 1200 });
        let t = NormPoint::new(1.0, 1.0).to_touch(Rotation::Deg0, PANEL);
        assert_eq!(t, TouchPoint { x: 1079, y: 2399 });
    }

    #[test]
    fn corners_follow_clockwise_rotation() {
        // Изображение повёрнуто на 90° по часовой: левый верхний угол экрана —
        // это левый нижний угол панели.
        let top_left = NormPoint::new(0.0, 0.0).to_panel(Rotation::Deg90);
        assert_eq!((top_left.x(), top_left.y()), (0.0, 1.0));
        let top_right = NormPoint::new(1.0, 0.0).to_panel(Rotation::Deg90);
        assert_eq!((top_right.x(), top_right.y()), (0.0, 0.0));

        let top_left = NormPoint::new(0.0, 0.0).to_panel(Rotation::Deg270);
        assert_eq!((top_left.x(), top_left.y()), (1.0, 0.0));

        let top_left = NormPoint::new(0.0, 0.0).to_panel(Rotation::Deg180);
        assert_eq!((top_left.x(), top_left.y()), (1.0, 1.0));
    }

    #[test]
    fn opposite_rotations_cancel() {
        let p = NormPoint::new(0.2, 0.7);
        let q = p.to_panel(Rotation::Deg90).to_panel(Rotation::Deg270);
        assert!((q.x() - p.x()).abs() < 1e-6 && (q.y() - p.y()).abs() < 1e-6);
        let q = p.to_panel(Rotation::Deg180).to_panel(Rotation::Deg180);
        assert!((q.x() - p.x()).abs() < 1e-6 && (q.y() - p.y()).abs() < 1e-6);
    }

    #[test]
    fn zero_sized_panel_does_not_underflow() {
        let t = NormPoint::new(1.0, 1.0).to_touch(
            Rotation::Deg0,
            PanelSize {
                width: 0,
                height: 0,
            },
        );
        assert_eq!(t, TouchPoint { x: 0, y: 0 });
    }

    #[test]
    fn swaps_axes() {
        assert!(!Rotation::Deg0.swaps_axes());
        assert!(Rotation::Deg90.swaps_axes());
        assert!(!Rotation::Deg180.swaps_axes());
        assert!(Rotation::Deg270.swaps_axes());
    }
}
