//! Поток композитора: кадр гостя → D3D12, метрики для оверлея и логов.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustdroid_gpu::Mailbox;
use rustdroid_telemetry::FrameStats;
use rustdroid_telemetry::overlay::OverlayData;

use crate::d3d12::{PresentMode, Presenter};
use crate::usage::{GuestRuntime, ThreadMeter, UsageMeter};
use crate::window::WindowHandle;

/// Пауза между кадрами, после которой экран считается неподвижным: Android не присылает кадры,
/// пока ничего не меняется, и такая пауза — не медленный кадр. В статистику она не идёт.
const IDLE_GAP: Duration = Duration::from_secs(1);
/// Как часто писать в лог кадры и профиль CPU.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// Настройки вывода.
#[derive(Debug, Clone, Copy)]
pub struct PresentConfig {
    /// VSync или без него.
    pub mode: PresentMode,
    /// Лимит FPS (0 — без лимита).
    pub fps_limit: u32,
    /// Показывать оверлей.
    pub overlay: bool,
    /// Частота дисплея гостя (для целевой линии графика).
    pub refresh_hz: u32,
}

/// Запускает композитор в текущем потоке до `stop`.
///
/// `guest` — счётчик времени работы vCPU для строки `VCPU` оверлея.
pub fn run(
    window: WindowHandle,
    mailbox: &Arc<Mailbox>,
    stop: &AtomicBool,
    cfg: PresentConfig,
    guest: Option<GuestRuntime>,
) {
    let _mmcss = MmcssGames::enter();
    let Some(first) = wait_first(mailbox, stop) else {
        return;
    };
    let mut presenter = match Presenter::new(window.0, first.width, first.height) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "не удалось создать D3D12-композитор");
            window.close();
            return;
        }
    };
    if cfg.mode == PresentMode::Immediate && !presenter.tearing_supported() {
        tracing::warn!(
            "tearing не поддерживается: режим без VSync будет ограничен композитором Windows"
        );
    }
    let mut stats = FrameStats::new(1024);
    let mut latency = FrameStats::new(256);
    let mut seen = 0_u64;
    let mut last_flush: Option<Instant> = None;
    let mut last_log = Instant::now();
    let mut times: Vec<f32> = Vec::with_capacity(256);
    let mut next_deadline = Instant::now();
    let target_ms = 1000.0 / cfg.refresh_hz.max(1) as f32;
    let mut meter = UsageMeter::new(guest);
    let mut usage = meter.update();
    let mut last_usage = Instant::now();
    let mut threads = ThreadMeter::new();
    let _ = threads.update();
    let mut idle_gaps = 0_u32;
    let mut frames = 0_u32;
    let mut overlay = OverlayData {
        lines: Vec::with_capacity(6),
        frame_times_ms: Vec::with_capacity(256),
        target_ms,
    };

    while !stop.load(Ordering::Acquire) {
        presenter.wait_for_slot(100);
        if last_usage.elapsed() >= Duration::from_secs(1) {
            last_usage = Instant::now();
            usage = meter.update();
        }
        if last_log.elapsed() >= LOG_EVERY {
            last_log = Instant::now();
            log_periodic(
                &mut stats,
                &mut latency,
                &usage,
                &mut threads,
                frames,
                idle_gaps,
            );
            idle_gaps = 0;
            frames = 0;
        }
        let Some(frame) = mailbox.wait_after(seen, Duration::from_millis(100)) else {
            continue;
        };
        seen = frame.seq;
        frames += 1;
        if let Some(prev) = last_flush {
            let dt = frame.flushed_at.saturating_duration_since(prev);
            if dt >= IDLE_GAP {
                idle_gaps += 1;
            } else {
                stats.record(dt);
                if times.len() == times.capacity() {
                    times.remove(0);
                }
                times.push(dt.as_secs_f32() * 1000.0);
            }
        }
        last_flush = Some(frame.flushed_at);

        if cfg.fps_limit > 0 {
            let now = Instant::now();
            if next_deadline > now {
                std::thread::sleep(next_deadline - now);
            }
            next_deadline =
                next_deadline.max(now) + Duration::from_secs_f64(1.0 / f64::from(cfg.fps_limit));
        }

        let overlay_ref = if cfg.overlay {
            overlay.lines.clear();
            if let Some(s) = stats.summary() {
                overlay.lines.push(format!("FPS {:.1}", s.avg_fps));
                overlay
                    .lines
                    .push(format!("1% LOW {:.1}", s.low_1_percent_fps));
                overlay.lines.push(format!("FT {:.2} MS", s.avg_frame_ms));
            }
            if let Some(l) = latency.summary() {
                overlay.lines.push(format!("LAT {:.1} MS", l.avg_frame_ms));
            }
            overlay.lines.extend(usage.overlay_lines());
            overlay.frame_times_ms.clear();
            overlay.frame_times_ms.extend_from_slice(&times);
            Some(&overlay)
        } else {
            None
        };
        if let Err(e) = presenter.present(&frame, overlay_ref, cfg.mode) {
            tracing::error!(error = %e, "ошибка вывода кадра");
            break;
        }
        // Задержка от RESOURCE_FLUSH гостя до Present.
        latency.record(frame.flushed_at.elapsed());
        mailbox.recycle(frame);
    }
}

