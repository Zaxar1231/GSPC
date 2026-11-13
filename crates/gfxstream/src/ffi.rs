//! Типы C API gfxstream (`virtio-gpu-gfxstream-renderer.h`, версия 0.1.2).
//!
//! Раскладка структур повторяет заголовок; тесты проверяют размеры для x86_64.

#![allow(non_camel_case_types, reason = "имена как в заголовке C")]

use std::ffi::{c_char, c_int, c_void};

/// Версия API, под которую написана обёртка.
pub const STREAM_RENDERER_VERSION: (u32, u32, u32) = (0, 1, 2);

/// Буфер памяти (на Windows заголовок объявляет свой `iovec`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct iovec {
    /// Начало.
    pub iov_base: *mut c_void,
    /// Длина, байт.
    pub iov_len: usize,
}

/// Область ресурса.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct stream_renderer_box {
    /// Начало по X.
    pub x: u32,
    /// Начало по Y.
    pub y: u32,
    /// Начало по Z.
    pub z: u32,
    /// Ширина.
    pub w: u32,
    /// Высота.
    pub h: u32,
    /// Глубина.
    pub d: u32,
}

/// Аргументы создания ресурса (как `virtio_gpu_resource_create_3d`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct stream_renderer_resource_create_args {
    /// Номер ресурса.
    pub handle: u32,
    /// Тип (PIPE_TEXTURE_2D и т. п.).
    pub target: u32,
    /// Формат.
    pub format: u32,
    /// Привязки.
    pub bind: u32,
    /// Ширина.
    pub width: u32,
    /// Высота.
    pub height: u32,
    /// Глубина.
    pub depth: u32,
    /// Число слоёв.
    pub array_size: u32,
    /// Последний mip-уровень.
    pub last_level: u32,
    /// Сэмплов.
    pub nr_samples: u32,
    /// Флаги.
    pub flags: u32,
}

/// Фенс — флаг: есть кольцо.
pub const STREAM_RENDERER_FLAG_FENCE: u32 = 1 << 0;
/// Фенс — флаг: поле `ring_idx` действительно.
pub const STREAM_RENDERER_FLAG_FENCE_RING_IDX: u32 = 1 << 1;

/// Фенс.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct stream_renderer_fence {
    /// Флаги `STREAM_RENDERER_FLAG_FENCE*`.
    pub flags: u32,
    /// Номер фенса.
    pub fence_id: u64,
    /// Контекст.
    pub ctx_id: u32,
    /// Кольцо.
    pub ring_idx: u8,
}

/// Тип дескриптора: память Win32 (NT handle).
pub const STREAM_HANDLE_TYPE_MEM_OPAQUE_WIN32: u32 = 0x3;
/// Тип дескриптора: разделяемая память.
pub const STREAM_HANDLE_TYPE_MEM_SHM: u32 = 0x4;
/// Тип дескриптора: синхронизация Win32.
pub const STREAM_HANDLE_TYPE_SIGNAL_OPAQUE_WIN32: u32 = 0x30;

/// Дескриптор ОС.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct stream_renderer_handle {
    /// HANDLE или fd.
    pub os_handle: i64,
    /// `STREAM_HANDLE_TYPE_*`.
    pub handle_type: u32,
}

/// Уровень сообщения отладки: ошибка.
pub const STREAM_RENDERER_DEBUG_ERROR: u32 = 0x1;
/// Предупреждение.
pub const STREAM_RENDERER_DEBUG_WARN: u32 = 0x2;
/// Информация.
pub const STREAM_RENDERER_DEBUG_INFO: u32 = 0x3;
/// Отладка.
pub const STREAM_RENDERER_DEBUG_DEBUG: u32 = 0x4;

/// Сообщение отладки.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct stream_renderer_debug {
    /// `STREAM_RENDERER_DEBUG_*`.
    pub debug_type: u32,
    /// Текст (C-строка).
    pub message: *const c_char,
}

/// Колбэк фенса.
pub type stream_renderer_fence_callback =
    unsafe extern "C" fn(user_data: *mut c_void, fence: *mut stream_renderer_fence);
/// Колбэк отладки.
pub type stream_renderer_debug_callback =
    unsafe extern "C" fn(user_data: *mut c_void, debug: *mut stream_renderer_debug);

