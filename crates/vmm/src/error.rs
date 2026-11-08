//! Ошибки VMM.

use std::path::PathBuf;

/// Результат операций VMM.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Ошибки VMM.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Ошибка гипервизора.
    #[error(transparent)]
    Whpx(#[from] rustdroid_whpx::Error),

    /// Не удалось прочитать файл.
    #[error("не удалось прочитать {path}: {source}")]
    ReadFile {
        /// Путь.
        path: PathBuf,
        /// Причина.
        source: std::io::Error,
    },

    /// Некорректный образ ядра.
    #[error("некорректный образ ядра: {0}")]
    Kernel(&'static str),

    /// Не хватает гостевой памяти.
    #[error("не хватает гостевой памяти: {0}")]
    OutOfGuestMemory(&'static str),

    /// Обращение за пределы гостевой RAM.
    #[error("адрес 0x{gpa:x} (+{len}) вне гостевой RAM")]
    GuestAddress {
        /// Адрес.
        gpa: u64,
        /// Длина.
        len: usize,
    },

    /// Не удалось выделить память хоста.
    #[error("не удалось выделить {size} байт памяти хоста под RAM гостя")]
    HostAllocation {
        /// Размер.
        size: u64,
    },

    /// Неверная конфигурация.
    #[error("неверная конфигурация: {0}")]
    Config(String),

    /// Гость остановился с невосстановимой ошибкой.
    #[error("гость остановлен: {reason} (RIP=0x{rip:x})")]
    GuestFault {
        /// Причина.
        reason: &'static str,
        /// RIP в момент ошибки.
        rip: u64,
    },
}
