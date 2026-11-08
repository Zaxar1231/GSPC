//! Транспорт virtio-pci (modern, virtio 1.x, раздел 4.1).
//!
//! Раскладка BAR0 (32-битный MMIO, 32 КиБ):
//!
//! | Смещение | Область |
//! |---|---|
//! | 0x0000 | common config |
//! | 0x1000 | ISR |
//! | 0x2000 | device config |
//! | 0x3000 | notify (4 байта на очередь) |
//! | 0x4000 | таблица MSI-X |
//! | 0x5000 | MSI-X PBA |
//!
//! Notify-адреса очередей регистрируются как doorbell-события WHPX: запись гостя будит worker
//! устройства без выхода vCPU в user mode. Без doorbell (тесты) уведомление обрабатывается
//! синхронно через [`VirtioPci::bar_write`].

use std::sync::Arc;

use crate::device::{
    VIRTIO_F_VERSION_1, VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC, VirtioDevice,
};
use crate::mem::GuestMemory;
use crate::msix::{MsiSink, MsixTable};
use crate::queue::{NO_VECTOR, Queue};

/// Размер BAR0.
pub const BAR_SIZE: u64 = 0x8000;
/// Смещение common config.
pub const COMMON_OFFSET: u64 = 0x0000;
/// Смещение ISR.
pub const ISR_OFFSET: u64 = 0x1000;
/// Смещение device config.
pub const DEVICE_OFFSET: u64 = 0x2000;
/// Смещение области notify.
pub const NOTIFY_OFFSET: u64 = 0x3000;
/// Шаг notify-адресов очередей.
pub const NOTIFY_MULTIPLIER: u32 = 4;
/// Смещение таблицы MSI-X.
pub const MSIX_TABLE_OFFSET: u64 = 0x4000;
/// Смещение MSI-X PBA.
pub const MSIX_PBA_OFFSET: u64 = 0x5000;

const COMMON_SIZE: u64 = 0x38;
const VENDOR_VIRTIO: u16 = 0x1AF4;
const CAP_MSIX: u64 = 0x40;
const CAP_COMMON: u64 = 0x4C;
const CAP_ISR: u64 = 0x5C;
const CAP_DEVICE: u64 = 0x6C;
const CAP_NOTIFY: u64 = 0x7C;
const CONFIG_SIZE: usize = 256;

const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 0x80;

/// virtio-устройство на транспорте PCI.
pub struct VirtioPci {
    device: Box<dyn VirtioDevice>,
    queues: Vec<Queue>,
    msix: MsixTable,
    mem: Arc<dyn GuestMemory>,
    msi: Arc<dyn MsiSink>,
    config: [u8; CONFIG_SIZE],
    bar0: u32,
    bar_sizing: bool,
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    msix_config: u16,
    status: u8,
    config_generation: u8,
    queue_select: u16,
    isr: u8,
    active: bool,
}

impl std::fmt::Debug for VirtioPci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioPci")
            .field("type", &self.device.device_type())
            .field("bar0", &format_args!("0x{:x}", self.bar0))
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

