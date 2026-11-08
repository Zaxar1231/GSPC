//! Возможности гипервизора (`WHvGetCapability`).

use std::ffi::c_void;

use windows::Win32::System::Hypervisor::{
    WHV_CAPABILITY, WHV_CAPABILITY_CODE, WHvCapabilityCodeHypervisorPresent,
    WHvCapabilityCodeInterruptClockFrequency, WHvCapabilityCodeProcessorClockFrequency,
};

use crate::api;
use crate::error::{Error, Result, check};

fn get(code: WHV_CAPABILITY_CODE, call: &'static str) -> Result<WHV_CAPABILITY> {
    let api = api::platform()?;
    let mut cap = WHV_CAPABILITY::default();
    let mut written = 0_u32;
    #[allow(clippy::cast_possible_truncation, reason = "размер union — сотни байт")]
    let size = size_of::<WHV_CAPABILITY>() as u32;
    // SAFETY: буфер — локальный WHV_CAPABILITY нужного размера, указатели валидны на время вызова.
    let hr = unsafe {
        (api.get_capability)(
            code,
            (&raw mut cap).cast::<c_void>(),
            size,
            &raw mut written,
        )
    };
    check(call, hr)?;
    Ok(cap)
}

/// `true`, если гипервизор Windows запущен и доступен через WHPX.
///
/// # Errors
/// Если библиотека WHPX не загружается.
pub fn hypervisor_present() -> Result<bool> {
    let cap = get(
        WHvCapabilityCodeHypervisorPresent,
        "WHvGetCapability(HypervisorPresent)",
    )?;
    // SAFETY: для этого кода гипервизор заполняет поле HypervisorPresent (BOOL).
    Ok(unsafe { cap.HypervisorPresent }.as_bool())
}

/// Проверяет, что гипервизор доступен.
///
/// # Errors
/// [`Error::LibraryNotFound`], если компонент не включён; [`Error::HypervisorNotPresent`],
/// если гипервизор не запущен.
pub fn ensure_available() -> Result<()> {
    if hypervisor_present()? {
        Ok(())
    } else {
        Err(Error::HypervisorNotPresent)
    }
}

/// Частота TSC гостя, Гц.
///
/// # Errors
/// Если запрос не поддерживается.
pub fn processor_clock_hz() -> Result<u64> {
    let cap = get(
        WHvCapabilityCodeProcessorClockFrequency,
        "WHvGetCapability(ProcessorClockFrequency)",
    )?;
    // SAFETY: для этого кода гипервизор заполняет поле ProcessorClockFrequency (u64).
    Ok(unsafe { cap.ProcessorClockFrequency })
}

/// Частота таймера LAPIC гостя, Гц.
///
/// # Errors
/// Если запрос не поддерживается.
pub fn interrupt_clock_hz() -> Result<u64> {
    let cap = get(
        WHvCapabilityCodeInterruptClockFrequency,
        "WHvGetCapability(InterruptClockFrequency)",
    )?;
    // SAFETY: для этого кода гипервизор заполняет поле InterruptClockFrequency (u64).
    Ok(unsafe { cap.InterruptClockFrequency })
}
