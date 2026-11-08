//! virtio-blk (virtio 1.x, раздел 5.2).

use std::sync::Arc;

use rustdroid_virtio::device::TYPE_BLOCK;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

use crate::disk::{Disk, SECTOR_SIZE};

const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const QUEUE_SIZE: u16 = 256;
const SEG_MAX: u32 = 126;
/// Наибольший буфер копирования (дальше — частями).
const CHUNK: usize = 1 << 20;

/// virtio-blk поверх [`Disk`].
pub struct VirtioBlk {
    disk: Arc<dyn Disk>,
    id: [u8; 20],
    sizes: [u16; 1],
    chain: Chain,
    buf: Vec<u8>,
}

impl std::fmt::Debug for VirtioBlk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioBlk")
            .field("size", &self.disk.size())
            .finish_non_exhaustive()
    }
}

impl VirtioBlk {
    /// Устройство с серийным номером `id` (до 20 байт).
    #[must_use]
    pub fn new(disk: Arc<dyn Disk>, id: &str) -> Self {
        let mut serial = [0_u8; 20];
        for (dst, src) in serial.iter_mut().zip(id.bytes()) {
            *dst = src;
        }
        Self {
            disk,
            id: serial,
            sizes: [QUEUE_SIZE],
            chain: Chain::default(),
            buf: Vec::new(),
        }
    }

