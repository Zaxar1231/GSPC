//! Движок контролов: клавиши, кнопки геймпада, курсор, колесо и время → события касаний.
//!
//! Чистая логика без потоков и часов: время передаёт вызывающий (монотонные `Duration`
//! от старта ВМ), клавиши — по имени (см. [`crate::profile::is_key`]).
//!
//! Слоты мультитача берутся из пула на время касания: контролов в профиле может быть больше,
//! чем слотов, но одновременно касаний не больше [`MAX_TOUCH_SLOTS`]. Если слотов нет,
//! касание пропускается (палец не ставится). Слоты 0 и 1 заняты мышью и колесом окна.
//!
//! Ограничения: прицел и скилл MOBA берут движение курсора как абсолютную позицию в окне
//! (захвата курсора и сырого ввода мыши пока нет); стики геймпада не поддержаны.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::geometry::NormPoint;
use crate::profile::{Control, HoldMode, MAX_TOUCH_SLOTS, MacroStep, Profile, ProfileError};

/// Первый слот, который отдаёт движок.
pub const FIRST_SLOT: u32 = 2;
/// Сколько касание держится в шаге макроса, мс.
pub const TAP_MS: u64 = 30;
/// Через сколько без щелчков колеса зум отпускается, мс.
const ZOOM_IDLE_MS: u128 = 400;
/// Расстояние пальцев зума: начальное, наименьшее и наибольшее, доля экрана.
const ZOOM_START: f32 = 0.05;
const ZOOM_MIN: f32 = 0.02;
const ZOOM_MAX: f32 = 0.45;

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

/// Пул свободных слотов: берётся наименьший.
#[derive(Debug)]
struct Pool {
    free: Vec<u32>,
}

impl Pool {
    fn new() -> Self {
        Self {
            free: (FIRST_SLOT..FIRST_SLOT + MAX_TOUCH_SLOTS).rev().collect(),
        }
    }

    fn take(&mut self) -> Option<u32> {
        self.free.pop()
    }

    fn give(&mut self, slot: u32) {
        self.free.push(slot);
        self.free.sort_unstable_by(|a, b| b.cmp(a));
    }

    fn len(&self) -> usize {
        self.free.len()
    }
}

/// Одно касание: слот, пока палец на экране.
#[derive(Debug, Default, Clone, Copy)]
struct Touch {
    slot: Option<u32>,
}

impl Touch {
    fn is_down(&self) -> bool {
        self.slot.is_some()
    }

    /// Ставит палец, если он ещё не стоит и есть свободный слот.
    fn down(&mut self, pool: &mut Pool, at: NormPoint, out: &mut Vec<Out>) {
        if self.slot.is_some() {
            return;
        }
        if let Some(slot) = pool.take() {
            self.slot = Some(slot);
            out.push(Out::Down { slot, at });
        }
    }

    fn move_to(&self, at: NormPoint, out: &mut Vec<Out>) {
        if let Some(slot) = self.slot {
            out.push(Out::Move { slot, at });
        }
    }

    /// Поднимает палец и возвращает слот в пул.
    fn up(&mut self, pool: &mut Pool, out: &mut Vec<Out>) {
        if let Some(slot) = self.slot.take() {
            pool.give(slot);
            out.push(Out::Up { slot });
        }
    }
}

/// Состояние контрола. Поля используются только нужными видами контролов.
#[derive(Debug, Default, Clone, Copy)]
struct Run {
    /// Клавиша контрола нажата (фронт нажатия — по изменению).
    held: bool,
    /// Вторая клавиша (отмена у MOBA).
    cancel_held: bool,
    /// Основное касание.
    t: Touch,
    /// Второе касание (зум).
    t2: Touch,
    /// Переключатель включён (hold-toggle, aim).
    toggled: bool,
    /// Начало повтора или свайпа.
    since: Duration,
    /// Направления джойстика: вверх, влево, вниз, вправо.
    dirs: [bool; 4],
    /// Текущая точка касания (прицел, джойстик, MOBA).
    pos: (f32, f32),
    /// Отмена скилла MOBA: касание уведено за пределы.
    cancel: bool,
    /// Шаг макроса и момент его окончания.
    macro_idx: usize,
    macro_end: Option<Duration>,
    /// Расстояние пальцев зума и последний щелчок.
    zoom_dist: f32,
    zoom_last: Duration,
}

