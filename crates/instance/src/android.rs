//! Подготовка загрузки Android из образов Cuttlefish x86_64.
//!
//! - разбор `boot.img` / `init_boot.img` (boot image v3/v4) и `vendor_boot.img` (v3/v4);
//! - initrd = vendor ramdisk(s) + generic ramdisk (`init_boot`) + bootconfig;
//! - bootconfig = параметры из `vendor_boot` + параметры RustDroid (наши побеждают);
//! - план разделов GPT для составного диска.
//!
//! Форматы — `system/tools/mkbootimg/include/bootimg/bootimg.h` (AOSP), bootconfig —
//! `Documentation/admin-guide/bootconfig.rst`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Ошибки разбора образов.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AndroidError {
    /// Неверная сигнатура или версия заголовка.
    #[error("{0}: неподдерживаемый формат образа")]
    Format(&'static str),
    /// Образ короче, чем указано в заголовке.
    #[error("{0}: образ повреждён или обрезан")]
    Truncated(&'static str),
    /// Нет обязательного образа.
    #[error("в каталоге образов нет {0}")]
    Missing(&'static str),
}

const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
const PAGE_V3: usize = 4096;
const BOOTCONFIG_MAGIC: &[u8; 12] = b"#BOOTCONFIG\n";

fn u32_at(b: &[u8], off: usize, what: &'static str) -> Result<u32, AndroidError> {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(AndroidError::Truncated(what))
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(b.get(..end).unwrap_or_default())
        .trim()
        .to_owned()
}

fn align(v: usize, page: usize) -> usize {
    v.div_ceil(page) * page
}

fn slice<'a>(
    b: &'a [u8],
    off: usize,
    len: usize,
    what: &'static str,
) -> Result<&'a [u8], AndroidError> {
    b.get(off..off + len).ok_or(AndroidError::Truncated(what))
}

/// Разобранный `boot.img` / `init_boot.img` (v3/v4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootImage<'a> {
    /// Версия заголовка.
    pub version: u32,
    /// Ядро (пусто у `init_boot`).
    pub kernel: &'a [u8],
    /// Generic ramdisk.
    pub ramdisk: &'a [u8],
    /// Командная строка из заголовка.
    pub cmdline: String,
}

impl<'a> BootImage<'a> {
    /// Разбирает образ.
    ///
    /// # Errors
    /// Неверная сигнатура, версия ниже 3 или обрезанный файл.
    pub fn parse(b: &'a [u8], what: &'static str) -> Result<Self, AndroidError> {
        if b.get(..8) != Some(BOOT_MAGIC.as_slice()) {
            return Err(AndroidError::Format(what));
        }
        let version = u32_at(b, 40, what)?;
        if !(3..=4).contains(&version) {
            return Err(AndroidError::Format(what));
        }
        let kernel_size = u32_at(b, 8, what)? as usize;
        let ramdisk_size = u32_at(b, 12, what)? as usize;
        let header_size = u32_at(b, 20, what)? as usize;
        let cmdline = cstr(slice(b, 44, 1536, what)?);
        let kernel_off = align(header_size, PAGE_V3);
        let ramdisk_off = kernel_off + align(kernel_size, PAGE_V3);
        Ok(Self {
            version,
            kernel: slice(b, kernel_off, kernel_size, what)?,
            ramdisk: slice(b, ramdisk_off, ramdisk_size, what)?,
            cmdline,
        })
    }
}

/// Разобранный `vendor_boot.img` (v3/v4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorBoot<'a> {
    /// Версия заголовка.
    pub version: u32,
    /// Все vendor ramdisk'и подряд (как их и нужно передать ядру).
    pub ramdisk: &'a [u8],
    /// Командная строка.
    pub cmdline: String,
    /// Текст bootconfig (v4).
    pub bootconfig: String,
}

