//! Дисковые бэкенды для virtio-blk.
//!
//! - [`RawDisk`] — обычный файл образа (только чтение или чтение-запись);
//! - [`CowDisk`] — copy-on-write оверлей поверх базового образа: изменения пишутся в отдельный
//!   файл кластерами по 64 КиБ, базовый образ не меняется (мгновенный сброс инстанса — удалить
//!   оверлей);
//! - [`SparseDisk`] — образ Android sparse (`simg`) только для чтения, без распаковки;
//! - [`CompositeDisk`] — один диск с синтезированной таблицей GPT из нескольких образов
//!   (system, vendor, super, userdata…) без копирования данных.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError, RwLock};

/// Размер сектора virtio-blk.
pub const SECTOR_SIZE: u64 = 512;

/// Блочное устройство.
pub trait Disk: Send + Sync {
    /// Размер в байтах.
    fn size(&self) -> u64;
    /// Только чтение.
    fn read_only(&self) -> bool;
    /// Читает `buf.len()` байт с `offset`. Чтение за концом данных возвращает нули.
    ///
    /// # Errors
    /// Ошибка ввода-вывода.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Пишет `buf` по `offset`.
    ///
    /// # Errors
    /// Ошибка ввода-вывода или диск только для чтения.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Сбрасывает данные на носитель.
    ///
    /// # Errors
    /// Ошибка ввода-вывода.
    fn flush(&self) -> io::Result<()>;
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}
#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}
#[cfg(unix)]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}
#[cfg(windows)]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

/// Позиционное чтение; хвост за концом файла заполняется нулями.
fn read_exact_or_zero(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match pread(file, buf, offset) {
            Ok(0) => {
                buf.fill(0);
                return Ok(());
            }
            Ok(n) => {
                buf = buf.get_mut(n..).unwrap_or_default();
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match pwrite(file, buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "запись 0 байт")),
            Ok(n) => {
                buf = buf.get(n..).unwrap_or_default();
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_only_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "диск только для чтения")
}

/// Файл образа.
#[derive(Debug)]
pub struct RawDisk {
    file: File,
    size: u64,
    read_only: bool,
}

impl RawDisk {
    /// Открывает образ.
    ///
    /// # Errors
    /// Файл не открывается.
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file,
            size,
            read_only,
        })
    }
}

impl Disk for RawDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        read_exact_or_zero(&self.file, buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.read_only {
            return Err(read_only_error());
        }
        write_all_at(&self.file, buf, offset)
    }
    fn flush(&self) -> io::Result<()> {
        if self.read_only {
            Ok(())
        } else {
            self.file.sync_data()
        }
    }
}

/// Пустой диск заданного размера, читается нулями (например, `misc`).
#[derive(Debug)]
pub struct ZeroDisk(pub u64);

