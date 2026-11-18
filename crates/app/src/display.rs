//! Режим с окном: virtio-gpu + virtio-input, D3D12-композитор и ввод мышью/клавиатурой.
//!
//! Потоки:
//! - UI (текущий): окно и цикл сообщений; ввод сразу уходит в virtio-input;
//! - `vm`: vCPU гостя;
//! - `present`: композитор, ждёт кадр гостя и выводит его, не завися от UI.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustdroid_devices::input::key_events;
use rustdroid_devices::{InputKind, TouchState};
use rustdroid_gfxstream::{Config as GfxConfig, ffi::flags as gfx_flags};
use rustdroid_gpu::gfxstream::GfxstreamBackend;
use rustdroid_gpu::{DisplayMode, FenceSink, Mailbox};
use rustdroid_input::keys::{KEY_BACK, linux_keycode, nav_shortcut};
use rustdroid_input::profile::key_name;
use rustdroid_input::{Engine, NormPoint, Out, Profile};
use rustdroid_present::d3d12::PresentMode;
use rustdroid_present::run::PresentConfig;
use rustdroid_present::window::{self, WindowEvent};
use rustdroid_vmm::{ExitStats, InputHandle, RunOutcome, Vm, VmDevices};

use crate::cli::{DisplaySpec, GpuMode, VmArgs};

/// Параметры bootconfig для программного рендера в госте (как `gpu_mode=guest_swiftshader`
/// у Cuttlefish): gralloc minigbm, hwcomposer ranchu через DRM virtio-gpu, ANGLE/SwiftShader.
/// Любой параметр можно переопределить через `--set`.
pub(crate) const GUEST_SWIFTSHADER: &[(&str, &str)] = &[
    ("androidboot.hardware.gralloc", "minigbm"),
    ("androidboot.hardware.hwcomposer", "ranchu"),
    ("androidboot.hardware.hwcomposer.mode", "client"),
    ("androidboot.hardware.hwcomposer.display_finder_mode", "drm"),
    ("androidboot.hardware.egl", "angle"),
    ("androidboot.hardware.vulkan", "pastel"),
    ("androidboot.opengles.version", "196609"),
    // Vulkan 1.2 в SwiftShader (pastel), как у Cuttlefish в режиме guest_swiftshader.
    ("androidboot.cpuvulkan.version", "4202496"),
    ("androidboot.lcd_density", "240"),
];

/// Параметры bootconfig для 3D через gfxstream (как `gpu_mode=gfxstream_guest_angle` у
/// Cuttlefish): GLES в госте — ANGLE поверх Vulkan, Vulkan — драйвер gfxstream (`ranchu`),
/// команды идут в virtio-gpu; композиция — клиентская (SurfaceFlinger через ANGLE), кадр — через
/// DRM virtio-gpu.
pub(crate) const GFXSTREAM_GUEST_ANGLE: &[(&str, &str)] = &[
    ("androidboot.hardware.gralloc", "minigbm"),
    ("androidboot.hardware.hwcomposer", "ranchu"),
    ("androidboot.hardware.hwcomposer.mode", "client"),
    ("androidboot.hardware.hwcomposer.display_finder_mode", "drm"),
    (
        "androidboot.hardware.hwcomposer.display_framebuffer_format",
        "rgba",
    ),
    ("androidboot.hardware.egl", "angle"),
    ("androidboot.hardware.vulkan", "ranchu"),
    ("androidboot.hardware.gltransport", "virtio-gpu-asg"),
    ("androidboot.opengles.version", "196609"),
    ("androidboot.cpuvulkan.version", "0"),
    ("androidboot.lcd_density", "240"),
];

/// Параметры bootconfig для 3D через gfxstream на GLES хоста (уровень 2, видеокарты без
/// Vulkan; как `gpu_mode=gfxstream` у Cuttlefish): GLES в госте — драйвер gfxstream (`emulation`),
/// команды GLES идут в virtio-gpu и исполняются на хосте через ANGLE поверх Direct3D 11. GLES 3.0:
/// ANGLE на D3D11 даёт 3.0 (3.1 — не на всех видеокартах). Vulkan гостя (`ranchu`) без Vulkan
/// хоста видеокарт не находит, приложения рисуют через GLES.
pub(crate) const GFXSTREAM_GUEST_GLES: &[(&str, &str)] = &[
    ("androidboot.hardware.gralloc", "minigbm"),
    ("androidboot.hardware.hwcomposer", "ranchu"),
    ("androidboot.hardware.hwcomposer.mode", "client"),
    ("androidboot.hardware.hwcomposer.display_finder_mode", "drm"),
    (
        "androidboot.hardware.hwcomposer.display_framebuffer_format",
        "rgba",
    ),
    ("androidboot.hardware.egl", "emulation"),
    ("androidboot.hardware.vulkan", "ranchu"),
    ("androidboot.hardware.gltransport", "virtio-gpu-asg"),
    ("androidboot.opengles.version", "196608"),
    ("androidboot.cpuvulkan.version", "0"),
    ("androidboot.lcd_density", "240"),
];

