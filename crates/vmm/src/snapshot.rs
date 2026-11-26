//! Файл снапшота ВМ (этап 10): разделы с тегами и контрольными суммами.
//!
//! Формат (всё little-endian):
//!
//! ```text
//! "RDSNAP\0\0"  версия u32
//! раздел*:      тег [u8; 4]  длина u64  crc32 u32  данные
//! конец:        "END\0"      0          0
//! ```
//!
//! Раздел — состояние одного устройства или области памяти. Неизвестные разделы читаются и
//! пропускаются вызывающим (новые версии могут добавлять устройства). Длина из файла не
//! используется для выделения памяти заранее: данные читаются по мере поступления, поэтому
//! испорченная длина даёт ошибку «файл обрезан», а не попытку выделить терабайт.

use std::io::{Read, Write};

use rustdroid_devices::CmosRtc;

use crate::ioapic::IoApic;

/// Сигнатура файла.
pub const MAGIC: [u8; 8] = *b"RDSNAP\0\0";
/// Версия формата.
pub const VERSION: u32 = 1;
/// Тег раздела.
pub type Tag = [u8; 4];
/// Тег конца файла.
const END: Tag = *b"END\0";

/// Ошибка снапшота.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Не снапшот RustDroid.
    #[error("не файл снапшота RustDroid")]
    BadMagic,
    /// Версия формата не поддерживается.
    #[error("версия снапшота {0} не поддерживается (нужна {VERSION})")]
    Version(u32),
    /// Контрольная сумма раздела не совпала.
    #[error("раздел {0}: контрольная сумма не совпала")]
    Crc(String),
    /// Раздел встречается дважды.
    #[error("раздел {0} встречается дважды")]
    Duplicate(String),
    /// Тег зарезервирован форматом.
    #[error("тег {0} зарезервирован")]
    Reserved(String),
    /// Нет нужного раздела.
    #[error("нет раздела {0}")]
    Missing(String),
    /// Данные раздела не подходят устройству.
    #[error("раздел {0}: неверный размер или содержимое")]
    Invalid(String),
    /// Файл закончился раньше конца.
    #[error("файл снапшота обрезан")]
    Truncated,
    /// Ошибка ввода-вывода.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Тег как текст для сообщений.
#[must_use]
pub fn tag_name(tag: Tag) -> String {
    String::from_utf8_lossy(&tag)
        .trim_end_matches(['\0', ' '])
        .to_owned()
}

/// Состояние, которое умеет сохраняться в раздел снапшота.
pub trait Snapshot {
    /// Тег раздела.
    const TAG: Tag;
    /// Сохраняет состояние.
    fn save(&self) -> Vec<u8>;
    /// Восстанавливает состояние.
    ///
    /// # Errors
    /// Данные не подходят.
    fn restore(&mut self, data: &[u8]) -> Result<(), SnapshotError>;
}

impl Snapshot for IoApic {
    const TAG: Tag = *b"IOAP";
    fn save(&self) -> Vec<u8> {
        self.save_state()
    }
    fn restore(&mut self, data: &[u8]) -> Result<(), SnapshotError> {
        self.restore_state(data)
            .then_some(())
            .ok_or_else(|| SnapshotError::Invalid(tag_name(Self::TAG)))
    }
}

impl Snapshot for CmosRtc {
    const TAG: Tag = *b"RTC ";
    fn save(&self) -> Vec<u8> {
        self.save_state().to_vec()
    }
    fn restore(&mut self, data: &[u8]) -> Result<(), SnapshotError> {
        self.restore_state(data)
            .then_some(())
            .ok_or_else(|| SnapshotError::Invalid(tag_name(Self::TAG)))
    }
}

/// Пишет снапшот раздел за разделом.
#[derive(Debug)]
pub struct Writer<W: Write> {
    w: W,
    tags: Vec<Tag>,
}

