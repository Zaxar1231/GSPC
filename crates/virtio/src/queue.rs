//! Split virtqueue (virtio 1.x, раздел 2.7) с `VIRTIO_F_EVENT_IDX` и indirect-дескрипторами.
//!
//! Горячий путь устройства: `pop` → обработка → `add_used` → `needs_notification`.
//! Аллокаций нет: цепочка дескрипторов пишется в переиспользуемый [`Chain`].

use std::sync::atomic::{Ordering, fence};

use crate::mem::{GuestMemory, GuestMemoryExt, MemoryError};

/// Дескриптор продолжается в `next`.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Буфер доступен устройству на запись.
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Дескриптор указывает на таблицу indirect-дескрипторов.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;
const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
const DESC_SIZE: u64 = 16;

/// Ошибки разбора очереди (некорректный драйвер гостя).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    /// Обращение вне RAM.
    #[error(transparent)]
    Memory(#[from] MemoryError),
    /// Индекс дескриптора за пределами таблицы.
    #[error("индекс дескриптора {0} вне таблицы")]
    DescriptorIndex(u16),
    /// Цепочка длиннее допустимого (цикл).
    #[error("цепочка дескрипторов слишком длинная (возможен цикл)")]
    ChainTooLong,
    /// Некорректная таблица indirect-дескрипторов.
    #[error("некорректная indirect-таблица")]
    BadIndirect,
    /// Очередь не готова.
    #[error("очередь не настроена")]
    NotReady,
}

/// Дескриптор буфера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Descriptor {
    /// GPA буфера.
    pub addr: u64,
    /// Длина.
    pub len: u32,
    /// Флаги.
    pub flags: u16,
    /// Следующий дескриптор.
    pub next: u16,
}

impl Descriptor {
    /// Буфер для записи устройством.
    #[must_use]
    pub fn is_write(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }
}

/// Цепочка дескрипторов одного запроса (переиспользуется между вызовами).
#[derive(Debug, Default)]
pub struct Chain {
    /// Индекс головного дескриптора (возвращается в used ring).
    pub head: u16,
    /// Дескрипторы по порядку.
    pub descriptors: Vec<Descriptor>,
}

/// Состояние одной очереди.
#[derive(Debug, Clone)]
pub struct Queue {
    /// Максимальный размер (сообщает устройство).
    pub max_size: u16,
    /// Размер, выбранный драйвером.
    pub size: u16,
    /// Включена драйвером.
    pub ready: bool,
    /// GPA таблицы дескрипторов.
    pub desc_table: u64,
    /// GPA avail ring (driver area).
    pub avail_ring: u64,
    /// GPA used ring (device area).
    pub used_ring: u64,
    /// Вектор MSI-X очереди.
    pub msix_vector: u16,
    /// Согласован `VIRTIO_F_EVENT_IDX`.
    pub event_idx: bool,
    next_avail: u16,
    next_used: u16,
    signalled_used: Option<u16>,
}

/// «Нет вектора» в MSI-X.
pub const NO_VECTOR: u16 = 0xFFFF;