impl Disk for ZeroDisk {
    fn size(&self) -> u64 {
        self.0
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<()> {
        Err(read_only_error())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------- Copy-on-write ----------------

const COW_MAGIC: &[u8; 8] = b"RDCOWv1\0";
const COW_HEADER: u64 = 4096;
/// Размер кластера оверлея.
pub const COW_CLUSTER: u64 = 64 * 1024;

/// Copy-on-write оверлей поверх базового диска.
///
/// Формат файла: заголовок 4 КиБ (`RDCOWv1`, размер кластера, виртуальный размер), таблица
/// `u64` на каждый кластер (0 — кластер в базе, иначе смещение данных в оверлее), данные
/// кластеров дописываются в конец.
pub struct CowDisk {
    base: Box<dyn Disk>,
    file: File,
    size: u64,
    table: RwLock<Vec<u64>>,
    alloc: Mutex<u64>,
}

impl std::fmt::Debug for CowDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CowDisk")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl CowDisk {
    /// Открывает оверлей `path` поверх `base` или создаёт новый.
    ///
    /// # Errors
    /// Ошибка ввода-вывода или оверлей создан для другого размера базы.
    pub fn open(base: Box<dyn Disk>, path: &Path) -> io::Result<Self> {
        let size = base.size();
        let clusters = size.div_ceil(COW_CLUSTER);
        let table_bytes = clusters * 8;
        let data_start = (COW_HEADER + table_bytes).div_ceil(COW_CLUSTER) * COW_CLUSTER;
        let exists = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let n = usize::try_from(clusters).map_err(|_| io::Error::other("слишком большой диск"))?;
        let mut table = vec![0_u64; n];
        if exists && file.metadata()?.len() >= COW_HEADER {
            let mut hdr = [0_u8; 32];
            read_exact_or_zero(&file, &mut hdr, 0)?;
            let virt = u64::from_le_bytes(hdr[16..24].try_into().map_err(io::Error::other)?);
            if &hdr[..8] != COW_MAGIC || virt != size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "оверлей не соответствует базовому образу",
                ));
            }
            let mut raw = vec![0_u8; n * 8];
            read_exact_or_zero(&file, &mut raw, COW_HEADER)?;
            for (entry, chunk) in table.iter_mut().zip(raw.as_chunks::<8>().0) {
                *entry = u64::from_le_bytes(*chunk);
            }
        } else {
            let mut hdr = vec![0_u8; COW_HEADER as usize];
            hdr[..8].copy_from_slice(COW_MAGIC);
            #[allow(clippy::cast_possible_truncation, reason = "кластер 64 КиБ")]
            hdr[8..12].copy_from_slice(&(COW_CLUSTER as u32).to_le_bytes());
            hdr[16..24].copy_from_slice(&size.to_le_bytes());
            write_all_at(&file, &hdr, 0)?;
            file.set_len(data_start)?;
        }
        let end = file.metadata()?.len().max(data_start);
        let alloc = end.div_ceil(COW_CLUSTER) * COW_CLUSTER;
        Ok(Self {
            base,
            file,
            size,
            table: RwLock::new(table),
            alloc: Mutex::new(alloc),
        })
    }

    /// Сколько кластеров уже изменено.
    #[must_use]
    pub fn allocated_clusters(&self) -> usize {
        self.table
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|&&e| e != 0)
            .count()
    }

    fn for_each_cluster(
        offset: u64,
        len: usize,
        mut f: impl FnMut(usize, u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut done = 0_usize;
        while done < len {
            let pos = offset + done as u64;
            let idx = usize::try_from(pos / COW_CLUSTER).map_err(io::Error::other)?;
            let within = pos % COW_CLUSTER;
            let n = ((COW_CLUSTER - within) as usize).min(len - done);
            f(idx, within, done..done + n)?;
            done += n;
        }
        Ok(())
    }
}

impl Disk for CowDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        false
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let table = self.table.read().unwrap_or_else(PoisonError::into_inner);
        Self::for_each_cluster(offset, buf.len(), |idx, within, range| {
            let dst = buf
                .get_mut(range)
                .ok_or_else(|| io::Error::other("диапазон"))?;
            match table.get(idx).copied() {
                Some(0) | None => self.base.read_at(dst, offset_of(idx, within)),
                Some(at) => read_exact_or_zero(&self.file, dst, at + within),
            }
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "запись за концом диска",
            ));
        }
        Self::for_each_cluster(offset, buf.len(), |idx, within, range| {
            let src = buf.get(range).ok_or_else(|| io::Error::other("диапазон"))?;
            let existing = self
                .table
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(idx)
                .copied()
                .unwrap_or(0);
            if existing != 0 {
                return write_all_at(&self.file, src, existing + within);
            }
            // Первая запись в кластер: копия из базы + новые данные, затем запись в таблицу.
            let mut table = self.table.write().unwrap_or_else(PoisonError::into_inner);
            let current = table.get(idx).copied().unwrap_or(0);
            if current != 0 {
                return write_all_at(&self.file, src, current + within);
            }
            let mut cluster = vec![0_u8; COW_CLUSTER as usize];
            self.base.read_at(&mut cluster, offset_of(idx, 0))?;
            let start = within as usize;
            if let Some(dst) = cluster.get_mut(start..start + src.len()) {
                dst.copy_from_slice(src);
            }
            let at = {
                let mut alloc = self.alloc.lock().unwrap_or_else(PoisonError::into_inner);
                let at = *alloc;
                *alloc += COW_CLUSTER;
                at
            };
            write_all_at(&self.file, &cluster, at)?;
            write_all_at(&self.file, &at.to_le_bytes(), COW_HEADER + idx as u64 * 8)?;
            if let Some(e) = table.get_mut(idx) {
                *e = at;
            }
            Ok(())
        })
    }

    fn flush(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

fn offset_of(cluster: usize, within: u64) -> u64 {
    cluster as u64 * COW_CLUSTER + within
}

// ---------------- Составной диск с GPT ----------------

const MIB: u64 = 1 << 20;
const GPT_ENTRIES: u64 = 128;
const GPT_ENTRY_SIZE: u64 = 128;
const GPT_ENTRIES_SECTORS: u64 = GPT_ENTRIES * GPT_ENTRY_SIZE / SECTOR_SIZE; // 32
/// Тип раздела «Linux filesystem data».
const LINUX_DATA_GUID: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

/// Раздел составного диска.
pub struct Partition {
    /// Имя раздела GPT (`super`, `boot_a`, …).
    pub name: String,
    /// Данные.
    pub disk: Box<dyn Disk>,
}

struct Placed {
    name: String,
    disk: Box<dyn Disk>,
    start: u64,
    /// Размер в байтах с округлением до 1 МиБ.
    span: u64,
}

/// Диск из нескольких образов с синтезированной таблицей GPT.
pub struct CompositeDisk {
    parts: Vec<Placed>,
    primary: Vec<u8>,
    backup: Vec<u8>,
    backup_start: u64,
    size: u64,
}

impl std::fmt::Debug for CompositeDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeDisk")
            .field(
                "partitions",
                &self
                    .parts
                    .iter()
                    .map(|p| (&p.name, p.start, p.span))
                    .collect::<Vec<_>>(),
            )
            .field("size", &self.size)
            .finish()
    }
}

