//! Аргументы командной строки.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// RustDroid — Android-эмулятор для игр на Windows 11.
#[derive(Debug, Parser)]
#[command(version, about)]
pub(crate) struct Cli {
    /// Подробнее логи (-v — debug, -vv — trace). Также переменная RUSTDROID_LOG.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Команды.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Проверить, готова ли система (Windows 11, виртуализация, Windows Hypervisor Platform).
    Check,
    /// Загрузить ядро Linux с initramfs; консоль гостя — в этом терминале.
    Boot(BootArgs),
}

/// Параметры `boot`.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools, reason = "флаги командной строки")]
pub(crate) struct BootArgs {
    /// Образ ядра (bzImage).
    #[arg(long)]
    pub kernel: PathBuf,

    /// initramfs / initrd.
    #[arg(long)]
    pub initrd: Option<PathBuf>,

    /// RAM гостя, МиБ (кратно 2).
    #[arg(long, default_value_t = 2048)]
    pub memory: u64,

    /// Добавить параметры к командной строке ядра.
    #[arg(long, default_value = "")]
    pub append: String,

    /// Полностью заменить командную строку ядра (параметры по умолчанию не добавляются).
    #[arg(long)]
    pub cmdline: Option<String>,

    /// Выделить RAM на больших страницах 2 МиБ (нужна привилегия «Блокировка страниц в памяти»).
    #[arg(long)]
    pub large_pages: bool,

    /// CONFIG_HZ ядра гостя (GKI x86_64 — 250).
    #[arg(long, default_value_t = 250)]
    pub guest_hz: u32,

    /// Дополнительно писать вывод консоли гостя в файл.
    #[arg(long)]
    pub serial_log: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_boot() {
        let cli = Cli::parse_from([
            "rustdroid",
            "boot",
            "--kernel",
            "bzImage",
            "--initrd",
            "init.cpio",
            "--memory",
            "1024",
        ]);
        let Some(Command::Boot(b)) = cli.command else {
            panic!("ожидалась команда boot")
        };
        assert_eq!(b.kernel, PathBuf::from("bzImage"));
        assert_eq!(b.memory, 1024);
        assert_eq!(b.guest_hz, 250);
    }
}
