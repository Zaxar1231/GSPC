//! Статистика времени кадра.
//!
//! Определения (совпадают с тем, как считаем при проверке через PresentMon):
//! - **средний FPS** = число кадров / суммарное время кадров;
//! - **1% low** = FPS, соответствующий 99-му перцентилю времени кадра (nearest-rank).
//!   Цель проекта: `1% low / средний FPS ≥ 0.85`.

use std::time::Duration;

/// Сводка по окну кадров.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSummary {
    /// Число кадров в окне.
    pub frames: usize,
    /// Средний FPS.
    pub avg_fps: f64,
    /// 1% low (FPS для 99-го перцентиля времени кадра).
    pub low_1_percent_fps: f64,
    /// Среднее время кадра, мс.
    pub avg_frame_ms: f64,
    /// 99-й перцентиль времени кадра, мс.
    pub p99_frame_ms: f64,
    /// Максимальное время кадра, мс.
    pub max_frame_ms: f64,
}

impl FrameSummary {
    /// Отношение 1% low к среднему FPS. `0.0`, если среднее не определено.
    #[must_use]
    pub fn low_to_avg_ratio(&self) -> f64 {
        if self.avg_fps > 0.0 {
            self.low_1_percent_fps / self.avg_fps
        } else {
            0.0
        }
    }
}

/// Кольцевое окно времён кадра фиксированной ёмкости.
///
/// [`FrameStats::record`] не аллоцирует и не блокирует, поэтому его можно вызывать прямо из
/// present-потока. [`FrameStats::summary`] тоже не аллоцирует: рабочий буфер выделяется
/// один раз в [`FrameStats::new`].
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// Времена кадров в миллисекундах.
    samples: Box<[f32]>,
    /// Индекс, куда будет записан следующий кадр.
    head: usize,
    /// Число валидных значений (не больше ёмкости).
    len: usize,
    /// Рабочий буфер для вычисления перцентиля.
    scratch: Box<[f32]>,
}

impl FrameStats {
    /// Создаёт окно на `capacity` кадров (минимум 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: vec![0.0; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
            scratch: vec![0.0; capacity].into_boxed_slice(),
        }
    }

    /// Ёмкость окна в кадрах.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// Число кадров в окне.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true`, если ни одного кадра ещё не записано.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Очищает окно (например, при смене игры или режима).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Записывает время одного кадра. Самое старое значение вытесняется при заполнении.
    #[inline]
    pub fn record(&mut self, frame_time: Duration) {
        self.record_ms(frame_time.as_secs_f32() * 1000.0);
    }

    /// Записывает время кадра в миллисекундах. Неконечные и отрицательные значения
    /// игнорируются.
    #[inline]
    pub fn record_ms(&mut self, frame_ms: f32) {
        if !frame_ms.is_finite() || frame_ms < 0.0 {
            return;
        }
        let cap = self.samples.len();
        if let Some(slot) = self.samples.get_mut(self.head) {
            *slot = frame_ms;
        }
        self.head = (self.head + 1) % cap;
        if self.len < cap {
            self.len += 1;
        }
    }

    /// Считает сводку по текущему окну. `None`, если окно пусто или суммарное время равно нулю.
    pub fn summary(&mut self) -> Option<FrameSummary> {
        let n = self.len;
        let valid = self.samples.get(..n)?;
        let scratch = self.scratch.get_mut(..n)?;
        scratch.copy_from_slice(valid);

        let mut total_ms = 0.0_f64;
        let mut max_ms = 0.0_f32;
        for &v in scratch.iter() {
            total_ms += f64::from(v);
            max_ms = max_ms.max(v);
        }
        if total_ms <= 0.0 {
            return None;
        }

        // Nearest-rank: ранг = ceil(0.99 * n), индекс = ранг - 1.
        let rank = (n * 99).div_ceil(100).max(1);
        let (_, p99, _) = scratch.select_nth_unstable_by(rank - 1, f32::total_cmp);
        let p99_ms = f64::from(*p99);

        #[allow(clippy::cast_precision_loss, reason = "окно кадров много меньше 2^52")]
        let frames = n as f64;
        let avg_frame_ms = total_ms / frames;
        Some(FrameSummary {
            frames: n,
            avg_fps: 1000.0 / avg_frame_ms,
            low_1_percent_fps: if p99_ms > 0.0 { 1000.0 / p99_ms } else { 0.0 },
            avg_frame_ms,
            p99_frame_ms: p99_ms,
            max_frame_ms: f64::from(max_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn empty_has_no_summary() {
        let mut s = FrameStats::new(10);
        assert!(s.summary().is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn zero_capacity_is_clamped() {
        let mut s = FrameStats::new(0);
        assert_eq!(s.capacity(), 1);
        s.record_ms(10.0);
        let sum = s.summary().unwrap();
        assert!(approx(sum.avg_fps, 100.0));
    }

    #[test]
    fn constant_frame_time() {
        let mut s = FrameStats::new(1000);
        for _ in 0..600 {
            s.record_ms(10.0);
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.frames, 600);
        assert!(approx(sum.avg_fps, 100.0));
        assert!(approx(sum.low_1_percent_fps, 100.0));
        assert!(approx(sum.low_to_avg_ratio(), 1.0));
    }

    #[test]
    fn spikes_lower_one_percent_low() {
        let mut s = FrameStats::new(100);
        for _ in 0..98 {
            s.record_ms(10.0);
        }
        s.record_ms(50.0);
        s.record_ms(50.0);
        let sum = s.summary().unwrap();
        // Ранг ceil(0.99 * 100) = 99 → 99-е значение по возрастанию = 50 мс.
        assert!(approx(sum.p99_frame_ms, 50.0));
        assert!(approx(sum.low_1_percent_fps, 20.0));
        assert!(approx(sum.max_frame_ms, 50.0));
        assert!(sum.low_to_avg_ratio() < 0.85);
    }

    #[test]
    fn single_spike_in_hundred_is_within_one_percent() {
        let mut s = FrameStats::new(100);
        for _ in 0..99 {
            s.record_ms(10.0);
        }
        s.record_ms(80.0);
        let sum = s.summary().unwrap();
        assert!(approx(sum.p99_frame_ms, 10.0));
        assert!(approx(sum.max_frame_ms, 80.0));
    }

    #[test]
    fn ring_overwrites_oldest() {
        let mut s = FrameStats::new(4);
        for v in [100.0, 100.0, 10.0, 10.0, 10.0, 10.0] {
            s.record_ms(v);
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.frames, 4);
        assert!(approx(sum.max_frame_ms, 10.0));
    }

    #[test]
    fn invalid_samples_are_ignored() {
        let mut s = FrameStats::new(4);
        s.record_ms(f32::NAN);
        s.record_ms(f32::INFINITY);
        s.record_ms(-1.0);
        assert!(s.is_empty());
        s.record(Duration::from_millis(8));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn clear_resets_window() {
        let mut s = FrameStats::new(4);
        s.record_ms(10.0);
        s.clear();
        assert!(s.summary().is_none());
    }
}
