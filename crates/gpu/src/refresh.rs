//! Повторное чтение последнего кадра после затишья.
//!
//! С gfxstream кадр читается из ресурса по `RESOURCE_FLUSH`, а хост мог ещё не закончить его
//! рисовать (команды гостя исполняются потоками gfxstream асинхронно). Пока гость рисует подряд,
//! следующий кадр всё исправляет; но последний кадр перед паузой оставался недорисованным —
//! картинка «замирала», пока касание не вызывало новый кадр. Поэтому после затишья устройство
//! перечитывает кадр ещё [`DELAYS`]`.len()` раз.
//!
//! Таймер — отдельный поток: по сроку он будит обработку очереди управления (тот же будильник,
//! что и у фенсов), а само чтение делает поток устройства — бэкенд не потокобезопасен.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::backend::FenceSink;

/// Через сколько после последнего кадра (и затем после предыдущего перечитывания) читать снова.
pub(crate) const DELAYS: [Duration; 2] = [Duration::from_millis(80), Duration::from_millis(500)];

/// План перечитывания: какой ресурс и когда.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    due: Option<(u32, Instant, usize)>,
}

impl Plan {
    /// Новый кадр ресурса `id` в момент `now`: план начинается заново. Возвращает задержку до
    /// первого перечитывания.
    pub(crate) fn frame(&mut self, id: u32, now: Instant) -> Duration {
        let d = DELAYS[0];
        self.due = Some((id, now + d, 0));
        d
    }

    /// Пора ли перечитать (в момент `now`): ресурс, и задержка до следующего раза (`None` —
    /// это последнее). План сдвигается.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<(u32, Option<Duration>)> {
        let (id, at, step) = self.due?;
        if now < at {
            return None;
        }
        let next = DELAYS.get(step + 1).copied();
        self.due = next.map(|d| (id, now + d, step + 1));
        Some((id, next))
    }

    /// Отменяет план (сброс устройства, смена scanout).
    pub(crate) fn clear(&mut self) {
        self.due = None;
    }
}

/// Меняется ли содержимое кадров, которые уходят в окно: число кадров, сколько из них отличались
/// от предыдущего, и хэш последнего — раз в [`CONTENT_LOG`]. Сравнивается с хэшем `screencap`
/// гостя: если гость видит изменения, а кадры одинаковые — устаревшие кадры читаем мы.
#[derive(Debug)]
pub(crate) struct Content {
    last: u64,
    frames: u32,
    changed: u32,
    since: Instant,
}

/// Как часто писать сводку [`Content`].
pub(crate) const CONTENT_LOG: Duration = Duration::from_secs(10);

impl Content {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last: 0,
            frames: 0,
            changed: 0,
            since: now,
        }
    }

    /// Учитывает кадр; раз в [`CONTENT_LOG`] возвращает сводку `(кадров, изменённых, хэш)`.
    pub(crate) fn frame(&mut self, data: &[u8], now: Instant) -> Option<(u32, u32, u64)> {
        let h = hash(data);
        self.frames += 1;
        if h != self.last {
            self.changed += 1;
            self.last = h;
        }
        if now.duration_since(self.since) < CONTENT_LOG {
            return None;
        }
        let out = (self.frames, self.changed, self.last);
        self.frames = 0;
        self.changed = 0;
        self.since = now;
        Some(out)
    }
}

/// Быстрый хэш кадра: каждое 4-е 8-байтовое слово (кадр 1280×720 — ~115 тыс. умножений).
#[must_use]
pub(crate) fn hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in data.as_chunks::<32>().0 {
        let w: [u8; 8] = [
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ];
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(0x0100_0000_01b3);
    }
    h ^ data.len() as u64
}

/// Поток-таймер: будит устройство через заданную задержку; новая задержка заменяет старую.
#[derive(Debug)]
pub(crate) struct Timer {
    tx: Sender<Duration>,
}

impl Timer {
    /// Запускает поток; он завершится, когда таймер удалят. `None` — поток не создан
    /// (перечитывания не будет, кадры идут как раньше).
    #[must_use]
    pub(crate) fn spawn(fences: Arc<FenceSink>) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Duration>();
        std::thread::Builder::new()
            .name("gpu-refresh".into())
            .spawn(move || {
                while let Ok(mut delay) = rx.recv() {
                    loop {
                        match rx.recv_timeout(delay) {
                            Ok(d) => delay = d,
                            Err(RecvTimeoutError::Timeout) => {
                                fences.wake();
                                break;
                            }
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                }
            })
            .map_err(|e| tracing::debug!(error = %e, "поток перечитывания кадра не создан"))
            .ok()?;
        Some(Self { tx })
    }

    /// Разбудить устройство через `delay`.
    pub(crate) fn arm(&self, delay: Duration) {
        let _ = self.tx.send(delay);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn plan_rereads_after_quiet() {
        let t0 = Instant::now();
        let mut p = Plan::default();
        assert_eq!(p.take_due(t0), None);
        assert_eq!(p.frame(7, t0), DELAYS[0]);
        assert_eq!(p.take_due(t0 + Duration::from_millis(10)), None);
        // Новый кадр сдвигает срок.
        p.frame(7, t0 + Duration::from_millis(50));
        assert_eq!(p.take_due(t0 + Duration::from_millis(100)), None);
        let t1 = t0 + Duration::from_millis(140);
        assert_eq!(p.take_due(t1), Some((7, Some(DELAYS[1]))));
        assert_eq!(p.take_due(t1 + Duration::from_millis(100)), None);
        assert_eq!(p.take_due(t1 + DELAYS[1]), Some((7, None)));
        assert_eq!(p.take_due(t1 + Duration::from_secs(10)), None);
        p.frame(3, t1);
        p.clear();
        assert_eq!(p.take_due(t1 + Duration::from_secs(1)), None);
    }

    #[test]
    fn content_changes() {
        let t0 = Instant::now();
        let mut c = Content::new(t0);
        let a = vec![1u8; 4096];
        let mut b = a.clone();
        b[64] = 2;
        assert_ne!(hash(&a), hash(&b));
        assert_eq!(c.frame(&a, t0), None);
        assert_eq!(c.frame(&a, t0), None);
        assert_eq!(c.frame(&b, t0), None);
        let s = c.frame(&b, t0 + CONTENT_LOG).map(|(f, ch, _)| (f, ch));
        assert_eq!(s, Some((4, 2)));
        assert_eq!(
            c.frame(&b, t0 + CONTENT_LOG * 2).map(|(f, ch, _)| (f, ch)),
            Some((1, 0))
        );
    }

    #[test]
    fn timer_wakes_once_after_last_arm() {
        let sink = FenceSink::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        sink.set_waker(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        let timer = Timer::spawn(Arc::clone(&sink)).expect("поток таймера");
        timer.arm(Duration::from_millis(30));
        timer.arm(Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        timer.arm(Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