/// GUID в формате GPT (первые три поля little-endian).
fn guid_bytes(s: &str) -> [u8; 16] {
    let hex: Vec<u8> = s
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>()
        .as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap_or("0"), 16).unwrap_or(0))
        .collect();
    let mut b = [0_u8; 16];
    for (dst, src) in b.iter_mut().zip(&hex) {
        *dst = *src;
    }
    b[0..4].reverse();
    b[4..6].reverse();
    b[6..8].reverse();
    b
}

/// Детерминированный GUID раздела из имени (стабилен между запусками).
fn name_guid(disk_seed: u64, name: &str) -> [u8; 16] {
    let mut h1 = 0xCBF2_9CE4_8422_2325_u64 ^ disk_seed;
    let mut h2 = 0x8422_2325_CBF2_9CE4_u64;
    for b in name.bytes() {
        h1 = (h1 ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        h2 = (h2 ^ u64::from(b))
            .wrapping_mul(0x0000_0100_0000_01B3)
            .rotate_left(7);
    }
    let mut g = [0_u8; 16];
    g[..8].copy_from_slice(&h1.to_le_bytes());
    g[8..].copy_from_slice(&h2.to_le_bytes());
    g[7] = (g[7] & 0x0F) | 0x40; // версия 4
    g[8] = (g[8] & 0x3F) | 0x80; // вариант RFC 4122
    g
}

impl CompositeDisk {
    /// Собирает диск. Разделы выравниваются на 1 МиБ.
    ///
    /// # Errors
    /// Больше 128 разделов или имя длиннее 36 символов.
    pub fn new(partitions: Vec<Partition>) -> io::Result<Self> {
        if partitions.len() as u64 > GPT_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "больше 128 разделов",
            ));
        }
        let mut parts = Vec::with_capacity(partitions.len());
        let mut pos = MIB;
        for p in partitions {
            if p.name.encode_utf16().count() > 36 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("слишком длинное имя раздела {}", p.name),
                ));
            }
            let span = p.disk.size().div_ceil(MIB).max(1) * MIB;
            parts.push(Placed {
                name: p.name,
                disk: p.disk,
                start: pos,
                span,
            });
            pos += span;
        }
        let backup_start = pos;
        let total_sectors = (pos + MIB) / SECTOR_SIZE;
        let size = total_sectors * SECTOR_SIZE;
        let last_lba = total_sectors - 1;

        let entries = Self::entries(&parts);
        let entries_crc = crc32fast::hash(&entries);
        let header = |current: u64, backup: u64, entries_lba: u64| -> Vec<u8> {
            let mut h = vec![0_u8; SECTOR_SIZE as usize];
            h[0..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
            h[12..16].copy_from_slice(&92_u32.to_le_bytes());
            h[24..32].copy_from_slice(&current.to_le_bytes());
            h[32..40].copy_from_slice(&backup.to_le_bytes());
            h[40..48].copy_from_slice(&(2 + GPT_ENTRIES_SECTORS).to_le_bytes());
            h[48..56].copy_from_slice(&(last_lba - 1 - GPT_ENTRIES_SECTORS).to_le_bytes());
            h[56..72].copy_from_slice(&name_guid(0x5244, "rustdroid-disk"));
            h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
            #[allow(clippy::cast_possible_truncation, reason = "константы GPT")]
            {
                h[80..84].copy_from_slice(&(GPT_ENTRIES as u32).to_le_bytes());
                h[84..88].copy_from_slice(&(GPT_ENTRY_SIZE as u32).to_le_bytes());
            }
            h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
            let crc = crc32fast::hash(&h[..92]);
            h[16..20].copy_from_slice(&crc.to_le_bytes());
            h
        };

        // Первичная область: защитный MBR, заголовок GPT, записи — 1 МиБ.
        let mut primary = vec![0_u8; MIB as usize];
        let mbr_sectors = u32::try_from(last_lba).unwrap_or(u32::MAX);
        primary[446..462]
            .copy_from_slice(&[0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0, 0, 0, 0, 0]);
        primary[458..462].copy_from_slice(&mbr_sectors.to_le_bytes());
        primary[510] = 0x55;
        primary[511] = 0xAA;
        primary[512..1024].copy_from_slice(&header(1, last_lba, 2));
        primary[1024..1024 + entries.len()].copy_from_slice(&entries);

        // Резервная область (последний 1 МиБ): записи, затем заголовок в последнем секторе.
        let mut backup = vec![0_u8; MIB as usize];
        let entries_lba = last_lba - GPT_ENTRIES_SECTORS;
        let entries_off = ((entries_lba * SECTOR_SIZE) - backup_start) as usize;
        backup[entries_off..entries_off + entries.len()].copy_from_slice(&entries);
        let hdr_off = (MIB - SECTOR_SIZE) as usize;
        backup[hdr_off..].copy_from_slice(&header(last_lba, 1, entries_lba));

        Ok(Self {
            parts,
            primary,
            backup,
            backup_start,
            size,
        })
    }

    fn entries(parts: &[Placed]) -> Vec<u8> {
        let mut e = vec![0_u8; (GPT_ENTRIES * GPT_ENTRY_SIZE) as usize];
        let type_guid = guid_bytes(LINUX_DATA_GUID);
        for (i, p) in parts.iter().enumerate() {
            let at = i * GPT_ENTRY_SIZE as usize;
            let Some(slot) = e.get_mut(at..at + GPT_ENTRY_SIZE as usize) else {
                break;
            };
            slot[0..16].copy_from_slice(&type_guid);
            slot[16..32].copy_from_slice(&name_guid(0x5244, &p.name));
            slot[32..40].copy_from_slice(&(p.start / SECTOR_SIZE).to_le_bytes());
            slot[40..48].copy_from_slice(&((p.start + p.span) / SECTOR_SIZE - 1).to_le_bytes());
            for (j, c) in p.name.encode_utf16().take(36).enumerate() {
                slot[56 + j * 2..58 + j * 2].copy_from_slice(&c.to_le_bytes());
            }
        }
        e
    }

    /// Имена и смещения разделов (для логов и тестов).
    #[must_use]
    pub fn layout(&self) -> Vec<(String, u64, u64)> {
        self.parts
            .iter()
            .map(|p| (p.name.clone(), p.start, p.span))
            .collect()
    }

    fn for_each_region(
        &self,
        offset: u64,
        len: usize,
        mut f: impl FnMut(Region<'_>, u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut done = 0_usize;
        while done < len {
            let pos = offset + done as u64;
            let (region, rel, region_end) = if pos < MIB {
                (Region::Primary, pos, MIB)
            } else if pos >= self.backup_start {
                (Region::Backup, pos - self.backup_start, self.size)
            } else {
                match self
                    .parts
                    .iter()
                    .find(|p| pos >= p.start && pos < p.start + p.span)
                {
                    Some(p) => (Region::Part(p), pos - p.start, p.start + p.span),
                    None => (Region::Zero, 0, pos + 1),
                }
            };
            let n = usize::try_from((region_end - pos).min((len - done) as u64))
                .map_err(io::Error::other)?;
            f(region, rel, done..done + n)?;
            done += n;
        }
        Ok(())
    }
}