#[derive(Debug)]
struct Bound {
    control: Control,
    run: Run,
}

/// Движок одного профиля.
#[derive(Debug)]
pub struct Engine {
    bound: Vec<Bound>,
    pool: Pool,
    gamepad: BTreeMap<String, String>,
    /// Последняя точка курсора в окне.
    pointer: Option<NormPoint>,
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Касание, пока клавиша нажата.
fn touch_while_held(
    run: &mut Run,
    pool: &mut Pool,
    at: NormPoint,
    pressed: bool,
    out: &mut Vec<Out>,
) {
    if pressed == run.held {
        return;
    }
    run.held = pressed;
    if pressed {
        run.t.down(pool, at, out);
    } else {
        run.t.up(pool, out);
    }
}

/// Шаг макроса: касание или пауза, начатые в `t`.
fn macro_begin(
    steps: &[MacroStep],
    run: &mut Run,
    pool: &mut Pool,
    t: Duration,
    out: &mut Vec<Out>,
) {
    match steps.get(run.macro_idx) {
        Some(MacroStep::Tap { tap }) => {
            run.t.down(pool, NormPoint::new(tap.x, tap.y), out);
            run.macro_end = Some(t + Duration::from_millis(TAP_MS));
        }
        Some(MacroStep::Wait { wait_ms }) => {
            run.macro_end = Some(t + Duration::from_millis(u64::from(*wait_ms)));
        }
        None => run.macro_end = None,
    }
}

impl Engine {
    /// Создаёт движок; профиль проверяется.
    ///
    /// # Errors
    /// Профиль не прошёл проверку.
    pub fn new(profile: &Profile) -> Result<Self, ProfileError> {
        profile.validate()?;
        Ok(Self {
            bound: profile
                .controls
                .iter()
                .map(|c| Bound {
                    control: c.clone(),
                    run: Run::default(),
                })
                .collect(),
            pool: Pool::new(),
            gamepad: profile.gamepad.clone(),
            pointer: None,
        })
    }

