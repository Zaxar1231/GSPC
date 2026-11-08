//! RustDroid — точка входа приложения.
//!
//! Этап 1: консольные команды
//! - `rustdroid check` — проверка, что машина готова запускать ВМ;
//! - `rustdroid boot --kernel bzImage [--initrd initramfs]` — загрузка ядра Linux, консоль гостя
//!   (COM1) выводится в терминал, ввод с клавиатуры уходит в гостя. Выход: `Ctrl+A`, затем `X`.
//!
//! Окно, UI (Material Design 3) и запуск Android появятся на этапах 2–3.

mod cli;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod preflight;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "RustDroid запускается");
    run(cli.command.unwrap_or(Command::Check))
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_env("RUSTDROID_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    // Логи — в stderr, чтобы не смешиваться с консолью гостя в stdout.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(windows)]
fn run(command: Command) -> Result<()> {
    match command {
        Command::Check => preflight::run(),
        Command::Boot(args) => console::boot(&args),
    }
}

#[cfg(not(windows))]
fn run(_command: Command) -> Result<()> {
    anyhow::bail!("RustDroid поддерживает только Windows 11 x64")
}
