//! virtio-gpu.
//!
//! - [`device`] — этап 3: 2D-scanout. Гость рисует через SwiftShader, изменённые
//!   прямоугольники копируются в ресурс, по `RESOURCE_FLUSH` кадр уходит в [`scanout::Mailbox`];
//! - [`edid`] — EDID с режимом дисплея (разрешение и частота 60/90/120/144 Гц);
//! - этап 4: 3D через gfxstream (FFI, C++). В госте ANGLE-on-Vulkan, на хосте только Vulkan,
//!   кадр передаётся в D3D12 через общий NT handle без копий через CPU.
//!
//! Подробности: `docs/architecture.md`, раздел «Путь кадра».

pub mod device;
pub mod edid;
pub mod scanout;

pub use device::VirtioGpu;
pub use edid::DisplayMode;
pub use scanout::{Frame, Mailbox, PixelFormat};
