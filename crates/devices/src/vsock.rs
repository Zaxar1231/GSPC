//! virtio-vsock (virtio 1.x, раздел 5.10): служебный канал хост ↔ гость.
//!
//! Очередь 0 — receiveq (хост → гость), 1 — transmitq (гость → хост), 2 — eventq.
//!
//! Сейчас хост ведёт себя как «чёрная дыра»: принимает любое подключение гостя, читает данные
//! и ничего не отвечает, соединение закрывает по SHUTDOWN. Этого достаточно HAL-службам
//! Cuttlefish: серверам в госте (подсветка) нужно само устройство, клиентам (модем RIL) —
//! установленное соединение, иначе они завершаются и init перезапускает zygote. Настоящие
//! сервисы хоста (гостевой агент, установка APK, ADB) появятся на этапе 5 поверх этого
//! устройства.

use std::collections::{HashMap, VecDeque};

use rustdroid_virtio::device::TYPE_VSOCK;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

/// Индекс очереди приёма.
pub const RX_QUEUE: usize = 0;
/// Индекс очереди передачи.
pub const TX_QUEUE: usize = 1;
/// Индекс очереди событий.
pub const EVENT_QUEUE: usize = 2;

/// CID хоста.
pub const HOST_CID: u64 = 2;
/// Размер заголовка пакета.
pub const HEADER_LEN: usize = 44;

/// Тип сокета: поток.
const TYPE_STREAM: u16 = 1;

/// Операции.
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

/// Размер буфера приёма хоста, который объявляется гостю.
const HOST_BUF_ALLOC: u32 = 256 * 1024;
/// Предел очереди ответов: защита от гостя, который шлёт запросы, не забирая ответы.
const MAX_PENDING: usize = 4096;

/// Заголовок пакета virtio-vsock (little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// CID отправителя.
    pub src_cid: u64,
    /// CID получателя.
    pub dst_cid: u64,
    /// Порт отправителя.
    pub src_port: u32,
    /// Порт получателя.
    pub dst_port: u32,
    /// Длина данных после заголовка.
    pub len: u32,
    /// Тип сокета.
    pub kind: u16,
    /// Операция.
    pub op: u16,
    /// Флаги (для SHUTDOWN).
    pub flags: u32,
    /// Размер буфера приёма отправителя.
    pub buf_alloc: u32,
    /// Сколько байт отправитель уже прочитал.
    pub fwd_cnt: u32,
}

