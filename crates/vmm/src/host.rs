//! Настройка потоков хоста: производительные ядра (CPU Sets) и MMCSS.
//!
//! На гибридных CPU (Intel P/E-ядра) vCPU-потоки привязываются только к ядрам с максимальным
//! `EfficiencyClass`. На обычных CPU привязка не делается, планировщик Windows справляется сам.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::SystemInformation::{
    CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
    SetThreadSelectedCpuSets,
};
use windows::core::PCWSTR;

/// CPU Set ядра хоста.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSet {
    /// Идентификатор CPU Set.
    pub id: u32,
    /// Класс эффективности (больше — производительнее).
    pub efficiency_class: u8,
}

/// Все CPU Sets системы.
#[must_use]
pub fn cpu_sets() -> Vec<CpuSet> {
    let mut needed = 0_u32;
    // SAFETY: запрос размера: буфер не передаётся, выходной указатель локальный.
    let _ = unsafe { GetSystemCpuSetInformation(None, 0, &raw mut needed, None, None) };
    if needed == 0 {
        return Vec::new();
    }
    // Буфер из u64 гарантирует выравнивание структур.
    let mut buf = vec![0_u64; (needed as usize).div_ceil(8)];
    let mut returned = 0_u32;
    // SAFETY: буфер размером не меньше `needed` байт, выходной указатель локальный.
    let ok = unsafe {
        GetSystemCpuSetInformation(
            Some(buf.as_mut_ptr().cast::<SYSTEM_CPU_SET_INFORMATION>()),
            needed,
            &raw mut returned,
            None,
            None,
        )
    };
    if !ok.as_bool() {
        return Vec::new();
    }
    let bytes = buf.as_ptr().cast::<u8>();
    let total = returned as usize;
    let mut sets = Vec::new();
    let mut off = 0_usize;
    while off + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= total {
        // SAFETY: off + размер записи не выходит за заполненную часть буфера (условие цикла).
        let entry = unsafe { bytes.add(off) }.cast::<SYSTEM_CPU_SET_INFORMATION>();
        // SAFETY: запись целиком лежит в буфере; read_unaligned не требует выравнивания.
        let info = unsafe { entry.read_unaligned() };
        if info.Size == 0 {
            break;
        }
        if info.Type == CpuSetInformation {
            // SAFETY: для типа CpuSetInformation активен вариант CpuSet.
            let cs = unsafe { info.Anonymous.CpuSet };
            sets.push(CpuSet {
                id: cs.Id,
                efficiency_class: cs.EfficiencyClass,
            });
        }
        off += info.Size as usize;
    }
    sets
}

/// CPU Sets производительных ядер. Пусто, если CPU не гибридный (все ядра одного класса).
#[must_use]
pub fn performance_cpu_sets() -> Vec<u32> {
    select_performance(&cpu_sets())
}

fn select_performance(sets: &[CpuSet]) -> Vec<u32> {
    let Some(max) = sets.iter().map(|s| s.efficiency_class).max() else {
        return Vec::new();
    };
    let min = sets.iter().map(|s| s.efficiency_class).min().unwrap_or(max);
    if max == min {
        return Vec::new();
    }
    sets.iter()
        .filter(|s| s.efficiency_class == max)
        .map(|s| s.id)
        .collect()
}

/// Результат настройки потока. При удалении снимает регистрацию MMCSS.
#[derive(Debug)]
pub struct ThreadTuning {
    mmcss: Option<HANDLE>,
    /// Число CPU Sets, к которым привязан поток (0 — без привязки).
    pub pinned: usize,
}

impl Drop for ThreadTuning {
    fn drop(&mut self) {
        if let Some(h) = self.mmcss.take() {
            // SAFETY: дескриптор получен из AvSetMmThreadCharacteristicsW в этом же потоке.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// Настраивает текущий поток: MMCSS-задача (`"Games"`, `"Pro Audio"`; `None` — без MMCSS) и, на
/// гибридных CPU, привязка к производительным ядрам. Ошибки не фатальны и только пишутся в лог.
///
/// vCPU-потокам MMCSS не даётся: гость, ожидающий ответа gfxstream, крутится на vCPU, и
/// поднятый приоритет vCPU отнимает процессор у потоков gfxstream и драйвера видеокарты.
#[must_use]
pub fn tune_current_thread(mmcss_task: Option<&str>) -> ThreadTuning {
    let mmcss = mmcss_task.and_then(|name| {
        let task: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut index = 0_u32;
        // SAFETY: имя задачи — нуль-терминированная UTF-16 строка, живущая до конца вызова.
        match unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(task.as_ptr()), &raw mut index) } {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!(task = name, error = %e, "MMCSS недоступен");
                None
            }
        }
    });

    let perf = performance_cpu_sets();
    let mut pinned = 0;
    if !perf.is_empty() {
        // SAFETY: возвращает псевдо-дескриптор текущего потока, закрывать не нужно.
        let thread = unsafe { GetCurrentThread() };
        // SAFETY: дескриптор потока валиден, срез идентификаторов живёт до конца вызова.
        let ok = unsafe { SetThreadSelectedCpuSets(thread, &perf) };
        if ok.as_bool() {
            pinned = perf.len();
        } else {
            tracing::warn!("не удалось привязать поток к производительным ядрам");
        }
    }
    tracing::debug!(task = ?mmcss_task, pinned, "поток настроен");
    ThreadTuning { mmcss, pinned }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_selects_top_class() {
        let sets = [
            CpuSet {
                id: 256,
                efficiency_class: 1,
            },
            CpuSet {
                id: 257,
                efficiency_class: 1,
            },
            CpuSet {
                id: 258,
                efficiency_class: 0,
            },
        ];
        assert_eq!(select_performance(&sets), vec![256, 257]);
    }

    #[test]
    fn homogeneous_cpu_is_not_pinned() {
        let sets = [
            CpuSet {
                id: 1,
                efficiency_class: 0,
            },
            CpuSet {
                id: 2,
                efficiency_class: 0,
            },
        ];
        assert!(select_performance(&sets).is_empty());
        assert!(select_performance(&[]).is_empty());
    }
}
