//! Шина PCI: конфигурационное пространство через ECAM (шина 0) и маршрутизация BAR.
//!
//! BAR назначаются заранее (как это сделала бы прошивка) из окна
//! [`PCI_MMIO_START`]..[`PCI_MMIO_END`]; ядро обычно оставляет их на месте. Если ядро
//! переназначит BAR, обращения продолжат работать: шина ищет устройство по текущему BAR.

use std::sync::{Arc, Mutex, PoisonError};

use rustdroid_virtio::VirtioPci;
use rustdroid_virtio::pci::BAR_SIZE;

use crate::layout::{ECAM_SIZE, PCI_MMIO_END, PCI_MMIO_START, PCI_MMIO64_END, PCI_MMIO64_START};

/// Функция PCI, разделяемая между vCPU (config/MMIO) и worker-потоком устройства.
pub type SharedFunction = Arc<Mutex<VirtioPci>>;

#[derive(Debug)]
struct Slot {
    device: u8,
    function: SharedFunction,
}

/// Шина PCI 0.
#[derive(Debug)]
pub struct PciBus {
    slots: Vec<Slot>,
    next_bar: u64,
    next_bar64: u64,
}

impl Default for PciBus {
    fn default() -> Self {
        Self::new()
    }
}

impl PciBus {
    /// Пустая шина.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            next_bar: PCI_MMIO_START,
            next_bar64: PCI_MMIO64_START,
        }
    }

    /// Следующий свободный адрес BAR0; `None`, если окно MMIO исчерпано.
    pub fn allocate_bar(&mut self) -> Option<u32> {
        let addr = self.next_bar.div_ceil(BAR_SIZE) * BAR_SIZE;
        if addr + BAR_SIZE > PCI_MMIO_END {
            return None;
        }
        self.next_bar = addr + BAR_SIZE;
        u32::try_from(addr).ok()
    }

    /// Адрес 64-битного BAR размера `size` (степень двойки) в окне выше RAM; `None`, если окно
    /// исчерпано.
    pub fn allocate_bar64(&mut self, size: u64) -> Option<u64> {
        if !size.is_power_of_two() {
            return None;
        }
        let addr = self.next_bar64.div_ceil(size) * size;
        if addr.checked_add(size)? > PCI_MMIO64_END {
            return None;
        }
        self.next_bar64 = addr + size;
        Some(addr)
    }

    /// Добавляет функцию; возвращает номер устройства (слоты 1..31, 0 — свободен).
    pub fn add(&mut self, function: SharedFunction) -> Option<u8> {
        #[allow(clippy::cast_possible_truncation, reason = "не больше 31 устройства")]
        let device = self.slots.len() as u8 + 1;
        if device > 31 {
            return None;
        }
        self.slots.push(Slot { device, function });
        Some(device)
    }

    /// Функции шины с номерами устройств.
    pub fn functions(&self) -> impl Iterator<Item = (u8, &SharedFunction)> {
        self.slots.iter().map(|s| (s.device, &s.function))
    }

    fn decode(offset: u64) -> Option<(u8, u8, u64)> {
        if offset >= ECAM_SIZE {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, reason = "поля ECAM")]
        let (dev, func) = (((offset >> 15) & 0x1F) as u8, ((offset >> 12) & 0x7) as u8);
        Some((dev, func, offset & 0xFFF))
    }

    fn find(&self, dev: u8, func: u8) -> Option<&SharedFunction> {
        if func != 0 {
            return None;
        }
        self.slots
            .iter()
            .find(|s| s.device == dev)
            .map(|s| &s.function)
    }

    /// Чтение ECAM по смещению от базы.
    pub fn ecam_read(&self, offset: u64, data: &mut [u8]) {
        match Self::decode(offset).and_then(|(d, f, reg)| self.find(d, f).map(|func| (func, reg))) {
            Some((func, reg)) if reg < 256 => {
                func.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .config_read(reg, data);
            }
            Some(_) => data.fill(0),
            // Нет устройства: все единицы (Vendor ID 0xFFFF).
            None => data.fill(0xFF),
        }
    }

    /// Запись ECAM.
    pub fn ecam_write(&self, offset: u64, data: &[u8]) {
        if let Some((func, reg)) =
            Self::decode(offset).and_then(|(d, f, reg)| self.find(d, f).map(|func| (func, reg)))
            && reg < 256
        {
            func.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .config_write(reg, data);
        }
    }

    /// Функция, чей BAR0 содержит `gpa`, и смещение внутри BAR.
    #[must_use]
    pub fn bar_lookup(&self, gpa: u64) -> Option<(&SharedFunction, u64)> {
        self.slots.iter().find_map(|s| {
            let bar = s
                .function
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .bar0();
            (bar != 0 && gpa >= bar && gpa < bar + BAR_SIZE).then(|| (&s.function, gpa - bar))
        })
    }
}