impl Header {
    /// Разбор 44 байт заголовка.
    #[must_use]
    pub fn parse(b: &[u8; HEADER_LEN]) -> Self {
        let u64_at = |o: usize| {
            u64::from_le_bytes([
                b[o],
                b[o + 1],
                b[o + 2],
                b[o + 3],
                b[o + 4],
                b[o + 5],
                b[o + 6],
                b[o + 7],
            ])
        };
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Self {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            kind: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    /// Запись в 44 байта.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0_u8; HEADER_LEN];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.kind.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

/// Ключ соединения: (порт гостя, порт хоста).
type ConnKey = (u32, u32);

#[derive(Debug, Default, Clone, Copy)]
struct Conn {
    /// Сколько байт хост «прочитал».
    fwd_cnt: u32,
    /// Значение `fwd_cnt`, которое гость уже знает.
    reported: u32,
}

/// virtio-vsock.
#[derive(Debug)]
pub struct VirtioVsock {
    guest_cid: u64,
    sizes: [u16; 3],
    conns: HashMap<ConnKey, Conn>,
    pending: VecDeque<Header>,
    pending_queues: u64,
    chain: Chain,
}

impl VirtioVsock {
    /// Устройство с CID гостя `guest_cid` (обычно 3).
    #[must_use]
    pub fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            sizes: [128, 128, 16],
            conns: HashMap::new(),
            pending: VecDeque::new(),
            pending_queues: 0,
            chain: Chain::default(),
        }
    }

    /// Ответ гостю на пакет `req` с операцией `op`.
    fn reply(&mut self, req: &Header, op: u16, fwd_cnt: u32) {
        if self.pending.len() >= MAX_PENDING {
            tracing::warn!("vsock: очередь ответов переполнена, пакет отброшен");
            return;
        }
        self.pending.push_back(Header {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: req.dst_port,
            dst_port: req.src_port,
            len: 0,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: HOST_BUF_ALLOC,
            fwd_cnt,
        });
        self.pending_queues |= 1 << RX_QUEUE;
    }

    /// Обработка пакета гостя.
    fn handle(&mut self, h: &Header) {
        let key = (h.src_port, h.dst_port);
        if h.dst_cid != HOST_CID || h.kind != TYPE_STREAM {
            if h.op != OP_RST {
                self.reply(h, OP_RST, 0);
            }
            return;
        }
        match h.op {
            OP_REQUEST => {
                tracing::debug!(
                    guest_port = h.src_port,
                    host_port = h.dst_port,
                    "vsock: подключение гостя"
                );
                self.conns.insert(key, Conn::default());
                self.reply(h, OP_RESPONSE, 0);
            }
            OP_RW => {
                let Some(c) = self.conns.get_mut(&key) else {
                    self.reply(h, OP_RST, 0);
                    return;
                };
                c.fwd_cnt = c.fwd_cnt.wrapping_add(h.len);
                // Освобождаем кредит гостю, когда «прочитали» половину буфера.
                if c.fwd_cnt.wrapping_sub(c.reported) >= HOST_BUF_ALLOC / 2 {
                    c.reported = c.fwd_cnt;
                    let fwd = c.fwd_cnt;
                    self.reply(h, OP_CREDIT_UPDATE, fwd);
                }
            }
            OP_CREDIT_REQUEST => {
                if let Some(c) = self.conns.get_mut(&key) {
                    c.reported = c.fwd_cnt;
                    let fwd = c.fwd_cnt;
                    self.reply(h, OP_CREDIT_UPDATE, fwd);
                } else {
                    self.reply(h, OP_RST, 0);
                }
            }
            OP_SHUTDOWN => {
                self.conns.remove(&key);
                self.reply(h, OP_RST, 0);
            }
            OP_RST => {
                self.conns.remove(&key);
            }
            OP_CREDIT_UPDATE => {}
            // RESPONSE на несуществующий запрос хоста и неизвестные операции.
            _ => {
                self.conns.remove(&key);
                self.reply(h, OP_RST, 0);
            }
        }
    }

    fn transmit(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while queue.pop(mem, &mut self.chain)? {
            // Заголовок может быть разбит между дескрипторами.
            let mut hdr = [0_u8; HEADER_LEN];
            let mut got = 0_usize;
            for d in self.chain.descriptors.iter().filter(|d| !d.is_write()) {
                if got == HEADER_LEN {
                    break;
                }
                let n = (d.len as usize).min(HEADER_LEN - got);
                if let Some(dst) = hdr.get_mut(got..got + n)
                    && mem.read(d.addr, dst).is_ok()
                {
                    got += n;
                } else {
                    break;
                }
            }
            if got == HEADER_LEN {
                let h = Header::parse(&hdr);
                self.handle(&h);
            }
            queue.add_used(mem, self.chain.head, 0)?;
            any = true;
        }
        Ok(any)
    }

    fn receive(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while !self.pending.is_empty() && queue.pop(mem, &mut self.chain)? {
            let Some(h) = self.pending.pop_front() else {
                break;
            };
            let bytes = h.to_bytes();
            let mut written = 0_usize;
            for d in self.chain.descriptors.iter().filter(|d| d.is_write()) {
                if written == HEADER_LEN {
                    break;
                }
                let n = (d.len as usize).min(HEADER_LEN - written);
                if let Some(src) = bytes.get(written..written + n)
                    && mem.write(d.addr, src).is_ok()
                {
                    written += n;
                } else {
                    break;
                }
            }
            #[allow(clippy::cast_possible_truncation, reason = "не больше 44")]
            queue.add_used(mem, self.chain.head, written as u32)?;
            any = true;
        }
        Ok(any)
    }
}

impl VirtioDevice for VirtioVsock {
    fn device_type(&self) -> u32 {
        TYPE_VSOCK
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.guest_cid.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = usize::try_from(offset)
                .ok()
                .and_then(|o| cfg.get(o + i))
                .copied()
                .unwrap_or(0);
        }
    }
    fn reset(&mut self) {
        self.conns.clear();
        self.pending.clear();
        self.pending_queues = 0;
    }
    fn take_pending_queues(&mut self) -> u64 {
        std::mem::take(&mut self.pending_queues)
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

    fn queue(base: u64) -> Queue {
        let mut q = Queue::new(8);
        q.desc_table = base;
        q.avail_ring = base + 0x1000;
        q.used_ring = base + 0x2000;
        q.ready = true;
        q
    }

    /// Кладёт в очередь `q` цепочку из одного дескриптора.
    fn post(mem: &VecMemory, q: &Queue, slot: u16, addr: u64, len: u32, flags: u16) {
        let mut b = [0_u8; 16];
        b[..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        mem.write(q.desc_table + u64::from(slot) * 16, &b).unwrap();
        mem.write_u16(q.avail_ring + 4 + u64::from(slot) * 2, slot)
            .unwrap();
        mem.write_u16(q.avail_ring + 2, slot + 1).unwrap();
    }

    fn guest_packet(op: u16, len: u32) -> Header {
        Header {
            src_cid: 3,
            dst_cid: HOST_CID,
            src_port: 50_000,
            dst_port: 9600,
            len,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 65_536,
            fwd_cnt: 0,
        }
    }

    #[test]
    fn header_roundtrip() {
        let h = guest_packet(OP_RW, 123);
        assert_eq!(Header::parse(&h.to_bytes()), h);
    }

    #[test]
    fn connection_is_accepted_and_reset_on_shutdown() {
        let mem = VecMemory::new(0x20000);
        let mut dev = VirtioVsock::new(3);
        let mut tx = queue(0x1000);
        let mut rx = queue(0x8000);

        // Гость: REQUEST → хост: RESPONSE.
        mem.write(0x10000, &guest_packet(OP_REQUEST, 0).to_bytes())
            .unwrap();
        post(&mem, &tx, 0, 0x10000, HEADER_LEN as u32, 0);
        assert!(dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap());
        assert_eq!(dev.take_pending_queues(), 1 << RX_QUEUE);
        post(&mem, &rx, 0, 0x11000, 64, VIRTQ_DESC_F_WRITE);
        assert!(dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
        let mut b = [0_u8; HEADER_LEN];
        mem.read(0x11000, &mut b).unwrap();
        let resp = Header::parse(&b);
        assert_eq!(resp.op, OP_RESPONSE);
        assert_eq!((resp.src_cid, resp.dst_cid), (HOST_CID, 3));
        assert_eq!((resp.src_port, resp.dst_port), (9600, 50_000));
        assert_eq!(resp.buf_alloc, HOST_BUF_ALLOC);

        // Данные «читаются» без ответа.
        let mut rw = guest_packet(OP_RW, 100).to_bytes().to_vec();
        rw.extend_from_slice(&[0xAA; 100]);
        mem.write(0x12000, &rw).unwrap();
        post(&mem, &tx, 1, 0x12000, rw.len() as u32, 0);
        dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap();
        assert_eq!(dev.take_pending_queues(), 0);
        assert_eq!(dev.conns.get(&(50_000, 9600)).unwrap().fwd_cnt, 100);

        // SHUTDOWN → RST.
        mem.write(0x13000, &guest_packet(OP_SHUTDOWN, 0).to_bytes())
            .unwrap();
        post(&mem, &tx, 2, 0x13000, HEADER_LEN as u32, 0);
        dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap();
        post(&mem, &rx, 1, 0x14000, 64, VIRTQ_DESC_F_WRITE);
        dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap();
        mem.read(0x14000, &mut b).unwrap();
        assert_eq!(Header::parse(&b).op, OP_RST);
        assert!(dev.conns.is_empty());
    }

    #[test]
    fn credit_request_and_unknown_connection() {
        let mut dev = VirtioVsock::new(3);
        dev.handle(&guest_packet(OP_RW, 10));
        assert_eq!(dev.pending.pop_front().unwrap().op, OP_RST);
        dev.handle(&guest_packet(OP_REQUEST, 0));
        dev.pending.clear();
        dev.handle(&guest_packet(OP_RW, HOST_BUF_ALLOC / 2));
        let upd = dev.pending.pop_front().unwrap();
        assert_eq!(
            (upd.op, upd.fwd_cnt),
            (OP_CREDIT_UPDATE, HOST_BUF_ALLOC / 2)
        );
        dev.handle(&guest_packet(OP_CREDIT_REQUEST, 0));
        assert_eq!(dev.pending.pop_front().unwrap().op, OP_CREDIT_UPDATE);
        // Пакет не хосту — RST.
        let mut other = guest_packet(OP_REQUEST, 0);
        other.dst_cid = 7;
        dev.handle(&other);
        assert_eq!(dev.pending.pop_front().unwrap().op, OP_RST);
    }

    #[test]
    fn config_exposes_guest_cid() {
        let dev = VirtioVsock::new(3);
        let mut b = [0_u8; 8];
        dev.read_config(0, &mut b);
        assert_eq!(u64::from_le_bytes(b), 3);
        let mut hi = [0xFF_u8; 4];
        dev.read_config(4, &mut hi);
        assert_eq!(hi, [0; 4]);
    }
}
