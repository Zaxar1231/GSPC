//! Партиция WHPX: создание, свойства, память, прерывания.

use std::ffi::c_void;
use std::sync::Arc;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Hypervisor::{
    WHV_DOORBELL_MATCH_DATA, WHV_INTERRUPT_CONTROL, WHV_MAP_GPA_RANGE_FLAGS, WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE, WHvPartitionPropertyCodeCpuidExitList,
    WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeProcessorCount,
    WHvX64LocalApicEmulationModeXApic,
};

use crate::api::{self, Platform};
use crate::error::{Error, Result, check};
use crate::vcpu::Vcpu;

/// Флаги отображения гостевой памяти.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapFlags(i32);

impl MapFlags {
    /// Чтение.
    pub const READ: Self = Self(1);
    /// Запись.
    pub const WRITE: Self = Self(2);
    /// Исполнение.
    pub const EXECUTE: Self = Self(4);
    /// Отслеживание грязных страниц (для снапшотов).
    pub const TRACK_DIRTY: Self = Self(8);
    /// Обычная RAM: чтение, запись, исполнение.
    pub const RAM: Self = Self(1 | 2 | 4);

    /// Объединение флагов.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Тип доставки прерывания.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptType {
    /// Обычное (fixed).
    Fixed,
    /// Lowest priority.
    LowestPriority,
    /// NMI.
    Nmi,
}

/// Режим адресации получателя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationMode {
    /// Физический APIC ID.
    Physical,
    /// Логический.
    Logical,
}

/// Режим срабатывания.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    /// По фронту.
    Edge,
    /// По уровню.
    Level,
}

/// Запрос на инъекцию прерывания в LAPIC гостя (MSI, IOAPIC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupt {
    /// Тип доставки.
    pub kind: InterruptType,
    /// Режим адресации.
    pub destination_mode: DestinationMode,
    /// Режим срабатывания.
    pub trigger: TriggerMode,
    /// APIC ID (или логическая маска).
    pub destination: u32,
    /// Вектор.
    pub vector: u32,
}

impl Interrupt {
    fn to_raw(self) -> WHV_INTERRUPT_CONTROL {
        let kind: u64 = match self.kind {
            InterruptType::Fixed => 0,
            InterruptType::LowestPriority => 1,
            InterruptType::Nmi => 4,
        };
        let mode: u64 = match self.destination_mode {
            DestinationMode::Physical => 0,
            DestinationMode::Logical => 1,
        };
        let trigger: u64 = match self.trigger {
            TriggerMode::Edge => 0,
            TriggerMode::Level => 1,
        };
        // Битовые поля: Type:8, DestinationMode:4, TriggerMode:4.
        WHV_INTERRUPT_CONTROL {
            _bitfield: kind | (mode << 8) | (trigger << 12),
            Destination: self.destination,
            Vector: self.vector,
        }
    }
}

/// Параметры партиции.
#[derive(Debug, Clone, Default)]
pub struct PartitionConfig {
    /// Число виртуальных процессоров.
    pub processor_count: u32,
    /// Листы CPUID, для которых нужен выход в user mode.
    pub cpuid_exits: Vec<u32>,
}

/// Партиция WHPX. Разделяется между потоками vCPU и устройств через [`Arc`].
pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
    api: &'static Platform,
}

// SAFETY: дескриптор партиции — непрозрачное значение; WHPX допускает вызовы с одной
// партицией из разных потоков (запуск конкретного VP — только из одного потока, это
// обеспечивает `Vcpu`, который не является `Sync`).
unsafe impl Send for Partition {}
// SAFETY: см. выше; все методы `&self` — потокобезопасные вызовы WHPX.
unsafe impl Sync for Partition {}

impl std::fmt::Debug for Partition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Partition")
            .field("handle", &self.handle.0)
            .finish()
    }
}

impl Partition {
    /// Создаёт и настраивает партицию.
    ///
    /// # Errors
    /// Ошибки WHPX при создании или настройке.
    pub fn new(config: &PartitionConfig) -> Result<Arc<Self>> {
        if config.processor_count == 0 {
            return Err(Error::InvalidArgument("processor_count = 0"));
        }
        let api = api::platform()?;
        let mut handle = WHV_PARTITION_HANDLE::default();
        // SAFETY: выходной указатель на локальную переменную.
        check("WHvCreatePartition", unsafe {
            (api.create_partition)(&raw mut handle)
        })?;
        // С этого момента Drop удалит партицию при любой ошибке.
        let partition = Self { handle, api };

        partition.set_property(
            WHvPartitionPropertyCodeProcessorCount,
            &config.processor_count,
            "SetPartitionProperty(ProcessorCount)",
        )?;
        // LAPIC и его таймер эмулирует гипервизор: без выходов в user mode на каждое
        // обращение к APIC.
        partition.set_property(
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            &WHvX64LocalApicEmulationModeXApic,
            "SetPartitionProperty(LocalApicEmulationMode)",
        )?;
        if !config.cpuid_exits.is_empty() {
            partition.set_property_slice(
                WHvPartitionPropertyCodeCpuidExitList,
                &config.cpuid_exits,
                "SetPartitionProperty(CpuidExitList)",
            )?;
        }
        // SAFETY: дескриптор валиден.
        check("WHvSetupPartition", unsafe {
            (api.setup_partition)(partition.handle)
        })?;
        Ok(Arc::new(partition))
    }

    fn set_property<T>(
        &self,
        code: WHV_PARTITION_PROPERTY_CODE,
        value: &T,
        call: &'static str,
    ) -> Result<()> {
        self.set_property_slice(code, std::slice::from_ref(value), call)
    }

