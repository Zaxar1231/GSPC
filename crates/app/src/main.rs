//! RustDroid — точка входа приложения.
//!
//! Этап 0: инициализация логирования и проверка платформы. Окно, UI и запуск инстанса
//! появятся на этапах 1–3.

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    init_tracing();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "RustDroid запускается");
    run()
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_env("RUSTDROID_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(windows)]
fn run() -> Result<()> {
    use anyhow::Context as _;
    use rustdroid_instance::DataDirs;

    let dirs = DataDirs::from_env().context("не удалось определить каталоги данных")?;
    tracing::info!(
        local = %dirs.local_root().display(),
        roaming = %dirs.roaming_root().display(),
        "каталоги данных"
    );
    tracing::warn!("VMM ещё не реализован (этап 1). Завершение.");
    Ok(())
}

#[cfg(not(windows))]
fn run() -> Result<()> {
    anyhow::bail!("RustDroid поддерживает только Windows 11 x64")
}
