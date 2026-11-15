//! Колесо мыши → жест прокрутки на сенсорном экране.
//!
//! У гостя нет мыши (с устройством-мышью Android показывает курсор), поэтому прокрутка —
//! короткое движение отдельного «пальца»: касание под курсором, сдвиг на [`STEP_PER_NOTCH`]
//! высоты экрана за щелчок колеса и задержка в конце, чтобы Android не запускал инерцию (fling):
//! скорость пальца к моменту отпускания нулевая, и список останавливается там, куда его довели.

/// Сдвиг пальца за один щелчок колеса (120 единиц `WHEEL_DELTA`), доля высоты экрана.
pub const STEP_PER_NOTCH: f32 = 0.1;

/// Наибольший сдвиг за один жест, доля высоты.
pub const MAX_TRAVEL: f32 = 0.6;

/// Поля сверху и снизу: палец не касается края (там шторка и жесты навигации).
pub const EDGE: f32 = 0.08;

/// Сколько шагов движения и через сколько миллисекунд.
const MOVE_STEPS: u32 = 8;
const MOVE_MS: u64 = 10;
/// Сколько держать палец неподвижно в конце (окно скорости `VelocityTracker` — 100 мс).
const HOLD_STEPS: u32 = 4;
const HOLD_MS: u64 = 30;

/// Шаг жеста.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// Пауза перед шагом, мс.
    pub delay_ms: u64,
    /// Положение пальца по Y, доля высоты; `None` — отпустить.
    pub y: Option<f32>,
}

/// Жест для `notches` щелчков (положительные — колесо от себя, прокрутка к началу, как в
/// браузере: палец движется вниз) с курсором на высоте `y` (доля). Пустой — если сдвига нет.
#[must_use]
pub fn plan(y: f32, notches: f32) -> Vec<Step> {
    if !notches.is_finite() || notches == 0.0 || !y.is_finite() {
        return Vec::new();
    }
    let travel = (notches * STEP_PER_NOTCH).clamp(-MAX_TRAVEL, MAX_TRAVEL);
    // Начало — под курсором, но так, чтобы весь путь уместился между полями.
    let (lo, hi) = (EDGE, 1.0 - EDGE);
    let start = if travel > 0.0 {
        y.clamp(lo, (hi - travel).max(lo))
    } else {
        y.clamp((lo - travel).min(hi), hi)
    };
    let end = (start + travel).clamp(lo, hi);
    let mut out = vec![Step {
        delay_ms: 0,
        y: Some(start),
    }];
    for i in 1..=MOVE_STEPS {
        #[allow(clippy::cast_precision_loss, reason = "номер шага мал")]
        let t = i as f32 / MOVE_STEPS as f32;
        out.push(Step {
            delay_ms: MOVE_MS,
            y: Some(start + (end - start) * t),
        });
    }
    for _ in 0..HOLD_STEPS {
        out.push(Step {
            delay_ms: HOLD_MS,
            y: Some(end),
        });
    }
    out.push(Step {
        delay_ms: HOLD_MS,
        y: None,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ys(steps: &[Step]) -> Vec<f32> {
        steps.iter().filter_map(|s| s.y).collect()
    }

    #[test]
    fn one_notch_moves_finger_down() {
        let s = plan(0.5, 1.0);
        let y = ys(&s);
        assert!((y[0] - 0.5).abs() < 1e-6);
        assert!((y.last().copied().unwrap_or(0.0) - 0.6).abs() < 1e-6);
        assert!(y.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(s.last().map(|s| s.y), Some(None));
        // Конец держится неподвижно не меньше 100 мс.
        let hold: u64 = s
            .iter()
            .rev()
            .skip(1)
            .take(HOLD_STEPS as usize)
            .map(|s| s.delay_ms)
            .sum();
        assert!(hold >= 100);
    }

    #[test]
    fn negative_moves_up_and_respects_edges() {
        let y = ys(&plan(0.1, -2.0));
        // Курсор у верхнего края: начало сдвигается вниз, чтобы путь вверх уместился.
        assert!((y[0] - (EDGE + 0.2)).abs() < 1e-6);
        assert!((y.last().copied().unwrap_or(0.0) - EDGE).abs() < 1e-6);
        let y = ys(&plan(0.95, 3.0));
        assert!(y.iter().all(|v| (EDGE..=1.0 - EDGE).contains(v)));
    }

    #[test]
    fn travel_is_limited() {
        let y = ys(&plan(0.5, 50.0));
        let travel = y.last().copied().unwrap_or(0.0) - y[0];
        assert!(travel <= MAX_TRAVEL + 1e-6);
        assert!(travel > 0.5);
    }

    #[test]
    fn nothing_to_do() {
        assert!(plan(0.5, 0.0).is_empty());
        assert!(plan(f32::NAN, 1.0).is_empty());
        assert!(plan(0.5, f32::INFINITY).is_empty());
    }
}