/// Параметр: пусто.
pub const STREAM_RENDERER_PARAM_NULL: u64 = 0;
/// Параметр: пользовательские данные колбэков.
pub const STREAM_RENDERER_PARAM_USER_DATA: u64 = 1;
/// Параметр: флаги рендерера.
pub const STREAM_RENDERER_PARAM_RENDERER_FLAGS: u64 = 2;
/// Параметр: колбэк фенса.
pub const STREAM_RENDERER_PARAM_FENCE_CALLBACK: u64 = 3;
/// Параметр: ширина дисплея 0.
pub const STREAM_RENDERER_PARAM_WIN0_WIDTH: u64 = 4;
/// Параметр: высота дисплея 0.
pub const STREAM_RENDERER_PARAM_WIN0_HEIGHT: u64 = 5;
/// Параметр: колбэк отладки.
pub const STREAM_RENDERER_PARAM_DEBUG_CALLBACK: u64 = 6;

/// Параметр инициализации.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct stream_renderer_param {
    /// `STREAM_RENDERER_PARAM_*`.
    pub key: u64,
    /// Значение.
    pub value: u64,
}

/// Команда контекста.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct stream_renderer_command {
    /// Контекст.
    pub ctx_id: u32,
    /// Размер команды.
    pub cmd_size: u32,
    /// Команда.
    pub cmd: *mut u8,
    /// Число входных фенсов.
    pub num_in_fences: u32,
    /// Входные фенсы.
    pub fences: *mut stream_renderer_handle,
}

/// Blob в памяти гостя.
pub const STREAM_BLOB_MEM_GUEST: u32 = 1;
/// Blob в памяти хоста (3D).
pub const STREAM_BLOB_MEM_HOST3D: u32 = 2;
/// Blob хоста с копией в госте.
pub const STREAM_BLOB_MEM_HOST3D_GUEST: u32 = 3;
/// Blob можно отобразить в гостя.
pub const STREAM_BLOB_FLAG_USE_MAPPABLE: u32 = 1;
/// Blob можно экспортировать.
pub const STREAM_BLOB_FLAG_USE_SHAREABLE: u32 = 2;

/// Аргументы создания blob.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct stream_renderer_create_blob {
    /// `STREAM_BLOB_MEM_*`.
    pub blob_mem: u32,
    /// `STREAM_BLOB_FLAG_*`.
    pub blob_flags: u32,
    /// Номер blob.
    pub blob_id: u64,
    /// Размер.
    pub size: u64,
}

/// Кэшируемость отображения: маска.
pub const STREAM_RENDERER_MAP_CACHE_MASK: u32 = 0x0f;
/// Без кэша.
pub const STREAM_RENDERER_MAP_CACHE_NONE: u32 = 0x00;
/// Кэшируемая.
pub const STREAM_RENDERER_MAP_CACHE_CACHED: u32 = 0x01;
/// Некэшируемая.
pub const STREAM_RENDERER_MAP_CACHE_UNCACHED: u32 = 0x02;
/// Write-combined.
pub const STREAM_RENDERER_MAP_CACHE_WC: u32 = 0x03;

/// Флаги рендерера (`enum RendererFlags`).
pub mod flags {
    /// EGL.
    pub const USE_EGL: u32 = 1 << 0;
    /// Синхронизация потоков.
    pub const THREAD_SYNC: u32 = 1 << 1;
    /// GLX.
    pub const USE_GLX: u32 = 1 << 2;
    /// Без окна (рендер во внеэкранные буферы).
    pub const USE_SURFACELESS: u32 = 1 << 3;
    /// GLES.
    pub const USE_GLES: u32 = 1 << 4;
    /// Vulkan.
    pub const USE_VK: u32 = 1 << 5;
    /// Внешние blob.
    pub const USE_EXTERNAL_BLOB: u32 = 1 << 6;
    /// Системные blob.
    pub const USE_SYSTEM_BLOB: u32 = 1 << 7;
    /// Нативный swapchain Vulkan.
    pub const VULKAN_NATIVE_SWAPCHAIN: u32 = 1 << 8;
}

/// Сигнатуры экспортируемых функций.
pub mod fns {
    use super::*;

