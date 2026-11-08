//! Начальное состояние BSP для 64-битной точки входа: identity-mapping, GDT, регистры.

use crate::error::Result;
use crate::layout::{GDT_ADDR, IDT_ADDR, PD_ADDR, PDPT_ADDR, PML4_ADDR};
use crate::loader::LoadedKernel;
use crate::memory::GuestRam;

/// Селектор кода (`__BOOT_CS`).
pub const BOOT_CS: u16 = 0x10;
/// Селектор данных (`__BOOT_DS`).
pub const BOOT_DS: u16 = 0x18;
/// Селектор TSS.
pub const BOOT_TSS: u16 = 0x20;

/// GDT: null, null, code64, data, TSS (64-битный дескриптор занимает 2 слота).
const GDT: [u64; 6] = [
    0,
    0,
    0x00AF_9B00_0000_FFFF, // CS: 64-бит, DPL0, present, readable
    0x00CF_9300_0000_FFFF, // DS: 32-бит flat, writable
    0x0000_8B00_0000_0067, // TSS (busy), limit 0x67
    0,
];

const CR0_PE: u64 = 1 << 0;
const CR0_ET: u64 = 1 << 4;
const CR0_NE: u64 = 1 << 5;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

/// Сегментный регистр в виде, независимом от WHPX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// База.
    pub base: u64,
    /// Лимит.
    pub limit: u32,
    /// Селектор.
    pub selector: u16,
    /// Атрибуты (формат VMX/WHPX: type, S, DPL, P, AVL, L, D/B, G).
    pub attributes: u16,
}

/// Начальное состояние процессора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootCpuState {
    /// RIP.
    pub rip: u64,
    /// RSI (адрес boot_params).
    pub rsi: u64,
    /// RFLAGS.
    pub rflags: u64,
    /// CR0.
    pub cr0: u64,
    /// CR3.
    pub cr3: u64,
    /// CR4.
    pub cr4: u64,
    /// EFER.
    pub efer: u64,
    /// CS.
    pub cs: Segment,
    /// DS/ES/FS/GS/SS.
    pub ds: Segment,
    /// TR.
    pub tr: Segment,
    /// GDTR (база, лимит).
    pub gdt: (u64, u16),
    /// IDTR (база, лимит).
    pub idt: (u64, u16),
}

/// Пишет таблицы страниц (identity map 0–4 ГиБ страницами 2 МиБ) и GDT.
///
/// # Errors
/// Если структуры не помещаются в RAM.
pub fn setup_boot_tables(ram: &GuestRam) -> Result<()> {
    const PRESENT_RW: u64 = 0x3;
    const PAGE_2M: u64 = 0x80;

    ram.zero(PML4_ADDR, 0x1000)?;
    ram.zero(PDPT_ADDR, 0x1000)?;
    ram.zero(PD_ADDR, 0x4000)?;
    ram.write_u64(PML4_ADDR, PDPT_ADDR | PRESENT_RW)?;
    for gib in 0..4_u64 {
        let pd = PD_ADDR + gib * 0x1000;
        ram.write_u64(PDPT_ADDR + gib * 8, pd | PRESENT_RW)?;
        for entry in 0..512_u64 {
            let phys = (gib << 30) | (entry << 21);
            ram.write_u64(pd + entry * 8, phys | PRESENT_RW | PAGE_2M)?;
        }
    }
    for (i, d) in GDT.iter().enumerate() {
        ram.write_u64(GDT_ADDR + 8 * i as u64, *d)?;
    }
    ram.zero(IDT_ADDR, 16)?;
    Ok(())
}

/// Состояние BSP для прыжка в 64-битную точку входа ядра.
#[must_use]
pub fn boot_cpu_state(kernel: &LoadedKernel) -> BootCpuState {
    // Атрибуты: type | S<<4 | DPL<<5 | P<<7 | L<<13 | DB<<14 | G<<15.
    let code = Segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: BOOT_CS,
        attributes: 0xB | 1 << 4 | 1 << 7 | 1 << 13 | 1 << 15,
    };
    let data = Segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: BOOT_DS,
        attributes: 0x3 | 1 << 4 | 1 << 7 | 1 << 14 | 1 << 15,
    };
    let tr = Segment {
        base: 0,
        limit: 0x67,
        selector: BOOT_TSS,
        attributes: 0xB | 1 << 7,
    };
    #[allow(clippy::cast_possible_truncation, reason = "GDT — 6 записей")]
    let gdt_limit = (GDT.len() * 8 - 1) as u16;
    BootCpuState {
        rip: kernel.entry,
        rsi: kernel.boot_params,
        rflags: 0x2,
        cr0: CR0_PE | CR0_ET | CR0_NE | CR0_PG,
        cr3: PML4_ADDR,
        cr4: CR4_PAE,
        efer: EFER_LME | EFER_LMA,
        cs: code,
        ds: data,
        tr,
        gdt: (GDT_ADDR, gdt_limit),
        idt: (IDT_ADDR, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::AllocOptions;

    fn read_u64(ram: &GuestRam, gpa: u64) -> u64 {
        let mut b = [0; 8];
        ram.read(gpa, &mut b).unwrap();
        u64::from_le_bytes(b)
    }

    #[test]
    fn identity_maps_first_4gib() {
        let ram = GuestRam::new(64 << 20, AllocOptions::default()).unwrap();
        setup_boot_tables(&ram).unwrap();
        assert_eq!(read_u64(&ram, PML4_ADDR), PDPT_ADDR | 3);
        assert_eq!(read_u64(&ram, PDPT_ADDR + 3 * 8), (PD_ADDR + 0x3000) | 3);
        // Последняя 2-МиБ страница 4-го гигабайта: 0xFFE0_0000.
        assert_eq!(
            read_u64(&ram, PD_ADDR + 0x3000 + 511 * 8),
            0xFFE0_0000 | 0x83
        );
        assert_eq!(read_u64(&ram, GDT_ADDR + 0x10), GDT[2]);
    }

    #[test]
    fn long_mode_state() {
        let k = LoadedKernel {
            entry: 0x100_0200,
            boot_params: 0x7000,
            kernel_addr: 0x100_0000,
            initrd: None,
        };
        let s = boot_cpu_state(&k);
        assert_eq!(s.rip, 0x100_0200);
        assert_eq!(s.rsi, 0x7000);
        assert_ne!(s.cr0 & CR0_PG, 0);
        assert_ne!(s.efer & EFER_LMA, 0);
        assert_eq!(s.cs.attributes, 0xA09B);
        assert_eq!(s.ds.attributes, 0xC093);
        assert_eq!(s.gdt.1, 47);
    }
}
