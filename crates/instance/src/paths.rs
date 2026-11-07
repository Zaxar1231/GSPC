//! Каталоги данных.
//!
//! - `%LOCALAPPDATA%\RustDroid` — тяжёлые и машинно-зависимые данные: инстансы, образы,
//!   логи, кэш шейдеров, дампы;
//! - `%APPDATA%\RustDroid` — настройки и профили управления (роуминг-профиль).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Имя каталога продукта.
pub const PRODUCT_DIR: &str = "RustDroid";

/// Ошибки определения каталогов.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathsError {
    /// Переменная окружения не задана или пуста.
    #[error("переменная окружения {0} не задана")]
    MissingVar(&'static str),
    /// Путь в переменной окружения не абсолютный.
    #[error("переменная окружения {0} содержит неабсолютный путь")]
    NotAbsolute(&'static str),
}

/// Корневые каталоги данных RustDroid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    local: PathBuf,
    roaming: PathBuf,
}

impl DataDirs {
    /// Определяет каталоги по `%LOCALAPPDATA%` и `%APPDATA%` текущего пользователя.
    ///
    /// # Errors
    /// Если переменные не заданы или содержат неабсолютные пути.
    pub fn from_env() -> Result<Self, PathsError> {
        Self::from_vars(
            std::env::var_os("LOCALAPPDATA"),
            std::env::var_os("APPDATA"),
        )
    }

    /// Определяет каталоги по явно переданным значениям `LOCALAPPDATA` и `APPDATA`.
    ///
    /// # Errors
    /// Если значение отсутствует, пустое или не является абсолютным путём.
    pub fn from_vars(
        local_app_data: Option<OsString>,
        app_data: Option<OsString>,
    ) -> Result<Self, PathsError> {
        let local = base(local_app_data, "LOCALAPPDATA")?.join(PRODUCT_DIR);
        let roaming = base(app_data, "APPDATA")?.join(PRODUCT_DIR);
        Ok(Self { local, roaming })
    }

    /// `%LOCALAPPDATA%\RustDroid`.
    #[must_use]
    pub fn local_root(&self) -> &Path {
        &self.local
    }

    /// `%APPDATA%\RustDroid`.
    #[must_use]
    pub fn roaming_root(&self) -> &Path {
        &self.roaming
    }

    /// Инстансы (диски, конфиги, снапшоты).
    #[must_use]
    pub fn instances(&self) -> PathBuf {
        self.local.join("instances")
    }

    /// Скачанные образы Android.
    #[must_use]
    pub fn images(&self) -> PathBuf {
        self.local.join("images")
    }

    /// Логи `tracing` с ротацией.
    #[must_use]
    pub fn logs(&self) -> PathBuf {
        self.local.join("logs")
    }

    /// Minidump-файлы сбоев.
    #[must_use]
    pub fn crashes(&self) -> PathBuf {
        self.local.join("crashes")
    }

    /// Постоянный кэш шейдеров и пайплайнов.
    #[must_use]
    pub fn shader_cache(&self) -> PathBuf {
        self.local.join("shader-cache")
    }

    /// Профили управления.
    #[must_use]
    pub fn profiles(&self) -> PathBuf {
        self.roaming.join("profiles")
    }

    /// Файл глобальных настроек приложения.
    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.roaming.join("settings.json")
    }
}

fn base(value: Option<OsString>, name: &'static str) -> Result<PathBuf, PathsError> {
    let value = value
        .filter(|v| !v.is_empty())
        .ok_or(PathsError::MissingVar(name))?;
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(PathsError::NotAbsolute(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    const LOCAL: &str = r"C:\Users\u\AppData\Local";
    #[cfg(windows)]
    const ROAMING: &str = r"C:\Users\u\AppData\Roaming";
    #[cfg(not(windows))]
    const LOCAL: &str = "/home/u/local";
    #[cfg(not(windows))]
    const ROAMING: &str = "/home/u/roaming";

    fn dirs() -> DataDirs {
        DataDirs::from_vars(Some(LOCAL.into()), Some(ROAMING.into())).unwrap()
    }

    #[test]
    fn layout() {
        let d = dirs();
        assert_eq!(d.local_root(), Path::new(LOCAL).join("RustDroid"));
        assert_eq!(
            d.instances(),
            Path::new(LOCAL).join("RustDroid").join("instances")
        );
        assert_eq!(
            d.shader_cache(),
            Path::new(LOCAL).join("RustDroid").join("shader-cache")
        );
        assert_eq!(
            d.profiles(),
            Path::new(ROAMING).join("RustDroid").join("profiles")
        );
        assert_eq!(
            d.settings_file(),
            Path::new(ROAMING).join("RustDroid").join("settings.json")
        );
    }

    #[test]
    fn missing_or_empty_var() {
        assert_eq!(
            DataDirs::from_vars(None, Some(ROAMING.into())),
            Err(PathsError::MissingVar("LOCALAPPDATA"))
        );
        assert_eq!(
            DataDirs::from_vars(Some(LOCAL.into()), Some(OsString::new())),
            Err(PathsError::MissingVar("APPDATA"))
        );
    }

    #[test]
    fn relative_path_rejected() {
        assert_eq!(
            DataDirs::from_vars(Some("relative".into()), Some(ROAMING.into())),
            Err(PathsError::NotAbsolute("LOCALAPPDATA"))
        );
    }
}