impl<'a> VendorBoot<'a> {
    /// Разбирает образ.
    ///
    /// # Errors
    /// Неверная сигнатура, версия или обрезанный файл.
    pub fn parse(b: &'a [u8]) -> Result<Self, AndroidError> {
        const W: &str = "vendor_boot.img";
        if b.get(..8) != Some(VENDOR_BOOT_MAGIC.as_slice()) {
            return Err(AndroidError::Format(W));
        }
        let version = u32_at(b, 8, W)?;
        if !(3..=4).contains(&version) {
            return Err(AndroidError::Format(W));
        }
        let page = (u32_at(b, 12, W)? as usize).max(1);
        let ramdisk_size = u32_at(b, 24, W)? as usize;
        let cmdline = cstr(slice(b, 28, 2048, W)?);
        let header_size = u32_at(b, 2096, W)? as usize;
        let dtb_size = u32_at(b, 2100, W)? as usize;
        let ramdisk_off = align(header_size, page);
        let ramdisk = slice(b, ramdisk_off, ramdisk_size, W)?;
        let bootconfig = if version >= 4 {
            let table_size = u32_at(b, 2112, W)? as usize;
            let bootconfig_size = u32_at(b, 2124, W)? as usize;
            let off = ramdisk_off
                + align(ramdisk_size, page)
                + align(dtb_size, page)
                + align(table_size, page);
            String::from_utf8_lossy(slice(b, off, bootconfig_size, W)?).into_owned()
        } else {
            String::new()
        };
        Ok(Self {
            version,
            ramdisk,
            cmdline,
            bootconfig,
        })
    }
}

/// Параметры bootconfig: разбор `key = value` и сборка без дубликатов ключей
/// (дубликаты ядро считает ошибкой).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootConfig(BTreeMap<String, String>);

impl BootConfig {
    /// Разбирает текст bootconfig (по строке на параметр, `#` — комментарий).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim().trim_matches('"');
                map.insert(
                    k.trim().trim_end_matches(':').trim().to_owned(),
                    v.to_owned(),
                );
            }
        }
        Self(map)
    }

    /// Устанавливает параметр (заменяет существующий).
    pub fn set(&mut self, key: &str, value: &str) {
        self.0.insert(key.to_owned(), value.to_owned());
    }

    /// Значение параметра.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Текст bootconfig.
    #[must_use]
    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k} = \"{v}\"\n"))
            .collect()
    }
}

/// Добавляет к initrd блок bootconfig с трейлером (`size`, `checksum`, `#BOOTCONFIG\n`).
#[must_use]
pub fn append_bootconfig(mut initrd: Vec<u8>, bootconfig: &str) -> Vec<u8> {
    let mut data = bootconfig.as_bytes().to_vec();
    data.push(0);
    // Размер данных выравнивается до 4 байт нулями.
    while !data.len().is_multiple_of(4) {
        data.push(0);
    }
    let checksum = data
        .iter()
        .fold(0_u32, |a, &b| a.wrapping_add(u32::from(b)));
    #[allow(clippy::cast_possible_truncation, reason = "bootconfig — килобайты")]
    let size = data.len() as u32;
    initrd.extend_from_slice(&data);
    initrd.extend_from_slice(&size.to_le_bytes());
    initrd.extend_from_slice(&checksum.to_le_bytes());
    initrd.extend_from_slice(BOOTCONFIG_MAGIC);
    initrd
}

/// Раздел диска Android.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionPlan {
    /// Имя раздела GPT.
    pub name: String,
    /// Образ (или `None` — пустой раздел размера `empty_size`).
    pub image: Option<PathBuf>,
    /// Размер пустого раздела.
    pub empty_size: u64,
}

/// Образы, которые кладутся на диск как разделы (с суффиксом слота `_a`, кроме общих).
const SLOTTED: &[&str] = &[
    "boot",
    "init_boot",
    "vendor_boot",
    "vendor_kernel_boot",
    "vbmeta",
    "vbmeta_system",
    "vbmeta_system_dlkm",
    "vbmeta_vendor_dlkm",
];
const SHARED: &[&str] = &["super", "userdata"];

