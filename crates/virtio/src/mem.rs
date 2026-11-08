//! Доступ устройств к гостевой памяти.

/// Обращение за пределы гостевой RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("гостевой адрес 0x{gpa:x} (+{len}) вне RAM")]
pub struct MemoryError {
    /// Адрес.
    pub gpa: u64,
    /// Длина.
    pub len: usize,
}

/// Гостевая память с точки зрения устройства: копирование по GPA.
///
/// Реализация — `rustdroid_vmm::memory::GuestRam`; в тестах — вектор.
pub trait GuestMemory: Send + Sync {
    /// Читает `buf.len()` байт с адреса `gpa`.
    ///
    /// # Errors
    /// Если диапазон вне RAM.
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), MemoryError>;

    /// Записывает `buf` по адресу `gpa`.
    ///
    /// # Errors
    /// Если диапазон вне RAM.
    fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), MemoryError>;
}

/// Чтение и запись чисел little-endian поверх [`GuestMemory`].
pub trait GuestMemoryExt: GuestMemory {
    /// `u16`.
    ///
    /// # Errors
    /// Если адрес вне RAM.
    fn read_u16(&self, gpa: u64) -> Result<u16, MemoryError> {
        let mut b = [0; 2];
        self.read(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    /// `u32`.
    ///
    /// # Errors
    /// Если адрес вне RAM.
    fn read_u32(&self, gpa: u64) -> Result<u32, MemoryError> {
        let mut b = [0; 4];
        self.read(gpa, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// `u64`.
    ///
    /// # Errors
    /// Если адрес вне RAM.
    fn read_u64(&self, gpa: u64) -> Result<u64, MemoryError> {
        let mut b = [0; 8];
        self.read(gpa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Запись `u16`.
    ///
    /// # Errors
    /// Если адрес вне RAM.
    fn write_u16(&self, gpa: u64, v: u16) -> Result<(), MemoryError> {
        self.write(gpa, &v.to_le_bytes())
    }

    /// Запись `u32`.
    ///
    /// # Errors
    /// Если адрес вне RAM.
    fn write_u32(&self, gpa: u64, v: u32) -> Result<(), MemoryError> {
        self.write(gpa, &v.to_le_bytes())
    }
}

impl<T: GuestMemory + ?Sized> GuestMemoryExt for T {}

/// Простая память для тестов: вектор с базовым адресом 0.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug)]
pub struct VecMemory(std::sync::Mutex<Vec<u8>>);

#[cfg(any(test, feature = "test-utils"))]
impl VecMemory {
    /// Память размером `size` байт, заполненная нулями.
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self(std::sync::Mutex::new(vec![0; size]))
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl GuestMemory for VecMemory {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), MemoryError> {
        let m = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = usize::try_from(gpa).map_err(|_| MemoryError {
            gpa,
            len: buf.len(),
        })?;
        let src = m.get(start..start + buf.len()).ok_or(MemoryError {
            gpa,
            len: buf.len(),
        })?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), MemoryError> {
        let mut m = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = usize::try_from(gpa).map_err(|_| MemoryError {
            gpa,
            len: buf.len(),
        })?;
        let dst = m.get_mut(start..start + buf.len()).ok_or(MemoryError {
            gpa,
            len: buf.len(),
        })?;
        dst.copy_from_slice(buf);
        Ok(())
    }
}
