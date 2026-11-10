//! Аргументы командной строки.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Загрузить ядро Linux; консоль гостя — в этом терминале.
    Boot(BootArgs),
    /// Загрузить Android из каталога образов Cuttlefish x86_64.
    BootAndroid(AndroidArgs),
}

/// Куда направлять ввод с клавиатуры.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ConsoleKind {
    /// COM1 (ttyS0).
    Serial,
    /// virtio-console (hvc0).
    Hvc,
}

/// Общие параметры ВМ.
#[derive(Debug, Args)]
pub(crate) struct VmArgs {
    /// RAM гостя, МиБ (кратно 2).
    #[arg(long, default_value_t = 2048)]
    pub memory: u64,

    /// Число vCPU.
    #[arg(long, default_value_t = 1)]
    pub cpus: u32,

    /// Добавить параметры к командной строке ядра.
    #[arg(long, default_value = "")]
    pub append: String,

    /// Выделить RAM на больших страницах 2 МиБ (нужна привилегия «Блокировка страниц в памяти»).
    #[arg(long)]
    pub large_pages: bool,

    /// CONFIG_HZ ядра гостя (GKI x86_64 — 250).
    #[arg(long, default_value_t = 250)]
    pub guest_hz: u32,

    /// Дополнительно писать вывод консоли гостя в файл.
    #[arg(long)]
    pub serial_log: Option<PathBuf>,

    /// Подключить virtio-console и направить в неё ввод (`hvc`) или оставить COM1 (`serial`).
    /// Для `boot-android` не действует: консоль Android всегда `hvc0`.
    #[arg(long, value_enum, default_value_t = ConsoleKind::Serial)]
    pub console: ConsoleKind,

    /// Окно с экраном гостя: virtio-gpu + virtio-input, `ШИРИНАxВЫСОТА@ГЦ` (например 1920x1080@120).
    #[arg(long)]
    pub display: Option<String>,

    /// Выводить без VSync (tearing; для VRR/G-Sync/FreeSync).
    #[arg(long)]
    pub no_vsync: bool,

    /// Лимит FPS композитора (0 — без лимита).
    #[arg(long, default_value_t = 0)]
    pub fps_limit: u32,

    /// Не показывать оверлей производительности.
    #[arg(long)]
    pub no_overlay: bool,

    /// Остановить ВМ через N секунд (0 — не останавливать). Для автоматических проверок.
    #[arg(long, default_value_t = 0)]
    pub timeout: u64,
}

/// Разобранный режим `--display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct DisplaySpec {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl DisplaySpec {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        let (size, hz) = s.split_once('@').unwrap_or((s, "60"));
        let (w, h) = size
            .split_once(['x', 'X'])
            .ok_or("ожидается ШИРИНАxВЫСОТА[@ГЦ]")?;
        let num = |v: &str, what: &str| {
            v.trim()
                .parse::<u32>()
                .map_err(|_| format!("неверное значение {what}: {v}"))
        };
        let spec = Self {
            width: num(w, "ширины")?,
            height: num(h, "высоты")?,
            refresh_hz: num(hz, "частоты")?,
        };
        if !(320..=7680).contains(&spec.width) || !(240..=4320).contains(&spec.height) {
            return Err("разрешение вне диапазона 320x240..7680x4320".into());
        }
        if !(24..=360).contains(&spec.refresh_hz) {
            return Err("частота вне диапазона 24..360 Гц".into());
        }
        Ok(spec)
    }
}

/// Параметры `boot`.
#[derive(Debug, Args)]
pub(crate) struct BootArgs {
    /// Образ ядра (bzImage).
    #[arg(long)]
    pub kernel: PathBuf,

    /// initramfs / initrd.
    #[arg(long)]
    pub initrd: Option<PathBuf>,

    /// Диск virtio-blk: `ПУТЬ`, `ПУТЬ,ro` или `ПУТЬ,cow=ОВЕРЛЕЙ`. Можно указать несколько раз.
    #[arg(long = "disk")]
    pub disks: Vec<String>,

    /// Полностью заменить командную строку ядра (параметры по умолчанию не добавляются).
    #[arg(long)]
    pub cmdline: Option<String>,

