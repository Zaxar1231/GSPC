//! virtio-vsock (virtio 1.x, раздел 5.10): служебный канал хост ↔ гость.
//!
//! Очередь 0 — receiveq (хост → гость), 1 — transmitq (гость → хост), 2 — eventq.
//!
//! Подключения к портам, на которых зарегистрирована служба ([`VsockService`]), обслуживает
//! она: данные гостя — в `on_data`, её ответы уходят гостю с учётом его кредита (`buf_alloc` и
//! `fwd_cnt`), а [`VsockOutbox`] позволяет слать данные из любого потока (например, APK для
//! установки). Ко всем остальным портам хост ведёт себя как «чёрная дыра»: принимает любое
//! подключение гостя, читает данные и ничего не отвечает, соединение закрывает по SHUTDOWN. Этого
//! достаточно HAL-службам Cuttlefish: серверам в госте (подсветка) нужно само устройство,
//! клиентам (модем RIL) — установленное соединение, иначе они завершаются и init перезапускает
//! zygote.

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

/// Самый большой кусок данных в одном пакете к гостю: буферы приёма гостя — 4 КиБ с заголовком.
const MAX_PAYLOAD: usize = 4000;
/// Самый длинный пакет от гостя, который читаем целиком.
const MAX_TX_PAYLOAD: usize = 64 * 1024;
/// Сколько байт может ждать отправки гостю на одно соединение службы (защита памяти хоста).
const MAX_OUT: usize = 512 * 1024 * 1024;

/// Служба хоста на порту vsock: получает поток байт от гостя и отвечает.
///
/// Подключения гостя к зарегистрированному порту обслуживает служба; к остальным портам хост
/// ведёт себя как «чёрная дыра» (см. описание модуля).
pub trait VsockService: Send + std::fmt::Debug {
    /// Гость открыл соединение с портом `guest_port`; возвращает данные, которые хост шлёт
    /// сразу.
    fn on_open(&mut self, _guest_port: u32) -> Vec<u8> {
        Vec::new()
    }
    /// Данные от гостя; возвращает ответ (может быть пустым).
    fn on_data(&mut self, guest_port: u32, data: &[u8]) -> Vec<u8>;
    /// Соединение закрыто.
    fn on_close(&mut self, _guest_port: u32) {}
}

/// Исходящие данные хоста для служб: из любого потока можно отправить байты в соединение,
/// открытое гостем к порту хоста.
#[derive(Default)]
pub struct VsockOutbox {
    queue: std::sync::Mutex<Vec<(u32, Vec<u8>)>>,
    waker: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for VsockOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VsockOutbox").finish_non_exhaustive()
    }
}