    /// Клавиша `name` нажата или отпущена в момент `now`. Повторы ОС (нажато дважды) игнорируются.
    pub fn key_event(&mut self, name: &str, pressed: bool, now: Duration, out: &mut Vec<Out>) {
        let Self { bound, pool, .. } = self;
        for b in bound.iter_mut() {
            let Bound { control, run } = b;
            match control {
                Control::Tap { key, x, y } | Control::Shoot { key, x, y } => {
                    if same(&key.0, name) {
                        touch_while_held(run, pool, NormPoint::new(*x, *y), pressed, out);
                    }
                }
                Control::Hold {
                    key,
                    x,
                    y,
                    mode: HoldMode::Hold,
                } => {
                    if same(&key.0, name) {
                        touch_while_held(run, pool, NormPoint::new(*x, *y), pressed, out);
                    }
                }
                Control::Hold {
                    key,
                    x,
                    y,
                    mode: HoldMode::Toggle,
                } => {
                    if same(&key.0, name) && pressed != run.held {
                        run.held = pressed;
                        if pressed {
                            run.toggled = !run.toggled;
                            if run.toggled {
                                run.t.down(pool, NormPoint::new(*x, *y), out);
                            } else {
                                run.t.up(pool, out);
                            }
                        }
                    }
                }
                Control::RepeatTap { key, x, y, .. } => {
                    if same(&key.0, name) && pressed != run.held {
                        run.held = pressed;
                        if pressed {
                            run.since = now;
                            run.t.down(pool, NormPoint::new(*x, *y), out);
                        } else {
                            run.t.up(pool, out);
                        }
                    }
                }
                Control::Swipe { key, from, .. } => {
                    if same(&key.0, name) && pressed != run.held {
                        // Свайп идёт до конца, отпускание клавиши его не прерывает.
                        run.held = pressed;
                        if pressed && !run.t.is_down() {
                            run.since = now;
                            run.t.down(pool, NormPoint::new(from.x, from.y), out);
                        }
                    }
                }
                Control::Joystick { keys, x, y, radius } => {
                    let idx = if same(&keys.up.0, name) {
                        0
                    } else if same(&keys.left.0, name) {
                        1
                    } else if same(&keys.down.0, name) {
                        2
                    } else if same(&keys.right.0, name) {
                        3
                    } else {
                        continue;
                    };
                    if run.dirs[idx] == pressed {
                        continue;
                    }
                    run.dirs[idx] = pressed;
                    let [up, left, down, right] = run.dirs;
                    let mut vx = f32::from(u8::from(right)) - f32::from(u8::from(left));
                    let mut vy = f32::from(u8::from(down)) - f32::from(u8::from(up));
                    if vx != 0.0 && vy != 0.0 {
                        vx *= std::f32::consts::FRAC_1_SQRT_2;
                        vy *= std::f32::consts::FRAC_1_SQRT_2;
                    }
                    if vx == 0.0 && vy == 0.0 {
                        run.t.up(pool, out);
                        continue;
                    }
                    let at = NormPoint::new(*x + vx * *radius, *y + vy * *radius);
                    run.pos = (at.x(), at.y());
                    if run.t.is_down() {
                        run.t.move_to(at, out);
                    } else {
                        run.t.down(pool, at, out);
                    }
                }
                Control::Aim {
                    toggle_key, x, y, ..
                } => {
                    if same(&toggle_key.0, name) && pressed != run.held {
                        run.held = pressed;
                        if pressed {
                            run.toggled = !run.toggled;
                            if run.toggled {
                                run.pos = (*x, *y);
                                run.t.down(pool, NormPoint::new(*x, *y), out);
                            } else {
                                run.t.up(pool, out);
                            }
                        }
                    }
                }
                Control::Macro { key, steps } => {
                    if same(&key.0, name) && pressed != run.held {
                        run.held = pressed;
                        // Макрос доигрывает и после отпускания клавиши.
                        if pressed && run.macro_end.is_none() {
                            run.macro_idx = 0;
                            macro_begin(steps, run, pool, now, out);
                        }
                    }
                }
                Control::MobaSkill {
                    key,
                    x,
                    y,
                    cancel_key,
                    ..
                } => {
                    if same(&key.0, name) && pressed != run.held {
                        run.held = pressed;
                        if pressed {
                            run.cancel = false;
                            run.pos = (*x, *y);
                            run.t.down(pool, NormPoint::new(*x, *y), out);
                        } else {
                            run.t.up(pool, out);
                        }
                    } else if same(&cancel_key.0, name) && pressed != run.cancel_held {
                        // Отмена: касание уходит в угол экрана, и отпускание не применяет скилл
                        // (его уберёт кромка: по отпусканию здесь та же отпускающая точка).
                        run.cancel_held = pressed;
                        if pressed && run.t.is_down() && !run.cancel {
                            run.cancel = true;
                            run.pos = (0.0, 0.0);
                            run.t.move_to(NormPoint::new(0.0, 0.0), out);
                        }
                    }
                }
                Control::Zoom { .. } => {}
            }
        }
    }

    /// Назначена ли клавиша `name` какому-нибудь контролу (окно тогда не шлёт её гостю).
    #[must_use]
    pub fn binds_key(&self, name: &str) -> bool {
        self.bound
            .iter()
            .any(|b| b.control.keys().iter().any(|k| same(&k.0, name)))
    }

    /// Есть ли в профиле зум: тогда колесо управляет им, а не прокручивает список.
    #[must_use]
    pub fn has_zoom(&self) -> bool {
        self.bound
            .iter()
            .any(|b| matches!(b.control, Control::Zoom { .. }))
    }

    /// Кнопка геймпада `button` (имя XInput) нажата или отпущена; назначение — из профиля.
    pub fn pad_event(&mut self, button: &str, pressed: bool, now: Duration, out: &mut Vec<Out>) {
        let Some(key) = self.gamepad.get(button).cloned() else {
            return;
        };
        self.key_event(&key, pressed, now, out);
    }

