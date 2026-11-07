//! Общий каркас virtio.
//!
//! - транспорт virtio-pci (modern, 1.x): capability-структуры, common/notify/ISR/device cfg;
//! - MSI-X для конфигурации и для каждой очереди;
//! - virtqueue (split ring) с `VIRTIO_F_EVENT_IDX` и батчингом уведомлений;
//! - трейт устройства и worker-потоки, просыпающиеся по doorbell-событиям WHPX.
//!
//! Реализация — этап 2 (см. `docs/roadmap.md`).
