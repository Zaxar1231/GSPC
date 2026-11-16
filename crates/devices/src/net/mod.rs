//! virtio-net (virtio 1.x, раздел 5.1): сетевая карта гостя.
//!
//! Очередь 0 — receiveq (хост → гость), 1 — transmitq (гость → хост). Возможности: `MAC`,
//! `STATUS` (ссылка всегда поднята); без offload (контрольные суммы гость считает сам), без
//! mergeable-буферов и GSO — заголовок пакета 12 байт, кадр до 1514 байт.
//!
//! Сам трафик обслуживает [`NetBackend`]; готовый бэкенд — [`NatBackend`] (NAT в
//! пользовательском режиме из `rustdroid-net`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustdroid_net::{Nat, NatConfig};
use rustdroid_virtio::device::TYPE_NET;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

/// Индекс очереди приёма.
pub const RX_QUEUE: usize = 0;
/// Индекс очереди передачи.
pub const TX_QUEUE: usize = 1;

/// Возможность: MAC в конфигурации.
const F_MAC: u64 = 1 << 5;
/// Возможность: поле статуса.
const F_STATUS: u64 = 1 << 16;
/// Длина заголовка пакета (`virtio_net_hdr_v1`).
const HDR_LEN: usize = 12;
/// Самый длинный кадр, который принимаем от гостя.
const MAX_FRAME: usize = 65_536;
/// MAC гостя по умолчанию (как у QEMU).
pub const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// Источник и приёмник кадров Ethernet для [`VirtioNet`].
pub trait NetBackend: Send {
    /// Кадр от гостя.
    fn send(&mut self, frame: &[u8]);
    /// Следующий кадр для гостя, если есть.
    fn recv(&mut self) -> Option<Vec<u8>>;
    /// Вернуть кадр в начало очереди приёма (у гостя нет места).
    fn unrecv(&mut self, frame: Vec<u8>);
}

/// Будильник очереди приёма: бэкенд зовёт [`NetWake::wake`], когда у него появились кадры для
/// гостя; ВМ связывает его с worker'ом устройства.
#[derive(Default)]
pub struct NetWake {
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for NetWake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetWake").finish_non_exhaustive()
    }
}

impl NetWake {
    /// Пустой будильник.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Задаёт функцию, будящую обработку очереди приёма.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(waker));
    }

    /// Будит обработку очереди приёма.
    pub fn wake(&self) {
        if let Some(w) = self
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            w();
        }
    }
}

/// Устройство virtio-net.
pub struct VirtioNet {
    backend: Box<dyn NetBackend>,
    mac: [u8; 6],
    sizes: [u16; 2],
    chain: Chain,
    frame: Vec<u8>,
}

impl std::fmt::Debug for VirtioNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioNet")
            .field("mac", &self.mac)
            .finish_non_exhaustive()
    }
}

impl VirtioNet {
    /// Карта с MAC `mac` поверх `backend`.
    #[must_use]
    pub fn new(backend: Box<dyn NetBackend>, mac: [u8; 6]) -> Self {
        Self {
            backend,
            mac,
            sizes: [256, 256],
            chain: Chain::default(),
            frame: Vec::new(),
        }
    }

    fn transmit(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while queue.pop(mem, &mut self.chain)? {
            self.frame.clear();
            for d in self.chain.descriptors.iter().filter(|d| !d.is_write()) {
                let len = (d.len as usize).min(MAX_FRAME + HDR_LEN - self.frame.len());
                let at = self.frame.len();
                self.frame.resize(at + len, 0);
                if let Some(dst) = self.frame.get_mut(at..)
                    && mem.read(d.addr, dst).is_err()
                {
                    self.frame.truncate(at);
                    break;
                }
            }
            if let Some(frame) = self.frame.get(HDR_LEN..)
                && !frame.is_empty()
            {
                self.backend.send(frame);
            }
            queue.add_used(mem, self.chain.head, 0)?;
            any = true;
        }
        Ok(any)
    }

