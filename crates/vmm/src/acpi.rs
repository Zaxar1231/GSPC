//! ACPI-таблицы: RSDP, XSDT, FADT (hardware-reduced), MADT, DSDT.
//!
//! Платформа «hardware-reduced»: без PIC 8259, PIT, PM-таймера и CMOS RTC. Таймер — LAPIC,
//! часы — TSC. Это убирает целый класс выходов из гостя на устаревшие устройства.
//! COM1 описан в DSDT устройством `PNP0501`, чтобы ядро нашло его IRQ через ACPI.

use crate::error::Result;
use crate::layout::{
    ACPI_ADDR, ACPI_SIZE, ECAM_ADDR, IOAPIC_ADDR, LAPIC_ADDR, PCI_MMIO_END, PCI_MMIO_START,
};
use crate::memory::GuestRam;

const OEM_ID: &[u8; 6] = b"RDROID";
const OEM_TABLE_ID: &[u8; 8] = b"RUSTDRD ";
const CREATOR_ID: &[u8; 4] = b"RDRD";
const HEADER_LEN: usize = 36;

/// Параметры платформы для таблиц.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpiConfig {
    /// Число vCPU (APIC ID 0..n).
    pub cpus: u8,
    /// APIC ID IOAPIC.
    pub ioapic_id: u8,
    /// Порт и IRQ COM1.
    pub com1: (u16, u8),
}

impl Default for AcpiConfig {
    fn default() -> Self {
        Self {
            cpus: 1,
            ioapic_id: 1,
            com1: (0x3F8, 4),
        }
    }
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0_u8, |acc, b| acc.wrapping_add(*b))
        .wrapping_neg()
}

/// Таблица с заголовком SDT.
fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let len = HEADER_LEN + body.len();
    let mut t = Vec::with_capacity(len);
    t.extend_from_slice(signature);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "таблицы — единицы килобайт"
    )]
    t.extend_from_slice(&(len as u32).to_le_bytes());
    t.push(revision);
    t.push(0); // checksum
    t.extend_from_slice(OEM_ID);
    t.extend_from_slice(OEM_TABLE_ID);
    t.extend_from_slice(&1_u32.to_le_bytes());
    t.extend_from_slice(CREATOR_ID);
    t.extend_from_slice(&1_u32.to_le_bytes());
    t.extend_from_slice(body);
    let c = checksum(&t);
    if let Some(b) = t.get_mut(9) {
        *b = c;
    }
    t
}

