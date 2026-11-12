//! [`Backend3d`] поверх gfxstream (`gfxstream_backend.dll`, только Windows).
//!
//! Переводит вызовы устройства в C API `stream_renderer_*`: backing ресурсов передаётся как
//! `iovec` с указателями хоста на гостевую RAM ([`GuestMemory::host_range`]), фенсы gfxstream
//! приходят в [`FenceSink`] из его потоков.

use std::path::Path;
use std::sync::Arc;

use rustdroid_gfxstream::{CAPSETS, Config, Library, Renderer, Transfer, ffi};
use rustdroid_virtio::GuestMemory;

use crate::backend::{
    Backend3d, BackendError, BackendResult, Blob, CapsetInfo, FenceId, FenceSink, MemEntry,
    Resource3d, Transfer3d,
};

/// 3D-бэкенд gfxstream.
pub struct GfxstreamBackend {
    renderer: Renderer,
    capsets: Vec<CapsetInfo>,
}

impl std::fmt::Debug for GfxstreamBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GfxstreamBackend")
            .field("capsets", &self.capsets)
            .finish_non_exhaustive()
    }
}

fn failed(call: &str, e: &rustdroid_gfxstream::Error) -> BackendError {
    tracing::debug!(call, error = %e, "вызов gfxstream не удался");
    BackendError::Failed
}

/// Backing гостя как `iovec`. `None` — кусок вне RAM.
fn iovecs(backing: &[MemEntry], mem: &dyn GuestMemory) -> Option<Vec<ffi::iovec>> {
    backing
        .iter()
        .map(|e| {
            let len = e.len as usize;
            let ptr = mem.host_range(e.addr, len)?;
            Some(ffi::iovec {
                iov_base: ptr.as_ptr().cast(),
                iov_len: len,
            })
        })
        .collect()
}

impl GfxstreamBackend {
    /// Загружает DLL `library` и инициализирует gfxstream; фенсы уходят в `fences`.
    ///
    /// # Errors
    /// DLL нет, она несовместима или gfxstream не инициализировался (например, нет Vulkan).
    pub fn new(
        library: &Path,
        config: Config,
        fences: Arc<FenceSink>,
    ) -> Result<Self, rustdroid_gfxstream::Error> {
        let lib = Library::load(library)?;
        let renderer = Renderer::init(
            lib,
            config,
            Box::new(move |f| {
                fences.signal(FenceId {
                    fence_id: f.fence_id,
                    ctx_id: f.ctx_id,
                    ring_idx: f.ring_idx,
                });
            }),
        )?;
        let capsets: Vec<CapsetInfo> = CAPSETS
            .iter()
            .filter_map(|&id| {
                renderer
                    .capset_info(id)
                    .map(|(max_version, max_size)| CapsetInfo {
                        id,
                        max_version,
                        max_size,
                    })
            })
            .collect();
        tracing::info!(?capsets, "gfxstream инициализирован");
        Ok(Self { renderer, capsets })
    }
}

impl Backend3d for GfxstreamBackend {
    fn num_capsets(&self) -> u32 {
        u32::try_from(self.capsets.len()).unwrap_or(0)
    }

    fn capset_info(&self, index: u32) -> Option<CapsetInfo> {
        self.capsets.get(index as usize).copied()
    }

    fn capset(&self, id: u32, version: u32) -> Option<Vec<u8>> {
        let info = self.capsets.iter().find(|c| c.id == id)?;
        if version > info.max_version {
            return None;
        }
        Some(self.renderer.capset(id, version))
    }

    fn context_create(&mut self, ctx_id: u32, context_init: u32, name: &[u8]) -> BackendResult {
        self.renderer
            .context_create(ctx_id, name, context_init)
            .map_err(|e| failed("context_create", &e))
    }

    fn context_destroy(&mut self, ctx_id: u32) {
        self.renderer.context_destroy(ctx_id);
    }

