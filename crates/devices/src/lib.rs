//! Виртуальные устройства гостя.
//!
//! | Устройство     | Назначение                                              | Этап |
//! |----------------|---------------------------------------------------------|------|
//! | 16550 UART     | консоль ядра и шелл ([`serial`])                        | 1    |
//! | virtio-blk     | system/vendor/userdata, copy-on-write оверлеи           | 2    |
//! | virtio-console | логи ядра и `logcat`                                    | 2    |
//! | virtio-input   | клавиатура и мультитач (ABS_MT_*)                       | 3    |
//! | virtio-net     | user-mode NAT на smoltcp, без TAP и прав администратора | 5    |
//! | virtio-vsock   | служебный канал хост↔гость (агент)                      | 5    |
//! | virtio-snd     | звук через WASAPI                                       | 9    |
//!
//! virtio-gpu вынесен в отдельный крейт `rustdroid-gpu`.

pub mod serial;

pub use serial::{InterruptLine, Serial16550};
