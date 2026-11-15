//! Пульс гостя: shell гостя раз в [`PERIOD_SECS`] пишет в консоль `rustdroid: hb N`, а хост
//! сверяет, вовремя ли пришла строка. Опоздание значит, что гость не просыпался по своему
//! таймеру (vCPU спал в HLT дольше, чем надо) — так выглядит «зависание без касаний».
//!
//! Строки на консоли перемешиваются с сообщениями ядра, поэтому часть отметок может не
//! распознаться; номер `N` позволяет не считать пропуск опозданием.

/// Период пульса в гостевом скрипте, с.
pub(crate) const PERIOD_SECS: f64 = 5.0;

/// С какого опоздания отметка считается задержкой, с.
pub(crate) const LATE_SECS: f64 = 1.0;

const MARKER: &[u8] = b"rustdroid: hb ";

/// Сколько хвоста держать между кусками вывода: маркер и число могут прийти в разных `write`.
const TAIL: usize = 32;

/// Находит номера отметок `rustdroid: hb N` в потоке байт консоли.
#[derive(Debug, Default)]
pub(crate) struct Scanner {
    buf: Vec<u8>,
}

impl Scanner {
    /// Добавляет кусок вывода, возвращает номера полностью пришедших отметок.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<u64> {
        self.buf.extend_from_slice(bytes);
        let mut found = Vec::new();
        let mut consumed = 0;
        let mut from = 0;
        while let Some(pos) = find(self.buf.get(from..).unwrap_or(&[]), MARKER) {
            let start = from + pos + MARKER.len();
            let rest = self.buf.get(start..).unwrap_or(&[]);
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits == rest.len() {
                // Число ещё не закончилось — ждём следующий кусок.
                consumed = from + pos;
                break;
            }
            if digits > 0 {
                let n = std::str::from_utf8(rest.get(..digits).unwrap_or(&[]))
                    .ok()
                    .and_then(|s| s.parse().ok());
                if let Some(n) = n {
                    found.push(n);
                }
            }
            from = start + digits;
            consumed = from;
        }
        if consumed == 0 && found.is_empty() {
            // Маркера нет — достаточно хвоста, в котором может начинаться следующий.
            consumed = self.buf.len().saturating_sub(TAIL);
        }
        self.buf.drain(..consumed.min(self.buf.len()));
        if self.buf.len() > 4 * TAIL {
            let cut = self.buf.len() - TAIL;
            self.buf.drain(..cut);
        }
        found
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Итог одной отметки.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Beat {
    /// Номер отметки.
    pub(crate) seq: u64,
    /// Опоздание относительно предыдущей распознанной отметки, с (не меньше 0).
    pub(crate) late: f64,
}

/// Считает опоздания отметок по времени хоста.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    last: Option<(f64, u64)>,
    /// Наибольшее опоздание за всё время, с.
    pub(crate) max_late: f64,
    /// Отметок с опозданием от [`LATE_SECS`].
    pub(crate) late_count: u64,
    /// Всего отметок.
    pub(crate) beats: u64,
}

impl Tracker {
    /// Отметка `seq` пришла в момент `now` (секунды хоста от любой точки отсчёта).
    pub(crate) fn beat(&mut self, now: f64, seq: u64) -> Beat {
        self.beats += 1;
        let late = match self.last {
            // Номер не растёт — гость перезапустил скрипт; начинаем заново.
            Some((t, s)) if seq > s => {
                #[allow(clippy::cast_precision_loss, reason = "номера отметок малы")]
                let expected = (seq - s) as f64 * PERIOD_SECS;
                (now - t - expected).max(0.0)
            }
            _ => 0.0,
        };
        self.last = Some((now, seq));
        if late >= LATE_SECS {
            self.late_count += 1;
        }
        self.max_late = self.max_late.max(late);
        Beat { seq, late }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_markers_across_chunks() {
        let mut s = Scanner::default();
        assert_eq!(s.feed(b"junk rustdroid: hb 1\nmore"), vec![1]);
        assert_eq!(s.feed(b"[ 1.0] x rustdroid: h"), Vec::<u64>::new());
        assert_eq!(s.feed(b"b 2"), Vec::<u64>::new());
        assert_eq!(s.feed(b"3\r\nrustdroid: hb 24\n"), vec![23, 24]);
        // Испорченная отметка без числа пропускается.
        assert_eq!(s.feed(b"rustdroid: hb x rustdroid: hb 25 "), vec![25]);
    }

    #[test]
    fn buffer_stays_small() {
        let mut s = Scanner::default();
        for _ in 0..1000 {
            assert!(s.feed(&[b'a'; 100]).is_empty());
        }
        assert!(s.buf.len() <= 4 * TAIL);
    }

    #[test]
    fn lateness() {
        let mut t = Tracker::default();
        assert_eq!(t.beat(100.0, 1).late, 0.0);
        assert!((t.beat(105.2, 2).late - 0.2).abs() < 1e-9);
        // Пропущенная отметка 3 — не опоздание.
        assert!(t.beat(115.3, 4).late < 0.2);
        // Гость спал 40 с вместо 5.
        assert!((t.beat(155.3, 5).late - 35.0).abs() < 1e-9);
        // Раньше срока — не отрицательное.
        assert_eq!(t.beat(159.0, 6).late, 0.0);
        // Перезапуск счётчика.
        assert_eq!(t.beat(200.0, 1).late, 0.0);
        assert_eq!(t.late_count, 1);
        assert_eq!(t.beats, 6);
        assert!((t.max_late - 35.0).abs() < 1e-9);
    }
}
