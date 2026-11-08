//! Раскладка гостевого физического адресного пространства (x86_64).
//!
//! ```text
//! 0x0000_0500  GDT
//! 0x0000_0520  IDT (пустая)
//! 0x0000_7000  boot_params (zero page)
//! 0x0000_9000  PML4, 0xA000 PDPT, 0xB000..0xF000 PD (identity map первых 4 ГиБ, страницы 2 МиБ)
//! 0x0002_0000  командная строка ядра
//! 0x0009_FC00  EBDA (reserved)
//! 0x000E_0000  ACPI: RSDP, XSDT, FADT, MADT, DSDT (reserved)
//! 0x0010_0000  RAM
//! 0x0100_0000  ядро (protected-mode часть bzImage)
//!   ...        initrd (под верхней границей нижней RAM)
//! 0xC000_0000  MMIO-дыра (до 4 ГиБ):
//!   0xC000_0000..0xE000_0000  окно BAR устройств PCI
//!   0xE000_0000               ECAM (конфигурационное пространство PCI, шина 0)
//!   0xFEC0_0000 IOAPIC, 0xFEE0_0000 LAPIC
//! 0x1_0000_0000 остаток RAM
//! ```

/// GDT.
pub const GDT_ADDR: u64 = 0x500;
/// IDT (пустая, ядро ставит свою).
pub const IDT_ADDR: u64 = 0x520;
/// boot_params.
pub const ZERO_PAGE_ADDR: u64 = 0x7000;
/// PML4.
pub const PML4_ADDR: u64 = 0x9000;
/// PDPT.
pub const PDPT_ADDR: u64 = 0xA000;
/// Первая из четырёх таблиц PD.
pub const PD_ADDR: u64 = 0xB000;
/// Командная строка ядра.
pub const CMDLINE_ADDR: u64 = 0x2_0000;
/// Максимальная длина командной строки (вместе с завершающим нулём).
pub const CMDLINE_MAX: usize = 0x1_0000;
/// Начало EBDA.
pub const EBDA_START: u64 = 0x9_FC00;
/// Область ACPI-таблиц (RSDP ищется ядром в 0xE0000..0xFFFFF).
pub const ACPI_ADDR: u64 = 0xE_0000;
/// Размер области ACPI.
pub const ACPI_SIZE: u64 = 0x2_0000;
/// Начало «высокой» памяти (1 МиБ).
pub const HIGH_MEMORY_START: u64 = 0x10_0000;
/// Адрес загрузки ядра по умолчанию (16 МиБ, кратно 2 МиБ).
pub const KERNEL_LOAD_ADDR: u64 = 0x100_0000;
/// Начало MMIO-дыры под 4 ГиБ.
pub const MMIO_HOLE_START: u64 = 0xC000_0000;
/// Конец MMIO-дыры.
pub const MMIO_HOLE_END: u64 = 0x1_0000_0000;
/// Начало окна BAR устройств PCI.
pub const PCI_MMIO_START: u64 = 0xC000_0000;
/// Конец окна BAR (не включительно).
pub const PCI_MMIO_END: u64 = 0xE000_0000;
/// База ECAM.
pub const ECAM_ADDR: u64 = 0xE000_0000;
/// Размер ECAM для шины 0 (32 устройства × 8 функций × 4 КиБ).
pub const ECAM_SIZE: u64 = 0x10_0000;
/// Базовый адрес IOAPIC.
pub const IOAPIC_ADDR: u64 = 0xFEC0_0000;
/// Размер окна IOAPIC.
pub const IOAPIC_SIZE: u64 = 0x1000;
/// Базовый адрес LAPIC (эмулируется гипервизором).
pub const LAPIC_ADDR: u64 = 0xFEE0_0000;

/// Непрерывный участок RAM в гостевых физических адресах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamRange {
    /// Начальный GPA.
    pub gpa: u64,
    /// Размер в байтах.
    pub size: u64,
}

impl RamRange {
    /// Конец участка (не включительно).
    #[must_use]
    pub fn end(&self) -> u64 {
        self.gpa + self.size
    }
}

