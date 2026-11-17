//! Профили управления в окне (этап 6): загрузка, выбор по активному приложению, поток движка,
//! редактор поверх игры (F12).
//!
//! Движок ([`rustdroid_input::keymap`]) общий для UI-потока (события окна обрабатываются сразу,
//! без очереди) и потока `keymap`: тот выполняет запланированные жесты, раз в секунду
//! спрашивает у гостевого агента активное приложение и включает захват мыши для прицела.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustdroid_devices::TouchState;
use rustdroid_devices::agent::AgentHost;
use rustdroid_input::editor::{EditEvent, Editor};
use rustdroid_input::keymap::{HostEvent, Keymap, TouchOp};
use rustdroid_input::profile::{Orientation, Profile, VERSION};
use rustdroid_present::run::{Marker, Markers};
use rustdroid_present::window::WindowHandle;
use rustdroid_vmm::InputHandle;

use crate::cli::VmArgs;

/// Как часто спрашивать у агента активное приложение.
const FOREGROUND_EVERY: Duration = Duration::from_secs(1);
/// Самый долгий сон потока движка (захват мыши включается не позже).
const IDLE_SLEEP: Duration = Duration::from_millis(20);
/// Куда сохранить новый профиль, если не заданы ни `--profile`, ни `--profiles`.
const FALLBACK_FILE: &str = "rustdroid.rdprofile";

/// Профиль и файл, из которого он прочитан (туда же сохраняются правки).
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    profile: Profile,
    path: PathBuf,
}

/// Где хранить профили.
#[derive(Debug, Clone, Default)]
pub(crate) struct Store {
    entries: Vec<Entry>,
    dir: Option<PathBuf>,
}

fn empty_profile(name: &str, package: &str) -> Profile {
    Profile {
        version: VERSION,
        name: name.to_owned(),
        package: package.to_owned(),
        orientation: Orientation::Landscape,
        controls: Vec::new(),
    }
}

/// Загружает профили: все `*.rdprofile`/`*.json` из `--profiles DIR` и `--profile FILE`
/// (если файла ещё нет — пустой профиль, редактор создаст его).
///
/// # Errors
/// Файл не читается или профиль неверный (с именем файла).
pub(crate) fn load(vm: &VmArgs) -> Result<Store> {
    let mut entries = Vec::new();
    if let Some(dir) = &vm.profiles {
        let list = std::fs::read_dir(dir)
            .with_context(|| format!("не удалось открыть папку профилей {}", dir.display()))?;
        let mut found: Vec<_> = list
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("rdprofile") || e == "json")
            })
            .collect();
        found.sort();
        for path in found {
            entries.push(Entry {
                profile: read(&path)?,
                path,
            });
        }
    }
    if let Some(path) = &vm.profile {
        let profile = if path.exists() {
            read(path)?
        } else {
            let name = path.file_stem().unwrap_or_default().to_string_lossy();
            empty_profile(&name, "")
        };
        entries.push(Entry {
            profile,
            path: path.clone(),
        });
    }
    Ok(Store {
        entries,
        dir: vm.profiles.clone(),
    })
}

fn read(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("не удалось прочитать профиль {}", path.display()))?;
    Profile::from_json(&text).with_context(|| format!("профиль {}", path.display()))
}

/// Профиль для приложения `package`: свой (по `package`), иначе общий (без `package`).
fn select(entries: &[Entry], package: &str) -> Option<usize> {
    entries
        .iter()
        .position(|e| !package.is_empty() && e.profile.package == package)
        .or_else(|| entries.iter().position(|e| e.profile.package.is_empty()))
}

struct Inner {
    store: Store,
    /// Активный профиль: индекс в `store.entries` и его движок.
    active: Option<(usize, Keymap)>,
    package: String,
    aspect: f32,
    editor: Option<Editor>,
}

impl Inner {
    fn activate(&mut self, idx: Option<usize>, out: &mut Vec<TouchOp>) {
        if let Some((_, k)) = self.active.as_mut() {
            k.release_all(out);
        }
        let aspect = self.aspect;
        self.active = idx.and_then(|i| {
            let p = self.store.entries.get(i)?.profile.clone();
            tracing::info!(profile = %p.name, package = %self.package, "профиль управления");
            Some((i, Keymap::new(p, aspect)))
        });
    }
}

/// Движок раскладки окна; клонируется между UI-потоком и потоком `keymap`.
#[derive(Clone)]
pub(crate) struct Controls {
    inner: Arc<Mutex<Inner>>,
    start: Instant,
    touch: Option<InputHandle>,
    state: Arc<Mutex<TouchState>>,
    size: (u32, u32),
    markers: Arc<Markers>,
}

