//! Биндинги и безопасная обёртка над Windows Hypervisor Platform.
//!
//! - `caps`: проверка доступности гипервизора, частоты TSC и таймера LAPIC;
//! - `Partition`: партиция, отображение памяти (`WHvMapGpaRange`), инъекция прерываний;
//! - `Vcpu`: запуск VP, регистры, обработка выходов ввода-вывода с быстрым путём для простых
//!   `in`/`out` и эмулятором `WinHvEmulation.dll` для MMIO и строковых инструкций.
//!
//! Функции WHPX загружаются динамически: без включённого компонента Windows Hypervisor Platform
//! приложение запускается и показывает понятную ошибку ([`Error::LibraryNotFound`]).
//!
//! Весь гипервизорный `unsafe` проекта живёт здесь, каждый блок с комментарием `// SAFETY:`.
//! На других ОС крейт содержит только типы ошибок.

mod error;

pub use error::{Error, Result};

#[cfg(windows)]
mod api;
#[cfg(windows)]
pub mod caps;
#[cfg(windows)]
mod partition;
#[cfg(windows)]
mod vcpu;

#[cfg(windows)]
pub use partition::{
    DestinationMode, Interrupt, InterruptType, MapFlags, Partition, PartitionConfig, TriggerMode,
};
#[cfg(windows)]
pub use vcpu::{Bus, Exit, RegisterName, RegisterValue, Vcpu};

/// Сырые типы и константы WHPX (имена регистров, структуры сегментов) для `rustdroid-vmm`.
#[cfg(windows)]
pub mod sys {
    pub use windows::Win32::System::Hypervisor::*;
}
