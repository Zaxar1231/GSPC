//! Виртуальный процессор: запуск, регистры, обработка выходов ввода-вывода.

use std::cell::Cell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::Arc;

use windows::Win32::System::Hypervisor::{
    WHV_EMULATOR_CALLBACKS, WHV_EMULATOR_IO_ACCESS_INFO, WHV_EMULATOR_MEMORY_ACCESS_INFO,
    WHV_EMULATOR_STATUS, WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_CONTEXT,
    WHV_TRANSLATE_GVA_FLAGS, WHV_TRANSLATE_GVA_RESULT, WHV_TRANSLATE_GVA_RESULT_CODE,
    WHV_X64_PENDING_EXCEPTION_EVENT_0, WHvRegisterPendingEvent, WHvRunVpExitReasonCanceled,
    WHvRunVpExitReasonException, WHvRunVpExitReasonInvalidVpRegisterValue,
    WHvRunVpExitReasonMemoryAccess, WHvRunVpExitReasonUnrecoverableException,
    WHvRunVpExitReasonUnsupportedFeature, WHvRunVpExitReasonX64ApicEoi, WHvRunVpExitReasonX64Cpuid,
    WHvRunVpExitReasonX64Halt, WHvRunVpExitReasonX64InterruptWindow,
    WHvRunVpExitReasonX64IoPortAccess, WHvRunVpExitReasonX64MsrAccess,
    WHvTranslateGvaFlagSetPageTableBits, WHvTranslateGvaFlagValidateRead,
    WHvTranslateGvaFlagValidateWrite, WHvTranslateGvaResultPageNotPresent,
    WHvTranslateGvaResultSuccess, WHvX64RegisterCr2, WHvX64RegisterRax, WHvX64RegisterRbx,
    WHvX64RegisterRcx, WHvX64RegisterRdx, WHvX64RegisterRip,
};
use windows::core::HRESULT;

use crate::api::{self, Emulation};
use crate::error::{Error, Result, check};
use crate::partition::Partition;

/// Имя регистра (как в WHPX).
pub type RegisterName = WHV_REGISTER_NAME;
/// Значение регистра (как в WHPX).
pub type RegisterValue = WHV_REGISTER_VALUE;

const S_OK: HRESULT = HRESULT(0);
#[allow(clippy::cast_possible_wrap, reason = "стандартный код E_FAIL")]
const E_FAIL: HRESULT = HRESULT(0x8000_4005_u32 as i32);

/// Шина устройств, к которой обращается гость через порты и MMIO.
///
/// Обращения к RAM при эмуляции строковых инструкций (`rep outsb` и т. п.) тоже приходят в
/// `mmio_read` / `mmio_write`: реализация должна обслуживать и RAM, и устройства.
pub trait Bus {
    /// Чтение из порта ввода-вывода (1, 2 или 4 байта).
    fn pio_read(&mut self, port: u16, data: &mut [u8]);
    /// Запись в порт ввода-вывода.
    fn pio_write(&mut self, port: u16, data: &[u8]);
    /// Чтение по гостевому физическому адресу (до 8 байт).
    fn mmio_read(&mut self, gpa: u64, data: &mut [u8]);
    /// Запись по гостевому физическому адресу.
    fn mmio_write(&mut self, gpa: u64, data: &[u8]);
}

/// Результат перевода виртуального адреса гостя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translation {
    /// Физический адрес.
    Mapped(u64),
    /// Обращение вызвало бы `#PF`: страницы нет (`present = false`) или нет прав.
    PageFault {
        /// Страница присутствует (нарушение прав).
        present: bool,
    },
}

