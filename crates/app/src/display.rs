//! Режим с окном: virtio-gpu + virtio-input, D3D12-композитор и ввод мышью/клавиатурой.
//!
//! Потоки:
//! - UI (текущий): окно и цикл сообщений; ввод сразу уходит в virtio-input;
//! - `vm`: vCPU гостя;
//! - `present`: композитор, ждёт кадр гостя и выводит его, не завися от UI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use rustdroid_devices::input::key_events;
use rustdroid_devices::{InputKind, TouchState};
use rustdroid_gpu::{DisplayMode, Mailbox};
use rustdroid_input::keys::linux_keycode;
use rustdroid_present::d3d12::PresentMode;
use rustdroid_present::run::PresentConfig;
use rustdroid_present::window::{self, WindowEvent};
use rustdroid_vmm::{ExitStats, RunOutcome, Vm, VmDevices};

use crate::cli::{DisplaySpec, VmArgs};

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

/// Экран, подключённый к ВМ.
#[derive(Debug)]
pub(crate) struct Screen {
    spec: DisplaySpec,
    mailbox: Arc<Mailbox>,
}

/// Добавляет virtio-gpu, сенсорный экран и клавиатуру.
pub(crate) fn attach(devices: &mut VmDevices, spec: DisplaySpec) -> Screen {
    let mailbox = Mailbox::new();
    let mode = DisplayMode {
        width: spec.width,
        height: spec.height,
        refresh_hz: spec.refresh_hz,
    };
    devices.gpu = Some((mode, Arc::clone(&mailbox)));
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
        "дисплей гостя"
    );
    Screen { spec, mailbox }
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

/// Запускает ВМ с окном; возвращается, когда ВМ остановилась или окно закрыто.
pub(crate) fn run(machine: Vm, screen: Screen, vm: &VmArgs) -> Result<(RunOutcome, ExitStats)> {
    window::enable_dpi_awareness();
    let inputs = machine.inputs();
    let touch = inputs.first().cloned();
    let keyboard = inputs.get(1).cloned();
    let stopper = machine.stopper();
    let (w, h) = (screen.spec.width, screen.spec.height);

    let mut touch_state = TouchState::default();
    let close_stopper = stopper.clone();
    let handler = move |e: WindowEvent| match e {
        WindowEvent::Pointer { x, y, down } => {
            if let Some(t) = &touch {
                let events = if down {
                    touch_state.touch(0, to_panel(x, w), to_panel(y, h))
                } else {
                    touch_state.release(0)
                };
                if !events.is_empty() {
                    t.push(&events);
                }
            }
        }
        WindowEvent::Key {
            scancode,
            extended,
            pressed,
        } => {
            if let (Some(k), Some(code)) = (&keyboard, linux_keycode(scancode, extended)) {
                k.push(&key_events(code, pressed));
            }
        }
        WindowEvent::Close => close_stopper.stop(),
    };
    let title = format!("RustDroid — {w}x{h} @ {} Гц", screen.spec.refresh_hz);
    let win = window::create(&title, w, h, handler).context("не удалось создать окно")?;

    let stop = Arc::new(AtomicBool::new(false));
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
}
