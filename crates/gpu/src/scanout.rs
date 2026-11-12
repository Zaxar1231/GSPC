//! Передача кадров от virtio-gpu к композитору: «почтовый ящик» последнего кадра.
//!
//! Устройство публикует кадр по `RESOURCE_FLUSH`, композитор забирает самый свежий, старые
//! кадры пропускаются (минимальная задержка). Буферы переиспользуются через пул, чтобы
//! в установившемся режиме не было аллокаций. Блокировка держится только на время обмена
//! указателем.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Формат пикселей кадра (порядок байт в памяти).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// B, G, R, A/X — `DXGI_FORMAT_B8G8R8A8_UNORM`.
    Bgra8,
    /// R, G, B, A/X — `DXGI_FORMAT_R8G8B8A8_UNORM`.
    Rgba8,
}

/// Кадр гостя.
#[derive(Debug)]
pub struct Frame {
    /// Ширина.
    pub width: u32,
    /// Высота.
    pub height: u32,
    /// Байт на строку.
    pub stride: u32,
    /// Формат.
    pub format: PixelFormat,
    /// Пиксели.
    pub data: Vec<u8>,
    /// Порядковый номер (растёт с каждым кадром).
    pub seq: u64,
    /// Момент `RESOURCE_FLUSH` (для измерения задержки до present).
    pub flushed_at: Instant,
}

/// Почтовый ящик кадров одного scanout.
#[derive(Debug, Default)]
pub struct Mailbox {
    latest: Mutex<Option<Arc<Frame>>>,
    seq: AtomicU64,
    pool: Mutex<Vec<Vec<u8>>>,
    ready: Condvar,
}

const POOL_LIMIT: usize = 3;

impl Mailbox {
    /// Новый пустой ящик.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Буфер из пула (или новый) длиной `len`.
    pub fn buffer(&self, len: usize) -> Vec<u8> {
        let mut buf = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_default();
        buf.resize(len, 0);
        buf
    }

    /// Публикует кадр; номер кадра назначается здесь.
    pub fn publish(&self, mut frame: Frame) {
        // Номер и кадр меняются под одной блокировкой: ожидающий не увидит новый номер со
        // старым кадром.
        let old = {
            let mut latest = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
            frame.seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
            latest.replace(Arc::new(frame))
        };
        self.ready.notify_all();
        if let Some(old) = old {
            self.recycle(old);
        }
    }

    /// Ждёт кадр новее `seen` не дольше `timeout` (композитор спит, пока гость не нарисует
    /// новый кадр, и просыпается сразу после `RESOURCE_FLUSH`).
    #[must_use]
    pub fn wait_after(&self, seen: u64, timeout: Duration) -> Option<Arc<Frame>> {
        let guard = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |_| self.seq.load(Ordering::Acquire) <= seen)
            .unwrap_or_else(PoisonError::into_inner);
        guard.as_ref().filter(|f| f.seq > seen).cloned()
    }

    /// Номер последнего опубликованного кадра.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Последний кадр, если он новее `seen`.
    #[must_use]
    pub fn latest_after(&self, seen: u64) -> Option<Arc<Frame>> {
        if self.seq() <= seen {
            return None;
        }
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Возвращает буфер кадра в пул, если на кадр больше никто не ссылается.
    pub fn recycle(&self, frame: Arc<Frame>) {
        if let Ok(f) = Arc::try_unwrap(frame) {
            let mut pool = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
            if pool.len() < POOL_LIMIT {
                pool.push(f.data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(mb: &Mailbox, fill: u8) -> Frame {
        let mut data = mb.buffer(16);
        data.fill(fill);
        Frame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra8,
            data,
            seq: 0,
            flushed_at: Instant::now(),
        }
    }

    #[test]
    fn latest_wins_and_seq_grows() {
        let mb = Mailbox::new();
        assert!(mb.latest_after(0).is_none());
        mb.publish(frame(&mb, 1));
        mb.publish(frame(&mb, 2));
        let f = mb.latest_after(0).unwrap();
        assert_eq!(f.seq, 2);
        assert_eq!(f.data[0], 2);
        assert!(mb.latest_after(2).is_none());
    }

    #[test]
    fn wait_wakes_on_publish() {
        let mb = Mailbox::new();
        assert!(mb.wait_after(0, Duration::from_millis(1)).is_none());
        let mb2 = mb.clone();
        let t =
            std::thread::spawn(move || mb2.wait_after(0, Duration::from_secs(5)).map(|f| f.seq));
        std::thread::sleep(Duration::from_millis(20));
        let f = frame(&mb, 3);
        mb.publish(f);
        assert_eq!(t.join().unwrap(), Some(1));
    }

    #[test]
    fn buffers_are_recycled() {
        let mb = Mailbox::new();
        mb.publish(frame(&mb, 1));
        mb.publish(frame(&mb, 2)); // первый кадр уходит в пул
        assert_eq!(mb.pool.lock().unwrap().len(), 1);
        let b = mb.buffer(16);
        assert_eq!(b.len(), 16);
        assert!(mb.pool.lock().unwrap().is_empty());
    }
}
