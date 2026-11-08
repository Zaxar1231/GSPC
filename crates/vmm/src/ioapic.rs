//! IOAPIC (82093AA): 24 входа, доставка в LAPIC гипервизора через [`InterruptSink`].
//!
//! Сейчас поддерживаются прерывания по фронту (ISA-устройства). Уровневые (с Remote IRR и EOI)
//! появятся вместе с PCI INTx, хотя virtio будет работать через MSI-X в обход IOAPIC.

const NUM_PINS: usize = 24;
const VERSION: u32 = 0x11;
const REG_ID: u32 = 0x00;
const REG_VERSION: u32 = 0x01;
const REG_ARB: u32 = 0x02;
const REG_REDIR_BASE: u32 = 0x10;

const MASKED: u64 = 1 << 16;
const DEST_LOGICAL: u64 = 1 << 11;
const TRIGGER_LEVEL: u64 = 1 << 15;
/// Биты, доступные гостю на запись (без Delivery Status и Remote IRR).
const WRITABLE: u64 = 0xFF00_0000_0001_AFFF;

/// Сообщение для LAPIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApicMessage {
    /// Вектор.
    pub vector: u8,
    /// Delivery mode (0 — fixed, 1 — lowest priority, 4 — NMI, …).
    pub delivery_mode: u8,
    /// Логическая адресация.
    pub logical: bool,
    /// Уровневое срабатывание.
    pub level: bool,
    /// Получатель (APIC ID или логическая маска).
    pub destination: u8,
}

/// Куда доставлять прерывания.
pub trait InterruptSink: Send + Sync {
    /// Доставить сообщение в LAPIC.
    fn deliver(&self, msg: ApicMessage);
}

/// Модель IOAPIC.
#[derive(Debug)]
pub struct IoApic {
    id: u8,
    select: u32,
    redirect: [u64; NUM_PINS],
}

impl IoApic {
    /// Новый IOAPIC с указанным APIC ID; все входы замаскированы.
    #[must_use]
    pub fn new(id: u8) -> Self {
        Self {
            id,
            select: 0,
            redirect: [MASKED; NUM_PINS],
        }
    }

    /// Чтение 32-битного регистра MMIO по смещению от базы.
    #[must_use]
    pub fn mmio_read(&self, offset: u64) -> u32 {
        match offset {
            0x00 => self.select,
            0x10 => self.read_register(self.select),
            _ => 0,
        }
    }

    /// Запись 32-битного регистра MMIO.
    pub fn mmio_write(&mut self, offset: u64, value: u32) {
        match offset {
            0x00 => self.select = value & 0xFF,
            0x10 => self.write_register(self.select, value),
            _ => {}
        }
    }

    fn read_register(&self, reg: u32) -> u32 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "берём половины 64-битной записи"
        )]
        match reg {
            REG_ID | REG_ARB => u32::from(self.id) << 24,
            REG_VERSION => ((NUM_PINS as u32 - 1) << 16) | VERSION,
            r if r >= REG_REDIR_BASE => {
                let idx = ((r - REG_REDIR_BASE) / 2) as usize;
                let entry = self.redirect.get(idx).copied().unwrap_or(0);
                if r % 2 == 0 {
                    entry as u32
                } else {
                    (entry >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, reg: u32, value: u32) {
        match reg {
            #[allow(clippy::cast_possible_truncation, reason = "ID — биты 24..27")]
            REG_ID => self.id = ((value >> 24) & 0x0F) as u8,
            r if r >= REG_REDIR_BASE => {
                let idx = ((r - REG_REDIR_BASE) / 2) as usize;
                if let Some(entry) = self.redirect.get_mut(idx) {
                    let v = u64::from(value);
                    let merged = if r % 2 == 0 {
                        (*entry & 0xFFFF_FFFF_0000_0000) | v
                    } else {
                        (*entry & 0x0000_0000_FFFF_FFFF) | (v << 32)
                    };
                    *entry = (*entry & !WRITABLE) | (merged & WRITABLE);
                }
            }
            _ => {}
        }
    }

    /// Фронт на входе `pin`: если вход не замаскирован, возвращает сообщение для LAPIC.
    #[must_use]
    pub fn edge(&self, pin: usize) -> Option<ApicMessage> {
        let entry = *self.redirect.get(pin)?;
        if entry & MASKED != 0 {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "поля записи перенаправления"
        )]
        Some(ApicMessage {
            vector: entry as u8,
            delivery_mode: ((entry >> 8) & 0x7) as u8,
            logical: entry & DEST_LOGICAL != 0,
            level: entry & TRIGGER_LEVEL != 0,
            destination: (entry >> 56) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_redir(io: &mut IoApic, pin: u32, value: u64) {
        io.mmio_write(0x00, REG_REDIR_BASE + 2 * pin);
        io.mmio_write(0x10, value as u32);
        io.mmio_write(0x00, REG_REDIR_BASE + 2 * pin + 1);
        io.mmio_write(0x10, (value >> 32) as u32);
    }

    #[test]
    fn version_and_id() {
        let mut io = IoApic::new(1);
        io.mmio_write(0x00, REG_VERSION);
        assert_eq!(io.mmio_read(0x10), 0x0017_0011);
        io.mmio_write(0x00, REG_ID);
        assert_eq!(io.mmio_read(0x10), 1 << 24);
    }

    #[test]
    fn masked_by_default() {
        let io = IoApic::new(1);
        assert!(io.edge(4).is_none());
    }

    #[test]
    fn unmasked_edge_delivers_vector() {
        let mut io = IoApic::new(1);
        write_redir(&mut io, 4, 0x0300_0000_0000_0024);
        let m = io.edge(4).unwrap();
        assert_eq!(
            m,
            ApicMessage {
                vector: 0x24,
                delivery_mode: 0,
                logical: false,
                level: false,
                destination: 3
            }
        );
        io.mmio_write(0x00, REG_REDIR_BASE + 8);
        assert_eq!(io.mmio_read(0x10), 0x24);
        assert!(io.edge(99).is_none());
    }

    #[test]
    fn read_only_bits_are_preserved() {
        let mut io = IoApic::new(1);
        // Попытка выставить Delivery Status (бит 12) и Remote IRR (бит 14) игнорируется.
        write_redir(&mut io, 0, (1 << 12) | (1 << 14) | 0x30);
        io.mmio_write(0x00, REG_REDIR_BASE);
        assert_eq!(io.mmio_read(0x10), 0x30);
    }
}
