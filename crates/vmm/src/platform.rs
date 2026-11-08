//! Шина платформы: RAM, COM1, IOAPIC, PCI (ECAM и BAR) и заглушки legacy-портов.
//!
//! Это не горячий путь: legacy-устройства используются только при загрузке и для консоли.
//! virtio (этап 2) пойдёт через doorbell-события и MSI-X мимо этой шины.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rustdroid_devices::{InterruptLine, Serial16550};

use crate::ioapic::{InterruptSink, IoApic};
use crate::layout::{ECAM_ADDR, ECAM_SIZE, IOAPIC_ADDR, IOAPIC_SIZE};
use crate::memory::GuestRam;
use crate::pci::PciBus;

/// Базовый порт COM1.
pub const COM1_PORT: u16 = 0x3F8;
/// IRQ COM1.
pub const COM1_IRQ: usize = 4;
const I8042_DATA: u16 = 0x60;
const I8042_COMMAND: u16 = 0x64;
const I8042_CMD_RESET: u8 = 0xFE;
const MAX_UNKNOWN_LOGGED: usize = 64;

/// События ВМ, которые выставляют устройства.
#[derive(Debug, Default)]
pub struct VmEvents {
    reset: AtomicBool,
    unknown_pio: AtomicU64,
    unknown_mmio: AtomicU64,
}

impl VmEvents {
    /// Гость запросил перезагрузку (i8042 0xFE).
    #[must_use]
    pub fn reset_requested(&self) -> bool {
        self.reset.load(Ordering::Acquire)
    }

    /// Число обращений к неизвестным портам.
    #[must_use]
    pub fn unknown_pio(&self) -> u64 {
        self.unknown_pio.load(Ordering::Relaxed)
    }

    /// Число обращений к неизвестным MMIO-адресам.
    #[must_use]
    pub fn unknown_mmio(&self) -> u64 {
        self.unknown_mmio.load(Ordering::Relaxed)
    }
}

/// Линия IRQ, подключённая ко входу IOAPIC.
pub struct IoApicLine {
    ioapic: Arc<Mutex<IoApic>>,
    pin: usize,
    sink: Arc<dyn InterruptSink>,
}

impl IoApicLine {
    /// Создаёт линию для входа `pin`.
    #[must_use]
    pub fn new(ioapic: Arc<Mutex<IoApic>>, pin: usize, sink: Arc<dyn InterruptSink>) -> Self {
        Self { ioapic, pin, sink }
    }
}

impl InterruptLine for IoApicLine {
    fn pulse(&self) {
        let msg = self
            .ioapic
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .edge(self.pin);
        if let Some(msg) = msg {
            self.sink.deliver(msg);
        }
    }
}

/// Шина платформы.
pub struct PlatformBus {
    ram: Arc<GuestRam>,
    serial: Arc<Mutex<Serial16550>>,
    ioapic: Arc<Mutex<IoApic>>,
    pci: Arc<PciBus>,
    events: Arc<VmEvents>,
    logged: HashSet<u64>,
}

impl std::fmt::Debug for PlatformBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformBus").finish_non_exhaustive()
    }
}

impl PlatformBus {
    /// Собирает шину.
    #[must_use]
    pub fn new(
        ram: Arc<GuestRam>,
        serial: Arc<Mutex<Serial16550>>,
        ioapic: Arc<Mutex<IoApic>>,
        pci: Arc<PciBus>,
        events: Arc<VmEvents>,
    ) -> Self {
        Self {
            ram,
            serial,
            ioapic,
            pci,
            events,
            logged: HashSet::new(),
        }
    }

    fn log_unknown(&mut self, kind: &'static str, addr: u64, write: bool) {
        if self.logged.len() < MAX_UNKNOWN_LOGGED && self.logged.insert(addr) {
            tracing::debug!(
                kind,
                addr = format_args!("0x{addr:x}"),
                write,
                "обращение к неизвестному устройству"
            );
        }
    }

