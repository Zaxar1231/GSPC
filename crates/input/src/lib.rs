//! Keymap-движок.
//!
//! Чистая логика без зависимостей от Windows: на вход события хостового ввода, на выход
//! мультитач-события для virtio-input. Детерминированная, покрывается unit- и fuzz-тестами.
//!
//! Состав: перевод скан-кодов Windows в коды Linux ([`keys`]), геометрия ([`geometry`]):
//! координаты профиля хранятся в долях экрана (0–1) и переводятся в координаты сенсорной
//! панели гостя с учётом ориентации; формат профиля ([`profile`]) и движок контролов
//! ([`engine`]): касание, удержание/переключение, повтор, свайп.
//! Контролы aim/joystick/shoot/zoom/macro/moba_skill и кнопки геймпада ([`gamepad`]) — в движке;
//! набор профилей с выбором по пакету — [`book`]. Редактор поверх игры — в планах этапа 6.

pub mod book;
pub mod engine;
pub mod gamepad;
pub mod geometry;
pub mod keys;
pub mod profile;
pub mod wheel;

pub use book::ProfileBook;
pub use engine::{Engine, Out};
pub use geometry::{NormPoint, PanelSize, Rotation, TouchPoint};
pub use profile::{Control, Profile, ProfileError};
