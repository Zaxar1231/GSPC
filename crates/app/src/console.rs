//! Запуск ВМ: консоль гостя в терминале Windows и, с `--display`, окно с экраном гостя.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustdroid_devices::{
    CompositeDisk, ConsoleInput, CowDisk, Disk, Partition, RawDisk, Serial16550, SparseDisk,
    ZeroDisk, open_image,
};
use rustdroid_instance::DataDirs;
use rustdroid_instance::android::{
    BootImage, VendorBoot, append_bootconfig, partition_plan, rustdroid_bootconfig,
};
use rustdroid_vmm::{ConsoleHandle, RunOutcome, Stopper, Vm, VmConfig, VmDevices};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
};

use crate::cli::{AndroidArgs, BootArgs, ConsoleKind, DiskSpec, DisplaySpec, VmArgs};
use crate::display;

/// Префикс управляющих комбинаций (как в QEMU): Ctrl+A.
const ESCAPE: u8 = 0x01;

/// Вывод консоли гостя: терминал и, опционально, общий файл лога.
#[derive(Clone)]
struct ConsoleOut {
    log: Option<Arc<Mutex<File>>>,
}

impl Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut out = std::io::stdout().lock();
        out.write_all(buf)?;
        out.flush()?;
        if let Some(f) = &self.log {
            f.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = &self.log {
            f.lock().unwrap_or_else(PoisonError::into_inner).flush()?;
        }
        std::io::stdout().flush()
    }
}

/// Сырой режим консоли на время работы ВМ; при удалении режимы восстанавливаются.
struct RawConsole {
    input: Option<(HANDLE, CONSOLE_MODE)>,
    output: Option<(HANDLE, CONSOLE_MODE)>,
}

impl RawConsole {
    fn enable() -> Self {
        let input = switch_mode(STD_INPUT_HANDLE, |m| {
            (m & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT
        });
        let output = switch_mode(STD_OUTPUT_HANDLE, |m| {
            m | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        });
        Self { input, output }
    }
}

fn switch_mode(
    which: windows::Win32::System::Console::STD_HANDLE,
    f: impl FnOnce(CONSOLE_MODE) -> CONSOLE_MODE,
) -> Option<(HANDLE, CONSOLE_MODE)> {
    // SAFETY: получение стандартного дескриптора процесса.
    let handle = unsafe { GetStdHandle(which) }.ok()?;
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: дескриптор и выходной указатель валидны; если это не консоль (перенаправление
    // в файл), вызов вернёт ошибку и режим не меняется.
    unsafe { GetConsoleMode(handle, &raw mut mode) }.ok()?;
    // SAFETY: дескриптор консоли валиден.
    unsafe { SetConsoleMode(handle, f(mode)) }.ok()?;
    Some((handle, mode))
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        for (handle, mode) in [self.input.take(), self.output.take()]
            .into_iter()
            .flatten()
        {
            // SAFETY: восстанавливаем исходный режим того же дескриптора.
            let _ = unsafe { SetConsoleMode(handle, mode) };
        }
    }
}

/// Куда отдавать ввод с клавиатуры.
enum InputTarget {
    Serial(Arc<Mutex<Serial16550>>),
    Hvc(ConsoleHandle),
}

impl InputTarget {
    fn send(&self, mut bytes: &[u8]) {
        match self {
            Self::Hvc(h) => h.push(bytes),
            Self::Serial(serial) => {
                while !bytes.is_empty() {
                    let n = serial
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .enqueue_input(bytes);
                    bytes = bytes.get(n..).unwrap_or(&[]);
                    if !bytes.is_empty() {
                        // FIFO гостя заполнен — ждём, пока драйвер его вычитает.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
    }
}

/// Поток ввода: клавиатура → гость. `Ctrl+A X` — остановить ВМ, `Ctrl+A Ctrl+A` — передать Ctrl+A.
fn spawn_input(target: InputTarget, stopper: Stopper) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0_u8; 256];
            let mut escape = false;
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let mut pending = Vec::with_capacity(n);
                for &b in buf.get(..n).unwrap_or(&[]) {
                    if escape {
                        escape = false;
                        match b {
                            b'x' | b'X' => {
                                stopper.stop();
                                return;
                            }
                            ESCAPE => pending.push(ESCAPE),
                            _ => {}
                        }
                    } else if b == ESCAPE {
                        escape = true;
                    } else {
                        pending.push(b);
                    }
                }
                target.send(&pending);
            }
        })?;
    Ok(())
}

/// Останавливает ВМ через `secs` секунд (`--timeout`).
fn spawn_timeout(secs: u64, stopper: Stopper) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("vm-timeout".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            tracing::info!(secs, "истёк --timeout, ВМ останавливается");
            stopper.stop();
        })?;
    Ok(())
}

fn open_log(path: Option<&PathBuf>) -> Result<ConsoleOut> {
    let log = path
        .map(|p| File::create(p).with_context(|| format!("не удалось создать {}", p.display())))
        .transpose()?
        .map(|f| Arc::new(Mutex::new(f)));
    Ok(ConsoleOut { log })
}