/// Причина выхода из гостя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Доступ к неотображённой памяти (MMIO). Обработать через [`Vcpu::handle_mmio`].
    MemoryAccess,
    /// Доступ к порту. Обработать через [`Vcpu::handle_io`].
    IoPort,
    /// CPUID (если лист в списке выходов). Обработать через [`Vcpu::handle_cpuid`].
    Cpuid,
    /// Доступ к MSR (если выходы включены).
    Msr,
    /// HLT.
    Halt,
    /// Окно для доставки прерывания.
    InterruptWindow,
    /// Запуск отменён через [`Partition::cancel_run`].
    Canceled,
    /// Тройная ошибка или иное невосстановимое исключение — гость остановлен.
    UnrecoverableException,
    /// Некорректное состояние регистров.
    InvalidRegisterValue,
    /// Неподдерживаемая гипервизором возможность.
    UnsupportedFeature,
    /// EOI в LAPIC для уровневого прерывания (вектор).
    ApicEoi(u32),
    /// Перехваченное исключение гостя.
    Exception,
    /// Другая причина (код WHPX).
    Other(i32),
}

/// Виртуальный процессор. Запускается только из одного потока (`Send`, но не `Sync`).
pub struct Vcpu {
    partition: Arc<Partition>,
    index: u32,
    exit: WHV_RUN_VP_EXIT_CONTEXT,
    emulator: *mut c_void,
    emu: &'static Emulation,
    _not_sync: PhantomData<Cell<()>>,
}

// SAFETY: VP и экземпляр эмулятора можно использовать из любого потока, но не из нескольких
// одновременно; `Vcpu` не `Sync`, поэтому одновременный доступ невозможен.
unsafe impl Send for Vcpu {}

impl std::fmt::Debug for Vcpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vcpu")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

/// Контекст колбэков эмулятора на время одного вызова `TryIo/TryMmio`.
struct EmuContext<'a> {
    partition: &'a Partition,
    index: u32,
    bus: &'a mut dyn Bus,
}

