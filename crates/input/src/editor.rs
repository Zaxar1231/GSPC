//! Редактор профиля поверх игры: выбор, перетаскивание, создание, переназначение и удаление
//! контролов. Чистая логика; окно рисует [`Editor::markers`] и передаёт события мыши и клавиш.
//!
//! - нажатие на контрол — выбор и перетаскивание;
//! - нажатие на пустое место и клавиша — новый контрол `tap` на этой клавише;
//! - клавиша при выбранном контроле — переназначение, `Delete` — удаление, `Escape` — снять выбор;
//! - колесо над выбранным джойстиком или скиллом — радиус.

use crate::profile::{Control, MacroStep, Point, Profile};

/// Радиус попадания по контролу без своего радиуса (доля высоты экрана).
const HIT: f32 = 0.04;

/// Метка контрола для отрисовки (координаты и радиус — как в профиле).
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    /// Центр X (доля ширины).
    pub x: f32,
    /// Центр Y (доля высоты).
    pub y: f32,
    /// Радиус (доля высоты).
    pub radius: f32,
    /// Подпись (клавиша).
    pub label: String,
    /// Выбран.
    pub selected: bool,
}

/// Событие редактора.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditEvent<'a> {
    /// Левая кнопка нажата в точке.
    Press {
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Мышь с нажатой кнопкой.
    Drag {
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Левая кнопка отпущена.
    Release,
    /// Нажата клавиша (имя как в профиле).
    Key(&'a str),
    /// Колесо мыши.
    Wheel(f32),
}

/// Редактор одного профиля.
#[derive(Debug)]
pub struct Editor {
    profile: Profile,
    aspect: f32,
    selected: Option<usize>,
    /// Перетаскивание: смещение точки захвата от опорной точки контрола.
    grab: Option<(f32, f32)>,
    /// Пустое место, куда поставить новый контрол по следующей клавише.
    spot: Option<Point>,
    changed: bool,
}

impl Editor {
    /// Редактор `profile` на экране с соотношением сторон `aspect` (ширина / высота).
    #[must_use]
    pub fn new(profile: Profile, aspect: f32) -> Self {
        Self {
            profile,
            aspect: if aspect > 0.0 { aspect } else { 1.0 },
            selected: None,
            grab: None,
            spot: None,
            changed: false,
        }
    }

    /// Профиль с правками.
    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Были ли правки.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Обрабатывает событие; `true` — изменилось то, что надо перерисовать.
    pub fn handle(&mut self, ev: EditEvent<'_>) -> bool {
        match ev {
            EditEvent::Press { x, y } => {
                self.selected = self.hit(x, y);
                self.grab = self.selected.and_then(|i| {
                    let a = anchor(self.profile.controls.get(i)?)?;
                    Some((x - a.x, y - a.y))
                });
                self.spot = if self.selected.is_none() {
                    Some(Point { x, y })
                } else {
                    None
                };
                true
            }
            EditEvent::Drag { x, y } => {
                let (Some(i), Some((ox, oy))) = (self.selected, self.grab) else {
                    return false;
                };
                let Some(c) = self.profile.controls.get_mut(i) else {
                    return false;
                };
                let Some(a) = anchor(c) else {
                    return false;
                };
                let to = Point {
                    x: (x - ox).clamp(0.0, 1.0),
                    y: (y - oy).clamp(0.0, 1.0),
                };
                shift(c, to.x - a.x, to.y - a.y);
                self.changed = true;
                true
            }
            EditEvent::Release => {
                self.grab = None;
                false
            }
            EditEvent::Key(name) => self.key(name),
            EditEvent::Wheel(n) => {
                let Some(Control::Joystick { radius, .. } | Control::MobaSkill { radius, .. }) =
                    self.selected.and_then(|i| self.profile.controls.get_mut(i))
                else {
                    return false;
                };
                *radius = (*radius + 0.01 * n).clamp(0.02, 0.4);
                self.changed = true;
                true
            }
        }
    }

    fn key(&mut self, name: &str) -> bool {
        if name.eq_ignore_ascii_case("Escape") {
            self.selected = None;
            self.spot = None;
            return true;
        }
        if let Some(i) = self.selected {
            if name.eq_ignore_ascii_case("Delete") {
                self.profile.controls.remove(i);
                self.selected = None;
                self.changed = true;
                return true;
            }
            let Some(c) = self.profile.controls.get_mut(i) else {
                return false;
            };
            let slot = match c {
                Control::Tap { key, .. }
                | Control::Shoot { key, .. }
                | Control::Swipe { key, .. }
                | Control::RepeatTap { key, .. }
                | Control::Hold { key, .. }
                | Control::Zoom { key, .. }
                | Control::Macro { key, .. }
                | Control::MobaSkill { key, .. } => key,
                Control::Aim { toggle_key, .. } => toggle_key,
                // У джойстика четыре клавиши: их меняют в файле профиля.
                Control::Joystick { .. } => return false,
            };
            name.clone_into(slot);
            self.changed = true;
            return true;
        }
        let Some(p) = self.spot.take() else {
            return false;
        };
        self.profile.controls.push(Control::Tap {
            key: name.to_owned(),
            x: p.x,
            y: p.y,
        });
        self.selected = Some(self.profile.controls.len() - 1);
        self.changed = true;
        true
    }

    /// Контрол под точкой (ближайший из попавших).
    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.markers()
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let d = ((m.x - x) * self.aspect).hypot(m.y - y);
                (d <= m.radius.max(HIT)).then_some((i, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Метки всех контролов (по одной на контрол, в порядке профиля).
    #[must_use]
    pub fn markers(&self) -> Vec<Marker> {
        self.profile
            .controls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let a = anchor(c).unwrap_or(Point { x: 0.5, y: 0.5 });
                Marker {
                    x: a.x,
                    y: a.y,
                    radius: match c {
                        Control::Joystick { radius, .. } | Control::MobaSkill { radius, .. } => {
                            *radius
                        }
                        _ => HIT,
                    },
                    label: label(c),
                    selected: self.selected == Some(i),
                }
            })
            .collect()
    }
}

/// Опорная точка контрола (то, что тащит мышь).
fn anchor(c: &Control) -> Option<Point> {
    Some(match c {
        Control::Tap { x, y, .. }
        | Control::Joystick { x, y, .. }
        | Control::Aim { x, y, .. }
        | Control::Shoot { x, y, .. }
        | Control::RepeatTap { x, y, .. }
        | Control::Hold { x, y, .. }
        | Control::MobaSkill { x, y, .. } => Point { x: *x, y: *y },
        Control::Swipe { from, .. } => *from,
        Control::Zoom { center, .. } => *center,
        Control::Macro { steps, .. } => steps.iter().find_map(|s| match s {
            MacroStep::Tap(p) => Some(*p),
            MacroStep::WaitMs(_) => None,
        })?,
    })
}

/// Сдвигает все точки контрола.
fn shift(c: &mut Control, dx: f32, dy: f32) {
    let mv = |p: &mut Point| {
        p.x = (p.x + dx).clamp(0.0, 1.0);
        p.y = (p.y + dy).clamp(0.0, 1.0);
    };
    match c {
        Control::Tap { x, y, .. }
        | Control::Joystick { x, y, .. }
        | Control::Aim { x, y, .. }
        | Control::Shoot { x, y, .. }
        | Control::RepeatTap { x, y, .. }
        | Control::Hold { x, y, .. }
        | Control::MobaSkill { x, y, .. } => {
            *x = (*x + dx).clamp(0.0, 1.0);
            *y = (*y + dy).clamp(0.0, 1.0);
        }
        Control::Swipe { from, to, .. } => {
            mv(from);
            mv(to);
        }
        Control::Zoom { center, .. } => mv(center),
        Control::Macro { steps, .. } => {
            for s in steps {
                if let MacroStep::Tap(p) = s {
                    mv(p);
                }
            }
        }
    }
}

/// Подпись контрола.
fn label(c: &Control) -> String {
    match c {
        Control::Joystick { keys, .. } => {
            format!("{}{}{}{}", keys.up, keys.left, keys.down, keys.right)
        }
        Control::Aim { toggle_key, .. } => format!("AIM {toggle_key}"),
        Control::Zoom { .. } => "ZOOM".to_owned(),
        Control::Macro { key, .. } => format!("M {key}"),
        Control::Tap { key, .. }
        | Control::Shoot { key, .. }
        | Control::Swipe { key, .. }
        | Control::RepeatTap { key, .. }
        | Control::Hold { key, .. }
        | Control::MobaSkill { key, .. } => key.clone(),
    }
    .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{JoystickKeys, Orientation};

    fn editor(controls: Vec<Control>) -> Editor {
        Editor::new(
            Profile {
                version: 1,
                name: "t".into(),
                package: String::new(),
                orientation: Orientation::Landscape,
                controls,
            },
            2.0,
        )
    }

    fn tap(key: &str, x: f32, y: f32) -> Control {
        Control::Tap {
            key: key.into(),
            x,
            y,
        }
    }

    #[test]
    fn drag_moves_control_keeping_grab_offset() {
        let mut e = editor(vec![tap("F", 0.5, 0.5)]);
        assert!(e.handle(EditEvent::Press { x: 0.51, y: 0.5 }));
        assert!(e.markers()[0].selected);
        e.handle(EditEvent::Drag { x: 0.31, y: 0.2 });
        e.handle(EditEvent::Release);
        let m = &e.markers()[0];
        assert!((m.x - 0.3).abs() < 1e-5 && (m.y - 0.2).abs() < 1e-5);
        assert!(e.changed());
        // Без захвата перетаскивание ничего не двигает.
        assert!(!e.handle(EditEvent::Drag { x: 0.9, y: 0.9 }));
    }

    #[test]
    fn empty_spot_then_key_creates_tap() {
        let mut e = editor(vec![]);
        e.handle(EditEvent::Press { x: 0.7, y: 0.4 });
        assert!(e.handle(EditEvent::Key("G")));
        assert_eq!(e.profile().controls, vec![tap("G", 0.7, 0.4)]);
        assert!(e.markers()[0].selected);
        // Вторая клавиша переназначает только что созданный контрол.
        e.handle(EditEvent::Key("H"));
        assert_eq!(e.profile().controls, vec![tap("H", 0.7, 0.4)]);
    }

    #[test]
    fn rebind_delete_and_escape() {
        let mut e = editor(vec![tap("F", 0.2, 0.2), tap("E", 0.8, 0.8)]);
        e.handle(EditEvent::Press { x: 0.8, y: 0.8 });
        e.handle(EditEvent::Key("Q"));
        assert_eq!(e.markers()[1].label, "Q");
        e.handle(EditEvent::Key("Escape"));
        assert!(!e.handle(EditEvent::Key("Z")), "без выбора и места — ничего");
        e.handle(EditEvent::Press { x: 0.2, y: 0.2 });
        e.handle(EditEvent::Key("Delete"));
        assert_eq!(e.profile().controls, vec![tap("Q", 0.8, 0.8)]);
    }

    #[test]
    fn wheel_resizes_joystick_and_swipe_moves_both_ends() {
        let mut e = editor(vec![
            Control::Joystick {
                keys: JoystickKeys {
                    up: "W".into(),
                    left: "A".into(),
                    down: "S".into(),
                    right: "D".into(),
                },
                x: 0.2,
                y: 0.7,
                radius: 0.1,
            },
            Control::Swipe {
                key: "Space".into(),
                from: Point { x: 0.5, y: 0.8 },
                to: Point { x: 0.5, y: 0.4 },
                duration_ms: 80,
            },
        ]);
        assert_eq!(e.markers()[0].label, "WASD");
        // Попадание внутри радиуса джойстика, а не только в центр.
        e.handle(EditEvent::Press { x: 0.22, y: 0.75 });
        assert!(e.handle(EditEvent::Wheel(2.0)));
        assert!((e.markers()[0].radius - 0.12).abs() < 1e-5);
        e.handle(EditEvent::Press { x: 0.5, y: 0.8 });
        e.handle(EditEvent::Drag { x: 0.6, y: 0.7 });
        let Control::Swipe { from, to, .. } = &e.profile().controls[1] else {
            panic!("ожидался свайп");
        };
        assert!((from.x - 0.6).abs() < 1e-5 && (to.y - 0.3).abs() < 1e-5);
    }
}
