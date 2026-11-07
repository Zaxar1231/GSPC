//! Инстансы и данные RustDroid.
//!
//! Сейчас: раскладка каталогов данных ([`paths`]).
//! Дальше: конфигурация инстанса (CPU, RAM, разрешение, DPI, FPS, рендер, microG),
//! менеджер образов Android, клонирование и снапшоты.

pub mod paths;

pub use paths::{DataDirs, PathsError};
