//! virtio-gpu (virtio 1.3, раздел 5.7): 2D-ресурсы, scanout, EDID и 3D-контексты.
//!
//! **2D.** Гость рисует программно (SwiftShader), изменённые прямоугольники копируются из
//! гостевой памяти в ресурс, по `RESOURCE_FLUSH` кадр уходит композитору.
//!
//! **3D** (если устройство создано с [`Backend3d`]): capsets, контексты (`CONTEXT_INIT`),
//! 3D- и blob-ресурсы, `SUBMIT_3D`. Команды исполняет бэкенд (gfxstream), устройство проверяет
//! аргументы и ведёт учёт. Команда с фенсом возвращается гостю только после того, как бэкенд
//! сообщит о выполнении фенса через [`FenceSink`]; до этого её цепочка дескрипторов придержана.
//!
//! Host-visible blob-ресурсы (`RESOURCE_MAP_BLOB`) отображаются в область общей памяти
//! устройства (BAR2, [`ShmMapper`]), если она задана ([`VirtioGpu::with_shm`]). Кадр
//! 3D-ресурса пока читается через CPU (zero-copy — задача 4.5).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use rustdroid_virtio::device::TYPE_GPU;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

use crate::backend::{
    Backend3d, BackendError, Blob, Box3d, FenceId, FenceSink, MemEntry, Resource3d, ShmMapper,
    Transfer3d,
};
use crate::edid::{self, DisplayMode};
use crate::scanout::{Frame, Mailbox, PixelFormat};

/// Возможность: 3D-команды (исторически «virgl»; нужна ядру для контекстов и `SUBMIT_3D`).
const VIRTIO_GPU_F_VIRGL: u64 = 1 << 0;
/// Возможность: EDID.
const VIRTIO_GPU_F_EDID: u64 = 1 << 1;
/// Возможность: blob-ресурсы.
const VIRTIO_GPU_F_RESOURCE_BLOB: u64 = 1 << 3;
/// Возможность: тип контекста в `CTX_CREATE` и фенсы по кольцам.
const VIRTIO_GPU_F_CONTEXT_INIT: u64 = 1 << 4;

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_CAPSET_INFO: u32 = 0x0108;
const CMD_GET_CAPSET: u32 = 0x0109;
const CMD_GET_EDID: u32 = 0x010A;
const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010C;
const CMD_SET_SCANOUT_BLOB: u32 = 0x010D;
const CMD_CTX_CREATE: u32 = 0x0200;
const CMD_CTX_DESTROY: u32 = 0x0201;
const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
const CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
const CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const CMD_SUBMIT_3D: u32 = 0x0207;
const CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
const CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_CAPSET_INFO: u32 = 0x1102;
const RESP_OK_CAPSET: u32 = 0x1103;
const RESP_OK_MAP_INFO: u32 = 0x1106;
const RESP_OK_EDID: u32 = 0x1104;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

const FLAG_FENCE: u32 = 1;
/// В заголовке задано кольцо контекста (`ring_idx`) для фенса.
const FLAG_INFO_RING_IDX: u32 = 2;
/// Смещение `ring_idx` в заголовке.
const HDR_RING_IDX: usize = 20;
/// Номер области общей памяти host-visible (`VIRTIO_GPU_SHM_ID_HOST_VISIBLE`).
const SHM_ID_HOST_VISIBLE: u8 = 1;
const PAGE: u64 = 4096;
/// Длина имени контекста в `CTX_CREATE`.
const CTX_NAME_LEN: usize = 64;
const HDR: usize = 24;
const MAX_SCANOUTS: usize = 16;
/// Предел памяти под ресурсы хоста (защита от гостя, просящего гигабайты).
const MAX_RESOURCE_BYTES: u64 = 1 << 30;

/// Очередь управления.
pub const CONTROL_QUEUE: usize = 0;
/// Очередь курсора.
pub const CURSOR_QUEUE: usize = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

struct Resource {
    width: u32,
    height: u32,
    format: PixelFormat,
    data: Vec<u8>,
    backing: Vec<(u64, u32)>,
}

impl Resource {
    fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

fn format_from_virtio(f: u32) -> Option<PixelFormat> {
    match f {
        // B8G8R8A8, B8G8R8X8 (= DRM XRGB8888 / ARGB8888)
        1 | 2 => Some(PixelFormat::Bgra8),
        // R8G8B8A8, R8G8B8X8, A8B8G8R8/X8B8G8R8 (байты R,G,B,A)
        67 | 134 | 121 | 68 => Some(PixelFormat::Rgba8),
        _ => None,
    }
}

/// 3D-ресурс: данные у бэкенда, здесь — то, что нужно для показа.
#[derive(Debug, Clone, Copy, Default)]
struct Res3d {
    width: u32,
    height: u32,
    format: Option<PixelFormat>,
}

/// Команда, ждущая фенса.
#[derive(Debug, Clone, Copy)]
struct PendingFence {
    head: u16,
    written: u32,
    fence: FenceId,
}

/// Область общей памяти и отображённые в неё blob: ресурс → (смещение, длина).
struct Shm {
    size: u64,
    mapper: Arc<dyn ShmMapper>,
    mapped: HashMap<u32, (u64, u64)>,
}

impl Shm {
    fn unmap(&mut self, id: u32) -> bool {
        match self.mapped.remove(&id) {
            Some((offset, len)) => {
                self.mapper.unmap(offset, len);
                true
            }
            None => false,
        }
    }

    fn unmap_all(&mut self) {
        for (_, (offset, len)) in self.mapped.drain() {
            self.mapper.unmap(offset, len);
        }
    }
}

/// Состояние 3D-части.
struct ThreeD {
    backend: Box<dyn Backend3d>,
    fences: Arc<FenceSink>,
    contexts: HashSet<u32>,
    resources: HashMap<u32, Res3d>,
    pending: Vec<PendingFence>,
    shm: Option<Shm>,
}

impl ThreeD {
    /// Снимает отображение blob в госте и на хосте, если оно есть.
    fn unmap_blob(&mut self, id: u32) -> bool {
        let was = self.shm.as_mut().is_some_and(|s| s.unmap(id));
        if was {
            self.backend.unmap_blob(id);
        }
        was
    }

    /// `RESOURCE_MAP_BLOB`: blob по смещению `offset` области. Возвращает `map_info`.
    fn map_blob(&mut self, id: u32, offset: u64) -> Result<u32, u32> {
        let Some(shm) = self.shm.as_mut() else {
            return Err(RESP_ERR_UNSPEC);
        };
        if !self.resources.contains_key(&id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        if shm.mapped.contains_key(&id) || !offset.is_multiple_of(PAGE) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let m = self.backend.map_blob(id).map_err(backend_status)?;
        let len = m.size.div_ceil(PAGE) * PAGE;
        let fits = offset.checked_add(len).is_some_and(|end| end <= shm.size);
        let overlaps = shm
            .mapped
            .values()
            .any(|&(o, l)| offset < o + l && o < offset + len);
        if len == 0 || !fits || overlaps || !m.ptr.addr().get().is_multiple_of(PAGE as usize) {
            tracing::debug!(
                id,
                offset,
                size = m.size,
                "blob нельзя отобразить в эту область"
            );
            self.backend.unmap_blob(id);
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        // SAFETY: память blob выделена бэкендом и остаётся выделенной, пока мы не вызовем
        // backend.unmap_blob/resource_unref — а это делается только после mapper.unmap
        // (Shm::unmap, unmap_all в reset и Drop устройства).
        if let Err(e) = unsafe { shm.mapper.map(offset, m.ptr, len) } {
            tracing::warn!(id, offset, len, error = %e, "blob не отображён в гостя");
            self.backend.unmap_blob(id);
            return Err(RESP_ERR_UNSPEC);
        }
        shm.mapped.insert(id, (offset, len));
        Ok(m.map_info)
    }
}

fn backend_status(e: BackendError) -> u32 {
    match e {
        BackendError::InvalidResource => RESP_ERR_INVALID_RESOURCE_ID,
        BackendError::InvalidContext => RESP_ERR_INVALID_CONTEXT_ID,
        BackendError::InvalidParameter => RESP_ERR_INVALID_PARAMETER,
        BackendError::OutOfMemory => RESP_ERR_OUT_OF_MEMORY,
        BackendError::Failed => RESP_ERR_UNSPEC,
    }
}

/// Устройство virtio-gpu (один scanout; 3D — если задан бэкенд).
pub struct VirtioGpu {
    mode: DisplayMode,
    mailbox: Arc<Mailbox>,
    resources: HashMap<u32, Resource>,
    three_d: Option<ThreeD>,
    scanout_resource: Option<u32>,
    resource_bytes: u64,
    sizes: [u16; 2],
    chain: Chain,
    req: Vec<u8>,
    resp: Vec<u8>,
}

impl std::fmt::Debug for VirtioGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioGpu")
            .field("mode", &self.mode)
            .field("resources", &self.resources.len())
            .field("three_d", &self.three_d.is_some())
            .finish_non_exhaustive()
    }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    b.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}
fn mem_entries(b: &[u8], off: usize, n: usize) -> Option<Vec<MemEntry>> {
    let end = n.checked_mul(16)?.checked_add(off)?;
    if b.len() < end {
        return None;
    }
    Some(
        (0..n)
            .map(|i| MemEntry {
                addr: u64_at(b, off + i * 16),
                len: u32_at(b, off + 8 + i * 16),
            })
            .collect(),
    )
}

/// Заголовок ответа: тип, флаги фенса, номер фенса, контекст и кольцо из запроса.
fn begin_response(req: &[u8], resp: &mut Vec<u8>, kind: u32) {
    let flags = u32_at(req, 4) & (FLAG_FENCE | FLAG_INFO_RING_IDX);
    let fence = if flags & FLAG_FENCE != 0 {
        u64_at(req, 8)
    } else {
        0
    };
    let ring = if flags & FLAG_INFO_RING_IDX != 0 {
        req.get(HDR_RING_IDX).copied().unwrap_or(0)
    } else {
        0
    };
    resp.clear();
    resp.extend_from_slice(&kind.to_le_bytes());
    resp.extend_from_slice(&flags.to_le_bytes());
    resp.extend_from_slice(&fence.to_le_bytes());
    resp.extend_from_slice(&u32_at(req, 16).to_le_bytes());
    resp.extend_from_slice(&[ring, 0, 0, 0]);
}

fn rect_at(b: &[u8], off: usize) -> Rect {
    Rect {
        x: u32_at(b, off),
        y: u32_at(b, off + 4),
        w: u32_at(b, off + 8),
        h: u32_at(b, off + 12),
    }
}

impl VirtioGpu {
    /// Устройство с режимом дисплея `mode`; кадры публикуются в `mailbox`.
    #[must_use]
    pub fn new(mode: DisplayMode, mailbox: Arc<Mailbox>) -> Self {
        Self {
            mode,
            mailbox,
            resources: HashMap::new(),
            three_d: None,
            scanout_resource: None,
            resource_bytes: 0,
            sizes: [256, 16],
            chain: Chain::default(),
            req: Vec::new(),
            resp: Vec::new(),
        }
    }