enum Region<'a> {
    Primary,
    Backup,
    Part(&'a Placed),
    Zero,
}

impl Disk for CompositeDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        false
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.for_each_region(offset, buf.len(), |region, rel, range| {
            let dst = buf
                .get_mut(range)
                .ok_or_else(|| io::Error::other("диапазон"))?;
            let copy = |src: &[u8], dst: &mut [u8]| {
                let r = rel as usize;
                dst.copy_from_slice(src.get(r..r + dst.len()).unwrap_or(&[]));
            };
            match region {
                Region::Primary => copy(&self.primary, dst),
                Region::Backup => copy(&self.backup, dst),
                Region::Zero => dst.fill(0),
                Region::Part(p) => {
                    let size = p.disk.size();
                    if rel >= size {
                        dst.fill(0);
                    } else {
                        let n = usize::try_from((size - rel).min(dst.len() as u64))
                            .map_err(io::Error::other)?;
                        let (data, pad) = dst.split_at_mut(n);
                        p.disk.read_at(data, rel)?;
                        pad.fill(0);
                    }
                }
            }
            Ok(())
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.for_each_region(offset, buf.len(), |region, rel, range| {
            let src = buf.get(range).ok_or_else(|| io::Error::other("диапазон"))?;
            match region {
                Region::Part(p) => {
                    let size = p.disk.size();
                    if rel + src.len() as u64 > size {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("запись за концом раздела {}", p.name),
                        ));
                    }
                    p.disk.write_at(src, rel)
                }
                // Таблица разделов синтезирована и неизменяема.
                _ => {
                    tracing::warn!(
                        offset = rel,
                        "запись в служебную область составного диска проигнорирована"
                    );
                    Ok(())
                }
            }
        })
    }

    fn flush(&self) -> io::Result<()> {
        for p in &self.parts {
            p.disk.flush()?;
        }
        Ok(())
    }
}