    fn set_property_slice<T>(
        &self,
        code: WHV_PARTITION_PROPERTY_CODE,
        values: &[T],
        call: &'static str,
    ) -> Result<()> {
        let size = u32::try_from(size_of_val(values))
            .map_err(|_| Error::InvalidArgument("свойство слишком большое"))?;
        // SAFETY: буфер валиден и имеет указанный размер на время вызова; тип значения
        // соответствует коду свойства (обеспечивают вызывающие в этом модуле).
        let hr = unsafe {
            (self.api.set_partition_property)(
                self.handle,
                code,
                values.as_ptr().cast::<c_void>(),
                size,
            )
        };
        check(call, hr)
    }

    /// Отображает область памяти хоста в гостевые физические адреса.
    ///
    /// # Errors
    /// Ошибка WHPX (например, перекрытие с существующим отображением).
    ///
    /// # Safety
    /// `host` должен указывать на выделенную и закоммиченную память размером не меньше `size`,
    /// которая живёт дольше отображения (до [`Partition::unmap_gpa_range`] или удаления партиции).
    pub unsafe fn map_gpa_range(
        &self,
        host: *const u8,
        gpa: u64,
        size: u64,
        flags: MapFlags,
    ) -> Result<()> {
        // SAFETY: инварианты памяти гарантирует вызывающий.
        let hr = unsafe {
            (self.api.map_gpa_range)(
                self.handle,
                host.cast::<c_void>(),
                gpa,
                size,
                WHV_MAP_GPA_RANGE_FLAGS(flags.0),
            )
        };
        check("WHvMapGpaRange", hr)
    }

    /// Снимает отображение гостевой памяти.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn unmap_gpa_range(&self, gpa: u64, size: u64) -> Result<()> {
        // SAFETY: дескриптор валиден; снятие отображения не обращается к памяти хоста.
        check("WHvUnmapGpaRange", unsafe {
            (self.api.unmap_gpa_range)(self.handle, gpa, size)
        })
    }

    /// Инъекция прерывания (доставляется встроенным LAPIC гипервизора). Можно вызывать из
    /// любого потока, в том числе пока vCPU исполняется.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn request_interrupt(&self, interrupt: Interrupt) -> Result<()> {
        let raw = interrupt.to_raw();
        #[allow(clippy::cast_possible_truncation, reason = "структура — 16 байт")]
        let size = size_of::<WHV_INTERRUPT_CONTROL>() as u32;
        // SAFETY: указатель на локальную структуру нужного размера.
        check("WHvRequestInterrupt", unsafe {
            (self.api.request_interrupt)(self.handle, &raw const raw, size)
        })
    }

    /// Прерывает текущий `WHvRunVirtualProcessor` для vCPU `index` (из другого потока).
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn cancel_run(&self, index: u32) -> Result<()> {
        // SAFETY: дескриптор валиден.
        check("WHvCancelRunVirtualProcessor", unsafe {
            (self.api.cancel_run_vp)(self.handle, index, 0)
        })
    }

    /// Регистрирует doorbell: запись гостя по `gpa` сигналит `event` без выхода vCPU в user
    /// mode. Сопоставление идёт по адресу, при любой длине и значении записи.
    ///
    /// # Errors
    /// Ошибка WHPX (например, адрес уже зарегистрирован).
    pub fn register_doorbell(&self, gpa: u64, event: HANDLE) -> Result<()> {
        let m = doorbell(gpa);
        // SAFETY: структура локальная; событие принадлежит вызывающему и должно жить до
        // `unregister_doorbell` (обеспечивает владелец события).
        check("WHvRegisterPartitionDoorbellEvent", unsafe {
            (self.api.register_doorbell)(self.handle, &raw const m, event)
        })
    }

    /// Снимает doorbell.
    ///
    /// # Errors
    /// Ошибка WHPX.
    pub fn unregister_doorbell(&self, gpa: u64) -> Result<()> {
        let m = doorbell(gpa);
        // SAFETY: структура локальная, дескриптор партиции валиден.
        check("WHvUnregisterPartitionDoorbellEvent", unsafe {
            (self.api.unregister_doorbell)(self.handle, &raw const m)
        })
    }

    /// Создаёт виртуальный процессор.
    ///
    /// # Errors
    /// Ошибка WHPX или эмулятора инструкций.
    pub fn create_vcpu(self: &Arc<Self>, index: u32) -> Result<Vcpu> {
        Vcpu::new(Arc::clone(self), index)
    }

    pub(crate) fn handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }

    pub(crate) fn api(&self) -> &'static Platform {
        self.api
    }
}

fn doorbell(gpa: u64) -> WHV_DOORBELL_MATCH_DATA {
    // Битовые поля MatchOnValue:1, MatchOnLength:1 — оба выключены.
    WHV_DOORBELL_MATCH_DATA {
        GuestAddress: gpa,
        Value: 0,
        Length: 0,
        _bitfield: 0,
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: дескриптор валиден и больше не используется (все Vcpu держат Arc и уже удалены).
        let hr = unsafe { (self.api.delete_partition)(self.handle) };
        if let Err(e) = check("WHvDeletePartition", hr) {
            tracing::warn!(error = %e, "не удалось удалить партицию");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_bitfield_layout() {
        let raw = Interrupt {
            kind: InterruptType::Nmi,
            destination_mode: DestinationMode::Logical,
            trigger: TriggerMode::Level,
            destination: 3,
            vector: 0x30,
        }
        .to_raw();
        assert_eq!(raw._bitfield, 4 | (1 << 8) | (1 << 12));
        assert_eq!((raw.Destination, raw.Vector), (3, 0x30));
    }
}