    /// Устройство с 3D: команды контекстов исполняет `backend`, о фенсах он сообщает в `fences`.
    #[must_use]
    pub fn with_3d(
        mode: DisplayMode,
        mailbox: Arc<Mailbox>,
        backend: Box<dyn Backend3d>,
        fences: Arc<FenceSink>,
    ) -> Self {
        let mut gpu = Self::new(mode, mailbox);
        gpu.three_d = Some(ThreeD {
            backend,
            fences,
            contexts: HashSet::new(),
            resources: HashMap::new(),
            pending: Vec::new(),
            shm: None,
        });
        gpu
    }

    /// Добавляет область общей памяти `size` байт (степень двойки) для host-visible blob.
    /// Без 3D-бэкенда ничего не делает.
    #[must_use]
    pub fn with_shm(mut self, size: u64, mapper: Arc<dyn ShmMapper>) -> Self {
        if let Some(t) = self.three_d.as_mut() {
            t.shm = Some(Shm {
                size,
                mapper,
                mapped: HashMap::new(),
            });
        }
        self
    }

    /// Собирает читаемую часть запроса в `self.req`.
    fn gather(&mut self, mem: &dyn GuestMemory) -> Result<(), QueueError> {
        self.req.clear();
        for d in self.chain.descriptors.iter().filter(|d| !d.is_write()) {
            let start = self.req.len();
            self.req.resize(start + d.len as usize, 0);
            if let Some(buf) = self.req.get_mut(start..) {
                mem.read(d.addr, buf)?;
            }
        }
        Ok(())
    }

    /// Пишет `self.resp` в записываемые дескрипторы; возвращает число записанных байт.
    fn scatter(&self, mem: &dyn GuestMemory) -> u32 {
        let mut off = 0_usize;
        for d in self.chain.descriptors.iter().filter(|d| d.is_write()) {
            if off >= self.resp.len() {
                break;
            }
            let n = (d.len as usize).min(self.resp.len() - off);
            if mem
                .write(d.addr, self.resp.get(off..off + n).unwrap_or(&[]))
                .is_err()
            {
                break;
            }
            off += n;
        }
        u32::try_from(off).unwrap_or(u32::MAX)
    }

    fn begin_response(&mut self, kind: u32) {
        begin_response(&self.req, &mut self.resp, kind);
    }

    fn is_3d_resource(&self, id: u32) -> bool {
        self.three_d
            .as_ref()
            .is_some_and(|t| t.resources.contains_key(&id))
    }

    fn command(&mut self, mem: &dyn GuestMemory) -> u32 {
        if self.req.len() < HDR {
            return RESP_ERR_UNSPEC;
        }
        let kind = u32_at(&self.req, 0);
        if let Some(status) = self.command_3d(kind, mem) {
            if status == RESP_OK_NODATA {
                self.begin_response(RESP_OK_NODATA);
            }
            return status;
        }
        let b = HDR;
        match kind {
            CMD_GET_DISPLAY_INFO => {
                self.begin_response(RESP_OK_DISPLAY_INFO);
                for i in 0..MAX_SCANOUTS {
                    let (w, h, en) = if i == 0 {
                        (self.mode.width, self.mode.height, 1)
                    } else {
                        (0, 0, 0)
                    };
                    for v in [0, 0, w, h, en, 0] {
                        self.resp.extend_from_slice(&u32::to_le_bytes(v));
                    }
                }
                return RESP_OK_DISPLAY_INFO;
            }
            CMD_GET_EDID => {
                if u32_at(&self.req, b) != 0 {
                    return RESP_ERR_INVALID_SCANOUT_ID;
                }
                self.begin_response(RESP_OK_EDID);
                let e = edid::generate(&self.mode);
                self.resp.extend_from_slice(&(e.len() as u32).to_le_bytes());
                self.resp.extend_from_slice(&[0; 4]);
                self.resp.extend_from_slice(&e);
                self.resp.resize(HDR + 8 + 1024, 0);
                return RESP_OK_EDID;
            }
            CMD_RESOURCE_CREATE_2D => {
                let (id, fmt, w, h) = (
                    u32_at(&self.req, b),
                    u32_at(&self.req, b + 4),
                    u32_at(&self.req, b + 8),
                    u32_at(&self.req, b + 12),
                );
                let Some(format) = format_from_virtio(fmt) else {
                    return RESP_ERR_INVALID_PARAMETER;
                };
                if id == 0
                    || self.resources.contains_key(&id)
                    || self.is_3d_resource(id)
                    || w == 0
                    || h == 0
                {
                    return RESP_ERR_INVALID_RESOURCE_ID;
                }
                let bytes = u64::from(w) * u64::from(h) * 4;
                if self.resource_bytes + bytes > MAX_RESOURCE_BYTES {
                    return RESP_ERR_OUT_OF_MEMORY;
                }
                let Ok(len) = usize::try_from(bytes) else {
                    return RESP_ERR_OUT_OF_MEMORY;
                };
                self.resource_bytes += bytes;
                self.resources.insert(
                    id,
                    Resource {
                        width: w,
                        height: h,
                        format,
                        data: vec![0; len],
                        backing: Vec::new(),
                    },
                );
            }
            CMD_RESOURCE_UNREF => {
                let id = u32_at(&self.req, b);
                match self.resources.remove(&id) {
                    Some(r) => {
                        self.resource_bytes -= r.data.len() as u64;
                        if self.scanout_resource == Some(id) {
                            self.scanout_resource = None;
                        }
                    }
                    None => return RESP_ERR_INVALID_RESOURCE_ID,
                }
            }
            CMD_RESOURCE_ATTACH_BACKING => {
                let id = u32_at(&self.req, b);
                let n = u32_at(&self.req, b + 4) as usize;
                let Some(entries) = mem_entries(&self.req, b + 8, n) else {
                    return RESP_ERR_INVALID_PARAMETER;
                };
                match self.resources.get_mut(&id) {
                    Some(r) => r.backing = entries.iter().map(|e| (e.addr, e.len)).collect(),
                    None => return RESP_ERR_INVALID_RESOURCE_ID,
                }
            }
            CMD_RESOURCE_DETACH_BACKING => match self.resources.get_mut(&u32_at(&self.req, b)) {
                Some(r) => r.backing.clear(),
                None => return RESP_ERR_INVALID_RESOURCE_ID,
            },
            CMD_SET_SCANOUT => {
                let scanout = u32_at(&self.req, b + 16);
                let id = u32_at(&self.req, b + 20);
                if scanout != 0 {
                    return RESP_ERR_INVALID_SCANOUT_ID;
                }
                if id == 0 {
                    self.scanout_resource = None;
                } else if self.resources.contains_key(&id) {
                    self.scanout_resource = Some(id);
                } else {
                    return RESP_ERR_INVALID_RESOURCE_ID;
                }
            }
            CMD_TRANSFER_TO_HOST_2D => {
                let r = rect_at(&self.req, b);
                let offset = u64_at(&self.req, b + 16);
                let id = u32_at(&self.req, b + 24);
                let Some(res) = self.resources.get_mut(&id) else {
                    return RESP_ERR_INVALID_RESOURCE_ID;
                };
                if !transfer(res, r, offset, mem) {
                    return RESP_ERR_INVALID_PARAMETER;
                }
            }
            CMD_RESOURCE_FLUSH => {
                let id = u32_at(&self.req, b + 16);
                if !self.resources.contains_key(&id) {
                    return RESP_ERR_INVALID_RESOURCE_ID;
                }
                if self.scanout_resource == Some(id) {
                    self.publish(id);
                }
            }
            _ => return RESP_ERR_UNSPEC,
        }
        self.begin_response(RESP_OK_NODATA);
        RESP_OK_NODATA
    }