impl Controls {
    /// Движок для экрана `size` (касания идут в `touch` через общий `state`).
    pub(crate) fn new(
        store: Store,
        size: (u32, u32),
        touch: Option<InputHandle>,
        state: Arc<Mutex<TouchState>>,
        markers: Arc<Markers>,
    ) -> Self {
        #[allow(clippy::cast_precision_loss, reason = "размеры экрана")]
        let aspect = size.0.max(1) as f32 / size.1.max(1) as f32;
        let first = select(&store.entries, "");
        let mut inner = Inner {
            store,
            active: None,
            package: String::new(),
            aspect,
            editor: None,
        };
        inner.activate(first, &mut Vec::new());
        Self {
            inner: Arc::new(Mutex::new(inner)),
            start: Instant::now(),
            touch,
            state,
            size,
            markers,
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
            .is_some_and(|(_, k)| k.handle(ev, now, &mut out));
        self.apply(&out);
        used
    }

    /// Отпускает все пальцы профиля (окно потеряло фокус).
    pub(crate) fn release_all(&self) {
        let mut out = Vec::new();
        if let Some((_, k)) = self.lock().active.as_mut() {
            k.release_all(&mut out);
        }
        self.apply(&out);
    }

    /// Открыт ли редактор.
    pub(crate) fn editing(&self) -> bool {
        self.lock().editor.is_some()
    }

    /// F12: открыть редактор активного профиля (или нового — для текущей игры) или закрыть
    /// его с сохранением правок в файл профиля.
    pub(crate) fn toggle_editor(&self) {
        let mut out = Vec::new();
        let marks = {
            let mut inner = self.lock();
            match inner.editor.take() {
                None => {
                    if let Some((_, k)) = inner.active.as_mut() {
                        k.release_all(&mut out);
                    }
                    let profile = match &inner.active {
                        Some((i, _)) => inner.store.entries[*i].profile.clone(),
                        None => {
                            let name = if inner.package.is_empty() {
                                "Профиль"
                            } else {
                                inner.package.as_str()
                            };
                            empty_profile(name, &inner.package)
                        }
                    };
                    tracing::info!(profile = %profile.name, "редактор управления открыт (F12 — закрыть)");
                    let editor = Editor::new(profile, inner.aspect);
                    let marks = to_marks(&editor);
                    inner.editor = Some(editor);
                    marks
                }
                Some(editor) => {
                    if editor.changed() {
                        Self::save(&mut inner, editor.profile().clone(), &mut out);
                    }
                    tracing::info!("редактор управления закрыт");
                    Vec::new()
                }
            }
        };
        self.markers.set(marks);
        self.apply(&out);
    }

    /// Сохраняет профиль из редактора в его файл и делает активным.
    fn save(inner: &mut Inner, profile: Profile, out: &mut Vec<TouchOp>) {
        let idx = match &inner.active {
            Some((i, _)) => *i,
            None => {
                let file = format!(
                    "{}.rdprofile",
                    if profile.package.is_empty() {
                        "profile"
                    } else {
                        profile.package.as_str()
                    }
                );
                let path = inner
                    .store
                    .dir
                    .as_ref()
                    .map_or_else(|| PathBuf::from(FALLBACK_FILE), |d| d.join(file));
                inner.store.entries.push(Entry {
                    profile: profile.clone(),
                    path,
                });
                inner.store.entries.len() - 1
            }
        };
        let entry = &mut inner.store.entries[idx];
        entry.profile = profile;
        match std::fs::write(&entry.path, entry.profile.to_json()) {
            Ok(()) => tracing::info!(path = %entry.path.display(), "профиль управления сохранён"),
            Err(e) => {
                tracing::error!(path = %entry.path.display(), error = %e, "профиль не сохранён");
            }
        }
        inner.activate(Some(idx), out);
    }

    /// Событие окна в режиме редактора (метки перерисовываются сразу).
    pub(crate) fn edit(&self, ev: EditEvent<'_>) {
        let marks = {
            let mut inner = self.lock();
            let Some(editor) = inner.editor.as_mut() else {
                return;
            };
            if !editor.handle(ev) {
                return;
            }
            to_marks(editor)
        };
        self.markers.set(marks);
    }

    /// Активное приложение гостя сменилось: другой профиль — старый отпускает пальцы.
    fn set_package(&self, package: String) {
        let mut out = Vec::new();
        {
            let mut inner = self.lock();
            if inner.package == package || inner.editor.is_some() {
                return;
            }
            let next = select(&inner.store.entries, &package);
            inner.package = package;
            if next != inner.active.as_ref().map(|(i, _)| *i) {
                inner.activate(next, &mut out);
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
                        let editing = inner.editor.is_some();
                        match inner.active.as_mut() {
                            Some((_, k)) => {
                                k.tick(now, &mut out);
                                (k.next_deadline(), k.captures_mouse() && !editing)
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

/// Метки редактора для композитора.
fn to_marks(editor: &Editor) -> Vec<Marker> {
    editor
        .markers()
        .into_iter()
        .map(|m| Marker {
            x: m.x,
            y: m.y,
            radius: m.radius,
            label: m.label,
            selected: m.selected,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, package: &str) -> Entry {
        Entry {
            profile: empty_profile(name, package),
            path: PathBuf::from(format!("{name}.rdprofile")),
        }
    }

    #[test]
    fn game_profile_wins_over_common() {
        let es = vec![entry("общий", ""), entry("игра", "com.game")];
        assert_eq!(select(&es, "com.game"), Some(1));
        assert_eq!(select(&es, "com.other"), Some(0));
        assert_eq!(select(&es, ""), Some(0));
        assert_eq!(select(&es[1..], "com.other"), None);
    }
}
