//! virtio-gpu 2D (virtio 1.2, раздел 5.7): ресурсы, backing, scanout, flush, EDID.
//!
//! Гость рисует программно (SwiftShader), а мы копируем изменённые прямоугольники из
//! гостевой памяти в ресурс и по `RESOURCE_FLUSH` публикуем кадр композитору. На этапе 4
//! этот путь заменит gfxstream с zero-copy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rustdroid_virtio::device::TYPE_GPU;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

use crate::edid::{self, DisplayMode};
use crate::scanout::{Frame, Mailbox, PixelFormat};

/// Возможность: EDID.
const VIRTIO_GPU_F_EDID: u64 = 1 << 1;

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_EDID: u32 = 0x010A;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_EDID: u32 = 0x1104;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

const FLAG_FENCE: u32 = 1;
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

/// Устройство virtio-gpu (2D, один scanout).
pub struct VirtioGpu {
    mode: DisplayMode,
    mailbox: Arc<Mailbox>,
    resources: HashMap<u32, Resource>,
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
            scanout_resource: None,
            resource_bytes: 0,
            sizes: [256, 16],
            chain: Chain::default(),
            req: Vec::new(),
            resp: Vec::new(),
        }
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
        let flags = u32_at(&self.req, 4);
        let fence = u64_at(&self.req, 8);
        let ctx = u32_at(&self.req, 16);
        self.resp.clear();
        self.resp.extend_from_slice(&kind.to_le_bytes());
        self.resp
            .extend_from_slice(&(flags & FLAG_FENCE).to_le_bytes());
        self.resp
            .extend_from_slice(&if flags & FLAG_FENCE != 0 { fence } else { 0 }.to_le_bytes());
        self.resp.extend_from_slice(&ctx.to_le_bytes());
        self.resp.extend_from_slice(&[0; 4]);
    }

    fn command(&mut self, mem: &dyn GuestMemory) -> u32 {
        if self.req.len() < HDR {
            return RESP_ERR_UNSPEC;
        }
        let kind = u32_at(&self.req, 0);
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
                if id == 0 || self.resources.contains_key(&id) || w == 0 || h == 0 {
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
                let entries: Vec<(u64, u32)> = (0..n)
                    .map(|i| {
                        (
                            u64_at(&self.req, b + 8 + i * 16),
                            u32_at(&self.req, b + 16 + i * 16),
                        )
                    })
                    .collect();
                if self.req.len() < b + 8 + n * 16 {
                    return RESP_ERR_INVALID_PARAMETER;
                }
                match self.resources.get_mut(&id) {
                    Some(r) => r.backing = entries,
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

impl VirtioDevice for VirtioGpu {
    fn device_type(&self) -> u32 {
        TYPE_GPU
    }

    fn device_features(&self) -> u64 {
        VIRTIO_GPU_F_EDID
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // events_read, events_clear, num_scanouts, num_capsets
        let mut cfg = [0_u8; 16];
        cfg[8..12].copy_from_slice(&1_u32.to_le_bytes());
        data.fill(0);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        let mut any = false;
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
                self.scatter(mem)
            } else {
                // Курсор: UPDATE/MOVE_CURSOR — аппаратный курсор пока не рисуем.
                self.gather(mem)?;
                let _ = matches!(u32_at(&self.req, 0), CMD_UPDATE_CURSOR | CMD_MOVE_CURSOR);
                0
            };
            queue.add_used(mem, self.chain.head, written)?;
            any = true;
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