/// Сигнатура образа Android sparse (`ED26FF3A`, little-endian).
pub const SPARSE_MAGIC: u32 = 0xED26_FF3A;
const SPARSE_HEADER_LEN: usize = 28;
const SPARSE_CHUNK_HEADER_LEN: usize = 12;
const CHUNK_RAW: u16 = 0xCAC1;
const CHUNK_FILL: u16 = 0xCAC2;
const CHUNK_DONT_CARE: u16 = 0xCAC3;
const CHUNK_CRC32: u16 = 0xCAC4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SparseData {
    /// Данные лежат в файле с этого смещения.
    Raw(u64),
    /// Блок заполнен 4-байтовым шаблоном.
    Fill([u8; 4]),
    /// Нули.
    Zero,
}

#[derive(Debug, Clone, Copy)]
struct SparseChunk {
    /// Начало в развёрнутом образе, байты.
    start: u64,
    /// Длина, байты.
    len: u64,
    data: SparseData,
}

/// Образ Android sparse (`simg`) только для чтения, без распаковки на диск.
///
/// Так поставляются `super.img` и `userdata.img` в архивах Cuttlefish. При открытии строится
/// индекс чанков; чтение находит чанк двоичным поиском. Запись — через [`CowDisk`] поверх.
#[derive(Debug)]
pub struct SparseDisk {
    file: File,
    size: u64,
    chunks: Vec<SparseChunk>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .and_then(|s| s.try_into().ok())
        .map_or(0, u16::from_le_bytes)
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

impl SparseDisk {
    /// Открывает sparse-образ и строит индекс чанков.
    ///
    /// # Errors
    /// Файл не открывается или повреждён.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut header = [0_u8; SPARSE_HEADER_LEN];
        read_exact_or_zero(&file, &mut header, 0)?;
        if le32(&header, 0) != SPARSE_MAGIC {
            return Err(invalid("не sparse-образ"));
        }
        if le16(&header, 4) != 1 {
            return Err(invalid(format!(
                "неподдерживаемая версия sparse {}",
                le16(&header, 4)
            )));
        }
        let file_hdr = u64::from(le16(&header, 8));
        let chunk_hdr = u64::from(le16(&header, 10));
        let block = u64::from(le32(&header, 12));
        let total_blocks = u64::from(le32(&header, 16));
        let total_chunks = le32(&header, 20);
        if file_hdr < SPARSE_HEADER_LEN as u64
            || chunk_hdr < SPARSE_CHUNK_HEADER_LEN as u64
            || block == 0
            || block % 4 != 0
        {
            return Err(invalid("некорректный заголовок sparse"));
        }