impl<W: Write> Writer<W> {
    /// Начинает файл: сигнатура и версия.
    ///
    /// # Errors
    /// Ошибка записи.
    pub fn new(mut w: W) -> Result<Self, SnapshotError> {
        w.write_all(&MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        Ok(Self {
            w,
            tags: Vec::new(),
        })
    }

    /// Раздел с данными.
    ///
    /// # Errors
    /// Повтор тега, зарезервированный тег или ошибка записи.
    pub fn section(&mut self, tag: Tag, data: &[u8]) -> Result<(), SnapshotError> {
        if tag == END {
            return Err(SnapshotError::Reserved(tag_name(tag)));
        }
        if self.tags.contains(&tag) {
            return Err(SnapshotError::Duplicate(tag_name(tag)));
        }
        self.tags.push(tag);
        self.w.write_all(&tag)?;
        self.w.write_all(&(data.len() as u64).to_le_bytes())?;
        self.w.write_all(&crc32fast::hash(data).to_le_bytes())?;
        self.w.write_all(data)?;
        Ok(())
    }

    /// Раздел с состоянием устройства.
    ///
    /// # Errors
    /// См. [`Self::section`].
    pub fn save<T: Snapshot>(&mut self, dev: &T) -> Result<(), SnapshotError> {
        self.section(T::TAG, &dev.save())
    }

    /// Завершает файл и возвращает поток.
    ///
    /// # Errors
    /// Ошибка записи.
    pub fn finish(mut self) -> Result<W, SnapshotError> {
        self.w.write_all(&END)?;
        self.w.write_all(&0u64.to_le_bytes())?;
        self.w.write_all(&0u32.to_le_bytes())?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Прочитанные разделы.
#[derive(Debug, Default)]
pub struct Sections(Vec<(Tag, Vec<u8>)>);

impl Sections {
    /// Данные раздела.
    #[must_use]
    pub fn get(&self, tag: Tag) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, d)| d.as_slice())
    }

    /// Теги по порядку файла.
    #[must_use]
    pub fn tags(&self) -> Vec<Tag> {
        self.0.iter().map(|(t, _)| *t).collect()
    }

    /// Восстанавливает устройство из его раздела.
    ///
    /// # Errors
    /// Раздела нет или данные не подходят.
    pub fn restore<T: Snapshot>(&self, dev: &mut T) -> Result<(), SnapshotError> {
        let data = self
            .get(T::TAG)
            .ok_or_else(|| SnapshotError::Missing(tag_name(T::TAG)))?;
        dev.restore(data)
    }
}

fn read_exact_or_truncated(r: &mut impl Read, buf: &mut [u8]) -> Result<(), SnapshotError> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            SnapshotError::Truncated
        } else {
            SnapshotError::Io(e)
        }
    })
}

