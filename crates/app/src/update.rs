//! Обновления и включение WHPX (этап 7): `rustdroid update`, фоновая загрузка, учёт «здоровья»
//! новой версии с откатом, `rustdroid enable-whpx`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustdroid_updater::{Found, Health};

/// Столько версия должна проработать, чтобы считаться «здоровой».
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// `rustdroid update [--check | --rollback]`.
///
/// # Errors
/// Копия не установлена, сеть, подпись фида, пакет.
pub(crate) fn run(check: bool, rollback: bool) -> Result<()> {
    let current = rustdroid_updater::manager(None)
        .context("обновления — только для копии, установленной через Setup.exe")?
        .get_current_version_as_string();
    let um = rustdroid_updater::manager(rollback.then(|| current.clone()))
        .context("обновления — только для копии, установленной через Setup.exe")?;
    let found = if check {
        rustdroid_updater::check(&um, false)
    } else {
        rustdroid_updater::apply(&um)
    }
    .context("обновление не удалось")?;
    match (found, check) {
        (Found::UpToDate, _) if rollback => tracing::info!(current, "предыдущей версии нет"),
        (Found::UpToDate, _) => tracing::info!(current, "установлена последняя версия"),
        (Found::Version(v), true) => tracing::info!(current, available = %v, "есть обновление"),
        (Found::Version(v), false) => {
            tracing::info!(current, version = %v, "ставлю и перезапускаю")
        }
    }
    Ok(())
}

fn health_file() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("RustDroid")
            .join("health.txt"),
    )
}

/// Запуск ВМ у установленной копии: учёт «здоровья», откат после повторных сбоев новой
/// версии, фоновая загрузка обновления. Портативная копия и сборка из исходников — без этого.
pub(crate) fn on_start() {
    let Some(um) = rustdroid_updater::manager(None) else {
        return;
    };
    let current = um.get_current_version_as_string();
    let Some(path) = health_file() else {
        return;
    };
    let old = Health::parse(&std::fs::read_to_string(&path).unwrap_or_default());
    let (health, rollback) = old.on_start(&current);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, health.render());
    if rollback {
        tracing::warn!(
            version = %current,
            starts = health.failed_starts,
            "новая версия раз за разом не запускается — возвращаю предыдущую"
        );
        match rustdroid_updater::manager(Some(current)).map(|m| rustdroid_updater::apply(&m)) {
            Some(Ok(Found::Version(_))) => return,
            Some(Ok(Found::UpToDate)) | None => tracing::warn!("предыдущей версии в фиде нет"),
            Some(Err(e)) => tracing::warn!(error = %e, "откат не удался"),
        }
    }
    let spawned = std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            // Проработали минуту — версия «здорова».
            std::thread::sleep(HEALTHY_AFTER);
            let healthy = Health {
                healthy: true,
                ..health
            };
            let _ = std::fs::write(&path, healthy.render());
            match rustdroid_updater::check(&um, true) {
                Ok(Found::Version(v)) => {
                    tracing::info!(version = %v, "обновление скачано, применится при следующем запуске");
                }
                Ok(Found::UpToDate) => {}
                Err(e) => tracing::debug!(error = %e, "проверка обновлений не удалась"),
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "поток обновлений не запущен");
    }
}

/// `rustdroid enable-whpx`: `dism` с правами администратора (UAC только на этом шаге).
///
/// # Errors
/// Пользователь отказал в правах или `dism` завершился ошибкой.
pub(crate) fn enable_whpx() -> Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::core::w;

    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: w!("dism.exe"),
        lpParameters: w!("/online /enable-feature /featurename:HypervisorPlatform /all /norestart"),
        nShow: 1,
        ..Default::default()
    };
    tracing::info!("включаю Windows Hypervisor Platform (подтвердите запрос прав администратора)");
    // SAFETY: структура заполнена, строки статические.
    unsafe { ShellExecuteExW(&raw mut info) }.context("dism не запущен (отказ в правах?)")?;
    let mut code = 1_u32;
    // SAFETY: процесс запущен с SEE_MASK_NOCLOSEPROCESS — дескриптор наш.
    let _ = unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
    // SAFETY: как выше; выходное значение локальное.
    let got = unsafe { GetExitCodeProcess(info.hProcess, &raw mut code) };
    // SAFETY: дескриптор больше не нужен.
    let _ = unsafe { CloseHandle(info.hProcess) };
    got.context("не удалось узнать итог dism")?;
    match code {
        0 => tracing::info!("Windows Hypervisor Platform включена"),
        3010 => tracing::info!("Windows Hypervisor Platform включена — перезагрузите компьютер"),
        c => bail!("dism завершился с кодом {c}"),
    }
    Ok(())
}
