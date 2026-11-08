//! Прямая загрузка Linux: bzImage + initrd + cmdline по 64-битному boot protocol
//! (Documentation/arch/x86/boot.rst), без BIOS.

use crate::error::{Error, Result};
use crate::layout::{CMDLINE_ADDR, CMDLINE_MAX, E820Entry, KERNEL_LOAD_ADDR, ZERO_PAGE_ADDR};
use crate::memory::GuestRam;

// Смещения в boot_params / setup header.
const HDR_START: usize = 0x1F1;
const OFF_SETUP_SECTS: usize = 0x1F1;
const OFF_VID_MODE: usize = 0x1FA;
const OFF_JUMP: usize = 0x200;
const OFF_HEADER_MAGIC: usize = 0x202;
const OFF_VERSION: usize = 0x206;
const OFF_TYPE_OF_LOADER: usize = 0x210;
const OFF_LOADFLAGS: usize = 0x211;
const OFF_RAMDISK_IMAGE: usize = 0x218;
const OFF_RAMDISK_SIZE: usize = 0x21C;
const OFF_CMD_LINE_PTR: usize = 0x228;
const OFF_INITRD_ADDR_MAX: usize = 0x22C;
const OFF_KERNEL_ALIGNMENT: usize = 0x230;
const OFF_RELOCATABLE: usize = 0x234;
const OFF_XLOADFLAGS: usize = 0x236;
const OFF_CMDLINE_SIZE: usize = 0x238;
const OFF_PREF_ADDRESS: usize = 0x258;
const OFF_INIT_SIZE: usize = 0x260;
const OFF_ACPI_RSDP: usize = 0x070;
const OFF_EXT_RAMDISK_IMAGE: usize = 0x0C0;
const OFF_EXT_RAMDISK_SIZE: usize = 0x0C4;
const OFF_EXT_CMD_LINE_PTR: usize = 0x0C8;
const OFF_E820_ENTRIES: usize = 0x1E8;
const OFF_E820_TABLE: usize = 0x2D0;
const E820_MAX: usize = 128;

const MIN_PROTOCOL: u16 = 0x020C; // 2.12: XLF_KERNEL_64
const XLF_KERNEL_64: u16 = 1 << 0;
const LOADED_HIGH: u8 = 1 << 0;
const ZERO_PAGE_SIZE: usize = 4096;

/// Разобранный bzImage.
#[derive(Debug)]
pub struct BzImage<'a> {
    setup_header: &'a [u8],
    payload: &'a [u8],
    /// Версия boot protocol.
    pub protocol: u16,
    /// Сколько памяти ядру нужно от адреса загрузки до завершения распаковки.
    pub init_size: u64,
    /// Требуемое выравнивание адреса загрузки.
    pub alignment: u64,
    /// Ядро может работать с любого адреса.
    pub relocatable: bool,
    /// Предпочтительный адрес загрузки.
    pub pref_address: u64,
    /// Максимальный адрес конца initrd.
    pub initrd_addr_max: u64,
    /// Максимальная длина командной строки (без завершающего нуля).
    pub cmdline_size: usize,
}

fn le16(b: &[u8], off: usize) -> Result<u16> {
    b.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(Error::Kernel("файл слишком короткий"))
}

fn le32(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::Kernel("файл слишком короткий"))
}

fn le64(b: &[u8], off: usize) -> Result<u64> {
    b.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(Error::Kernel("файл слишком короткий"))
}

fn put(b: &mut [u8], off: usize, bytes: &[u8]) -> Result<()> {
    b.get_mut(off..off + bytes.len())
        .ok_or(Error::Kernel("выход за пределы boot_params"))?
        .copy_from_slice(bytes);
    Ok(())
}

