//! Виртуальная машина этапа 1 на WHPX: один vCPU, RAM, COM1, IOAPIC, прямая загрузка ядра.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustdroid_devices::Serial16550;
use rustdroid_whpx::sys::{
    WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0, WHV_X64_TABLE_REGISTER,
    WHvX64RegisterCr0, WHvX64RegisterCr3, WHvX64RegisterCr4, WHvX64RegisterCs, WHvX64RegisterDs,
    WHvX64RegisterEfer, WHvX64RegisterEs, WHvX64RegisterFs, WHvX64RegisterGdtr, WHvX64RegisterGs,
    WHvX64RegisterIdtr, WHvX64RegisterPat, WHvX64RegisterRflags, WHvX64RegisterRip,
    WHvX64RegisterRsi, WHvX64RegisterSs, WHvX64RegisterTr,
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
use crate::platform::{COM1_IRQ, IoApicLine, PlatformBus, VmEvents};

/// Параметры запуска.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// bzImage.
    pub kernel: PathBuf,
    /// initrd / initramfs.
    pub initrd: Option<PathBuf>,
    /// RAM гостя, МиБ (кратно 2).
    pub memory_mib: u64,
    /// Дополнительные параметры командной строки.
    pub cmdline_extra: String,
    /// Полная замена командной строки (без параметров по умолчанию).
    pub cmdline_override: Option<String>,
    /// Большие страницы для RAM.
    pub large_pages: bool,
    /// `CONFIG_HZ` ядра гостя.
    pub guest_hz: u32,
}

/// Чем закончился запуск.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Гость запросил перезагрузку (или выключение через `reboot`).
    Reset,
    /// Остановлено пользователем.
    Stopped,
}

/// Счётчики выходов из гостя.
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
}

/// Доставка прерываний IOAPIC в LAPIC гипервизора.
struct WhpxSink(Arc<Partition>);

impl InterruptSink for WhpxSink {
    fn deliver(&self, m: ApicMessage) {
        let irq = Interrupt {
            kind: match m.delivery_mode {
                1 => InterruptType::LowestPriority,
                4 => InterruptType::Nmi,
                _ => InterruptType::Fixed,
            },
            destination_mode: if m.logical {
                DestinationMode::Logical
            } else {
                DestinationMode::Physical
            },
            trigger: if m.level {
                TriggerMode::Level
            } else {
                TriggerMode::Edge
            },
            destination: u32::from(m.destination),
            vector: u32::from(m.vector),
        };
        if let Err(e) = self.0.request_interrupt(irq) {
            tracing::warn!(error = %e, vector = m.vector, "не удалось доставить прерывание");
        }
    }
}

/// Остановка ВМ из другого потока.
#[derive(Debug, Clone)]
pub struct Stopper {
    partition: Arc<Partition>,
    flag: Arc<AtomicBool>,
}

impl Stopper {
    /// Просит vCPU остановиться.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        if let Err(e) = self.partition.cancel_run(0) {
            tracing::debug!(error = %e, "cancel_run");
        }
    }
}