fn madt(cfg: &AcpiConfig) -> Vec<u8> {
    let mut body = Vec::new();
    #[allow(clippy::cast_possible_truncation, reason = "адрес LAPIC под 4 ГиБ")]
    body.extend_from_slice(&(LAPIC_ADDR as u32).to_le_bytes());
    // Flags = 0: нет двойного 8259 (PCAT_COMPAT).
    body.extend_from_slice(&0_u32.to_le_bytes());
    for id in 0..cfg.cpus {
        // Processor Local APIC: type 0, len 8, ACPI UID, APIC ID, flags (enabled).
        body.extend_from_slice(&[0, 8, id, id]);
        body.extend_from_slice(&1_u32.to_le_bytes());
    }
    // I/O APIC: type 1, len 12, id, reserved, address, GSI base.
    body.extend_from_slice(&[1, 12, cfg.ioapic_id, 0]);
    #[allow(clippy::cast_possible_truncation, reason = "адрес IOAPIC под 4 ГиБ")]
    body.extend_from_slice(&(IOAPIC_ADDR as u32).to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    sdt(b"APIC", 5, &body)
}

fn fadt(dsdt_addr: u64) -> Vec<u8> {
    const FADT_LEN: usize = 276;
    const HW_REDUCED_ACPI: u32 = 1 << 20;
    // IAPC_BOOT_ARCH: VGA Not Present (бит 2), CMOS RTC Not Present (бит 5).
    const IAPC_BOOT_ARCH: u16 = (1 << 2) | (1 << 5);

    let mut body = vec![0_u8; FADT_LEN - HEADER_LEN];
    let mut put = |off: usize, bytes: &[u8]| {
        if let Some(dst) = body.get_mut(off - HEADER_LEN..off - HEADER_LEN + bytes.len()) {
            dst.copy_from_slice(bytes);
        }
    };
    #[allow(clippy::cast_possible_truncation, reason = "DSDT под 4 ГиБ")]
    put(40, &(dsdt_addr as u32).to_le_bytes()); // DSDT
    put(109, &IAPC_BOOT_ARCH.to_le_bytes());
    put(112, &HW_REDUCED_ACPI.to_le_bytes()); // Flags
    put(131, &[5]); // FADT Minor Version (ACPI 6.5)
    put(140, &dsdt_addr.to_le_bytes()); // X_DSDT
    put(268, b"RDROIDHV"); // Hypervisor Vendor Identity
    sdt(b"FACP", 6, &body)
}

/// Минимальный кодировщик AML (только то, что нужно для DSDT).
mod aml {
    /// PkgLength: длина, включая сами байты PkgLength.
    pub(super) fn pkg_length(content_len: usize) -> Vec<u8> {
        for n in 1..=4_usize {
            let total = content_len + n;
            let fits = match n {
                1 => total < 0x40,
                2 => total < 0x1000,
                3 => total < 0x10_0000,
                _ => total < 0x1000_0000,
            };
            if fits {
                #[allow(clippy::cast_possible_truncation, reason = "биты разложены по байтам")]
                return if n == 1 {
                    vec![total as u8]
                } else {
                    let mut v = vec![((n - 1) << 6) as u8 | (total & 0x0F) as u8];
                    for i in 0..n - 1 {
                        v.push((total >> (4 + 8 * i)) as u8);
                    }
                    v
                };
            }
        }
        Vec::new()
    }

    fn with_pkg(op: &[u8], content: &[u8]) -> Vec<u8> {
        let mut v = op.to_vec();
        v.extend(pkg_length(content.len()));
        v.extend_from_slice(content);
        v
    }

    /// `Scope(name) { body }`.
    pub(super) fn scope(name: &[u8], body: &[u8]) -> Vec<u8> {
        let mut c = name.to_vec();
        c.extend_from_slice(body);
        with_pkg(&[0x10], &c)
    }

    /// `Device(name) { body }`.
    pub(super) fn device(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = name.to_vec();
        c.extend_from_slice(body);
        with_pkg(&[0x5B, 0x82], &c)
    }

    /// `Name(name, value)`.
    pub(super) fn name(name: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut v = vec![0x08];
        v.extend_from_slice(name);
        v.extend_from_slice(value);
        v
    }

    /// DWordConst.
    pub(super) fn dword(value: u32) -> Vec<u8> {
        let mut v = vec![0x0C];
        v.extend_from_slice(&value.to_le_bytes());
        v
    }

    /// ZeroOp.
    pub(super) const ZERO: [u8; 1] = [0x00];

    /// Сжатый EISA ID, например `PNP0501` → `0x0105_D041`.
    pub(super) fn eisa_id(id: &[u8; 7]) -> u32 {
        let c = |b: u8| u32::from(b.wrapping_sub(0x40) & 0x1F);
        let hex = |b: u8| u32::from(char::from(b).to_digit(16).unwrap_or(0) as u8);
        let vendor = (c(id[0]) << 10) | (c(id[1]) << 5) | c(id[2]);
        let product = (hex(id[3]) << 12) | (hex(id[4]) << 8) | (hex(id[5]) << 4) | hex(id[6]);
        // Байты: vendor big-endian, затем product big-endian.
        u32::from_le_bytes([
            (vendor >> 8) as u8,
            vendor as u8,
            (product >> 8) as u8,
            product as u8,
        ])
    }

    /// `Buffer() { bytes }`.
    pub(super) fn buffer(bytes: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        #[allow(clippy::cast_possible_truncation, reason = "буферы ресурсов короткие")]
        if bytes.len() <= 0xFF {
            c.extend_from_slice(&[0x0A, bytes.len() as u8]);
        } else {
            c.push(0x0B);
            c.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        }
        c.extend_from_slice(bytes);
        with_pkg(&[0x11], &c)
    }
}

fn dsdt(cfg: &AcpiConfig) -> Vec<u8> {
    let (port, irq) = cfg.com1;
    let [lo, hi] = port.to_le_bytes();
    let irq_mask = 1_u16 << (irq & 0x0F);
    let [m0, m1] = irq_mask.to_le_bytes();
    let resources = [
        0x47, 0x01, lo, hi, lo, hi, 0x01, 0x08, // IO(Decode16, port, port, 1, 8)
        0x22, m0, m1, // IRQNoFlags() {irq}
        0x79, 0x00, // EndTag
    ];
    let mut com1 = aml::name(b"_HID", &aml::dword(aml::eisa_id(b"PNP0501")));
    com1.extend(aml::name(b"_UID", &aml::ZERO));
    com1.extend(aml::name(b"_CRS", &aml::buffer(&resources)));
    let mut sb = aml::device(b"COM1", &com1);
    sb.extend(aml::device(b"PCI0", &pci_root()));
    let body = aml::scope(b"\\_SB_", &sb);
    sdt(b"DSDT", 2, &body)
}

/// Корневой мост PCIe: шина 0, окно MMIO для BAR.
fn pci_root() -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation, reason = "окно BAR под 4 ГиБ")]
    let (min, max) = (PCI_MMIO_START as u32, (PCI_MMIO_END - 1) as u32);
    let len = max - min + 1;
    let mut res = vec![
        // WordBusNumber(ResourceProducer, MinFixed, MaxFixed, , 0, 0, 0, 0, 1)
        0x88, 0x0D, 0x00, 0x02, 0x0C, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0,
        // DWordMemory(ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite)
        0x87, 0x17, 0x00, 0x00, 0x0C, 0x01,
    ];
    for v in [0, min, max, 0, len] {
        res.extend_from_slice(&v.to_le_bytes());
    }
    res.extend_from_slice(&[0x79, 0x00]);
    let mut body = aml::name(b"_HID", &aml::dword(aml::eisa_id(b"PNP0A08")));
    body.extend(aml::name(b"_CID", &aml::dword(aml::eisa_id(b"PNP0A03"))));
    body.extend(aml::name(b"_SEG", &aml::ZERO));
    body.extend(aml::name(b"_BBN", &aml::ZERO));
    body.extend(aml::name(b"_UID", &aml::ZERO));
    body.extend(aml::name(b"_CRS", &aml::buffer(&res)));
    body
}