impl VsockOutbox {
    /// Пустой ящик.
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Задаёт функцию, будящую очередь приёма устройства.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(waker));
    }

    /// Отправляет `data` в открытое гостем соединение с портом хоста `host_port`. Если
    /// соединения нет, данные теряются.
    pub fn send(&self, host_port: u32, data: Vec<u8>) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((host_port, data));
        if let Some(w) = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            w();
        }
    }

    pub(crate) fn take(&self) -> Vec<(u32, Vec<u8>)> {
        std::mem::take(
            &mut *self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Пакет к гостю: заголовок и данные (только у `OP_RW`).
#[derive(Debug)]
struct Packet {
    hdr: Header,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct Conn {
    /// Сколько байт хост «прочитал» от гостя.
    fwd_cnt: u32,
    /// Значение `fwd_cnt`, которое гость уже знает.
    reported: u32,
    /// Подключение к порту, который обслуживает служба.
    service: bool,
    /// Размер буфера приёма гостя (из его заголовков).
    peer_buf_alloc: u32,
    /// Сколько байт гость уже прочитал (из его заголовков).
    peer_fwd_cnt: u32,
    /// Сколько байт хост отправил гостю.
    sent: u32,
    /// Данные службы, ждущие кредита гостя.
    out: VecDeque<u8>,
}

impl Conn {
    /// Сколько байт можно отправить гостю сейчас.
    fn credit(&self) -> usize {
        let in_flight = self.sent.wrapping_sub(self.peer_fwd_cnt);
        usize::try_from(self.peer_buf_alloc.saturating_sub(in_flight)).unwrap_or(0)
    }
}

/// virtio-vsock.
#[derive(Debug)]
pub struct VirtioVsock {
    guest_cid: u64,
    sizes: [u16; 3],
    conns: HashMap<ConnKey, Conn>,
    pending: VecDeque<Packet>,
    pending_queues: u64,
    chain: Chain,
    services: HashMap<u32, Box<dyn VsockService>>,
    outbox: Option<std::sync::Arc<VsockOutbox>>,
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
            services: HashMap::new(),
            outbox: None,
        }
    }

    /// Подключает службы хоста по портам и ящик исходящих данных для них.
    #[must_use]
    pub fn with_services(
        mut self,
        services: Vec<(u32, Box<dyn VsockService>)>,
        outbox: std::sync::Arc<VsockOutbox>,
    ) -> Self {
        self.services = services.into_iter().collect();
        self.outbox = Some(outbox);
        self
    }

    fn push(&mut self, hdr: Header, data: Vec<u8>) {
        if self.pending.len() >= MAX_PENDING {
            tracing::warn!("vsock: очередь ответов переполнена, пакет отброшен");
            return;
        }
        self.pending.push_back(Packet { hdr, data });
        self.pending_queues |= 1 << RX_QUEUE;
    }

    /// Заготовка пакета хоста гостю по соединению `key`.
    fn host_header(&self, key: ConnKey, op: u16, len: usize, fwd_cnt: u32) -> Header {
        Header {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: key.1,
            dst_port: key.0,
            #[allow(clippy::cast_possible_truncation, reason = "len ≤ MAX_PAYLOAD")]
            len: len as u32,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: HOST_BUF_ALLOC,
            fwd_cnt,
        }
    }

    /// Ответ гостю на пакет `req` с операцией `op`.
    fn reply(&mut self, req: &Header, op: u16, fwd_cnt: u32) {
        let key = (req.src_port, req.dst_port);
        let hdr = self.host_header(key, op, 0, fwd_cnt);
        self.push(hdr, Vec::new());
    }

    /// Отправляет гостю накопленные данные службы, пока хватает его кредита.
    fn flush(&mut self, key: ConnKey) {
        loop {
            let Some(c) = self.conns.get_mut(&key) else {
                return;
            };
            let n = c.out.len().min(c.credit()).min(MAX_PAYLOAD);
            if n == 0 || self.pending.len() >= MAX_PENDING {
                return;
            }
            let data: Vec<u8> = c.out.drain(..n).collect();
            #[allow(clippy::cast_possible_truncation, reason = "n ≤ MAX_PAYLOAD")]
            {
                c.sent = c.sent.wrapping_add(n as u32);
            }
            let fwd = c.fwd_cnt;
            c.reported = fwd;
            let hdr = self.host_header(key, OP_RW, n, fwd);
            self.push(hdr, data);
        }
    }

    /// Складывает данные службы в очередь соединения и отправляет, что можно.
    fn queue_out(&mut self, key: ConnKey, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        if let Some(c) = self.conns.get_mut(&key) {
            if c.out.len() + data.len() > MAX_OUT {
                tracing::warn!(?key, "vsock: очередь службы переполнена, данные отброшены");
                return;
            }
            c.out.extend(data);
        }
        self.flush(key);
    }

    /// Забирает исходящие данные служб из ящика.
    fn drain_outbox(&mut self) {
        let Some(outbox) = self.outbox.clone() else {
            return;
        };
        for (port, data) in outbox.take() {
            let key = self
                .conns
                .iter()
                .filter(|(k, c)| k.1 == port && c.service)
                .map(|(k, _)| *k)
                .min();
            match key {
                Some(k) => self.queue_out(k, data),
                None => tracing::debug!(port, "vsock: нет соединения гостя, данные отброшены"),
            }
        }
    }

    /// Обработка пакета гостя; `data` — его данные (для `OP_RW`).
    fn handle(&mut self, h: &Header, data: &[u8]) {
        let key = (h.src_port, h.dst_port);
        if h.dst_cid != HOST_CID || h.kind != TYPE_STREAM {
            if h.op != OP_RST {
                self.reply(h, OP_RST, 0);
            }
            return;
        }
        // Кредит гостя обновляется каждым его пакетом.
        if let Some(c) = self.conns.get_mut(&key) {
            c.peer_buf_alloc = h.buf_alloc;
            c.peer_fwd_cnt = h.fwd_cnt;
        }
        match h.op {
            OP_REQUEST => {
                tracing::debug!(
                    guest_port = h.src_port,
                    host_port = h.dst_port,
                    "vsock: подключение гостя"
                );
                let service = self.services.contains_key(&h.dst_port);
                self.conns.insert(
                    key,
                    Conn {
                        service,
                        peer_buf_alloc: h.buf_alloc,
                        peer_fwd_cnt: h.fwd_cnt,
                        ..Conn::default()
                    },
                );
                self.reply(h, OP_RESPONSE, 0);
                if let Some(svc) = self.services.get_mut(&h.dst_port) {
                    let first = svc.on_open(h.src_port);
                    self.queue_out(key, first);
                }
            }
            OP_RW => {
                let Some(c) = self.conns.get_mut(&key) else {
                    self.reply(h, OP_RST, 0);
                    return;
                };
                c.fwd_cnt = c.fwd_cnt.wrapping_add(h.len);
                let service = c.service;
                // Освобождаем кредит гостю, когда «прочитали» половину буфера.
                let credit =
                    (c.fwd_cnt.wrapping_sub(c.reported) >= HOST_BUF_ALLOC / 2).then(|| {
                        c.reported = c.fwd_cnt;
                        c.fwd_cnt
                    });
                if let Some(fwd) = credit {
                    self.reply(h, OP_CREDIT_UPDATE, fwd);
                }
                if service && let Some(svc) = self.services.get_mut(&h.dst_port) {
                    let answer = svc.on_data(h.src_port, data);
                    self.queue_out(key, answer);
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
                self.close(key);
                self.reply(h, OP_RST, 0);
            }
            OP_RST => self.close(key),
            // Гость освободил место в буфере: можно слать накопленное.
            OP_CREDIT_UPDATE => self.flush(key),
            // RESPONSE на несуществующий запрос хоста и неизвестные операции.
            _ => {
                self.close(key);
                self.reply(h, OP_RST, 0);
            }
        }
    }

    fn close(&mut self, key: ConnKey) {
        if let Some(c) = self.conns.remove(&key)
            && c.service
            && let Some(svc) = self.services.get_mut(&key.1)
        {
            svc.on_close(key.0);
        }
    }

    fn transmit(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while queue.pop(mem, &mut self.chain)? {
            // Заголовок и данные могут быть разбиты между дескрипторами.
            let mut buf = vec![0_u8; HEADER_LEN];
            let mut got = 0_usize;
            let mut want = HEADER_LEN;
            for d in self.chain.descriptors.iter().filter(|d| !d.is_write()) {
                if got == want {
                    break;
                }
                let n = (d.len as usize).min(want - got);
                if let Some(dst) = buf.get_mut(got..got + n)
                    && mem.read(d.addr, dst).is_ok()
                {
                    got += n;
                } else {
                    break;
                }
                // Заголовок прочитан целиком: узнаём, сколько данных ждать.
                if want == HEADER_LEN && got == HEADER_LEN {
                    let mut hb = [0_u8; HEADER_LEN];
                    hb.copy_from_slice(buf.get(..HEADER_LEN).unwrap_or(&[0; HEADER_LEN]));
                    let len = (Header::parse(&hb).len as usize).min(MAX_TX_PAYLOAD);
                    want = HEADER_LEN + len;
                    buf.resize(want, 0);
                    // Остаток текущего дескриптора относится к данным.
                    let used = n;
                    let left = (d.len as usize).saturating_sub(used);
                    if left > 0 && len > 0 {
                        let m = left.min(len);
                        if let Some(dst) = buf.get_mut(got..got + m)
                            && mem.read(d.addr + used as u64, dst).is_ok()
                        {
                            got += m;
                        }
                    }
                }
            }
            if got >= HEADER_LEN {
                let mut hb = [0_u8; HEADER_LEN];
                hb.copy_from_slice(buf.get(..HEADER_LEN).unwrap_or(&[0; HEADER_LEN]));
                let h = Header::parse(&hb);
                let data = buf.get(HEADER_LEN..got).unwrap_or(&[]);
                self.handle(&h, data);
            }
            queue.add_used(mem, self.chain.head, 0)?;
            any = true;
        }
        Ok(any)
    }

    fn receive(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        self.drain_outbox();
        let keys: Vec<ConnKey> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.out.is_empty())
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.flush(k);
        }
        let mut any = false;
        while !self.pending.is_empty() && queue.pop(mem, &mut self.chain)? {
            let Some(mut p) = self.pending.pop_front() else {
                break;
            };
            // Больше буфера гостя писать нельзя: новые ядра дают буферы меньше 4 КБ (заголовок
            // + ~3,7 КБ), обрезанный пакет гость читает как нули. Остаток — следующим пакетом.
            let cap: usize = self
                .chain
                .descriptors
                .iter()
                .filter(|d| d.is_write())
                .map(|d| d.len as usize)
                .sum();
            let room = cap.saturating_sub(HEADER_LEN);
            if p.data.len() > room && room > 0 {
                let rest = p.data.split_off(room);
                let mut hdr = p.hdr;
                #[allow(clippy::cast_possible_truncation, reason = "остаток ≤ MAX_PAYLOAD")]
                {
                    hdr.len = rest.len() as u32;
                    p.hdr.len = room as u32;
                }
                self.pending.push_front(Packet { hdr, data: rest });
            }
            let mut bytes = p.hdr.to_bytes().to_vec();
            bytes.extend_from_slice(&p.data);
            let mut written = 0_usize;
            for d in self.chain.descriptors.iter().filter(|d| d.is_write()) {
                if written == bytes.len() {
                    break;
                }
                let n = (d.len as usize).min(bytes.len() - written);
                if let Some(src) = bytes.get(written..written + n)
                    && mem.write(d.addr, src).is_ok()
                {
                    written += n;
                } else {
                    break;
                }
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "не больше заголовка и MAX_PAYLOAD"
            )]
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
        dev.handle(&guest_packet(OP_RW, 10), &[]);
        assert_eq!(dev.pending.pop_front().unwrap().hdr.op, OP_RST);
        dev.handle(&guest_packet(OP_REQUEST, 0), &[]);
        dev.pending.clear();
        dev.handle(&guest_packet(OP_RW, HOST_BUF_ALLOC / 2), &[]);
        let upd = dev.pending.pop_front().unwrap();
        assert_eq!(
            (upd.hdr.op, upd.hdr.fwd_cnt),
            (OP_CREDIT_UPDATE, HOST_BUF_ALLOC / 2)
        );
        dev.handle(&guest_packet(OP_CREDIT_REQUEST, 0), &[]);
        assert_eq!(dev.pending.pop_front().unwrap().hdr.op, OP_CREDIT_UPDATE);
        // Пакет не хосту — RST.
        let mut other = guest_packet(OP_REQUEST, 0);
        other.dst_cid = 7;
        dev.handle(&other, &[]);
        assert_eq!(dev.pending.pop_front().unwrap().hdr.op, OP_RST);
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

    #[derive(Debug, Default)]
    struct Echo {
        opened: Vec<u32>,
        closed: Vec<u32>,
        greeting: Vec<u8>,
        reply_len: usize,
    }

    impl VsockService for Echo {
        fn on_open(&mut self, guest_port: u32) -> Vec<u8> {
            self.opened.push(guest_port);
            self.greeting.clone()
        }
        fn on_data(&mut self, _guest_port: u32, data: &[u8]) -> Vec<u8> {
            if self.reply_len > 0 {
                vec![b'r'; self.reply_len]
            } else {
                data.to_vec()
            }
        }
        fn on_close(&mut self, guest_port: u32) {
            self.closed.push(guest_port);
        }
    }

    fn with_service(echo: Echo) -> (VirtioVsock, std::sync::Arc<VsockOutbox>) {
        let outbox = VsockOutbox::new();
        let dev = VirtioVsock::new(3)
            .with_services(vec![(9600, Box::new(echo))], std::sync::Arc::clone(&outbox));
        (dev, outbox)
    }

    fn drain(dev: &mut VirtioVsock) -> Vec<(u16, Vec<u8>)> {
        dev.pending.drain(..).map(|p| (p.hdr.op, p.data)).collect()
    }

    #[test]
    fn service_gets_data_and_answers() {
        let (mut dev, _) = with_service(Echo {
            greeting: b"hello".to_vec(),
            ..Echo::default()
        });
        dev.handle(&guest_packet(OP_REQUEST, 0), &[]);
        // RESPONSE, затем приветствие службы.
        assert_eq!(
            drain(&mut dev),
            vec![(OP_RESPONSE, vec![]), (OP_RW, b"hello".to_vec())]
        );
        dev.handle(&guest_packet(OP_RW, 4), b"ping");
        assert_eq!(drain(&mut dev), vec![(OP_RW, b"ping".to_vec())]);
        dev.handle(&guest_packet(OP_SHUTDOWN, 0), &[]);
        assert_eq!(drain(&mut dev), vec![(OP_RST, vec![])]);
        assert!(dev.conns.is_empty());
    }

    #[test]
    fn other_ports_stay_black_holes() {
        let (mut dev, _) = with_service(Echo::default());
        let mut h = guest_packet(OP_REQUEST, 0);
        h.dst_port = 1234;
        dev.handle(&h, &[]);
        assert_eq!(drain(&mut dev), vec![(OP_RESPONSE, vec![])]);
        h.op = OP_RW;
        h.len = 3;
        dev.handle(&h, b"abc");
        assert!(drain(&mut dev).is_empty());
    }

    #[test]
    fn host_data_respects_guest_credit() {
        let (mut dev, _) = with_service(Echo {
            reply_len: 250,
            ..Echo::default()
        });
        // Гость объявил буфер приёма в 100 байт.
        let mut req = guest_packet(OP_REQUEST, 0);
        req.buf_alloc = 100;
        dev.handle(&req, &[]);
        drain(&mut dev);
        let mut rw = guest_packet(OP_RW, 1);
        rw.buf_alloc = 100;
        dev.handle(&rw, b"x");
        let sent: usize = drain(&mut dev).iter().map(|(_, d)| d.len()).sum();
        assert_eq!(sent, 100, "больше кредита гостя не уходит");
        // Гость прочитал 100 байт: уходят следующие 100.
        let mut upd = guest_packet(OP_CREDIT_UPDATE, 0);
        upd.buf_alloc = 100;
        upd.fwd_cnt = 100;
        dev.handle(&upd, &[]);
        let sent: usize = drain(&mut dev).iter().map(|(_, d)| d.len()).sum();
        assert_eq!(sent, 100);
        upd.fwd_cnt = 200;
        dev.handle(&upd, &[]);
        let sent: usize = drain(&mut dev).iter().map(|(_, d)| d.len()).sum();
        assert_eq!(sent, 50, "остаток");
        assert!(dev.conns.values().all(|c| c.out.is_empty()));
    }

    #[test]
    fn outbox_delivers_to_open_connection_only() {
        let (mut dev, outbox) = with_service(Echo::default());
        // Соединения ещё нет: данные теряются.
        outbox.send(9600, b"lost".to_vec());
        dev.drain_outbox();
        assert!(drain(&mut dev).is_empty());
        dev.handle(&guest_packet(OP_REQUEST, 0), &[]);
        drain(&mut dev);
        outbox.send(9600, b"push".to_vec());
        outbox.send(7, b"nobody".to_vec());
        dev.drain_outbox();
        assert_eq!(drain(&mut dev), vec![(OP_RW, b"push".to_vec())]);
    }

    #[test]
    fn packet_larger_than_guest_buffer_is_split() {
        let mem = VecMemory::new(0x20000);
        let mut dev = VirtioVsock::new(3);
        let mut rx = queue(0x8000);
        let data: Vec<u8> = (0..200_u8).collect();
        dev.pending.push_back(Packet {
            hdr: guest_packet(OP_RW, 200),
            data: data.clone(),
        });
        // Буферы гостя по 100 байт: в каждый — заголовок и 56 байт данных.
        for slot in 0..4_u16 {
            post(
                &mem,
                &rx,
                slot,
                0x11000 + u64::from(slot) * 0x100,
                100,
                VIRTQ_DESC_F_WRITE,
            );
        }
        dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap();
        let mut got = Vec::new();
        for slot in 0..4_u64 {
            let mut b = [0_u8; HEADER_LEN];
            mem.read(0x11000 + slot * 0x100, &mut b).unwrap();
            let h = Header::parse(&b);
            assert!(HEADER_LEN + h.len as usize <= 100);
            let mut d = vec![0_u8; h.len as usize];
            mem.read(0x11000 + slot * 0x100 + HEADER_LEN as u64, &mut d)
                .unwrap();
            got.extend(d);
        }
        assert_eq!(got, data);
        assert!(dev.pending.is_empty());
    }

    #[test]
    fn agent_install_frames_arrive_intact() {
        use rustdroid_agent::proto::{Decoder, Message, PORT, VERSION, encode};
        let outbox = VsockOutbox::new();
        let (svc, host) = crate::agent::new(std::sync::Arc::clone(&outbox));
        let mut dev = VirtioVsock::new(3).with_services(
            vec![(PORT, Box::new(svc) as Box<dyn VsockService>)],
            std::sync::Arc::clone(&outbox),
        );
        let mut req = guest_packet(OP_REQUEST, 0);
        req.dst_port = PORT;
        req.buf_alloc = 256 * 1024;
        dev.handle(&req, &[]);
        let hello = encode(&Message::Hello { version: VERSION });
        let mut rw = guest_packet(OP_RW, u32::try_from(hello.len()).unwrap());
        rw.dst_port = PORT;
        rw.buf_alloc = 256 * 1024;
        dev.handle(&rw, &hello);
        let apk: Vec<u8> = (0..33_000_u32).map(|i| (i % 251) as u8).collect();
        let _rx = host.install("a.apk", &apk).unwrap();
        dev.drain_outbox();

        let mut dec = Decoder::default();
        let mut got = Vec::new();
        for p in dev.pending.drain(..).filter(|p| p.hdr.op == OP_RW) {
            assert_eq!(p.data.len(), p.hdr.len as usize);
            assert!(p.data.len() <= MAX_PAYLOAD);
            got.extend(dec.feed(&p.data).unwrap());
        }
        let data: Vec<u8> = got
            .iter()
            .filter_map(|m| match m {
                Message::InstallChunk { data, .. } => Some(data.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(data, apk);
        assert!(matches!(got.last(), Some(Message::InstallEnd { .. })));
    }

    #[test]
    fn closing_notifies_service() {
        let (mut dev, _) = with_service(Echo::default());
        dev.handle(&guest_packet(OP_REQUEST, 0), &[]);
        dev.handle(&guest_packet(OP_RST, 0), &[]);
        assert!(dev.conns.is_empty());
    }

    #[test]
    fn rx_packet_with_data_spans_guest_buffers_and_tx_reads_payload() {
        let mem = VecMemory::new(0x20000);
        let outbox = VsockOutbox::new();
        let mut dev = VirtioVsock::new(3).with_services(
            vec![(9600, Box::new(Echo::default()) as Box<dyn VsockService>)],
            std::sync::Arc::clone(&outbox),
        );
        let (mut tx, mut rx) = (queue(0x1000), queue(0x8000));

        // Запрос и данные гостя: заголовок и данные в одном дескрипторе.
        mem.write(0x10000, &guest_packet(OP_REQUEST, 0).to_bytes())
            .unwrap();
        post(&mem, &tx, 0, 0x10000, HEADER_LEN as u32, 0);
        assert!(dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap());
        let mut pkt = guest_packet(OP_RW, 5).to_bytes().to_vec();
        pkt.extend_from_slice(b"hello");
        mem.write(0x10100, &pkt).unwrap();
        post(&mem, &tx, 1, 0x10100, u32::try_from(pkt.len()).unwrap(), 0);
        assert!(dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap());

        // Приём: буфер 1 вмещает заголовок и 2 байта, буфер 2 — остаток. Это два разных
        // пакета в двух буферах: RESPONSE и эхо «hello».
        post(&mem, &rx, 0, 0x11000, 64, VIRTQ_DESC_F_WRITE);
        post(&mem, &rx, 1, 0x12000, 64, VIRTQ_DESC_F_WRITE);
        assert!(dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
        let mut b = [0_u8; HEADER_LEN];
        mem.read(0x11000, &mut b).unwrap();
        assert_eq!(Header::parse(&b).op, OP_RESPONSE);
        mem.read(0x12000, &mut b).unwrap();
        let rw = Header::parse(&b);
        assert_eq!((rw.op, rw.len), (OP_RW, 5));
        let mut data = [0_u8; 5];
        mem.read(0x12000 + HEADER_LEN as u64, &mut data).unwrap();
        assert_eq!(&data, b"hello");
    }
}