fn put(buf: &mut [u8], off: u64, bytes: &[u8]) {
    let off = off as usize;
    if let Some(dst) = buf.get_mut(off..off + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

fn read_window(src: &[u8], off: u64, data: &mut [u8]) {
    data.fill(0);
    let off = off as usize;
    for (i, b) in data.iter_mut().enumerate() {
        *b = src.get(off + i).copied().unwrap_or(0);
    }
}

fn to_u64(data: &[u8]) -> u64 {
    let mut b = [0_u8; 8];
    for (dst, src) in b.iter_mut().zip(data) {
        *dst = *src;
    }
    u64::from_le_bytes(b)
}

impl VirtioPci {
    /// Создаёт функцию PCI для устройства. `bar0` — заранее назначенный адрес BAR0.
    #[must_use]
    pub fn new(
        device: Box<dyn VirtioDevice>,
        bar0: u32,
        mem: Arc<dyn GuestMemory>,
        msi: Arc<dyn MsiSink>,
    ) -> Self {
        let queues: Vec<Queue> = device
            .queue_max_sizes()
            .iter()
            .map(|&s| Queue::new(s))
            .collect();
        #[allow(clippy::cast_possible_truncation, reason = "очередей — единицы")]
        let vectors = (queues.len() + 1) as u16;
        let mut pci = Self {
            device,
            queues,
            msix: MsixTable::new(vectors),
            mem,
            msi,
            config: [0; CONFIG_SIZE],
            bar0,
            bar_sizing: false,
            device_feature_select: 0,
            driver_feature_select: 0,
            driver_features: 0,
            msix_config: NO_VECTOR,
            status: 0,
            config_generation: 0,
            queue_select: 0,
            isr: 0,
            active: false,
        };
        pci.build_config_space(vectors);
        pci
    }

    fn build_config_space(&mut self, vectors: u16) {
        let dev_type = self.device.device_type();
        let class: u32 = match dev_type {
            crate::device::TYPE_BLOCK => 0x01_80_00, // mass storage, other
            crate::device::TYPE_CONSOLE => 0x07_80_00, // communication, other
            crate::device::TYPE_GPU => 0x03_80_00,   // display, other
            crate::device::TYPE_INPUT => 0x09_80_00, // input, other
            _ => 0xFF_00_00,
        };
        let c = &mut self.config;
        #[allow(clippy::cast_possible_truncation, reason = "тип virtio < 0x100")]
        let device_id = 0x1040 + dev_type as u16;
        put(c, 0x00, &VENDOR_VIRTIO.to_le_bytes());
        put(c, 0x02, &device_id.to_le_bytes());
        put(c, 0x06, &0x0010_u16.to_le_bytes()); // status: capabilities list
        put(c, 0x08, &(class << 8 | 0x01).to_le_bytes()); // revision 1 + class
        put(c, 0x0E, &[0x00]); // header type 0
        put(c, 0x2C, &VENDOR_VIRTIO.to_le_bytes()); // subsystem vendor
        #[allow(clippy::cast_possible_truncation, reason = "тип virtio < 0x100")]
        put(c, 0x2E, &(0x40 + dev_type as u16).to_le_bytes()); // subsystem id
        put(c, 0x34, &[CAP_MSIX as u8]);

        // MSI-X: id, next, message control (table size - 1), table и PBA в BAR0.
        put(c, CAP_MSIX, &[0x11, CAP_COMMON as u8]);
        put(c, CAP_MSIX + 2, &(vectors - 1).to_le_bytes());
        #[allow(clippy::cast_possible_truncation, reason = "смещения в BAR < 4 ГиБ")]
        {
            put(c, CAP_MSIX + 4, &(MSIX_TABLE_OFFSET as u32).to_le_bytes());
            put(c, CAP_MSIX + 8, &(MSIX_PBA_OFFSET as u32).to_le_bytes());
        }

        let mut virtio_cap =
            |at: u64, next: u64, cfg_type: u8, offset: u64, len: u64, cap_len: u8| {
                put(c, at, &[0x09, next as u8, cap_len, cfg_type, 0, 0, 0, 0]);
                #[allow(clippy::cast_possible_truncation, reason = "смещения в BAR < 4 ГиБ")]
                {
                    put(c, at + 8, &(offset as u32).to_le_bytes());
                    put(c, at + 12, &(len as u32).to_le_bytes());
                }
            };
        virtio_cap(CAP_COMMON, CAP_ISR, 1, COMMON_OFFSET, COMMON_SIZE, 16);
        virtio_cap(CAP_ISR, CAP_DEVICE, 3, ISR_OFFSET, 4, 16);
        virtio_cap(CAP_DEVICE, CAP_NOTIFY, 4, DEVICE_OFFSET, 0x1000, 16);
        let notify_len = self.queues.len() as u64 * u64::from(NOTIFY_MULTIPLIER);
        virtio_cap(CAP_NOTIFY, 0, 2, NOTIFY_OFFSET, notify_len.max(4), 20);
        put(c, CAP_NOTIFY + 16, &NOTIFY_MULTIPLIER.to_le_bytes());
    }

    /// Адрес BAR0.
    #[must_use]
    pub fn bar0(&self) -> u64 {
        u64::from(self.bar0)
    }

    /// Число очередей.
    #[must_use]
    pub fn num_queues(&self) -> usize {
        self.queues.len()
    }

    /// GPA notify-адреса очереди (для регистрации doorbell).
    #[must_use]
    pub fn notify_address(&self, queue: usize) -> u64 {
        self.bar0() + NOTIFY_OFFSET + queue as u64 * u64::from(NOTIFY_MULTIPLIER)
    }

    /// Устройство активировано драйвером (DRIVER_OK).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Доступ к устройству (например, чтобы передать ввод консоли).
    pub fn device_mut(&mut self) -> &mut dyn VirtioDevice {
        self.device.as_mut()
    }

    // ---------- PCI config space ----------

    /// Чтение PCI config space.
    pub fn config_read(&self, offset: u64, data: &mut [u8]) {
        if offset == 0x10 && self.bar_sizing {
            // Размер BAR: биты адреса, которые нельзя изменить.
            #[allow(clippy::cast_possible_truncation, reason = "BAR 32-битный")]
            let mask = !(BAR_SIZE as u32 - 1);
            read_window(&mask.to_le_bytes(), 0, data);
            return;
        }
        let mut c = self.config;
        put(&mut c, 0x10, &self.bar0.to_le_bytes());
        let mut ctrl = u16::from_le_bytes([c[CAP_MSIX as usize + 2], c[CAP_MSIX as usize + 3]]);
        if self.msix.enabled {
            ctrl |= 1 << 15;
        }
        if self.msix.function_masked {
            ctrl |= 1 << 14;
        }
        put(&mut c, CAP_MSIX + 2, &ctrl.to_le_bytes());
        read_window(&c, offset, data);
    }

    /// Запись в PCI config space.
    pub fn config_write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            // Command register: принимаем memory/bus master, остальное игнорируем.
            0x04 => {
                if let Some(&b) = data.first() {
                    self.config[0x04] = b & 0x06;
                }
            }
            0x10 => {
                let v = to_u64(data) as u32;
                if v == 0xFFFF_FFFF {
                    self.bar_sizing = true;
                } else {
                    self.bar_sizing = false;
                    #[allow(clippy::cast_possible_truncation, reason = "BAR 32-битный")]
                    let mask = !(BAR_SIZE as u32 - 1);
                    self.bar0 = v & mask;
                }
            }
            o if o == CAP_MSIX + 2 || o == CAP_MSIX + 3 => {
                let hi = if o == CAP_MSIX + 2 {
                    data.get(1).copied()
                } else {
                    data.first().copied()
                };
                if let Some(hi) = hi {
                    let was_masked = self.msix.function_masked;
                    self.msix.enabled = hi & 0x80 != 0;
                    self.msix.function_masked = hi & 0x40 != 0;
                    if was_masked && !self.msix.function_masked {
                        for v in self.msix.take_pending() {
                            self.deliver(v);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Включена ли декодировка памяти (бит 1 Command).
    #[must_use]
    pub fn memory_enabled(&self) -> bool {
        self.config[0x04] & 0x02 != 0
    }

    // ---------- BAR0 ----------

    fn all_features(&self) -> u64 {
        self.device.device_features()
            | VIRTIO_F_VERSION_1
            | VIRTIO_RING_F_EVENT_IDX
            | VIRTIO_RING_F_INDIRECT_DESC
    }

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(usize::from(self.queue_select))
    }

    /// Чтение BAR0 по смещению.
    pub fn bar_read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        match offset {
            o if o < COMMON_OFFSET + COMMON_SIZE => self.common_read(o - COMMON_OFFSET, data),
            o if (ISR_OFFSET..ISR_OFFSET + 4).contains(&o) => {
                // Чтение ISR сбрасывает его.
                if let Some(b) = data.first_mut() {
                    *b = self.isr;
                }
                self.isr = 0;
            }
            o if (DEVICE_OFFSET..DEVICE_OFFSET + 0x1000).contains(&o) => {
                self.device.read_config(o - DEVICE_OFFSET, data);
            }
            o if (MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + self.msix.table_size()).contains(&o) => {
                self.msix.read_table(o - MSIX_TABLE_OFFSET, data);
            }
            o if (MSIX_PBA_OFFSET..MSIX_PBA_OFFSET + 0x1000).contains(&o) => {
                self.msix.read_pba(o - MSIX_PBA_OFFSET, data);
            }
            _ => {}
        }
    }

    /// Запись в BAR0. Запись в notify обрабатывает очередь синхронно (путь без doorbell).
    pub fn bar_write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            o if o < COMMON_OFFSET + COMMON_SIZE => self.common_write(o - COMMON_OFFSET, data),
            o if (DEVICE_OFFSET..DEVICE_OFFSET + 0x1000).contains(&o) => {
                self.device.write_config(o - DEVICE_OFFSET, data);
            }
            o if (NOTIFY_OFFSET..NOTIFY_OFFSET + 0x1000).contains(&o) => {
                let q = ((o - NOTIFY_OFFSET) / u64::from(NOTIFY_MULTIPLIER)) as usize;
                self.notify(q);
            }
            o if (MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + self.msix.table_size()).contains(&o) => {
                if let Some(v) = self.msix.write_table(o - MSIX_TABLE_OFFSET, data) {
                    self.deliver(v);
                }
            }
            _ => {}
        }
    }

    fn common_read(&mut self, off: u64, data: &mut [u8]) {
        let feature_word = |features: u64, select: u32| -> u32 {
            match select {
                0 => (features & 0xFFFF_FFFF) as u32,
                1 => (features >> 32) as u32,
                _ => 0,
            }
        };
        let num_queues = self.queues.len() as u16;
        let qsel = self.queue_select;
        let q = self.queues.get(usize::from(qsel)).cloned();
        let value: u64 = match off {
            0x00 => u64::from(self.device_feature_select),
            0x04 => u64::from(feature_word(
                self.all_features(),
                self.device_feature_select,
            )),
            0x08 => u64::from(self.driver_feature_select),
            0x0C => u64::from(feature_word(
                self.driver_features,
                self.driver_feature_select,
            )),
            0x10 => u64::from(self.msix_config),
            0x12 => u64::from(num_queues),
            0x14 => u64::from(self.status),
            0x15 => u64::from(self.config_generation),
            0x16 => u64::from(qsel),
            0x18 => q.as_ref().map_or(0, |q| u64::from(q.size)),
            0x1A => q
                .as_ref()
                .map_or(u64::from(NO_VECTOR), |q| u64::from(q.msix_vector)),
            0x1C => q.as_ref().map_or(0, |q| u64::from(q.ready)),
            0x1E => u64::from(qsel), // queue_notify_off = индекс очереди
            0x20 => q.as_ref().map_or(0, |q| q.desc_table),
            0x24 => q.as_ref().map_or(0, |q| q.desc_table >> 32),
            0x28 => q.as_ref().map_or(0, |q| q.avail_ring),
            0x2C => q.as_ref().map_or(0, |q| q.avail_ring >> 32),
            0x30 => q.as_ref().map_or(0, |q| q.used_ring),
            0x34 => q.as_ref().map_or(0, |q| q.used_ring >> 32),
            _ => 0,
        };
        read_window(&value.to_le_bytes(), 0, data);
    }

    fn set_addr(field: &mut u64, high: bool, v: u64) {
        if high {
            *field = (*field & 0xFFFF_FFFF) | (v << 32);
        } else {
            *field = (*field & !0xFFFF_FFFF) | (v & 0xFFFF_FFFF);
        }
    }

    fn common_write(&mut self, off: u64, data: &[u8]) {
        let v = to_u64(data);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "ширина полей common config"
        )]
        match off {
            0x00 => self.device_feature_select = v as u32,
            0x08 => self.driver_feature_select = v as u32,
            0x0C => {
                let shift = match self.driver_feature_select {
                    0 => 0,
                    1 => 32,
                    _ => return,
                };
                let mask = 0xFFFF_FFFF_u64 << shift;
                self.driver_features = (self.driver_features & !mask) | ((v << shift) & mask);
                self.driver_features &= self.all_features();
            }
            0x10 => self.msix_config = v as u16,
            0x14 => self.write_status(v as u8),
            0x16 => self.queue_select = v as u16,
            0x18 => {
                if let Some(q) = self.selected_queue() {
                    let size = (v as u16).min(q.max_size);
                    q.size = size;
                }
            }
            0x1A => {
                let vectors = self.msix.len();
                if let Some(q) = self.selected_queue() {
                    let vec = v as u16;
                    q.msix_vector = if usize::from(vec) < vectors {
                        vec
                    } else {
                        NO_VECTOR
                    };
                }
            }
            0x1C => {
                let event_idx = self.driver_features & VIRTIO_RING_F_EVENT_IDX != 0;
                if let Some(q) = self.selected_queue() {
                    q.ready = v & 1 != 0;
                    q.event_idx = event_idx;
                }
            }
            0x20 | 0x24 => {
                if let Some(q) = self.selected_queue() {
                    Self::set_addr(&mut q.desc_table, off == 0x24, v);
                }
            }
            0x28 | 0x2C => {
                if let Some(q) = self.selected_queue() {
                    Self::set_addr(&mut q.avail_ring, off == 0x2C, v);
                }
            }
            0x30 | 0x34 => {
                if let Some(q) = self.selected_queue() {
                    Self::set_addr(&mut q.used_ring, off == 0x34, v);
                }
            }
            _ => {}
        }
    }

    fn write_status(&mut self, status: u8) {
        if status == 0 {
            tracing::debug!(
                device = self.device.device_type(),
                "reset virtio-устройства"
            );
            self.status = 0;
            self.driver_features = 0;
            self.active = false;
            self.isr = 0;
            self.msix_config = NO_VECTOR;
            for q in &mut self.queues {
                q.reset();
            }
            self.device.reset();
            return;
        }
        self.status = status;
        if status & STATUS_DRIVER_OK != 0 && status & STATUS_FAILED == 0 && !self.active {
            self.active = true;
            tracing::info!(
                device = self.device.device_type(),
                features = format_args!("0x{:x}", self.driver_features),
                "virtio-устройство активировано"
            );
            self.device.activate(self.driver_features);
        }
    }

    fn deliver(&mut self, vector: u16) {
        if let Some(msg) = self.msix.message(vector) {
            self.msi.signal(msg);
        }
    }

    /// Обрабатывает очередь `index` (по doorbell или записи в notify) и шлёт прерывание,
    /// если оно нужно драйверу.
    pub fn notify(&mut self, index: usize) {
        if !self.active {
            return;
        }
        let Some(queue) = self.queues.get_mut(index) else {
            return;
        };
        if !queue.ready {
            return;
        }
        match self.device.process_queue(index, queue, self.mem.as_ref()) {
            Ok(true) => {
                let vector = queue.msix_vector;
                match queue.needs_notification(self.mem.as_ref()) {
                    Ok(true) => {
                        self.isr |= 1;
                        if vector != NO_VECTOR {
                            self.deliver(vector);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, queue = index, "ошибка чтения used_event"),
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::error!(error = %e, queue = index, device = self.device.device_type(), "ошибка очереди, устройство остановлено");
                self.status |= 0x40; // DEVICE_NEEDS_RESET
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::mem::{GuestMemoryExt, VecMemory};
    use crate::msix::MsiMessage;
    use crate::queue::{Chain, QueueError};

    #[derive(Default)]
    struct Sink(Mutex<Vec<MsiMessage>>);
    impl MsiSink for Sink {
        fn signal(&self, msg: MsiMessage) {
            self.0.lock().unwrap().push(msg);
        }
    }

    /// Устройство-эхо: возвращает каждый запрос с длиной 7.
    struct Echo {
        sizes: [u16; 1],
        chain: Chain,
        activated: Option<u64>,
    }
    impl VirtioDevice for Echo {
        fn device_type(&self) -> u32 {
            crate::device::TYPE_CONSOLE
        }
        fn device_features(&self) -> u64 {
            1
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &self.sizes
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn activate(&mut self, features: u64) {
            self.activated = Some(features);
        }
        fn process_queue(
            &mut self,
            _: usize,
            q: &mut Queue,
            mem: &dyn GuestMemory,
        ) -> Result<bool, QueueError> {
            let mut any = false;
            while q.pop(mem, &mut self.chain)? {
                q.add_used(mem, self.chain.head, 7)?;
                any = true;
            }
            Ok(any)
        }
    }

    fn setup() -> (VirtioPci, Arc<VecMemory>, Arc<Sink>) {
        let mem = Arc::new(VecMemory::new(0x10000));
        let sink = Arc::new(Sink::default());
        let dev = Echo {
            sizes: [8],
            chain: Chain::default(),
            activated: None,
        };
        let pci = VirtioPci::new(Box::new(dev), 0xC000_0000, mem.clone(), sink.clone());
        (pci, mem, sink)
    }

    fn w32(p: &mut VirtioPci, off: u64, v: u32) {
        p.bar_write(off, &v.to_le_bytes());
    }
    fn r32(p: &mut VirtioPci, off: u64) -> u32 {
        let mut b = [0; 4];
        p.bar_read(off, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn config_space_identity_and_caps() {
        let (p, ..) = setup();
        let mut b = [0_u8; 4];
        p.config_read(0, &mut b);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), 0x1AF4);
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), 0x1043);
        // Цепочка capability: MSI-X → common → ISR → device → notify.
        let mut ptr = [0_u8; 1];
        p.config_read(0x34, &mut ptr);
        let mut seen = Vec::new();
        let mut at = u64::from(ptr[0]);
        while at != 0 {
            let mut hdr = [0_u8; 4];
            p.config_read(at, &mut hdr);
            seen.push((hdr[0], hdr[3]));
            at = u64::from(hdr[1]);
        }
        assert_eq!(
            seen,
            vec![(0x11, 0x00), (0x09, 1), (0x09, 3), (0x09, 4), (0x09, 2)]
        );
    }

    #[test]
    fn bar_sizing_protocol() {
        let (mut p, ..) = setup();
        p.config_write(0x10, &0xFFFF_FFFF_u32.to_le_bytes());
        let mut b = [0_u8; 4];
        p.config_read(0x10, &mut b);
        assert_eq!(u32::from_le_bytes(b), 0xFFFF_8000);
        p.config_write(0x10, &0xD000_0000_u32.to_le_bytes());
        p.config_read(0x10, &mut b);
        assert_eq!(u32::from_le_bytes(b), 0xD000_0000);
        assert_eq!(p.notify_address(0), 0xD000_3000);
    }

    #[test]
    fn feature_negotiation_and_activation() {
        let (mut p, ..) = setup();
        w32(&mut p, 0x00, 1);
        assert_eq!(r32(&mut p, 0x04), 0x1 | (1 << 0)); // VERSION_1 = бит 32 → слово 1, бит 0
        w32(&mut p, 0x00, 0);
        assert_eq!(r32(&mut p, 0x04) & (1 << 29), 1 << 29); // EVENT_IDX
        // Драйвер принимает VERSION_1 и EVENT_IDX, лишний бит отбрасывается.
        w32(&mut p, 0x08, 0);
        w32(&mut p, 0x0C, (1 << 29) | (1 << 5));
        w32(&mut p, 0x08, 1);
        w32(&mut p, 0x0C, 1);
        p.bar_write(0x14, &[1 | 2 | 8 | 4]);
        assert!(p.is_active());
        assert_eq!(r32(&mut p, 0x08), 1);
    }

    #[test]
    fn queue_setup_notify_and_msix_interrupt() {
        let (mut p, mem, sink) = setup();
        // MSI-X: включить в config space, запрограммировать вектор 1.
        p.config_write(CAP_MSIX + 2, &0x8001_u16.to_le_bytes());
        w32(&mut p, MSIX_TABLE_OFFSET + 16, 0xFEE0_0000);
        w32(&mut p, MSIX_TABLE_OFFSET + 16 + 8, 0x45);
        w32(&mut p, MSIX_TABLE_OFFSET + 16 + 12, 0);
        // Очередь 0.
        p.bar_write(0x16, &0_u16.to_le_bytes());
        assert_eq!(r32(&mut p, 0x18) & 0xFFFF, 8);
        p.bar_write(0x1A, &1_u16.to_le_bytes());
        w32(&mut p, 0x20, 0x1000);
        w32(&mut p, 0x28, 0x2000);
        w32(&mut p, 0x30, 0x3000);
        p.bar_write(0x1C, &1_u16.to_le_bytes());
        p.bar_write(0x14, &[0x0F]);

        // Запрос в avail ring.
        mem.write(0x1000, &[0, 0x50, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        mem.write_u16(0x2004, 0).unwrap();
        mem.write_u16(0x2002, 1).unwrap();
        p.bar_write(NOTIFY_OFFSET, &0_u16.to_le_bytes());

        assert_eq!(mem.read_u16(0x3002).unwrap(), 1);
        assert_eq!(mem.read_u32(0x3008).unwrap(), 7);
        let msgs = sink.0.lock().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].vector(), 0x45);
    }

    #[test]
    fn reset_clears_state() {
        let (mut p, ..) = setup();
        p.bar_write(0x14, &[0x0F]);
        assert!(p.is_active());
        p.bar_write(0x14, &[0]);
        assert!(!p.is_active());
        assert_eq!(r32(&mut p, 0x14) & 0xFF, 0);
    }

    #[test]
    fn device_config_window() {
        let (mut p, ..) = setup();
        let mut b = [0_u8; 2];
        p.bar_read(DEVICE_OFFSET + 5, &mut b);
        assert_eq!(b, [5, 5]);
    }
}
