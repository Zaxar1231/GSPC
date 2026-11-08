//! Запись архива cpio в формате `newc` (формат initramfs ядра Linux).

use std::io::{self, Write};

const MAGIC: &str = "070701";
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFCHR: u32 = 0o020_000;

/// Писатель cpio newc.
pub(crate) struct CpioWriter<W: Write> {
    out: W,
    written: u64,
    inode: u32,
}

impl<W: Write> CpioWriter<W> {
    pub(crate) fn new(out: W) -> Self {
        Self {
            out,
            written: 0,
            inode: 1,
        }
    }

    fn raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn pad4(&mut self) -> io::Result<()> {
        let pad = (4 - (self.written % 4)) % 4;
        #[allow(clippy::cast_possible_truncation, reason = "pad < 4")]
        self.raw(&[0; 3][..pad as usize])
    }

    fn entry(&mut self, name: &str, mode: u32, rdev: (u32, u32), data: &[u8]) -> io::Result<()> {
        let size = u32::try_from(data.len()).map_err(|_| io::Error::other("файл больше 4 ГиБ"))?;
        let namesize =
            u32::try_from(name.len() + 1).map_err(|_| io::Error::other("слишком длинное имя"))?;
        let nlink = if mode & S_IFDIR != 0 { 2 } else { 1 };
        let header = format!(
            "{MAGIC}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
            self.inode, mode, 0, 0, nlink, 0, size, 0, 0, rdev.0, rdev.1, namesize, 0
        );
        self.inode += 1;
        self.raw(header.as_bytes())?;
        self.raw(name.as_bytes())?;
        self.raw(&[0])?;
        self.pad4()?;
        self.raw(data)?;
        self.pad4()
    }

    pub(crate) fn dir(&mut self, name: &str) -> io::Result<()> {
        self.entry(name, S_IFDIR | 0o755, (0, 0), &[])
    }

    pub(crate) fn file(&mut self, name: &str, perm: u32, data: &[u8]) -> io::Result<()> {
        self.entry(name, S_IFREG | perm, (0, 0), data)
    }

    pub(crate) fn symlink(&mut self, name: &str, target: &str) -> io::Result<()> {
        self.entry(name, S_IFLNK | 0o777, (0, 0), target.as_bytes())
    }

    pub(crate) fn char_device(
        &mut self,
        name: &str,
        perm: u32,
        major: u32,
        minor: u32,
    ) -> io::Result<()> {
        self.entry(name, S_IFCHR | perm, (major, minor), &[])
    }

    /// Завершающая запись `TRAILER!!!`.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.entry("TRAILER!!!", 0, (0, 0), &[])?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_aligned_and_terminated() {
        let mut w = CpioWriter::new(Vec::new());
        w.dir("bin").unwrap();
        w.file("init", 0o755, b"#!/bin/sh\n").unwrap();
        w.symlink("bin/sh", "busybox").unwrap();
        w.char_device("dev/console", 0o600, 5, 1).unwrap();
        let out = w.finish().unwrap();
        assert_eq!(out.len() % 4, 0);
        assert!(out.starts_with(b"070701"));
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("TRAILER!!!"));
        assert!(s.contains("#!/bin/sh"));
        // Первая запись: заголовок 110 байт + "bin\0" (4) → данные начинаются на границе 4.
        assert_eq!(&out[110..114], b"bin\0");
    }
}
