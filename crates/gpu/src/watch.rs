//! Диагностика 3D-пути virtio-gpu: журнал команд и сторож зависших вызовов бэкенда.
//!
//! Команда virtio-gpu исполняется в потоке устройства, пока занят мьютекс функции PCI; если
//! gfxstream не возвращается из вызова, гость со временем останавливается целиком (vCPU ждут тот
//! же мьютекс). Сторож — отдельный поток — замечает команду, которая идёт дольше
//! [`STUCK_AFTER`], и пишет в лог, какая это команда, — пока поток устройства стоит.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// Через сколько считать команду зависшей.
const STUCK_AFTER: Duration = Duration::from_secs(3);
/// Как часто повторять предупреждение о той же команде.
const REPEAT_EVERY: Duration = Duration::from_secs(10);
/// Сколько первых команд писать в лог (debug) целиком; дальше — только редкие.
const LOG_FIRST: u64 = 500;

/// Имя команды virtio-gpu для лога.
pub(crate) fn command_name(kind: u32) -> &'static str {
    match kind {
        0x0100 => "GET_DISPLAY_INFO",
        0x0101 => "RESOURCE_CREATE_2D",
        0x0102 => "RESOURCE_UNREF",
        0x0103 => "SET_SCANOUT",
        0x0104 => "RESOURCE_FLUSH",
        0x0105 => "TRANSFER_TO_HOST_2D",
        0x0106 => "RESOURCE_ATTACH_BACKING",
        0x0107 => "RESOURCE_DETACH_BACKING",
        0x0108 => "GET_CAPSET_INFO",
        0x0109 => "GET_CAPSET",
        0x010A => "GET_EDID",
        0x010B => "RESOURCE_ASSIGN_UUID",
        0x010C => "RESOURCE_CREATE_BLOB",
        0x010D => "SET_SCANOUT_BLOB",
        0x0200 => "CTX_CREATE",
        0x0201 => "CTX_DESTROY",
        0x0202 => "CTX_ATTACH_RESOURCE",
        0x0203 => "CTX_DETACH_RESOURCE",
        0x0204 => "RESOURCE_CREATE_3D",
        0x0205 => "TRANSFER_TO_HOST_3D",
        0x0206 => "TRANSFER_FROM_HOST_3D",
        0x0207 => "SUBMIT_3D",
        0x0208 => "RESOURCE_MAP_BLOB",
        0x0209 => "RESOURCE_UNMAP_BLOB",
        0x0300 => "UPDATE_CURSOR",
        0x0301 => "MOVE_CURSOR",
        _ => "?",
    }
}

/// Команды, которые пишутся в лог всегда (их немного за всё время работы).
fn is_rare(kind: u32) -> bool {
    matches!(
        kind,
        0x0103 | 0x0108 | 0x0109 | 0x010C | 0x010D | 0x0200 | 0x0201 | 0x0208 | 0x0209
    )
}

#[derive(Debug, Clone, Copy)]
struct Running {
    kind: u32,
    ctx: u32,
    since: Instant,
    reported: Option<Instant>,
}

/// Текущая команда и счётчик команд; сторожевой поток держит слабую ссылку.
#[derive(Debug, Default)]
pub(crate) struct CommandWatch {
    running: Mutex<Option<Running>>,
    count: AtomicU64,
}

impl CommandWatch {
    /// Создаёт сторожа и его поток; поток завершается вместе с последней сильной ссылкой.
    pub(crate) fn spawn() -> Arc<Self> {
        let watch = Arc::new(Self::default());
        let weak = Arc::downgrade(&watch);
        if let Err(e) = std::thread::Builder::new()
            .name("virtio-gpu-watch".into())
            .spawn(move || run(&weak))
        {
            tracing::warn!(error = %e, "сторож virtio-gpu не запущен");
        }
        watch
    }

    /// Команда `kind` контекста `ctx` начата.
    pub(crate) fn begin(&self, kind: u32, ctx: u32, resource: u32) {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n < LOG_FIRST || is_rare(kind) {
            tracing::debug!(n, cmd = command_name(kind), ctx, resource, "virtio-gpu");
        }
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = Some(Running {
            kind,
            ctx,
            since: Instant::now(),
            reported: None,
        });
    }

    /// Команда закончена; если сторож успел пожаловаться, пишем, сколько она шла.
    pub(crate) fn end(&self) {
        let done = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(r) = done
            && r.reported.is_some()
        {
            tracing::warn!(
                cmd = command_name(r.kind),
                ctx = r.ctx,
                secs = r.since.elapsed().as_secs_f32(),
                "долгая команда virtio-gpu всё-таки завершилась"
            );
        }
    }
}

fn run(weak: &Weak<CommandWatch>) {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some(watch) = weak.upgrade() else {
            return;
        };
        let mut running = watch.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(r) = running.as_mut() {
            let now = Instant::now();
            let due = r
                .reported
                .map_or(now - r.since >= STUCK_AFTER, |t| now - t >= REPEAT_EVERY);
            if due {
                r.reported = Some(now);
                tracing::warn!(
                    cmd = command_name(r.kind),
                    ctx = r.ctx,
                    secs = r.since.elapsed().as_secs(),
                    "команда virtio-gpu не завершается: поток устройства ждёт ответа gfxstream"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_rare_commands() {
        assert_eq!(command_name(0x0207), "SUBMIT_3D");
        assert_eq!(command_name(0x7777), "?");
        assert!(is_rare(0x0208));
        assert!(!is_rare(0x0207));
    }

    #[test]
    fn begin_end_tracks_the_running_command() {
        let w = CommandWatch::spawn();
        w.begin(0x0207, 3, 0);
        assert_eq!(
            w.running.lock().unwrap().map(|r| (r.kind, r.ctx)),
            Some((0x0207, 3))
        );
        w.end();
        assert!(w.running.lock().unwrap().is_none());
        assert_eq!(w.count.load(Ordering::Relaxed), 1);
    }
}