impl Queue {
    /// Новая очередь с максимальным размером `max_size` (степень двойки).
    #[must_use]
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            desc_table: 0,
            avail_ring: 0,
            used_ring: 0,
            msix_vector: NO_VECTOR,
            event_idx: false,
            next_avail: 0,
            next_used: 0,
            signalled_used: None,
        }
    }

    /// Сброс к состоянию после reset устройства.
    pub fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    fn check_ready(&self) -> Result<(), QueueError> {
        if self.ready && self.size > 0 && self.size.is_power_of_two() {
            Ok(())
        } else {
            Err(QueueError::NotReady)
        }
    }

    fn read_desc(mem: &dyn GuestMemory, table: u64, index: u16) -> Result<Descriptor, QueueError> {
        let mut b = [0_u8; 16];
        mem.read(table + u64::from(index) * DESC_SIZE, &mut b)?;
        let [
            a0,
            a1,
            a2,
            a3,
            a4,
            a5,
            a6,
            a7,
            l0,
            l1,
            l2,
            l3,
            f0,
            f1,
            n0,
            n1,
        ] = b;
        Ok(Descriptor {
            addr: u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
            len: u32::from_le_bytes([l0, l1, l2, l3]),
            flags: u16::from_le_bytes([f0, f1]),
            next: u16::from_le_bytes([n0, n1]),
        })
    }

    /// Есть ли новые запросы.
    ///
    /// # Errors
    /// Очередь не настроена или avail ring вне RAM.
    pub fn has_pending(&self, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        self.check_ready()?;
        Ok(mem.read_u16(self.avail_ring + 2)? != self.next_avail)
    }

    /// Забирает следующий запрос в `chain`. `false`, если новых запросов нет.
    ///
    /// # Errors
    /// Некорректные структуры очереди.
    pub fn pop(&mut self, mem: &dyn GuestMemory, chain: &mut Chain) -> Result<bool, QueueError> {
        self.check_ready()?;
        let avail_idx = mem.read_u16(self.avail_ring + 2)?;
        if avail_idx == self.next_avail {
            return Ok(false);
        }
        // Индекс прочитан до содержимого кольца и дескрипторов.
        fence(Ordering::Acquire);
        let slot = u64::from(self.next_avail % self.size);
        let head = mem.read_u16(self.avail_ring + 4 + slot * 2)?;
        self.next_avail = self.next_avail.wrapping_add(1);
        if self.event_idx {
            // avail_event: хотим уведомление о следующем запросе.
            mem.write_u16(
                self.used_ring + 4 + u64::from(self.size) * 8,
                self.next_avail,
            )?;
        }

        chain.head = head;
        chain.descriptors.clear();
        let mut index = head;
        let mut table = self.desc_table;
        let mut table_len = self.size;
        let mut indirect = false;
        loop {
            if index >= table_len {
                return Err(QueueError::DescriptorIndex(index));
            }
            let d = Self::read_desc(mem, table, index)?;
            if d.flags & VIRTQ_DESC_F_INDIRECT != 0 {
                if indirect || d.len == 0 || d.len % 16 != 0 {
                    return Err(QueueError::BadIndirect);
                }
                table = d.addr;
                table_len = u16::try_from(d.len / 16).map_err(|_| QueueError::BadIndirect)?;
                index = 0;
                indirect = true;
                continue;
            }
            if chain.descriptors.len() >= usize::from(table_len) {
                return Err(QueueError::ChainTooLong);
            }
            chain.descriptors.push(d);
            if d.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            index = d.next;
        }
        Ok(true)
    }

    /// Возвращает запрос драйверу: `len` — сколько байт устройство записало в буферы.
    ///
    /// # Errors
    /// Used ring вне RAM.
    pub fn add_used(
        &mut self,
        mem: &dyn GuestMemory,
        head: u16,
        len: u32,
    ) -> Result<(), QueueError> {
        self.check_ready()?;
        let slot = u64::from(self.next_used % self.size);
        let elem = self.used_ring + 4 + slot * 8;
        mem.write_u32(elem, u32::from(head))?;
        mem.write_u32(elem + 4, len)?;
        self.next_used = self.next_used.wrapping_add(1);
        // Элемент виден драйверу до нового индекса.
        fence(Ordering::Release);
        mem.write_u16(self.used_ring + 2, self.next_used)?;
        Ok(())
    }

    /// Нужно ли прерывание после добавленных used-элементов.
    ///
    /// # Errors
    /// Avail ring вне RAM.
    pub fn needs_notification(&mut self, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        self.check_ready()?;
        // used idx записан до чтения used_event / flags.
        fence(Ordering::SeqCst);
        let new = self.next_used;
        if self.event_idx {
            let used_event = mem.read_u16(self.avail_ring + 4 + u64::from(self.size) * 2)?;
            let old = self.signalled_used.replace(new);
            Ok(match old {
                // vring_need_event(event, new, old)
                Some(old) => new.wrapping_sub(used_event).wrapping_sub(1) < new.wrapping_sub(old),
                None => true,
            })
        } else {
            Ok(mem.read_u16(self.avail_ring)? & VIRTQ_AVAIL_F_NO_INTERRUPT == 0)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mem::VecMemory;

    pub(crate) const DESC: u64 = 0x1000;
    pub(crate) const AVAIL: u64 = 0x2000;
    pub(crate) const USED: u64 = 0x3000;

    /// Драйвер-эмулятор для тестов.
    pub(crate) struct Driver {
        pub(crate) avail_idx: u16,
        pub(crate) size: u16,
    }

    impl Driver {
        pub(crate) fn queue(size: u16) -> (Queue, Self) {
            let mut q = Queue::new(size);
            q.desc_table = DESC;
            q.avail_ring = AVAIL;
            q.used_ring = USED;
            q.ready = true;
            (q, Self { avail_idx: 0, size })
        }

        pub(crate) fn desc(&self, mem: &VecMemory, i: u16, d: Descriptor) {
            let mut b = [0_u8; 16];
            b[..8].copy_from_slice(&d.addr.to_le_bytes());
            b[8..12].copy_from_slice(&d.len.to_le_bytes());
            b[12..14].copy_from_slice(&d.flags.to_le_bytes());
            b[14..16].copy_from_slice(&d.next.to_le_bytes());
            mem.write(DESC + u64::from(i) * 16, &b).unwrap();
        }

        pub(crate) fn publish(&mut self, mem: &VecMemory, head: u16) {
            let slot = u64::from(self.avail_idx % self.size);
            mem.write_u16(AVAIL + 4 + slot * 2, head).unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            mem.write_u16(AVAIL + 2, self.avail_idx).unwrap();
        }

        pub(crate) fn used_idx(mem: &VecMemory) -> u16 {
            mem.read_u16(USED + 2).unwrap()
        }
    }

    fn d(addr: u64, len: u32, flags: u16, next: u16) -> Descriptor {
        Descriptor {
            addr,
            len,
            flags,
            next,
        }
    }

    #[test]
    fn pops_chain_and_returns_used() {
        let mem = VecMemory::new(0x10000);
        let (mut q, mut drv) = Driver::queue(8);
        drv.desc(&mem, 0, d(0x5000, 16, VIRTQ_DESC_F_NEXT, 1));
        drv.desc(
            &mem,
            1,
            d(0x6000, 512, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2),
        );
        drv.desc(&mem, 2, d(0x7000, 1, VIRTQ_DESC_F_WRITE, 0));
        drv.publish(&mem, 0);

        let mut chain = Chain::default();
        assert!(q.has_pending(&mem).unwrap());
        assert!(q.pop(&mem, &mut chain).unwrap());
        assert_eq!(chain.head, 0);
        assert_eq!(chain.descriptors.len(), 3);
        assert!(!chain.descriptors[0].is_write());
        assert!(chain.descriptors[1].is_write());
        assert!(!q.pop(&mem, &mut chain).unwrap());

        q.add_used(&mem, 0, 513).unwrap();
        assert_eq!(Driver::used_idx(&mem), 1);
        assert_eq!(mem.read_u32(USED + 4).unwrap(), 0);
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 513);
        assert!(q.needs_notification(&mem).unwrap());
    }

    #[test]
    fn indirect_descriptors() {
        let mem = VecMemory::new(0x10000);
        let (mut q, mut drv) = Driver::queue(4);
        // Indirect-таблица по адресу 0x8000 из двух дескрипторов.
        let table = [
            d(0x5000, 16, VIRTQ_DESC_F_NEXT, 1),
            d(0x6000, 4096, VIRTQ_DESC_F_WRITE, 0),
        ];
        for (i, t) in table.iter().enumerate() {
            let mut b = [0_u8; 16];
            b[..8].copy_from_slice(&t.addr.to_le_bytes());
            b[8..12].copy_from_slice(&t.len.to_le_bytes());
            b[12..14].copy_from_slice(&t.flags.to_le_bytes());
            b[14..16].copy_from_slice(&t.next.to_le_bytes());
            mem.write(0x8000 + i as u64 * 16, &b).unwrap();
        }
        drv.desc(&mem, 0, d(0x8000, 32, VIRTQ_DESC_F_INDIRECT, 0));
        drv.publish(&mem, 0);
        let mut chain = Chain::default();
        assert!(q.pop(&mem, &mut chain).unwrap());
        assert_eq!(chain.descriptors.len(), 2);
        assert_eq!(chain.descriptors[1].len, 4096);
    }

    #[test]
    fn loop_is_detected() {
        let mem = VecMemory::new(0x10000);
        let (mut q, mut drv) = Driver::queue(4);
        drv.desc(&mem, 0, d(0x5000, 1, VIRTQ_DESC_F_NEXT, 1));
        drv.desc(&mem, 1, d(0x5000, 1, VIRTQ_DESC_F_NEXT, 0));
        drv.publish(&mem, 0);
        let mut chain = Chain::default();
        assert_eq!(q.pop(&mem, &mut chain), Err(QueueError::ChainTooLong));
    }

    #[test]
    fn out_of_range_index_is_error() {
        let mem = VecMemory::new(0x10000);
        let (mut q, mut drv) = Driver::queue(4);
        drv.desc(&mem, 0, d(0x5000, 1, VIRTQ_DESC_F_NEXT, 9));
        drv.publish(&mem, 0);
        let mut chain = Chain::default();
        assert_eq!(q.pop(&mem, &mut chain), Err(QueueError::DescriptorIndex(9)));
    }

    #[test]
    fn not_ready_queue_is_error() {
        let mem = VecMemory::new(0x10000);
        let q = Queue::new(8);
        assert_eq!(q.has_pending(&mem), Err(QueueError::NotReady));
    }

    #[test]
    fn event_idx_suppresses_interrupts() {
        let mem = VecMemory::new(0x10000);
        let (mut q, mut drv) = Driver::queue(8);
        q.event_idx = true;
        for i in 0..3 {
            drv.desc(&mem, i, d(0x5000, 1, 0, 0));
        }
        // used_event = 2: драйвер хочет прерывание, когда used idx перейдёт 2.
        mem.write_u16(AVAIL + 4 + 8 * 2, 2).unwrap();
        let mut chain = Chain::default();
        let mut irqs = Vec::new();
        for i in 0..3 {
            drv.publish(&mem, i);
            assert!(q.pop(&mem, &mut chain).unwrap());
            q.add_used(&mem, chain.head, 0).unwrap();
            irqs.push(q.needs_notification(&mem).unwrap());
        }
        // Первое — всегда; второе (used=2) подавлено; третье (used=3 > event 2) — нужно.
        assert_eq!(irqs, vec![true, false, true]);
        // avail_event обновлён устройством.
        assert_eq!(mem.read_u16(USED + 4 + 8 * 8).unwrap(), 3);
    }

    #[test]
    fn no_interrupt_flag_respected_without_event_idx() {
        let mem = VecMemory::new(0x10000);
        let (mut q, mut drv) = Driver::queue(8);
        drv.desc(&mem, 0, d(0x5000, 1, 0, 0));
        drv.publish(&mem, 0);
        mem.write_u16(AVAIL, VIRTQ_AVAIL_F_NO_INTERRUPT).unwrap();
        let mut chain = Chain::default();
        q.pop(&mem, &mut chain).unwrap();
        q.add_used(&mem, 0, 0).unwrap();
        assert!(!q.needs_notification(&mem).unwrap());
    }
}
