//! RustDroid — точка входа приложения.
//!
//! Консольные команды (этапы 1–2):
//! - `rustdroid check` — проверка, что машина готова запускать ВМ;
//! - `rustdroid boot --kernel bzImage [--initrd …] [--disk …] [--cpus N]` — загрузка ядра Linux;
//! - `rustdroid boot-android --images <каталог Cuttlefish>` — загрузка Android до шелла.
//!
//! Консоль гостя выводится в терминал, ввод с клавиатуры уходит в гостя. Выход: `Ctrl+A`, `X`.
//! С `--display` открывается окно с экраном гостя (virtio-gpu 2D, D3D12) и вводом мышью и
//! клавиатурой. UI в стиле Material Design 3 — на этапе 4.

mod cli;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod gpu_probe;
#[cfg(windows)]
mod preflight;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

/// Просит драйвер NVIDIA Optimus запускать RustDroid на дискретной видеокарте.
#[cfg(windows)]
#[unsafe(no_mangle)]
#[used]
#[allow(non_upper_case_globals, reason = "имя задано драйвером NVIDIA")]
pub static NvOptimusEnablement: u32 = 1;

/// То же для AMD PowerXpress / Switchable Graphics.
#[cfg(windows)]
#[unsafe(no_mangle)]
#[used]
#[allow(non_upper_case_globals, reason = "имя задано драйвером AMD")]
pub static AmdPowerXpressRequestHighPerformance: i32 = 1;

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "RustDroid запускается");
    run(cli.command.unwrap_or(Command::Check), cli.verbose)
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
fn run(command: Command, verbose: u8) -> Result<()> {
    match command {
        Command::Check => preflight::run(verbose),
        Command::GpuProbe => gpu_probe::run(),
        Command::Boot(args) => console::boot(&args),
        Command::BootAndroid(args) => console::boot_android(&args),
    }
}

#[cfg(not(windows))]
fn run(_command: Command, _verbose: u8) -> Result<()> {
    anyhow::bail!("RustDroid поддерживает только Windows 11 x64")
}
