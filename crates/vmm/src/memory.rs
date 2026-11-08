//! Гостевая RAM.
//!
//! Вся RAM выделяется одним блоком при старте инстанса и сразу «касается» (prefault), чтобы во
//! время игры не было промахов SLAT на первом обращении. На Windows — `VirtualAlloc`, опционально
//! на больших страницах 2 МиБ (нужна привилегия `SeLockMemoryPrivilege`); в тестах на других ОС —
//! обычная выровненная куча.
//!
//! Решение по `vm-memory`: свой тонкий слой вместо крейта. Нам нужен только плоский блок с
//! несколькими участками GPA, контроль над способом выделения (большие страницы, позже
//! file-backed для снапшотов) и отсутствие лишних зависимостей в горячих путях.

use std::ptr::NonNull;

use crate::error::{Error, Result};
use crate::layout::{RamRange, ram_ranges};

const PAGE_SIZE: usize = 4096;

/// Как выделять память хоста.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AllocOptions {
    /// Попробовать большие страницы (2 МиБ). При неудаче — обычные страницы с предупреждением.
    pub large_pages: bool,
}

/// Участок RAM: GPA и смещение в блоке хоста.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamRegion {
    /// Участок GPA.
    pub range: RamRange,
    /// Смещение в блоке хоста.
    pub offset: usize,
}

/// Гостевая RAM.
pub struct GuestRam {
    backing: Backing,
    regions: Vec<RamRegion>,
}

// SAFETY: блок памяти принадлежит `GuestRam` и освобождается только в Drop. Гость и устройства
// обращаются к нему одновременно так же, как к физической памяти; синхронизация протоколов
// (virtqueue и т. п.) — забота устройств, Rust-ссылок на эту память наружу не выдаётся.
unsafe impl Send for GuestRam {}
// SAFETY: см. выше; методы копируют данные через сырые указатели, не создавая `&mut`.
unsafe impl Sync for GuestRam {}

impl std::fmt::Debug for GuestRam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestRam")
            .field("size", &self.backing.len)
            .field("large_pages", &self.backing.large_pages)
            .field("regions", &self.regions)
            .finish()
    }
}

impl GuestRam {
    /// Выделяет `size` байт RAM (кратно 2 МиБ) и раскладывает по GPA согласно [`ram_ranges`].
    ///
    /// # Errors
    /// Неверный размер или ошибка выделения.
    pub fn new(size: u64, options: AllocOptions) -> Result<Self> {
        const ALIGN: u64 = 2 << 20;
        if size == 0 || !size.is_multiple_of(ALIGN) {
            return Err(Error::Config(format!(
                "размер RAM должен быть кратен 2 МиБ, получено {size}"
            )));
        }
        let len = usize::try_from(size).map_err(|_| Error::HostAllocation { size })?;
        let backing = Backing::allocate(len, options)?;
        let mut offset = 0_usize;
        let regions = ram_ranges(size)
            .into_iter()
            .map(|range| {
                let r = RamRegion { range, offset };
                offset += usize::try_from(range.size).unwrap_or(usize::MAX);
                r
            })
            .collect();
        let ram = Self { backing, regions };
        ram.prefault();
        Ok(ram)
    }

    /// Общий размер.
    #[must_use]
    pub fn size(&self) -> usize {
        self.backing.len
    }

    /// Выделена ли память на больших страницах.
    #[must_use]
    pub fn large_pages(&self) -> bool {
        self.backing.large_pages
    }

    /// Участки RAM.
    #[must_use]
    pub fn regions(&self) -> &[RamRegion] {
        &self.regions
    }

    /// Конец нижнего участка RAM (под MMIO-дырой).
    #[must_use]
    pub fn low_end(&self) -> u64 {
        self.regions.first().map_or(0, |r| r.range.end())
    }

    /// Указатель хоста на начало участка (для `WHvMapGpaRange`).
    #[must_use]
    pub fn host_ptr(&self, region: &RamRegion) -> *mut u8 {
        // SAFETY: смещение участка лежит внутри блока (участки построены из его размера).
        unsafe { self.backing.ptr.as_ptr().add(region.offset) }
    }

    /// Указатель хоста на байт `off` блока. `off` не больше размера блока.
    fn host_at(&self, off: usize) -> *mut u8 {
        debug_assert!(off <= self.backing.len);
        // SAFETY: вызывающие передают смещения внутри блока (или его конец), поэтому
        // результат остаётся в пределах одного выделения.
        unsafe { self.backing.ptr.as_ptr().add(off) }
    }