    /// 3D-команды и команды над 3D-ресурсами. `None` — команда не 3D (или 3D выключено),
    /// её обрабатывает 2D-путь. `RESP_OK_NODATA` — успех без данных (заголовок ответа
    /// допишет вызывающий); прочие `RESP_OK_*` — ответ уже собран в `resp`.
    #[allow(clippy::too_many_lines, reason = "плоский разбор команд протокола")]
    fn command_3d(&mut self, kind: u32, mem: &dyn GuestMemory) -> Option<u32> {
        let Self {
            req,
            resp,
            three_d,
            resources,
            scanout_resource,
            mailbox,
            ..
        } = self;
        let t = three_d.as_mut()?;
        let b = HDR;
        let ctx = u32_at(req, 16);
        let res_arg = u32_at(req, b);
        let status = match kind {
            CMD_GET_CAPSET_INFO => {
                let Some(info) = t.backend.capset_info(res_arg) else {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                };
                begin_response(req, resp, RESP_OK_CAPSET_INFO);
                for v in [info.id, info.max_version, info.max_size, 0] {
                    resp.extend_from_slice(&v.to_le_bytes());
                }
                RESP_OK_CAPSET_INFO
            }
            CMD_GET_CAPSET => {
                let Some(data) = t.backend.capset(u32_at(req, b), u32_at(req, b + 4)) else {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                };
                begin_response(req, resp, RESP_OK_CAPSET);
                resp.extend_from_slice(&data);
                RESP_OK_CAPSET
            }
            CMD_CTX_CREATE => {
                if ctx == 0 || t.contexts.contains(&ctx) {
                    return Some(RESP_ERR_INVALID_CONTEXT_ID);
                }
                let nlen = (u32_at(req, b) as usize).min(CTX_NAME_LEN);
                let name = req.get(b + 8..b + 8 + nlen).unwrap_or(&[]);
                if let Err(e) = t.backend.context_create(ctx, u32_at(req, b + 4), name) {
                    return Some(backend_status(e));
                }
                t.contexts.insert(ctx);
                RESP_OK_NODATA
            }
            CMD_CTX_DESTROY => {
                if !t.contexts.remove(&ctx) {
                    return Some(RESP_ERR_INVALID_CONTEXT_ID);
                }
                t.backend.context_destroy(ctx);
                RESP_OK_NODATA
            }
            CMD_CTX_ATTACH_RESOURCE | CMD_CTX_DETACH_RESOURCE => {
                if !t.contexts.contains(&ctx) {
                    return Some(RESP_ERR_INVALID_CONTEXT_ID);
                }
                if !t.resources.contains_key(&res_arg) {
                    return Some(RESP_ERR_INVALID_RESOURCE_ID);
                }
                if kind == CMD_CTX_ATTACH_RESOURCE {
                    t.backend.context_attach_resource(ctx, res_arg);
                } else {
                    t.backend.context_detach_resource(ctx, res_arg);
                }
                RESP_OK_NODATA
            }
            CMD_RESOURCE_CREATE_3D => {
                let v = |i: usize| u32_at(req, b + 4 * i);
                let args = Resource3d {
                    target: v(1),
                    format: v(2),
                    bind: v(3),
                    width: v(4),
                    height: v(5),
                    depth: v(6),
                    array_size: v(7),
                    last_level: v(8),
                    nr_samples: v(9),
                    flags: v(10),
                };
                if res_arg == 0
                    || resources.contains_key(&res_arg)
                    || t.resources.contains_key(&res_arg)
                {
                    return Some(RESP_ERR_INVALID_RESOURCE_ID);
                }
                if let Err(e) = t.backend.resource_create_3d(res_arg, &args) {
                    return Some(backend_status(e));
                }
                t.resources.insert(
                    res_arg,
                    Res3d {
                        width: args.width,
                        height: args.height,
                        format: format_from_virtio(args.format),
                    },
                );
                RESP_OK_NODATA
            }
            CMD_RESOURCE_CREATE_BLOB => {
                let blob = Blob {
                    ctx_id: ctx,
                    blob_mem: u32_at(req, b + 4),
                    blob_flags: u32_at(req, b + 8),
                    blob_id: u64_at(req, b + 16),
                    size: u64_at(req, b + 24),
                };
                let Some(entries) = mem_entries(req, b + 32, u32_at(req, b + 12) as usize) else {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                };
                if res_arg == 0
                    || resources.contains_key(&res_arg)
                    || t.resources.contains_key(&res_arg)
                {
                    return Some(RESP_ERR_INVALID_RESOURCE_ID);
                }
                if let Err(e) = t
                    .backend
                    .resource_create_blob(res_arg, &blob, &entries, mem)
                {
                    return Some(backend_status(e));
                }
                t.resources.insert(res_arg, Res3d::default());
                RESP_OK_NODATA
            }
            CMD_SET_SCANOUT_BLOB => {
                let (scanout, id) = (u32_at(req, b + 16), u32_at(req, b + 20));
                if scanout != 0 {
                    return Some(RESP_ERR_INVALID_SCANOUT_ID);
                }
                if id == 0 {
                    *scanout_resource = None;
                    return Some(RESP_OK_NODATA);
                }
                let Some(r) = t.resources.get_mut(&id) else {
                    return Some(RESP_ERR_INVALID_RESOURCE_ID);
                };
                let Some(format) = format_from_virtio(u32_at(req, b + 32)) else {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                };
                *r = Res3d {
                    width: u32_at(req, b + 24),
                    height: u32_at(req, b + 28),
                    format: Some(format),
                };
                *scanout_resource = Some(id);
                RESP_OK_NODATA
            }
            CMD_TRANSFER_TO_HOST_3D | CMD_TRANSFER_FROM_HOST_3D => {
                let v = |i: usize| u32_at(req, b + 4 * i);
                let transfer = Transfer3d {
                    region: Box3d {
                        x: v(0),
                        y: v(1),
                        z: v(2),
                        w: v(3),
                        h: v(4),
                        d: v(5),
                    },
                    offset: u64_at(req, b + 24),
                    resource_id: v(8),
                    ctx_id: ctx,
                    level: v(9),
                    stride: v(10),
                    layer_stride: v(11),
                };
                if !t.resources.contains_key(&transfer.resource_id) {
                    return Some(RESP_ERR_INVALID_RESOURCE_ID);
                }
                if let Err(e) = t
                    .backend
                    .transfer(&transfer, kind == CMD_TRANSFER_TO_HOST_3D)
                {
                    return Some(backend_status(e));
                }
                RESP_OK_NODATA
            }
            CMD_SUBMIT_3D => {
                if !t.contexts.contains(&ctx) {
                    return Some(RESP_ERR_INVALID_CONTEXT_ID);
                }
                let size = u32_at(req, b) as usize;
                let in_fences = u32_at(req, b + 4) as usize;
                let ring = (u32_at(req, 4) & FLAG_INFO_RING_IDX != 0)
                    .then(|| req.get(HDR_RING_IDX).copied().unwrap_or(0));
                // Номера входных фенсов (u64) идут перед потоком команд; gfxstream ждёт их сам
                // по своим кольцам, поэтому мы их пропускаем.
                let start = in_fences.checked_mul(8).and_then(|n| n.checked_add(b + 8));
                let Some(cmd) = start
                    .and_then(|s| Some(s..s.checked_add(size)?))
                    .and_then(|r| req.get_mut(r))
                else {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                };
                if let Err(e) = t.backend.submit(ctx, ring, cmd) {
                    return Some(backend_status(e));
                }
                RESP_OK_NODATA
            }
            CMD_RESOURCE_MAP_BLOB => match t.map_blob(res_arg, u64_at(req, b + 8)) {
                Ok(info) => {
                    begin_response(req, resp, RESP_OK_MAP_INFO);
                    resp.extend_from_slice(&info.to_le_bytes());
                    resp.extend_from_slice(&[0; 4]);
                    RESP_OK_MAP_INFO
                }
                Err(status) => return Some(status),
            },
            CMD_RESOURCE_UNMAP_BLOB => {
                if !t.resources.contains_key(&res_arg) {
                    return Some(RESP_ERR_INVALID_RESOURCE_ID);
                }
                if !t.unmap_blob(res_arg) {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                }
                RESP_OK_NODATA
            }
            // Команды над ресурсами, общие для 2D и 3D: здесь — только для 3D-ресурсов.
            CMD_RESOURCE_UNREF if t.resources.contains_key(&res_arg) => {
                t.unmap_blob(res_arg);
                t.resources.remove(&res_arg);
                t.backend.resource_unref(res_arg);
                if *scanout_resource == Some(res_arg) {
                    *scanout_resource = None;
                }
                RESP_OK_NODATA
            }
            CMD_RESOURCE_ATTACH_BACKING if t.resources.contains_key(&res_arg) => {
                let Some(entries) = mem_entries(req, b + 8, u32_at(req, b + 4) as usize) else {
                    return Some(RESP_ERR_INVALID_PARAMETER);
                };
                if let Err(e) = t.backend.attach_backing(res_arg, &entries, mem) {
                    return Some(backend_status(e));
                }
                RESP_OK_NODATA
            }
            CMD_RESOURCE_DETACH_BACKING if t.resources.contains_key(&res_arg) => {
                t.backend.detach_backing(res_arg);
                RESP_OK_NODATA
            }
            CMD_TRANSFER_TO_HOST_2D if t.resources.contains_key(&u32_at(req, b + 24)) => {
                let r = rect_at(req, b);
                let transfer = Transfer3d {
                    resource_id: u32_at(req, b + 24),
                    region: Box3d {
                        x: r.x,
                        y: r.y,
                        z: 0,
                        w: r.w,
                        h: r.h,
                        d: 1,
                    },
                    offset: u64_at(req, b + 16),
                    ..Transfer3d::default()
                };
                if let Err(e) = t.backend.transfer(&transfer, true) {
                    return Some(backend_status(e));
                }
                RESP_OK_NODATA
            }
            CMD_SET_SCANOUT if t.resources.contains_key(&u32_at(req, b + 20)) => {
                if u32_at(req, b + 16) != 0 {
                    return Some(RESP_ERR_INVALID_SCANOUT_ID);
                }
                *scanout_resource = Some(u32_at(req, b + 20));
                RESP_OK_NODATA
            }
            CMD_RESOURCE_FLUSH if t.resources.contains_key(&u32_at(req, b + 16)) => {
                let id = u32_at(req, b + 16);
                if *scanout_resource == Some(id) {
                    publish_3d(t, mailbox, id);
                }
                RESP_OK_NODATA
            }
            _ => return None,
        };
        Some(status)
    }