        let mut chunks = Vec::with_capacity(total_chunks.min(1 << 20) as usize);
        let mut pos = file_hdr;
        let mut out = 0_u64;
        for _ in 0..total_chunks {
            let mut ch = [0_u8; SPARSE_CHUNK_HEADER_LEN];
            read_exact_or_zero(&file, &mut ch, pos)?;
            let kind = le16(&ch, 0);
            let blocks = u64::from(le32(&ch, 4));
            let total = u64::from(le32(&ch, 8));
            let body = pos + chunk_hdr;
            let len = blocks
                .checked_mul(block)
                .ok_or_else(|| invalid("переполнение размера чанка"))?;
            let data = match kind {
                CHUNK_RAW => {
                    if total != chunk_hdr + len || body + len > file_len {
                        return Err(invalid("чанк raw выходит за файл"));
                    }
                    Some(SparseData::Raw(body))
                }
                CHUNK_FILL => {
                    let mut pattern = [0_u8; 4];
                    read_exact_or_zero(&file, &mut pattern, body)?;
                    Some(if pattern == [0; 4] {
                        SparseData::Zero
                    } else {
                        SparseData::Fill(pattern)
                    })
                }
                CHUNK_DONT_CARE => Some(SparseData::Zero),
                CHUNK_CRC32 => None,
                other => return Err(invalid(format!("неизвестный тип чанка 0x{other:04X}"))),
            };
            if let Some(data) = data
                && len > 0
            {
                chunks.push(SparseChunk {
                    start: out,
                    len,
                    data,
                });
                out += len;
            }
            if total < chunk_hdr {
                return Err(invalid("некорректный размер чанка"));
            }
            pos += total;
        }
        let size = total_blocks
            .checked_mul(block)
            .ok_or_else(|| invalid("переполнение размера образа"))?;
        if out != size {
            return Err(invalid(format!(
                "чанки покрывают {out} байт, а в заголовке {size}"
            )));
        }
        Ok(Self { file, size, chunks })
    }

    /// Является ли файл sparse-образом.
    ///
    /// # Errors
    /// Файл не открывается.
    pub fn is_sparse(path: &Path) -> io::Result<bool> {
        let file = File::open(path)?;
        let mut magic = [0_u8; 4];
        read_exact_or_zero(&file, &mut magic, 0)?;
        Ok(u32::from_le_bytes(magic) == SPARSE_MAGIC)
    }
}

