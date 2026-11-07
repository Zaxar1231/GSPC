//! Хостовый композитор на чистом D3D12.
//!
//! Путь кадра гостя проходит без лишних слоёв:
//! общий NT handle → `ID3D12Resource` → один полноэкранный проход (кадр гостя + оверлей) →
//! DXGI flip model.
//!
//! - waitable swapchain, `SetMaximumFrameLatency(1)`;
//! - режимы: VSync, без VSync (`DXGI_PRESENT_ALLOW_TEARING`), лимит FPS
//!   (high-resolution waitable timer); VRR через tearing + flip model;
//! - метки VBlank отдаются гостю, чтобы VSync гостя шёл от хоста;
//! - выбор адаптера (дискретная видеокарта по умолчанию на ноутбуках).
//!
//! Present-поток не ждёт UI-поток: панели egui приходят готовыми вершинами через двойной буфер.
//!
//! Реализация — этап 3.