impl Vcpu {
    pub(crate) fn new(partition: Arc<Partition>, index: u32) -> Result<Self> {
        let api = partition.api();
        let emu = api::emulation()?;
        // SAFETY: дескриптор партиции валиден.
        check("WHvCreateVirtualProcessor", unsafe {
            (api.create_vp)(partition.handle(), index, 0)
        })?;

        #[allow(
            clippy::cast_possible_truncation,
            reason = "структура колбэков — десятки байт"
        )]
        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(cb_io_port),
            WHvEmulatorMemoryCallback: Some(cb_memory),
            WHvEmulatorGetVirtualProcessorRegisters: Some(cb_get_registers),
            WHvEmulatorSetVirtualProcessorRegisters: Some(cb_set_registers),
            WHvEmulatorTranslateGvaPage: Some(cb_translate_gva),
        };
        let mut emulator: *mut c_void = std::ptr::null_mut();
        // SAFETY: структура колбэков и выходной указатель валидны на время вызова.
        let hr = unsafe { (emu.create)(&raw const callbacks, &raw mut emulator) };
        if let Err(e) = check("WHvEmulatorCreateEmulator", hr) {
            // SAFETY: VP только что создан и больше нигде не используется.
            let _ = unsafe { (api.delete_vp)(partition.handle(), index) };
            return Err(e);
        }
        Ok(Self {
            partition,
            index,
            exit: WHV_RUN_VP_EXIT_CONTEXT::default(),
            emulator,
            emu,
            _not_sync: PhantomData,
        })
    }

    /// Индекс VP.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Партиция, которой принадлежит VP.
    #[must_use]
    pub fn partition(&self) -> &Arc<Partition> {
        &self.partition
    }

    /// Полный контекст последнего выхода.
    #[must_use]
    pub fn exit_context(&self) -> &WHV_RUN_VP_EXIT_CONTEXT {
        &self.exit
    }

    /// Запускает гостя до следующего выхода.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn run(&mut self) -> Result<Exit> {
        let api = self.partition.api();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "контекст выхода — сотни байт"
        )]
        let size = size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32;
        // SAFETY: буфер выхода принадлежит `self` и имеет нужный размер.
        let hr = unsafe {
            (api.run_vp)(
                self.partition.handle(),
                self.index,
                (&raw mut self.exit).cast::<c_void>(),
                size,
            )
        };
        check("WHvRunVirtualProcessor", hr)?;
        let reason = self.exit.ExitReason;
        Ok(match reason {
            r if r == WHvRunVpExitReasonMemoryAccess => Exit::MemoryAccess,
            r if r == WHvRunVpExitReasonX64IoPortAccess => Exit::IoPort,
            r if r == WHvRunVpExitReasonX64Cpuid => Exit::Cpuid,
            r if r == WHvRunVpExitReasonX64MsrAccess => Exit::Msr,
            r if r == WHvRunVpExitReasonX64Halt => Exit::Halt,
            r if r == WHvRunVpExitReasonX64InterruptWindow => Exit::InterruptWindow,
            r if r == WHvRunVpExitReasonCanceled => Exit::Canceled,
            r if r == WHvRunVpExitReasonUnrecoverableException => Exit::UnrecoverableException,
            r if r == WHvRunVpExitReasonInvalidVpRegisterValue => Exit::InvalidRegisterValue,
            r if r == WHvRunVpExitReasonUnsupportedFeature => Exit::UnsupportedFeature,
            r if r == WHvRunVpExitReasonException => Exit::Exception,
            r if r == WHvRunVpExitReasonX64ApicEoi => {
                // SAFETY: для этой причины выхода заполнено поле ApicEoi.
                Exit::ApicEoi(unsafe { self.exit.Anonymous.ApicEoi.InterruptVector })
            }
            other => Exit::Other(other.0),
        })
    }

    /// Читает регистры.
    ///
    /// # Errors
    /// Ошибка WHPX или разная длина срезов.
    pub fn get_registers(
        &self,
        names: &[RegisterName],
        values: &mut [RegisterValue],
    ) -> Result<()> {
        if names.len() != values.len() {
            return Err(Error::InvalidArgument("names.len() != values.len()"));
        }
        let count = u32::try_from(names.len())
            .map_err(|_| Error::InvalidArgument("слишком много регистров"))?;
        let api = self.partition.api();
        // SAFETY: срезы валидны и одной длины `count`.
        let hr = unsafe {
            (api.get_vp_registers)(
                self.partition.handle(),
                self.index,
                names.as_ptr(),
                count,
                values.as_mut_ptr(),
            )
        };
        check("WHvGetVirtualProcessorRegisters", hr)
    }

    /// Записывает регистры.
    ///
    /// # Errors
    /// Ошибка WHPX или разная длина срезов.
    pub fn set_registers(&self, names: &[RegisterName], values: &[RegisterValue]) -> Result<()> {
        if names.len() != values.len() {
            return Err(Error::InvalidArgument("names.len() != values.len()"));
        }
        let count = u32::try_from(names.len())
            .map_err(|_| Error::InvalidArgument("слишком много регистров"))?;
        let api = self.partition.api();
        // SAFETY: срезы валидны и одной длины `count`.
        let hr = unsafe {
            (api.set_vp_registers)(
                self.partition.handle(),
                self.index,
                names.as_ptr(),
                count,
                values.as_ptr(),
            )
        };
        check("WHvSetVirtualProcessorRegisters", hr)
    }

    /// Перехваченное исключение (после [`Exit::Exception`]): вектор и байты инструкции.
    #[must_use]
    pub fn exception(&self) -> (u8, &[u8]) {
        // SAFETY: для выхода Exception заполнено поле VpException; вызывающий проверяет
        // причину выхода, иначе байты — нули и декодер их отвергнет.
        let e = unsafe { &self.exit.Anonymous.VpException };
        let n = usize::from(e.InstructionByteCount).min(e.InstructionBytes.len());
        (
            e.ExceptionType,
            e.InstructionBytes.get(..n).unwrap_or_default(),
        )
    }

    /// Уровень привилегий гостя в момент выхода (0 — ядро, 3 — пользователь).
    #[must_use]
    pub fn cpl(&self) -> u8 {
        // SAFETY: ExecutionState — битовое поле, младшие 2 бита — CPL.
        #[allow(clippy::cast_possible_truncation, reason = "2 бита")]
        let cpl = (unsafe { self.exit.VpContext.ExecutionState.Anonymous._bitfield } & 3) as u8;
        cpl
    }

    /// Переводит виртуальный адрес гостя в физический с проверкой прав, как при обращении
    /// с текущим CPL; биты Accessed/Dirty в таблицах страниц выставляются.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn translate(&self, gva: u64, write: bool) -> Result<Translation> {
        let mut flags = WHvTranslateGvaFlagValidateRead.0 | WHvTranslateGvaFlagSetPageTableBits.0;
        if write {
            flags |= WHvTranslateGvaFlagValidateWrite.0;
        }
        let api = self.partition.api();
        let mut result = WHV_TRANSLATE_GVA_RESULT::default();
        let mut gpa = 0_u64;
        // SAFETY: выходные указатели — локальные переменные.
        check("WHvTranslateGva", unsafe {
            (api.translate_gva)(
                self.partition.handle(),
                self.index,
                gva,
                WHV_TRANSLATE_GVA_FLAGS(flags),
                &raw mut result,
                &raw mut gpa,
            )
        })?;
        Ok(match result.ResultCode {
            c if c == WHvTranslateGvaResultSuccess => Translation::Mapped(gpa),
            c if c == WHvTranslateGvaResultPageNotPresent => {
                Translation::PageFault { present: false }
            }
            _ => Translation::PageFault { present: true },
        })
    }

    /// Ставит гостю исключение `vector` (с кодом ошибки и параметром — CR2 для `#PF`); оно
    /// будет доставлено при следующем запуске vCPU.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn inject_exception(
        &self,
        vector: u8,
        error_code: Option<u32>,
        parameter: u64,
    ) -> Result<()> {
        // WHV_X64_PENDING_EXCEPTION_EVENT: EventPending:1, EventType:3 (0 — исключение),
        // Reserved:4, DeliverErrorCode:1, Reserved:7, Vector:16.
        let bits = 1_u32 | (u32::from(error_code.is_some()) << 8) | (u32::from(vector) << 16);
        let mut value = RegisterValue::default();
        value.ExceptionEvent.Anonymous = WHV_X64_PENDING_EXCEPTION_EVENT_0 {
            _bitfield: bits,
            ErrorCode: error_code.unwrap_or(0),
            ExceptionParameter: parameter,
        };
        if vector == 14 {
            let mut cr2 = RegisterValue::default();
            cr2.Reg64 = parameter;
            self.set_registers(&[WHvX64RegisterCr2], &[cr2])?;
        }
        self.set_registers(&[WHvRegisterPendingEvent], &[value])
    }

    fn instruction_length(&self) -> u64 {
        // Битовое поле VpContext: InstructionLength:4, Cr8:4.
        u64::from(self.exit.VpContext._bitfield & 0x0F)
    }

    /// Обрабатывает выход [`Exit::IoPort`]. Простые `in`/`out` обслуживаются напрямую
    /// (быстрый путь), строковые и с префиксом `rep` — через эмулятор WHPX.
    ///
    /// # Errors
    /// Ошибка WHPX или эмулятора.
    pub fn handle_io(&mut self, bus: &mut dyn Bus) -> Result<()> {
        // SAFETY: метод вызывается после выхода IoPort, поле IoPortAccess заполнено.
        let io = unsafe { self.exit.Anonymous.IoPortAccess };
        // SAFETY: AccessInfo — union из u32 и битового поля, чтение u32 всегда корректно.
        let info = unsafe { io.AccessInfo.AsUINT32 };
        let is_write = info & 1 != 0;
        let size = ((info >> 1) & 0x7) as usize;
        let string_op = info & (1 << 4) != 0;
        let rep = info & (1 << 5) != 0;

        if string_op || rep || !matches!(size, 1 | 2 | 4) {
            let mut status = WHV_EMULATOR_STATUS::default();
            let mut ctx = EmuContext {
                partition: &self.partition,
                index: self.index,
                bus,
            };
            // SAFETY: эмулятор создан для этого VP; контекст живёт до конца вызова, а колбэки
            // получают его обратно как `*const c_void`.
            let hr = unsafe {
                (self.emu.try_io)(
                    self.emulator,
                    (&raw mut ctx).cast::<c_void>(),
                    &raw const self.exit.VpContext,
                    &raw const io,
                    &raw mut status,
                )
            };
            check("WHvEmulatorTryIoEmulation", hr)?;
            return emulation_result(status);
        }

        let port = io.PortNumber;
        let mut rax = io.Rax;
        if is_write {
            let bytes = rax.to_le_bytes();
            bus.pio_write(port, bytes.get(..size).unwrap_or(&[]));
        } else {
            let mut bytes = [0_u8; 4];
            if let Some(buf) = bytes.get_mut(..size) {
                bus.pio_read(port, buf);
            }
            let value = u64::from(u32::from_le_bytes(bytes));
            rax = match size {
                1 => (rax & !0xFF) | (value & 0xFF),
                2 => (rax & !0xFFFF) | (value & 0xFFFF),
                // 32-битная запись в EAX обнуляет старшую половину RAX.
                _ => value,
            };
        }
        let rip = self.exit.VpContext.Rip + self.instruction_length();
        self.set_registers(
            &[WHvX64RegisterRax, WHvX64RegisterRip],
            &[RegisterValue { Reg64: rax }, RegisterValue { Reg64: rip }],
        )
    }

    /// Обрабатывает выход [`Exit::MemoryAccess`] через эмулятор инструкций.
    ///
    /// # Errors
    /// Ошибка WHPX или эмулятора.
    pub fn handle_mmio(&mut self, bus: &mut dyn Bus) -> Result<()> {
        // SAFETY: метод вызывается после выхода MemoryAccess, поле MemoryAccess заполнено.
        let mem = unsafe { self.exit.Anonymous.MemoryAccess };
        let mut status = WHV_EMULATOR_STATUS::default();
        let mut ctx = EmuContext {
            partition: &self.partition,
            index: self.index,
            bus,
        };
        // SAFETY: как в `handle_io`.
        let hr = unsafe {
            (self.emu.try_mmio)(
                self.emulator,
                (&raw mut ctx).cast::<c_void>(),
                &raw const self.exit.VpContext,
                &raw const mem,
                &raw mut status,
            )
        };
        check("WHvEmulatorTryMmioEmulation", hr)?;
        emulation_result(status)
    }

    /// Гостевой физический адрес последнего MMIO-выхода.
    #[must_use]
    pub fn mmio_gpa(&self) -> u64 {
        // SAFETY: поле читается как POD; осмысленно после выхода MemoryAccess.
        unsafe { self.exit.Anonymous.MemoryAccess.Gpa }
    }

    /// Обрабатывает выход [`Exit::Cpuid`]: `f` получает `(leaf, subleaf)` и результат
    /// гипервизора по умолчанию `[eax, ebx, ecx, edx]` и возвращает итоговые значения.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn handle_cpuid(&mut self, f: impl FnOnce(u32, u32, [u32; 4]) -> [u32; 4]) -> Result<()> {
        // SAFETY: метод вызывается после выхода Cpuid, поле CpuidAccess заполнено.
        let c = unsafe { self.exit.Anonymous.CpuidAccess };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "CPUID работает с 32-битными значениями"
        )]
        let [a, b, cx, d] = f(
            c.Rax as u32,
            c.Rcx as u32,
            [
                c.DefaultResultRax as u32,
                c.DefaultResultRbx as u32,
                c.DefaultResultRcx as u32,
                c.DefaultResultRdx as u32,
            ],
        );
        let rip = self.exit.VpContext.Rip + self.instruction_length();
        self.set_registers(
            &[
                WHvX64RegisterRax,
                WHvX64RegisterRbx,
                WHvX64RegisterRcx,
                WHvX64RegisterRdx,
                WHvX64RegisterRip,
            ],
            &[
                RegisterValue {
                    Reg64: u64::from(a),
                },
                RegisterValue {
                    Reg64: u64::from(b),
                },
                RegisterValue {
                    Reg64: u64::from(cx),
                },
                RegisterValue {
                    Reg64: u64::from(d),
                },
                RegisterValue { Reg64: rip },
            ],
        )
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: эмулятор создан в `new` и больше не используется.
        let _ = unsafe { (self.emu.destroy)(self.emulator) };
        let api = self.partition.api();
        // SAFETY: VP создан в `new`; партиция жива (держим Arc).
        let hr = unsafe { (api.delete_vp)(self.partition.handle(), self.index) };
        if let Err(e) = check("WHvDeleteVirtualProcessor", hr) {
            tracing::warn!(error = %e, index = self.index, "не удалось удалить vCPU");
        }
    }
}