    /// `stream_renderer_init`.
    pub type Init = unsafe extern "C" fn(*mut stream_renderer_param, u64) -> c_int;
    /// `stream_renderer_teardown`.
    pub type Teardown = unsafe extern "C" fn();
    /// `stream_renderer_resource_create`.
    pub type ResourceCreate =
        unsafe extern "C" fn(*mut stream_renderer_resource_create_args, *mut iovec, u32) -> c_int;
    /// `stream_renderer_resource_unref`.
    pub type ResourceUnref = unsafe extern "C" fn(u32);
    /// `stream_renderer_context_destroy`.
    pub type ContextDestroy = unsafe extern "C" fn(u32);
    /// `stream_renderer_submit_cmd`.
    pub type SubmitCmd = unsafe extern "C" fn(*mut stream_renderer_command) -> c_int;
    /// `stream_renderer_transfer_read_iov`.
    pub type TransferReadIov = unsafe extern "C" fn(
        u32,
        u32,
        u32,
        u32,
        u32,
        *mut stream_renderer_box,
        u64,
        *mut iovec,
        c_int,
    ) -> c_int;
    /// `stream_renderer_transfer_write_iov`.
    pub type TransferWriteIov = unsafe extern "C" fn(
        u32,
        u32,
        c_int,
        u32,
        u32,
        *mut stream_renderer_box,
        u64,
        *mut iovec,
        u32,
    ) -> c_int;
    /// `stream_renderer_get_cap_set`.
    pub type GetCapSet = unsafe extern "C" fn(u32, *mut u32, *mut u32);
    /// `stream_renderer_fill_caps`.
    pub type FillCaps = unsafe extern "C" fn(u32, u32, *mut c_void);
    /// `stream_renderer_resource_attach_iov`.
    pub type ResourceAttachIov = unsafe extern "C" fn(c_int, *mut iovec, c_int) -> c_int;
    /// `stream_renderer_resource_detach_iov`.
    pub type ResourceDetachIov = unsafe extern "C" fn(c_int, *mut *mut iovec, *mut c_int);
    /// `stream_renderer_ctx_attach_resource` / `_detach_resource`.
    pub type CtxResource = unsafe extern "C" fn(c_int, c_int);
    /// `stream_renderer_create_blob`.
    pub type CreateBlob = unsafe extern "C" fn(
        u32,
        u32,
        *const stream_renderer_create_blob,
        *const iovec,
        u32,
        *const stream_renderer_handle,
    ) -> c_int;
    /// `stream_renderer_export_blob`.
    pub type ExportBlob = unsafe extern "C" fn(u32, *mut stream_renderer_handle) -> c_int;
    /// `stream_renderer_resource_map`.
    pub type ResourceMap = unsafe extern "C" fn(u32, *mut *mut c_void, *mut u64) -> c_int;
    /// `stream_renderer_resource_unmap`.
    pub type ResourceUnmap = unsafe extern "C" fn(u32) -> c_int;
    /// `stream_renderer_context_create`.
    pub type ContextCreate = unsafe extern "C" fn(u32, u32, *const c_char, u32) -> c_int;
    /// `stream_renderer_create_fence`.
    pub type CreateFence = unsafe extern "C" fn(*const stream_renderer_fence) -> c_int;
    /// `stream_renderer_resource_map_info`.
    pub type ResourceMapInfo = unsafe extern "C" fn(u32, *mut u32) -> c_int;
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_the_header() {
        assert_eq!(size_of::<iovec>(), 16);
        assert_eq!(size_of::<stream_renderer_box>(), 24);
        assert_eq!(size_of::<stream_renderer_resource_create_args>(), 44);
        // flags (4) + выравнивание (4) + fence_id (8) + ctx_id (4) + ring_idx (1) + 3.
        assert_eq!(size_of::<stream_renderer_fence>(), 24);
        assert_eq!(std::mem::offset_of!(stream_renderer_fence, fence_id), 8);
        assert_eq!(std::mem::offset_of!(stream_renderer_fence, ring_idx), 20);
        assert_eq!(size_of::<stream_renderer_handle>(), 16);
        assert_eq!(size_of::<stream_renderer_param>(), 16);
        assert_eq!(size_of::<stream_renderer_command>(), 32);
        assert_eq!(std::mem::offset_of!(stream_renderer_command, fences), 24);
        assert_eq!(size_of::<stream_renderer_create_blob>(), 24);
    }
}
