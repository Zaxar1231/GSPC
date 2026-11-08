//! Инстансы и данные RustDroid.
//!
//! Сейчас: раскладка каталогов данных ([`paths`]) и подготовка загрузки Android из образов
//! Cuttlefish ([`android`]).
//! Дальше: конфигурация инстанса (CPU, RAM, разрешение, DPI, FPS, рендер, microG),
//! менеджер образов Android, клонирование и снапшоты.

pub mod android;
pub mod paths;

pub use paths::{DataDirs, PathsError};