    #[command(flatten)]
    pub vm: VmArgs,
}

/// Параметры `boot-android`.
#[derive(Debug, Args)]
pub(crate) struct AndroidArgs {
    /// Каталог с образами (boot.img, init_boot.img, vendor_boot.img, super.img, userdata.img, vbmeta*.img).
    #[arg(long)]
    pub images: PathBuf,

    /// Ядро вместо ядра из boot.img.
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Имя инстанса (каталог copy-on-write оверлеев в %LOCALAPPDATA%\RustDroid\instances).
    #[arg(long, default_value = "default")]
    pub instance: String,

    /// Сбросить инстанс к исходным образам (удалить оверлеи) перед запуском.
    #[arg(long)]
    pub reset: bool,

    /// Параметр bootconfig `КЛЮЧ=ЗНАЧЕНИЕ` поверх параметров по умолчанию (можно несколько).
    #[arg(long = "set")]
    pub bootconfig: Vec<String>,

    /// Куда писать logcat гостя (по умолчанию logcat.txt в каталоге инстанса).
    #[arg(long)]
    pub logcat: Option<PathBuf>,

    #[command(flatten)]
    pub vm: VmArgs,
}

/// Разобранная спецификация `--disk`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct DiskSpec {
    pub path: PathBuf,
    pub read_only: bool,
    pub overlay: Option<PathBuf>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl DiskSpec {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        let mut parts = s.split(',');
        let path = parts
            .next()
            .filter(|p| !p.is_empty())
            .ok_or("пустой путь диска")?;
        let mut spec = Self {
            path: PathBuf::from(path),
            read_only: false,
            overlay: None,
        };
        for opt in parts {
            match opt.split_once('=') {
                None if opt == "ro" => spec.read_only = true,
                Some(("cow", p)) if !p.is_empty() => spec.overlay = Some(PathBuf::from(p)),
                _ => return Err(format!("неизвестный параметр диска: {opt}")),
            }
        }
        Ok(spec)
    }
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
            "--cpus",
            "4",
            "--disk",
            "a.img,ro",
            "--disk",
            "b.img,cow=b.rdcow",
            "--console",
            "hvc",
        ]);
        let Some(Command::Boot(b)) = cli.command else {
            panic!("ожидалась команда boot")
        };
        assert_eq!(b.kernel, PathBuf::from("bzImage"));
        assert_eq!(b.vm.memory, 1024);
        assert_eq!(b.vm.cpus, 4);
        assert_eq!(b.vm.console, ConsoleKind::Hvc);
        assert_eq!(b.disks.len(), 2);
    }

    #[test]
    fn parses_boot_android() {
        let cli = Cli::parse_from(["rustdroid", "boot-android", "--images", "cf", "--reset"]);
        let Some(Command::BootAndroid(a)) = cli.command else {
            panic!("ожидалась команда boot-android")
        };
        assert_eq!(a.images, PathBuf::from("cf"));
        assert_eq!(a.instance, "default");
        assert!(a.reset);
    }

    #[test]
    fn display_spec() {
        assert_eq!(
            DisplaySpec::parse("1920x1080@144").unwrap(),
            DisplaySpec {
                width: 1920,
                height: 1080,
                refresh_hz: 144
            }
        );
        assert_eq!(DisplaySpec::parse("1280X720").unwrap().refresh_hz, 60);
        assert!(DisplaySpec::parse("1920").is_err());
        assert!(DisplaySpec::parse("10x10").is_err());
        assert!(DisplaySpec::parse("1920x1080@1000").is_err());
    }

    #[test]
    fn disk_spec() {
        assert_eq!(
            DiskSpec::parse("x.img,cow=x.rdcow").unwrap(),
            DiskSpec {
                path: "x.img".into(),
                read_only: false,
                overlay: Some("x.rdcow".into())
            }
        );
        assert!(DiskSpec::parse("x.img,ro").unwrap().read_only);
        assert!(DiskSpec::parse("x.img,bogus").is_err());
        assert!(DiskSpec::parse("").is_err());
    }
}