impl<'a> BzImage<'a> {
    /// Разбирает bzImage.
    ///
    /// # Errors
    /// Если это не bzImage, протокол старше 2.12 или ядро не поддерживает 64-битную точку входа.
    pub fn parse(image: &'a [u8]) -> Result<Self> {
        if image.get(OFF_HEADER_MAGIC..OFF_HEADER_MAGIC + 4) != Some(b"HdrS".as_slice()) {
            return Err(Error::Kernel("нет сигнатуры HdrS — это не bzImage"));
        }
        let protocol = le16(image, OFF_VERSION)?;
        if protocol < MIN_PROTOCOL {
            return Err(Error::Kernel("boot protocol старше 2.12 не поддерживается"));
        }
        if le16(image, OFF_XLOADFLAGS)? & XLF_KERNEL_64 == 0 {
            return Err(Error::Kernel(
                "ядро не поддерживает 64-битную точку входа (XLF_KERNEL_64)",
            ));
        }
        let setup_sects = match image.get(OFF_SETUP_SECTS).copied() {
            Some(0) => 4,
            Some(n) => usize::from(n),
            None => return Err(Error::Kernel("файл слишком короткий")),
        };
        let payload_offset = (setup_sects + 1) * 512;
        let payload = image
            .get(payload_offset..)
            .filter(|p| !p.is_empty())
            .ok_or(Error::Kernel("нет protected-mode части ядра"))?;
        // Конец setup header: 0x202 + байт смещения короткого jmp по адресу 0x201.
        let header_end = 0x202 + usize::from(image.get(OFF_JUMP + 1).copied().unwrap_or(0));
        let setup_header = image
            .get(HDR_START..header_end.min(payload_offset))
            .ok_or(Error::Kernel("повреждён setup header"))?;

        Ok(Self {
            setup_header,
            payload,
            protocol,
            init_size: u64::from(le32(image, OFF_INIT_SIZE)?),
            alignment: u64::from(le32(image, OFF_KERNEL_ALIGNMENT)?).max(1),
            relocatable: image.get(OFF_RELOCATABLE).copied().unwrap_or(0) != 0,
            pref_address: le64(image, OFF_PREF_ADDRESS)?,
            initrd_addr_max: u64::from(le32(image, OFF_INITRD_ADDR_MAX)?),
            cmdline_size: usize::try_from(le32(image, OFF_CMDLINE_SIZE)?).unwrap_or(usize::MAX),
        })
    }

    /// Адрес, по которому будет загружено ядро.
    #[must_use]
    pub fn load_address(&self) -> u64 {
        if self.relocatable {
            KERNEL_LOAD_ADDR.div_ceil(self.alignment) * self.alignment
        } else {
            self.pref_address
        }
    }
}

/// Результат загрузки: где что лежит и куда прыгать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedKernel {
    /// 64-битная точка входа (`load + 0x200`).
    pub entry: u64,
    /// Адрес boot_params (передаётся в RSI).
    pub boot_params: u64,
    /// Адрес загрузки ядра.
    pub kernel_addr: u64,
    /// Адрес и размер initrd.
    pub initrd: Option<(u64, u64)>,
}

/// Что загрузить.
#[derive(Debug)]
pub struct BootSpec<'a> {
    /// Содержимое bzImage.
    pub kernel: &'a [u8],
    /// Содержимое initrd.
    pub initrd: Option<&'a [u8]>,
    /// Командная строка ядра.
    pub cmdline: &'a str,
    /// Карта памяти.
    pub e820: &'a [E820Entry],
    /// Адрес RSDP (для `boot_params.acpi_rsdp_addr`).
    pub rsdp_addr: u64,
}

/// Загружает ядро, initrd, командную строку и заполняет boot_params.
///
/// # Errors
/// Некорректный образ, слишком длинная командная строка или не хватает памяти.
pub fn load(ram: &GuestRam, spec: &BootSpec<'_>) -> Result<LoadedKernel> {
    let image = BzImage::parse(spec.kernel)?;

    let kernel_addr = image.load_address();
    let payload_len = image.payload.len() as u64;
    let kernel_end = kernel_addr + image.init_size.max(payload_len);
    if kernel_end > ram.low_end() {
        return Err(Error::OutOfGuestMemory("ядро не помещается в нижнюю RAM"));
    }
    ram.write(kernel_addr, image.payload)?;

    // Командная строка.
    let max_cmdline = image.cmdline_size.min(CMDLINE_MAX - 1);
    if spec.cmdline.len() > max_cmdline {
        return Err(Error::Config(format!(
            "командная строка длиннее {max_cmdline} байт"
        )));
    }
    ram.zero(CMDLINE_ADDR, CMDLINE_MAX)?;
    ram.write(CMDLINE_ADDR, spec.cmdline.as_bytes())?;

    // initrd — как можно выше, но ниже initrd_addr_max и конца нижней RAM.
    let initrd = match spec.initrd {
        Some(data) if !data.is_empty() => {
            let size = data.len() as u64;
            let limit = ram.low_end().min(image.initrd_addr_max.saturating_add(1));
            let addr = limit
                .checked_sub(size)
                .map(|a| a & !0xFFF)
                .filter(|&a| a >= kernel_end)
                .ok_or(Error::OutOfGuestMemory(
                    "initrd не помещается между ядром и концом RAM",
                ))?;
            ram.write(addr, data)?;
            Some((addr, size))
        }
        _ => None,
    };

    let mut zp = [0_u8; ZERO_PAGE_SIZE];
    build_boot_params(&mut zp, &image, initrd, spec.e820, spec.rsdp_addr)?;
    ram.write(ZERO_PAGE_ADDR, &zp)?;

    Ok(LoadedKernel {
        entry: kernel_addr + 0x200,
        boot_params: ZERO_PAGE_ADDR,
        kernel_addr,
        initrd,
    })
}