    fn receive(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while let Some(frame) = self.backend.recv() {
            if !queue.pop(mem, &mut self.chain)? {
                // У гостя нет буферов: кадр ждёт, пока они появятся (гость уведомит очередь).
                self.backend.unrecv(frame);
                break;
            }
            let mut hdr = [0u8; HDR_LEN];
            // num_buffers = 1.
            hdr[10] = 1;
            let mut written = 0usize;
            let total = HDR_LEN + frame.len();
            for d in self.chain.descriptors.iter().filter(|d| d.is_write()) {
                if written >= total {
                    break;
                }
                let n = (d.len as usize).min(total - written);
                let mut at = 0usize;
                // Кусок заголовка и/или кадра, попадающий в этот дескриптор.
                let mut piece = Vec::with_capacity(n);
                while at < n {
                    let pos = written + at;
                    let src = if pos < HDR_LEN {
                        hdr.get(pos..).unwrap_or(&[])
                    } else {
                        frame.get(pos - HDR_LEN..).unwrap_or(&[])
                    };
                    let take = src.len().min(n - at);
                    piece.extend_from_slice(src.get(..take).unwrap_or(&[]));
                    at += take;
                    if take == 0 {
                        break;
                    }
                }
                if mem.write(d.addr, &piece).is_err() {
                    break;
                }
                written += piece.len();
            }
            #[allow(clippy::cast_possible_truncation, reason = "кадр не длиннее 64 КиБ")]
            queue.add_used(mem, self.chain.head, written as u32)?;
            any = true;
        }
        Ok(any)
    }
}

impl VirtioDevice for VirtioNet {
    fn device_type(&self) -> u32 {
        TYPE_NET
    }
    fn device_features(&self) -> u64 {
        F_MAC | F_STATUS
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // mac[6], status (u16 LE: LINK_UP = 1).
        let mut cfg = [0u8; 8];
        cfg[..6].copy_from_slice(&self.mac);
        cfg[6] = 1;
        for (i, b) in data.iter_mut().enumerate() {
            *b = usize::try_from(offset)
                .ok()
                .and_then(|o| cfg.get(o + i))
                .copied()
                .unwrap_or(0);
        }
    }
    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        match index {
            // Гость выложил буферы приёма или нас разбудил бэкенд.
            RX_QUEUE => self.receive(queue, mem),
            TX_QUEUE => self.transmit(queue, mem),
            _ => Ok(false),
        }
    }
}

/// Общее состояние бэкенда и его потока.
struct Shared {
    nat: Mutex<Nat>,
    wake_thread: Condvar,
    stop: AtomicBool,
}

/// Бэкенд на NAT в пользовательском режиме: свой поток опрашивает сокеты хоста и будит приём.
pub struct NatBackend {
    shared: Arc<Shared>,
    wake: Arc<NetWake>,
}

impl std::fmt::Debug for NatBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatBackend").finish_non_exhaustive()
    }
}

impl NatBackend {
    /// Запускает NAT; `wake` будит очередь приёма гостя.
    ///
    /// # Errors
    /// Не удалось создать поток.
    pub fn new(cfg: NatConfig, wake: Arc<NetWake>) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            nat: Mutex::new(Nat::new(cfg)),
            wake_thread: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let s = Arc::clone(&shared);
        let w = Arc::clone(&wake);
        std::thread::Builder::new()
            .name("net-nat".into())
            .spawn(move || nat_loop(&s, &w))?;
        Ok(Self { shared, wake })
    }

    fn nat(&self) -> std::sync::MutexGuard<'_, Nat> {
        self.shared
            .nat
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Пока есть соединения — опрос раз в миллисекунду, иначе раз в 10 мс; кадр от гостя будит
/// поток сразу.
fn nat_loop(shared: &Shared, wake: &NetWake) {
    let mut nat = shared.nat.lock().unwrap_or_else(PoisonError::into_inner);
    while !shared.stop.load(Ordering::Acquire) {
        let progress = nat.poll(Instant::now());
        let queued = nat.queued() > 0;
        if progress && queued {
            wake.wake();
        }
        let wait = if progress || nat.tcp_flows() > 0 {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(10)
        };
        nat = shared
            .wake_thread
            .wait_timeout(nat, wait)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

impl Drop for NatBackend {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake_thread.notify_all();
    }
}

impl NetBackend for NatBackend {
    fn send(&mut self, frame: &[u8]) {
        let immediate = {
            let mut nat = self.nat();
            nat.guest_frame(frame, Instant::now());
            nat.queued() > 0
        };
        // Ответы ARP/DHCP/RST готовы сразу; остальное (данные, SYN-ACK) подготовит поток —
        // будим его, не дожидаясь периода опроса.
        if immediate {
            self.wake.wake();
        }
        self.shared.wake_thread.notify_all();
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        self.nat().pop_frame()
    }
    fn unrecv(&mut self, frame: Vec<u8>) {
        self.nat().push_back_front(frame);
    }
}

#[cfg(test)]
mod tests;