/// Параметры bootconfig гостя для выбранной графики.
pub(crate) fn guest_bootconfig(gpu: GpuMode) -> &'static [(&'static str, &'static str)] {
    match gpu {
        GpuMode::Software => GUEST_SWIFTSHADER,
        GpuMode::Gfxstream => GFXSTREAM_GUEST_ANGLE,
        GpuMode::GfxstreamGl => GFXSTREAM_GUEST_GLES,
    }
}

/// Флаги рендерера gfxstream для режима 3D (`None` — без gfxstream).
pub(crate) fn renderer_flags(gpu: GpuMode) -> Option<u32> {
    match gpu {
        GpuMode::Software => None,
        GpuMode::Gfxstream => Some(gfx_flags::USE_VK | gfx_flags::USE_SURFACELESS),
        // USE_EGL: GLES хоста — ANGLE (libEGL/libGLESv2 рядом с rustdroid.exe) поверх D3D11.
        // OpenGL драйвера через WGL не годится: шейдеры GLSL ES без транслятора ANGLE не
        // компилируются (см. docs/stage-4.md, 4.6).
        GpuMode::GfxstreamGl => {
            Some(gfx_flags::USE_EGL | gfx_flags::USE_GLES | gfx_flags::USE_SURFACELESS)
        }
    }
}

/// Экран, подключённый к ВМ.
#[derive(Debug)]
pub(crate) struct Screen {
    spec: DisplaySpec,
    mailbox: Arc<Mailbox>,
}

/// Добавляет virtio-gpu, сенсорный экран и клавиатуру.
///
/// # Errors
/// С `--gpu gfxstream`: нет DLL или gfxstream не запустился. Запасного пути нет намеренно:
/// параметры гостя уже выбраны под gfxstream, и программный рендер с ними не заработает.
pub(crate) fn attach(devices: &mut VmDevices, spec: DisplaySpec, gpu: GpuMode) -> Result<Screen> {
    let mailbox = Mailbox::new();
    let mode = DisplayMode {
        width: spec.width,
        height: spec.height,
        refresh_hz: spec.refresh_hz,
    };
    devices.gpu = Some((mode, Arc::clone(&mailbox)));
    if let Some(flags) = renderer_flags(gpu) {
        let path = crate::gpu_probe::library_path()
            .context("не удалось определить каталог rustdroid.exe")?;
        if gpu == GpuMode::GfxstreamGl {
            // Без DLL ANGLE gfxstream не падает с ошибкой, а обращается по нулевому указателю.
            let missing = crate::gpu_probe::missing_angle(&path);
            if !missing.is_empty() {
                anyhow::bail!(
                    "для --gpu gfxstream-gl нужны DLL ANGLE рядом с rustdroid.exe, нет: {} \
                     (их кладёт hw-test.ps1 из pre-release deps-angle)",
                    missing.join(", ")
                );
            }
        }
        let fences = FenceSink::new();
        let backend = GfxstreamBackend::new(
            &path,
            GfxConfig {
                flags,
                width: spec.width,
                height: spec.height,
            },
            Arc::clone(&fences),
        )
        .with_context(|| {
            format!(
                "gfxstream не запустился ({}). Проверьте `rustdroid check` или запустите с --gpu software",
                path.display()
            )
        })?;
        devices.gpu_3d = Some((Box::new(backend), fences));
    }
    devices.inputs.push((
        InputKind::Touchscreen {
            width: spec.width,
            height: spec.height,
            slots: 10,
        },
        "RustDroid Touchscreen".into(),
        Arc::default(),
    ));
    devices.inputs.push((
        InputKind::Keyboard,
        "RustDroid Keyboard".into(),
        Arc::default(),
    ));
    tracing::info!(
        width = spec.width,
        height = spec.height,
        hz = spec.refresh_hz,
        ?gpu,
        "дисплей гостя"
    );
    Ok(Screen { spec, mailbox })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "доли экрана → пиксели"
)]
fn to_panel(frac: f32, size: u32) -> u32 {
    (frac.clamp(0.0, 1.0) * size.saturating_sub(1) as f32).round() as u32
}

