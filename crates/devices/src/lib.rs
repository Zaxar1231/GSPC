//! Виртуальные устройства гостя.
//!
//! | Устройство     | Назначение                                              | Этап |
//! |----------------|---------------------------------------------------------|------|
//! | 16550 UART     | консоль ядра и шелл ([`serial`])                        | 1    |
//! | virtio-blk     | диски ([`blk`]) поверх бэкендов [`disk`]: raw, CoW, GPT  | 2    |
//! | virtio-console | консоль Android ([`console`])                           | 2    |
//! | virtio-input   | мультитач (протокол B) и клавиатура ([`input`])          | 3    |
//! | virtio-net     | user-mode NAT на smoltcp, без TAP и прав администратора | 5    |
//! | virtio-vsock   | служебный канал хост↔гость (агент)                      | 5    |
//! | virtio-snd     | звук через WASAPI                                       | 9    |
//!
//! virtio-gpu вынесен в отдельный крейт `rustdroid-gpu`.

pub mod blk;
pub mod console;
pub mod disk;
pub mod input;
pub mod serial;

pub use blk::VirtioBlk;
pub use console::{ConsoleInput, VirtioConsole};
pub use disk::{CompositeDisk, CowDisk, Disk, Partition, RawDisk, ZeroDisk};
pub use input::{InputEvent, InputEvents, InputKind, TouchState, VirtioInput};
pub use serial::{InterruptLine, Serial16550};
