//! Виртуальная машина на WHPX: vCPU (SMP), RAM, COM1, IOAPIC, PCI с virtio-устройствами.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustdroid_devices::{
    CmosRtc, ConsoleInput, Disk, InputEvent, InputEvents, InputKind, Serial16550, VirtioBlk,
    VirtioConsole, VirtioInput, VirtioVsock,
};
use rustdroid_gpu::{DisplayMode, Mailbox, VirtioGpu};
use rustdroid_virtio::{GuestMemory, MsiMessage, MsiSink, VirtioPci};
use rustdroid_whpx::sys::{
    WHV_INTERNAL_ACTIVITY_REGISTER, WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0,
    WHV_X64_TABLE_REGISTER, WHvRegisterInternalActivityState, WHvX64RegisterCr0, WHvX64RegisterCr3,
    WHvX64RegisterCr4, WHvX64RegisterCs, WHvX64RegisterDs, WHvX64RegisterEfer, WHvX64RegisterEs,
    WHvX64RegisterFs, WHvX64RegisterGdtr, WHvX64RegisterGs, WHvX64RegisterIdtr, WHvX64RegisterPat,
    WHvX64RegisterRflags, WHvX64RegisterRip, WHvX64RegisterRsi, WHvX64RegisterSs, WHvX64RegisterTr,
};
use rustdroid_whpx::{
    DestinationMode, Exit, Interrupt, InterruptType, MapFlags, Partition, PartitionConfig,
    RegisterName, RegisterValue, TriggerMode, Vcpu, caps,
};

use crate::acpi::{self, AcpiConfig};
use crate::boot_cpu::{BootCpuState, Segment, boot_cpu_state, setup_boot_tables};
use crate::cmdline::{self, Timing};
use crate::error::{Error, Result};
use crate::ioapic::{ApicMessage, InterruptSink, IoApic};
use crate::layout::e820_map;
use crate::loader::{self, BootSpec};
use crate::memory::{AllocOptions, GuestRam};
use crate::pci::{PciBus, SharedFunction};
use crate::platform::{COM1_IRQ, IoApicLine, PlatformBus, VmEvents};
use crate::worker::DeviceWorker;

/// Максимум vCPU.
pub const MAX_CPUS: u32 = 64;

/// Параметры запуска.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// bzImage.
    pub kernel: PathBuf,
    /// initrd / initramfs.
    pub initrd: Option<PathBuf>,
    /// Готовое содержимое initrd (имеет приоритет над `initrd`), например собранное из
    /// образов Android.
    pub initrd_data: Option<Arc<Vec<u8>>>,
    /// RAM гостя, МиБ (кратно 2).
    pub memory_mib: u64,
    /// Число vCPU.
    pub cpus: u32,
    /// Дополнительные параметры командной строки.
    pub cmdline_extra: String,
    /// Полная замена командной строки (без параметров по умолчанию).
    pub cmdline_override: Option<String>,
    /// Большие страницы для RAM.
    pub large_pages: bool,
    /// `CONFIG_HZ` ядра гостя.
    pub guest_hz: u32,
    /// Консоль ядра: `ttyS0` или `hvc0`.
    pub kernel_console: String,
}

/// virtio-устройства ВМ.
#[derive(Default)]
pub struct VmDevices {
    /// Диски virtio-blk (по порядку слотов PCI, начиная с 00:01.0).
    pub disks: Vec<(String, Arc<dyn Disk>)>,
    /// virtio-console: вывод и буфер ввода (`hvc0`).
    pub console: Option<(Box<dyn Write + Send>, Arc<ConsoleInput>)>,
    /// Дополнительные virtio-console без ввода (`hvc1`, `hvc2`, … по порядку): имя и вывод.
    /// Нумерация `hvcN` в госте идёт по порядку устройств, поэтому они сразу за `hvc0`.
    pub extra_consoles: Vec<(String, Box<dyn Write + Send>)>,
    /// virtio-vsock с указанным CID гостя.
    pub vsock_cid: Option<u64>,
    /// virtio-gpu: режим дисплея и ящик кадров для композитора.
    pub gpu: Option<(DisplayMode, Arc<Mailbox>)>,
    /// virtio-input: вид устройства, имя и очередь событий.
    pub inputs: Vec<(InputKind, String, Arc<InputEvents>)>,
}

