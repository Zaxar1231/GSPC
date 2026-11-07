//! Биндинги и безопасная обёртка над Windows Hypervisor Platform.
//!
//! Зона ответственности крейта:
//! - партиция (`WHvCreatePartition`, свойства, `WHvSetupPartition`);
//! - отображение гостевой памяти (`WHvMapGpaRange`, dirty-bitmap для снапшотов);
//! - виртуальные процессоры (`WHvRunVirtualProcessor`, регистры, отмена запуска);
//! - doorbell-события (`WHvRegisterPartitionDoorbellEvent`) для virtio notify без выхода в user mode;
//! - инъекция прерываний (`WHvRequestInterrupt`) при встроенной эмуляции LAPIC;
//! - эмулятор инструкций для MMIO/PIO (`WinHvEmulation.dll`).
//!
//! Весь `unsafe` проекта, связанный с гипервизором, живёт только здесь. Каждый блок
//! сопровождается комментарием `// SAFETY:`.
//!
//! Реализация — этап 1 (см. `docs/roadmap.md`).
