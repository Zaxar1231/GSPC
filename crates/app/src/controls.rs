//! Профили управления в окне (этап 6): загрузка, выбор по активному приложению, поток движка.
//!
//! Движок ([`rustdroid_input::keymap`]) общий для UI-потока (события окна обрабатываются сразу,
//! без очереди) и потока `keymap`: тот выполняет запланированные жесты, раз в секунду
//! спрашивает у гостевого агента активное приложение и включает захват мыши для прицела.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustdroid_devices::TouchState;
use rustdroid_devices::agent::AgentHost;
use rustdroid_input::keymap::{HostEvent, Keymap, TouchOp};
use rustdroid_input::profile::Profile;
use rustdroid_present::window::WindowHandle;
use rustdroid_vmm::InputHandle;

use crate::cli::VmArgs;

/// Как часто спрашивать у агента активное приложение.
const FOREGROUND_EVERY: Duration = Duration::from_secs(1);
/// Самый долгий сон потока движка (захват мыши включается не позже).
const IDLE_SLEEP: Duration = Duration::from_millis(20);

/// Загружает профили: `--profile FILE` и все `*.rdprofile`/`*.json` из `--profiles DIR`.
///
/// # Errors
/// Файл не читается или профиль неверный (с именем файла).
pub(crate) fn load(vm: &VmArgs) -> Result<Vec<Profile>> {
    let mut files = Vec::new();
    if let Some(dir) = &vm.profiles {
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("не удалось открыть папку профилей {}", dir.display()))?;
        let mut found: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("rdprofile") || e == "json")
            })
            .collect();
        found.sort();
        files.extend(found);
    }
    files.extend(vm.profile.iter().cloned());
    files.iter().map(|p| read(p)).collect()
}

fn read(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("не удалось прочитать профиль {}", path.display()))?;
    Profile::from_json(&text).with_context(|| format!("профиль {}", path.display()))
}

/// Профиль для приложения `package`: свой (по `package`), иначе общий (без `package`).
fn select<'a>(profiles: &'a [Profile], package: &str) -> Option<&'a Profile> {
    profiles
        .iter()
        .find(|p| !package.is_empty() && p.package == package)
        .or_else(|| profiles.iter().find(|p| p.package.is_empty()))
}

struct Inner {
    profiles: Vec<Profile>,
    active: Option<Keymap>,
    package: String,
    aspect: f32,
}

/// Движок раскладки окна; клонируется между UI-потоком и потоком `keymap`.
#[derive(Clone)]
pub(crate) struct Controls {
    inner: Arc<Mutex<Inner>>,
    start: Instant,
    touch: Option<InputHandle>,
    state: Arc<Mutex<TouchState>>,
    size: (u32, u32),
}

impl Controls {
    /// Движок для экрана `size` (касания идут в `touch` через общий `state`).
    pub(crate) fn new(
        profiles: Vec<Profile>,
        size: (u32, u32),
        touch: Option<InputHandle>,
        state: Arc<Mutex<TouchState>>,
    ) -> Self {
        #[allow(clippy::cast_precision_loss, reason = "размеры экрана")]
        let aspect = size.0.max(1) as f32 / size.1.max(1) as f32;
        let active = select(&profiles, "").map(|p| {
            tracing::info!(profile = %p.name, "профиль управления");
            Keymap::new(p.clone(), aspect)
        });
        Self {
            inner: Arc::new(Mutex::new(Inner {
                profiles,
                active,
                package: String::new(),
                aspect,
            })),
            start: Instant::now(),
            touch,
            state,
            size,
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Событие окна → профиль. `true` — событие занято профилем (гостю его не передавать).
    pub(crate) fn handle(&self, ev: HostEvent<'_>) -> bool {
        let now = self.now_ms();
        let mut out = Vec::new();
        let used = self
            .lock()
            .active
            .as_mut()
            .is_some_and(|k| k.handle(ev, now, &mut out));
        self.apply(&out);
        used
    }

    /// Отпускает все пальцы профиля (окно потеряло фокус).
    pub(crate) fn release_all(&self) {
        let mut out = Vec::new();
        if let Some(k) = self.lock().active.as_mut() {
            k.release_all(&mut out);
        }
        self.apply(&out);
    }

    /// Активное приложение гостя сменилось: другой профиль — старый отпускает пальцы.
    fn set_package(&self, package: String) {
        let mut out = Vec::new();
        {
            let mut inner = self.lock();
            if inner.package == package {
                return;
            }
            let next = select(&inner.profiles, &package).cloned();
            let same = match (&inner.active, &next) {
                (Some(k), Some(p)) => k.profile() == p,
                (None, None) => true,
                _ => false,
            };
            inner.package.clone_from(&package);
            if !same {
                if let Some(k) = inner.active.as_mut() {
                    k.release_all(&mut out);
                }
                let aspect = inner.aspect;
                tracing::info!(
                    package = %package,
                    profile = next.as_ref().map_or("—", |p| p.name.as_str()),
                    "профиль управления"
                );
                inner.active = next.map(|p| Keymap::new(p, aspect));
            }
        }
        self.apply(&out);
    }

    /// Касания движка → события сенсорной панели гостя.
    fn apply(&self, ops: &[TouchOp]) {
        let Some(touch) = &self.touch else {
            return;
        };
        if ops.is_empty() {
            return;
        }
        let (w, h) = self.size;
        let mut events = Vec::new();
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for op in ops {
            events.extend(match *op {
                TouchOp::Down { slot, at } | TouchOp::Move { slot, at } => st.touch(
                    slot,
                    crate::display::to_panel(at.x(), w),
                    crate::display::to_panel(at.y(), h),
                ),
                TouchOp::Up { slot } => st.release(slot),
            });
        }
        drop(st);
        touch.push(&events);
    }

    /// Поток `keymap`: жесты по времени, профиль по активному приложению, захват мыши.
    pub(crate) fn spawn(self, win: WindowHandle, agent: Option<AgentHost>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("keymap".into())
            .spawn(move || {
                let mut captured = false;
                let mut next_fg = Instant::now();
                loop {
                    let now = self.now_ms();
                    let mut out = Vec::new();
                    let (deadline, capture) = {
                        let mut inner = self.lock();
                        match inner.active.as_mut() {
                            Some(k) => {
                                k.tick(now, &mut out);
                                (k.next_deadline(), k.captures_mouse())
                            }
                            None => (None, false),
                        }
                    };
                    self.apply(&out);
                    if capture != captured {
                        captured = capture;
                        win.set_mouse_capture(capture);
                    }
                    if let Some(agent) = &agent
                        && Instant::now() >= next_fg
                    {
                        next_fg = Instant::now() + FOREGROUND_EVERY;
                        self.set_package(agent.foreground());
                    }
                    let sleep = deadline.map_or(IDLE_SLEEP, |d| {
                        Duration::from_millis(d.saturating_sub(self.now_ms())).min(IDLE_SLEEP)
                    });
                    std::thread::sleep(sleep);
                }
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, package: &str) -> Profile {
        Profile::from_json(&format!(
            r#"{{"version":1,"name":"{name}","package":"{package}","controls":[]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn game_profile_wins_over_common() {
        let ps = vec![profile("общий", ""), profile("игра", "com.game")];
        assert_eq!(select(&ps, "com.game").unwrap().name, "игра");
        assert_eq!(select(&ps, "com.other").unwrap().name, "общий");
        assert_eq!(select(&ps, "").unwrap().name, "общий");
        assert!(select(&ps[1..], "com.other").is_none());
    }
}
