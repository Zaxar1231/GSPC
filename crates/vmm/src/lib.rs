//! Ядро VMM RustDroid.
//!
//! Зона ответственности:
//! - поток на каждый vCPU с циклом обработки exit-причин (MMIO, PIO, MSR, CPUID, HLT);
//! - гостевая RAM: заранее выделенная, с prefault и опциональными страницами 2 МБ;
//! - прямая загрузка bzImage + initramfs + cmdline (64-битная точка входа, без BIOS);
//! - IOAPIC, маршрутизация MSI/MSI-X, ACPI-таблицы (RSDP, XSDT, MADT, FADT, DSDT, MCFG);
//! - PCI-шина (ECAM) для virtio-pci устройств;
//! - снапшоты (RAM + состояние vCPU и устройств).
//!
//! Реализация — этапы 1–2 и 10 (см. `docs/roadmap.md`).