/// Участки RAM для заданного объёма: всё, что не помещается под MMIO-дырой, уходит выше 4 ГиБ.
#[must_use]
pub fn ram_ranges(total: u64) -> Vec<RamRange> {
    let low = total.min(MMIO_HOLE_START);
    let mut ranges = vec![RamRange { gpa: 0, size: low }];
    if total > low {
        ranges.push(RamRange {
            gpa: MMIO_HOLE_END,
            size: total - low,
        });
    }
    ranges
}

/// Тип записи e820.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E820Kind {
    /// Обычная RAM.
    Ram = 1,
    /// Зарезервировано.
    Reserved = 2,
}

/// Запись карты памяти e820.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct E820Entry {
    /// Начальный адрес.
    pub addr: u64,
    /// Размер.
    pub size: u64,
    /// Тип.
    pub kind: E820Kind,
}

/// Карта e820 для заданного объёма RAM.
#[must_use]
pub fn e820_map(total: u64) -> Vec<E820Entry> {
    let ranges = ram_ranges(total);
    let mut map = vec![
        E820Entry {
            addr: 0,
            size: EBDA_START,
            kind: E820Kind::Ram,
        },
        E820Entry {
            addr: EBDA_START,
            size: HIGH_MEMORY_START - EBDA_START,
            kind: E820Kind::Reserved,
        },
    ];
    if let Some(low) = ranges.first()
        && low.end() > HIGH_MEMORY_START
    {
        map.push(E820Entry {
            addr: HIGH_MEMORY_START,
            size: low.end() - HIGH_MEMORY_START,
            kind: E820Kind::Ram,
        });
    }
    // ECAM должен быть зарезервирован в e820, иначе ядро не станет использовать MCFG.
    map.push(E820Entry {
        addr: ECAM_ADDR,
        size: ECAM_SIZE,
        kind: E820Kind::Reserved,
    });
    if let Some(high) = ranges.get(1) {
        map.push(E820Entry {
            addr: high.gpa,
            size: high.size,
            kind: E820Kind::Ram,
        });
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn small_ram_has_single_range() {
        assert_eq!(
            ram_ranges(2 * GIB),
            vec![RamRange {
                gpa: 0,
                size: 2 * GIB
            }]
        );
    }

    #[test]
    fn large_ram_is_split_around_hole() {
        let r = ram_ranges(8 * GIB);
        assert_eq!(
            r[0],
            RamRange {
                gpa: 0,
                size: MMIO_HOLE_START
            }
        );
        assert_eq!(
            r[1],
            RamRange {
                gpa: MMIO_HOLE_END,
                size: 8 * GIB - MMIO_HOLE_START
            }
        );
        assert_eq!(r.iter().map(|x| x.size).sum::<u64>(), 8 * GIB);
    }

    #[test]
    fn e820_reserves_bios_area() {
        let m = e820_map(8 * GIB);
        assert_eq!(
            m[0],
            E820Entry {
                addr: 0,
                size: EBDA_START,
                kind: E820Kind::Ram
            }
        );
        assert_eq!(m[1].kind, E820Kind::Reserved);
        assert_eq!(m[1].addr + m[1].size, HIGH_MEMORY_START);
        assert_eq!(m[2].addr, HIGH_MEMORY_START);
        assert_eq!(
            m[3],
            E820Entry {
                addr: ECAM_ADDR,
                size: ECAM_SIZE,
                kind: E820Kind::Reserved
            }
        );
        assert_eq!(m[4].addr, MMIO_HOLE_END);
        // ACPI-таблицы лежат в зарезервированной области.
        assert!(ACPI_ADDR >= m[1].addr && ACPI_ADDR + ACPI_SIZE <= m[1].addr + m[1].size);
    }

    #[test]
    fn fixed_structures_do_not_overlap() {
        let mut regions = [
            (GDT_ADDR, 0x20),
            (IDT_ADDR, 0x10),
            (ZERO_PAGE_ADDR, 0x1000),
            (PML4_ADDR, 0x1000),
            (PDPT_ADDR, 0x1000),
            (PD_ADDR, 0x4000),
            (CMDLINE_ADDR, CMDLINE_MAX as u64),
        ];
        regions.sort_unstable();
        for w in regions.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "{w:x?}");
        }
        assert!(CMDLINE_ADDR + CMDLINE_MAX as u64 <= EBDA_START);
    }
}
