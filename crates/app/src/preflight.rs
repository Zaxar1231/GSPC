//! `rustdroid check`: проверки при старте с понятными сообщениями.

use anyhow::{Result, bail};
use rustdroid_whpx::{Error as WhpxError, caps};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED};

/// Первая сборка Windows 11.
const WINDOWS_11_BUILD: u32 = 22000;

/// Результат одной проверки.
#[derive(Debug)]
pub(crate) enum Check {
    Ok(String),
    Fail { problem: String, fix: &'static str },
}

fn windows_build() -> Option<(u32, u32, u32)> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "размер структуры — сотни байт"
    )]
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: структура локальная, поле размера заполнено.
    let status = unsafe { RtlGetVersion(&raw mut info) };
    status
        .is_ok()
        .then_some((info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber))
}

fn check_windows() -> Check {
    match windows_build() {
        Some((10, 0, build)) if build >= WINDOWS_11_BUILD => {
            Check::Ok(format!("Windows 11, сборка {build}"))
        }
        Some((major, minor, build)) => Check::Fail {
            problem: format!("Windows {major}.{minor} (сборка {build}) не поддерживается"),
            fix: "RustDroid работает только на Windows 11 x64. Обновите систему.",
        },
        None => Check::Fail {
            problem: "не удалось определить версию Windows".into(),
            fix: "",
        },
    }
}

fn check_hypervisor() -> Vec<Check> {
    // SAFETY: простой запрос флага процессора.
    let firmware = unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }.as_bool();
    match caps::hypervisor_present() {
        Ok(true) => {
            let mut v = vec![Check::Ok(
                "Windows Hypervisor Platform включена, гипервизор запущен".into(),
            )];
            match (caps::processor_clock_hz(), caps::interrupt_clock_hz()) {
                (Ok(tsc), Ok(apic)) => v.push(Check::Ok(format!(
                    "частота TSC гостя {:.0} МГц, таймера LAPIC {:.0} МГц",
                    mhz(tsc),
                    mhz(apic)
                ))),
                _ => v.push(Check::Fail {
                    problem: "гипервизор не сообщает частоты таймеров".into(),
                    fix: "Обновите Windows 11 до актуальной сборки.",
                }),
            }
            v
        }
        Ok(false) if !firmware => vec![Check::Fail {
            problem: "виртуализация выключена в UEFI/BIOS".into(),
            fix: "Включите Intel Virtualization Technology (VT-x) или SVM Mode (AMD) в настройках UEFI. \
                  Проверка: Диспетчер задач → Производительность → ЦП → «Виртуализация».",
        }],
        Ok(false) => vec![Check::Fail {
            problem: "гипервизор Windows не запущен".into(),
            fix: "Включите компонент «Платформа низкоуровневой оболочки Windows» и перезагрузите компьютер. \
                  Если RustDroid запущен внутри виртуальной машины, включите в ней вложенную виртуализацию.",
        }],
        Err(WhpxError::LibraryNotFound(_)) => vec![Check::Fail {
            problem: "компонент Windows Hypervisor Platform не включён".into(),
            fix: "PowerShell от администратора: Enable-WindowsOptionalFeature -Online -FeatureName \
                  HypervisorPlatform -All, затем перезагрузка.",
        }],
        Err(e) => vec![Check::Fail {
            problem: e.to_string(),
            fix: "",
        }],
    }
}

fn check_cpu() -> Check {
    let sets = rustdroid_vmm::host::cpu_sets();
    let perf = rustdroid_vmm::host::performance_cpu_sets();
    if perf.is_empty() {
        Check::Ok(format!("логических процессоров: {}", sets.len()))
    } else {
        Check::Ok(format!(
            "гибридный CPU: {} логических процессоров, из них производительных {} (vCPU пойдут на них)",
            sets.len(),
            perf.len()
        ))
    }
}

#[allow(clippy::cast_precision_loss, reason = "вывод в МГц")]
fn mhz(hz: u64) -> f64 {
    hz as f64 / 1e6
}

/// Все проверки.
pub(crate) fn checks() -> Vec<Check> {
    let mut v = vec![check_windows()];
    v.extend(check_hypervisor());
    v.push(check_cpu());
    v
}

/// Печатает результаты; ошибка, если хоть одна проверка не прошла.
pub(crate) fn run() -> Result<()> {
    let results = checks();
    let mut failed = 0;
    for c in &results {
        match c {
            Check::Ok(msg) => tracing::info!("[ OK ] {msg}"),
            Check::Fail { problem, fix } => {
                failed += 1;
                tracing::error!("[FAIL] {problem}");
                if !fix.is_empty() {
                    tracing::error!("       Как исправить: {fix}");
                }
            }
        }
    }
    if failed > 0 {
        bail!("система не готова: проблем — {failed}");
    }
    tracing::info!("Система готова к запуску RustDroid");
    Ok(())
}
