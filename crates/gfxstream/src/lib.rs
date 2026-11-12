//! Хостовая часть gfxstream (этап 4): загрузка `gfxstream_backend.dll` и безопасная обёртка над
//! её C API `stream_renderer_*`.
//!
//! gfxstream принимает поток команд GLES/Vulkan гостя (контексты virtio-gpu с capset
//! gfxstream) и исполняет их на видеокарте хоста. DLL собирается в CI из зафиксированного
//! коммита `google/gfxstream` (workflow `gfxstream`) и лежит рядом с `rustdroid.exe`; если её
//! нет, эмулятор работает с программным рендером, как на этапе 3.
//!
//! Модули: [`ffi`] — типы заголовка (на всех платформах, с проверкой раскладки);
//! `renderer` — загрузка и обёртка (только Windows).

pub mod ffi;
#[cfg(windows)]
mod renderer;

#[cfg(windows)]
pub use renderer::{Config, Library, Renderer, Transfer};

/// Имя библиотеки.
pub const LIBRARY_NAME: &str = "gfxstream_backend.dll";

/// Capset gfxstream для Vulkan (`VIRTIO_GPU_CAPSET_GFXSTREAM_VULKAN`).
pub const CAPSET_GFXSTREAM_VULKAN: u32 = 3;
/// Capset gfxstream для GLES (`VIRTIO_GPU_CAPSET_GFXSTREAM_GLES`).
pub const CAPSET_GFXSTREAM_GLES: u32 = 7;

/// Сработавший фенс.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence {
    /// Номер фенса.
    pub fence_id: u64,
    /// Контекст.
    pub ctx_id: u32,
    /// Кольцо, если фенс привязан к кольцу.
    pub ring_idx: Option<u8>,
}

impl From<&ffi::stream_renderer_fence> for Fence {
    fn from(f: &ffi::stream_renderer_fence) -> Self {
        Self {
            fence_id: f.fence_id,
            ctx_id: f.ctx_id,
            ring_idx: (f.flags & ffi::STREAM_RENDERER_FLAG_FENCE_RING_IDX != 0)
                .then_some(f.ring_idx),
        }
    }
}

/// Ошибки gfxstream.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Нет DLL.
    #[error("не удалось загрузить {path}: {reason}")]
    LibraryNotFound {
        /// Путь.
        path: String,
        /// Причина.
        reason: String,
    },
    /// В DLL нет функции (несовместимая версия).
    #[error("в {LIBRARY_NAME} нет функции {0} — версия DLL не подходит")]
    MissingFunction(&'static str),
    /// gfxstream уже инициализирован (это глобальное состояние процесса).
    #[error("gfxstream уже инициализирован в этом процессе")]
    AlreadyInitialized,
    /// Функция вернула ошибку.
    #[error("{call} вернула {code}")]
    Call {
        /// Функция.
        call: &'static str,
        /// Код.
        code: i32,
    },
}

/// Результат.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_ring_flag() {
        let mut raw = ffi::stream_renderer_fence {
            flags: ffi::STREAM_RENDERER_FLAG_FENCE,
            fence_id: 7,
            ctx_id: 2,
            ring_idx: 1,
        };
        assert_eq!(Fence::from(&raw).ring_idx, None);
        raw.flags |= ffi::STREAM_RENDERER_FLAG_FENCE_RING_IDX;
        assert_eq!(
            Fence::from(&raw),
            Fence {
                fence_id: 7,
                ctx_id: 2,
                ring_idx: Some(1)
            }
        );
    }
}
