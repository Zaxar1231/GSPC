//! Счётчики Windows для оверлея: время CPU процесса и системы, рабочий набор процесса.

use std::time::Instant;

use rustdroid_telemetry::threads::{GroupLoad, ThreadSample, group_load};
use rustdroid_telemetry::usage::{Usage, UsageSample};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};

fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Источник времени работы vCPU гостя: функция (сумма по vCPU, 100 нс) и число vCPU.
pub type GuestRuntime = (Box<dyn FnMut() -> Option<u64> + Send>, u32);

/// Измеритель загрузки: раз в интервал сравнивает снимки счётчиков.
pub struct UsageMeter {
    started: Instant,
    cpus: u32,
    guest: Option<GuestRuntime>,
    prev: Option<UsageSample>,
    last: Usage,
}

impl std::fmt::Debug for UsageMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageMeter").finish_non_exhaustive()
    }
}

impl UsageMeter {
    /// Измеритель; `guest` — счётчик времени работы vCPU, если есть.
    #[must_use]
    pub fn new(guest: Option<GuestRuntime>) -> Self {
        Self {
            started: Instant::now(),
            cpus: std::thread::available_parallelism()
                .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX)),
            guest,
            prev: None,
            last: Usage::default(),
        }
    }
}

impl UsageMeter {
    /// Новый снимок и загрузка с прошлого вызова (при ошибке API — прошлые значения).
    pub fn update(&mut self) -> Usage {
        if let Some(now) = self.sample() {
            if let Some(prev) = &self.prev {
                let vcpus = self.guest.as_ref().map_or(1, |g| g.1);
                self.last = Usage::between(prev, &now, self.cpus, vcpus);
            }
            self.prev = Some(now);
        }
        self.last
    }

    fn sample(&mut self) -> Option<UsageSample> {
        // SAFETY: псевдо-дескриптор текущего процесса, закрывать не нужно.
        let process = unsafe { GetCurrentProcess() };
        let (mut creation, mut exit, mut kernel, mut user) = Default::default();
        // SAFETY: дескриптор текущего процесса и локальные выходные структуры.
        unsafe {
            GetProcessTimes(
                process,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        }
        .ok()?;
        let (mut idle, mut s_kernel, mut s_user) = Default::default();
        // SAFETY: локальные выходные структуры.
        unsafe {
            GetSystemTimes(
                Some(&raw mut idle),
                Some(&raw mut s_kernel),
                Some(&raw mut s_user),
            )
        }
        .ok()?;
        let mut mem = PROCESS_MEMORY_COUNTERS::default();
        #[allow(clippy::cast_possible_truncation, reason = "размер структуры")]
        let cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: дескриптор текущего процесса, структура нужного размера.
        unsafe { GetProcessMemoryInfo(process, &raw mut mem, cb) }.ok()?;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "100 нс за сотни лет влезут в u64"
        )]
        let wall = (self.started.elapsed().as_nanos() / 100) as u64;
        Some(UsageSample {
            wall,
            process: ticks(kernel) + ticks(user),
            system_idle: ticks(idle),
            system_kernel: ticks(s_kernel),
            system_user: ticks(s_user),
            working_set: mem.WorkingSetSize as u64,
            guest_runtime: self.guest.as_mut().and_then(|g| (g.0)()),
        })
    }
}

/// Загрузка CPU по группам потоков процесса (см. [`rustdroid_telemetry::threads`]).
#[derive(Debug)]
pub struct ThreadMeter {
    started: Instant,
    cpus: u32,
    prev: Option<(u64, Vec<ThreadSample>)>,
}

impl ThreadMeter {
    /// Новый измеритель.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            cpus: std::thread::available_parallelism()
                .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX)),
            prev: None,
        }
    }

    /// Снимок потоков и загрузка групп с прошлого вызова (первый вызов — пусто).
    pub fn update(&mut self) -> Vec<GroupLoad> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "100 нс за сотни лет влезут в u64"
        )]
        let wall = (self.started.elapsed().as_nanos() / 100) as u64;
        let now = thread_samples();
        let loads = match &self.prev {
            Some((w, prev)) => group_load(prev, &now, wall.saturating_sub(*w), self.cpus),
            None => Vec::new(),
        };
        self.prev = Some((wall, now));
        loads
    }
}

impl Default for ThreadMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Потоки текущего процесса: имя и время CPU. Недоступные потоки пропускаются.
fn thread_samples() -> Vec<ThreadSample> {
    use windows::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadTimes, OpenThread,
        THREAD_QUERY_LIMITED_INFORMATION,
    };

    let mut out = Vec::new();
    // SAFETY: снимок всех потоков системы; дескриптор закрывается ниже.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        return out;
    };
    // SAFETY: без параметров.
    let pid = unsafe { GetCurrentProcessId() };
    #[allow(clippy::cast_possible_truncation, reason = "размер структуры")]
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: дескриптор снимка валиден, `entry` с заполненным dwSize.
    let mut more = unsafe { Thread32First(snap, &raw mut entry) }.is_ok();
    while more {
        if entry.th32OwnerProcessID == pid {
            let id = entry.th32ThreadID;
            // SAFETY: поток своего процесса, только чтение сведений; дескриптор закрывается ниже.
            if let Ok(h) = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, id) } {
                let (mut c, mut e, mut k, mut u) = Default::default();
                // SAFETY: дескриптор открыт выше, выходные структуры локальные.
                let times =
                    unsafe { GetThreadTimes(h, &raw mut c, &raw mut e, &raw mut k, &raw mut u) };
                // SAFETY: дескриптор открыт выше; строку выделяет система, освобождается LocalFree.
                let name = match unsafe { GetThreadDescription(h) } {
                    Ok(p) if !p.is_null() => {
                        // SAFETY: система вернула строку с нулём в конце.
                        let s = unsafe { p.to_string() }.unwrap_or_default();
                        // SAFETY: память строки выделена системой через LocalAlloc.
                        let _ = unsafe { LocalFree(Some(HLOCAL(p.0.cast()))) };
                        s
                    }
                    _ => String::new(),
                };
                // SAFETY: дескриптор открыт выше и закрывается один раз.
                let _ = unsafe { CloseHandle(h) };
                if times.is_ok() {
                    out.push(ThreadSample {
                        id,
                        name,
                        cpu: ticks(k) + ticks(u),
                    });
                }
            }
        }
        // SAFETY: как для Thread32First.
        more = unsafe { Thread32Next(snap, &raw mut entry) }.is_ok();
    }
    // SAFETY: дескриптор снимка закрывается один раз.
    let _ = unsafe { CloseHandle(snap) };
    out
}