    /// Курсор в точке `at` (доли окна).
    pub fn pointer(&mut self, at: NormPoint, out: &mut Vec<Out>) {
        let prev = self.pointer.replace(at);
        let (dx, dy) = prev.map_or((0.0, 0.0), |p| (at.x() - p.x(), at.y() - p.y()));
        let Self { bound, .. } = self;
        for b in bound.iter_mut() {
            let Bound { control, run } = b;
            match control {
                Control::Aim {
                    sensitivity,
                    invert_y,
                    ..
                } if run.toggled && run.t.is_down() && (dx != 0.0 || dy != 0.0) => {
                    let sy = if *invert_y {
                        -sensitivity.y
                    } else {
                        sensitivity.y
                    };
                    run.pos = (
                        (run.pos.0 + dx * sensitivity.x).clamp(0.0, 1.0),
                        (run.pos.1 + dy * sy).clamp(0.0, 1.0),
                    );
                    run.t.move_to(NormPoint::new(run.pos.0, run.pos.1), out);
                }
                Control::MobaSkill { x, y, radius, .. } if run.t.is_down() && !run.cancel => {
                    // Направление — от центра скилла к курсору, длина не больше радиуса.
                    let (cx, cy, r) = (*x, *y, *radius);
                    let (mut ddx, mut ddy) = (at.x() - cx, at.y() - cy);
                    let len = ddx.hypot(ddy);
                    if len > r && len > 0.0 {
                        let k = r / len;
                        ddx *= k;
                        ddy *= k;
                    }
                    let to = NormPoint::new(cx + ddx, cy + ddy);
                    run.pos = (to.x(), to.y());
                    run.t.move_to(to, out);
                }
                _ => {}
            }
        }
    }

    /// Щелчки колеса: зум меняет расстояние между двумя касаниями. Первый щелчок ставит оба
    /// (если в пуле есть два слота).
    pub fn wheel(&mut self, notches: f32, now: Duration, out: &mut Vec<Out>) {
        if !notches.is_finite() || notches == 0.0 {
            return;
        }
        let Self { bound, pool, .. } = self;
        for b in bound.iter_mut() {
            let Bound { control, run } = b;
            let Control::Zoom { center, step } = control else {
                continue;
            };
            let starting = !run.t.is_down();
            if starting {
                if pool.len() < 2 {
                    continue;
                }
                run.zoom_dist = ZOOM_START;
            }
            run.zoom_dist = (run.zoom_dist + notches * *step).clamp(ZOOM_MIN, ZOOM_MAX);
            run.zoom_last = now;
            let d = run.zoom_dist;
            let left = NormPoint::new(center.x - d, center.y);
            let right = NormPoint::new(center.x + d, center.y);
            if starting {
                run.t.down(pool, left, out);
                run.t2.down(pool, right, out);
            } else {
                run.t.move_to(left, out);
                run.t2.move_to(right, out);
            }
        }
    }

    /// Продвигает повторы, свайпы, макросы и зум к моменту `now`.
    pub fn tick(&mut self, now: Duration, out: &mut Vec<Out>) {
        let Self { bound, pool, .. } = self;
        for b in bound.iter_mut() {
            let Bound { control, run } = b;
            match control {
                Control::RepeatTap {
                    x, y, interval_ms, ..
                } if run.held => {
                    let iv = u128::from((*interval_ms).max(1));
                    let elapsed = now.saturating_sub(run.since).as_millis();
                    if elapsed % iv < iv / 2 {
                        run.t.down(pool, NormPoint::new(*x, *y), out);
                    } else {
                        run.t.up(pool, out);
                    }
                }
                Control::Swipe {
                    from,
                    to,
                    duration_ms,
                    ..
                } if run.t.is_down() => {
                    let dur = u128::from((*duration_ms).max(1));
                    let elapsed = now.saturating_sub(run.since).as_millis();
                    if elapsed >= dur {
                        run.t.up(pool, out);
                    } else {
                        #[allow(clippy::cast_precision_loss, reason = "доля от 0 до 1")]
                        let t = elapsed as f32 / dur as f32;
                        let at = NormPoint::new(
                            from.x + (to.x - from.x) * t,
                            from.y + (to.y - from.y) * t,
                        );
                        run.t.move_to(at, out);
                    }
                }
                Control::Macro { steps, .. } => {
                    while let Some(end) = run.macro_end {
                        if now < end {
                            break;
                        }
                        run.t.up(pool, out);
                        run.macro_idx += 1;
                        macro_begin(steps, run, pool, end, out);
                    }
                }
                Control::Zoom { .. }
                    if run.t.is_down()
                        && now.saturating_sub(run.zoom_last).as_millis() > ZOOM_IDLE_MS =>
                {
                    run.t.up(pool, out);
                    run.t2.up(pool, out);
                }
                _ => {}
            }
        }
    }