/// План разделов по каталогу образов: слотовые разделы `_a`, `super`, `userdata`, плюс пустые
/// `misc` (1 МиБ) и `metadata` (64 МиБ), которые Android форматирует сам.
///
/// # Errors
/// Нет `super.img`.
pub fn partition_plan(dir: &Path) -> Result<Vec<PartitionPlan>, AndroidError> {
    let exists = |n: &str| {
        dir.join(format!("{n}.img"))
            .is_file()
            .then(|| dir.join(format!("{n}.img")))
    };
    if exists("super").is_none() {
        return Err(AndroidError::Missing("super.img"));
    }
    let mut plan = vec![PartitionPlan {
        name: "misc".into(),
        image: None,
        empty_size: 1 << 20,
    }];
    for n in SLOTTED {
        if let Some(p) = exists(n) {
            plan.push(PartitionPlan {
                name: format!("{n}_a"),
                image: Some(p),
                empty_size: 0,
            });
        }
    }
    for n in SHARED {
        if let Some(p) = exists(n) {
            plan.push(PartitionPlan {
                name: (*n).to_owned(),
                image: Some(p),
                empty_size: 0,
            });
        }
    }
    plan.push(PartitionPlan {
        name: "metadata".into(),
        image: None,
        empty_size: 64 << 20,
    });
    Ok(plan)
}

