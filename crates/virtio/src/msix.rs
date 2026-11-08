//! Таблица MSI-X и формат MSI-сообщения x86.

/// MSI-сообщение (адрес + данные), как его записал драйвер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiMessage {
    /// Адрес (0xFEEx_xxxx).
    pub address: u64,
    /// Данные (вектор, delivery mode, trigger).
    pub data: u32,
}

impl MsiMessage {
    /// Вектор.
    #[must_use]
    pub fn vector(&self) -> u8 {
        (self.data & 0xFF) as u8
    }

    /// Delivery mode (биты 8..10).
    #[must_use]
    pub fn delivery_mode(&self) -> u8 {
        ((self.data >> 8) & 0x7) as u8
    }

    /// Уровневое срабатывание (бит 15).
    #[must_use]
    pub fn level(&self) -> bool {
        self.data & (1 << 15) != 0
    }

    /// Получатель: APIC ID (биты адреса 12..19).
    #[must_use]
    pub fn destination(&self) -> u8 {
        ((self.address >> 12) & 0xFF) as u8
    }

    /// Логическая адресация (бит адреса 2).
    #[must_use]
    pub fn logical(&self) -> bool {
        self.address & (1 << 2) != 0
    }
}

/// Куда отправлять MSI.
pub trait MsiSink: Send + Sync {
    /// Доставить сообщение.
    fn signal(&self, msg: MsiMessage);
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    addr_lo: u32,
    addr_hi: u32,
    data: u32,
    control: u32,
}

const ENTRY_SIZE: u64 = 16;
const ENTRY_MASKED: u32 = 1;

/// Таблица MSI-X устройства и Pending Bit Array.
#[derive(Debug, Clone)]
pub struct MsixTable {
    entries: Vec<Entry>,
    pending: Vec<bool>,
    /// MSI-X включён (бит 15 Message Control).
    pub enabled: bool,
    /// Function Mask (бит 14 Message Control).
    pub function_masked: bool,
}

impl MsixTable {
    /// Таблица на `vectors` векторов; все записи замаскированы.
    #[must_use]
    pub fn new(vectors: u16) -> Self {
        let n = usize::from(vectors);
        Self {
            entries: vec![
                Entry {
                    control: ENTRY_MASKED,
                    ..Entry::default()
                };
                n
            ],
            pending: vec![false; n],
            enabled: false,
            function_masked: false,
        }
    }

    /// Число векторов.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Пустая таблица.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Размер таблицы в байтах.
    #[must_use]
    pub fn table_size(&self) -> u64 {
        self.entries.len() as u64 * ENTRY_SIZE
    }

    /// Чтение из таблицы (смещение от начала таблицы).
    pub fn read_table(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let Ok(idx) = usize::try_from(offset / ENTRY_SIZE) else {
            return;
        };
        let Some(e) = self.entries.get(idx) else {
            return;
        };
        let mut raw = [0_u8; 16];
        raw[0..4].copy_from_slice(&e.addr_lo.to_le_bytes());
        raw[4..8].copy_from_slice(&e.addr_hi.to_le_bytes());
        raw[8..12].copy_from_slice(&e.data.to_le_bytes());
        raw[12..16].copy_from_slice(&e.control.to_le_bytes());
        let start = (offset % ENTRY_SIZE) as usize;
        for (dst, src) in data.iter_mut().zip(raw.iter().skip(start)) {
            *dst = *src;
        }
    }

    /// Запись в таблицу. Возвращает вектор, если с него сняли маску и для него было
    /// отложенное прерывание (его нужно доставить).
    pub fn write_table(&mut self, offset: u64, data: &[u8]) -> Option<u16> {
        let idx = usize::try_from(offset / ENTRY_SIZE).ok()?;
        let e = self.entries.get_mut(idx)?;
        let mut raw = [0_u8; 16];
        raw[0..4].copy_from_slice(&e.addr_lo.to_le_bytes());
        raw[4..8].copy_from_slice(&e.addr_hi.to_le_bytes());
        raw[8..12].copy_from_slice(&e.data.to_le_bytes());
        raw[12..16].copy_from_slice(&e.control.to_le_bytes());
        let start = (offset % ENTRY_SIZE) as usize;
        for (dst, src) in raw.iter_mut().skip(start).zip(data) {
            *dst = *src;
        }
        let word = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
        let was_masked = e.control & ENTRY_MASKED != 0;
        *e = Entry {
            addr_lo: word(0),
            addr_hi: word(4),
            data: word(8),
            control: word(12),
        };
        let unmasked = was_masked && e.control & ENTRY_MASKED == 0;
        let pending = self.pending.get_mut(idx)?;
        if unmasked && *pending {
            *pending = false;
            return u16::try_from(idx).ok();
        }
        None
    }

