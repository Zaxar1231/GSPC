//! Keymap-движок.
//!
//! Чистая логика без зависимостей от Windows: на вход события хостового ввода, на выход
//! мультитач-события для virtio-input. Детерминированная, покрывается unit- и fuzz-тестами.
//!
//! Сейчас здесь перевод скан-кодов Windows в коды Linux ([`keys`]) и геометрия ([`geometry`]): координаты профиля хранятся в долях экрана
//! (0–1) и переводятся в координаты сенсорной панели гостя с учётом ориентации.
//! [`profile`] — формат профиля управления, [`keymap`] — движок контролов (этап 6).

pub mod geometry;
pub mod keymap;
pub mod keys;
pub mod profile;
pub mod wheel;

pub use geometry::{NormPoint, PanelSize, Rotation, TouchPoint};
