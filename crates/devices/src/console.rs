//! virtio-console (virtio 1.x, раздел 5.3), один порт без multiport.
//!
//! Очередь 0 — receiveq (хост → гость), очередь 1 — transmitq (гость → хост).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use rustdroid_virtio::device::TYPE_CONSOLE;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

/// Индекс очереди приёма.
pub const RX_QUEUE: usize = 0;
/// Индекс очереди передачи.
pub const TX_QUEUE: usize = 1;

/// Буфер ввода для гостя (заполняется потоком консоли хоста).
#[derive(Debug, Default)]
pub struct ConsoleInput(Mutex<VecDeque<u8>>);

impl ConsoleInput {
    /// Добавляет байты ввода.
    pub fn push(&self, bytes: &[u8]) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(bytes);
    }

    /// Забирает весь накопленный ввод (для тестов).
    #[cfg(test)]
    pub(crate) fn take_all(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

/// virtio-console.
pub struct VirtioConsole {
    out: Box<dyn Write + Send>,
    input: Arc<ConsoleInput>,
    sizes: [u16; 2],
    chain: Chain,
    buf: Vec<u8>,
}

impl std::fmt::Debug for VirtioConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioConsole").finish_non_exhaustive()
    }
}

impl VirtioConsole {
    /// Консоль: вывод гостя в `out`, ввод из `input`.
    #[must_use]
    pub fn new(out: Box<dyn Write + Send>, input: Arc<ConsoleInput>) -> Self {
        Self {
            out,
            input,
            sizes: [64, 64],
            chain: Chain::default(),
            buf: Vec::new(),
        }
    }

    fn transmit(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while queue.pop(mem, &mut self.chain)? {
            for d in self.chain.descriptors.iter().filter(|d| !d.is_write()) {
                let n = d.len as usize;
                self.buf.resize(n, 0);
                if mem.read(d.addr, &mut self.buf).is_ok() {
                    let _ = self.out.write_all(&self.buf);
                }
            }
            let _ = self.out.flush();
            queue.add_used(mem, self.chain.head, 0)?;
            any = true;
        }
        Ok(any)
    }

    fn receive(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        // Буферы забираем, только если есть что отдать: вернуть буфер в avail ring нельзя.
        while !self.input.is_empty() && queue.pop(mem, &mut self.chain)? {
            let mut written = 0_u32;
            let mut input = self.input.0.lock().unwrap_or_else(PoisonError::into_inner);
            for d in self.chain.descriptors.iter().filter(|d| d.is_write()) {
                let n = (d.len as usize).min(input.len());
                if n == 0 {
                    break;
                }
                let bytes: Vec<u8> = input.drain(..n).collect();
                if mem.write(d.addr, &bytes).is_ok() {
                    #[allow(clippy::cast_possible_truncation, reason = "n ≤ длины дескриптора")]
                    {
                        written += n as u32;
                    }
                }
            }
            drop(input);
            queue.add_used(mem, self.chain.head, written)?;
            any = true;
        }
        Ok(any)
    }
}

impl VirtioDevice for VirtioConsole {
    fn device_type(&self) -> u32 {
        TYPE_CONSOLE
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn take_pending_queues(&mut self) -> u64 {
        // Служба хоста на этой консоли могла ответить прямо во время передачи гостя.
        if self.input.is_empty() {
            0
        } else {
            1 << RX_QUEUE
        }
    }
    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        match index {
            RX_QUEUE => self.receive(queue, mem),
            TX_QUEUE => self.transmit(queue, mem),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use rustdroid_virtio::mem::{GuestMemoryExt, VecMemory};
    use rustdroid_virtio::queue::VIRTQ_DESC_F_WRITE;

    use super::*;

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

    fn queue() -> Queue {
        let mut q = Queue::new(8);
        q.desc_table = 0x1000;
        q.avail_ring = 0x2000;
        q.used_ring = 0x3000;
        q.ready = true;
        q
    }

    fn one_desc(mem: &VecMemory, addr: u64, len: u32, flags: u16) {
        let mut b = [0_u8; 16];
        b[..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        mem.write(0x1000, &b).unwrap();
        mem.write_u16(0x2004, 0).unwrap();
        mem.write_u16(0x2002, 1).unwrap();
    }

    #[test]
    fn transmit_writes_to_host() {
        let mem = VecMemory::new(0x10000);
        let out = Out::default();
        let mut c = VirtioConsole::new(Box::new(out.clone()), Arc::default());
        mem.write(0x5000, b"init: started").unwrap();
        one_desc(&mem, 0x5000, 13, 0);
        let mut q = queue();
        assert!(c.process_queue(TX_QUEUE, &mut q, &mem).unwrap());
        assert_eq!(&*out.0.lock().unwrap(), b"init: started");
    }

    #[test]
    fn receive_delivers_input_only_when_available() {
        let mem = VecMemory::new(0x10000);
        let input = Arc::new(ConsoleInput::default());
        let mut c = VirtioConsole::new(Box::new(Out::default()), input.clone());
        one_desc(&mem, 0x5000, 64, VIRTQ_DESC_F_WRITE);
        let mut q = queue();
        // Ввода нет — буфер не забирается.
        assert!(!c.process_queue(RX_QUEUE, &mut q, &mem).unwrap());
        input.push(b"ls\n");
        assert!(c.process_queue(RX_QUEUE, &mut q, &mem).unwrap());
        let mut got = [0_u8; 3];
        mem.read(0x5000, &mut got).unwrap();
        assert_eq!(&got, b"ls\n");
        assert_eq!(mem.read_u32(0x3008).unwrap(), 3);
    }
}
