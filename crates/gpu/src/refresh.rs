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
