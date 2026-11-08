//! Ядро VMM RustDroid.
//!
//! Портируемая часть (тестируется на любой ОС):
//! - [`layout`] — раскладка гостевых физических адресов и карта e820;
//! - [`memory`] — гостевая RAM (предвыделение, prefault, большие страницы);
//! - [`loader`] — прямая загрузка bzImage + initrd + cmdline по 64-битному boot protocol;
//! - [`boot_cpu`] — identity-mapping, GDT и регистры BSP для 64-битной точки входа;
//! - [`acpi`] — RSDP/XSDT/FADT (hardware-reduced)/MADT/DSDT;
//! - [`ioapic`] — модель IOAPIC;
//! - [`pci`] — шина PCI 0: ECAM и маршрутизация BAR;
//! - [`platform`] — шина: RAM, COM1, IOAPIC, PCI, заглушки legacy-портов;
//! - [`cmdline`] — командная строка ядра по умолчанию.
//!
//! Только Windows: `vm` (ВМ на WHPX, SMP, устройства), `worker` (doorbell-потоки virtio) и
//! `host` (P-ядра, MMCSS).

pub mod acpi;
pub mod boot_cpu;
pub mod cmdline;
mod error;
pub mod ioapic;
pub mod layout;
pub mod loader;
pub mod memory;
pub mod pci;
pub mod platform;

#[cfg(windows)]
pub mod host;
#[cfg(windows)]
pub mod vm;
#[cfg(windows)]
pub mod worker;

pub use error::{Error, Result};
#[cfg(windows)]
pub use vm::{ConsoleHandle, ExitStats, InputHandle, RunOutcome, Stopper, Vm, VmConfig, VmDevices};