    /// Обрабатывает один запрос; возвращает (статус, байт записано в гостя без статуса).
    fn handle(&mut self, mem: &dyn GuestMemory) -> (u8, u32) {
        let descs = &self.chain.descriptors;
        let (Some(hdr), Some(status)) = (descs.first(), descs.last()) else {
            return (S_IOERR, 0);
        };
        if hdr.is_write() || hdr.len < 16 || !status.is_write() || descs.len() < 2 {
            return (S_IOERR, 0);
        }
        let mut h = [0_u8; 16];
        if mem.read(hdr.addr, &mut h).is_err() {
            return (S_IOERR, 0);
        }
        let kind = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let sector = u64::from_le_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
        let data = descs.get(1..descs.len() - 1).unwrap_or(&[]);

        match kind {
            T_IN | T_OUT => {
                let Some(mut offset) = sector.checked_mul(SECTOR_SIZE) else {
                    return (S_IOERR, 0);
                };
                let mut written = 0_u32;
                for d in data {
                    if (kind == T_IN) != d.is_write() {
                        return (S_IOERR, written);
                    }
                    let mut done = 0_usize;
                    let total = d.len as usize;
                    while done < total {
                        let n = (total - done).min(CHUNK);
                        if offset + n as u64 > self.disk.size() {
                            return (S_IOERR, written);
                        }
                        if self.buf.len() < n {
                            self.buf.resize(n, 0);
                        }
                        let Some(buf) = self.buf.get_mut(..n) else {
                            return (S_IOERR, written);
                        };
                        let gpa = d.addr + done as u64;
                        let ok = if kind == T_IN {
                            self.disk.read_at(buf, offset).is_ok() && mem.write(gpa, buf).is_ok()
                        } else {
                            mem.read(gpa, buf).is_ok() && self.disk.write_at(buf, offset).is_ok()
                        };
                        if !ok {
                            return (S_IOERR, written);
                        }
                        if kind == T_IN {
                            #[allow(clippy::cast_possible_truncation, reason = "n ≤ 1 МиБ")]
                            {
                                written += n as u32;
                            }
                        }
                        offset += n as u64;
                        done += n;
                    }
                }
                (S_OK, written)
            }
            T_FLUSH => (
                if self.disk.flush().is_ok() {
                    S_OK
                } else {
                    S_IOERR
                },
                0,
            ),
            T_GET_ID => match data.first() {
                Some(d) if d.is_write() => {
                    let n = (d.len as usize).min(self.id.len());
                    match mem.write(d.addr, self.id.get(..n).unwrap_or(&[])) {
                        #[allow(clippy::cast_possible_truncation, reason = "n ≤ 20")]
                        Ok(()) => (S_OK, n as u32),
                        Err(_) => (S_IOERR, 0),
                    }
                }
                _ => (S_IOERR, 0),
            },
            _ => (S_UNSUPP, 0),
        }
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn device_features(&self) -> u64 {
        let mut f = VIRTIO_BLK_F_SEG_MAX | VIRTIO_BLK_F_BLK_SIZE | VIRTIO_BLK_F_FLUSH;
        if self.disk.read_only() {
            f |= VIRTIO_BLK_F_RO;
        }
        f
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0_u8; 36];
        cfg[0..8].copy_from_slice(&(self.disk.size() / SECTOR_SIZE).to_le_bytes());
        cfg[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        #[allow(clippy::cast_possible_truncation, reason = "сектор 512")]
        cfg[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        data.fill(0);
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(off + i).copied().unwrap_or(0);
        }
    }

    fn process_queue(
        &mut self,
        _index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        let mut any = false;
        while queue.pop(mem, &mut self.chain)? {
            let (status, written) = self.handle(mem);
            if let Some(s) = self.chain.descriptors.last() {
                // Если статус не пишется, драйвер получит ошибку по таймауту — запрос всё равно вернём.
                let _ = mem.write(s.addr, &[status]);
            }
            queue.add_used(mem, self.chain.head, written + 1)?;
            any = true;
        }
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rustdroid_virtio::mem::{GuestMemoryExt, VecMemory};
    use rustdroid_virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};

    use super::*;
    use crate::disk::MemDisk;

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;

    fn queue() -> Queue {
        let mut q = Queue::new(16);
        q.desc_table = DESC;
        q.avail_ring = AVAIL;
        q.used_ring = USED;
        q.ready = true;
        q
    }

    fn desc(mem: &VecMemory, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut b = [0_u8; 16];
        b[..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        mem.write(DESC + u64::from(i) * 16, &b).unwrap();
    }

    fn request(
        mem: &VecMemory,
        kind: u32,
        sector: u64,
        data_len: u32,
        write_data: bool,
        avail: u16,
    ) {
        let mut h = [0_u8; 16];
        h[..4].copy_from_slice(&kind.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        mem.write(0x5000, &h).unwrap();
        desc(mem, 0, 0x5000, 16, VIRTQ_DESC_F_NEXT, 1);
        let f = if write_data { VIRTQ_DESC_F_WRITE } else { 0 };
        desc(mem, 1, 0x6000, data_len, f | VIRTQ_DESC_F_NEXT, 2);
        desc(mem, 2, 0x7000, 1, VIRTQ_DESC_F_WRITE, 0);
        mem.write_u16(AVAIL + 4 + u64::from(avail % 16) * 2, 0)
            .unwrap();
        mem.write_u16(AVAIL + 2, avail + 1).unwrap();
    }

    fn status(mem: &VecMemory) -> u8 {
        let mut s = [0xFF_u8];
        mem.read(0x7000, &mut s).unwrap();
        s[0]
    }

    fn blk(data: Vec<u8>, ro: bool) -> VirtioBlk {
        VirtioBlk::new(Arc::new(MemDisk(Mutex::new(data), ro)), "rustdroid-test")
    }

    #[test]
    fn config_reports_capacity_and_features() {
        let b = blk(vec![0; 8192], true);
        let mut cap = [0_u8; 8];
        b.read_config(0, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 16);
        assert_ne!(b.device_features() & VIRTIO_BLK_F_RO, 0);
    }

    #[test]
    fn read_write_flush_get_id() {
        let mem = VecMemory::new(0x10000);
        let mut q = queue();
        let mut data = vec![0_u8; 4096];
        data[512..516].copy_from_slice(b"DATA");
        let mut b = blk(data, false);

        request(&mem, T_IN, 1, 512, true, 0);
        assert!(b.process_queue(0, &mut q, &mem).unwrap());
        assert_eq!(status(&mem), S_OK);
        let mut got = [0_u8; 4];
        mem.read(0x6000, &mut got).unwrap();
        assert_eq!(&got, b"DATA");
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 513);

        mem.write(0x6000, &[0x5A; 512]).unwrap();
        request(&mem, T_OUT, 2, 512, false, 1);
        b.process_queue(0, &mut q, &mem).unwrap();
        assert_eq!(status(&mem), S_OK);
        let mut back = [0_u8; 1];
        b.disk.read_at(&mut back, 1024).unwrap();
        assert_eq!(back[0], 0x5A);

        request(&mem, T_GET_ID, 0, 20, true, 2);
        b.process_queue(0, &mut q, &mem).unwrap();
        let mut id = [0_u8; 14];
        mem.read(0x6000, &mut id).unwrap();
        assert_eq!(&id, b"rustdroid-test");

        request(&mem, T_FLUSH, 0, 0, false, 3);
        b.process_queue(0, &mut q, &mem).unwrap();
        assert_eq!(status(&mem), S_OK);
    }

    #[test]
    fn out_of_range_and_read_only_fail() {
        let mem = VecMemory::new(0x10000);
        let mut q = queue();
        let mut b = blk(vec![0; 1024], true);
        request(&mem, T_IN, 2, 512, true, 0);
        b.process_queue(0, &mut q, &mem).unwrap();
        assert_eq!(status(&mem), S_IOERR);
        request(&mem, T_OUT, 0, 512, false, 1);
        b.process_queue(0, &mut q, &mem).unwrap();
        assert_eq!(status(&mem), S_IOERR);
        request(&mem, 99, 0, 512, false, 2);
        b.process_queue(0, &mut q, &mem).unwrap();
        assert_eq!(status(&mem), S_UNSUPP);
    }
}
