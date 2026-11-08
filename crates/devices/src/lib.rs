//! Виртуальные устройства гостя.
//!
//! | Устройство     | Назначение                                              | Этап |
//! |----------------|---------------------------------------------------------|------|
//! | 16550 UART     | консоль ядра и шелл ([`serial`])                        | 1    |
//! | CMOS RTC       | время гостя при загрузке ([`rtc`])                       | 2    |
//! | virtio-blk     | диски ([`blk`]) поверх бэкендов [`disk`]: raw, CoW, GPT  | 2    |
//! | virtio-console | консоль Android ([`console`])                           | 2    |
//! | virtio-input   | мультитач (протокол B) и клавиатура ([`input`])          | 3    |
//! | virtio-net     | user-mode NAT на smoltcp, без TAP и прав администратора | 5    |
//! | virtio-vsock   | служебный канал хост↔гость ([`vsock`])                   | 3/5  |
//! | virtio-snd     | звук через WASAPI                                       | 9    |
//!
//! virtio-gpu вынесен в отдельный крейт `rustdroid-gpu`.

pub mod blk;
pub mod console;
pub mod disk;
pub mod input;
pub mod rtc;
pub mod serial;
pub mod vsock;

pub use blk::VirtioBlk;
pub use console::{ConsoleInput, VirtioConsole};
pub use disk::{
    CompositeDisk, CowDisk, Disk, Partition, RawDisk, SparseDisk, ZeroDisk, open_image,
};
pub use input::{InputEvent, InputEvents, InputKind, TouchState, VirtioInput};
pub use rtc::CmosRtc;
pub use serial::{InterruptLine, Serial16550};
pub use vsock::VirtioVsock;
