//! D3D12-композитор этапа 3: кадр гостя → upload-буфер → back buffer → DXGI flip model.
//!
//! - flip-model swapchain (`FLIP_DISCARD`), 2 буфера, waitable object, `SetMaximumFrameLatency(1)`;
//! - без VSync — `DXGI_PRESENT_ALLOW_TEARING` (нужно для VRR/G-Sync/FreeSync в окне);
//! - копия кадра в back buffer через `CopyTextureRegion` из persistently-mapped upload-буфера
//!   (по одному на кадр в полёте), шейдеров нет: масштабирует DXGI (`DXGI_SCALING_STRETCH`);
//! - адаптер — самый производительный (`DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE`).
//!
//! На этапе 4 кадр будет приходить как общая D3D12-текстура от gfxstream (без копий через CPU).

use std::mem::ManuallyDrop;

use rustdroid_gpu::{Frame, PixelFormat};
use rustdroid_telemetry::overlay::{self, Canvas, OverlayData};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_FENCE_FLAG_NONE,
    D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_UPLOAD,
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0,
    D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, D3D12_RESOURCE_BARRIER_FLAG_NONE,
    D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
    D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_GENERIC_READ,
    D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATES, D3D12_RESOURCE_TRANSITION_BARRIER,
    D3D12_SUBRESOURCE_FOOTPRINT, D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    D3D12_TEXTURE_DATA_PITCH_ALIGNMENT, D3D12_TEXTURE_LAYOUT_ROW_MAJOR, D3D12CreateDevice,
    ID3D12CommandAllocator, ID3D12CommandList, ID3D12CommandQueue, ID3D12Device, ID3D12Fence,
    ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_FEATURE_PRESENT_ALLOW_TEARING,
    DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter1, IDXGIFactory6,
    IDXGISwapChain3,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObjectEx};
use windows::core::{BOOL, Interface};

/// Число back buffer'ов и кадров в полёте.
const BUFFERS: usize = 2;

/// Режим вывода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    /// VSync.
    Vsync,
    /// Без VSync (tearing; для VRR).
    Immediate,
}

/// Ошибка композитора.
#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    /// Ошибка D3D12/DXGI.
    #[error("{what}: {cause}")]
    Api {
        /// Вызов.
        what: &'static str,
        /// Причина.
        cause: windows::core::Error,
    },
    /// Кадр некорректен.
    #[error("некорректный кадр {0}×{1}")]
    BadFrame(u32, u32),
}

type Result<T> = std::result::Result<T, PresentError>;

trait Ctx<T> {
    fn ctx(self, what: &'static str) -> Result<T>;
}

impl<T> Ctx<T> for windows::core::Result<T> {
    fn ctx(self, what: &'static str) -> Result<T> {
        self.map_err(|cause| PresentError::Api { what, cause })
    }
}

struct Upload {
    resource: ID3D12Resource,
    ptr: *mut u8,
    size: usize,
}

/// D3D12-композитор одного окна.
pub struct Presenter {
    queue: ID3D12CommandQueue,
    swapchain: IDXGISwapChain3,
    waitable: HANDLE,
    allocators: Vec<ID3D12CommandAllocator>,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    fence_event: HANDLE,
    fence_value: u64,
    in_flight: [u64; BUFFERS],
    uploads: Vec<Option<Upload>>,
    size: (u32, u32, DXGI_FORMAT),
    tearing: bool,
    adapter: String,
    scratch: Vec<u8>,
    device: ID3D12Device,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("adapter", &self.adapter)
            .field("tearing", &self.tearing)
            .finish_non_exhaustive()
    }
}

fn dxgi_format(f: PixelFormat) -> DXGI_FORMAT {
    match f {
        PixelFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Rgba8 => DXGI_FORMAT_R8G8B8A8_UNORM,
    }
}

fn swap_flags(tearing: bool) -> DXGI_SWAP_CHAIN_FLAG {
    let mut f = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
    if tearing {
        f |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }
    f
}

fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // Копия интерфейса без AddRef: барьер живёт меньше `resource`.
                // SAFETY: см. `borrow`.
                pResource: unsafe { borrow(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// «Заимствует» COM-указатель без AddRef/Release для структур D3D12 с `ManuallyDrop`.
///
/// # Safety
/// Результат нельзя удалять (он в `ManuallyDrop`) и нельзя использовать дольше `resource`.
unsafe fn borrow(resource: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    // SAFETY: побитовая копия указателя интерфейса; счётчик ссылок не меняется, а
    // ManuallyDrop гарантирует, что Release не будет вызван.
    unsafe { std::mem::transmute_copy(resource) }
}

impl Presenter {
    /// Создаёт устройство, очередь и swapchain для окна.
    ///
    /// # Errors
    /// Нет адаптера с D3D12 или ошибка создания объектов.
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        // SAFETY: создание фабрики без флагов.
        let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .ctx("CreateDXGIFactory2")?;
        // SAFETY: индекс 0 — самый производительный адаптер.
        let adapter: IDXGIAdapter1 =
            unsafe { factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) }
                .ctx("EnumAdapterByGpuPreference")?;
        // SAFETY: адаптер валиден, описание пишется в локальную структуру.
        let adapter_name = unsafe { adapter.GetDesc1() }
            .map(|d| {
                String::from_utf16_lossy(&d.Description)
                    .trim_end_matches('\0')
                    .to_owned()
            })
            .unwrap_or_default();
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: выходной указатель локальный.
        unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &raw mut device) }
            .ctx("D3D12CreateDevice")?;
        let device = device.ok_or(PresentError::Api {
            what: "D3D12CreateDevice",
            cause: windows::core::Error::empty(),
        })?;

        let desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        };
        // SAFETY: описание локальное.
        let queue: ID3D12CommandQueue =
            unsafe { device.CreateCommandQueue(&raw const desc) }.ctx("CreateCommandQueue")?;

        let mut tearing = BOOL(0);
        // SAFETY: буфер — локальный BOOL нужного размера.
        let tearing = unsafe {
            factory.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&raw mut tearing).cast(),
                size_of::<BOOL>() as u32,
            )
        }
        .is_ok()
            && tearing.as_bool();

        let sc_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS as u32,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: swap_flags(tearing).0 as u32,
            ..Default::default()
        };
        // SAFETY: очередь и окно валидны, описание локальное.
        let swapchain =
            unsafe { factory.CreateSwapChainForHwnd(&queue, hwnd, &raw const sc_desc, None, None) }
                .ctx("CreateSwapChainForHwnd")?;
        // SAFETY: окно валидно; Alt+Enter обрабатываем сами (полноэкранный режим без DXGI).
        let _ = unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) };
        let swapchain: IDXGISwapChain3 = swapchain.cast().ctx("IDXGISwapChain3")?;
        // SAFETY: swapchain создан с флагом waitable object.
        unsafe { swapchain.SetMaximumFrameLatency(1) }.ctx("SetMaximumFrameLatency")?;
        // SAFETY: см. выше.
        let waitable = unsafe { swapchain.GetFrameLatencyWaitableObject() };

        let allocators = (0..BUFFERS)
            // SAFETY: устройство валидно.
            .map(|_| unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }.ctx("CreateCommandAllocator"))
            .collect::<Result<Vec<ID3D12CommandAllocator>>>()?;
        let first = allocators.first().ok_or(PresentError::BadFrame(0, 0))?;
        // SAFETY: аллокатор валиден, начальное состояние не задаётся.
        let list: ID3D12GraphicsCommandList =
            unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, first, None) }
                .ctx("CreateCommandList")?;
        // SAFETY: список только что создан в записывающем состоянии.
        unsafe { list.Close() }.ctx("Close")?;
        // SAFETY: устройство валидно.
        let fence: ID3D12Fence =
            unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.ctx("CreateFence")?;
        // SAFETY: безымянное событие.
        let fence_event = unsafe { CreateEventW(None, false, false, None) }.ctx("CreateEventW")?;

        tracing::info!(adapter = %adapter_name, tearing, "D3D12-композитор создан");
        Ok(Self {
            queue,
            swapchain,
            waitable,
            allocators,
            list,
            fence,
            fence_event,
            fence_value: 0,
            in_flight: [0; BUFFERS],
            uploads: (0..BUFFERS).map(|_| None).collect(),
            size: (width.max(1), height.max(1), DXGI_FORMAT_B8G8R8A8_UNORM),
            tearing,
            adapter: adapter_name,
            scratch: Vec::new(),
            device,
        })
    }

    /// Имя адаптера.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Поддерживается ли tearing (режим без VSync и VRR в окне).
    #[must_use]
    pub fn tearing_supported(&self) -> bool {
        self.tearing
    }

    fn wait_fence(&self, value: u64) -> Result<()> {
        // SAFETY: забор валиден.
        if value == 0 || unsafe { self.fence.GetCompletedValue() } >= value {
            return Ok(());
        }
        // SAFETY: событие валидно.
        unsafe { self.fence.SetEventOnCompletion(value, self.fence_event) }
            .ctx("SetEventOnCompletion")?;
        // SAFETY: ожидание события.
        unsafe { WaitForSingleObjectEx(self.fence_event, INFINITE, false) };
        Ok(())
    }

    fn wait_idle(&self) -> Result<()> {
        for v in self.in_flight {
            self.wait_fence(v)?;
        }
        Ok(())
    }

    /// Ждёт, пока swapchain готов принять кадр (минимальная задержка).
    pub fn wait_for_slot(&self, timeout_ms: u32) -> bool {
        // SAFETY: waitable object принадлежит swapchain.
        let r = unsafe { WaitForSingleObjectEx(self.waitable, timeout_ms, true) };
        r == WAIT_OBJECT_0
    }

    fn ensure_size(&mut self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<()> {
        if self.size == (w, h, format) {
            return Ok(());
        }
        self.wait_idle()?;
        // SAFETY: все кадры завершены, ссылок на back buffer'ы нет.
        unsafe {
            self.swapchain
                .ResizeBuffers(BUFFERS as u32, w, h, format, swap_flags(self.tearing))
        }
        .ctx("ResizeBuffers")?;
        self.size = (w, h, format);
        tracing::info!(w, h, "размер swapchain изменён");
        Ok(())
    }

    fn upload(&mut self, slot: usize, size: usize) -> Result<(ID3D12Resource, *mut u8)> {
        if let Some(Some(u)) = self.uploads.get(slot)
            && u.size >= size
        {
            return Ok((u.resource.clone(), u.ptr));
        }
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut res: Option<ID3D12Resource> = None;
        // SAFETY: описания локальные, выходной указатель локальный.
        unsafe {
            self.device.CreateCommittedResource(
                &raw const heap,
                D3D12_HEAP_FLAG_NONE,
                &raw const desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &raw mut res,
            )
        }
        .ctx("CreateCommittedResource")?;
        let res = res.ok_or(PresentError::Api {
            what: "CreateCommittedResource",
            cause: windows::core::Error::empty(),
        })?;
        let mut ptr = std::ptr::null_mut();
        // SAFETY: upload-ресурс можно держать отображённым всё время жизни.
        unsafe { res.Map(0, None, Some(&raw mut ptr)) }.ctx("Map")?;
        if let Some(slot) = self.uploads.get_mut(slot) {
            *slot = Some(Upload {
                resource: res.clone(),
                ptr: ptr.cast(),
                size,
            });
        }
        Ok((res, ptr.cast()))
    }

    /// Выводит кадр (с оверлеем, если задан).
    ///
    /// # Errors
    /// Ошибка D3D12/DXGI или некорректный кадр.
    pub fn present(
        &mut self,
        frame: &Frame,
        overlay_data: Option<&OverlayData>,
        mode: PresentMode,
    ) -> Result<()> {
        let (w, h) = (frame.width, frame.height);
        let stride = frame.stride as usize;
        if w == 0 || h == 0 || stride < w as usize * 4 || frame.data.len() < stride * h as usize {
            return Err(PresentError::BadFrame(w, h));
        }
        let format = dxgi_format(frame.format);
        self.ensure_size(w, h, format)?;

        // SAFETY: swapchain валиден.
        let index = unsafe { self.swapchain.GetCurrentBackBufferIndex() } as usize % BUFFERS;
        self.wait_fence(self.in_flight[index])?;

        let pitch_align = D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize;
        let row_pitch = (w as usize * 4).div_ceil(pitch_align) * pitch_align;
        let (upload, ptr) = self.upload(index, row_pitch * h as usize)?;

        // Копия строк кадра в upload-буфер (write-combined: только запись, без чтения).
        let row = w as usize * 4;
        for y in 0..h as usize {
            let src = frame.data.get(y * stride..y * stride + row).unwrap_or(&[]);
            // SAFETY: смещение y * row_pitch < row_pitch * h — внутри upload-буфера.
            let dst = unsafe { ptr.add(y * row_pitch) };
            // SAFETY: в строке upload-буфера ≥ row_pitch ≥ src.len() байт, области не пересекаются.
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) };
        }
        if let Some(data) = overlay_data {
            self.draw_overlay(frame, data, ptr, row_pitch);
        }

        // SAFETY: back buffer с этим индексом существует.
        let back: ID3D12Resource =
            unsafe { self.swapchain.GetBuffer(index as u32) }.ctx("GetBuffer")?;
        let alloc = self
            .allocators
            .get(index)
            .ok_or(PresentError::BadFrame(w, h))?;
        // SAFETY: кадр, использовавший аллокатор, завершён (ждали забор выше).
        unsafe { alloc.Reset() }.ctx("CommandAllocator::Reset")?;
        // SAFETY: список закрыт, аллокатор сброшен.
        unsafe { self.list.Reset(alloc, None) }.ctx("CommandList::Reset")?;

        let dst = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: структура живёт до конца функции, `back` — дольше.
            pResource: unsafe { borrow(&back) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: как выше.
            pResource: unsafe { borrow(&upload) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: w,
                        Height: h,
                        Depth: 1,
                        RowPitch: row_pitch as u32,
                    },
                },
            },
        };
        let to_copy = [transition(
            &back,
            D3D12_RESOURCE_STATE_PRESENT,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )];
        let to_present = [transition(
            &back,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PRESENT,
        )];
        // SAFETY: список в записывающем состоянии, барьер ссылается на живой back buffer.
        unsafe { self.list.ResourceBarrier(&to_copy) };
        // SAFETY: описания копирования локальные, ресурсы живы до конца функции.
        unsafe {
            self.list
                .CopyTextureRegion(&raw const dst, 0, 0, 0, &raw const src, None)
        };
        // SAFETY: как для первого барьера.
        unsafe { self.list.ResourceBarrier(&to_present) };
        // SAFETY: запись команд завершена.
        unsafe { self.list.Close() }.ctx("Close")?;
        let lists = [Some(
            self.list
                .cast::<ID3D12CommandList>()
                .ctx("ID3D12CommandList")?,
        )];
        // SAFETY: список закрыт.
        unsafe { self.queue.ExecuteCommandLists(&lists) };

        let (sync, flags) = match mode {
            PresentMode::Vsync => (1, DXGI_PRESENT(0)),
            PresentMode::Immediate if self.tearing => (0, DXGI_PRESENT_ALLOW_TEARING),
            PresentMode::Immediate => (0, DXGI_PRESENT(0)),
        };
        // SAFETY: swapchain валиден.
        unsafe { self.swapchain.Present(sync, flags) }
            .ok()
            .ctx("Present")?;

        self.fence_value += 1;
        // SAFETY: забор и очередь валидны.
        unsafe { self.queue.Signal(&self.fence, self.fence_value) }.ctx("Signal")?;
        if let Some(v) = self.in_flight.get_mut(index) {
            *v = self.fence_value;
        }
        Ok(())
    }

    /// Оверлей: копия угла кадра в CPU-буфер, отрисовка, запись в upload-буфер.
    fn draw_overlay(&mut self, frame: &Frame, data: &OverlayData, ptr: *mut u8, row_pitch: usize) {
        let scale = if frame.height >= 1440 {
            3
        } else if frame.height >= 900 {
            2
        } else {
            1
        };
        let rw = (300 * scale).min(frame.width as usize);
        let rh = ((data.lines.len() * 10 + 48) * scale).min(frame.height as usize);
        let stride = frame.stride as usize;
        self.scratch.resize(rw * rh * 4, 0);
        for y in 0..rh {
            let src = frame
                .data
                .get(y * stride..y * stride + rw * 4)
                .unwrap_or(&[]);
            if let Some(dst) = self.scratch.get_mut(y * rw * 4..y * rw * 4 + src.len()) {
                dst.copy_from_slice(src);
            }
        }
        let mut canvas = Canvas {
            data: &mut self.scratch,
            width: rw,
            height: rh,
            stride: rw * 4,
        };
        overlay::draw(&mut canvas, data, scale);
        for y in 0..rh {
            let src = self
                .scratch
                .get(y * rw * 4..(y + 1) * rw * 4)
                .unwrap_or(&[]);
            // SAFETY: строка y < h — внутри upload-буфера.
            let dst = unsafe { ptr.add(y * row_pitch) };
            // SAFETY: rw ≤ w, поэтому src.len() ≤ row_pitch; области не пересекаются.
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) };
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        let _ = self.wait_idle();
        // SAFETY: событие создано в `new` и закрывается один раз.
        let _ = unsafe { CloseHandle(self.fence_event) };
        // SAFETY: waitable object swapchain закрывает владелец (документация DXGI).
        let _ = unsafe { CloseHandle(self.waitable) };
    }
}