    /// Придерживает команду с фенсом до его выполнения (только 3D). `true` — придержана.
    fn hold_for_fence(&mut self, status: u32, written: u32) -> bool {
        let Some(t) = self.three_d.as_mut() else {
            return false;
        };
        let flags = u32_at(&self.req, 4);
        if flags & FLAG_FENCE == 0 || status >= RESP_ERR_UNSPEC {
            return false;
        }
        let fence = FenceId {
            fence_id: u64_at(&self.req, 8),
            ctx_id: u32_at(&self.req, 16),
            ring_idx: (flags & FLAG_INFO_RING_IDX != 0)
                .then(|| self.req.get(HDR_RING_IDX).copied().unwrap_or(0)),
        };
        if let Err(e) = t.backend.create_fence(fence) {
            tracing::debug!(error = %e, ?fence, "фенс не поставлен, ответ без ожидания");
            return false;
        }
        t.pending.push(PendingFence {
            head: self.chain.head,
            written,
            fence,
        });
        true
    }

    /// Возвращает гостю команды, чьи фенсы выполнены.
    fn complete_fences(
        &mut self,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        let Some(t) = self.three_d.as_mut() else {
            return Ok(false);
        };
        let signaled = t.fences.take();
        if signaled.is_empty() || t.pending.is_empty() {
            return Ok(false);
        }
        let (done, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut t.pending)
            .into_iter()
            .partition(|p| signaled.iter().any(|s| p.fence.completed_by(s)));
        t.pending = keep;
        for p in &done {
            queue.add_used(mem, p.head, p.written)?;
        }
        Ok(!done.is_empty())
    }

    fn publish(&self, id: u32) {
        let Some(res) = self.resources.get(&id) else {
            return;
        };
        let mut data = self.mailbox.buffer(res.data.len());
        data.copy_from_slice(&res.data);
        self.mailbox.publish(Frame {
            width: res.width,
            height: res.height,
            stride: res.width * 4,
            format: res.format,
            data,
            seq: 0,
            flushed_at: Instant::now(),
        });
    }
}

/// Читает кадр 3D-ресурса у бэкенда и публикует его.
fn publish_3d(t: &mut ThreeD, mailbox: &Mailbox, id: u32) {
    let Some(&Res3d {
        width,
        height,
        format: Some(format),
    }) = t.resources.get(&id)
    else {
        tracing::debug!(
            id,
            "scanout-ресурс без поддерживаемого формата, кадр пропущен"
        );
        return;
    };
    let Ok(len) = usize::try_from(u64::from(width) * u64::from(height) * 4) else {
        return;
    };
    if len == 0 || len as u64 > MAX_RESOURCE_BYTES {
        return;
    }
    let mut data = mailbox.buffer(len);
    if let Err(e) = t.backend.read_pixels(id, width, height, &mut data) {
        tracing::debug!(id, error = %e, "кадр 3D-ресурса не прочитан");
        return;
    }
    mailbox.publish(Frame {
        width,
        height,
        stride: width * 4,
        format,
        data,
        seq: 0,
        flushed_at: Instant::now(),
    });
}

/// Копирует прямоугольник из backing гостя в ресурс.
fn transfer(res: &mut Resource, r: Rect, offset: u64, mem: &dyn GuestMemory) -> bool {
    if r.w == 0 || r.h == 0 {
        return true;
    }
    if r.x.checked_add(r.w).is_none_or(|e| e > res.width)
        || r.y.checked_add(r.h).is_none_or(|e| e > res.height)
    {
        return false;
    }
    let stride = res.stride();
    let row = r.w as usize * 4;
    for line in 0..r.h as usize {
        let src = offset + (line * stride) as u64;
        let dst = (r.y as usize + line) * stride + r.x as usize * 4;
        let Some(out) = res.data.get_mut(dst..dst + row) else {
            return false;
        };
        if !read_backing(&res.backing, src, out, mem) {
            return false;
        }
    }
    true
}

/// Читает из scatter-списка backing начиная со смещения `offset`.
fn read_backing(
    backing: &[(u64, u32)],
    mut offset: u64,
    mut out: &mut [u8],
    mem: &dyn GuestMemory,
) -> bool {
    for &(addr, len) in backing {
        let len = u64::from(len);
        if out.is_empty() {
            break;
        }
        if offset >= len {
            offset -= len;
            continue;
        }
        let n = usize::try_from((len - offset).min(out.len() as u64)).unwrap_or(0);
        let (head, tail) = out.split_at_mut(n);
        if mem.read(addr + offset, head).is_err() {
            return false;
        }
        out = tail;
        offset = 0;
    }
    out.is_empty()
}

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        // Гость не должен видеть память blob после того, как бэкенд её освободит: снимаем
        // отображения до удаления бэкенда (поля удаляются после этого).
        if let Some(shm) = self.three_d.as_mut().and_then(|t| t.shm.as_mut()) {
            shm.unmap_all();
        }
    }
}

impl VirtioDevice for VirtioGpu {
    fn device_type(&self) -> u32 {
        TYPE_GPU
    }

    fn device_features(&self) -> u64 {
        if self.three_d.is_some() {
            VIRTIO_GPU_F_EDID
                | VIRTIO_GPU_F_VIRGL
                | VIRTIO_GPU_F_RESOURCE_BLOB
                | VIRTIO_GPU_F_CONTEXT_INIT
        } else {
            VIRTIO_GPU_F_EDID
        }
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }

    fn shared_memory_region(&self) -> Option<(u8, u64)> {
        self.three_d
            .as_ref()
            .and_then(|t| t.shm.as_ref())
            .map(|s| (SHM_ID_HOST_VISIBLE, s.size))
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // events_read, events_clear, num_scanouts, num_capsets
        let mut cfg = [0_u8; 16];
        cfg[8..12].copy_from_slice(&1_u32.to_le_bytes());
        let capsets = self.three_d.as_ref().map_or(0, |t| t.backend.num_capsets());
        cfg[12..16].copy_from_slice(&capsets.to_le_bytes());
        data.fill(0);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn reset(&mut self) {
        self.resources.clear();
        self.resource_bytes = 0;
        self.scanout_resource = None;
        if let Some(t) = self.three_d.as_mut() {
            if let Some(shm) = t.shm.as_mut() {
                shm.unmap_all();
            }
            for (id, _) in t.resources.drain() {
                t.backend.resource_unref(id);
            }
            for ctx in t.contexts.drain() {
                t.backend.context_destroy(ctx);
            }
            t.pending.clear();
            let _ = t.fences.take();
        }
    }

    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        let mut any = false;
        if index == CONTROL_QUEUE {
            any |= self.complete_fences(queue, mem)?;
        }
        while queue.pop(mem, &mut self.chain)? {
            let written = if index == CONTROL_QUEUE {
                self.gather(mem)?;
                let status = self.command(mem);
                if status >= RESP_ERR_UNSPEC {
                    tracing::debug!(
                        cmd = format_args!("0x{:x}", u32_at(&self.req, 0)),
                        status = format_args!("0x{status:x}"),
                        "команда virtio-gpu отклонена"
                    );
                    self.begin_response(status);
                }
                let written = self.scatter(mem);
                if self.hold_for_fence(status, written) {
                    continue;
                }
                written
            } else {
                // Курсор: UPDATE/MOVE_CURSOR — аппаратный курсор пока не рисуем.
                self.gather(mem)?;
                let _ = matches!(u32_at(&self.req, 0), CMD_UPDATE_CURSOR | CMD_MOVE_CURSOR);
                0
            };
            queue.add_used(mem, self.chain.head, written)?;
            any = true;
        }
        if index == CONTROL_QUEUE {
            // Фенсы, которые бэкенд выполнил сразу (в том числе внутри create_fence).
            any |= self.complete_fences(queue, mem)?;
        }
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    use rustdroid_virtio::mem::{GuestMemoryExt, VecMemory};
    use rustdroid_virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};

    use super::*;

    struct Harness {
        mem: VecMemory,
        q: Queue,
        avail: u16,
        gpu: VirtioGpu,
        mailbox: Arc<Mailbox>,
    }

    const REQ: u64 = 0x10000;
    const RESP: u64 = 0x20000;

    impl Harness {
        fn new() -> Self {
            let mut q = Queue::new(16);
            q.desc_table = 0x1000;
            q.avail_ring = 0x2000;
            q.used_ring = 0x3000;
            q.ready = true;
            let mailbox = Mailbox::new();
            let gpu = VirtioGpu::new(
                DisplayMode {
                    width: 4,
                    height: 2,
                    refresh_hz: 120,
                },
                mailbox.clone(),
            );
            Self {
                mem: VecMemory::new(0x80000),
                q,
                avail: 0,
                gpu,
                mailbox,
            }
        }

        fn desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
            let mut b = [0_u8; 16];
            b[..8].copy_from_slice(&addr.to_le_bytes());
            b[8..12].copy_from_slice(&len.to_le_bytes());
            b[12..14].copy_from_slice(&flags.to_le_bytes());
            b[14..16].copy_from_slice(&next.to_le_bytes());
            self.mem.write(0x1000 + u64::from(i) * 16, &b).unwrap();
        }

        /// Отправляет команду, возвращает ответ.
        fn cmd(&mut self, kind: u32, body: &[u32], extra: &[u8], resp_len: u32) -> Vec<u8> {
            let mut req = vec![0_u8; HDR];
            req[..4].copy_from_slice(&kind.to_le_bytes());
            req[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
            req[8..16].copy_from_slice(&77_u64.to_le_bytes());
            for v in body {
                req.extend_from_slice(&v.to_le_bytes());
            }
            req.extend_from_slice(extra);
            self.mem.write(REQ, &req).unwrap();
            self.desc(0, REQ, req.len() as u32, VIRTQ_DESC_F_NEXT, 1);
            self.desc(1, RESP, resp_len, VIRTQ_DESC_F_WRITE, 0);
            self.mem
                .write_u16(0x2004 + u64::from(self.avail % 16) * 2, 0)
                .unwrap();
            self.avail += 1;
            self.mem.write_u16(0x2002, self.avail).unwrap();
            self.gpu
                .process_queue(CONTROL_QUEUE, &mut self.q, &self.mem)
                .unwrap();
            let mut out = vec![0_u8; resp_len as usize];
            self.mem.read(RESP, &mut out).unwrap();
            out
        }
    }

    fn resp_type(r: &[u8]) -> u32 {
        u32_at(r, 0)
    }

    // ---------- 3D ----------

    use std::sync::Mutex;

    use crate::backend::{BackendResult, BlobMapping, CapsetInfo};

    /// Заглушка бэкенда: записывает вызовы, фенсы выполняет только по команде теста.
    #[derive(Default)]
    struct Mock {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Mock {
        fn push(&self, s: String) {
            self.log.lock().unwrap().push(s);
        }
    }

    impl Backend3d for Mock {
        fn num_capsets(&self) -> u32 {
            2
        }
        fn capset_info(&self, index: u32) -> Option<CapsetInfo> {
            [(3, 1, 16), (8, 2, 4)]
                .get(index as usize)
                .map(|&(id, max_version, max_size)| CapsetInfo {
                    id,
                    max_version,
                    max_size,
                })
        }
        fn capset(&self, id: u32, version: u32) -> Option<Vec<u8>> {
            (id == 3).then(|| vec![version as u8; 16])
        }
        fn context_create(&mut self, ctx: u32, init: u32, name: &[u8]) -> BackendResult {
            self.push(format!(
                "ctx_create {ctx} {init} {}",
                String::from_utf8_lossy(name)
            ));
            Ok(())
        }
        fn context_destroy(&mut self, ctx: u32) {
            self.push(format!("ctx_destroy {ctx}"));
        }
        fn context_attach_resource(&mut self, ctx: u32, res: u32) {
            self.push(format!("attach {ctx} {res}"));
        }
        fn context_detach_resource(&mut self, ctx: u32, res: u32) {
            self.push(format!("detach {ctx} {res}"));
        }
        fn resource_create_3d(&mut self, id: u32, a: &Resource3d) -> BackendResult {
            if a.width > 8192 {
                return Err(BackendError::OutOfMemory);
            }
            self.push(format!(
                "create_3d {id} {}x{} f{} b{}",
                a.width, a.height, a.format, a.bind
            ));
            Ok(())
        }
        fn resource_create_blob(
            &mut self,
            id: u32,
            blob: &Blob,
            backing: &[MemEntry],
            _mem: &dyn GuestMemory,
        ) -> BackendResult {
            self.push(format!(
                "blob {id} ctx{} mem{} flags{} id{} size{} entries{}",
                blob.ctx_id,
                blob.blob_mem,
                blob.blob_flags,
                blob.blob_id,
                blob.size,
                backing.len()
            ));
            Ok(())
        }
        fn attach_backing(
            &mut self,
            id: u32,
            backing: &[MemEntry],
            _mem: &dyn GuestMemory,
        ) -> BackendResult {
            let parts: Vec<String> = backing
                .iter()
                .map(|e| format!("{:x}+{}", e.addr, e.len))
                .collect();
            self.push(format!("backing {id} {}", parts.join(",")));
            Ok(())
        }
        fn detach_backing(&mut self, id: u32) {
            self.push(format!("detach_backing {id}"));
        }
        fn resource_unref(&mut self, id: u32) {
            self.push(format!("unref {id}"));
        }
        fn transfer(&mut self, t: &Transfer3d, to_host: bool) -> BackendResult {
            let r = t.region;
            self.push(format!(
                "transfer {} {} ctx{} box{},{},{},{},{},{} off{} l{} s{}",
                if to_host { "to" } else { "from" },
                t.resource_id,
                t.ctx_id,
                r.x,
                r.y,
                r.z,
                r.w,
                r.h,
                r.d,
                t.offset,
                t.level,
                t.stride
            ));
            Ok(())
        }
        fn submit(&mut self, ctx: u32, ring: Option<u8>, cmd: &mut [u8]) -> BackendResult {
            self.push(format!("submit {ctx} {ring:?} {cmd:?}"));
            Ok(())
        }
        fn create_fence(&mut self, f: FenceId) -> BackendResult {
            self.push(format!(
                "fence {} {} {:?}",
                f.fence_id, f.ctx_id, f.ring_idx
            ));
            Ok(())
        }
        fn map_blob(&mut self, id: u32) -> BackendResult<BlobMapping> {
            self.push(format!("map {id}"));
            // Ресурс 13 — «невыровненная» память хоста; 14 — не host-visible.
            let addr = match id {
                13 => 0x1_0010,
                14 => return Err(BackendError::InvalidParameter),
                _ => 0x10_0000 * id as usize,
            };
            Ok(BlobMapping {
                ptr: std::ptr::NonNull::new(std::ptr::without_provenance_mut(addr)).unwrap(),
                size: 5000,
                map_info: 3,
            })
        }
        fn unmap_blob(&mut self, id: u32) {
            self.push(format!("unmap {id}"));
        }
        fn read_pixels(&mut self, id: u32, w: u32, h: u32, out: &mut [u8]) -> BackendResult {
            self.push(format!("read {id} {w}x{h}"));
            out.fill(0x5A);
            Ok(())
        }
    }

