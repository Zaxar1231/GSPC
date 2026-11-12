//! Проверка 3D-графики хоста для `rustdroid check`.
//!
//! Нативный код драйвера и gfxstream при неудаче может завершить процесс (`abort()` внутри
//! DLL), поэтому проверка идёт в дочернем процессе `rustdroid gpu-probe`: родитель читает его
//! вывод и код выхода и превращает их в пункты проверки. Если дочерний процесс упал, это тоже
//! результат: 3D на этой машине недоступно, будет программный рендер.
//!
//! Строки результата в stdout дочернего процесса: `OK <текст>`, `WARN <текст>`.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rustdroid_gfxstream::{
    CAPSET_GFXSTREAM_COMPOSER, CAPSET_GFXSTREAM_GLES, CAPSET_GFXSTREAM_VULKAN, Config,
    LIBRARY_NAME, ffi,
};
use rustdroid_gpu::FenceSink;
use rustdroid_gpu::gfxstream::GfxstreamBackend;
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{PCSTR, w};

use crate::preflight::Check;

/// Ссылка на установщик рантайма Visual C++ 2015–2022 (нужен gfxstream).
const VC_REDIST: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// Путь к DLL gfxstream: рядом с `rustdroid.exe`.
pub(crate) fn library_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(LIBRARY_NAME))
}

// ---------------------------------------------------------------- родитель ---

/// Запускает `rustdroid gpu-probe` и переводит его вывод в пункты проверки.
pub(crate) fn checks(verbose: u8) -> Vec<Check> {
    let Ok(exe) = std::env::current_exe() else {
        return vec![warn(
            "не удалось определить путь rustdroid.exe для проверки 3D",
        )];
    };
    let mut cmd = Command::new(exe);
    if verbose > 0 {
        cmd.arg(format!("-{}", "v".repeat(verbose.into())));
    }
    // stderr (логи драйвера и gfxstream) наследуется и попадает в лог проверки.
    let out = match cmd.arg("gpu-probe").output() {
        Ok(o) => o,
        Err(e) => return vec![warn(&format!("проверка 3D не запустилась: {e}"))],
    };
    let mut v: Vec<Check> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            if let Some(m) = l.strip_prefix("OK ") {
                Some(Check::Ok(m.to_owned()))
            } else {
                l.strip_prefix("WARN ").map(warn)
            }
        })
        .collect();
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        #[allow(
            clippy::cast_sign_loss,
            reason = "коды исключений Windows печатаются как u32"
        )]
        v.push(warn(&format!(
            "проверка 3D завершилась аварийно (код 0x{:08X}): драйвер или gfxstream упали при \
             запуске. 3D будет выключено, Android рисует программно",
            code as u32
        )));
    }
    v
}

fn warn(problem: &str) -> Check {
    Check::Warn(problem.to_owned())
}

// ---------------------------------------------------------- дочерний процесс ---

/// Тело `rustdroid gpu-probe`. Код выхода: 0 — gfxstream работает, 1 — нет.
pub(crate) fn run() -> anyhow::Result<()> {
    let vk = vulkan();
    let gfx = gfxstream();
    if vk && gfx {
        Ok(())
    } else {
        // Обычный неуспех: родитель читает строки WARN, отдельное сообщение не нужно.
        std::process::exit(1);
    }
}

#[allow(clippy::print_stdout, reason = "протокол с родительским процессом")]
fn report(ok: bool, text: &str) {
    println!("{} {text}", if ok { "OK" } else { "WARN" });
}

/// Vulkan хоста через системный загрузчик `vulkan-1.dll`.
fn vulkan() -> bool {
    match vulkan_devices() {
        Ok(devices) if devices.is_empty() => {
            report(
                false,
                "Vulkan: драйвер не сообщает ни одной видеокарты. Нужен уровень графики без \
                 Vulkan (задача 4.6)",
            );
            false
        }
        Ok(devices) => {
            for d in &devices {
                report(true, &format!("Vulkan: {d}"));
            }
            true
        }
        Err(e) => {
            report(false, &format!("Vulkan: {e}"));
            false
        }
    }
}

/// gfxstream: загрузка DLL и инициализация на Vulkan.
fn gfxstream() -> bool {
    let Some(path) = library_path() else {
        report(
            false,
            "gfxstream: не удалось определить каталог rustdroid.exe",
        );
        return false;
    };
    if !path.exists() {
        report(
            false,
            &format!(
                "gfxstream: нет {LIBRARY_NAME} рядом с rustdroid.exe — 3D выключено (её кладёт \
                 hw-test.ps1 из pre-release deps-gfxstream)"
            ),
        );
        return false;
    }
    match probe_gfxstream(&path) {
        Ok(capsets) => {
            report(
                true,
                &format!("gfxstream: инициализирован на Vulkan, capsets: {capsets}"),
            );
            true
        }
        Err(rustdroid_gfxstream::Error::LibraryNotFound { reason, .. }) => {
            report(
                false,
                &format!(
                    "gfxstream: DLL не загрузилась ({reason}). Чаще всего не хватает рантайма \
                     Visual C++ 2015–2022: {VC_REDIST}"
                ),
            );
            false
        }
        Err(e) => {
            report(
                false,
                &format!("gfxstream: не запустился ({e}). Подробности — в логе с -v"),
            );
            false
        }
    }
}

