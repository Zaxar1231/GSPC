//! 3D-бэкенд virtio-gpu: то, что выполняет команды контекстов гостя.
//!
//! Устройство ([`crate::VirtioGpu`]) разбирает протокол virtio-gpu, проверяет аргументы и ведёт
//! учёт ресурсов и фенсов, а исполнение отдаёт бэкенду. Настоящий бэкенд — gfxstream
//! ([`crate::gfxstream`], только Windows); в тестах — заглушка, записывающая вызовы.
//!
//! Фенсы асинхронные: бэкенд сообщает о выполненном фенсе через [`FenceSink::signal`] из любого
//! потока, устройство при следующей обработке очереди возвращает гостю придержанные команды.

use std::sync::{Arc, Mutex, PoisonError};

use rustdroid_virtio::GuestMemory;

/// Описание capset для `GET_CAPSET_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsetInfo {
    /// Номер capset (`VIRTIO_GPU_CAPSET_*`).
    pub id: u32,
    /// Максимальная версия.
    pub max_version: u32,
    /// Размер описания в байтах.
    pub max_size: u32,
}

/// Аргументы `RESOURCE_CREATE_3D` (поля как в virtio-gpu).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resource3d {
    /// `PIPE_TEXTURE_*` / `PIPE_BUFFER`.
    pub target: u32,
    /// Формат (`VIRTIO_GPU_FORMAT_*` / `PIPE_FORMAT_*`).
    pub format: u32,
    /// Флаги `PIPE_BIND_*`.
    pub bind: u32,
    /// Ширина.
    pub width: u32,
    /// Высота.
    pub height: u32,
    /// Глубина.
    pub depth: u32,
    /// Число слоёв.
    pub array_size: u32,
    /// Последний mip-уровень.
    pub last_level: u32,
    /// Сэмплов на пиксель.
    pub nr_samples: u32,
    /// Флаги.
    pub flags: u32,
}

/// Аргументы `RESOURCE_CREATE_BLOB`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Blob {
    /// Контекст, создающий blob (0 для `BLOB_MEM_GUEST` без контекста).
    pub ctx_id: u32,
    /// `VIRTIO_GPU_BLOB_MEM_*`.
    pub blob_mem: u32,
    /// `VIRTIO_GPU_BLOB_FLAG_*`.
    pub blob_flags: u32,
    /// Идентификатор объекта в контексте.
    pub blob_id: u64,
    /// Размер.
    pub size: u64,
}

/// Объём (box) передачи данных.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Box3d {
    /// X.
    pub x: u32,
    /// Y.
    pub y: u32,
    /// Z.
    pub z: u32,
    /// Ширина.
    pub w: u32,
    /// Высота.
    pub h: u32,
    /// Глубина.
    pub d: u32,
}

/// Аргументы `TRANSFER_TO_HOST_3D` / `TRANSFER_FROM_HOST_3D`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer3d {
    /// Ресурс.
    pub resource_id: u32,
    /// Контекст команды (0 — без контекста).
    pub ctx_id: u32,
    /// Mip-уровень.
    pub level: u32,
    /// Шаг строки в backing.
    pub stride: u32,
    /// Шаг слоя в backing.
    pub layer_stride: u32,
    /// Область.
    pub region: Box3d,
    /// Смещение в backing.
    pub offset: u64,
}

/// Кусок памяти гостя в backing ресурса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemEntry {
    /// GPA.
    pub addr: u64,
    /// Длина.
    pub len: u32,
}

/// Фенс гостя.
///
/// `ring_idx = None` — глобальная временная шкала (фенс без `VIRTIO_GPU_FLAG_INFO_RING_IDX`):
/// такие фенсы завершаются по порядку номеров независимо от контекста.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FenceId {
    /// Номер фенса (растёт на каждой временной шкале).
    pub fence_id: u64,
    /// Контекст.
    pub ctx_id: u32,
    /// Кольцо контекста.
    pub ring_idx: Option<u8>,
}

impl FenceId {
    /// `true`, если сработавший фенс `signaled` завершает этот (та же шкала, номер не больше).
    #[must_use]
    pub fn completed_by(&self, signaled: &Self) -> bool {
        let same_timeline = match (self.ring_idx, signaled.ring_idx) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b && self.ctx_id == signaled.ctx_id,
            _ => false,
        };
        same_timeline && self.fence_id <= signaled.fence_id
    }
}

/// Ошибка бэкенда; устройство переводит её в код ответа virtio-gpu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// Нет такого ресурса.
    #[error("нет такого ресурса")]
    InvalidResource,
    /// Нет такого контекста.
    #[error("нет такого контекста")]
    InvalidContext,
    /// Неверные аргументы.
    #[error("неверные аргументы")]
    InvalidParameter,
    /// Не хватило памяти.
    #[error("не хватило памяти")]
    OutOfMemory,
    /// Прочая ошибка бэкенда.
    #[error("ошибка бэкенда")]
    Failed,
}

/// Результат вызова бэкенда.
pub type BackendResult<T = ()> = std::result::Result<T, BackendError>;

/// Исполнитель 3D-команд.
///
/// Методы вызываются из потока устройства по одному. Память гостя (`mem`) передаётся туда,
/// где бэкенду нужен её адрес на хосте.
pub trait Backend3d: Send {
    /// Число capsets.
    fn num_capsets(&self) -> u32;

    /// Capset по индексу `0..num_capsets`.
    fn capset_info(&self, index: u32) -> Option<CapsetInfo>;