impl std::fmt::Debug for VmDevices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmDevices")
            .field(
                "disks",
                &self
                    .disks
                    .iter()
                    .map(|(n, d)| (n, d.size()))
                    .collect::<Vec<_>>(),
            )
            .field("console", &self.console.is_some())
            .field("extra_consoles", &self.extra_consoles.len())
            .field("vsock_cid", &self.vsock_cid)
            .field("gpu", &self.gpu.as_ref().map(|(m, _)| *m))
            .field("inputs", &self.inputs.len())
            .finish()
    }
}

/// Чем закончился запуск.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Гость запросил перезагрузку (или выключение через `reboot`).
    Reset,
    /// Остановлено пользователем.
    Stopped,
}

/// Счётчики выходов из гостя (сумма по всем vCPU).
#[derive(Debug, Default, Clone, Copy)]
pub struct ExitStats {
    /// Порты.
    pub io: u64,
    /// MMIO.
    pub mmio: u64,
    /// CPUID.
    pub cpuid: u64,
    /// HLT.
    pub halt: u64,
    /// Прочие.
    pub other: u64,
}

impl ExitStats {
    /// Всего выходов.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.io + self.mmio + self.cpuid + self.halt + self.other
    }

    fn add(&mut self, o: &Self) {
        self.io += o.io;
        self.mmio += o.mmio;
        self.cpuid += o.cpuid;
        self.halt += o.halt;
        self.other += o.other;
    }
}

/// Доставка прерываний (IOAPIC и MSI-X) в LAPIC гипервизора.
struct WhpxSink(Arc<Partition>);

impl WhpxSink {
    fn request(&self, delivery_mode: u8, logical: bool, level: bool, destination: u8, vector: u8) {
        let irq = Interrupt {
            kind: match delivery_mode {
                1 => InterruptType::LowestPriority,
                4 => InterruptType::Nmi,
                _ => InterruptType::Fixed,
            },
            destination_mode: if logical {
                DestinationMode::Logical
            } else {
                DestinationMode::Physical
            },
            trigger: if level {
                TriggerMode::Level
            } else {
                TriggerMode::Edge
            },
            destination: u32::from(destination),
            vector: u32::from(vector),
        };
        if let Err(e) = self.0.request_interrupt(irq) {
            tracing::warn!(error = %e, vector, "не удалось доставить прерывание");
        }
    }
}

impl InterruptSink for WhpxSink {
    fn deliver(&self, m: ApicMessage) {
        self.request(m.delivery_mode, m.logical, m.level, m.destination, m.vector);
    }
}

impl MsiSink for WhpxSink {
    fn signal(&self, m: MsiMessage) {
        self.request(
            m.delivery_mode(),
            m.logical(),
            m.level(),
            m.destination(),
            m.vector(),
        );
    }
}

/// Остановка ВМ из другого потока.
#[derive(Debug, Clone)]
pub struct Stopper {
    partition: Arc<Partition>,
    flag: Arc<AtomicBool>,
    cpus: u32,
}

impl Stopper {
    /// Просит все vCPU остановиться.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        for i in 0..self.cpus {
            if let Err(e) = self.partition.cancel_run(i) {
                tracing::debug!(error = %e, vcpu = i, "cancel_run");
            }
        }
    }
}

/// Ручка консоли virtio: ввод и пробуждение очереди приёма.
#[derive(Clone)]
pub struct ConsoleHandle {
    input: Arc<ConsoleInput>,
    function: SharedFunction,
}

impl std::fmt::Debug for ConsoleHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleHandle").finish_non_exhaustive()
    }
}

impl ConsoleHandle {
    /// Передаёт байты в гостя.
    pub fn push(&self, bytes: &[u8]) {
        self.input.push(bytes);
        self.function
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .notify(rustdroid_devices::console::RX_QUEUE);
    }
}

/// Ручка устройства ввода: события и пробуждение очереди событий.
#[derive(Clone)]
pub struct InputHandle {
    events: Arc<InputEvents>,
    function: SharedFunction,
}

impl std::fmt::Debug for InputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputHandle").finish_non_exhaustive()
    }
}

impl InputHandle {
    /// Передаёт события в гостя сразу, без ожидания кадра.
    pub fn push(&self, events: &[InputEvent]) {
        self.events.push(events);
        self.function
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .notify(0);
    }
}