    /// Отпускает все касания и сбрасывает состояние — при потере фокуса окном.
    pub fn release_all(&mut self, out: &mut Vec<Out>) {
        let Self { bound, pool, .. } = self;
        for b in bound.iter_mut() {
            b.run.t.up(pool, out);
            b.run.t2.up(pool, out);
            b.run = Run::default();
        }
        self.pointer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(controls: &str, pad: &str) -> Engine {
        let json =
            format!(r#"{{"version":1,"name":"t","controls":[{controls}],"gamepad":{{{pad}}}}}"#);
        let p = Profile::from_json(&json).expect("профиль");
        Engine::new(&p).expect("движок")
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn down(slot: u32, x: f32, y: f32) -> Out {
        Out::Down {
            slot,
            at: NormPoint::new(x, y),
        }
    }

    fn run(e: &mut Engine, f: impl FnOnce(&mut Engine, &mut Vec<Out>)) -> Vec<Out> {
        let mut out = Vec::new();
        f(e, &mut out);
        out
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn tap_follows_key_and_ignores_os_repeat() {
        let mut e = engine(r#"{"type":"tap","key":"F","x":0.8,"y":0.6}"#, "");
        assert_eq!(
            run(&mut e, |e, o| e.key_event("F", true, ms(0), o)),
            vec![down(2, 0.8, 0.6)]
        );
        assert!(run(&mut e, |e, o| e.key_event("F", true, ms(5), o)).is_empty());
        assert_eq!(
            run(&mut e, |e, o| e.key_event("F", false, ms(10), o)),
            vec![Out::Up { slot: 2 }]
        );
    }

    #[test]
    fn toggle_flips_on_press() {
        let mut e = engine(
            r#"{"type":"hold","key":"C","x":0.9,"y":0.85,"mode":"toggle"}"#,
            "",
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("C", true, ms(0), o)).len(),
            1
        );
        assert!(run(&mut e, |e, o| e.key_event("C", false, ms(10), o)).is_empty());
        assert_eq!(
            run(&mut e, |e, o| e.key_event("C", true, ms(20), o)),
            vec![Out::Up { slot: 2 }]
        );
    }

    #[test]
    fn repeat_alternates_by_interval() {
        let mut e = engine(
            r#"{"type":"repeat_tap","key":"E","x":0.7,"y":0.8,"interval_ms":50}"#,
            "",
        );
        run(&mut e, |e, o| e.key_event("E", true, ms(0), o));
        assert!(run(&mut e, |e, o| e.tick(ms(10), o)).is_empty());
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(30), o)),
            vec![Out::Up { slot: 2 }]
        );
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(55), o)),
            vec![down(2, 0.7, 0.8)]
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("E", false, ms(60), o)),
            vec![Out::Up { slot: 2 }]
        );
        assert!(run(&mut e, |e, o| e.tick(ms(200), o)).is_empty());
    }

    #[test]
    fn swipe_runs_to_the_end_after_release() {
        let mut e = engine(
            r#"{"type":"swipe","key":"Space","from":{"x":0.5,"y":0.8},"to":{"x":0.5,"y":0.4},"duration_ms":80}"#,
            "",
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("Space", true, ms(0), o)),
            vec![down(2, 0.5, 0.8)]
        );
        assert!(run(&mut e, |e, o| e.key_event("Space", false, ms(10), o)).is_empty());
        match run(&mut e, |e, o| e.tick(ms(40), o)).as_slice() {
            [Out::Move { slot: 2, at }] => assert!(near(at.y(), 0.6)),
            other => panic!("ожидался Move, пришло {other:?}"),
        }
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(100), o)),
            vec![Out::Up { slot: 2 }]
        );
        assert!(run(&mut e, |e, o| e.tick(ms(200), o)).is_empty());
    }

    #[test]
    fn joystick_moves_and_normalises_diagonals() {
        let mut e = engine(
            r#"{"type":"joystick","keys":{"up":"W","left":"A","down":"S","right":"D"},"x":0.2,"y":0.7,"radius":0.1}"#,
            "",
        );
        match run(&mut e, |e, o| e.key_event("W", true, ms(0), o)).as_slice() {
            [Out::Down { slot: 2, at }] => assert!(near(at.x(), 0.2) && near(at.y(), 0.6)),
            other => panic!("ожидался Down, пришло {other:?}"),
        }
        // W+D: диагональ, вектор ровно радиуса.
        match run(&mut e, |e, o| e.key_event("D", true, ms(0), o)).as_slice() {
            [Out::Move { slot: 2, at }] => {
                let (dx, dy) = (at.x() - 0.2, at.y() - 0.7);
                assert!((dx.hypot(dy) - 0.1).abs() < 1e-4, "{dx} {dy}");
            }
            other => panic!("ожидался Move, пришло {other:?}"),
        }
        // Отпустили W — остался вправо: полный радиус по X.
        match run(&mut e, |e, o| e.key_event("W", false, ms(0), o)).as_slice() {
            [Out::Move { slot: 2, at }] => assert!(near(at.x(), 0.3) && near(at.y(), 0.7)),
            other => panic!("ожидался Move, пришло {other:?}"),
        }
        assert_eq!(
            run(&mut e, |e, o| e.key_event("D", false, ms(0), o)),
            vec![Out::Up { slot: 2 }]
        );
    }

    #[test]
    fn aim_follows_pointer_with_sensitivity_and_invert() {
        let mut e = engine(
            r#"{"type":"aim","toggle_key":"LAlt","x":0.5,"y":0.5,"sensitivity":{"x":2.0,"y":1.0},"invert_y":true}"#,
            "",
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("LAlt", true, ms(0), o)).len(),
            1
        );
        // Первое положение курсора — только запоминается.
        assert!(run(&mut e, |e, o| e.pointer(NormPoint::new(0.4, 0.4), o)).is_empty());
        match run(&mut e, |e, o| e.pointer(NormPoint::new(0.45, 0.5), o)).as_slice() {
            [Out::Move { slot: 2, at }] => {
                // dx = 0.05 * 2 = 0.1; dy = +0.1 инвертировано → -0.1.
                assert!(near(at.x(), 0.6) && near(at.y(), 0.4), "{at:?}");
            }
            other => panic!("ожидался Move, пришло {other:?}"),
        }
        // Отпускание и новое нажатие выключают прицел.
        assert!(run(&mut e, |e, o| e.key_event("LAlt", false, ms(4), o)).is_empty());
        assert_eq!(
            run(&mut e, |e, o| e.key_event("LAlt", true, ms(5), o)),
            vec![Out::Up { slot: 2 }]
        );
        // Выключенный прицел не двигается.
        assert!(run(&mut e, |e, o| e.pointer(NormPoint::new(0.9, 0.9), o)).is_empty());
    }

    #[test]
    fn zoom_pinches_around_center() {
        let mut e = engine(
            r#"{"type":"zoom","key":"Wheel","center":{"x":0.5,"y":0.5},"step":0.05}"#,
            "",
        );
        let half = |outs: &[Out]| -> f32 {
            let xs: Vec<f32> = outs
                .iter()
                .filter_map(|o| match o {
                    Out::Move { at, .. } | Out::Down { at, .. } => Some(at.x()),
                    Out::Up { .. } => None,
                })
                .collect();
            (xs[1] - xs[0]) / 2.0
        };
        let first = run(&mut e, |e, o| e.wheel(1.0, ms(0), o));
        assert!(matches!(
            first.as_slice(),
            [Out::Down { slot: 2, .. }, Out::Down { slot: 3, .. }]
        ));
        assert!(near(half(&first), 0.1));
        let second = run(&mut e, |e, o| e.wheel(-5.0, ms(50), o));
        assert!(near(half(&second), ZOOM_MIN));
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(600), o)),
            vec![Out::Up { slot: 2 }, Out::Up { slot: 3 }]
        );
    }

    #[test]
    fn macro_runs_its_timeline_after_release() {
        let mut e = engine(
            r#"{"type":"macro","key":"Q","steps":[{"tap":{"x":0.6,"y":0.6}},{"wait_ms":120},{"tap":{"x":0.7,"y":0.6}}]}"#,
            "",
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("Q", true, ms(0), o)),
            vec![down(2, 0.6, 0.6)]
        );
        run(&mut e, |e, o| e.key_event("Q", false, ms(5), o));
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(30), o)),
            vec![Out::Up { slot: 2 }]
        );
        assert!(run(&mut e, |e, o| e.tick(ms(100), o)).is_empty());
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(150), o)),
            vec![down(2, 0.7, 0.6)]
        );
        assert_eq!(
            run(&mut e, |e, o| e.tick(ms(200), o)),
            vec![Out::Up { slot: 2 }]
        );
        assert!(run(&mut e, |e, o| e.tick(ms(900), o)).is_empty());
    }

    #[test]
    fn moba_skill_aims_and_cancels() {
        let mut e = engine(
            r#"{"type":"moba_skill","key":"1","x":0.8,"y":0.8,"radius":0.1,"cancel_key":"Escape"}"#,
            "",
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("1", true, ms(0), o)),
            vec![down(2, 0.8, 0.8)]
        );
        match run(&mut e, |e, o| e.pointer(NormPoint::new(1.0, 0.8), o)).as_slice() {
            [Out::Move { slot: 2, at }] => assert!(near(at.x(), 0.9)),
            other => panic!("ожидался Move, пришло {other:?}"),
        }
        assert_eq!(
            run(&mut e, |e, o| e.key_event("1", false, ms(10), o)),
            vec![Out::Up { slot: 2 }]
        );

        run(&mut e, |e, o| e.key_event("1", true, ms(20), o));
        assert_eq!(
            run(&mut e, |e, o| e.key_event("Escape", true, ms(25), o)),
            vec![Out::Move {
                slot: 2,
                at: NormPoint::new(0.0, 0.0)
            }]
        );
        assert_eq!(
            run(&mut e, |e, o| e.key_event("1", false, ms(30), o)),
            vec![Out::Up { slot: 2 }]
        );
    }

    #[test]
    fn gamepad_buttons_go_through_key_names() {
        let mut e = engine(
            r#"{"type":"tap","key":"Space","x":0.5,"y":0.5}"#,
            r#""A":"Space""#,
        );
        assert_eq!(
            run(&mut e, |e, o| e.pad_event("A", true, ms(0), o)),
            vec![down(2, 0.5, 0.5)]
        );
        assert_eq!(
            run(&mut e, |e, o| e.pad_event("A", false, ms(5), o)),
            vec![Out::Up { slot: 2 }]
        );
        assert!(run(&mut e, |e, o| e.pad_event("B", true, ms(5), o)).is_empty());
    }

    #[test]
    fn slots_are_reused_after_release() {
        // Контролов больше восьми, но одновременно касаний — не больше восьми.
        let controls: String = (1..=9)
            .map(|i| format!(r#"{{"type":"tap","key":"F{i}","x":0.5,"y":0.5}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let mut e = engine(&controls, "");
        let mut outs = Vec::new();
        for i in 1..=9 {
            e.key_event(&format!("F{i}"), true, ms(0), &mut outs);
        }
        let downs = outs
            .iter()
            .filter(|o| matches!(o, Out::Down { .. }))
            .count();
        assert_eq!(downs, 8, "девятое касание без слота пропускается");
        // Отпустили первое — слот 2 свободен и достаётся следующему.
        outs.clear();
        e.key_event("F1", false, ms(5), &mut outs);
        assert_eq!(outs, vec![Out::Up { slot: 2 }]);
        // F9 стояла без слота: отпустили и нажали снова — теперь слот есть.
        outs.clear();
        e.key_event("F9", false, ms(5), &mut outs);
        assert!(outs.is_empty());
        e.key_event("F9", true, ms(6), &mut outs);
        assert_eq!(outs, vec![down(2, 0.5, 0.5)]);
    }

    #[test]
    fn release_all_lifts_every_touch_once() {
        let mut e = engine(
            r#"{"type":"tap","key":"F","x":0.8,"y":0.6},{"type":"zoom","center":{"x":0.5,"y":0.5},"step":0.05}"#,
            "",
        );
        run(&mut e, |e, o| e.key_event("F", true, ms(0), o));
        run(&mut e, |e, o| e.wheel(1.0, ms(0), o));
        assert_eq!(
            run(&mut e, |e, o| e.release_all(o)),
            vec![
                Out::Up { slot: 2 },
                Out::Up { slot: 3 },
                Out::Up { slot: 4 }
            ]
        );
        assert!(run(&mut e, |e, o| e.release_all(o)).is_empty());
    }

    #[test]
    fn invalid_profile_is_refused() {
        let mut p = Profile::from_json(
            r#"{"version":1,"name":"t","controls":[{"type":"tap","key":"F","x":0.1,"y":0.1}]}"#,
        )
        .expect("профиль");
        p.version = 9;
        assert!(Engine::new(&p).is_err());
    }
}