    fn context_attach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.renderer.ctx_attach_resource(ctx_id, resource_id);
    }

    fn context_detach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.renderer.ctx_detach_resource(ctx_id, resource_id);
    }

    fn resource_create_3d(&mut self, resource_id: u32, a: &Resource3d) -> BackendResult {
        let args = ffi::stream_renderer_resource_create_args {
            handle: resource_id,
            target: a.target,
            format: a.format,
            bind: a.bind,
            width: a.width,
            height: a.height,
            depth: a.depth,
            array_size: a.array_size,
            last_level: a.last_level,
            nr_samples: a.nr_samples,
            flags: a.flags,
        };
        // SAFETY: iov пустой — памяти гостя ресурс пока не получает (её привяжет
        // ATTACH_BACKING), поэтому требование к времени жизни буферов выполнено.
        unsafe { self.renderer.resource_create(&args, &mut []) }
            .map_err(|e| failed("resource_create", &e))
    }

    fn resource_create_blob(
        &mut self,
        resource_id: u32,
        blob: &Blob,
        backing: &[MemEntry],
        mem: &dyn GuestMemory,
    ) -> BackendResult {
        let iov = iovecs(backing, mem).ok_or(BackendError::InvalidParameter)?;
        let args = ffi::stream_renderer_create_blob {
            blob_mem: blob.blob_mem,
            blob_flags: blob.blob_flags,
            blob_id: blob.blob_id,
            size: blob.size,
        };
        // SAFETY: iov указывает в гостевую RAM. Устройство вместе с этим бэкендом
        // удаляется раньше памяти (`VirtioPci` хранит устройство перед `mem`, а RAM живёт до
        // конца ВМ), а teardown gfxstream в `Renderer::drop` прекращает доступ к ней.
        unsafe {
            self.renderer
                .create_blob(blob.ctx_id, resource_id, &args, &iov)
        }
        .map_err(|e| failed("create_blob", &e))
    }

    fn attach_backing(
        &mut self,
        resource_id: u32,
        backing: &[MemEntry],
        mem: &dyn GuestMemory,
    ) -> BackendResult {
        let mut iov = iovecs(backing, mem).ok_or(BackendError::InvalidParameter)?;
        // SAFETY: как в `resource_create_blob`: гостевая RAM переживает gfxstream.
        unsafe { self.renderer.resource_attach_iov(resource_id, &mut iov) }
            .map_err(|e| failed("resource_attach_iov", &e))
    }

    fn detach_backing(&mut self, resource_id: u32) {
        self.renderer.resource_detach_iov(resource_id);
    }

    fn resource_unref(&mut self, resource_id: u32) {
        self.renderer.resource_unref(resource_id);
    }

    fn transfer(&mut self, t: &Transfer3d, to_host: bool) -> BackendResult {
        let r = t.region;
        let transfer = Transfer {
            handle: t.resource_id,
            ctx_id: t.ctx_id,
            level: t.level,
            stride: t.stride,
            layer_stride: t.layer_stride,
            region: ffi::stream_renderer_box {
                x: r.x,
                y: r.y,
                z: r.z,
                w: r.w,
                h: r.h,
                d: r.d,
            },
            offset: t.offset,
        };
        self.renderer
            .transfer(to_host, &transfer)
            .map_err(|e| failed("transfer", &e))
    }

    fn submit(&mut self, ctx_id: u32, _ring_idx: Option<u8>, cmd: &mut [u8]) -> BackendResult {
        // Кольцо gfxstream берёт из самого потока команд; ring_idx нужен только фенсам.
        self.renderer
            .submit(ctx_id, cmd)
            .map_err(|e| failed("submit_cmd", &e))
    }

    fn create_fence(&mut self, f: FenceId) -> BackendResult {
        self.renderer
            .create_fence(f.fence_id, f.ctx_id, f.ring_idx)
            .map_err(|e| failed("create_fence", &e))
    }

    fn read_pixels(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
        out: &mut [u8],
    ) -> BackendResult {
        let transfer = Transfer {
            handle: resource_id,
            ctx_id: 0,
            level: 0,
            stride: width.saturating_mul(4),
            layer_stride: 0,
            region: ffi::stream_renderer_box {
                x: 0,
                y: 0,
                z: 0,
                w: width,
                h: height,
                d: 1,
            },
            offset: 0,
        };
        self.renderer
            .transfer_read_into(&transfer, out)
            .map_err(|e| failed("transfer_read_iov", &e))
    }
}
