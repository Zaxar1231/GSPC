//! Трейт virtio-устройства (не зависит от транспорта).

use crate::mem::GuestMemory;
use crate::queue::{Queue, QueueError};

/// Тип устройства virtio-blk.
pub const TYPE_BLOCK: u32 = 2;
/// Тип устройства virtio-console.
pub const TYPE_CONSOLE: u32 = 3;
/// Тип устройства virtio-gpu.
pub const TYPE_GPU: u32 = 16;
/// Тип устройства virtio-input.
pub const TYPE_INPUT: u32 = 18;

/// Возможность: устройство соответствует virtio 1.x.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Возможность: indirect-дескрипторы.
pub const VIRTIO_RING_F_INDIRECT_DESC: u64 = 1 << 28;
/// Возможность: подавление уведомлений через event index.
pub const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;

/// virtio-устройство.
pub trait VirtioDevice: Send {
    /// Тип устройства (`TYPE_*`).
    fn device_type(&self) -> u32;

    /// Специфичные для устройства возможности (транспортные добавляет транспорт).
    fn device_features(&self) -> u64;

    /// Максимальные размеры очередей.
    fn queue_max_sizes(&self) -> &[u16];

    /// Чтение конфигурации устройства.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Запись конфигурации устройства.
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    /// Драйвер выставил DRIVER_OK с согласованными возможностями.
    fn activate(&mut self, _features: u64) {}

    /// Reset устройства драйвером.
    fn reset(&mut self) {}

    /// Обрабатывает очередь `index`. Возвращает `true`, если в used ring что-то добавлено.
    ///
    /// # Errors
    /// Некорректные структуры очереди.
    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError>;
}