/// Слот мультитача левой кнопки мыши.
const MOUSE_SLOT: u32 = 0;
/// Шаг потока профиля управления (повторы, свайпы, макросы), мс.
const KEYMAP_TICK_MS: u64 = 5;

/// Читает профиль управления и строит движок.
fn load_engine(path: &std::path::Path) -> Result<Engine> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("не удалось прочитать профиль {}", path.display()))?;
    let profile = Profile::from_json(&text)
        .map_err(|e| anyhow::anyhow!("профиль {}: {e}", path.display()))?;
    tracing::info!(name = %profile.name, controls = profile.controls.len(), "профиль управления");
    Engine::new(&profile).map_err(|e| anyhow::anyhow!("профиль {}: {e}", path.display()))
}

/// Отправляет гостю касания, которые выдал движок. Вызывать, держа блокировку движка: порядок
/// событий (отпускание слота раньше его повторного нажатия) сохраняется только так.
fn apply(out: &[Out], state: &Mutex<TouchState>, touch: &InputHandle, w: u32, h: u32) {
    if out.is_empty() {
        return;
    }
    let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
    let mut events = Vec::new();
    for o in out {
        match *o {
            Out::Down { slot, at } | Out::Move { slot, at } => {
                events.extend(st.touch(slot, to_panel(at.x(), w), to_panel(at.y(), h)));
            }
            Out::Up { slot } => events.extend(st.release(slot)),
        }
    }
    drop(st);
    if !events.is_empty() {
        touch.push(&events);
    }
}

/// Опрос первого геймпада (XInput): изменения кнопок идут в движок по назначению профиля.
#[cfg(windows)]
fn pad_loop(
    eng: &Mutex<Engine>,
    state: &Mutex<TouchState>,
    touch: &InputHandle,
    w: u32,
    h: u32,
    stop: &AtomicBool,
    t0: Instant,
) {
    use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};
    let mut prev = 0u16;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(KEYMAP_TICK_MS));
        let mut pad = XINPUT_STATE::default();
        // SAFETY: XInputGetState пишет в структуру `pad`, живую на время вызова.
        let rc = unsafe { XInputGetState(0, &raw mut pad) };
        if rc != 0 {
            // Геймпада нет (или он отключён): отпускать уже нечего.
            prev = 0;
            continue;
        }
        let now = pad.Gamepad.wButtons.0;
        for (name, pressed) in rustdroid_input::gamepad::changes(prev, now) {
            drive(eng, state, touch, w, h, |g, o| {
                g.pad_event(name, pressed, t0.elapsed(), o)
            });
        }
        prev = now;
    }
}

/// Выполняет `f` над движком и отправляет результат гостю.
fn drive(
    eng: &Mutex<Engine>,
    state: &Mutex<TouchState>,
    touch: &InputHandle,
    w: u32,
    h: u32,
    f: impl FnOnce(&mut Engine, &mut Vec<Out>),
) {
    let mut g = eng.lock().unwrap_or_else(PoisonError::into_inner);
    let mut out = Vec::new();
    f(&mut g, &mut out);
    apply(&out, state, touch, w, h);
}
/// Слот «пальца» прокрутки колесом: отдельный, чтобы не мешать нажатой кнопке.
const WHEEL_SLOT: u32 = 1;

/// Поток прокрутки колесом: щелчки, пришедшие во время жеста, складываются в следующий жест
/// (см. [`rustdroid_input::wheel`]).
fn wheel_loop(
    rx: &Receiver<(f32, f32, f32)>,
    touch: &rustdroid_vmm::InputHandle,
    state: &Mutex<TouchState>,
    w: u32,
    h: u32,
) {
    while let Ok((x, mut y, mut notches)) = rx.recv() {
        // Всё, что накопилось, — одним жестом (курсор — последний).
        while let Ok((_, ny, n)) = rx.try_recv() {
            y = ny;
            notches += n;
        }
        for step in rustdroid_input::wheel::plan(y, notches) {
            if step.delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(step.delay_ms));
            }
            let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
            let events = match step.y {
                Some(fy) => st.touch(WHEEL_SLOT, to_panel(x, w), to_panel(fy, h)),
                None => st.release(WHEEL_SLOT),
            };
            drop(st);
            if !events.is_empty() {
                touch.push(&events);
            }
        }
    }
}