fn probe_gfxstream(path: &Path) -> Result<String, rustdroid_gfxstream::Error> {
    let backend = GfxstreamBackend::new(
        path,
        Config {
            flags: ffi::flags::USE_VK,
            width: 1280,
            height: 720,
        },
        Arc::new(FenceSink::default()),
    )?;
    use rustdroid_gpu::Backend3d as _;
    let names: Vec<String> = (0..backend.num_capsets())
        .filter_map(|i| backend.capset_info(i))
        .map(|c| {
            let name = match c.id {
                CAPSET_GFXSTREAM_VULKAN => "vulkan",
                CAPSET_GFXSTREAM_GLES => "gles",
                CAPSET_GFXSTREAM_COMPOSER => "composer",
                _ => "?",
            };
            format!("{name} v{} ({} Б)", c.max_version, c.max_size)
        })
        .collect();
    Ok(if names.is_empty() {
        "нет".into()
    } else {
        names.join(", ")
    })
}

// ------------------------------------------------------------ Vulkan FFI ---

const VK_SUCCESS: i32 = 0;
const VK_INCOMPLETE: i32 = 5;
const VK_STRUCTURE_TYPE_APPLICATION_INFO: u32 = 0;
const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: u32 = 1;
const fn vk_version(major: u32, minor: u32) -> u32 {
    (major << 22) | (minor << 12)
}

#[repr(C)]
struct VkApplicationInfo {
    s_type: u32,
    p_next: *const c_void,
    p_application_name: *const c_char,
    application_version: u32,
    p_engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct VkInstanceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    p_application_info: *const VkApplicationInfo,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
}

type Instance = *mut c_void;
type PhysicalDevice = *mut c_void;
type GetInstanceProcAddr = unsafe extern "system" fn(Instance, *const c_char) -> *const c_void;
type CreateInstance =
    unsafe extern "system" fn(*const VkInstanceCreateInfo, *const c_void, *mut Instance) -> i32;
type DestroyInstance = unsafe extern "system" fn(Instance, *const c_void);
type EnumerateInstanceVersion = unsafe extern "system" fn(*mut u32) -> i32;
type EnumeratePhysicalDevices =
    unsafe extern "system" fn(Instance, *mut u32, *mut PhysicalDevice) -> i32;
type GetPhysicalDeviceProperties = unsafe extern "system" fn(PhysicalDevice, *mut c_void);

/// Начало `VkPhysicalDeviceProperties`; вся структура меньше 1 КиБ, буфер с запасом.
#[repr(C, align(8))]
struct PropsBuf([u8; 2048]);

fn version_str(v: u32) -> String {
    format!("{}.{}.{}", (v >> 22) & 0x7F, (v >> 12) & 0x3FF, v & 0xFFF)
}

fn device_type(t: u32) -> &'static str {
    match t {
        1 => "встроенная",
        2 => "дискретная",
        3 => "виртуальная",
        4 => "программная",
        _ => "другая",
    }
}