impl Disk for SparseDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        let mut idx = self.chunks.partition_point(|c| c.start + c.len <= offset);
        while !buf.is_empty() {
            let Some(c) = self.chunks.get(idx) else {
                buf.fill(0);
                return Ok(());
            };
            let within = offset.saturating_sub(c.start);
            let n = usize::try_from((c.len - within).min(buf.len() as u64)).unwrap_or(buf.len());
            let (head, tail) = buf.split_at_mut(n);
            match c.data {
                SparseData::Raw(at) => read_exact_or_zero(&self.file, head, at + within)?,
                SparseData::Zero => head.fill(0),
                SparseData::Fill(p) => {
                    for (i, b) in head.iter_mut().enumerate() {
                        *b = p[(within as usize + i) % 4];
                    }
                }
            }
            buf = tail;
            offset += n as u64;
            idx += 1;
        }
        Ok(())
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<()> {
        Err(read_only_error())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Открывает образ только для чтения: sparse — как [`SparseDisk`], иначе как [`RawDisk`].
///
/// # Errors
/// Файл не открывается или sparse-образ повреждён.
pub fn open_image(path: &Path) -> io::Result<Box<dyn Disk>> {
    if SparseDisk::is_sparse(path)? {
        Ok(Box::new(SparseDisk::open(path)?))
    } else {
        Ok(Box::new(RawDisk::open(path, true)?))
    }
}

/// Диск в памяти (для тестов).
#[cfg(test)]
pub(crate) struct MemDisk(pub Mutex<Vec<u8>>, pub bool);

#[cfg(test)]
impl Disk for MemDisk {
    fn size(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
    fn read_only(&self) -> bool {
        self.1
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let d = self.0.lock().unwrap();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = d.get(offset as usize + i).copied().unwrap_or(0);
        }
        Ok(())
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        if self.1 {
            return Err(read_only_error());
        }
        let mut d = self.0.lock().unwrap();
        let o = offset as usize;
        d.get_mut(o..o + buf.len())
            .ok_or_else(|| io::Error::other("за концом"))?
            .copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rd-disk-{}-{name}", std::process::id()))
    }

    /// Собирает sparse-образ: блок 4096, чанки raw, fill, don't care и crc.
    fn sparse_image() -> (Vec<u8>, Vec<u8>) {
        const B: usize = 4096;
        let mut f = Vec::new();
        let hdr = |f: &mut Vec<u8>, blocks: u32, chunks: u32| {
            f.extend_from_slice(&SPARSE_MAGIC.to_le_bytes());
            f.extend_from_slice(&1_u16.to_le_bytes());
            f.extend_from_slice(&0_u16.to_le_bytes());
            f.extend_from_slice(&28_u16.to_le_bytes());
            f.extend_from_slice(&12_u16.to_le_bytes());
            f.extend_from_slice(&(B as u32).to_le_bytes());
            f.extend_from_slice(&blocks.to_le_bytes());
            f.extend_from_slice(&chunks.to_le_bytes());
            f.extend_from_slice(&0_u32.to_le_bytes());
        };
        let chunk = |f: &mut Vec<u8>, kind: u16, blocks: u32, body: &[u8]| {
            f.extend_from_slice(&kind.to_le_bytes());
            f.extend_from_slice(&0_u16.to_le_bytes());
            f.extend_from_slice(&blocks.to_le_bytes());
            f.extend_from_slice(&(12 + body.len() as u32).to_le_bytes());
            f.extend_from_slice(body);
        };
        hdr(&mut f, 6, 5);
        let raw: Vec<u8> = (0..2 * B).map(|i| (i % 251) as u8).collect();
        chunk(&mut f, CHUNK_RAW, 2, &raw);
        chunk(&mut f, CHUNK_FILL, 1, &[1, 2, 3, 4]);
        chunk(&mut f, CHUNK_DONT_CARE, 2, &[]);
        chunk(&mut f, CHUNK_CRC32, 0, &[0; 4]);
        chunk(&mut f, CHUNK_RAW, 1, &vec![0xAB; B]);

        let mut expect = raw;
        expect.extend((0..B).map(|i| [1, 2, 3, 4][i % 4]));
        expect.extend(std::iter::repeat_n(0, 2 * B));
        expect.extend(std::iter::repeat_n(0xAB, B));
        (f, expect)
    }

    #[test]
    fn sparse_image_reads_like_unsparsed() {
        let (file, expect) = sparse_image();
        let path = tmp("sparse.img");
        std::fs::write(&path, &file).unwrap();
        assert!(SparseDisk::is_sparse(&path).unwrap());
        let d = open_image(&path).unwrap();
        assert_eq!(d.size(), expect.len() as u64);
        assert!(d.read_only());

        let mut all = vec![0_u8; expect.len()];
        d.read_at(&mut all, 0).unwrap();
        assert_eq!(all, expect);
        // Чтение через границы чанков и с невыровненным смещением.
        for (off, len) in [(8190, 10), (8193, 4096), (12286, 9000), (24570, 20)] {
            let mut b = vec![0xEE_u8; len];
            d.read_at(&mut b, off as u64).unwrap();
            let want: Vec<u8> = (off..off + len)
                .map(|i| expect.get(i).copied().unwrap_or(0))
                .collect();
            assert_eq!(b, want, "off={off} len={len}");
        }
        assert!(d.write_at(&[1], 0).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn sparse_rejects_truncated_image() {
        let (mut file, _) = sparse_image();
        file.truncate(1000);
        let path = tmp("sparse-bad.img");
        std::fs::write(&path, &file).unwrap();
        assert!(SparseDisk::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_image_falls_back_to_raw() {
        let path = tmp("raw-plain.img");
        std::fs::write(&path, [7_u8; 1024]).unwrap();
        let d = open_image(&path).unwrap();
        assert_eq!(d.size(), 1024);
        std::fs::remove_file(&path).unwrap();
    }

    fn mem(data: Vec<u8>, ro: bool) -> Box<dyn Disk> {
        Box::new(MemDisk(Mutex::new(data), ro))
    }

    #[test]
    fn raw_disk_roundtrip_and_read_only() {
        let p = tmp("raw");
        std::fs::write(&p, vec![7_u8; 4096]).unwrap();
        let d = RawDisk::open(&p, false).unwrap();
        d.write_at(b"abc", 100).unwrap();
        let mut b = [0_u8; 5];
        d.read_at(&mut b, 99).unwrap();
        assert_eq!(&b, &[7, b'a', b'b', b'c', 7]);
        // Чтение за концом — нули.
        let mut tail = [1_u8; 8];
        d.read_at(&mut tail, 4092).unwrap();
        assert_eq!(tail, [7, 7, 7, 7, 0, 0, 0, 0]);
        let ro = RawDisk::open(&p, true).unwrap();
        assert!(ro.write_at(b"x", 0).is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn cow_keeps_base_and_persists() {
        let p = tmp("cow");
        let _ = std::fs::remove_file(&p);
        let base_data: Vec<u8> = (0..(3 * COW_CLUSTER as usize))
            .map(|i| (i % 251) as u8)
            .collect();
        {
            let cow = CowDisk::open(mem(base_data.clone(), true), &p).unwrap();
            // Запись через границу кластеров.
            let off = COW_CLUSTER - 2;
            cow.write_at(&[0xAA; 4], off).unwrap();
            assert_eq!(cow.allocated_clusters(), 2);
            let mut b = [0_u8; 6];
            cow.read_at(&mut b, off - 1).unwrap();
            assert_eq!(
                b,
                [
                    base_data[off as usize - 1],
                    0xAA,
                    0xAA,
                    0xAA,
                    0xAA,
                    base_data[off as usize + 4]
                ]
            );
            cow.flush().unwrap();
        }
        // Повторное открытие видит изменения, база не тронута.
        let cow = CowDisk::open(mem(base_data.clone(), true), &p).unwrap();
        assert_eq!(cow.allocated_clusters(), 2);
        let mut b = [0_u8; 1];
        cow.read_at(&mut b, COW_CLUSTER).unwrap();
        assert_eq!(b[0], 0xAA);
        cow.read_at(&mut b, 2 * COW_CLUSTER + 5).unwrap();
        assert_eq!(b[0], base_data[2 * COW_CLUSTER as usize + 5]);
        // Оверлей другого размера отвергается.
        assert!(CowDisk::open(mem(vec![0; 1000], true), &p).is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn guid_encoding_is_mixed_endian() {
        let g = guid_bytes(LINUX_DATA_GUID);
        assert_eq!(&g[..4], &[0xAF, 0x3D, 0xC6, 0x0F]);
        assert_eq!(&g[8..10], &[0x8E, 0x79]);
    }

    #[test]
    fn composite_disk_gpt_and_data() {
        let d = CompositeDisk::new(vec![
            Partition {
                name: "boot_a".into(),
                disk: mem(vec![1; 1000], true),
            },
            Partition {
                name: "userdata".into(),
                disk: mem(vec![2; 3 * MIB as usize], false),
            },
        ])
        .unwrap();
        let layout = d.layout();
        assert_eq!(layout[0], ("boot_a".into(), MIB, MIB));
        assert_eq!(layout[1], ("userdata".into(), 2 * MIB, 3 * MIB));
        assert_eq!(d.size(), 6 * MIB);

        // Защитный MBR и заголовок GPT с корректными CRC.
        let mut s = vec![0_u8; 1024];
        d.read_at(&mut s, 0).unwrap();
        assert_eq!(&s[510..512], &[0x55, 0xAA]);
        assert_eq!(s[450], 0xEE);
        let hdr = &s[512..1024];
        assert_eq!(&hdr[..8], b"EFI PART");
        let mut check = hdr[..92].to_vec();
        let stored = u32::from_le_bytes(check[16..20].try_into().unwrap());
        check[16..20].fill(0);
        assert_eq!(crc32fast::hash(&check), stored);

        let mut entries = vec![0_u8; 16384];
        d.read_at(&mut entries, 1024).unwrap();
        assert_eq!(
            crc32fast::hash(&entries),
            u32::from_le_bytes(hdr[88..92].try_into().unwrap())
        );
        let name: Vec<u16> = entries[56..68]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&name).unwrap(), "boot_a");

        // Резервный заголовок в последнем секторе.
        let mut last = vec![0_u8; 512];
        d.read_at(&mut last, d.size() - 512).unwrap();
        assert_eq!(&last[..8], b"EFI PART");

        // Данные разделов и дополнение нулями.
        let mut b = [0_u8; 4];
        d.read_at(&mut b, MIB + 998).unwrap();
        assert_eq!(b, [1, 1, 0, 0]);
        d.write_at(&[9, 9], 2 * MIB + 10).unwrap();
        d.read_at(&mut b, 2 * MIB + 9).unwrap();
        assert_eq!(b, [2, 9, 9, 2]);
        // Запись в read-only раздел — ошибка.
        assert!(d.write_at(&[0], MIB).is_err());
    }
}
