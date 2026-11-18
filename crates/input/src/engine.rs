//! Движок контролов: клавиши и время → события касаний (слот, точка в долях экрана).
//!
//! Чистая логика без потоков и часов: время передаёт вызывающий (монотонные `Duration`
//! от старта ВМ). Каждому контролу назначается свой слот мультитача, начиная с
//! [`FIRST_SLOT`]; слоты 0 и 1 заняты мышью и колесом (см. `rustdroid-app`).

use std::time::Duration;

use crate::geometry::NormPoint;
use crate::profile::{Control, HoldMode, Profile, ProfileError, scancode};

/// Первый слот, доступный контролам профиля.
pub const FIRST_SLOT: u32 = 2;

/// Событие касания.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Out {
    /// Палец коснулся в точке.
    Down {
        /// Слот мультитача.
        slot: u32,
        /// Точка в долях экрана.
        at: NormPoint,
    },
    /// Палец переместился.
    Move {
        /// Слот.
        slot: u32,
        /// Новая точка.
        at: NormPoint,
    },
    /// Палец поднят.
    Up {
        /// Слот.
        slot: u32,
    },
}

/// Состояние одного контрола.
#[derive(Debug, Default, Clone, Copy)]
struct Run {
    /// Клавиша сейчас нажата (для переключения — по фронту нажатия).
    held: bool,
    /// Палец касается экрана (слот занят).
    down: bool,
    /// Момент начала повтора или свайпа.
    since: Duration,
}

#[derive(Debug)]
struct Bound {
    code: u16,
    slot: u32,
    control: Control,
    /// Для переключения: сейчас касание включено.
    toggled: bool,
    run: Run,
}

/// Движок одного профиля.
#[derive(Debug)]
pub struct Engine {
    bound: Vec<Bound>,
}

impl Engine {
    /// Создаёт движок; профиль проверяется.
    ///
    /// # Errors
    /// Профиль не прошёл проверку.
    pub fn new(profile: &Profile) -> Result<Self, ProfileError> {
        profile.validate()?;
        let mut bound = Vec::with_capacity(profile.controls.len());
        for (index, c) in profile.controls.iter().enumerate() {
            let name = &c.key().0;
            let code = scancode(name).ok_or_else(|| ProfileError::UnknownKey {
                index,
                key: name.clone(),
            })?;
            let slot = FIRST_SLOT + u32::try_from(index).unwrap_or(u32::MAX);
            bound.push(Bound {
                code,
                slot,
                control: c.clone(),
                toggled: false,
                run: Run::default(),
            });
        }
        Ok(Self { bound })
    }

    /// Клавиша с скан-кодом `code` нажата или отпущена в момент `now`.
    pub fn key_event(&mut self, code: u16, pressed: bool, now: Duration, out: &mut Vec<Out>) {
        for b in self.bound.iter_mut().filter(|b| b.code == code) {
            if pressed == b.run.held {
                continue;
            }
            b.run.held = pressed;
            match &b.control {
                Control::Tap { x, y, .. }
                | Control::Hold {
                    x,
                    y,
                    mode: HoldMode::Hold,
                    ..
                } => {
                    let at = NormPoint::new(*x, *y);
                    if pressed {
                        b.run.down = true;
                        out.push(Out::Down { slot: b.slot, at });
                    } else if b.run.down {
                        b.run.down = false;
                        out.push(Out::Up { slot: b.slot });
                    }
                }
                Control::Hold {
                    x,
                    y,
                    mode: HoldMode::Toggle,
                    ..
                } => {
                    if pressed {
                        b.toggled = !b.toggled;
                        b.run.down = b.toggled;
                        let at = NormPoint::new(*x, *y);
                        out.push(if b.toggled {
                            Out::Down { slot: b.slot, at }
                        } else {
                            Out::Up { slot: b.slot }
                        });
                    }
                }
                Control::RepeatTap { x, y, .. } => {
                    if pressed {
                        b.run.since = now;
                        b.run.down = true;
                        out.push(Out::Down {
                            slot: b.slot,
                            at: NormPoint::new(*x, *y),
                        });
                    } else if b.run.down {
                        b.run.down = false;
                        out.push(Out::Up { slot: b.slot });
                    }
                }
                Control::Swipe { from, .. } => {
                    // Свайп идёт до конца, отпускание клавиши его не прерывает.
                    if pressed && !b.run.down {
                        b.run.since = now;
                        b.run.down = true;
                        out.push(Out::Down {
                            slot: b.slot,
                            at: NormPoint::new(from.x, from.y),
                        });
                    }
                }
            }
        }
    }

    /// Продвигает повторы и свайпы к моменту `now`.
    pub fn tick(&mut self, now: Duration, out: &mut Vec<Out>) {
        for b in &mut self.bound {
            match &b.control {
                Control::RepeatTap {
                    x, y, interval_ms, ..
                } if b.run.held => {
                    let iv = u128::from((*interval_ms).max(1));
                    let elapsed = now.saturating_sub(b.run.since).as_millis();
                    let want = elapsed % iv < iv / 2;
                    if want != b.run.down {
                        b.run.down = want;
                        out.push(if want {
                            Out::Down {
                                slot: b.slot,
                                at: NormPoint::new(*x, *y),
                            }
                        } else {
                            Out::Up { slot: b.slot }
                        });
                    }
                }
                Control::Swipe {
                    from,
                    to,
                    duration_ms,
                    ..
                } if b.run.down => {
                    let dur = u128::from((*duration_ms).max(1));
                    let elapsed = now.saturating_sub(b.run.since).as_millis();
                    if elapsed >= dur {
                        b.run.down = false;
                        out.push(Out::Up { slot: b.slot });
                    } else {
                        #[allow(clippy::cast_precision_loss, reason = "доля от 0 до 1")]
                        let t = elapsed as f32 / dur as f32;
                        let at = NormPoint::new(
                            from.x + (to.x - from.x) * t,
                            from.y + (to.y - from.y) * t,
                        );
                        out.push(Out::Move { slot: b.slot, at });
                    }
                }
                _ => {}
            }
        }
    }