    struct Harness3d {
        h: Harness,
        log: Arc<Mutex<Vec<String>>>,
        fences: Arc<FenceSink>,
    }

    impl Harness3d {
        fn new() -> Self {
            let mut h = Harness::new();
            let mock = Mock::default();
            let log = Arc::clone(&mock.log);
            let fences = FenceSink::new();
            h.gpu = VirtioGpu::with_3d(
                h.gpu.mode,
                h.mailbox.clone(),
                Box::new(mock),
                Arc::clone(&fences),
            );
            Self { h, log, fences }
        }

        /// Команда без фенса, с контекстом `ctx`.
        fn cmd(&mut self, kind: u32, ctx: u32, body: &[u32], extra: &[u8], resp: u32) -> Vec<u8> {
            self.cmd_flags(kind, 0, 0, ctx, 0, body, extra, resp)
        }

        #[allow(clippy::too_many_arguments)]
        fn cmd_flags(
            &mut self,
            kind: u32,
            flags: u32,
            fence: u64,
            ctx: u32,
            ring: u8,
            body: &[u32],
            extra: &[u8],
            resp_len: u32,
        ) -> Vec<u8> {
            let mut req = vec![0_u8; HDR];
            req[..4].copy_from_slice(&kind.to_le_bytes());
            req[4..8].copy_from_slice(&flags.to_le_bytes());
            req[8..16].copy_from_slice(&fence.to_le_bytes());
            req[16..20].copy_from_slice(&ctx.to_le_bytes());
            req[20] = ring;
            for v in body {
                req.extend_from_slice(&v.to_le_bytes());
            }
            req.extend_from_slice(extra);
            self.h.mem.write(RESP, &vec![0; resp_len as usize]).unwrap();
            self.h.mem.write(REQ, &req).unwrap();
            self.h.desc(0, REQ, req.len() as u32, VIRTQ_DESC_F_NEXT, 1);
            self.h.desc(1, RESP, resp_len, VIRTQ_DESC_F_WRITE, 0);
            self.h
                .mem
                .write_u16(0x2004 + u64::from(self.h.avail % 16) * 2, 0)
                .unwrap();
            self.h.avail += 1;
            self.h.mem.write_u16(0x2002, self.h.avail).unwrap();
            self.poll();
            let mut out = vec![0_u8; resp_len as usize];
            self.h.mem.read(RESP, &mut out).unwrap();
            out
        }

        fn poll(&mut self) {
            self.h
                .gpu
                .process_queue(CONTROL_QUEUE, &mut self.h.q, &self.h.mem)
                .unwrap();
        }

        fn used(&self) -> u16 {
            self.h.mem.read_u16(0x3002).unwrap()
        }

        fn log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }

        fn ok(&mut self, kind: u32, ctx: u32, body: &[u32], extra: &[u8]) {
            let r = self.cmd(kind, ctx, body, extra, 24);
            assert_eq!(resp_type(&r), RESP_OK_NODATA, "команда 0x{kind:x}");
        }
    }

    fn ctx_create_body(name: &str, init: u32) -> (Vec<u32>, Vec<u8>) {
        let mut n = name.as_bytes().to_vec();
        n.resize(64, 0);
        (vec![name.len() as u32, init], n)
    }

    /// Заглушка отображения: пишет вызовы в общий журнал.
    struct MockShm(Arc<Mutex<Vec<String>>>);
    impl ShmMapper for MockShm {
        unsafe fn map(
            &self,
            offset: u64,
            host: std::ptr::NonNull<u8>,
            len: u64,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push(format!(
                "shm map 0x{offset:x} {:x} {len}",
                host.as_ptr() as usize
            ));
            Ok(())
        }
        fn unmap(&self, offset: u64, len: u64) {
            self.0
                .lock()
                .unwrap()
                .push(format!("shm unmap 0x{offset:x} {len}"));
        }
    }

    impl Harness3d {
        fn with_shm() -> Self {
            let mut t = Self::new();
            let mode = t.h.gpu.mode;
            let gpu = std::mem::replace(&mut t.h.gpu, VirtioGpu::new(mode, t.h.mailbox.clone()));
            t.h.gpu = gpu.with_shm(1 << 20, Arc::new(MockShm(Arc::clone(&t.log))));
            t
        }

        fn blob(&mut self, id: u32) {
            self.ok(
                CMD_RESOURCE_CREATE_BLOB,
                1,
                &[id, 2, 1, 0, id, 0, 4096, 0],
                &[],
            );
        }
    }

