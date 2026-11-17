//! Keymap-движок: события ввода хоста → касания (мультитач) по профилю.
//!
//! Чистая логика: время передаётся снаружи (`now_ms`), поэтому поведение детерминировано и
//! покрывается тестами. Жесты со временем (свайп, щипок, макрос, повтор касаний) движок
//! планирует сам; хост вызывает [`Keymap::tick`] не позже [`Keymap::next_deadline`].
//!
//! Касания — в долях отображаемого экрана ([`NormPoint`]); слоты мультитача — с
//! [`FIRST_SLOT`] (ниже — мышь и колесо окна).

use crate::geometry::NormPoint;
use crate::profile::{Control, HoldMode, MacroStep, Point, Profile};

/// Первый слот мультитача движка (0 — левая кнопка мыши, 1 — колесо).
pub const FIRST_SLOT: u32 = 2;
/// Слотов у сенсорной панели гостя.
pub const SLOTS: u32 = 10;

/// Шаг анимации жестов, мс (~60 Гц).
const FRAME_MS: u64 = 16;
/// Длительность короткого касания макроса, мс.
const MACRO_TAP_MS: u64 = 30;
/// Длительность щипка колесом, мс.
const ZOOM_MS: u64 = 100;
/// Начальное расстояние пальцев щипка от центра (доля высоты).
const ZOOM_START: f32 = 0.1;
/// Палец прицела дальше этого от точки старта (доля высоты) — отпускается и ставится заново.
const AIM_REACH: f32 = 0.2;
/// Курсор на таком расстоянии от центра экрана (доля высоты) — скилл MOBA на полном радиусе.
const MOBA_FULL: f32 = 0.3;

/// Событие ввода хоста.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostEvent<'a> {
    /// Клавиша или кнопка мыши (`LButton`, `RButton`, `MButton`) по имени.
    Key {
        /// Имя (как в профиле, регистр не важен).
        name: &'a str,
        /// Нажата.
        pressed: bool,
    },
    /// Относительное движение мыши в долях экрана (для прицела).
    MouseMove {
        /// По горизонтали.
        dx: f32,
        /// По вертикали.
        dy: f32,
    },
    /// Положение курсора в долях экрана (для скиллов MOBA).
    Cursor {
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Колесо мыши: щелчки, положительные — от себя.
    Wheel {
        /// Щелчки.
        notches: f32,
    },
}

/// Касание для сенсорной панели гостя.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchOp {
    /// Палец опущен.
    Down {
        /// Слот.
        slot: u32,
        /// Точка.
        at: NormPoint,
    },
    /// Палец сдвинут.
    Move {
        /// Слот.
        slot: u32,
        /// Точка.
        at: NormPoint,
    },
    /// Палец поднят.
    Up {
        /// Слот.
        slot: u32,
    },
}

/// Состояние контрола между событиями.
#[derive(Debug, Clone, Copy, Default)]
enum State {
    #[default]
    Idle,
    /// Палец стоит (tap, hold, shoot, скилл MOBA).
    Finger(u32),
    /// Нажатые направления джойстика (вверх, влево, вниз, вправо) и его палец.
    Stick([bool; 4], Option<u32>),
    /// Повтор касаний: палец, следующее переключение, палец сейчас опущен.
    Repeat(u32, u64, bool),
    /// Скилл MOBA отменён: отпускание клавиши ничего не делает.
    Cancelled,
}

/// Запланированное касание.
#[derive(Debug, Clone, Copy)]
struct Timed {
    at: u64,
    op: TouchOp,
    /// После этого Up слот возвращается в пул (у макроса — только после последнего касания).
    release: bool,
}

/// Движок раскладки для одного профиля.
#[derive(Debug)]
pub struct Keymap {
    profile: Profile,
    /// Ширина / высота экрана: радиусы в долях высоты переводятся в доли ширины.
    aspect: f32,
    states: Vec<State>,
    free: Vec<u32>,
    timeline: Vec<Timed>,
    held: Vec<String>,
    aim: bool,
    aim_finger: Option<(u32, f32, f32)>,
    cursor: (f32, f32),
}