/// Виртуальная машина. Поля удаляются в порядке объявления: партиция до RAM.
pub struct Vm {
    partition: Arc<Partition>,
    serial: Arc<Mutex<Serial16550>>,
    ioapic: Arc<Mutex<IoApic>>,
    events: Arc<VmEvents>,
    stop: Arc<AtomicBool>,
    boot: BootCpuState,
    cmdline: String,
    ram: Arc<GuestRam>,
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm")
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
    /// Создаёт ВМ и загружает ядро. Вывод COM1 пишется в `serial_out`.
    ///
    /// # Errors
    /// Гипервизор недоступен, файлы не читаются, образ некорректен или не хватает памяти.
    pub fn new(cfg: &VmConfig, serial_out: Box<dyn Write + Send>) -> Result<Self> {
        caps::ensure_available()?;
        let started = Instant::now();

        let timing = Timing {
            tsc_hz: caps::processor_clock_hz()?,
            apic_hz: caps::interrupt_clock_hz()?,
            guest_hz: cfg.guest_hz,
        };
        tracing::info!(
            tsc_hz = timing.tsc_hz,
            apic_hz = timing.apic_hz,
            "частоты гипервизора"
        );

        let partition = Partition::new(&PartitionConfig {
            processor_count: 1,
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

        let kernel = read_file(&cfg.kernel)?;
        let initrd = cfg.initrd.as_ref().map(read_file).transpose()?;
        let cmdline = cfg
            .cmdline_override
            .clone()
            .unwrap_or_else(|| cmdline::build(&timing, &cfg.cmdline_extra));

        let rsdp = acpi::write_tables(&ram, &AcpiConfig::default())?;
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
        tracing::info!(
            entry = format_args!("0x{:x}", loaded.entry),
            initrd = ?loaded.initrd,
            "ядро загружено"
        );

        let ioapic = Arc::new(Mutex::new(IoApic::new(AcpiConfig::default().ioapic_id)));
        let sink: Arc<dyn InterruptSink> = Arc::new(WhpxSink(Arc::clone(&partition)));
        let line = Arc::new(IoApicLine::new(Arc::clone(&ioapic), COM1_IRQ, sink));
        let serial = Arc::new(Mutex::new(Serial16550::new(line, serial_out)));

        Ok(Self {
            partition,
            serial,
            ioapic,
            events: Arc::new(VmEvents::default()),
            stop: Arc::new(AtomicBool::new(false)),
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

    /// Объект для остановки ВМ из другого потока.
    #[must_use]
    pub fn stopper(&self) -> Stopper {
        Stopper {
            partition: Arc::clone(&self.partition),
            flag: Arc::clone(&self.stop),
        }
    }

    /// Итоговая командная строка ядра.
    #[must_use]
    pub fn cmdline(&self) -> &str {
        &self.cmdline
    }

    /// Запускает vCPU 0 в текущем потоке до перезагрузки, остановки или ошибки.
    ///
    /// # Errors
    /// Ошибки WHPX или невосстановимая ошибка гостя.
    pub fn run(self) -> Result<(RunOutcome, ExitStats)> {
        let _tuning = crate::host::tune_current_thread("Games");
        let mut vcpu = self.partition.create_vcpu(0)?;
        set_boot_registers(&vcpu, &self.boot)?;

        let mut bus = PlatformBus::new(
            Arc::clone(&self.ram),
            Arc::clone(&self.serial),
            Arc::clone(&self.ioapic),
            Arc::clone(&self.events),
        );
        let result = run_loop(&mut vcpu, &mut bus, &self.events, &self.stop);
        tracing::info!(
            unknown_pio = self.events.unknown_pio(),
            unknown_mmio = self.events.unknown_mmio(),
            "обращения к неизвестным устройствам"
        );
        // vCPU удаляется до партиции и RAM.
        drop(vcpu);
        result
    }
}

fn run_loop(
    vcpu: &mut Vcpu,
    bus: &mut PlatformBus,
    events: &VmEvents,
    stop: &AtomicBool,
) -> Result<(RunOutcome, ExitStats)> {
    let mut stats = ExitStats::default();
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return finish(RunOutcome::Stopped, stats, started);
        }
        match vcpu.run()? {
            Exit::IoPort => {
                stats.io += 1;
                vcpu.handle_io(bus)?;
            }
            Exit::MemoryAccess => {
                stats.mmio += 1;
                if let Err(e) = vcpu.handle_mmio(bus) {
                    tracing::error!(gpa = format_args!("0x{:x}", vcpu.mmio_gpa()), error = %e, "MMIO не эмулирован");
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
            return finish(RunOutcome::Reset, stats, started);
        }
    }
}

fn finish(
    outcome: RunOutcome,
    stats: ExitStats,
    started: Instant,
) -> Result<(RunOutcome, ExitStats)> {
    let secs = started.elapsed().as_secs_f64().max(1e-3);
    #[allow(clippy::cast_precision_loss, reason = "статистика для лога")]
    let per_sec = stats.total() as f64 / secs;
    tracing::info!(
        ?outcome,
        ?stats,
        exits_per_sec = per_sec.round(),
        "vCPU остановлен"
    );
    Ok((outcome, stats))
}

fn fault(vcpu: &Vcpu, reason: &'static str) -> Error {
    let rip = vcpu.exit_context().VpContext.Rip;
    tracing::error!(reason, rip = format_args!("0x{rip:x}"), "гость остановлен");
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