/// Запускает ВМ с окном; возвращается, когда ВМ остановилась или окно закрыто.
pub(crate) fn run(machine: Vm, screen: Screen, vm: &VmArgs) -> Result<(RunOutcome, ExitStats)> {
    window::enable_dpi_awareness();
    let inputs = machine.inputs();
    let touch = inputs.first().cloned();
    let keyboard = inputs.get(1).cloned();
    let stopper = machine.stopper();
    let (w, h) = (screen.spec.width, screen.spec.height);

    let touch_state = Arc::new(Mutex::new(TouchState::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let engine: Option<Arc<Mutex<Engine>>> = match vm.profile.as_deref() {
        Some(path) => Some(Arc::new(Mutex::new(load_engine(path)?))),
        None => None,
    };
    if let (Some(eng), Some(t)) = (engine.clone(), touch.clone()) {
        let (state, stop_flag) = (Arc::clone(&touch_state), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("keymap".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(KEYMAP_TICK_MS));
                    drive(&eng, &state, &t, w, h, |g, o| g.tick(t0.elapsed(), o));
                }
            })
            .context("не удалось запустить поток профиля управления")?;
        #[cfg(windows)]
        if let (Some(eng), Some(t)) = (engine.clone(), touch.clone()) {
            let (state, stop_flag) = (Arc::clone(&touch_state), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("gamepad".into())
                .spawn(move || pad_loop(&eng, &state, &t, w, h, &stop_flag, t0))
                .context("не удалось запустить поток геймпада")?;
        }
    }
    let wheel = touch.clone().and_then(|t| {
        let (tx, rx) = mpsc::channel();
        let state = Arc::clone(&touch_state);
        std::thread::Builder::new()
            .name("wheel".into())
            .spawn(move || wheel_loop(&rx, &t, &state, w, h))
            .map_err(|e| tracing::warn!(error = %e, "поток прокрутки колесом не запущен"))
            .ok()
            .map(|_| tx)
    });
    let close_stopper = stopper.clone();
    let handler = move |ev: WindowEvent| match ev {
        WindowEvent::Pointer { x, y, down } => {
            if let Some(t) = &touch {
                let mut st = touch_state.lock().unwrap_or_else(PoisonError::into_inner);
                let events = if down {
                    st.touch(MOUSE_SLOT, to_panel(x, w), to_panel(y, h))
                } else {
                    st.release(MOUSE_SLOT)
                };
                drop(st);
                if !events.is_empty() {
                    t.push(&events);
                }
            }
            if let (Some(eng), Some(t)) = (&engine, &touch) {
                drive(eng, &touch_state, t, w, h, |g, o| {
                    g.key_event("LButton", down, t0.elapsed(), o);
                });
            }
        }
        WindowEvent::Hover { x, y } => {
            if let (Some(eng), Some(t)) = (&engine, &touch) {
                drive(eng, &touch_state, t, w, h, |g, o| {
                    g.pointer(NormPoint::new(x, y), o);
                });
            }
        }
        WindowEvent::FocusLost => {
            if let Some(t) = &touch {
                let mut st = touch_state.lock().unwrap_or_else(PoisonError::into_inner);
                let events = st.release(MOUSE_SLOT);
                drop(st);
                if !events.is_empty() {
                    t.push(&events);
                }
            }
            if let (Some(eng), Some(t)) = (&engine, &touch) {
                drive(eng, &touch_state, t, w, h, |g, o| g.release_all(o));
            }
        }
        WindowEvent::Wheel { x, y, notches } => {
            let zoom = engine.as_ref().is_some_and(|eng| {
                eng.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .has_zoom()
            });
            if zoom {
                if let (Some(eng), Some(t)) = (&engine, &touch) {
                    drive(eng, &touch_state, t, w, h, |g, o| {
                        g.wheel(notches, t0.elapsed(), o);
                    });
                }
            } else if let Some(tx) = &wheel {
                let _ = tx.send((x, y, notches));
            }
        }
        WindowEvent::Key {
            scancode,
            extended,
            pressed,
        } => {
            // Клавиша, назначенная профилем, гостю не уходит.
            let name = if extended { None } else { key_name(scancode) };
            if let (Some(eng), Some(t), Some(n)) = (&engine, &touch, name) {
                let bound = eng
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .binds_key(n);
                if bound {
                    drive(eng, &touch_state, t, w, h, |g, o| {
                        g.key_event(n, pressed, t0.elapsed(), o);
                    });
                    return;
                }
            }
            // Esc/F2/F3 — «Назад»/«Домой»/«Недавние»: у образа нет кнопок навигации на экране.
            let code =
                nav_shortcut(scancode, extended).or_else(|| linux_keycode(scancode, extended));
            if let (Some(k), Some(code)) = (&keyboard, code) {
                k.push(&key_events(code, pressed));
            }
        }
        WindowEvent::Secondary { down } => {
            if let (Some(eng), Some(t)) = (&engine, &touch) {
                drive(eng, &touch_state, t, w, h, |g, o| {
                    g.key_event("RButton", down, t0.elapsed(), o);
                });
            }
            if let Some(k) = &keyboard {
                k.push(&key_events(KEY_BACK, down));
            }
        }
        WindowEvent::Close => close_stopper.stop(),
    };
    let title = format!("RustDroid — {w}x{h} @ {} Гц", screen.spec.refresh_hz);
    let win = window::create(&title, w, h, handler).context("не удалось создать окно")?;

    let cfg = PresentConfig {
        mode: if vm.no_vsync {
            PresentMode::Immediate
        } else {
            PresentMode::Vsync
        },
        fps_limit: vm.fps_limit,
        overlay: !vm.no_overlay,
        refresh_hz: screen.spec.refresh_hz,
    };
    let load = machine.vcpu_load();
    let vcpus = load.cpus();
    let guest: rustdroid_present::usage::GuestRuntime =
        (Box::new(move || load.total_runtime_100ns()), vcpus);
    let present = {
        let (mailbox, stop) = (Arc::clone(&screen.mailbox), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("present".into())
            .spawn(move || rustdroid_present::run::run(win, &mailbox, &stop, cfg, Some(guest)))
            .context("не удалось запустить поток композитора")?
    };
    let vm_thread = std::thread::Builder::new()
        .name("vm".into())
        .spawn(move || {
            let r = machine.run();
            win.close();
            r
        })
        .context("не удалось запустить поток ВМ")?;

    window::run_message_loop();
    stop.store(true, Ordering::Release);
    stopper.stop();
    if present.join().is_err() {
        tracing::error!("поток композитора завершился аварийно");
    }
    let r = vm_thread
        .join()
        .map_err(|_| anyhow::anyhow!("поток ВМ завершился аварийно"))?;
    r.map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_mapping_covers_full_range() {
        assert_eq!(to_panel(0.0, 1080), 0);
        assert_eq!(to_panel(1.0, 1080), 1079);
        assert_eq!(to_panel(0.5, 1081), 540);
        assert_eq!(to_panel(2.0, 100), 99);
    }

    fn get(cfg: &[(&str, &str)], key: &str) -> Option<String> {
        cfg.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn gpu_modes_choose_guest_driver_and_host_api() {
        let gl = guest_bootconfig(GpuMode::GfxstreamGl);
        assert_eq!(
            get(gl, "androidboot.hardware.egl").as_deref(),
            Some("emulation")
        );
        assert_eq!(
            get(gl, "androidboot.opengles.version").as_deref(),
            Some("196608")
        );
        let vk = guest_bootconfig(GpuMode::Gfxstream);
        assert_eq!(
            get(vk, "androidboot.hardware.egl").as_deref(),
            Some("angle")
        );

        assert_eq!(renderer_flags(GpuMode::Software), None);
        let gl_flags = renderer_flags(GpuMode::GfxstreamGl).unwrap_or(0);
        assert_ne!(gl_flags & gfx_flags::USE_GLES, 0);
        assert_ne!(gl_flags & gfx_flags::USE_EGL, 0);
        assert_eq!(gl_flags & gfx_flags::USE_VK, 0);
        let vk_flags = renderer_flags(GpuMode::Gfxstream).unwrap_or(0);
        assert_ne!(vk_flags & gfx_flags::USE_VK, 0);
    }
}
