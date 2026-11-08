//! Счётчики Windows для оверлея: время CPU процесса и системы, рабочий набор процесса.

use std::time::Instant;

use rustdroid_telemetry::usage::{Usage, UsageSample};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};

fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Измеритель загрузки: раз в интервал сравнивает снимки счётчиков.
#[derive(Debug)]
pub struct UsageMeter {
    started: Instant,
    cpus: u32,
    prev: Option<UsageSample>,
    last: Usage,
}

impl Default for UsageMeter {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            cpus: std::thread::available_parallelism()
                .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX)),
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
                self.last = Usage::between(prev, &now, self.cpus);
            }
            self.prev = Some(now);
        }
        self.last
    }

    fn sample(&self) -> Option<UsageSample> {
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
        })
    }
}
