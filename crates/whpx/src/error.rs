//! Ошибки обёртки WHPX.

/// Результат операций WHPX.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Ошибки работы с Windows Hypervisor Platform.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Не загружается системная библиотека WHPX.
    #[error(
        "не найдена системная библиотека {0}. Включите компонент Windows «Платформа \
         низкоуровневой оболочки Windows» (Windows Hypervisor Platform) и перезагрузите компьютер"
    )]
    LibraryNotFound(&'static str),

    /// В библиотеке нет нужной функции (слишком старая сборка Windows).
    #[error("в {library} нет функции {function}. Обновите Windows 11 до актуальной сборки")]
    MissingFunction {
        /// Имя библиотеки.
        library: &'static str,
        /// Имя функции.
        function: &'static str,
    },

    /// Гипервизор не запущен (виртуализация выключена в UEFI или Hyper-V не стартовал).
    #[error(
        "гипервизор Windows не запущен. Проверьте, что виртуализация (Intel VT-x / AMD SVM) \
         включена в UEFI, а компонент «Платформа низкоуровневой оболочки Windows» включён"
    )]
    HypervisorNotPresent,

    /// Вызов WHPX вернул ошибку HRESULT.
    #[error("{call} завершился с ошибкой HRESULT 0x{hresult:08X}")]
    Call {
        /// Имя вызова.
        call: &'static str,
        /// Код HRESULT.
        hresult: u32,
    },

    /// Эмулятор инструкций не смог выполнить доступ.
    #[error("эмуляция инструкции не удалась, статус 0x{0:08X}")]
    Emulation(u32),

    /// Неверный аргумент.
    #[error("неверный аргумент: {0}")]
    InvalidArgument(&'static str),
}

#[cfg(windows)]
pub(crate) fn check(call: &'static str, hr: windows::core::HRESULT) -> Result<()> {
    if hr.is_ok() {
        Ok(())
    } else {
        #[allow(
            clippy::cast_sign_loss,
            reason = "HRESULT печатается как беззнаковый код"
        )]
        let hresult = hr.0 as u32;
        Err(Error::Call { call, hresult })
    }
}
