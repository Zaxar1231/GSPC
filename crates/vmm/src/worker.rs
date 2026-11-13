//! Worker-потоки virtio-устройств на doorbell-событиях WHPX.
//!
//! На каждую очередь — авто-сбрасываемое событие Windows, зарегистрированное как doorbell на
//! notify-адресе очереди. Запись гостя в notify будит worker без выхода vCPU в user mode;
//! worker обрабатывает очередь и шлёт MSI-X. Отдельное событие останавливает поток.

use std::sync::{Arc, PoisonError};
use std::thread::JoinHandle;

use rustdroid_whpx::Partition;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};

use crate::error::{Error, Result};
use crate::pci::SharedFunction;

/// Событие Windows (auto-reset), закрывается при удалении.
#[derive(Debug)]
pub struct Event(HANDLE);

// SAFETY: дескриптор события можно использовать из любого потока.
unsafe impl Send for Event {}
// SAFETY: SetEvent/WaitFor* потокобезопасны.
unsafe impl Sync for Event {}

impl Event {
    /// Новое auto-reset событие.
    ///
    /// # Errors
    /// Ошибка CreateEventW.
    pub fn new() -> Result<Self> {
        // SAFETY: безымянное событие с атрибутами по умолчанию.
        let h = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| Error::Config(format!("CreateEventW: {e}")))?;
        Ok(Self(h))
    }

    /// Дескриптор.
    #[must_use]
    pub fn handle(&self) -> HANDLE {
        self.0
    }

    /// Сигналит событие.
    pub fn set(&self) {
        // SAFETY: дескриптор валиден, пока жив `self`.
        if let Err(e) = unsafe { SetEvent(self.0) } {
            tracing::warn!(error = %e, "SetEvent");
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: дескриптор получен из CreateEventW и закрывается один раз.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Worker одного устройства.
pub struct DeviceWorker {
    partition: Arc<Partition>,
    doorbells: Vec<u64>,
    queue_events: Arc<Vec<Event>>,
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for DeviceWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceWorker")
            .field("doorbells", &self.doorbells.len())
            .finish_non_exhaustive()
    }
}

impl DeviceWorker {
    /// Регистрирует doorbell на каждую очередь функции и запускает поток.
    ///
    /// Если doorbell не зарегистрировался (старая сборка Windows), очередь продолжает работать
    /// через выход vCPU и синхронную обработку в шине — медленнее, но корректно.
    ///
    /// # Errors
    /// Не удалось создать события или поток.
    pub fn spawn(name: &str, partition: Arc<Partition>, function: SharedFunction) -> Result<Self> {
        let (queues, addrs): (usize, Vec<u64>) = {
            let f = function.lock().unwrap_or_else(PoisonError::into_inner);
            (
                f.num_queues(),
                (0..f.num_queues()).map(|q| f.notify_address(q)).collect(),
            )
        };
        let events: Vec<Event> = (0..queues).map(|_| Event::new()).collect::<Result<_>>()?;
        let mut doorbells = Vec::new();
        for (q, (addr, ev)) in addrs.iter().zip(&events).enumerate() {
            match partition.register_doorbell(*addr, ev.handle()) {
                Ok(()) => doorbells.push(*addr),
                Err(e) => {
                    tracing::warn!(device = name, queue = q, error = %e, "doorbell недоступен, используется путь через выход vCPU")
                }
            }
        }
        let queue_events = Arc::new(events);
        let stop = Arc::new(Event::new()?);
        let thread = {
            let queue_events = Arc::clone(&queue_events);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(format!("virtio-{name}"))
                .spawn(move || run(&function, &queue_events, &stop))
                .map_err(|e| Error::Config(format!("поток устройства: {e}")))?
        };
        tracing::info!(
            device = name,
            doorbells = doorbells.len(),
            queues,
            "worker устройства запущен"
        );
        Ok(Self {
            partition,
            doorbells,
            queue_events,
            stop,
            thread: Some(thread),
        })
    }

    /// Функция, которая будит обработку очереди из любого потока (фенсы gfxstream).
    pub fn kicker(&self, queue: usize) -> impl Fn() + Send + Sync + 'static {
        let events = Arc::clone(&self.queue_events);
        move || {
            if let Some(ev) = events.get(queue) {
                ev.set();
            }
        }
    }

    /// Будит обработку очереди (например, при новом вводе консоли).
    pub fn kick(&self, queue: usize) {
        if let Some(ev) = self.queue_events.get(queue) {
            ev.set();
        }
    }
}

fn run(function: &SharedFunction, queue_events: &[Event], stop: &Event) {
    let _tuning = crate::host::tune_current_thread(Some("Games"));
    let mut handles: Vec<HANDLE> = queue_events.iter().map(Event::handle).collect();
    handles.push(stop.handle());
    let stop_index = queue_events.len();
    loop {
        // SAFETY: все дескрипторы живы, пока жив worker (события удаляются после join).
        let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        let index = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        if index == stop_index || index > handles.len() {
            return;
        }
        function
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .notify(index);
    }
}

impl Drop for DeviceWorker {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for addr in &self.doorbells {
            let _ = self.partition.unregister_doorbell(*addr);
        }
    }
}