/// Строки лога раз в [`LOG_EVERY`]: кадры (если за интервал они были; статистика — по
/// скользящему окну последних кадров) и загрузка CPU по группам потоков.
fn log_periodic(
    stats: &mut FrameStats,
    latency: &mut FrameStats,
    usage: &rustdroid_telemetry::usage::Usage,
    threads: &mut ThreadMeter,
    frames: u32,
    idle_gaps: u32,
) {
    if frames > 0
        && let (Some(s), Some(l)) = (stats.summary(), latency.summary())
    {
        tracing::info!(
            frames,
            fps = format_args!("{:.1}", s.avg_fps),
            low_1 = format_args!("{:.1}", s.low_1_percent_fps),
            frame_ms = format_args!("{:.2}", s.avg_frame_ms),
            p99_ms = format_args!("{:.2}", s.p99_frame_ms),
            flush_to_present_ms = format_args!("{:.2}", l.avg_frame_ms),
            idle_gaps,
            ram_gib = format_args!("{:.2}", usage.working_set_gib),
            vcpu_pct = format_args!("{:.0}", usage.guest_cpu.unwrap_or(-1.0)),
            cpu_pct = format_args!("{:.0}", usage.process_cpu),
            system_cpu_pct = format_args!("{:.0}", usage.system_cpu),
            "кадры гостя"
        );
    }
    let loads = threads.update();
    tracing::info!(
        vcpu_pct = format_args!("{:.0}", usage.guest_cpu.unwrap_or(-1.0)),
        cpu_pct = format_args!("{:.0}", usage.process_cpu),
        threads = %rustdroid_telemetry::threads::format_loads(&loads),
        "профиль CPU"
    );
}

fn wait_first(mailbox: &Mailbox, stop: &AtomicBool) -> Option<Arc<rustdroid_gpu::Frame>> {
    while !stop.load(Ordering::Acquire) {
        if let Some(f) = mailbox.wait_after(0, Duration::from_millis(200)) {
            return Some(f);
        }
    }
    None
}

/// Регистрация потока в MMCSS «Games» на время работы композитора.
struct MmcssGames(Option<windows::Win32::Foundation::HANDLE>);

impl MmcssGames {
    fn enter() -> Self {
        use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let mut index = 0_u32;
        // SAFETY: имя задачи — статическая строка.
        Self(
            unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Games"), &raw mut index) }
                .ok(),
        )
    }
}

impl Drop for MmcssGames {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: дескриптор получен в `enter` этим же потоком.
            let _ =
                unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(h) };
        }
    }
}
