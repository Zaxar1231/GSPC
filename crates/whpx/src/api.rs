//! Динамическая загрузка `WinHvPlatform.dll` и `WinHvEmulation.dll`.
//!
//! Функции загружаются через `LoadLibraryExW` + `GetProcAddress`, а не статическим импортом.
//! Если компонент Windows Hypervisor Platform не включён, приложение всё равно запускается и
//! показывает понятное сообщение, а не системное окно «не найдена DLL».

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::System::Hypervisor::{
    WHV_CAPABILITY_CODE, WHV_DOORBELL_MATCH_DATA, WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_STATUS,
    WHV_INTERRUPT_CONTROL, WHV_MAP_GPA_RANGE_FLAGS, WHV_MEMORY_ACCESS_CONTEXT,
    WHV_PARTITION_HANDLE, WHV_PARTITION_PROPERTY_CODE, WHV_REGISTER_NAME, WHV_REGISTER_VALUE,
    WHV_TRANSLATE_GVA_FLAGS, WHV_TRANSLATE_GVA_RESULT, WHV_VP_EXIT_CONTEXT,
    WHV_X64_IO_PORT_ACCESS_CONTEXT,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{HRESULT, PCSTR, PCWSTR, w};

use crate::error::{Error, Result};

const PLATFORM_DLL: &str = "WinHvPlatform.dll";
const EMULATION_DLL: &str = "WinHvEmulation.dll";

pub(crate) type FnGetCapability =
    unsafe extern "system" fn(WHV_CAPABILITY_CODE, *mut c_void, u32, *mut u32) -> HRESULT;
pub(crate) type FnCreatePartition = unsafe extern "system" fn(*mut WHV_PARTITION_HANDLE) -> HRESULT;
pub(crate) type FnPartition = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
pub(crate) type FnSetPartitionProperty = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE,
    *const c_void,
    u32,
) -> HRESULT;
pub(crate) type FnMapGpaRange = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    *const c_void,
    u64,
    u64,
    WHV_MAP_GPA_RANGE_FLAGS,
) -> HRESULT;
pub(crate) type FnUnmapGpaRange =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, u64, u64) -> HRESULT;
pub(crate) type FnVpWithFlags =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, u32) -> HRESULT;
pub(crate) type FnDeleteVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32) -> HRESULT;
pub(crate) type FnRunVp =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, *mut c_void, u32) -> HRESULT;
pub(crate) type FnGetVpRegisters = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *mut WHV_REGISTER_VALUE,
) -> HRESULT;
pub(crate) type FnSetVpRegisters = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *const WHV_REGISTER_VALUE,
) -> HRESULT;
pub(crate) type FnRequestInterrupt =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, *const WHV_INTERRUPT_CONTROL, u32) -> HRESULT;
pub(crate) type FnTranslateGva = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    u64,
    WHV_TRANSLATE_GVA_FLAGS,
    *mut WHV_TRANSLATE_GVA_RESULT,
    *mut u64,
) -> HRESULT;

pub(crate) type FnRegisterDoorbell = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    *const WHV_DOORBELL_MATCH_DATA,
    HANDLE,
) -> HRESULT;
pub(crate) type FnUnregisterDoorbell =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, *const WHV_DOORBELL_MATCH_DATA) -> HRESULT;

/// `WHvGetVirtualProcessorCounters(partition, vp, counter_set, buffer, size, written)`.
pub(crate) type FnGetVpCounters = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    i32,
    *mut c_void,
    u32,
    *mut u32,
) -> HRESULT;
pub(crate) type FnEmulatorCreate =
    unsafe extern "system" fn(*const WHV_EMULATOR_CALLBACKS, *mut *mut c_void) -> HRESULT;
pub(crate) type FnEmulatorDestroy = unsafe extern "system" fn(*const c_void) -> HRESULT;
pub(crate) type FnEmulatorTryIo = unsafe extern "system" fn(
    *const c_void,
    *const c_void,
    *const WHV_VP_EXIT_CONTEXT,
    *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> HRESULT;
pub(crate) type FnEmulatorTryMmio = unsafe extern "system" fn(
    *const c_void,
    *const c_void,
    *const WHV_VP_EXIT_CONTEXT,
    *const WHV_MEMORY_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> HRESULT;

/// Таблица функций `WinHvPlatform.dll`.
pub(crate) struct Platform {
    pub get_capability: FnGetCapability,
    pub create_partition: FnCreatePartition,
    pub setup_partition: FnPartition,
    pub delete_partition: FnPartition,
    pub set_partition_property: FnSetPartitionProperty,
    pub map_gpa_range: FnMapGpaRange,
    pub unmap_gpa_range: FnUnmapGpaRange,
    pub create_vp: FnVpWithFlags,
    pub delete_vp: FnDeleteVp,
    pub run_vp: FnRunVp,
    pub cancel_run_vp: FnVpWithFlags,
    pub get_vp_registers: FnGetVpRegisters,
    pub set_vp_registers: FnSetVpRegisters,
    pub request_interrupt: FnRequestInterrupt,
    pub translate_gva: FnTranslateGva,
    pub register_doorbell: FnRegisterDoorbell,
    pub unregister_doorbell: FnUnregisterDoorbell,
    pub get_vp_counters: FnGetVpCounters,
}

/// Таблица функций `WinHvEmulation.dll`.
pub(crate) struct Emulation {
    pub create: FnEmulatorCreate,
    pub destroy: FnEmulatorDestroy,
    pub try_io: FnEmulatorTryIo,
    pub try_mmio: FnEmulatorTryMmio,
}

static PLATFORM: OnceLock<Result<Platform>> = OnceLock::new();
static EMULATION: OnceLock<Result<Emulation>> = OnceLock::new();

/// Функции `WinHvPlatform.dll` (загружаются один раз).
pub(crate) fn platform() -> Result<&'static Platform> {
    PLATFORM
        .get_or_init(load_platform)
        .as_ref()
        .map_err(Clone::clone)
}

/// Функции `WinHvEmulation.dll` (загружаются один раз).
pub(crate) fn emulation() -> Result<&'static Emulation> {
    EMULATION
        .get_or_init(load_emulation)
        .as_ref()
        .map_err(Clone::clone)
}