fn base_config(vm: &VmArgs, console: ConsoleKind, kernel: PathBuf) -> VmConfig {
    VmConfig {
        kernel,
        initrd: None,
        initrd_data: None,
        memory_mib: vm.memory,
        cpus: vm.cpus,
        cmdline_extra: vm.append.clone(),
        cmdline_override: None,
        large_pages: vm.large_pages,
        guest_hz: vm.guest_hz,
        kernel_console: match console {
            ConsoleKind::Serial => "ttyS0".into(),
            ConsoleKind::Hvc => "hvc0".into(),
        },
    }
}

fn run_vm(cfg: &VmConfig, mut devices: VmDevices, vm: &VmArgs, console: ConsoleKind) -> Result<()> {
    let out = open_log(vm.serial_log.as_ref())?;
    if console == ConsoleKind::Hvc {
        devices.console = Some((Box::new(out.clone()), Arc::new(ConsoleInput::default())));
    }
    let display = vm
        .display
        .as_deref()
        .map(DisplaySpec::parse)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let screen = display.map(|d| display::attach(&mut devices, d));

    let machine = Vm::new(cfg, devices, Box::new(out)).context("не удалось создать ВМ")?;
    tracing::info!(cmdline = machine.cmdline(), "командная строка ядра");
    tracing::info!("Консоль гостя ниже. Выход: Ctrl+A, затем X (или закройте окно).");

    let target = match (console, machine.console()) {
        (ConsoleKind::Hvc, Some(h)) => InputTarget::Hvc(h),
        _ => InputTarget::Serial(machine.serial()),
    };
    let raw = RawConsole::enable();
    spawn_input(target, machine.stopper()).context("не удалось запустить поток ввода")?;
    if vm.timeout > 0 {
        spawn_timeout(vm.timeout, machine.stopper()).context("не удалось запустить таймер")?;
    }
    let result = match screen {
        Some(screen) => display::run(machine, screen, vm),
        None => machine.run().map_err(anyhow::Error::from),
    };
    drop(raw);
    let _ = writeln!(std::io::stdout());
    let (outcome, stats) = result.context("ошибка выполнения ВМ")?;
    match outcome {
        RunOutcome::Reset => {
            tracing::info!(exits = stats.total(), "гость перезагрузился или выключился")
        }
        RunOutcome::Stopped => {
            tracing::info!(exits = stats.total(), "ВМ остановлена пользователем")
        }
    }
    Ok(())
}

fn open_disk(spec: &DiskSpec) -> Result<Arc<dyn Disk>> {
    let read_only = spec.read_only || spec.overlay.is_some();
    let open_err = || format!("не удалось открыть {}", spec.path.display());
    // Sparse-образ читается только через индекс; писать в него можно через `cow=`.
    let base: Box<dyn Disk> = if SparseDisk::is_sparse(&spec.path).with_context(open_err)? {
        Box::new(SparseDisk::open(&spec.path).with_context(open_err)?)
    } else {
        Box::new(RawDisk::open(&spec.path, read_only).with_context(open_err)?)
    };
    Ok(match &spec.overlay {
        Some(o) => {
            Arc::new(CowDisk::open(base, o).with_context(|| format!("оверлей {}", o.display()))?)
        }
        None => Arc::from(base),
    })
}

/// `rustdroid boot`.
pub(crate) fn boot(args: &BootArgs) -> Result<()> {
    let mut cfg = base_config(&args.vm, args.vm.console, args.kernel.clone());
    cfg.initrd.clone_from(&args.initrd);
    cfg.cmdline_override.clone_from(&args.cmdline);
    let mut devices = VmDevices::default();
    for (i, s) in args.disks.iter().enumerate() {
        let spec = DiskSpec::parse(s).map_err(anyhow::Error::msg)?;
        devices.disks.push((
            format!("disk{i}:{}", spec.path.display()),
            open_disk(&spec)?,
        ));
    }
    run_vm(&cfg, devices, &args.vm, args.vm.console)
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("не удалось прочитать {}", path.display()))
}