impl Keymap {
    /// Движок для `profile` на экране с соотношением сторон `aspect` (ширина / высота).
    #[must_use]
    pub fn new(profile: Profile, aspect: f32) -> Self {
        let states = vec![State::Idle; profile.controls.len()];
        Self {
            profile,
            aspect: if aspect > 0.0 { aspect } else { 1.0 },
            states,
            free: (FIRST_SLOT..SLOTS).rev().collect(),
            timeline: Vec::new(),
            held: Vec::new(),
            aim: false,
            aim_finger: None,
            cursor: (0.5, 0.5),
        }
    }

    /// Профиль движка.
    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Прицел включён: хост прячет и запирает курсор и шлёт [`HostEvent::MouseMove`].
    #[must_use]
    pub fn captures_mouse(&self) -> bool {
        self.aim
    }

    /// Когда вызвать [`Keymap::tick`] в следующий раз (`None` — нечего ждать).
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let repeat = self.states.iter().filter_map(|s| match s {
            State::Repeat(_, next, _) => Some(*next),
            _ => None,
        });
        self.timeline.iter().map(|t| t.at).chain(repeat).min()
    }

    /// Обрабатывает событие; касания — в `out`. Возвращает `true`, если событие занято
    /// профилем (тогда хост не передаёт его гостю как обычный ввод).
    pub fn handle(&mut self, ev: HostEvent<'_>, now_ms: u64, out: &mut Vec<TouchOp>) -> bool {
        match ev {
            HostEvent::Key { name, pressed } => self.key(name, pressed, now_ms, out),
            HostEvent::MouseMove { dx, dy } => self.mouse_move(dx, dy, out),
            HostEvent::Cursor { x, y } => {
                self.cursor = (x, y);
                self.update_moba(out);
                false
            }
            HostEvent::Wheel { notches } => self.wheel(notches, now_ms),
        }
    }

    /// Выполняет запланированное к моменту `now_ms`.
    pub fn tick(&mut self, now_ms: u64, out: &mut Vec<TouchOp>) {
        self.timeline.sort_by_key(|t| t.at);
        let due = self.timeline.iter().take_while(|t| t.at <= now_ms).count();
        for t in self.timeline.drain(..due).collect::<Vec<_>>() {
            if let (TouchOp::Up { slot }, true) = (t.op, t.release) {
                self.free.push(slot);
            }
            out.push(t.op);
        }
        for i in 0..self.states.len() {
            let Some(State::Repeat(slot, next, down)) = self.states.get(i).copied() else {
                continue;
            };
            if next > now_ms {
                continue;
            }
            let Some(Control::RepeatTap { x, y, interval_ms, .. }) =
                self.profile.controls.get(i)
            else {
                continue;
            };
            let half = u64::from((*interval_ms).max(2) / 2);
            out.push(if down {
                TouchOp::Up { slot }
            } else {
                TouchOp::Down {
                    slot,
                    at: NormPoint::new(*x, *y),
                }
            });
            self.states[i] = State::Repeat(slot, now_ms + half, !down);
        }
    }

    /// Отпускает все пальцы (смена профиля, окно потеряло фокус).
    pub fn release_all(&mut self, out: &mut Vec<TouchOp>) {
        let mut slots: Vec<u32> = self
            .timeline
            .iter()
            .map(|t| match t.op {
                TouchOp::Down { slot, .. } | TouchOp::Move { slot, .. } | TouchOp::Up { slot } => {
                    slot
                }
            })
            .collect();
        self.timeline.clear();
        for s in &mut self.states {
            match std::mem::take(s) {
                State::Finger(f) | State::Stick(_, Some(f)) | State::Repeat(f, _, _) => {
                    slots.push(f);
                }
                _ => {}
            }
        }
        if let Some((f, _, _)) = self.aim_finger.take() {
            slots.push(f);
        }
        slots.sort_unstable();
        slots.dedup();
        out.extend(slots.iter().map(|&slot| TouchOp::Up { slot }));
        self.free = (FIRST_SLOT..SLOTS).rev().collect();
        self.held.clear();
        self.aim = false;
    }

    fn alloc(&mut self) -> Option<u32> {
        self.free.pop()
    }

    fn put(&mut self, slot: u32, out: &mut Vec<TouchOp>) {
        out.push(TouchOp::Up { slot });
        self.free.push(slot);
    }

    fn key(&mut self, name: &str, pressed: bool, now: u64, out: &mut Vec<TouchOp>) -> bool {
        let already = self.held.iter().any(|h| h.eq_ignore_ascii_case(name));
        if pressed {
            if !already {
                self.held.push(name.to_owned());
            }
        } else {
            self.held.retain(|h| !h.eq_ignore_ascii_case(name));
        }
        let is = |k: &str| k.eq_ignore_ascii_case(name);
        let mut used = false;
        for i in 0..self.profile.controls.len() {
            let Some(c) = self.profile.controls.get(i).cloned() else {
                continue;
            };
            let bound = match &c {
                Control::Tap { key, .. }
                | Control::Swipe { key, .. }
                | Control::RepeatTap { key, .. }
                | Control::Hold { key, .. }
                | Control::Macro { key, .. } => is(key),
                Control::Shoot { key, .. } => is(key) && (self.aim || !pressed),
                Control::Joystick { keys, .. } => {
                    is(&keys.up) || is(&keys.left) || is(&keys.down) || is(&keys.right)
                }
                Control::Aim { toggle_key, .. } => is(toggle_key),
                // Отмена занимает клавишу, только пока скилл зажат (Esc — ещё и «Назад»).
                Control::MobaSkill {
                    key, cancel_key, ..
                } => {
                    is(key)
                        || (cancel_key.as_deref().is_some_and(is)
                            && matches!(self.states[i], State::Finger(_)))
                }
                Control::Zoom { .. } => false,
            };
            if !bound {
                continue;
            }
            // Шут без прицела: отпускание занято, только если палец действительно стоит.
            if matches!(c, Control::Shoot { .. })
                && !pressed
                && !matches!(self.states[i], State::Finger(_))
            {
                continue;
            }
            used = true;
            // Автоповтор клавиатуры Windows: повторные нажатия игнорируются.
            if pressed && already {
                continue;
            }
            self.apply(i, &c, name, pressed, now, out);
        }
        used
    }

    #[allow(clippy::too_many_lines, reason = "по ветке на тип контрола")]
    fn apply(
        &mut self,
        i: usize,
        c: &Control,
        name: &str,
        pressed: bool,
        now: u64,
        out: &mut Vec<TouchOp>,
    ) {
        let state = self.states[i];
        match c {
            Control::Tap { x, y, .. }
            | Control::Shoot { x, y, .. }
            | Control::Hold {
                x,
                y,
                mode: HoldMode::Hold,
                ..
            } => match (pressed, state) {
                (true, State::Idle) => {
                    if let Some(slot) = self.alloc() {
                        out.push(TouchOp::Down {
                            slot,
                            at: NormPoint::new(*x, *y),
                        });
                        self.states[i] = State::Finger(slot);
                    }
                }
                (false, State::Finger(slot)) => {
                    self.put(slot, out);
                    self.states[i] = State::Idle;
                }
                _ => {}
            },
            Control::Hold {
                x,
                y,
                mode: HoldMode::Toggle,
                ..
            } => {
                if !pressed {
                    return;
                }
                match state {
                    State::Finger(slot) => {
                        self.put(slot, out);
                        self.states[i] = State::Idle;
                    }
                    _ => {
                        if let Some(slot) = self.alloc() {
                            out.push(TouchOp::Down {
                                slot,
                                at: NormPoint::new(*x, *y),
                            });
                            self.states[i] = State::Finger(slot);
                        }
                    }
                }
            }
            Control::Joystick {
                keys, x, y, radius, ..
            } => {
                let (mut dirs, finger) = match state {
                    State::Stick(d, f) => (d, f),
                    _ => ([false; 4], None),
                };
                for (d, k) in [&keys.up, &keys.left, &keys.down, &keys.right]
                    .iter()
                    .enumerate()
                {
                    if k.eq_ignore_ascii_case(name) {
                        dirs[d] = pressed;
                    }
                }
                let vx = f32::from(u8::from(dirs[3])) - f32::from(u8::from(dirs[1]));
                let vy = f32::from(u8::from(dirs[2])) - f32::from(u8::from(dirs[0]));
                let len = vx.hypot(vy);
                let finger = if len == 0.0 {
                    if let Some(slot) = finger {
                        self.put(slot, out);
                    }
                    None
                } else {
                    let slot = match finger {
                        Some(s) => Some(s),
                        None => self.alloc().inspect(|&slot| {
                            out.push(TouchOp::Down {
                                slot,
                                at: NormPoint::new(*x, *y),
                            });
                        }),
                    };
                    if let Some(slot) = slot {
                        let at = NormPoint::new(
                            x + vx / len * radius / self.aspect,
                            y + vy / len * radius,
                        );
                        out.push(TouchOp::Move { slot, at });
                    }
                    slot
                };
                self.states[i] = State::Stick(dirs, finger);
            }
            Control::Aim { .. } => {
                if pressed {
                    self.aim = !self.aim;
                    if !self.aim
                        && let Some((slot, _, _)) = self.aim_finger.take()
                    {
                        self.put(slot, out);
                    }
                }
            }
            Control::Swipe {
                from,
                to,
                duration_ms,
                ..
            } => {
                if pressed && let Some(slot) = self.alloc() {
                    self.plan_swipe(slot, *from, *to, u64::from(*duration_ms), now, out);
                }
            }
            Control::RepeatTap { x, y, interval_ms, .. } => match (pressed, state) {
                (true, State::Idle) => {
                    if let Some(slot) = self.alloc() {
                        out.push(TouchOp::Down {
                            slot,
                            at: NormPoint::new(*x, *y),
                        });
                        let half = u64::from((*interval_ms).max(2) / 2);
                        self.states[i] = State::Repeat(slot, now + half, true);
                    }
                }
                (false, State::Repeat(slot, _, down)) => {
                    if down {
                        out.push(TouchOp::Up { slot });
                    }
                    self.free.push(slot);
                    self.states[i] = State::Idle;
                }
                _ => {}
            },
            Control::Macro { steps, .. } => {
                if pressed && let Some(slot) = self.alloc() {
                    self.plan_macro(slot, steps, now);
                }
            }
            Control::MobaSkill {
                key,
                x,
                y,
                cancel,
                ..
            } => {
                let is_key = key.eq_ignore_ascii_case(name);
                match (is_key, pressed, state) {
                    (true, true, State::Idle | State::Cancelled) => {
                        if let Some(slot) = self.alloc() {
                            out.push(TouchOp::Down {
                                slot,
                                at: NormPoint::new(*x, *y),
                            });
                            self.states[i] = State::Finger(slot);
                            self.update_moba(out);
                        }
                    }
                    (true, false, State::Finger(slot)) => {
                        self.put(slot, out);
                        self.states[i] = State::Idle;
                    }
                    (true, false, State::Cancelled) => self.states[i] = State::Idle,
                    (false, true, State::Finger(slot)) => {
                        if let Some(p) = cancel {
                            out.push(TouchOp::Move {
                                slot,
                                at: NormPoint::new(p.x, p.y),
                            });
                        }
                        self.put(slot, out);
                        self.states[i] = State::Cancelled;
                    }
                    _ => {}
                }
            }
            Control::Zoom { .. } => {}
        }
    }

    fn mouse_move(&mut self, dx: f32, dy: f32, out: &mut Vec<TouchOp>) -> bool {
        if !self.aim {
            return false;
        }
        let Some(Control::Aim {
            x,
            y,
            sensitivity,
            invert_y,
            ..
        }) = self
            .profile
            .controls
            .iter()
            .find(|c| matches!(c, Control::Aim { .. }))
            .cloned()
        else {
            return false;
        };
        let dy = if invert_y { -dy } else { dy };
        let (dx, dy) = (dx * sensitivity.x, dy * sensitivity.y);
        let (slot, px, py) = match self.aim_finger {
            Some(f) => f,
            None => {
                let Some(slot) = self.alloc() else {
                    return true;
                };
                out.push(TouchOp::Down {
                    slot,
                    at: NormPoint::new(x, y),
                });
                (slot, x, y)
            }
        };
        let (mut nx, mut ny) = (px + dx, py + dy);
        // Палец ушёл далеко или упёрся в край — переставляем его в точку старта, иначе
        // поворот камеры упрётся в границу экрана.
        let far = ((nx - x) * self.aspect).hypot(ny - y) > AIM_REACH;
        let edge = !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny);
        let slot = if far || edge {
            out.push(TouchOp::Up { slot });
            out.push(TouchOp::Down {
                slot,
                at: NormPoint::new(x, y),
            });
            nx = x + dx;
            ny = y + dy;
            slot
        } else {
            slot
        };
        out.push(TouchOp::Move {
            slot,
            at: NormPoint::new(nx, ny),
        });
        self.aim_finger = Some((slot, nx, ny));
        true
    }

    fn update_moba(&self, out: &mut Vec<TouchOp>) {
        for (c, s) in self.profile.controls.iter().zip(&self.states) {
            let (Control::MobaSkill { x, y, radius, .. }, State::Finger(slot)) = (c, s) else {
                continue;
            };
            let vx = (self.cursor.0 - 0.5) * self.aspect;
            let vy = self.cursor.1 - 0.5;
            let len = vx.hypot(vy);
            if len < f32::EPSILON {
                continue;
            }
            let k = (len / MOBA_FULL).min(1.0) * radius / len;
            out.push(TouchOp::Move {
                slot: *slot,
                at: NormPoint::new(x + vx * k / self.aspect, y + vy * k),
            });
        }
    }

    fn wheel(&mut self, notches: f32, now: u64) -> bool {
        let Some(Control::Zoom { center, step, .. }) = self
            .profile
            .controls
            .iter()
            .find(|c| matches!(c, Control::Zoom { key, .. } if key.eq_ignore_ascii_case("Wheel")))
            .cloned()
        else {
            return false;
        };
        let (Some(a), Some(b)) = (self.alloc(), self.alloc()) else {
            return true;
        };
        let end = (ZOOM_START + step * notches).clamp(0.02, 0.45);
        let at = |d: f32, sign: f32| NormPoint::new(center.x + sign * d / self.aspect, center.y);
        let steps = ZOOM_MS / FRAME_MS;
        for (slot, sign) in [(a, -1.0_f32), (b, 1.0)] {
            self.timeline.push(Timed {
                release: false,
                at: now,
                op: TouchOp::Down {
                    slot,
                    at: at(ZOOM_START, sign),
                },
            });
            for k in 1..=steps {
                #[allow(clippy::cast_precision_loss, reason = "шагов немного")]
                let t = k as f32 / steps as f32;
                self.timeline.push(Timed {
                    release: false,
                    at: now + k * FRAME_MS,
                    op: TouchOp::Move {
                        slot,
                        at: at(ZOOM_START + (end - ZOOM_START) * t, sign),
                    },
                });
            }
            self.timeline.push(Timed {
                release: true,
                at: now + steps * FRAME_MS + 1,
                op: TouchOp::Up { slot },
            });
        }
        true
    }

    fn plan_swipe(
        &mut self,
        slot: u32,
        from: Point,
        to: Point,
        duration: u64,
        now: u64,
        out: &mut Vec<TouchOp>,
    ) {
        out.push(TouchOp::Down {
            slot,
            at: NormPoint::new(from.x, from.y),
        });
        let steps = (duration / FRAME_MS).max(1);
        for k in 1..=steps {
            #[allow(clippy::cast_precision_loss, reason = "шагов немного")]
            let t = k as f32 / steps as f32;
            self.timeline.push(Timed {
                release: false,
                at: now + k * duration / steps,
                op: TouchOp::Move {
                    slot,
                    at: NormPoint::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t),
                },
            });
        }
        self.timeline.push(Timed {
            release: true,
            at: now + duration + 1,
            op: TouchOp::Up { slot },
        });
    }

    fn plan_macro(&mut self, slot: u32, steps: &[MacroStep], now: u64) {
        let mut t = now;
        for s in steps {
            match s {
                MacroStep::Tap(p) => {
                    self.timeline.push(Timed {
                        release: false,
                        at: t,
                        op: TouchOp::Down {
                            slot,
                            at: NormPoint::new(p.x, p.y),
                        },
                    });
                    t += MACRO_TAP_MS;
                    self.timeline.push(Timed {
                        release: false,
                        at: t,
                        op: TouchOp::Up { slot },
                    });
                    t += 1;
                }
                MacroStep::WaitMs(ms) => t += u64::from(*ms),
            }
        }
        // Слот возвращается в пул после последнего касания (или сразу, если касаний нет).
        match self.timeline.iter_mut().rev().find(|x| x.op == TouchOp::Up { slot }) {
            Some(last) => last.release = true,
            None => self.free.push(slot),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{JoystickKeys, Sensitivity};

    fn map(controls: Vec<Control>) -> Keymap {
        Keymap::new(
            Profile {
                version: 1,
                name: "t".into(),
                package: String::new(),
                orientation: crate::profile::Orientation::Landscape,
                controls,
            },
            2.0,
        )
    }

    /// Касания в сравнимом виде: координаты округлены до 1e-4 (точное равенство f32 хрупкое).
    fn r(ops: &[TouchOp]) -> Vec<(char, u32, i32, i32)> {
        let q = |at: NormPoint| ((at.x() * 1e4).round() as i32, (at.y() * 1e4).round() as i32);
        ops.iter()
            .map(|o| match *o {
                TouchOp::Down { slot, at } => ('d', slot, q(at).0, q(at).1),
                TouchOp::Move { slot, at } => ('m', slot, q(at).0, q(at).1),
                TouchOp::Up { slot } => ('u', slot, 0, 0),
            })
            .collect()
    }

    macro_rules! same {
        ($a:expr, [$($b:expr),* $(,)?]) => {
            assert_eq!(r(&$a), r(&[$($b),*]))
        };
    }

    fn key(k: &mut Keymap, name: &str, pressed: bool, now: u64) -> (bool, Vec<TouchOp>) {
        let mut out = Vec::new();
        let used = k.handle(HostEvent::Key { name, pressed }, now, &mut out);
        (used, out)
    }

    fn tick(k: &mut Keymap, now: u64) -> Vec<TouchOp> {
        let mut out = Vec::new();
        k.tick(now, &mut out);
        out
    }

    fn down(slot: u32, x: f32, y: f32) -> TouchOp {
        TouchOp::Down {
            slot,
            at: NormPoint::new(x, y),
        }
    }

    fn mv(slot: u32, x: f32, y: f32) -> TouchOp {
        TouchOp::Move {
            slot,
            at: NormPoint::new(x, y),
        }
    }

    fn up(slot: u32) -> TouchOp {
        TouchOp::Up { slot }
    }

    #[test]
    fn tap_holds_finger_while_key_down_and_ignores_autorepeat() {
        let mut k = map(vec![Control::Tap {
            key: "F".into(),
            x: 0.8,
            y: 0.6,
        }]);
        let (used, out) = key(&mut k, "f", true, 0);
        assert!(used);
        same!(out, [down(2, 0.8, 0.6)]);
        same!(key(&mut k, "F", true, 30).1, []);
        same!(key(&mut k, "F", false, 60).1, [up(2)]);
        assert_eq!(key(&mut k, "G", true, 70), (false, vec![]));
    }

    #[test]
    fn joystick_moves_finger_by_direction() {
        let mut k = map(vec![Control::Joystick {
            keys: JoystickKeys {
                up: "W".into(),
                left: "A".into(),
                down: "S".into(),
                right: "D".into(),
            },
            x: 0.2,
            y: 0.7,
            radius: 0.1,
        }]);
        // Радиус в долях высоты: по горизонтали на экране 2:1 — вдвое меньше.
        same!(key(&mut k, "D", true, 0).1, [down(2, 0.2, 0.7), mv(2, 0.25, 0.7)]);
        let out = key(&mut k, "W", true, 10).1;
        let Some(TouchOp::Move { at, .. }) = out.first().copied() else {
            panic!("ожидалось движение");
        };
        assert!(at.x() > 0.2 && at.y() < 0.7, "вверх-вправо");
        key(&mut k, "D", false, 20);
        same!(key(&mut k, "W", false, 30).1, [up(2)]);
    }

    #[test]
    fn aim_captures_mouse_and_shoot_works_only_in_aim() {
        let mut k = map(vec![
            Control::Aim {
                toggle_key: "LAlt".into(),
                x: 0.6,
                y: 0.5,
                sensitivity: Sensitivity { x: 1.0, y: 1.0 },
                invert_y: false,
            },
            Control::Shoot {
                key: "LButton".into(),
                x: 0.9,
                y: 0.5,
            },
        ]);
        // Без прицела левая кнопка — обычная мышь.
        assert_eq!(key(&mut k, "LButton", true, 0), (false, vec![]));
        assert_eq!(key(&mut k, "LButton", false, 5), (false, vec![]));
        let mut out = Vec::new();
        assert!(!k.handle(HostEvent::MouseMove { dx: 0.01, dy: 0.0 }, 0, &mut out));

        key(&mut k, "LAlt", true, 10);
        assert!(k.captures_mouse());
        assert!(k.handle(HostEvent::MouseMove { dx: 0.01, dy: 0.02 }, 20, &mut out));
        same!(out, [down(2, 0.6, 0.5), mv(2, 0.61, 0.52)]);
        // Далеко ушли — палец переставляется в точку старта.
        out.clear();
        k.handle(HostEvent::MouseMove { dx: 0.0, dy: 0.25 }, 30, &mut out);
        same!(out, [up(2), down(2, 0.6, 0.5), mv(2, 0.6, 0.75)]);

        let (used, out) = key(&mut k, "LButton", true, 40);
        assert!(used);
        same!(out, [down(3, 0.9, 0.5)]);
        key(&mut k, "LButton", false, 50);
        key(&mut k, "LAlt", false, 55);
        same!(key(&mut k, "LAlt", true, 60).1, [up(2)]);
        assert!(!k.captures_mouse());
    }

    #[test]
    fn swipe_is_animated_and_frees_slot() {
        let mut k = map(vec![Control::Swipe {
            key: "Space".into(),
            from: Point { x: 0.5, y: 0.8 },
            to: Point { x: 0.5, y: 0.4 },
            duration_ms: 64,
        }]);
        same!(key(&mut k, "Space", true, 1000).1, [down(2, 0.5, 0.8)]);
        assert_eq!(k.next_deadline(), Some(1016));
        same!(tick(&mut k, 1032), [mv(2, 0.5, 0.7), mv(2, 0.5, 0.6)]);
        same!(tick(&mut k, 2000), [mv(2, 0.5, 0.5), mv(2, 0.5, 0.4), up(2)]);
        assert_eq!(k.next_deadline(), None);
        assert_eq!(k.free.len(), 8);
    }

    #[test]
    fn repeat_tap_toggles_until_release() {
        let mut k = map(vec![Control::RepeatTap {
            key: "E".into(),
            x: 0.7,
            y: 0.8,
            interval_ms: 50,
        }]);
        same!(key(&mut k, "E", true, 0).1, [down(2, 0.7, 0.8)]);
        same!(tick(&mut k, 25), [up(2)]);
        same!(tick(&mut k, 50), [down(2, 0.7, 0.8)]);
        same!(key(&mut k, "E", false, 60).1, [up(2)]);
        assert_eq!(k.next_deadline(), None);
    }

    #[test]
    fn hold_toggle_switches_on_press() {
        let mut k = map(vec![Control::Hold {
            key: "C".into(),
            x: 0.9,
            y: 0.85,
            mode: HoldMode::Toggle,
        }]);
        same!(key(&mut k, "C", true, 0).1, [down(2, 0.9, 0.85)]);
        same!(key(&mut k, "C", false, 10).1, []);
        same!(key(&mut k, "C", true, 20).1, [up(2)]);
    }

    #[test]
    fn wheel_zoom_pinches_two_fingers() {
        let mut k = map(vec![Control::Zoom {
            key: "Wheel".into(),
            center: Point { x: 0.5, y: 0.5 },
            step: 0.05,
        }]);
        let mut out = Vec::new();
        assert!(k.handle(HostEvent::Wheel { notches: 2.0 }, 0, &mut out));
        let ops = tick(&mut k, 1000);
        let downs = ops
            .iter()
            .filter(|o| matches!(o, TouchOp::Down { .. }))
            .count();
        let ups = ops.iter().filter(|o| matches!(o, TouchOp::Up { .. })).count();
        assert_eq!((downs, ups), (2, 2));
        // Пальцы разошлись: 0.1 + 0.05 * 2 = 0.2 высоты, по горизонтали на 2:1 — 0.1.
        let last = ops
            .iter()
            .rev()
            .find_map(|o| match o {
                TouchOp::Move { slot: 3, at } => Some(*at),
                _ => None,
            })
            .unwrap();
        assert!((last.x() - 0.6).abs() < 1e-4);
        assert_eq!(k.free.len(), 8);
    }

    #[test]
    fn macro_taps_in_sequence_and_frees_slot_once() {
        let mut k = map(vec![Control::Macro {
            key: "Q".into(),
            steps: vec![
                MacroStep::Tap(Point { x: 0.6, y: 0.6 }),
                MacroStep::WaitMs(120),
                MacroStep::Tap(Point { x: 0.7, y: 0.6 }),
            ],
        }]);
        key(&mut k, "Q", true, 0);
        same!(tick(&mut k, 0), [down(2, 0.6, 0.6)]);
        same!(tick(&mut k, 30), [up(2)]);
        assert_eq!(k.free.len(), 7, "слот ещё занят макросом");
        same!(tick(&mut k, 151), [down(2, 0.7, 0.6)]);
        same!(tick(&mut k, 181), [up(2)]);
        assert_eq!(k.free.len(), 8);
    }

    #[test]
    fn moba_skill_aims_by_cursor_and_cancels() {
        let mut k = map(vec![Control::MobaSkill {
            key: "1".into(),
            x: 0.8,
            y: 0.8,
            radius: 0.1,
            cancel_key: Some("Escape".into()),
            cancel: Some(Point { x: 0.9, y: 0.2 }),
        }]);
        // Без зажатого скилла Esc свободен («Назад»).
        assert_eq!(key(&mut k, "Escape", true, 0), (false, vec![]));
        key(&mut k, "Escape", false, 1);
        let mut out = Vec::new();
        k.handle(HostEvent::Cursor { x: 0.5, y: 0.9 }, 0, &mut out);
        // Курсор ниже центра на 0.4 (> 0.3) — полный радиус вниз.
        same!(key(&mut k, "1", true, 10).1, [down(2, 0.8, 0.8), mv(2, 0.8, 0.9)]);
        let (used, out) = key(&mut k, "Escape", true, 20);
        assert!(used);
        same!(out, [mv(2, 0.9, 0.2), up(2)]);
        same!(key(&mut k, "1", false, 30).1, []);
    }

    #[test]
    fn release_all_lifts_every_finger() {
        let mut k = map(vec![
            Control::Tap {
                key: "F".into(),
                x: 0.1,
                y: 0.1,
            },
            Control::Swipe {
                key: "Space".into(),
                from: Point { x: 0.5, y: 0.8 },
                to: Point { x: 0.5, y: 0.4 },
                duration_ms: 80,
            },
        ]);
        key(&mut k, "F", true, 0);
        key(&mut k, "Space", true, 0);
        let mut out = Vec::new();
        k.release_all(&mut out);
        same!(out, [up(2), up(3)]);
        assert_eq!(k.next_deadline(), None);
        assert_eq!(key(&mut k, "F", true, 10).1.len(), 1);
    }

    #[test]
    fn slots_run_out_gracefully() {
        let names: Vec<String> = (1..=9).map(|i| format!("F{i}")).collect();
        let controls = names
            .iter()
            .map(|n| Control::Tap {
                key: n.clone(),
                x: 0.1,
                y: 0.1,
            })
            .collect();
        let mut k = map(controls);
        let downs: usize = names.iter().map(|n| key(&mut k, n, true, 0).1.len()).sum();
        assert_eq!(downs, 8, "свободных слотов 8");
    }
}