    /// `true`, если `[gpa, gpa + len)` целиком лежит в RAM.
    #[must_use]
    pub fn contains(&self, gpa: u64, len: usize) -> bool {
        self.translate(gpa, len).is_some()
    }

    fn translate(&self, gpa: u64, len: usize) -> Option<usize> {
        let len64 = u64::try_from(len).ok()?;
        let end = gpa.checked_add(len64)?;
        self.regions.iter().find_map(|r| {
            (gpa >= r.range.gpa && end <= r.range.end())
                .then(|| r.offset + usize::try_from(gpa - r.range.gpa).unwrap_or(usize::MAX))
        })
    }

    /// Копирует данные в гостевую память.
    ///
    /// # Errors
    /// Если диапазон выходит за пределы RAM.
    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let off = self.translate(gpa, data.len()).ok_or(Error::GuestAddress {
            gpa,
            len: data.len(),
        })?;
        let dst = self.host_at(off);
        // SAFETY: `translate` проверил, что [off, off + len) лежит внутри блока; источник —
        // отдельный срез хоста, поэтому области не пересекаются.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
        Ok(())
    }

    /// Читает данные из гостевой памяти.
    ///
    /// # Errors
    /// Если диапазон выходит за пределы RAM.
    pub fn read(&self, gpa: u64, data: &mut [u8]) -> Result<()> {
        let off = self.translate(gpa, data.len()).ok_or(Error::GuestAddress {
            gpa,
            len: data.len(),
        })?;
        let src = self.host_at(off);
        // SAFETY: как в `write`, в обратную сторону.
        unsafe { std::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), data.len()) };
        Ok(())
    }

    /// Записывает `u64` (little-endian).
    ///
    /// # Errors
    /// Если адрес вне RAM.
    pub fn write_u64(&self, gpa: u64, value: u64) -> Result<()> {
        self.write(gpa, &value.to_le_bytes())
    }

    /// Обнуляет диапазон.
    ///
    /// # Errors
    /// Если диапазон выходит за пределы RAM.
    pub fn zero(&self, gpa: u64, len: usize) -> Result<()> {
        let off = self
            .translate(gpa, len)
            .ok_or(Error::GuestAddress { gpa, len })?;
        let dst = self.host_at(off);
        // SAFETY: диапазон проверен `translate`.
        unsafe { std::ptr::write_bytes(dst, 0, len) };
        Ok(())
    }

    /// Касается каждой страницы, чтобы ОС выделила физическую память заранее.
    fn prefault(&self) {
        if self.backing.large_pages {
            // Большие страницы закоммичены и заблокированы сразу при выделении.
            return;
        }
        let mut off = 0;
        while off < self.backing.len {
            let page = self.host_at(off);
            // SAFETY: off < len, запись нуля в уже обнулённую память не меняет содержимое;
            // volatile не даёт компилятору выкинуть «бесполезную» запись.
            unsafe { page.write_volatile(0) };
            off += PAGE_SIZE;
        }
    }
}

/// Блок памяти хоста.
struct Backing {
    ptr: NonNull<u8>,
    len: usize,
    large_pages: bool,
}

