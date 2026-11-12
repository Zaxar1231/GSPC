//! Загрузка `gfxstream_backend.dll` и обёртка над `stream_renderer_*` (Windows).

use std::ffi::{CStr, c_int, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LoadLibraryExW,
};
use windows::core::{PCSTR, PCWSTR};

use crate::ffi::{self, fns};
use crate::{Error, Fence, Result};

/// Функции DLL.
#[derive(Clone, Copy)]
pub struct Library {
    init: fns::Init,
    teardown: fns::Teardown,
    resource_create: fns::ResourceCreate,
    resource_unref: fns::ResourceUnref,
    context_create: fns::ContextCreate,
    context_destroy: fns::ContextDestroy,
    submit_cmd: fns::SubmitCmd,
    transfer_read_iov: fns::TransferReadIov,
    transfer_write_iov: fns::TransferWriteIov,
    get_cap_set: fns::GetCapSet,
    fill_caps: fns::FillCaps,
    resource_attach_iov: fns::ResourceAttachIov,
    resource_detach_iov: fns::ResourceDetachIov,
    ctx_attach_resource: fns::CtxResource,
    ctx_detach_resource: fns::CtxResource,
    create_blob: fns::CreateBlob,
    export_blob: fns::ExportBlob,
    resource_map: fns::ResourceMap,
    resource_unmap: fns::ResourceUnmap,
    resource_map_info: fns::ResourceMapInfo,
    create_fence: fns::CreateFence,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

/// Ищет функцию `name` (нуль-терминированная) и приводит её к типу `F`.
///
/// # Safety
/// `F` — указатель на функцию с ABI и сигнатурой экспорта `name`.
unsafe fn symbol<F: Copy>(module: HMODULE, name: &'static [u8]) -> Result<F> {
    let display = std::str::from_utf8(name.strip_suffix(b"\0").unwrap_or(name)).unwrap_or("?");
    // SAFETY: `name` нуль-терминирован (литерал с `\0` в макросе ниже).
    let proc = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }
        .ok_or(Error::MissingFunction(display))?;
    // SAFETY: FARPROC и F — указатели на функцию одного размера; сигнатуру F гарантирует
    // вызывающий (типы в `ffi::fns` повторяют заголовок).
    Ok(unsafe { std::mem::transmute_copy::<_, F>(&proc) })
}

macro_rules! sym {
    ($m:expr, $name:literal) => {
        // SAFETY: типы в `ffi::fns` повторяют сигнатуры virtio-gpu-gfxstream-renderer.h.
        unsafe { symbol($m, concat!($name, "\0").as_bytes())? }
    };
}

