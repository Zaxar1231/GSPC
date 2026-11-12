//! Отчёт о падении процесса (Windows): код исключения и адреса в модулях.
//!
//! Падение внутри нативной DLL (gfxstream, драйвер Vulkan) не оставляет в логе ничего, а WER
//! пишет только «модуль unknown, смещение 0», если управление ушло по нулевому указателю. Фильтр
//! необработанных исключений печатает в stderr модуль и смещение инструкции, адрес возврата
//! с вершины стека и адреса из стека, попадающие в загруженные модули, — по ним и PDB
//! (`gfxstream_backend.pdb`, `rustdroid.pdb` из CI) восстанавливается место падения.
//! Потом исключение уходит дальше, в WER, как обычно.

use std::io::Write as _;

use windows::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, HMODULE};
use windows::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Threading::GetCurrentThreadStackLimits;
use windows::core::PCWSTR;

/// Сколько слов стека просматривать.
const STACK_WORDS: usize = 2048;
/// Сколько адресов из стека печатать.
const MAX_FRAMES: usize = 40;
/// Продолжить поиск обработчика (`EXCEPTION_CONTINUE_SEARCH`): дальше — WER.
const CONTINUE_SEARCH: i32 = 0;

/// Ставит фильтр необработанных исключений.
pub(crate) fn install() {
    // SAFETY: фильтр — функция с нужной сигнатурой, живущая всё время работы процесса.
    unsafe { SetUnhandledExceptionFilter(Some(filter)) };
}

/// Модуль, в который попадает адрес, и смещение от его начала.
fn module_of(addr: usize) -> Option<(String, usize)> {
    if addr < 0x1_0000 {
        return None;
    }
    let mut module = HMODULE::default();
    // SAFETY: с FROM_ADDRESS вместо имени передаётся адрес; UNCHANGED_REFCOUNT — без
    // увеличения счётчика ссылок, освобождать не нужно.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &raw mut module,
        )
    }
    .ok()?;
    let mut buf = [0_u16; 260];
    // SAFETY: буфер локальный, размер передаётся срезом.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let path = String::from_utf16_lossy(buf.get(..n)?);
    let name = path.rsplit('\\').next().unwrap_or(&path).to_owned();
    Some((name, addr - module.0 as usize))
}

fn describe(addr: usize) -> String {
    match module_of(addr) {
        Some((m, off)) => format!("0x{addr:016x} {m}+0x{off:x}"),
        None => format!("0x{addr:016x} (вне модулей)"),
    }
}

unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: система передаёт валидный указатель на EXCEPTION_POINTERS (или null).
    let Some(info) = (unsafe { info.as_ref() }) else {
        return CONTINUE_SEARCH;
    };
    // SAFETY: указатели в EXCEPTION_POINTERS валидны на время работы фильтра.
    let record = unsafe { info.ExceptionRecord.as_ref() };
    // SAFETY: как выше.
    let context = unsafe { info.ContextRecord.as_ref() };
    let mut out = String::from("\n=== RustDroid: процесс упал ===\n");
    if let Some(r) = record {
        let code = r.ExceptionCode.0 as u32;
        out.push_str(&format!(
            "исключение 0x{code:08X} по адресу {}\n",
            describe(r.ExceptionAddress as usize)
        ));
        if r.ExceptionCode == EXCEPTION_ACCESS_VIOLATION && r.NumberParameters >= 2 {
            let kind = match r.ExceptionInformation[0] {
                0 => "чтение",
                1 => "запись",
                8 => "исполнение",
                _ => "доступ",
            };
            out.push_str(&format!(
                "нарушение доступа: {kind} 0x{:x}\n",
                r.ExceptionInformation[1]
            ));
        }
    }
    if let Some(c) = context {
        let (rip, rsp) = (c.Rip as usize, c.Rsp as usize);
        out.push_str(&format!("RIP {}\n", describe(rip)));
        let (mut low, mut high) = (0_usize, 0_usize);
        // SAFETY: выходные указатели — локальные переменные.
        unsafe { GetCurrentThreadStackLimits(&raw mut low, &raw mut high) };
        if rsp >= low && rsp + 8 <= high {
            // SAFETY: [rsp, rsp + 8) внутри стека текущего потока (проверено выше).
            let ret = unsafe { (rsp as *const usize).read_unaligned() };
            out.push_str(&format!(
                "[RSP] (адрес возврата, если был вызов) {}\n",
                describe(ret)
            ));
            out.push_str("адреса из стека в модулях:\n");
            let words = ((high - rsp) / 8).min(STACK_WORDS);
            let mut frames = 0;
            for i in 0..words {
                // SAFETY: слово i лежит в [rsp, high) — внутри стека потока.
                let v = unsafe { ((rsp + i * 8) as *const usize).read_unaligned() };
                if let Some((m, off)) = module_of(v) {
                    out.push_str(&format!("  [rsp+0x{:x}] {m}+0x{off:x}\n", i * 8));
                    frames += 1;
                    if frames >= MAX_FRAMES {
                        break;
                    }
                }
            }
        }
    }
    out.push_str("=== конец отчёта ===\n");
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(out.as_bytes());
    let _ = err.flush();
    CONTINUE_SEARCH
}
