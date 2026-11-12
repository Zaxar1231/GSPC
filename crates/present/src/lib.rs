//! Хостовый композитор и окно (только Windows).
//!
//! Этап 3:
//! - `window` — окно Win32 с DPI awareness v2, мышь как касание, клавиатура со скан-кодами;
//! - `d3d12` — D3D12-композитор: flip model, waitable swapchain с задержкой 1 кадр, tearing
//!   для режима без VSync/VRR, копия кадра гостя через upload-буфер, оверлей метрик;
//! - `run` — поток композитора: ждёт новый кадр гостя, выводит, считает FPS/1% low/задержку.
//!
//! Present-поток не зависит от UI-потока: окно только принимает ввод.
//!
//! Этап 4: кадр придёт как общая D3D12-текстура от gfxstream, появятся шейдерный проход
//! (letterbox, масштабирование) и панели UI в стиле Material Design 3.

#[cfg(windows)]
pub mod d3d12;
#[cfg(windows)]
pub mod run;
#[cfg(windows)]
pub mod usage;
#[cfg(windows)]
pub mod window;