    /// Чтение Pending Bit Array.
    pub fn read_pba(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        for (i, byte) in data.iter_mut().enumerate() {
            let base = (offset as usize + i) * 8;
            for bit in 0..8 {
                if self.pending.get(base + bit).copied().unwrap_or(false) {
                    *byte |= 1 << bit;
                }
            }
        }
    }

    /// Сообщение для вектора, если доставка разрешена; иначе прерывание откладывается (PBA).
    pub fn message(&mut self, vector: u16) -> Option<MsiMessage> {
        if !self.enabled {
            return None;
        }
        let idx = usize::from(vector);
        let e = *self.entries.get(idx)?;
        if self.function_masked || e.control & ENTRY_MASKED != 0 {
            if let Some(p) = self.pending.get_mut(idx) {
                *p = true;
            }
            return None;
        }
        Some(MsiMessage {
            address: u64::from(e.addr_lo) | (u64::from(e.addr_hi) << 32),
            data: e.data,
        })
    }

    /// Сообщение для отложенного вектора после снятия Function Mask.
    pub fn take_pending(&mut self) -> Vec<u16> {
        if !self.enabled || self.function_masked {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (i, p) in self.pending.iter_mut().enumerate() {
            let masked = self
                .entries
                .get(i)
                .is_none_or(|e| e.control & ENTRY_MASKED != 0);
            if *p && !masked {
                *p = false;
                out.extend(u16::try_from(i).ok());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(t: &mut MsixTable, v: u64, addr: u32, data: u32, masked: bool) -> Option<u16> {
        t.write_table(v * 16, &addr.to_le_bytes());
        t.write_table(v * 16 + 4, &0_u32.to_le_bytes());
        t.write_table(v * 16 + 8, &data.to_le_bytes());
        t.write_table(v * 16 + 12, &u32::from(masked).to_le_bytes())
    }

    #[test]
    fn decodes_x86_message() {
        let m = MsiMessage {
            address: 0xFEE0_3004,
            data: 0x0000_8131,
        };
        assert_eq!(m.vector(), 0x31);
        assert_eq!(m.delivery_mode(), 1);
        assert!(m.level());
        assert_eq!(m.destination(), 3);
        assert!(m.logical());
    }

    #[test]
    fn disabled_table_does_not_deliver() {
        let mut t = MsixTable::new(2);
        program(&mut t, 0, 0xFEE0_0000, 0x41, false);
        assert!(t.message(0).is_none());
        t.enabled = true;
        assert_eq!(t.message(0).unwrap().vector(), 0x41);
    }

    #[test]
    fn masked_vector_becomes_pending_and_fires_on_unmask() {
        let mut t = MsixTable::new(2);
        t.enabled = true;
        assert_eq!(program(&mut t, 1, 0xFEE0_0000, 0x42, true), None);
        assert!(t.message(1).is_none());
        let mut pba = [0_u8; 1];
        t.read_pba(0, &mut pba);
        assert_eq!(pba[0], 0b10);
        // Снятие маски → вектор нужно доставить.
        assert_eq!(t.write_table(16 + 12, &0_u32.to_le_bytes()), Some(1));
        t.read_pba(0, &mut pba);
        assert_eq!(pba[0], 0);
    }

    #[test]
    fn table_reads_back() {
        let mut t = MsixTable::new(1);
        program(&mut t, 0, 0xFEE0_1000, 0x55, false);
        let mut b = [0_u8; 4];
        t.read_table(8, &mut b);
        assert_eq!(u32::from_le_bytes(b), 0x55);
        t.read_table(64, &mut b);
        assert_eq!(b, [0; 4]);
    }

    #[test]
    fn function_mask_defers() {
        let mut t = MsixTable::new(1);
        t.enabled = true;
        t.function_masked = true;
        program(&mut t, 0, 0xFEE0_0000, 0x30, false);
        assert!(t.message(0).is_none());
        t.function_masked = false;
        assert_eq!(t.take_pending(), vec![0]);
    }
}