/// MCFG: ECAM шины 0.
fn mcfg() -> Vec<u8> {
    let mut body = vec![0_u8; 8];
    body.extend_from_slice(&ECAM_ADDR.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes()); // сегмент
    body.extend_from_slice(&[0, 0]); // шины 0..0
    body.extend_from_slice(&0_u32.to_le_bytes());
    sdt(b"MCFG", 1, &body)
}

fn rsdp(xsdt_addr: u64) -> Vec<u8> {
    let mut r = Vec::with_capacity(36);
    r.extend_from_slice(b"RSD PTR ");
    r.push(0); // checksum первых 20 байт
    r.extend_from_slice(OEM_ID);
    r.push(2); // revision
    r.extend_from_slice(&0_u32.to_le_bytes()); // RSDT не используется
    r.extend_from_slice(&36_u32.to_le_bytes());
    r.extend_from_slice(&xsdt_addr.to_le_bytes());
    r.push(0); // extended checksum
    r.extend_from_slice(&[0; 3]);
    let c = checksum(r.get(..20).unwrap_or(&[]));
    if let Some(b) = r.get_mut(8) {
        *b = c;
    }
    let ec = checksum(&r);
    if let Some(b) = r.get_mut(32) {
        *b = ec;
    }
    r
}

/// Собирает таблицы и записывает их в гостевую память. Возвращает адрес RSDP.
///
/// # Errors
/// Если таблицы не помещаются в область ACPI.
pub fn write_tables(ram: &GuestRam, cfg: &AcpiConfig) -> Result<u64> {
    let align = |v: u64| v.div_ceil(16) * 16;
    let rsdp_addr = ACPI_ADDR;
    let xsdt_addr = rsdp_addr + 64;
    // XSDT: заголовок + 3 указателя.
    let fadt_addr = align(xsdt_addr + HEADER_LEN as u64 + 24);
    let dsdt_addr = align(fadt_addr + 276);
    let dsdt_table = dsdt(cfg);
    let madt_addr = align(dsdt_addr + dsdt_table.len() as u64);
    let madt_table = madt(cfg);
    let mcfg_addr = align(madt_addr + madt_table.len() as u64);
    let mcfg_table = mcfg();

    let mut xsdt_body = Vec::new();
    xsdt_body.extend_from_slice(&fadt_addr.to_le_bytes());
    xsdt_body.extend_from_slice(&madt_addr.to_le_bytes());
    xsdt_body.extend_from_slice(&mcfg_addr.to_le_bytes());

    if mcfg_addr + mcfg_table.len() as u64 > ACPI_ADDR + ACPI_SIZE {
        return Err(crate::error::Error::OutOfGuestMemory(
            "ACPI-таблицы не помещаются",
        ));
    }
    ram.zero(ACPI_ADDR, usize::try_from(ACPI_SIZE).unwrap_or(0))?;
    ram.write(rsdp_addr, &rsdp(xsdt_addr))?;
    ram.write(xsdt_addr, &sdt(b"XSDT", 1, &xsdt_body))?;
    ram.write(fadt_addr, &fadt(dsdt_addr))?;
    ram.write(dsdt_addr, &dsdt_table)?;
    ram.write(madt_addr, &madt_table)?;
    ram.write(mcfg_addr, &mcfg_table)?;
    Ok(rsdp_addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::AllocOptions;

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0_u8, |a, x| a.wrapping_add(*x))
    }

    #[test]
    fn eisa_id_matches_known_value() {
        assert_eq!(aml::eisa_id(b"PNP0501"), 0x0105_D041);
        assert_eq!(aml::eisa_id(b"PNP0A08"), 0x080A_D041);
    }

    #[test]
    fn pkg_length_encoding() {
        assert_eq!(aml::pkg_length(10), vec![11]);
        assert_eq!(aml::pkg_length(62), vec![63]);
        // 63 + 2 = 65 = 0x41 → [0x40 | 0x1, 0x04]
        assert_eq!(aml::pkg_length(63), vec![0x41, 0x04]);
    }

    #[test]
    fn com1_resource_buffer() {
        let b = aml::buffer(&[0x79, 0x00]);
        assert_eq!(b, vec![0x11, 0x05, 0x0A, 0x02, 0x79, 0x00]);
    }

    #[test]
    fn tables_have_valid_checksums() {
        let cfg = AcpiConfig::default();
        for t in [
            madt(&cfg),
            fadt(0x1234),
            dsdt(&cfg),
            mcfg(),
            sdt(b"XSDT", 1, &[0; 16]),
        ] {
            assert_eq!(sum(&t), 0, "{:?}", std::str::from_utf8(&t[..4]));
            let len = u32::from_le_bytes(t[4..8].try_into().unwrap()) as usize;
            assert_eq!(len, t.len());
        }
        assert_eq!(fadt(0).len(), 276);
        let r = rsdp(0xE0040);
        assert_eq!(sum(&r[..20]), 0);
        assert_eq!(sum(&r), 0);
    }

    #[test]
    fn pci_root_resources() {
        let root = pci_root();
        // _CRS: BufferOp, PkgLength = 1 + BufferSize (2) + 44 байта ресурсов
        // (WordBusNumber 16 + DWordMemory 26 + EndTag 2).
        let crs = root.windows(4).position(|w| w == b"_CRS").unwrap();
        assert_eq!(&root[crs + 4..crs + 8], &[0x11, 47, 0x0A, 44]);
        let dsdt = dsdt(&AcpiConfig::default());
        assert!(dsdt.windows(4).any(|w| w == b"PCI0"));
        let m = mcfg();
        assert_eq!(m.len(), 60);
        assert_eq!(u64::from_le_bytes(m[44..52].try_into().unwrap()), ECAM_ADDR);
    }

    #[test]
    fn madt_lists_cpus_and_ioapic() {
        let t = madt(&AcpiConfig {
            cpus: 2,
            ..AcpiConfig::default()
        });
        // 36 заголовок + 8 + 2*8 LAPIC + 12 IOAPIC
        assert_eq!(t.len(), 36 + 8 + 16 + 12);
        assert_eq!(&t[44..46], &[0, 8]);
        assert_eq!(&t[60..62], &[1, 12]);
    }

    #[test]
    fn writes_tables_to_ram() {
        let ram = GuestRam::new(32 << 20, AllocOptions::default()).unwrap();
        let rsdp_addr = write_tables(&ram, &AcpiConfig::default()).unwrap();
        let mut sig = [0_u8; 8];
        ram.read(rsdp_addr, &mut sig).unwrap();
        assert_eq!(&sig, b"RSD PTR ");
        let mut xsdt_ptr = [0_u8; 8];
        ram.read(rsdp_addr + 24, &mut xsdt_ptr).unwrap();
        let mut xsig = [0_u8; 4];
        ram.read(u64::from_le_bytes(xsdt_ptr), &mut xsig).unwrap();
        assert_eq!(&xsig, b"XSDT");
    }
}
