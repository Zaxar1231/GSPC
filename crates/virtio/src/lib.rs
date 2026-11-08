//! Общий каркас virtio.
//!
//! - [`queue`] — split virtqueue с `VIRTIO_F_EVENT_IDX` и indirect-дескрипторами, без аллокаций
//!   в горячем пути;
//! - [`msix`] — таблица MSI-X и формат MSI-сообщений x86;
//! - [`pci`] — транспорт virtio-pci (modern): config space с capability-структурами, BAR0 с
//!   common/notify/ISR/device cfg и MSI-X;
//! - [`device`] — трейт устройства;
//! - [`mem`] — доступ к гостевой памяти.
//!
//! Реализация своя (а не `virtio-queue` из rust-vmm): тот требует трейтов `vm-memory`, от
//! которых мы отказались (ADR 0007).

pub mod device;
pub mod mem;
pub mod msix;
pub mod pci;
pub mod queue;

pub use device::VirtioDevice;
pub use mem::{GuestMemory, GuestMemoryExt, MemoryError};
pub use msix::{MsiMessage, MsiSink};
pub use pci::VirtioPci;
pub use queue::{Chain, Descriptor, Queue, QueueError};