impl Library {
    /// Загружает DLL по абсолютному пути. Её зависимости ищутся в её же каталоге и в
    /// системных, но не в текущем каталоге.
    ///
    /// # Errors
    /// DLL нет или в ней нет нужных функций.
    pub fn load(path: &Path) -> Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: путь — нуль-терминированная UTF-16 строка, живущая до конца вызова.
        let module = unsafe {
            LoadLibraryExW(
                PCWSTR(wide.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map_err(|e| Error::LibraryNotFound {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        // Модуль не выгружается: gfxstream держит глобальное состояние и потоки.
        Ok(Self {
            init: sym!(module, "stream_renderer_init"),
            teardown: sym!(module, "stream_renderer_teardown"),
            resource_create: sym!(module, "stream_renderer_resource_create"),
            resource_unref: sym!(module, "stream_renderer_resource_unref"),
            context_create: sym!(module, "stream_renderer_context_create"),
            context_destroy: sym!(module, "stream_renderer_context_destroy"),
            submit_cmd: sym!(module, "stream_renderer_submit_cmd"),
            transfer_read_iov: sym!(module, "stream_renderer_transfer_read_iov"),
            transfer_write_iov: sym!(module, "stream_renderer_transfer_write_iov"),
            get_cap_set: sym!(module, "stream_renderer_get_cap_set"),
            fill_caps: sym!(module, "stream_renderer_fill_caps"),
            resource_attach_iov: sym!(module, "stream_renderer_resource_attach_iov"),
            resource_detach_iov: sym!(module, "stream_renderer_resource_detach_iov"),
            ctx_attach_resource: sym!(module, "stream_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!(module, "stream_renderer_ctx_detach_resource"),
            create_blob: sym!(module, "stream_renderer_create_blob"),
            export_blob: sym!(module, "stream_renderer_export_blob"),
            resource_map: sym!(module, "stream_renderer_resource_map"),
            resource_unmap: sym!(module, "stream_renderer_resource_unmap"),
            resource_map_info: sym!(module, "stream_renderer_resource_map_info"),
            create_fence: sym!(module, "stream_renderer_create_fence"),
        })
    }
}

/// Параметры инициализации.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Флаги [`ffi::flags`].
    pub flags: u32,
    /// Размер дисплея 0.
    pub width: u32,
    /// Высота дисплея 0.
    pub height: u32,
}

/// Параметры копирования между памятью гостя и ресурсом (`TRANSFER_*_3D`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Transfer {
    /// Ресурс.
    pub handle: u32,
    /// Контекст.
    pub ctx_id: u32,
    /// Mip-уровень.
    pub level: u32,
    /// Шаг строки.
    pub stride: u32,
    /// Шаг слоя.
    pub layer_stride: u32,
    /// Область.
    pub region: ffi::stream_renderer_box,
    /// Смещение в памяти гостя.
    pub offset: u64,
}

/// Колбэки, на которые указывает `user_data` gfxstream. Живут, пока жив [`Renderer`].
struct Callbacks {
    on_fence: Box<dyn Fn(Fence) + Send + Sync>,
}

unsafe extern "C" fn fence_trampoline(
    user_data: *mut c_void,
    fence: *mut ffi::stream_renderer_fence,
) {
    if user_data.is_null() || fence.is_null() {
        return;
    }
    // SAFETY: user_data — указатель на `Callbacks` из `Renderer::init`; он живёт до
    // `stream_renderer_teardown`, после которого колбэки не вызываются.
    let cb = unsafe { &*user_data.cast::<Callbacks>() };
    // SAFETY: gfxstream передаёт указатель на валидный фенс на время вызова.
    let f = unsafe { &*fence };
    (cb.on_fence)(Fence::from(f));
}

unsafe extern "C" fn debug_trampoline(
    _user_data: *mut c_void,
    debug: *mut ffi::stream_renderer_debug,
) {
    if debug.is_null() {
        return;
    }
    // SAFETY: gfxstream передаёт указатель на валидную структуру на время вызова.
    let d = unsafe { &*debug };
    if d.message.is_null() {
        return;
    }
    // SAFETY: message — нуль-терминированная строка gfxstream, живущая на время вызова.
    let msg = unsafe { CStr::from_ptr(d.message) }.to_string_lossy();
    match d.debug_type {
        ffi::STREAM_RENDERER_DEBUG_ERROR => tracing::error!(target: "gfxstream", "{msg}"),
        ffi::STREAM_RENDERER_DEBUG_WARN => tracing::warn!(target: "gfxstream", "{msg}"),
        ffi::STREAM_RENDERER_DEBUG_INFO => tracing::info!(target: "gfxstream", "{msg}"),
        _ => tracing::debug!(target: "gfxstream", "{msg}"),
    }
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);

fn check(call: &'static str, code: c_int) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Call { call, code })
    }
}

/// Инициализированный gfxstream. В процессе может быть только один.
pub struct Renderer {
    lib: Library,
    _callbacks: Box<Callbacks>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Renderer {
    /// Инициализирует gfxstream. `on_fence` вызывается из потоков gfxstream, когда фенс
    /// гостя выполнен.
    ///
    /// # Errors
    /// gfxstream уже инициализирован или вернул ошибку.
    pub fn init(
        lib: Library,
        config: Config,
        on_fence: Box<dyn Fn(Fence) + Send + Sync>,
    ) -> Result<Self> {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return Err(Error::AlreadyInitialized);
        }
        let callbacks = Box::new(Callbacks { on_fence });
        let user_data = std::ptr::from_ref::<Callbacks>(&callbacks) as u64;
        let mut params = [
            ffi::stream_renderer_param {
                key: ffi::STREAM_RENDERER_PARAM_USER_DATA,
                value: user_data,
            },
            ffi::stream_renderer_param {
                key: ffi::STREAM_RENDERER_PARAM_RENDERER_FLAGS,
                value: u64::from(config.flags),
            },
            ffi::stream_renderer_param {
                key: ffi::STREAM_RENDERER_PARAM_FENCE_CALLBACK,
                value: fence_trampoline as ffi::stream_renderer_fence_callback as usize as u64,
            },
            ffi::stream_renderer_param {
                key: ffi::STREAM_RENDERER_PARAM_WIN0_WIDTH,
                value: u64::from(config.width),
            },
            ffi::stream_renderer_param {
                key: ffi::STREAM_RENDERER_PARAM_WIN0_HEIGHT,
                value: u64::from(config.height),
            },
            ffi::stream_renderer_param {
                key: ffi::STREAM_RENDERER_PARAM_DEBUG_CALLBACK,
                value: debug_trampoline as ffi::stream_renderer_debug_callback as usize as u64,
            },
        ];
        // SAFETY: массив параметров валиден на время вызова; user_data указывает на
        // `callbacks`, который живёт в `Renderer` до teardown.
        let code = unsafe { (lib.init)(params.as_mut_ptr(), params.len() as u64) };
        if code != 0 {
            INITIALIZED.store(false, Ordering::Release);
            return Err(Error::Call {
                call: "stream_renderer_init",
                code,
            });
        }
        Ok(Self {
            lib,
            _callbacks: callbacks,
        })
    }

    /// Максимальные версия и размер capset `set` (`None`, если gfxstream его не знает).
    #[must_use]
    pub fn capset_info(&self, set: u32) -> Option<(u32, u32)> {
        let (mut ver, mut size) = (0_u32, 0_u32);
        // SAFETY: выходные указатели — локальные переменные.
        unsafe { (self.lib.get_cap_set)(set, &raw mut ver, &raw mut size) };
        (size > 0).then_some((ver, size))
    }

    /// Заполняет описание capset `set` версии `version`.
    #[must_use]
    pub fn capset(&self, set: u32, version: u32) -> Vec<u8> {
        let Some((_, size)) = self.capset_info(set) else {
            return Vec::new();
        };
        let mut buf = vec![0_u8; size as usize];
        // SAFETY: буфер размером, который сообщил gfxstream для этого capset.
        unsafe { (self.lib.fill_caps)(set, version, buf.as_mut_ptr().cast()) };
        buf
    }

    /// Создаёт контекст `ctx_id` гостя.
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn context_create(&self, ctx_id: u32, name: &[u8], context_init: u32) -> Result<()> {
        let len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        // SAFETY: имя валидно на время вызова, длина передаётся явно.
        check("stream_renderer_context_create", unsafe {
            (self.lib.context_create)(ctx_id, len, name.as_ptr().cast(), context_init)
        })
    }

    /// Уничтожает контекст.
    pub fn context_destroy(&self, ctx_id: u32) {
        // SAFETY: номер контекста — простое значение.
        unsafe { (self.lib.context_destroy)(ctx_id) };
    }

    /// Передаёт команды контекста.
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn submit(&self, ctx_id: u32, cmd: &mut [u8]) -> Result<()> {
        let mut c = ffi::stream_renderer_command {
            ctx_id,
            cmd_size: u32::try_from(cmd.len()).unwrap_or(u32::MAX),
            cmd: cmd.as_mut_ptr(),
            num_in_fences: 0,
            fences: std::ptr::null_mut(),
        };
        // SAFETY: буфер команд валиден на время вызова.
        check("stream_renderer_submit_cmd", unsafe {
            (self.lib.submit_cmd)(&raw mut c)
        })
    }

    /// Ставит фенс; когда он выполнится, gfxstream вызовет `on_fence`.
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn create_fence(&self, fence_id: u64, ctx_id: u32, ring_idx: Option<u8>) -> Result<()> {
        let f = ffi::stream_renderer_fence {
            flags: ffi::STREAM_RENDERER_FLAG_FENCE
                | if ring_idx.is_some() {
                    ffi::STREAM_RENDERER_FLAG_FENCE_RING_IDX
                } else {
                    0
                },
            fence_id,
            ctx_id,
            ring_idx: ring_idx.unwrap_or(0),
        };
        // SAFETY: структура валидна на время вызова.
        check("stream_renderer_create_fence", unsafe {
            (self.lib.create_fence)(&raw const f)
        })
    }

    /// Создаёт ресурс 3D с памятью гостя `iov`.
    ///
    /// # Safety
    /// Буферы `iov` (память гостя) должны оставаться отображёнными, пока ресурс жив или пока
    /// не вызван [`Self::resource_detach_iov`].
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub unsafe fn resource_create(
        &self,
        args: &ffi::stream_renderer_resource_create_args,
        iov: &mut [ffi::iovec],
    ) -> Result<()> {
        let mut a = *args;
        // SAFETY: аргументы и массив iov валидны на время вызова; время жизни памяти
        // гарантирует вызывающий.
        check("stream_renderer_resource_create", unsafe {
            (self.lib.resource_create)(
                &raw mut a,
                iov.as_mut_ptr(),
                u32::try_from(iov.len()).unwrap_or(0),
            )
        })
    }

    /// Освобождает ресурс.
    pub fn resource_unref(&self, handle: u32) {
        // SAFETY: номер ресурса — простое значение.
        unsafe { (self.lib.resource_unref)(handle) };
    }

    /// Привязывает к ресурсу память гостя.
    ///
    /// # Safety
    /// Как у [`Self::resource_create`].
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub unsafe fn resource_attach_iov(&self, handle: u32, iov: &mut [ffi::iovec]) -> Result<()> {
        let h = c_int::try_from(handle).unwrap_or(c_int::MAX);
        let n = c_int::try_from(iov.len()).unwrap_or(0);
        // SAFETY: массив валиден на время вызова; время жизни памяти — на вызывающем.
        check("stream_renderer_resource_attach_iov", unsafe {
            (self.lib.resource_attach_iov)(h, iov.as_mut_ptr(), n)
        })
    }

    /// Отвязывает память гостя от ресурса.
    pub fn resource_detach_iov(&self, handle: u32) {
        let h = c_int::try_from(handle).unwrap_or(c_int::MAX);
        // SAFETY: выходные указатели — null (gfxstream их тогда не заполняет).
        unsafe { (self.lib.resource_detach_iov)(h, std::ptr::null_mut(), std::ptr::null_mut()) };
    }

    /// Делает ресурс видимым контексту.
    pub fn ctx_attach_resource(&self, ctx_id: u32, handle: u32) {
        let (c, h) = (
            c_int::try_from(ctx_id).unwrap_or(0),
            c_int::try_from(handle).unwrap_or(0),
        );
        // SAFETY: простые значения.
        unsafe { (self.lib.ctx_attach_resource)(c, h) };
    }

    /// Убирает ресурс из контекста.
    pub fn ctx_detach_resource(&self, ctx_id: u32, handle: u32) {
        let (c, h) = (
            c_int::try_from(ctx_id).unwrap_or(0),
            c_int::try_from(handle).unwrap_or(0),
        );
        // SAFETY: простые значения.
        unsafe { (self.lib.ctx_detach_resource)(c, h) };
    }

    /// Копирует данные из памяти гостя в ресурс (`to_host = true`) или обратно.
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn transfer(&self, to_host: bool, t: &Transfer) -> Result<()> {
        let mut region = t.region;
        let code = if to_host {
            // SAFETY: используется память, привязанная к ресурсу (iov = null); область валидна.
            unsafe {
                (self.lib.transfer_write_iov)(
                    t.handle,
                    t.ctx_id,
                    c_int::try_from(t.level).unwrap_or(0),
                    t.stride,
                    t.layer_stride,
                    &raw mut region,
                    t.offset,
                    std::ptr::null_mut(),
                    0,
                )
            }
        } else {
            // SAFETY: как выше.
            unsafe {
                (self.lib.transfer_read_iov)(
                    t.handle,
                    t.ctx_id,
                    t.level,
                    t.stride,
                    t.layer_stride,
                    &raw mut region,
                    t.offset,
                    std::ptr::null_mut(),
                    0,
                )
            }
        };
        check("stream_renderer_transfer_iov", code)
    }

    /// Создаёт blob-ресурс.
    ///
    /// # Safety
    /// Как у [`Self::resource_create`] для `iov` (для `BLOB_MEM_GUEST`).
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub unsafe fn create_blob(
        &self,
        ctx_id: u32,
        handle: u32,
        blob: &ffi::stream_renderer_create_blob,
        iov: &[ffi::iovec],
    ) -> Result<()> {
        // SAFETY: структуры валидны на время вызова; время жизни памяти — на вызывающем.
        check("stream_renderer_create_blob", unsafe {
            (self.lib.create_blob)(
                ctx_id,
                handle,
                std::ptr::from_ref(blob),
                iov.as_ptr(),
                u32::try_from(iov.len()).unwrap_or(0),
                std::ptr::null(),
            )
        })
    }

    /// Экспортирует blob как дескриптор ОС (на Windows — NT handle памяти).
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn export_blob(&self, handle: u32) -> Result<ffi::stream_renderer_handle> {
        let mut out = ffi::stream_renderer_handle::default();
        // SAFETY: выходная структура локальная.
        check("stream_renderer_export_blob", unsafe {
            (self.lib.export_blob)(handle, &raw mut out)
        })?;
        Ok(out)
    }

    /// Отображает blob в адресное пространство хоста: указатель и размер.
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn resource_map(&self, handle: u32) -> Result<(*mut c_void, u64)> {
        let (mut ptr, mut size) = (std::ptr::null_mut(), 0_u64);
        // SAFETY: выходные указатели — локальные переменные.
        check("stream_renderer_resource_map", unsafe {
            (self.lib.resource_map)(handle, &raw mut ptr, &raw mut size)
        })?;
        Ok((ptr, size))
    }

    /// Снимает отображение blob.
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn resource_unmap(&self, handle: u32) -> Result<()> {
        // SAFETY: простое значение.
        check("stream_renderer_resource_unmap", unsafe {
            (self.lib.resource_unmap)(handle)
        })
    }

    /// Кэшируемость отображения blob (`STREAM_RENDERER_MAP_CACHE_*`).
    ///
    /// # Errors
    /// Ошибка gfxstream.
    pub fn resource_map_info(&self, handle: u32) -> Result<u32> {
        let mut info = 0_u32;
        // SAFETY: выходной указатель — локальная переменная.
        check("stream_renderer_resource_map_info", unsafe {
            (self.lib.resource_map_info)(handle, &raw mut info)
        })?;
        Ok(info)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: gfxstream инициализирован этим объектом; после teardown колбэки не
        // вызываются, и `_callbacks` можно освободить.
        unsafe { (self.lib.teardown)() };
        INITIALIZED.store(false, Ordering::Release);
    }
}