/// Параметры bootconfig RustDroid поверх vendor bootconfig.
///
/// `boot_device` — путь PCI диска с разделами, например `pci0000:00/0000:00:01.0`.
#[must_use]
pub fn rustdroid_bootconfig(vendor: &str, boot_device: &str, console: &str) -> BootConfig {
    let mut bc = BootConfig::parse(vendor);
    bc.set("androidboot.boot_devices", boot_device);
    bc.set("androidboot.slot_suffix", "_a");
    bc.set("androidboot.force_normal_boot", "1");
    bc.set("androidboot.verifiedbootstate", "orange");
    bc.set("androidboot.serialno", "RUSTDROID0001");
    bc.set("androidboot.console", console);
    bc.set("androidboot.selinux", "permissive");
    if bc.get("androidboot.hardware").is_none() {
        bc.set("androidboot.hardware", "cutf_cvm");
    }
    bc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot_v4(kernel: &[u8], ramdisk: &[u8], cmdline: &str) -> Vec<u8> {
        let mut h = vec![0_u8; 4096];
        h[..8].copy_from_slice(BOOT_MAGIC);
        h[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
        h[12..16].copy_from_slice(&(ramdisk.len() as u32).to_le_bytes());
        h[20..24].copy_from_slice(&1584_u32.to_le_bytes());
        h[40..44].copy_from_slice(&4_u32.to_le_bytes());
        h[44..44 + cmdline.len()].copy_from_slice(cmdline.as_bytes());
        let mut k = kernel.to_vec();
        k.resize(align(kernel.len(), 4096), 0);
        h.extend(k);
        h.extend_from_slice(ramdisk);
        h
    }

    fn vendor_v4(ramdisk: &[u8], cmdline: &str, bootconfig: &str) -> Vec<u8> {
        let page = 4096_usize;
        let mut h = vec![0_u8; page];
        h[..8].copy_from_slice(VENDOR_BOOT_MAGIC);
        h[8..12].copy_from_slice(&4_u32.to_le_bytes());
        h[12..16].copy_from_slice(&(page as u32).to_le_bytes());
        h[24..28].copy_from_slice(&(ramdisk.len() as u32).to_le_bytes());
        h[28..28 + cmdline.len()].copy_from_slice(cmdline.as_bytes());
        h[2096..2100].copy_from_slice(&2128_u32.to_le_bytes());
        h[2112..2116].copy_from_slice(&108_u32.to_le_bytes()); // таблица ramdisk
        h[2124..2128].copy_from_slice(&(bootconfig.len() as u32).to_le_bytes());
        let mut r = ramdisk.to_vec();
        r.resize(align(ramdisk.len(), page), 0);
        h.extend(r);
        h.extend(vec![0_u8; page]); // таблица (108 байт → страница)
        h.extend_from_slice(bootconfig.as_bytes());
        h
    }

    #[test]
    fn parses_boot_v4() {
        let img = boot_v4(b"KERNEL", b"RAMDISK", "console=ttyS0");
        let b = BootImage::parse(&img, "boot.img").unwrap();
        assert_eq!(b.version, 4);
        assert_eq!(b.kernel, b"KERNEL");
        assert_eq!(b.ramdisk, b"RAMDISK");
        assert_eq!(b.cmdline, "console=ttyS0");
    }

    #[test]
    fn rejects_bad_images() {
        assert_eq!(
            BootImage::parse(b"nope", "boot.img"),
            Err(AndroidError::Format("boot.img"))
        );
        let mut img = boot_v4(b"K", b"R", "");
        img[40] = 2;
        assert!(BootImage::parse(&img, "boot.img").is_err());
        let img = boot_v4(b"K", b"R", "");
        assert_eq!(
            BootImage::parse(&img[..4200], "boot.img"),
            Err(AndroidError::Truncated("boot.img"))
        );
    }

    #[test]
    fn parses_vendor_boot_v4_with_bootconfig() {
        let img = vendor_v4(
            b"VENDOR-RD",
            "loop.max_part=7",
            "androidboot.hardware=cutf_cvm\n",
        );
        let v = VendorBoot::parse(&img).unwrap();
        assert_eq!(v.ramdisk, b"VENDOR-RD");
        assert_eq!(v.cmdline, "loop.max_part=7");
        assert_eq!(v.bootconfig, "androidboot.hardware=cutf_cvm\n");
    }

    #[test]
    fn bootconfig_merge_has_no_duplicates() {
        let bc = rustdroid_bootconfig(
            "androidboot.hardware=cutf_cvm\nandroidboot.console = ttyS1 # comment\n",
            "pci0000:00/0000:00:01.0",
            "ttyS0",
        );
        assert_eq!(bc.get("androidboot.console"), Some("ttyS0"));
        assert_eq!(bc.get("androidboot.hardware"), Some("cutf_cvm"));
        let text = bc.render();
        assert_eq!(text.matches("androidboot.console").count(), 1);
        assert!(text.contains("androidboot.boot_devices = \"pci0000:00/0000:00:01.0\"\n"));
    }

    #[test]
    fn bootconfig_trailer_layout() {
        let out = append_bootconfig(b"RD".to_vec(), "a = \"1\"\n");
        assert!(out.ends_with(BOOTCONFIG_MAGIC));
        let n = out.len();
        let size = u32::from_le_bytes(out[n - 20..n - 16].try_into().unwrap()) as usize;
        let csum = u32::from_le_bytes(out[n - 16..n - 12].try_into().unwrap());
        assert_eq!(size % 4, 0);
        let data = &out[n - 20 - size..n - 20];
        assert_eq!(&out[..2], b"RD");
        assert!(data.starts_with(b"a = \"1\"\n\0"));
        assert_eq!(data.iter().map(|&b| u32::from(b)).sum::<u32>(), csum);
    }

    #[test]
    fn plan_requires_super_and_orders_partitions() {
        let dir = std::env::temp_dir().join(format!("rd-android-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            partition_plan(&dir),
            Err(AndroidError::Missing("super.img"))
        );
        for n in [
            "super",
            "boot",
            "vendor_boot",
            "vbmeta",
            "userdata",
            "boot-debug",
        ] {
            std::fs::write(dir.join(format!("{n}.img")), b"x").unwrap();
        }
        let plan = partition_plan(&dir).unwrap();
        let names: Vec<_> = plan.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "misc",
                "boot_a",
                "vendor_boot_a",
                "vbmeta_a",
                "super",
                "userdata",
                "metadata"
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