/// `rustdroid boot-android`.
pub(crate) fn boot_android(args: &AndroidArgs) -> Result<()> {
    let dir = &args.images;
    let boot = read(&dir.join("boot.img"))?;
    let vendor = read(&dir.join("vendor_boot.img"))?;
    let init_boot = dir
        .join("init_boot.img")
        .is_file()
        .then(|| read(&dir.join("init_boot.img")))
        .transpose()?;

    let boot_img = BootImage::parse(&boot, "boot.img")?;
    let vendor_img = VendorBoot::parse(&vendor)?;
    let generic_ramdisk = match &init_boot {
        Some(b) => BootImage::parse(b, "init_boot.img")?.ramdisk,
        None => boot_img.ramdisk,
    };

    // Ядро: из boot.img или отдельный файл.
    let dirs = DataDirs::from_env()?;
    let instance_dir = dirs.instances().join(&args.instance);
    let overlays = instance_dir.join("overlays");
    if args.reset && overlays.exists() {
        std::fs::remove_dir_all(&overlays)
            .with_context(|| format!("не удалось очистить {}", overlays.display()))?;
        tracing::info!(instance = %args.instance, "инстанс сброшен к исходным образам");
    }
    std::fs::create_dir_all(&overlays)
        .with_context(|| format!("не удалось создать {}", overlays.display()))?;
    let kernel = match &args.kernel {
        Some(k) => k.clone(),
        None => {
            if boot_img.kernel.is_empty() {
                bail!("в boot.img нет ядра — укажите --kernel");
            }
            let k = instance_dir.join("kernel.bin");
            std::fs::write(&k, boot_img.kernel)
                .with_context(|| format!("не удалось записать {}", k.display()))?;
            k
        }
    };

    // Составной диск с GPT: каждый раздел — copy-on-write поверх исходного образа.
    let mut parts = Vec::new();
    for p in partition_plan(dir)? {
        let base: Box<dyn Disk> = match &p.image {
            Some(img) => {
                open_image(img).with_context(|| format!("не удалось открыть {}", img.display()))?
            }
            None => Box::new(ZeroDisk(p.empty_size)),
        };
        let cow = CowDisk::open(base, &overlays.join(format!("{}.rdcow", p.name)))
            .with_context(|| format!("оверлей раздела {}", p.name))?;
        parts.push(Partition {
            name: p.name,
            disk: Box::new(cow),
        });
    }
    let disk = CompositeDisk::new(parts)?;
    for (name, start, span) in disk.layout() {
        tracing::debug!(partition = %name, start, mib = span >> 20, "раздел");
    }

    // Консоль Android — всегда virtio-console hvc0: вывод пачками, без выхода vCPU на каждый
    // байт, и нумерация hvcN совпадает с Cuttlefish (HAL ждут свои порты на hvc2…hvc19).
    // Диск — первое устройство PCI: 00:01.0.
    let mut bootconfig =
        rustdroid_bootconfig(&vendor_img.bootconfig, "pci0000:00/0000:00:01.0", "hvc0");
    if let Some(spec) = args.vm.display.as_deref() {
        // Программный рендер в госте (SwiftShader/ANGLE) и композитор через DRM virtio-gpu.
        for (k, v) in display::GUEST_SWIFTSHADER {
            bootconfig.set(k, v);
        }
        // Частота vsync композитора ranchu — частота дисплея гостя.
        if let Ok(d) = DisplaySpec::parse(spec) {
            bootconfig.set("androidboot.qemu.vsync", &d.refresh_hz.to_string());
        }
    }
    for kv in &args.bootconfig {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--set ожидает КЛЮЧ=ЗНАЧЕНИЕ, получено {kv}"))?;
        bootconfig.set(k.trim(), v.trim());
    }
    let mut initrd = Vec::with_capacity(vendor_img.ramdisk.len() + generic_ramdisk.len() + 4096);
    initrd.extend_from_slice(vendor_img.ramdisk);
    initrd.extend_from_slice(generic_ramdisk);
    let initrd = append_bootconfig(initrd, &bootconfig.render());

    let mut cfg = base_config(&args.vm, ConsoleKind::Hvc, kernel);
    cfg.initrd_data = Some(Arc::new(initrd));
    let needs_bootconfig = !vendor_img
        .cmdline
        .split_whitespace()
        .any(|p| p == "bootconfig");
    cfg.cmdline_extra = [
        vendor_img.cmdline.as_str(),
        boot_img.cmdline.as_str(),
        if needs_bootconfig { "bootconfig" } else { "" },
        args.vm.append.as_str(),
    ]
    .iter()
    .filter(|s| !s.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join(" ");
    tracing::info!(images = %dir.display(), instance = %args.instance, "загрузка Android");

    // hvc1…hvc19 как у Cuttlefish; в hvc2 Android пишет logcat (служба seriallogging).
    let logcat_path = args
        .logcat
        .clone()
        .unwrap_or_else(|| instance_dir.join("logcat.txt"));
    let logcat = File::create(&logcat_path)
        .with_context(|| format!("не удалось создать {}", logcat_path.display()))?;
    tracing::info!(path = %logcat_path.display(), "logcat гостя");
    let mut logcat = Some(logcat);
    let extra_consoles = (1..CUTTLEFISH_CONSOLES)
        .map(|n| {
            let out: Box<dyn Write + Send> = match logcat.take_if(|_| n == LOGCAT_HVC) {
                Some(f) => Box::new(f),
                None => Box::new(std::io::sink()),
            };
            (format!("hvc{n}"), out)
        })
        .collect();

    let devices = VmDevices {
        disks: vec![("android".into(), Arc::new(disk))],
        extra_consoles,
        vsock_cid: Some(GUEST_CID),
        ..VmDevices::default()
    };
    run_vm(&cfg, devices, &args.vm, ConsoleKind::Hvc)
}

/// Число консолей virtio у Cuttlefish: hvc0…hvc19.
const CUTTLEFISH_CONSOLES: u32 = 20;
/// Консоль с logcat.
const LOGCAT_HVC: u32 = 2;
/// CID гостя virtio-vsock.
const GUEST_CID: u64 = 3;