#[cfg(test)]
mod tests {
    use rustdroid_virtio::mem::VecMemory;
    use rustdroid_virtio::msix::{MsiMessage, MsiSink};
    use rustdroid_virtio::{GuestMemory, Queue, QueueError, VirtioDevice};

    use super::*;

    struct Nop([u16; 1]);
    impl VirtioDevice for Nop {
        fn device_type(&self) -> u32 {
            2
        }
        fn device_features(&self) -> u64 {
            0
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &self.0
        }
        fn read_config(&self, _: u64, d: &mut [u8]) {
            d.fill(0);
        }
        fn process_queue(
            &mut self,
            _: usize,
            _: &mut Queue,
            _: &dyn GuestMemory,
        ) -> Result<bool, QueueError> {
            Ok(false)
        }
    }
    struct NoMsi;
    impl MsiSink for NoMsi {
        fn signal(&self, _: MsiMessage) {}
    }

    fn bus_with_device() -> PciBus {
        let mut bus = PciBus::new();
        let bar = bus.allocate_bar().unwrap();
        let f = VirtioPci::new(
            Box::new(Nop([8])),
            bar,
            Arc::new(VecMemory::new(4096)),
            Arc::new(NoMsi),
        );
        assert_eq!(bus.add(Arc::new(Mutex::new(f))), Some(1));
        bus
    }

    #[test]
    fn ecam_finds_device_and_reports_absent_slots() {
        let bus = bus_with_device();
        let mut id = [0_u8; 4];
        bus.ecam_read(1 << 15, &mut id);
        assert_eq!(u16::from_le_bytes([id[0], id[1]]), 0x1AF4);
        bus.ecam_read(2 << 15, &mut id);
        assert_eq!(id, [0xFF; 4]);
        bus.ecam_read((1 << 15) | (1 << 12), &mut id);
        assert_eq!(id, [0xFF; 4]);
    }

    #[test]
    fn bar_lookup_follows_reprogramming() {
        let bus = bus_with_device();
        let (_, off) = bus.bar_lookup(PCI_MMIO_START + 0x3000).unwrap();
        assert_eq!(off, 0x3000);
        // Ядро переносит BAR.
        bus.ecam_write((1 << 15) | 0x10, &0xD000_0000_u32.to_le_bytes());
        assert!(bus.bar_lookup(PCI_MMIO_START).is_none());
        assert!(bus.bar_lookup(0xD000_0010).is_some());
    }

    #[test]
    fn bar64_allocation_is_aligned_and_bounded() {
        let mut bus = PciBus::new();
        assert_eq!(bus.allocate_bar64(4 << 30), Some(PCI_MMIO64_START));
        assert_eq!(
            bus.allocate_bar64(1 << 20),
            Some(PCI_MMIO64_START + (4 << 30))
        );
        // Следующий 4 ГиБ выравнивается и уже не помещается в окно 8 ГиБ.
        assert_eq!(bus.allocate_bar64(4 << 30), None);
        assert_eq!(bus.allocate_bar64(3 << 20), None);
    }

    #[test]
    fn bar_allocation_is_aligned_and_bounded() {
        let mut bus = PciBus::new();
        let a = bus.allocate_bar().unwrap();
        let b = bus.allocate_bar().unwrap();
        assert_eq!(u64::from(b) - u64::from(a), BAR_SIZE);
        assert_eq!(u64::from(a) % BAR_SIZE, 0);
    }
}