    /// Отпускает все касания и сбрасывает состояние — при потере фокуса окном.
    pub fn release_all(&mut self, out: &mut Vec<Out>) {
        for b in &mut self.bound {
            if b.run.down {
                out.push(Out::Up { slot: b.slot });
            }
            b.toggled = false;
            b.run = Run::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": 1, "name": "t",
        "controls": [
            { "type": "tap", "key": "F", "x": 0.8, "y": 0.6 },
            { "type": "hold", "key": "C", "x": 0.9, "y": 0.85, "mode": "toggle" },
            { "type": "repeat_tap", "key": "E", "x": 0.7, "y": 0.8, "interval_ms": 50 },
            { "type": "swipe", "key": "Space", "from": {"x": 0.5, "y": 0.8},
              "to": {"x": 0.5, "y": 0.4}, "duration_ms": 80 }
        ]
    }"#;

    fn engine() -> Engine {
        let p = Profile::from_json(SAMPLE).expect("профиль");
        Engine::new(&p).expect("движок")
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn code(name: &str) -> u16 {
        scancode(name).expect("клавиша")
    }

    #[test]
    fn tap_follows_key() {
        let mut e = engine();
        let mut out = Vec::new();
        e.key_event(code("F"), true, ms(0), &mut out);
        assert_eq!(
            out,
            vec![Out::Down {
                slot: FIRST_SLOT,
                at: NormPoint::new(0.8, 0.6)
            }]
        );
        out.clear();
        e.key_event(code("F"), true, ms(5), &mut out); // автоповтор ОС — без событий
        assert!(out.is_empty());
        e.key_event(code("F"), false, ms(10), &mut out);
        assert_eq!(out, vec![Out::Up { slot: FIRST_SLOT }]);
    }

    #[test]
    fn toggle_flips_on_press() {
        let mut e = engine();
        let mut out = Vec::new();
        e.key_event(code("C"), true, ms(0), &mut out);
        e.key_event(code("C"), false, ms(10), &mut out);
        assert_eq!(out.len(), 1); // отпускание ничего не делает
        out.clear();
        e.key_event(code("C"), true, ms(20), &mut out);
        assert_eq!(
            out,
            vec![Out::Up {
                slot: FIRST_SLOT + 1
            }]
        );
    }

    #[test]
    fn repeat_alternates_by_interval() {
        let mut e = engine();
        let mut out = Vec::new();
        e.key_event(code("E"), true, ms(0), &mut out);
        out.clear();
        e.tick(ms(10), &mut out); // первая половина интервала: касание идёт
        assert!(out.is_empty());
        e.tick(ms(30), &mut out); // вторая половина: поднят
        assert_eq!(
            out,
            vec![Out::Up {
                slot: FIRST_SLOT + 2
            }]
        );
        out.clear();
        e.tick(ms(55), &mut out); // новый интервал: снова вниз
        assert_eq!(
            out,
            vec![Out::Down {
                slot: FIRST_SLOT + 2,
                at: NormPoint::new(0.7, 0.8)
            }]
        );
        out.clear();
        e.key_event(code("E"), false, ms(60), &mut out);
        assert_eq!(
            out,
            vec![Out::Up {
                slot: FIRST_SLOT + 2
            }]
        );
        out.clear();
        e.tick(ms(200), &mut out); // отпущено — повторов нет
        assert!(out.is_empty());
    }

    #[test]
    fn swipe_runs_to_the_end_even_after_release() {
        let mut e = engine();
        let mut out = Vec::new();
        e.key_event(code("Space"), true, ms(0), &mut out);
        assert_eq!(
            out,
            vec![Out::Down {
                slot: FIRST_SLOT + 3,
                at: NormPoint::new(0.5, 0.8)
            }]
        );
        out.clear();
        e.key_event(code("Space"), false, ms(10), &mut out);
        assert!(out.is_empty());
        e.tick(ms(40), &mut out);
        match out.as_slice() {
            [Out::Move { slot, at }] => {
                assert_eq!(*slot, FIRST_SLOT + 3);
                assert!((at.y() - 0.6).abs() < 1e-5);
            }
            other => panic!("ожидался один Move, пришло {other:?}"),
        }
        out.clear();
        e.tick(ms(100), &mut out);
        assert_eq!(
            out,
            vec![Out::Up {
                slot: FIRST_SLOT + 3
            }]
        );
        out.clear();
        e.tick(ms(200), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn release_all_lifts_every_touch_once() {
        let mut e = engine();
        let mut out = Vec::new();
        e.key_event(code("F"), true, ms(0), &mut out);
        e.key_event(code("C"), true, ms(0), &mut out);
        out.clear();
        e.release_all(&mut out);
        assert_eq!(
            out,
            vec![
                Out::Up { slot: FIRST_SLOT },
                Out::Up {
                    slot: FIRST_SLOT + 1
                }
            ]
        );
        out.clear();
        e.release_all(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn invalid_profile_is_refused() {
        let mut p = Profile::from_json(SAMPLE).expect("профиль");
        p.version = 9;
        assert!(Engine::new(&p).is_err());
    }
}