    /// Чтение порта.
    pub fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        data.fill(0);
        match port {
            p if (COM1_PORT..COM1_PORT + Serial16550::PORTS).contains(&p) => {
                #[allow(clippy::cast_possible_truncation, reason = "смещение 0..8")]
                let off = (p - COM1_PORT) as u8;
                let mut serial = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(b) = data.first_mut() {
                    *b = serial.read(off);
                }
            }
            // i8042: нет данных, буферы пусты.
            I8042_DATA | I8042_COMMAND => {}
            // NMI status / POST-порт.
            0x61 | 0x80 => {}
            _ => {
                data.fill(0xFF);
                self.events.unknown_pio.fetch_add(1, Ordering::Relaxed);
                self.log_unknown("pio", u64::from(port), false);
            }
        }
    }

    /// Запись в порт.
    pub fn pio_write(&mut self, port: u16, data: &[u8]) {
        let value = data.first().copied().unwrap_or(0);
        match port {
            p if (COM1_PORT..COM1_PORT + Serial16550::PORTS).contains(&p) => {
                #[allow(clippy::cast_possible_truncation, reason = "смещение 0..8")]
                let off = (p - COM1_PORT) as u8;
                self.serial
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .write(off, value);
            }
            I8042_COMMAND if value == I8042_CMD_RESET => {
                tracing::info!("гость запросил перезагрузку (i8042)");
                self.events.reset.store(true, Ordering::Release);
            }
            I8042_DATA | I8042_COMMAND | 0x61 | 0x80 => {}
            _ => {
                self.events.unknown_pio.fetch_add(1, Ordering::Relaxed);
                self.log_unknown("pio", u64::from(port), true);
            }
        }
    }

    /// Чтение по GPA (RAM — для эмуляции строковых инструкций, или MMIO-устройство).
    pub fn mmio_read(&mut self, gpa: u64, data: &mut [u8]) {
        if self.ram.read(gpa, data).is_ok() {
            return;
        }
        if (IOAPIC_ADDR..IOAPIC_ADDR + IOAPIC_SIZE).contains(&gpa) {
            let v = self
                .ioapic
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .mmio_read(gpa - IOAPIC_ADDR);
            for (dst, src) in data.iter_mut().zip(v.to_le_bytes()) {
                *dst = src;
            }
            return;
        }
        if (ECAM_ADDR..ECAM_ADDR + ECAM_SIZE).contains(&gpa) {
            self.pci.ecam_read(gpa - ECAM_ADDR, data);
            return;
        }
        if let Some((func, off)) = self.pci.bar_lookup(gpa) {
            func.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .bar_read(off, data);
            return;
        }
        data.fill(0xFF);
        self.events.unknown_mmio.fetch_add(1, Ordering::Relaxed);
        self.log_unknown("mmio", gpa, false);
    }

    /// Запись по GPA.
    pub fn mmio_write(&mut self, gpa: u64, data: &[u8]) {
        if self.ram.write(gpa, data).is_ok() {
            return;
        }
        if (IOAPIC_ADDR..IOAPIC_ADDR + IOAPIC_SIZE).contains(&gpa) {
            let mut bytes = [0_u8; 4];
            for (dst, src) in bytes.iter_mut().zip(data) {
                *dst = *src;
            }
            self.ioapic
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .mmio_write(gpa - IOAPIC_ADDR, u32::from_le_bytes(bytes));
            return;
        }
        if (ECAM_ADDR..ECAM_ADDR + ECAM_SIZE).contains(&gpa) {
            self.pci.ecam_write(gpa - ECAM_ADDR, data);
            return;
        }
        if let Some((func, off)) = self.pci.bar_lookup(gpa) {
            // Путь без doorbell: запись в notify обрабатывается здесь же, в потоке vCPU.
            func.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .bar_write(off, data);
            return;
        }
        self.events.unknown_mmio.fetch_add(1, Ordering::Relaxed);
        self.log_unknown("mmio", gpa, true);
    }
}