/// Читает и проверяет снапшот целиком.
///
/// # Errors
/// Не снапшот, другая версия, испорченный или обрезанный файл.
pub fn read(mut r: impl Read) -> Result<Sections, SnapshotError> {
    let mut magic = [0u8; 8];
    read_exact_or_truncated(&mut r, &mut magic)?;
    if magic != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let mut word = [0u8; 4];
    read_exact_or_truncated(&mut r, &mut word)?;
    let version = u32::from_le_bytes(word);
    if version != VERSION {
        return Err(SnapshotError::Version(version));
    }
    let mut out = Sections::default();
    loop {
        let mut tag = [0u8; 4];
        let mut len = [0u8; 8];
        let mut crc = [0u8; 4];
        read_exact_or_truncated(&mut r, &mut tag)?;
        read_exact_or_truncated(&mut r, &mut len)?;
        read_exact_or_truncated(&mut r, &mut crc)?;
        let len = u64::from_le_bytes(len);
        if tag == END {
            return Ok(out);
        }
        let mut data = Vec::new();
        (&mut r).take(len).read_to_end(&mut data)?;
        if data.len() as u64 != len {
            return Err(SnapshotError::Truncated);
        }
        if crc32fast::hash(&data) != u32::from_le_bytes(crc) {
            return Err(SnapshotError::Crc(tag_name(tag)));
        }
        if out.get(tag).is_some() {
            return Err(SnapshotError::Duplicate(tag_name(tag)));
        }
        out.0.push((tag, data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut w = Writer::new(Vec::new()).unwrap();
        w.section(*b"AAAA", b"hello").unwrap();
        w.section(*b"BBBB", &[]).unwrap();
        w.finish().unwrap()
    }

    #[test]
    fn round_trip() {
        let s = read(sample().as_slice()).unwrap();
        assert_eq!(s.tags(), vec![*b"AAAA", *b"BBBB"]);
        assert_eq!(s.get(*b"AAAA"), Some(&b"hello"[..]));
        assert_eq!(s.get(*b"BBBB"), Some(&[][..]));
        assert_eq!(s.get(*b"CCCC"), None);
    }

    #[test]
    fn detects_damage() {
        let good = sample();
        // Испорченный байт данных первого раздела (после заголовка 12 + 16 байт раздела).
        let mut bad = good.clone();
        bad[12 + 16] ^= 1;
        assert!(matches!(read(bad.as_slice()), Err(SnapshotError::Crc(t)) if t == "AAAA"));
        // Обрезанный файл — на любом месте.
        for cut in [3, 10, 20, 30, good.len() - 1] {
            assert!(
                matches!(read(&good[..cut]), Err(SnapshotError::Truncated)),
                "cut {cut}"
            );
        }
        // Чужой файл и другая версия.
        assert!(matches!(
            read(&b"PK\x03\x04garbage"[..]),
            Err(SnapshotError::BadMagic)
        ));
        let mut v2 = good.clone();
        v2[8] = 2;
        assert!(matches!(
            read(v2.as_slice()),
            Err(SnapshotError::Version(2))
        ));
        // Огромная длина в испорченном файле не выделяет память заранее.
        let mut huge = good;
        huge[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            read(huge.as_slice()),
            Err(SnapshotError::Truncated)
        ));
    }

    #[test]
    fn writer_rejects_duplicates_and_reserved() {
        let mut w = Writer::new(Vec::new()).unwrap();
        w.section(*b"AAAA", b"1").unwrap();
        assert!(matches!(
            w.section(*b"AAAA", b"2"),
            Err(SnapshotError::Duplicate(_))
        ));
        assert!(matches!(
            w.section(END, b""),
            Err(SnapshotError::Reserved(_))
        ));
    }

    #[test]
    fn ioapic_and_rtc_survive_snapshot() {
        let mut ioapic = IoApic::new(3);
        // Вход 5: вектор 0x31, без маски.
        ioapic.mmio_write(0x00, 0x10 + 5 * 2);
        ioapic.mmio_write(0x10, 0x31);
        ioapic.mmio_write(0x00, 0x10 + 5 * 2 + 1);
        ioapic.mmio_write(0x10, 0x0100_0000);
        let mut rtc = CmosRtc::with_clock(|| 0);
        rtc.write(0x70, 0x40);
        rtc.write(0x71, 0xAB);

        let mut w = Writer::new(Vec::new()).unwrap();
        w.save(&ioapic).unwrap();
        w.save(&rtc).unwrap();
        let file = w.finish().unwrap();

        let s = read(file.as_slice()).unwrap();
        let mut ioapic2 = IoApic::new(0);
        let mut rtc2 = CmosRtc::with_clock(|| 0);
        s.restore(&mut ioapic2).unwrap();
        s.restore(&mut rtc2).unwrap();
        assert_eq!(ioapic2.save_state(), ioapic.save_state());
        // Регистр, выбранный перед снимком, остаётся выбранным.
        assert_eq!(ioapic2.mmio_read(0x00), ioapic.mmio_read(0x00));
        assert_eq!(ioapic2.mmio_read(0x10), ioapic.mmio_read(0x10));
        assert_eq!(rtc2.save_state(), rtc.save_state());
        rtc2.write(0x70, 0x40);
        assert_eq!(rtc2.read(0x71), 0xAB);

        // Раздела нет — понятная ошибка; неверный размер — тоже.
        let empty = read(
            Writer::new(Vec::new())
                .unwrap()
                .finish()
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert!(
            matches!(empty.restore(&mut ioapic2), Err(SnapshotError::Missing(t)) if t == "IOAP")
        );
        assert!(matches!(
            rtc2.restore(&[1, 2, 3]),
            Err(SnapshotError::Invalid(_))
        ));
    }
}