/// Виртуальная машина. Поля удаляются в порядке объявления: workers и партиция до RAM.
pub struct Vm {
    /// Держит worker-потоки устройств живыми до удаления ВМ.
    _workers: Vec<DeviceWorker>,
    partition: Arc<Partition>,
    serial: Arc<Mutex<Serial16550>>,
    rtc: Arc<Mutex<CmosRtc>>,
    ioapic: Arc<Mutex<IoApic>>,
    pci: Arc<PciBus>,
    console: Option<ConsoleHandle>,
    inputs: Vec<InputHandle>,
    events: Arc<VmEvents>,
    stop: Arc<AtomicBool>,
    cpus: u32,
    boot: BootCpuState,
    cmdline: String,
    ram: Arc<GuestRam>,
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm")
            .field("cpus", &self.cpus)
            .field("cmdline", &self.cmdline)
            .finish_non_exhaustive()
    }
}

fn read_file(path: &PathBuf) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| Error::ReadFile {
        path: path.clone(),
        source,
    })
}

impl Vm {
    /// Создаёт ВМ, устройства и загружает ядро. Вывод COM1 пишется в `serial_out`.
    ///
    /// # Errors
    /// Гипервизор недоступен, файлы не читаются, образ некорректен или не хватает памяти.
    pub fn new(
        cfg: &VmConfig,
        devices: VmDevices,
        serial_out: Box<dyn Write + Send>,
    ) -> Result<Self> {
        caps::ensure_available()?;
        if cfg.cpus == 0 || cfg.cpus > MAX_CPUS {
            return Err(Error::Config(format!(
                "число vCPU должно быть 1..{MAX_CPUS}"
            )));
        }
        let started = Instant::now();
        let timing = Timing {
            tsc_hz: caps::processor_clock_hz()?,
            apic_hz: caps::interrupt_clock_hz()?,
            guest_hz: cfg.guest_hz,
        };
        tracing::info!(
            tsc_hz = timing.tsc_hz,
            apic_hz = timing.apic_hz,
            cpus = cfg.cpus,
            "частоты гипервизора"
        );

        let partition = Partition::new(&PartitionConfig {
            processor_count: cfg.cpus,
            cpuid_exits: Vec::new(),
        })?;

        let size = cfg
            .memory_mib
            .checked_mul(1 << 20)
            .ok_or_else(|| Error::Config("слишком много RAM".into()))?;
        let ram = Arc::new(GuestRam::new(
            size,
            AllocOptions {
                large_pages: cfg.large_pages,
            },
        )?);
        for region in ram.regions() {
            // SAFETY: память участка принадлежит `ram`, которая живёт дольше партиции
            // (поле `ram` объявлено последним и удаляется после `partition`).
            unsafe {
                partition.map_gpa_range(
                    ram.host_ptr(region),
                    region.range.gpa,
                    region.range.size,
                    MapFlags::RAM,
                )
            }?;
        }
        tracing::info!(
            mib = cfg.memory_mib,
            large_pages = ram.large_pages(),
            ms = started.elapsed().as_millis(),
            "RAM гостя выделена и отображена"
        );

        // Устройства PCI.
        let sink = Arc::new(WhpxSink(Arc::clone(&partition)));
        let mem: Arc<dyn GuestMemory> = Arc::clone(&ram) as Arc<dyn GuestMemory>;
        let msi: Arc<dyn MsiSink> = Arc::clone(&sink) as Arc<dyn MsiSink>;
        let mut pci = PciBus::new();
        let mut named: Vec<(String, SharedFunction)> = Vec::new();
        let mut add = |name: String,
                       dev: Box<dyn rustdroid_virtio::VirtioDevice>|
         -> Result<SharedFunction> {
            let bar = pci
                .allocate_bar()
                .ok_or_else(|| Error::Config("исчерпано окно MMIO PCI".into()))?;
            let f: SharedFunction = Arc::new(Mutex::new(VirtioPci::new(
                dev,
                bar,
                Arc::clone(&mem),
                Arc::clone(&msi),
            )));
            let slot = pci
                .add(Arc::clone(&f))
                .ok_or_else(|| Error::Config("на шине PCI нет свободных слотов".into()))?;
            tracing::info!(device = %name, pci = format_args!("00:{slot:02x}.0"), bar = format_args!("0x{bar:x}"), "устройство PCI");
            named.push((name, Arc::clone(&f)));
            Ok(f)
        };
        for (i, (name, disk)) in devices.disks.into_iter().enumerate() {
            let id = format!("rustdroid-{i}");
            tracing::info!(disk = %name, size = disk.size(), read_only = disk.read_only(), "диск");
            add(format!("blk{i}"), Box::new(VirtioBlk::new(disk, &id)))?;
        }
        let console = match devices.console {
            Some((out, input)) => {
                let f = add(
                    "console".into(),
                    Box::new(VirtioConsole::new(out, Arc::clone(&input))),
                )?;
                Some(ConsoleHandle { input, function: f })
            }
            None => None,
        };
        for (name, out) in devices.extra_consoles {
            add(
                name,
                Box::new(VirtioConsole::new(out, Arc::new(ConsoleInput::default()))),
            )?;
        }
        if let Some((mode, mailbox)) = devices.gpu {
            add("gpu".into(), Box::new(VirtioGpu::new(mode, mailbox)))?;
        }
        let mut inputs = Vec::new();
        for (i, (kind, name, events)) in devices.inputs.into_iter().enumerate() {
            let f = add(
                format!("input{i}"),
                Box::new(VirtioInput::new(kind, &name, Arc::clone(&events))),
            )?;
            inputs.push(InputHandle {
                events,
                function: f,
            });
        }
        if let Some(cid) = devices.vsock_cid {
            add("vsock".into(), Box::new(VirtioVsock::new(cid)))?;
        }
        let pci = Arc::new(pci);

        // Ядро, ACPI, таблицы.
        let kernel = read_file(&cfg.kernel)?;
        let initrd = match (&cfg.initrd_data, &cfg.initrd) {
            (Some(data), _) => Some(data.as_ref().clone()),
            (None, Some(p)) => Some(read_file(p)?),
            (None, None) => None,
        };
        let cmdline = cfg
            .cmdline_override
            .clone()
            .unwrap_or_else(|| cmdline::build(&timing, &cfg.kernel_console, &cfg.cmdline_extra));
        #[allow(clippy::cast_possible_truncation, reason = "cpus ≤ 64")]
        let acpi_cfg = AcpiConfig {
            cpus: cfg.cpus as u8,
            ..AcpiConfig::default()
        };
        let rsdp = acpi::write_tables(&ram, &acpi_cfg)?;
        setup_boot_tables(&ram)?;
        let e820 = e820_map(size);
        let loaded = loader::load(
            &ram,
            &BootSpec {
                kernel: &kernel,
                initrd: initrd.as_deref(),
                cmdline: &cmdline,
                e820: &e820,
                rsdp_addr: rsdp,
            },
        )?;
        tracing::info!(entry = format_args!("0x{:x}", loaded.entry), initrd = ?loaded.initrd, "ядро загружено");

        let ioapic = Arc::new(Mutex::new(IoApic::new(acpi_cfg.ioapic_id)));
        let line = Arc::new(IoApicLine::new(
            Arc::clone(&ioapic),
            COM1_IRQ,
            Arc::clone(&sink) as Arc<dyn InterruptSink>,
        ));
        let serial = Arc::new(Mutex::new(Serial16550::new(line, serial_out)));

        let workers = named
            .into_iter()
            .map(|(name, f)| DeviceWorker::spawn(&name, Arc::clone(&partition), f))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            _workers: workers,
            partition,
            serial,
            rtc: Arc::new(Mutex::new(CmosRtc::default())),
            ioapic,
            pci,
            console,
            inputs,
            events: Arc::new(VmEvents::default()),
            stop: Arc::new(AtomicBool::new(false)),
            cpus: cfg.cpus,
            boot: boot_cpu_state(&loaded),
            cmdline,
            ram,
        })
    }

    /// COM1 — для передачи ввода с консоли хоста.
    #[must_use]
    pub fn serial(&self) -> Arc<Mutex<Serial16550>> {
        Arc::clone(&self.serial)
    }

    /// Устройства ввода в порядке `VmDevices::inputs`.
    #[must_use]
    pub fn inputs(&self) -> Vec<InputHandle> {
        self.inputs.clone()
    }

    /// virtio-console, если подключена.
    #[must_use]
    pub fn console(&self) -> Option<ConsoleHandle> {
        self.console.clone()
    }

    /// Объект для остановки ВМ из другого потока.
    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper {
            partition: Arc::clone(&self.partition),
            flag: Arc::clone(&self.stop),
            cpus: self.cpus,
        }
    }

    /// Итоговая командная строка ядра.
    #[must_use]
    pub fn cmdline(&self) -> &str {
        &self.cmdline
    }

    fn bus(&self) -> PlatformBus {
        PlatformBus::new(
            Arc::clone(&self.ram),
            Arc::clone(&self.serial),
            Arc::clone(&self.rtc),
            Arc::clone(&self.ioapic),
            Arc::clone(&self.pci),
            Arc::clone(&self.events),
        )
    }

    /// Запускает vCPU: BSP в текущем потоке, AP — в своих потоках. Возвращается после
    /// перезагрузки, остановки или ошибки любого vCPU.
    ///
    /// # Errors
    /// Ошибки WHPX или невосстановимая ошибка гостя.
    pub fn run(self) -> Result<(RunOutcome, ExitStats)> {
        let mut vcpus = Vec::new();
        for i in 0..self.cpus {
            let vcpu = self.partition.create_vcpu(i)?;
            if i == 0 {
                set_boot_registers(&vcpu, &self.boot)?;
            } else {
                // AP ждёт INIT/SIPI от BSP; доставку обрабатывает LAPIC гипервизора.
                let mut activity = WHV_INTERNAL_ACTIVITY_REGISTER::default();
                activity.AsUINT64 = 1; // StartupSuspend
                vcpu.set_registers(
                    &[WHvRegisterInternalActivityState],
                    &[RegisterValue {
                        InternalActivity: activity,
                    }],
                )?;
            }
            vcpus.push(vcpu);
        }
        let stopper = self.stopper();
        let started = Instant::now();
        let results: Vec<Result<(RunOutcome, ExitStats)>> = std::thread::scope(|scope| {
            let mut iter = vcpus.into_iter();
            let bsp = iter.next();
            let handles: Vec<_> = iter
                .map(|mut vcpu| {
                    let mut bus = self.bus();
                    let stopper = stopper.clone();
                    let (events, stop) = (&self.events, &self.stop);
                    std::thread::Builder::new()
                        .name(format!("vcpu{}", vcpu.index()))
                        .spawn_scoped(scope, move || {
                            let _tuning = crate::host::tune_current_thread("Games");
                            let r = run_loop(&mut vcpu, &mut bus, events, stop);
                            stopper.stop();
                            r
                        })
                })
                .collect();
            let mut results = Vec::new();
            if let Some(mut vcpu) = bsp {
                let _tuning = crate::host::tune_current_thread("Games");
                let mut bus = self.bus();
                results.push(run_loop(&mut vcpu, &mut bus, &self.events, &self.stop));
                stopper.stop();
            }
            for h in handles {
                match h {
                    Ok(h) => results.push(h.join().unwrap_or_else(|_| {
                        Err(Error::Config("поток vCPU завершился аварийно".into()))
                    })),
                    Err(e) => results.push(Err(Error::Config(format!(
                        "не удалось запустить поток vCPU: {e}"
                    )))),
                }
            }
            results
        });

        let mut stats = ExitStats::default();
        let mut outcome = RunOutcome::Stopped;
        let mut first_error = None;
        for r in results {
            match r {
                Ok((o, s)) => {
                    stats.add(&s);
                    if o == RunOutcome::Reset {
                        outcome = RunOutcome::Reset;
                    }
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        let secs = started.elapsed().as_secs_f64().max(1e-3);
        #[allow(clippy::cast_precision_loss, reason = "статистика для лога")]
        let per_sec = stats.total() as f64 / secs;
        tracing::info!(
            ?outcome,
            ?stats,
            exits_per_sec = per_sec.round(),
            "ВМ остановлена"
        );
        tracing::info!(
            unknown_pio = self.events.unknown_pio(),
            unknown_mmio = self.events.unknown_mmio(),
            "обращения к неизвестным устройствам"
        );
        match first_error {
            Some(e) => Err(e),
            None => Ok((outcome, stats)),
        }
    }
}

fn run_loop(
    vcpu: &mut Vcpu,
    bus: &mut PlatformBus,
    events: &VmEvents,
    stop: &AtomicBool,
) -> Result<(RunOutcome, ExitStats)> {
    let mut stats = ExitStats::default();
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok((RunOutcome::Stopped, stats));
        }
        match vcpu.run()? {
            Exit::IoPort => {
                stats.io += 1;
                vcpu.handle_io(bus)?;
            }
            Exit::MemoryAccess => {
                stats.mmio += 1;
                if let Err(e) = vcpu.handle_mmio(bus) {
                    tracing::error!(vcpu = vcpu.index(), gpa = format_args!("0x{:x}", vcpu.mmio_gpa()), error = %e, "MMIO не эмулирован");
                    return Err(e.into());
                }
            }
            Exit::Cpuid => {
                stats.cpuid += 1;
                vcpu.handle_cpuid(|_, _, default| default)?;
            }
            Exit::Halt => {
                // При эмуляции LAPIC гипервизором HLT обычно обрабатывается без выхода;
                // если выход всё же пришёл — короткое ожидание прерывания.
                stats.halt += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            Exit::Canceled | Exit::InterruptWindow | Exit::ApicEoi(_) => stats.other += 1,
            Exit::UnrecoverableException => {
                return Err(fault(vcpu, "тройная ошибка (triple fault)"));
            }
            Exit::InvalidRegisterValue => {
                return Err(fault(vcpu, "некорректное состояние регистров"));
            }
            Exit::UnsupportedFeature => {
                return Err(fault(vcpu, "неподдерживаемая гипервизором возможность"));
            }
            Exit::Exception => return Err(fault(vcpu, "необработанное исключение гостя")),
            Exit::Msr => return Err(fault(vcpu, "неожиданный выход по MSR")),
            Exit::Other(code) => {
                tracing::error!(code, "неизвестная причина выхода");
                return Err(fault(vcpu, "неизвестная причина выхода"));
            }
        }
        if events.reset_requested() {
            return Ok((RunOutcome::Reset, stats));
        }
    }
}

fn fault(vcpu: &Vcpu, reason: &'static str) -> Error {
    let rip = vcpu.exit_context().VpContext.Rip;
    tracing::error!(
        vcpu = vcpu.index(),
        reason,
        rip = format_args!("0x{rip:x}"),
        "гость остановлен"
    );
    Error::GuestFault { reason, rip }
}

fn segment(s: &Segment) -> RegisterValue {
    RegisterValue {
        Segment: WHV_X64_SEGMENT_REGISTER {
            Base: s.base,
            Limit: s.limit,
            Selector: s.selector,
            Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
                Attributes: s.attributes,
            },
        },
    }
}

