//! UART 16550 (COM1) на основе `vm-superio`.

use std::io::Write;
use std::sync::Arc;

use vm_superio::{Serial, Trigger};

/// Линия прерывания устройства (фронт).
pub trait InterruptLine: Send + Sync {
    /// Подать фронт на линию.
    fn pulse(&self);
}

/// Адаптер линии прерывания к `vm_superio::Trigger`.
struct LineTrigger(Arc<dyn InterruptLine>);

impl Trigger for LineTrigger {
    type E = std::convert::Infallible;

    fn trigger(&self) -> Result<(), Self::E> {
        self.0.pulse();
        Ok(())
    }
}

/// Последовательный порт 16550: восемь регистров ввода-вывода.
pub struct Serial16550 {
    inner: Serial<LineTrigger, vm_superio::serial::NoEvents, Box<dyn Write + Send>>,
}

impl std::fmt::Debug for Serial16550 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Serial16550").finish_non_exhaustive()
    }
}

impl Serial16550 {
    /// Число регистров (портов).
    pub const PORTS: u16 = 8;

    /// Создаёт порт: вывод гостя пишется в `out`, прерывания идут в `irq`.
    #[must_use]
    pub fn new(irq: Arc<dyn InterruptLine>, out: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Serial::new(LineTrigger(irq), out),
        }
    }

    /// Чтение регистра `offset` (0..8).
    pub fn read(&mut self, offset: u8) -> u8 {
        self.inner.read(offset)
    }

    /// Запись регистра `offset` (0..8).
    pub fn write(&mut self, offset: u8, value: u8) {
        if let Err(e) = self.inner.write(offset, value) {
            tracing::trace!(error = ?e, "ошибка записи вывода UART");
        }
    }

    /// Отдаёт гостю принятые байты (ввод с консоли хоста). Возвращает, сколько поместилось в FIFO.
    pub fn enqueue_input(&mut self, bytes: &[u8]) -> usize {
        self.inner.enqueue_raw_bytes(bytes).unwrap_or(0)
    }

    /// Свободное место во входном FIFO.
    #[must_use]
    pub fn input_capacity(&self) -> usize {
        self.inner.fifo_capacity()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct Counter(AtomicUsize);
    impl InterruptLine for Counter {
        fn pulse(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_reaches_sink() {
        let sink = Sink::default();
        let mut s = Serial16550::new(Arc::new(Counter::default()), Box::new(sink.clone()));
        for b in b"hi\n" {
            s.write(0, *b);
        }
        assert_eq!(&*sink.0.lock().unwrap(), b"hi\n");
    }

    #[test]
    fn input_raises_interrupt_when_enabled() {
        let irq = Arc::new(Counter::default());
        let mut s = Serial16550::new(irq.clone(), Box::new(Sink::default()));
        s.write(1, 0x01); // IER: прерывание по приёму данных
        assert_eq!(s.enqueue_input(b"ls\r"), 3);
        assert!(irq.0.load(Ordering::Relaxed) >= 1);
        // LSR: данные готовы
        assert_ne!(s.read(5) & 1, 0);
        assert_eq!(s.read(0), b'l');
    }
}