    /// Описание capset `id` версии `version`.
    fn capset(&self, id: u32, version: u32) -> Option<Vec<u8>>;

    /// Создаёт контекст. `context_init` — из `CTX_CREATE` (младшие 8 бит — capset).
    ///
    /// # Errors
    /// Бэкенд не смог создать контекст.
    fn context_create(&mut self, ctx_id: u32, context_init: u32, name: &[u8]) -> BackendResult;

    /// Удаляет контекст.
    fn context_destroy(&mut self, ctx_id: u32);

    /// Делает ресурс видимым контексту.
    fn context_attach_resource(&mut self, ctx_id: u32, resource_id: u32);

    /// Убирает ресурс из контекста.
    fn context_detach_resource(&mut self, ctx_id: u32, resource_id: u32);

    /// Создаёт 3D-ресурс.
    ///
    /// # Errors
    /// Неподдерживаемые параметры или нехватка памяти.
    fn resource_create_3d(&mut self, resource_id: u32, args: &Resource3d) -> BackendResult;

    /// Создаёт blob-ресурс; `backing` — память гостя для `BLOB_MEM_GUEST`.
    ///
    /// # Errors
    /// Неподдерживаемые параметры или нехватка памяти.
    fn resource_create_blob(
        &mut self,
        resource_id: u32,
        blob: &Blob,
        backing: &[MemEntry],
        mem: &dyn GuestMemory,
    ) -> BackendResult;

    /// Привязывает к ресурсу память гостя.
    ///
    /// # Errors
    /// Память вне RAM или ошибка бэкенда.
    fn attach_backing(
        &mut self,
        resource_id: u32,
        backing: &[MemEntry],
        mem: &dyn GuestMemory,
    ) -> BackendResult;

    /// Отвязывает память гостя.
    fn detach_backing(&mut self, resource_id: u32);

    /// Освобождает ресурс.
    fn resource_unref(&mut self, resource_id: u32);

    /// Передача между backing гостя и ресурсом.
    ///
    /// # Errors
    /// Неверная область или ошибка бэкенда.
    fn transfer(&mut self, transfer: &Transfer3d, to_host: bool) -> BackendResult;

    /// Поток команд контекста (`SUBMIT_3D`).
    ///
    /// # Errors
    /// Ошибка бэкенда.
    fn submit(&mut self, ctx_id: u32, ring_idx: Option<u8>, cmd: &mut [u8]) -> BackendResult;

    /// Ставит фенс; по выполнении бэкенд вызывает [`FenceSink::signal`].
    ///
    /// # Errors
    /// Ошибка бэкенда.
    fn create_fence(&mut self, fence: FenceId) -> BackendResult;

    /// Читает пиксели ресурса (RGBA/BGRA, шаг `width * 4`) для показа в окне.
    ///
    /// Временный путь до zero-copy (задача 4.5): кадр копируется через CPU.
    ///
    /// # Errors
    /// Ресурс нельзя прочитать.
    fn read_pixels(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
        out: &mut [u8],
    ) -> BackendResult;
}

type Waker = Box<dyn Fn() + Send + Sync>;

/// Сработавшие фенсы от бэкенда к устройству.
#[derive(Default)]
pub struct FenceSink {
    signaled: Mutex<Vec<FenceId>>,
    waker: Mutex<Option<Waker>>,
}

impl std::fmt::Debug for FenceSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FenceSink").finish_non_exhaustive()
    }
}

impl FenceSink {
    /// Пустой приёмник.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Фенс выполнен (вызывается из любого потока).
    pub fn signal(&self, fence: FenceId) {
        self.signaled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(fence);
        if let Some(w) = self
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            w();
        }
    }

    /// Функция, которая будит обработку очереди управления (worker устройства).
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(waker));
    }

    /// Забирает сработавшие фенсы.
    pub(crate) fn take(&self) -> Vec<FenceId> {
        std::mem::take(&mut *self.signaled.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(fence_id: u64, ctx_id: u32, ring_idx: Option<u8>) -> FenceId {
        FenceId {
            fence_id,
            ctx_id,
            ring_idx,
        }
    }

    #[test]
    fn global_timeline_ignores_context() {
        assert!(f(3, 0, None).completed_by(&f(5, 7, None)));
        assert!(f(5, 1, None).completed_by(&f(5, 0, None)));
        assert!(!f(6, 0, None).completed_by(&f(5, 0, None)));
    }

    #[test]
    fn ring_timeline_needs_same_context_and_ring() {
        assert!(f(2, 1, Some(0)).completed_by(&f(2, 1, Some(0))));
        assert!(!f(2, 1, Some(0)).completed_by(&f(9, 2, Some(0))));
        assert!(!f(2, 1, Some(0)).completed_by(&f(9, 1, Some(1))));
        assert!(!f(2, 1, Some(0)).completed_by(&f(9, 1, None)));
        assert!(!f(2, 1, None).completed_by(&f(9, 1, Some(0))));
    }

    #[test]
    fn sink_wakes_and_drains() {
        let sink = FenceSink::new();
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = Arc::clone(&woke);
        sink.set_waker(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        sink.signal(f(1, 0, None));
        sink.signal(f(2, 3, Some(1)));
        assert_eq!(woke.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(sink.take(), vec![f(1, 0, None), f(2, 3, Some(1))]);
        assert!(sink.take().is_empty());
    }
}