fn vulkan_devices() -> Result<Vec<String>, String> {
    // SAFETY: имя DLL — статическая строка; поиск только в System32 (загрузчик Vulkan
    // ставится туда драйвером).
    let module = unsafe { LoadLibraryExW(w!("vulkan-1.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| {
            "нет vulkan-1.dll — драйвер видеокарты не поддерживает Vulkan или устарел".to_owned()
        })?;
    // SAFETY: модуль загружен, имя функции нуль-терминировано.
    let gipa = unsafe { GetProcAddress(module, PCSTR(c"vkGetInstanceProcAddr".as_ptr().cast())) }
        .ok_or("в vulkan-1.dll нет vkGetInstanceProcAddr")?;
    // SAFETY: сигнатура vkGetInstanceProcAddr из спецификации Vulkan.
    let gipa: GetInstanceProcAddr = unsafe { std::mem::transmute(gipa) };
    let result = enumerate(gipa);
    // SAFETY: модуль загружен выше; экземпляр Vulkan уже уничтожен в `enumerate`.
    let _ = unsafe { FreeLibrary(module) };
    result
}

/// Загружает функцию Vulkan; `F` — её точная сигнатура.
///
/// # Safety
/// `F` должна совпадать с сигнатурой функции `name` из спецификации Vulkan.
unsafe fn vk_fn<F: Copy>(gipa: GetInstanceProcAddr, instance: Instance, name: &CStr) -> Option<F> {
    // SAFETY: gipa — функция загрузчика; имя нуль-терминировано.
    let p = unsafe { gipa(instance, name.as_ptr()) };
    if p.is_null() {
        return None;
    }
    debug_assert_eq!(size_of::<F>(), size_of::<*const c_void>());
    // SAFETY: указатель на функцию той же сигнатуры (гарантирует вызывающий).
    Some(unsafe { std::mem::transmute_copy::<*const c_void, F>(&p) })
}

fn enumerate(gipa: GetInstanceProcAddr) -> Result<Vec<String>, String> {
    let null = std::ptr::null_mut();
    // SAFETY: сигнатуры функций — из спецификации Vulkan.
    let version: Option<EnumerateInstanceVersion> =
        unsafe { vk_fn(gipa, null, c"vkEnumerateInstanceVersion") };
    let mut loader = vk_version(1, 0);
    if let Some(f) = version {
        // SAFETY: выходной указатель — локальная переменная.
        let _ = unsafe { f(&raw mut loader) };
    }
    // SAFETY: как выше.
    let create: CreateInstance =
        unsafe { vk_fn(gipa, null, c"vkCreateInstance") }.ok_or("нет vkCreateInstance")?;
    let app = VkApplicationInfo {
        s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        p_next: std::ptr::null(),
        p_application_name: c"rustdroid-check".as_ptr(),
        application_version: 1,
        p_engine_name: std::ptr::null(),
        engine_version: 0,
        api_version: vk_version(1, 1).min(loader),
    };
    let info = VkInstanceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: 0,
        p_application_info: &raw const app,
        enabled_layer_count: 0,
        pp_enabled_layer_names: std::ptr::null(),
        enabled_extension_count: 0,
        pp_enabled_extension_names: std::ptr::null(),
    };
    let mut instance: Instance = null;
    // SAFETY: структуры валидны на время вызова, выход — локальная переменная.
    let rc = unsafe { create(&raw const info, std::ptr::null(), &raw mut instance) };
    if rc != VK_SUCCESS {
        return Err(format!(
            "загрузчик Vulkan {} есть, но vkCreateInstance вернул {rc} (драйвер не поддерживает \
             Vulkan 1.1 или ICD повреждён)",
            version_str(loader)
        ));
    }
    let devices = list_devices(gipa, instance);
    // SAFETY: как выше.
    if let Some(destroy) = unsafe { vk_fn::<DestroyInstance>(gipa, instance, c"vkDestroyInstance") }
    {
        // SAFETY: экземпляр создан выше и больше не используется.
        unsafe { destroy(instance, std::ptr::null()) };
    }
    devices
}

fn list_devices(gipa: GetInstanceProcAddr, instance: Instance) -> Result<Vec<String>, String> {
    // SAFETY: сигнатуры функций — из спецификации Vulkan.
    let enumerate: EnumeratePhysicalDevices =
        unsafe { vk_fn(gipa, instance, c"vkEnumeratePhysicalDevices") }
            .ok_or("нет vkEnumeratePhysicalDevices")?;
    // SAFETY: как выше.
    let props: GetPhysicalDeviceProperties =
        unsafe { vk_fn(gipa, instance, c"vkGetPhysicalDeviceProperties") }
            .ok_or("нет vkGetPhysicalDeviceProperties")?;
    let mut n = 0_u32;
    // SAFETY: запрос количества, массив не передаётся.
    let rc = unsafe { enumerate(instance, &raw mut n, std::ptr::null_mut()) };
    if rc != VK_SUCCESS {
        return Err(format!("vkEnumeratePhysicalDevices вернул {rc}"));
    }
    let mut handles = vec![std::ptr::null_mut(); n as usize];
    // SAFETY: массив на `n` элементов.
    let rc = unsafe { enumerate(instance, &raw mut n, handles.as_mut_ptr()) };
    if rc != VK_SUCCESS && rc != VK_INCOMPLETE {
        return Err(format!("vkEnumeratePhysicalDevices вернул {rc}"));
    }
    handles.truncate(n as usize);
    Ok(handles
        .into_iter()
        .map(|h| {
            let mut buf = Box::new(PropsBuf([0; 2048]));
            // SAFETY: буфер больше VkPhysicalDeviceProperties и выровнен на 8.
            unsafe { props(h, buf.0.as_mut_ptr().cast()) };
            let b = &buf.0;
            let u = |off: usize| {
                b.get(off..off + 4)
                    .and_then(|s| s.try_into().ok())
                    .map_or(0, u32::from_le_bytes)
            };
            // apiVersion, driverVersion, vendorID, deviceID, deviceType, deviceName[256].
            let name = b
                .get(20..276)
                .and_then(|s| CStr::from_bytes_until_nul(s).ok())
                .map_or_else(|| "?".into(), |c| c.to_string_lossy().into_owned());
            format!(
                "{name} ({}, Vulkan {}, vendor 0x{:04x})",
                device_type(u(16)),
                version_str(u(0)),
                u(8)
            )
        })
        .collect())
}
