//! virtio-gpu.
//!
//! - Этап 3: 2D-scanout. Гость рисует через SwiftShader, scanout-ресурс отдаётся
//!   композитору (`rustdroid-present`) как текстура, без readback через CPU.
//! - Этап 4: 3D через gfxstream (FFI, C++). В госте ANGLE-on-Vulkan, на хосте только Vulkan.
//!   Host-visible blob-память выделяет VMM и импортирует в Vulkan через
//!   `VK_EXT_external_memory_host`. Кадр передаётся в D3D12 через общий NT handle.
//!
//! Подробности: `docs/architecture.md`, раздел «Путь кадра».