#[cfg(windows)]
impl rustdroid_whpx::Bus for PlatformBus {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        Self::pio_read(self, port, data);
    }
    fn pio_write(&mut self, port: u16, data: &[u8]) {
        Self::pio_write(self, port, data);
    }
    fn mmio_read(&mut self, gpa: u64, data: &mut [u8]) {
        Self::mmio_read(self, gpa, data);
    }
    fn mmio_write(&mut self, gpa: u64, data: &[u8]) {
        Self::mmio_write(self, gpa, data);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::ioapic::ApicMessage;
    use crate::memory::AllocOptions;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ApicMessage>>, AtomicUsize);
    impl InterruptSink for Recorder {
        fn deliver(&self, msg: ApicMessage) {
            self.0.lock().unwrap().push(msg);
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Clone, Default)]
    struct Out(Arc<Mutex<Vec<u8>>>);
    impl Write for Out {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn bus() -> (
        PlatformBus,
        Out,
        Arc<Recorder>,
        Arc<Mutex<Serial16550>>,
        Arc<VmEvents>,
    ) {
        let ram = Arc::new(GuestRam::new(4 << 20, AllocOptions::default()).unwrap());
        let ioapic = Arc::new(Mutex::new(IoApic::new(1)));
        let rec = Arc::new(Recorder::default());
        let line = Arc::new(IoApicLine::new(ioapic.clone(), COM1_IRQ, rec.clone()));
        let out = Out::default();
        let serial = Arc::new(Mutex::new(Serial16550::new(line, Box::new(out.clone()))));
        let events = Arc::new(VmEvents::default());
        (
            PlatformBus::new(
                ram,
                serial.clone(),
                ioapic,
                Arc::new(PciBus::new()),
                events.clone(),
            ),
            out,
            rec,
            serial,
            events,
        )
    }

    #[test]
    fn serial_output_via_pio() {
        let (mut b, out, ..) = bus();
        for c in b"ok" {
            b.pio_write(COM1_PORT, &[*c]);
        }
        assert_eq!(&*out.0.lock().unwrap(), b"ok");
    }

    #[test]
    fn serial_input_irq_routed_through_ioapic() {
        let (mut b, _out, rec, serial, _) = bus();
        // Программируем вход 4 IOAPIC: вектор 0x24, APIC 0.
        b.mmio_write(IOAPIC_ADDR, &(0x10_u32 + 8).to_le_bytes());
        b.mmio_write(IOAPIC_ADDR + 0x10, &0x24_u32.to_le_bytes());
        b.pio_write(COM1_PORT + 1, &[0x01]); // IER: RX
        serial.lock().unwrap().enqueue_input(b"x");
        let msgs = rec.0.lock().unwrap();
        assert!(msgs.iter().any(|m| m.vector == 0x24));
    }

    #[test]
    fn masked_irq_is_not_delivered() {
        let (mut b, _out, rec, serial, _) = bus();
        b.pio_write(COM1_PORT + 1, &[0x01]);
        serial.lock().unwrap().enqueue_input(b"x");
        assert_eq!(rec.1.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn i8042_reset_sets_event() {
        let (mut b, _o, _r, _s, ev) = bus();
        assert!(!ev.reset_requested());
        b.pio_write(I8042_COMMAND, &[I8042_CMD_RESET]);
        assert!(ev.reset_requested());
    }

    #[test]
    fn unknown_port_reads_all_ones() {
        let (mut b, _o, _r, _s, ev) = bus();
        let mut d = [0_u8; 2];
        b.pio_read(0x1234, &mut d);
        assert_eq!(d, [0xFF, 0xFF]);
        assert_eq!(ev.unknown_pio(), 1);
    }

    #[test]
    fn empty_pci_bus_reads_all_ones() {
        let (mut b, ..) = bus();
        let mut d = [0_u8; 4];
        b.mmio_read(ECAM_ADDR + (3 << 15), &mut d);
        assert_eq!(d, [0xFF; 4]);
    }

    #[test]
    fn ram_access_through_mmio_path() {
        let (mut b, ..) = bus();
        b.mmio_write(0x2000, &[1, 2, 3, 4]);
        let mut d = [0_u8; 4];
        b.mmio_read(0x2000, &mut d);
        assert_eq!(d, [1, 2, 3, 4]);
    }
}