fn load_library(name: PCWSTR, display: &'static str) -> Result<HMODULE> {
    // SAFETY: имя — статическая нуль-терминированная UTF-16 строка; поиск ограничен System32,
    // поэтому библиотека не может быть подменена из текущего каталога.
    unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| Error::LibraryNotFound(display))
}

/// Ищет функцию и приводит её к типу `F`.
///
/// # Safety
/// `F` должен быть указателем на функцию с тем же ABI и сигнатурой, что у экспортируемой
/// функции `name` (`name` — нуль-терминированная строка).
unsafe fn symbol<F: Copy>(
    module: HMODULE,
    library: &'static str,
    name: &'static [u8],
) -> Result<F> {
    let function = std::str::from_utf8(name.strip_suffix(b"\0").unwrap_or(name)).unwrap_or("?");
    // SAFETY: `name` нуль-терминирован (проверяется вызывающими через литерал с `\0`).
    let proc = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }
        .ok_or(Error::MissingFunction { library, function })?;
    debug_assert_eq!(size_of::<F>(), size_of_val(&proc));
    // SAFETY: FARPROC и F — указатели на функцию одного размера; вызывающий гарантирует,
    // что сигнатура F совпадает с реальной сигнатурой экспорта.
    Ok(unsafe { std::mem::transmute_copy::<_, F>(&proc) })
}

macro_rules! sym {
    ($module:expr, $lib:expr, $name:literal) => {
        // SAFETY: типы указателей в таблицах выше повторяют сигнатуры из заголовков
        // WinHvPlatform.h / WinHvEmulation.h (они же использованы в крейте `windows`).
        unsafe { symbol($module, $lib, concat!($name, "\0").as_bytes())? }
    };
}

fn load_platform() -> Result<Platform> {
    let m = load_library(w!("WinHvPlatform.dll"), PLATFORM_DLL)?;
    let l = PLATFORM_DLL;
    Ok(Platform {
        get_capability: sym!(m, l, "WHvGetCapability"),
        create_partition: sym!(m, l, "WHvCreatePartition"),
        setup_partition: sym!(m, l, "WHvSetupPartition"),
        delete_partition: sym!(m, l, "WHvDeletePartition"),
        set_partition_property: sym!(m, l, "WHvSetPartitionProperty"),
        map_gpa_range: sym!(m, l, "WHvMapGpaRange"),
        unmap_gpa_range: sym!(m, l, "WHvUnmapGpaRange"),
        create_vp: sym!(m, l, "WHvCreateVirtualProcessor"),
        delete_vp: sym!(m, l, "WHvDeleteVirtualProcessor"),
        run_vp: sym!(m, l, "WHvRunVirtualProcessor"),
        cancel_run_vp: sym!(m, l, "WHvCancelRunVirtualProcessor"),
        get_vp_registers: sym!(m, l, "WHvGetVirtualProcessorRegisters"),
        set_vp_registers: sym!(m, l, "WHvSetVirtualProcessorRegisters"),
        request_interrupt: sym!(m, l, "WHvRequestInterrupt"),
        translate_gva: sym!(m, l, "WHvTranslateGva"),
        register_doorbell: sym!(m, l, "WHvRegisterPartitionDoorbellEvent"),
        unregister_doorbell: sym!(m, l, "WHvUnregisterPartitionDoorbellEvent"),
        get_vp_counters: sym!(m, l, "WHvGetVirtualProcessorCounters"),
    })
}

fn load_emulation() -> Result<Emulation> {
    let m = load_library(w!("WinHvEmulation.dll"), EMULATION_DLL)?;
    let l = EMULATION_DLL;
    Ok(Emulation {
        create: sym!(m, l, "WHvEmulatorCreateEmulator"),
        destroy: sym!(m, l, "WHvEmulatorDestroyEmulator"),
        try_io: sym!(m, l, "WHvEmulatorTryIoEmulation"),
        try_mmio: sym!(m, l, "WHvEmulatorTryMmioEmulation"),
    })
}