fn build_boot_params(
    zp: &mut [u8; ZERO_PAGE_SIZE],
    image: &BzImage<'_>,
    initrd: Option<(u64, u64)>,
    e820: &[E820Entry],
    rsdp_addr: u64,
) -> Result<()> {
    put(zp, HDR_START, image.setup_header)?;
    put(zp, OFF_VID_MODE, &0xFFFF_u16.to_le_bytes())?;
    put(zp, OFF_TYPE_OF_LOADER, &[0xFF])?;
    let loadflags = zp.get(OFF_LOADFLAGS).copied().unwrap_or(0) | LOADED_HIGH;
    put(zp, OFF_LOADFLAGS, &[loadflags])?;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "поля разбиты на младшие/старшие 32 бита"
    )]
    let split = |v: u64| ((v as u32).to_le_bytes(), ((v >> 32) as u32).to_le_bytes());

    let (cmd_lo, cmd_hi) = split(CMDLINE_ADDR);
    put(zp, OFF_CMD_LINE_PTR, &cmd_lo)?;
    put(zp, OFF_EXT_CMD_LINE_PTR, &cmd_hi)?;

    if let Some((addr, size)) = initrd {
        let (a_lo, a_hi) = split(addr);
        let (s_lo, s_hi) = split(size);
        put(zp, OFF_RAMDISK_IMAGE, &a_lo)?;
        put(zp, OFF_EXT_RAMDISK_IMAGE, &a_hi)?;
        put(zp, OFF_RAMDISK_SIZE, &s_lo)?;
        put(zp, OFF_EXT_RAMDISK_SIZE, &s_hi)?;
    }

    if image.protocol >= 0x020E {
        put(zp, OFF_ACPI_RSDP, &rsdp_addr.to_le_bytes())?;
    }

    if e820.len() > E820_MAX {
        return Err(Error::Config("слишком много записей e820".into()));
    }
    for (i, e) in e820.iter().enumerate() {
        let off = OFF_E820_TABLE + i * 20;
        put(zp, off, &e.addr.to_le_bytes())?;
        put(zp, off + 8, &e.size.to_le_bytes())?;
        put(zp, off + 16, &(e.kind as u32).to_le_bytes())?;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "не больше 128 записей, проверено выше"
    )]
    put(zp, OFF_E820_ENTRIES, &[e820.len() as u8])?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::e820_map;
    use crate::memory::AllocOptions;

    const MIB: u64 = 1 << 20;

    /// Минимальный синтетический bzImage: setup header + payload.
    pub(crate) fn fake_bzimage(payload: &[u8]) -> Vec<u8> {
        let setup_sects = 4_usize;
        let mut img = vec![0_u8; (setup_sects + 1) * 512];
        img[OFF_SETUP_SECTS] = setup_sects as u8;
        img[OFF_JUMP] = 0xEB;
        img[OFF_JUMP + 1] = 0x6A; // header до 0x26C
        img[OFF_HEADER_MAGIC..OFF_HEADER_MAGIC + 4].copy_from_slice(b"HdrS");
        img[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&0x020F_u16.to_le_bytes());
        img[OFF_XLOADFLAGS..OFF_XLOADFLAGS + 2].copy_from_slice(&0x3_u16.to_le_bytes());
        img[OFF_INITRD_ADDR_MAX..OFF_INITRD_ADDR_MAX + 4]
            .copy_from_slice(&0x7FFF_FFFF_u32.to_le_bytes());
        img[OFF_KERNEL_ALIGNMENT..OFF_KERNEL_ALIGNMENT + 4]
            .copy_from_slice(&0x20_0000_u32.to_le_bytes());
        img[OFF_RELOCATABLE] = 1;
        img[OFF_CMDLINE_SIZE..OFF_CMDLINE_SIZE + 4].copy_from_slice(&2047_u32.to_le_bytes());
        img[OFF_PREF_ADDRESS..OFF_PREF_ADDRESS + 8].copy_from_slice(&0x100_0000_u64.to_le_bytes());
        img[OFF_INIT_SIZE..OFF_INIT_SIZE + 4].copy_from_slice(&(8 * MIB as u32).to_le_bytes());
        img.extend_from_slice(payload);
        img
    }

    #[test]
    fn rejects_non_bzimage() {
        assert!(BzImage::parse(&[0; 4096]).is_err());
        assert!(BzImage::parse(&[]).is_err());
    }

    #[test]
    fn rejects_old_protocol_and_32bit() {
        let mut img = fake_bzimage(b"x");
        img[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&0x020B_u16.to_le_bytes());
        assert!(BzImage::parse(&img).is_err());
        let mut img = fake_bzimage(b"x");
        img[OFF_XLOADFLAGS..OFF_XLOADFLAGS + 2].copy_from_slice(&0_u16.to_le_bytes());
        assert!(BzImage::parse(&img).is_err());
    }

    #[test]
    fn parses_header() {
        let img = fake_bzimage(b"payload");
        let bz = BzImage::parse(&img).unwrap();
        assert_eq!(bz.protocol, 0x020F);
        assert_eq!(bz.payload, b"payload");
        assert_eq!(bz.load_address(), KERNEL_LOAD_ADDR);
        assert_eq!(bz.cmdline_size, 2047);
        assert_eq!(bz.setup_header.len(), 0x26C - HDR_START);
    }

    #[test]
    fn loads_kernel_initrd_and_boot_params() {
        let ram = GuestRam::new(256 * MIB, AllocOptions::default()).unwrap();
        let img = fake_bzimage(b"KERNEL");
        let initrd = vec![0x5A_u8; 10_000];
        let e820 = e820_map(256 * MIB);
        let spec = BootSpec {
            kernel: &img,
            initrd: Some(&initrd),
            cmdline: "console=ttyS0",
            e820: &e820,
            rsdp_addr: 0xE0000,
        };
        let k = load(&ram, &spec).unwrap();
        assert_eq!(k.entry, KERNEL_LOAD_ADDR + 0x200);

        let mut buf = [0_u8; 6];
        ram.read(KERNEL_LOAD_ADDR, &mut buf).unwrap();
        assert_eq!(&buf, b"KERNEL");

        let (iaddr, isize) = k.initrd.unwrap();
        assert_eq!(isize, 10_000);
        assert_eq!(iaddr % 4096, 0);
        assert!(iaddr + isize <= 256 * MIB);

        let mut zp = [0_u8; ZERO_PAGE_SIZE];
        ram.read(ZERO_PAGE_ADDR, &mut zp).unwrap();
        assert_eq!(&zp[OFF_HEADER_MAGIC..OFF_HEADER_MAGIC + 4], b"HdrS");
        assert_eq!(zp[OFF_TYPE_OF_LOADER], 0xFF);
        assert_eq!(le32(&zp, OFF_CMD_LINE_PTR).unwrap() as u64, CMDLINE_ADDR);
        assert_eq!(le32(&zp, OFF_RAMDISK_IMAGE).unwrap() as u64, iaddr);
        assert_eq!(le32(&zp, OFF_RAMDISK_SIZE).unwrap(), 10_000);
        assert_eq!(le64(&zp, OFF_ACPI_RSDP).unwrap(), 0xE0000);
        assert_eq!(usize::from(zp[OFF_E820_ENTRIES]), e820.len());
        assert_eq!(le32(&zp, OFF_E820_TABLE + 16).unwrap(), 1);

        let mut cmd = [0_u8; 14];
        ram.read(CMDLINE_ADDR, &mut cmd).unwrap();
        assert_eq!(&cmd, b"console=ttyS0\0");
    }

    #[test]
    fn cmdline_too_long_is_error() {
        let ram = GuestRam::new(64 * MIB, AllocOptions::default()).unwrap();
        let img = fake_bzimage(b"K");
        let long = "x".repeat(4096);
        let e820 = e820_map(64 * MIB);
        let spec = BootSpec {
            kernel: &img,
            initrd: None,
            cmdline: &long,
            e820: &e820,
            rsdp_addr: 0,
        };
        assert!(load(&ram, &spec).is_err());
    }

    #[test]
    fn kernel_not_fitting_is_error() {
        let ram = GuestRam::new(16 * MIB, AllocOptions::default()).unwrap();
        let img = fake_bzimage(b"K");
        let e820 = e820_map(16 * MIB);
        let spec = BootSpec {
            kernel: &img,
            initrd: None,
            cmdline: "",
            e820: &e820,
            rsdp_addr: 0,
        };
        assert!(matches!(load(&ram, &spec), Err(Error::OutOfGuestMemory(_))));
    }
}