fn emulation_result(status: WHV_EMULATOR_STATUS) -> Result<()> {
    // SAFETY: union из u32 и битового поля, чтение u32 всегда корректно.
    let raw = unsafe { status.AsUINT32 };
    // Бит 0 — EmulationSuccessful.
    if raw & 1 != 0 {
        Ok(())
    } else {
        Err(Error::Emulation(raw))
    }
}

/// Восстанавливает контекст эмулятора из указателя колбэка.
///
/// # Safety
/// `context` — указатель на `EmuContext`, переданный в `TryIo/TryMmio` текущим вызовом.
unsafe fn ctx<'a>(context: *const c_void) -> &'a mut EmuContext<'a> {
    // SAFETY: см. контракт функции; эмулятор вызывает колбэки синхронно внутри Try*.
    unsafe { &mut *(context.cast_mut().cast::<EmuContext<'a>>()) }
}

unsafe extern "system" fn cb_io_port(
    context: *const c_void,
    access: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: контекст передан эмулятором и валиден на время вызова.
    let ctx = unsafe { ctx(context) };
    // SAFETY: структура доступа передана эмулятором и валидна на время вызова.
    let access = unsafe { &mut *access };
    let size = usize::from(access.AccessSize).min(4);
    let mut bytes = access.Data.to_le_bytes();
    let Some(buf) = bytes.get_mut(..size) else {
        return E_FAIL;
    };
    if access.Direction == 0 {
        ctx.bus.pio_read(access.Port, buf);
        access.Data = u32::from_le_bytes(bytes);
    } else {
        ctx.bus.pio_write(access.Port, buf);
    }
    S_OK
}

unsafe extern "system" fn cb_memory(
    context: *const c_void,
    access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: контекст передан эмулятором и валиден на время вызова.
    let ctx = unsafe { ctx(context) };
    // SAFETY: структура доступа передана эмулятором и валидна на время вызова.
    let access = unsafe { &mut *access };
    let size = usize::from(access.AccessSize).min(8);
    let Some(buf) = access.Data.get_mut(..size) else {
        return E_FAIL;
    };
    if access.Direction == 0 {
        ctx.bus.mmio_read(access.GpaAddress, buf);
    } else {
        ctx.bus.mmio_write(access.GpaAddress, buf);
    }
    S_OK
}

unsafe extern "system" fn cb_get_registers(
    context: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: контекст валиден; указатели и счётчик переданы эмулятором.
    let ctx = unsafe { ctx(context) };
    let api = ctx.partition.api();
    // SAFETY: прямой проброс аргументов эмулятора в WHPX.
    unsafe { (api.get_vp_registers)(ctx.partition.handle(), ctx.index, names, count, values) }
}

unsafe extern "system" fn cb_set_registers(
    context: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: контекст валиден; указатели и счётчик переданы эмулятором.
    let ctx = unsafe { ctx(context) };
    let api = ctx.partition.api();
    // SAFETY: прямой проброс аргументов эмулятора в WHPX.
    unsafe { (api.set_vp_registers)(ctx.partition.handle(), ctx.index, names, count, values) }
}

unsafe extern "system" fn cb_translate_gva(
    context: *const c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result_code: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    // SAFETY: контекст валиден; выходные указатели переданы эмулятором.
    let ctx = unsafe { ctx(context) };
    let api = ctx.partition.api();
    let mut result = WHV_TRANSLATE_GVA_RESULT::default();
    // SAFETY: выходные указатели валидны; результат пишется в локальную структуру.
    let hr = unsafe {
        (api.translate_gva)(
            ctx.partition.handle(),
            ctx.index,
            gva,
            flags,
            &raw mut result,
            gpa,
        )
    };
    // SAFETY: `result_code` валиден (передан эмулятором).
    unsafe { *result_code = result.ResultCode };
    hr
}