    #[test]
    fn map_blob_into_shared_memory() {
        let mut t = Harness3d::with_shm();
        assert_eq!(t.h.gpu.shared_memory_region(), Some((1, 1 << 20)));
        let (body, name) = ctx_create_body("vk", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        t.blob(5);
        t.log();
        // MAP_BLOB: resource 5, padding, offset 0x2000 (u64).
        let r = t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[5, 0, 0x2000, 0], &[], 32);
        assert_eq!(resp_type(&r), RESP_OK_MAP_INFO);
        assert_eq!(u32_at(&r, 24), 3);
        // Повторно и с перекрытием — нельзя; размер округлён до страниц (5000 → 8192).
        t.blob(6);
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[5, 0, 0x8000, 0], &[], 32)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[6, 0, 0x3000, 0], &[], 32)),
            RESP_ERR_INVALID_PARAMETER
        );
        // Невыровненное смещение и выход за область.
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[6, 0, 0x4001, 0], &[], 32)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[6, 0, 0xFF000, 0], &[], 32)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[6, 0, 0x4000, 0], &[], 32)),
            RESP_OK_MAP_INFO
        );
        t.ok(CMD_RESOURCE_UNMAP_BLOB, 1, &[5, 0], &[]);
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_UNMAP_BLOB, 1, &[5, 0], &[], 24)),
            RESP_ERR_INVALID_PARAMETER
        );
        // UNREF отображённого blob сначала снимает отображение.
        t.ok(CMD_RESOURCE_UNREF, 0, &[6, 0], &[]);
        assert_eq!(
            t.log(),
            vec![
                "map 5",
                "shm map 0x2000 500000 8192",
                "blob 6 ctx1 mem2 flags1 id6 size4096 entries0",
                // Перекрытие с blob 5 (0x2000..0x4000).
                "map 6",
                "unmap 6",
                // Выход за область; невыровненное смещение отклонено до бэкенда.
                "map 6",
                "unmap 6",
                "map 6",
                "shm map 0x4000 600000 8192",
                "shm unmap 0x2000 8192",
                "unmap 5",
                "shm unmap 0x4000 8192",
                "unmap 6",
                "unref 6",
            ]
        );
    }

    #[test]
    fn map_blob_rejects_unaligned_host_memory_and_unmappable_blobs() {
        let mut t = Harness3d::with_shm();
        let (body, name) = ctx_create_body("vk", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        t.blob(13);
        t.blob(14);
        t.log();
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[13, 0, 0, 0], &[], 32)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[14, 0, 0, 0], &[], 32)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(t.log(), vec!["map 13", "unmap 13", "map 14"]);
    }

    #[test]
    fn shared_memory_is_unmapped_on_reset_and_drop() {
        let mut t = Harness3d::with_shm();
        let (body, name) = ctx_create_body("vk", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        t.blob(5);
        t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[5, 0, 0, 0], &[], 32);
        t.log();
        t.h.gpu.reset();
        let log = t.log();
        assert_eq!(log.first().map(String::as_str), Some("shm unmap 0x0 8192"));
        t.blob(5);
        t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[5, 0, 0, 0], &[], 32);
        t.log();
        let mode = t.h.gpu.mode;
        drop(std::mem::replace(
            &mut t.h.gpu,
            VirtioGpu::new(mode, t.h.mailbox.clone()),
        ));
        assert_eq!(
            t.log().first().map(String::as_str),
            Some("shm unmap 0x0 8192")
        );
    }

    #[test]
    fn map_blob_without_shared_memory_fails() {
        let mut t = Harness3d::new();
        assert_eq!(t.h.gpu.shared_memory_region(), None);
        let (body, name) = ctx_create_body("vk", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        t.blob(5);
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_MAP_BLOB, 1, &[5, 0, 0, 0], &[], 32)),
            RESP_ERR_UNSPEC
        );
    }

    #[test]
    fn features_and_config_in_3d_mode() {
        let t = Harness3d::new();
        let f = t.h.gpu.device_features();
        assert_eq!(
            f,
            VIRTIO_GPU_F_VIRGL
                | VIRTIO_GPU_F_EDID
                | VIRTIO_GPU_F_RESOURCE_BLOB
                | VIRTIO_GPU_F_CONTEXT_INIT
        );
        let mut cfg = [0_u8; 4];
        t.h.gpu.read_config(12, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), 2);
        // Без бэкенда — только EDID и ноль capsets.
        let h = Harness::new();
        assert_eq!(h.gpu.device_features(), VIRTIO_GPU_F_EDID);
        h.gpu.read_config(12, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), 0);
    }

    #[test]
    fn capsets() {
        let mut t = Harness3d::new();
        let r = t.cmd(CMD_GET_CAPSET_INFO, 0, &[1, 0], &[], 40);
        assert_eq!(resp_type(&r), RESP_OK_CAPSET_INFO);
        assert_eq!((u32_at(&r, 24), u32_at(&r, 28), u32_at(&r, 32)), (8, 2, 4));
        let r = t.cmd(CMD_GET_CAPSET_INFO, 0, &[2, 0], &[], 40);
        assert_eq!(resp_type(&r), RESP_ERR_INVALID_PARAMETER);

        let r = t.cmd(CMD_GET_CAPSET, 0, &[3, 1], &[], 40);
        assert_eq!(resp_type(&r), RESP_OK_CAPSET);
        assert_eq!(&r[24..40], &[1; 16]);
        let r = t.cmd(CMD_GET_CAPSET, 0, &[99, 1], &[], 40);
        assert_eq!(resp_type(&r), RESP_ERR_INVALID_PARAMETER);
    }

    #[test]
    fn contexts_resources_and_submit() {
        let mut t = Harness3d::new();
        let (body, name) = ctx_create_body("surfaceflinger", 3);
        t.ok(CMD_CTX_CREATE, 5, &body, &name);
        // Повторный контекст и контекст 0 отклоняются.
        assert_eq!(
            resp_type(&t.cmd(CMD_CTX_CREATE, 5, &body, &name, 24)),
            RESP_ERR_INVALID_CONTEXT_ID
        );
        assert_eq!(
            resp_type(&t.cmd(CMD_CTX_CREATE, 0, &body, &name, 24)),
            RESP_ERR_INVALID_CONTEXT_ID
        );
        // RESOURCE_CREATE_3D: id, target, format, bind, w, h, depth, array, last_level, samples, flags, pad.
        t.ok(
            CMD_RESOURCE_CREATE_3D,
            5,
            &[7, 2, 1, 2, 4, 2, 1, 1, 0, 0, 0, 0],
            &[],
        );
        // Тот же id занят — и для 2D тоже.
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_CREATE_2D, 0, &[7, 1, 4, 2], &[], 24)),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        let mut entries = Vec::new();
        entries.extend_from_slice(&0x40000_u64.to_le_bytes());
        entries.extend_from_slice(&32_u32.to_le_bytes());
        entries.extend_from_slice(&0_u32.to_le_bytes());
        t.ok(CMD_RESOURCE_ATTACH_BACKING, 0, &[7, 1], &entries);
        t.ok(CMD_CTX_ATTACH_RESOURCE, 5, &[7, 0], &[]);
        // box 0,0,0,4,2,1; offset 16 (u64); res 7; level 0; stride 16; layer_stride 0.
        t.ok(
            CMD_TRANSFER_TO_HOST_3D,
            5,
            &[0, 0, 0, 4, 2, 1, 16, 0, 7, 0, 16, 0],
            &[],
        );
        t.ok(
            CMD_TRANSFER_FROM_HOST_3D,
            5,
            &[1, 1, 0, 1, 1, 1, 0, 0, 7, 0, 16, 0],
            &[],
        );
        // SUBMIT_3D: size 4, num_in_fences 1, затем номер фенса (u64) и 4 байта команд.
        let mut extra = 9_u64.to_le_bytes().to_vec();
        extra.extend_from_slice(&[1, 2, 3, 4]);
        t.ok(CMD_SUBMIT_3D, 5, &[4, 1], &extra);
        // Поток команд длиннее запроса.
        assert_eq!(
            resp_type(&t.cmd(CMD_SUBMIT_3D, 5, &[100, 0], &[1, 2], 24)),
            RESP_ERR_INVALID_PARAMETER
        );
        // SUBMIT в несуществующий контекст.
        assert_eq!(
            resp_type(&t.cmd(CMD_SUBMIT_3D, 6, &[0, 0], &[], 24)),
            RESP_ERR_INVALID_CONTEXT_ID
        );
        t.ok(CMD_CTX_DETACH_RESOURCE, 5, &[7, 0], &[]);
        t.ok(CMD_RESOURCE_DETACH_BACKING, 0, &[7, 0], &[]);
        t.ok(CMD_RESOURCE_UNREF, 0, &[7, 0], &[]);
        t.ok(CMD_CTX_DESTROY, 5, &[], &[]);
        assert_eq!(
            resp_type(&t.cmd(CMD_CTX_DESTROY, 5, &[], &[], 24)),
            RESP_ERR_INVALID_CONTEXT_ID
        );
        assert_eq!(
            t.log(),
            vec![
                "ctx_create 5 3 surfaceflinger",
                "create_3d 7 4x2 f1 b2",
                "backing 7 40000+32",
                "attach 5 7",
                "transfer to 7 ctx5 box0,0,0,4,2,1 off16 l0 s16",
                "transfer from 7 ctx5 box1,1,0,1,1,1 off0 l0 s16",
                "submit 5 None [1, 2, 3, 4]",
                "detach 5 7",
                "detach_backing 7",
                "unref 7",
                "ctx_destroy 5",
            ]
        );
    }

    #[test]
    fn backend_errors_and_unknown_resources() {
        let mut t = Harness3d::new();
        assert_eq!(
            resp_type(&t.cmd(
                CMD_RESOURCE_CREATE_3D,
                0,
                &[1, 2, 1, 2, 10000, 2, 1, 1, 0, 0, 0, 0],
                &[],
                24
            )),
            RESP_ERR_OUT_OF_MEMORY
        );
        let (body, name) = ctx_create_body("a", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        assert_eq!(
            resp_type(&t.cmd(CMD_CTX_ATTACH_RESOURCE, 1, &[42, 0], &[], 24)),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            resp_type(&t.cmd(
                CMD_TRANSFER_TO_HOST_3D,
                1,
                &[0, 0, 0, 1, 1, 1, 0, 0, 42, 0, 4, 0],
                &[],
                24
            )),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        // Неизвестный ресурс уходит в 2D-путь и там тоже отклоняется.
        assert_eq!(
            resp_type(&t.cmd(CMD_RESOURCE_UNREF, 0, &[42, 0], &[], 24)),
            RESP_ERR_INVALID_RESOURCE_ID
        );
    }

    #[test]
    fn blob_and_scanout_blob_publish_frame() {
        let mut t = Harness3d::new();
        let (body, name) = ctx_create_body("composer", 9);
        t.ok(CMD_CTX_CREATE, 2, &body, &name);
        // id 3, blob_mem 2 (HOST3D), flags 1, nr_entries 0, blob_id 77 (u64), size 32 (u64).
        t.ok(
            CMD_RESOURCE_CREATE_BLOB,
            2,
            &[3, 2, 1, 0, 77, 0, 32, 0],
            &[],
        );
        // Без поддерживаемого формата scanout не ставится.
        let mut sb = vec![0, 0, 4, 2, 0, 3, 4, 2, 999, 0];
        sb.extend_from_slice(&[16, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            resp_type(&t.cmd(CMD_SET_SCANOUT_BLOB, 0, &sb, &[], 24)),
            RESP_ERR_INVALID_PARAMETER
        );
        sb[8] = 2; // B8G8R8X8
        t.ok(CMD_SET_SCANOUT_BLOB, 0, &sb, &[]);
        t.ok(CMD_RESOURCE_FLUSH, 0, &[0, 0, 4, 2, 3, 0], &[]);
        let f = t.h.mailbox.latest_after(0).unwrap();
        assert_eq!((f.width, f.height, f.format), (4, 2, PixelFormat::Bgra8));
        assert_eq!(f.data, vec![0x5A; 32]);
        assert_eq!(
            t.log(),
            vec![
                "ctx_create 2 9 composer",
                "blob 3 ctx2 mem2 flags1 id77 size32 entries0",
                "read 3 4x2",
            ]
        );
    }

    #[test]
    fn dumb_buffer_as_3d_resource_on_scanout() {
        // Так ядро создаёт dumb-буфер при VIRGL: CREATE_3D + TRANSFER_TO_HOST_2D + FLUSH.
        let mut t = Harness3d::new();
        t.ok(
            CMD_RESOURCE_CREATE_3D,
            0,
            &[4, 2, 67, 2, 4, 2, 1, 1, 0, 0, 0, 0],
            &[],
        );
        t.ok(CMD_TRANSFER_TO_HOST_2D, 0, &[0, 0, 4, 2, 8, 0, 4, 0], &[]);
        t.ok(CMD_SET_SCANOUT, 0, &[0, 0, 4, 2, 0, 4], &[]);
        t.ok(CMD_RESOURCE_FLUSH, 0, &[0, 0, 4, 2, 4, 0], &[]);
        let f = t.h.mailbox.latest_after(0).unwrap();
        assert_eq!(f.format, PixelFormat::Rgba8);
        t.ok(CMD_RESOURCE_UNREF, 0, &[4, 0], &[]);
        assert_eq!(
            t.log(),
            vec![
                "create_3d 4 4x2 f67 b2",
                "transfer to 4 ctx0 box0,0,0,4,2,1 off8 l0 s0",
                "read 4 4x2",
                "unref 4",
            ]
        );
        // После UNREF scanout снят: FLUSH другого ресурса ничего не публикует.
        assert!(t.h.gpu.scanout_resource.is_none());
    }

    #[test]
    fn fenced_commands_wait_for_backend() {
        let mut t = Harness3d::new();
        let (body, name) = ctx_create_body("game", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        assert_eq!(t.used(), 1);
        t.log();

        // Две команды на кольце 0 контекста 1 и одна на глобальной шкале.
        let ring = FLAG_FENCE | FLAG_INFO_RING_IDX;
        let r = t.cmd_flags(CMD_SUBMIT_3D, ring, 10, 1, 0, &[0, 0], &[], 24);
        // Ответ записан сразу, с флагами и кольцом из запроса…
        assert_eq!(resp_type(&r), RESP_OK_NODATA);
        assert_eq!((u32_at(&r, 4), u64_at(&r, 8), r[20]), (ring, 10, 0));
        // …но в used ring команда не попала.
        assert_eq!(t.used(), 1);
        t.cmd_flags(CMD_SUBMIT_3D, ring, 11, 1, 0, &[0, 0], &[], 24);
        t.cmd_flags(CMD_SUBMIT_3D, FLAG_FENCE, 5, 1, 0, &[0, 0], &[], 24);
        assert_eq!(t.used(), 1);
        assert_eq!(
            t.log(),
            vec![
                "submit 1 Some(0) []",
                "fence 10 1 Some(0)",
                "submit 1 Some(0) []",
                "fence 11 1 Some(0)",
                "submit 1 None []",
                "fence 5 1 None",
            ]
        );

        // Фенс другого контекста и другого кольца ничего не завершает.
        t.fences.signal(FenceId {
            fence_id: 99,
            ctx_id: 2,
            ring_idx: Some(0),
        });
        t.fences.signal(FenceId {
            fence_id: 99,
            ctx_id: 1,
            ring_idx: Some(1),
        });
        t.poll();
        assert_eq!(t.used(), 1);

        // Фенс 11 завершает и 10, и 11 на своём кольце.
        t.fences.signal(FenceId {
            fence_id: 11,
            ctx_id: 1,
            ring_idx: Some(0),
        });
        t.poll();
        assert_eq!(t.used(), 3);

        // Глобальный фенс.
        t.fences.signal(FenceId {
            fence_id: 5,
            ctx_id: 0,
            ring_idx: None,
        });
        t.poll();
        assert_eq!(t.used(), 4);
    }

    #[test]
    fn failed_command_with_fence_is_not_held() {
        let mut t = Harness3d::new();
        let r = t.cmd_flags(CMD_SUBMIT_3D, FLAG_FENCE, 1, 9, 0, &[0, 0], &[], 24);
        assert_eq!(resp_type(&r), RESP_ERR_INVALID_CONTEXT_ID);
        assert_eq!(t.used(), 1);
        assert!(t.log().is_empty());
    }

    #[test]
    fn reset_releases_backend_objects() {
        let mut t = Harness3d::new();
        let (body, name) = ctx_create_body("x", 3);
        t.ok(CMD_CTX_CREATE, 1, &body, &name);
        t.ok(
            CMD_RESOURCE_CREATE_3D,
            1,
            &[2, 2, 1, 2, 4, 2, 1, 1, 0, 0, 0, 0],
            &[],
        );
        t.cmd_flags(CMD_SUBMIT_3D, FLAG_FENCE, 1, 1, 0, &[0, 0], &[], 24);
        t.log();
        t.h.gpu.reset();
        let mut log = t.log();
        log.sort();
        assert_eq!(log, vec!["ctx_destroy 1", "unref 2"]);
        assert!(t.h.gpu.three_d.as_ref().unwrap().pending.is_empty());
    }

    #[test]
    fn display_info_and_edid() {
        let mut h = Harness::new();
        let r = h.cmd(CMD_GET_DISPLAY_INFO, &[], &[], 408);
        assert_eq!(resp_type(&r), RESP_OK_DISPLAY_INFO);
        assert_eq!((u32_at(&r, 32), u32_at(&r, 36), u32_at(&r, 40)), (4, 2, 1));
        // Фенс возвращается в ответе.
        assert_eq!(u32_at(&r, 4), FLAG_FENCE);
        assert_eq!(u64_at(&r, 8), 77);

        let r = h.cmd(CMD_GET_EDID, &[0, 0], &[], (HDR + 8 + 1024) as u32);
        assert_eq!(resp_type(&r), RESP_OK_EDID);
        assert_eq!(u32_at(&r, HDR), 128);
        assert_eq!(
            &r[HDR + 8..HDR + 16],
            &[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]
        );
    }

    #[test]
    fn full_2d_flow_publishes_frame() {
        let mut h = Harness::new();
        assert_eq!(
            resp_type(&h.cmd(CMD_RESOURCE_CREATE_2D, &[1, 2, 4, 2], &[], 24)),
            RESP_OK_NODATA
        );
        // Backing из двух кусков: 20 + 12 байт.
        let mut entries = Vec::new();
        for (addr, len) in [(0x40000_u64, 20_u32), (0x50000, 12)] {
            entries.extend_from_slice(&addr.to_le_bytes());
            entries.extend_from_slice(&len.to_le_bytes());
            entries.extend_from_slice(&0_u32.to_le_bytes());
        }
        assert_eq!(
            resp_type(&h.cmd(CMD_RESOURCE_ATTACH_BACKING, &[1, 2], &entries, 24)),
            RESP_OK_NODATA
        );
        let pixels: Vec<u8> = (0..32).collect();
        h.mem.write(0x40000, &pixels[..20]).unwrap();
        h.mem.write(0x50000, &pixels[20..]).unwrap();
        // TRANSFER rect(0,0,4,2), offset 0, resource 1.
        assert_eq!(
            resp_type(&h.cmd(CMD_TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0], &[], 24)),
            RESP_OK_NODATA
        );
        assert_eq!(
            resp_type(&h.cmd(CMD_SET_SCANOUT, &[0, 0, 4, 2, 0, 1], &[], 24)),
            RESP_OK_NODATA
        );
        assert_eq!(
            resp_type(&h.cmd(CMD_RESOURCE_FLUSH, &[0, 0, 4, 2, 1, 0], &[], 24)),
            RESP_OK_NODATA
        );
        let f = h.mailbox.latest_after(0).unwrap();
        assert_eq!((f.width, f.height, f.format), (4, 2, PixelFormat::Bgra8));
        assert_eq!(f.data, pixels);
    }

    #[test]
    fn partial_transfer_uses_offset_and_stride() {
        let mut h = Harness::new();
        h.cmd(CMD_RESOURCE_CREATE_2D, &[5, 1, 4, 2], &[], 24);
        let mut e = Vec::new();
        e.extend_from_slice(&0x40000_u64.to_le_bytes());
        e.extend_from_slice(&32_u32.to_le_bytes());
        e.extend_from_slice(&0_u32.to_le_bytes());
        h.cmd(CMD_RESOURCE_ATTACH_BACKING, &[5, 1], &e, 24);
        h.mem.write(0x40000, &[0xAB; 32]).unwrap();
        // Пиксель (2,1): offset = 1*16 + 2*4 = 24.
        h.cmd(CMD_TRANSFER_TO_HOST_2D, &[2, 1, 1, 1, 24, 0, 5, 0], &[], 24);
        let res = &h.gpu.resources[&5];
        assert_eq!(&res.data[24..28], &[0xAB; 4]);
        assert_eq!(&res.data[..24], &[0; 24]);
    }

    #[test]
    fn errors_are_reported() {
        let mut h = Harness::new();
        assert_eq!(
            resp_type(&h.cmd(CMD_RESOURCE_CREATE_2D, &[1, 999, 4, 2], &[], 24)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            resp_type(&h.cmd(CMD_SET_SCANOUT, &[0, 0, 4, 2, 0, 9], &[], 24)),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            resp_type(&h.cmd(CMD_SET_SCANOUT, &[0, 0, 4, 2, 3, 0], &[], 24)),
            RESP_ERR_INVALID_SCANOUT_ID
        );
        h.cmd(CMD_RESOURCE_CREATE_2D, &[1, 1, 4, 2], &[], 24);
        // Прямоугольник за границей ресурса.
        assert_eq!(
            resp_type(&h.cmd(CMD_TRANSFER_TO_HOST_2D, &[2, 0, 4, 2, 0, 0, 1, 0], &[], 24)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(resp_type(&h.cmd(0x9999, &[], &[], 24)), RESP_ERR_UNSPEC);
        // Гигантский ресурс отклоняется.
        assert_eq!(
            resp_type(&h.cmd(CMD_RESOURCE_CREATE_2D, &[2, 1, 65535, 65535], &[], 24)),
            RESP_ERR_OUT_OF_MEMORY
        );
    }
}