fn table(base: u64, limit: u16) -> RegisterValue {
    RegisterValue {
        Table: WHV_X64_TABLE_REGISTER {
            Pad: [0; 3],
            Limit: limit,
            Base: base,
        },
    }
}

fn set_boot_registers(vcpu: &Vcpu, s: &BootCpuState) -> Result<()> {
    const PAT_DEFAULT: u64 = 0x0007_0406_0007_0406;
    let names: [RegisterName; 17] = [
        WHvX64RegisterCr0,
        WHvX64RegisterCr3,
        WHvX64RegisterCr4,
        WHvX64RegisterEfer,
        WHvX64RegisterPat,
        WHvX64RegisterGdtr,
        WHvX64RegisterIdtr,
        WHvX64RegisterCs,
        WHvX64RegisterDs,
        WHvX64RegisterEs,
        WHvX64RegisterFs,
        WHvX64RegisterGs,
        WHvX64RegisterSs,
        WHvX64RegisterTr,
        WHvX64RegisterRflags,
        WHvX64RegisterRsi,
        WHvX64RegisterRip,
    ];
    let values: [RegisterValue; 17] = [
        RegisterValue { Reg64: s.cr0 },
        RegisterValue { Reg64: s.cr3 },
        RegisterValue { Reg64: s.cr4 },
        RegisterValue { Reg64: s.efer },
        RegisterValue { Reg64: PAT_DEFAULT },
        table(s.gdt.0, s.gdt.1),
        table(s.idt.0, s.idt.1),
        segment(&s.cs),
        segment(&s.ds),
        segment(&s.ds),
        segment(&s.ds),
        segment(&s.ds),
        segment(&s.ds),
        segment(&s.tr),
        RegisterValue { Reg64: s.rflags },
        RegisterValue { Reg64: s.rsi },
        RegisterValue { Reg64: s.rip },
    ];
    vcpu.set_registers(&names, &values)?;
    Ok(())
}