#[cfg(windows)]
impl Backing {
    fn allocate(len: usize, options: AllocOptions) -> Result<Self> {
        use windows::Win32::System::Memory::{
            MEM_COMMIT, MEM_LARGE_PAGES, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        };

        if options.large_pages {
            match large_pages::enable_privilege() {
                Ok(()) => {
                    // SAFETY: обычный вызов VirtualAlloc без адреса; результат проверяется.
                    let p = unsafe {
                        VirtualAlloc(
                            None,
                            len,
                            MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
                            PAGE_READWRITE,
                        )
                    };
                    if let Some(ptr) = NonNull::new(p.cast::<u8>()) {
                        tracing::info!(size = len, "RAM гостя выделена на больших страницах");
                        return Ok(Self {
                            ptr,
                            len,
                            large_pages: true,
                        });
                    }
                    tracing::warn!(
                        "большие страницы недоступны (мало непрерывной памяти), используются обычные"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "нет привилегии SeLockMemoryPrivilege, используются обычные страницы")
                }
            }
        }
        // SAFETY: обычный вызов VirtualAlloc без адреса; результат проверяется.
        let p = unsafe { VirtualAlloc(None, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let ptr = NonNull::new(p.cast::<u8>()).ok_or(Error::HostAllocation { size: len as u64 })?;
        Ok(Self {
            ptr,
            len,
            large_pages: false,
        })
    }
}

#[cfg(windows)]
impl Drop for Backing {
    fn drop(&mut self) {
        use windows::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        // SAFETY: указатель получен из VirtualAlloc и освобождается один раз.
        if let Err(e) = unsafe { VirtualFree(self.ptr.as_ptr().cast(), 0, MEM_RELEASE) } {
            tracing::warn!(error = %e, "VirtualFree не удался");
        }
    }
}

#[cfg(windows)]
mod large_pages {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID,
    };
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_LOCK_MEMORY_NAME,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Включает `SeLockMemoryPrivilege` в токене процесса (если она выдана пользователю).
    pub(super) fn enable_privilege() -> windows::core::Result<()> {
        let mut token = HANDLE::default();
        // SAFETY: возвращает псевдо-дескриптор текущего процесса, закрывать не нужно.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: выходной указатель на локальную переменную.
        unsafe {
            OpenProcessToken(
                process,
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &raw mut token,
            )
        }?;
        let mut luid = LUID::default();
        // SAFETY: имя привилегии — статическая строка, выходной указатель локальный.
        let lookup = unsafe { LookupPrivilegeValueW(None, SE_LOCK_MEMORY_NAME, &raw mut luid) };
        let result = lookup.and_then(|()| {
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            // SAFETY: токен открыт с правом ADJUST_PRIVILEGES; структура локальная.
            unsafe {
                AdjustTokenPrivileges(token, false, Some(&raw const privileges), 0, None, None)
            }?;
            // AdjustTokenPrivileges «успешен», даже если привилегия не выдана пользователю.
            // SAFETY: чтение кода последней ошибки текущего потока.
            if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
                return Err(windows::core::Error::from(ERROR_NOT_ALL_ASSIGNED));
            }
            Ok(())
        });
        // SAFETY: дескриптор токена открыт выше и больше не используется.
        let _ = unsafe { CloseHandle(token) };
        result
    }
}

#[cfg(not(windows))]
impl Backing {
    fn allocate(len: usize, _options: AllocOptions) -> Result<Self> {
        let layout = std::alloc::Layout::from_size_align(len, PAGE_SIZE)
            .map_err(|_| Error::HostAllocation { size: len as u64 })?;
        // SAFETY: размер ненулевой (проверено в GuestRam::new), выравнивание — степень двойки.
        let p = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(p).ok_or(Error::HostAllocation { size: len as u64 })?;
        Ok(Self {
            ptr,
            len,
            large_pages: false,
        })
    }
}

#[cfg(not(windows))]
impl Drop for Backing {
    fn drop(&mut self) {
        if let Ok(layout) = std::alloc::Layout::from_size_align(self.len, PAGE_SIZE) {
            // SAFETY: блок выделен `alloc_zeroed` с тем же layout и освобождается один раз.
            unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn rejects_unaligned_size() {
        assert!(GuestRam::new(3 * MIB, AllocOptions::default()).is_err());
        assert!(GuestRam::new(0, AllocOptions::default()).is_err());
    }

    #[test]
    fn read_write_roundtrip_and_bounds() {
        let ram = GuestRam::new(64 * MIB, AllocOptions::default()).unwrap();
        ram.write(0x1000, b"RustDroid").unwrap();
        let mut buf = [0_u8; 9];
        ram.read(0x1000, &mut buf).unwrap();
        assert_eq!(&buf, b"RustDroid");

        assert!(ram.contains(64 * MIB - 8, 8));
        assert!(!ram.contains(64 * MIB - 4, 8));
        assert!(ram.write(64 * MIB, &[1]).is_err());
        assert!(ram.read(u64::MAX - 2, &mut buf).is_err());
        assert_eq!(ram.low_end(), 64 * MIB);
    }

    #[test]
    fn zero_clears_range() {
        let ram = GuestRam::new(4 * MIB, AllocOptions::default()).unwrap();
        ram.write(0x10, &[0xAA; 16]).unwrap();
        ram.zero(0x10, 16).unwrap();
        let mut buf = [1_u8; 16];
        ram.read(0x10, &mut buf).unwrap();
        assert_eq!(buf, [0; 16]);
    }
}
