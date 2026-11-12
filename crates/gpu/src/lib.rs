//! virtio-gpu.
//!
//! - [`device`] — этап 3: 2D-scanout. Гость рисует через SwiftShader, изменённые
//!   прямоугольники копируются в ресурс, по `RESOURCE_FLUSH` кадр уходит в [`scanout::Mailbox`];
//! - [`edid`] — EDID с режимом дисплея (разрешение и частота 60/90/120/144 Гц);
//! - [`backend`] — этап 4: трейт [`Backend3d`] для 3D-команд гостя и приёмник фенсов;
//!   исполняет их gfxstream (FFI, C++). Кадр пока читается через CPU, zero-copy через NT handle
//!   в D3D12 — задача 4.5.
//!
//! Подробности: `docs/architecture.md`, раздел «Путь кадра».

pub mod backend;
pub mod device;
pub mod edid;
#[cfg(windows)]
pub mod gfxstream;
pub mod scanout;

pub use backend::{Backend3d, FenceSink};
pub use device::VirtioGpu;
pub use edid::DisplayMode;
pub use scanout::{Frame, Mailbox, PixelFormat};
