//! Автообновление (этап 7): Velopack, подписанный фид, откат.
//!
//! - Пакеты и фиды Velopack лежат в pre-release [`FEED_URL`] (`releases.{канал}.json`, каналы
//!   `stable` и `beta`, полные и дельта-пакеты); канал берётся из установленного пакета.
//! - Фид подписан ed25519 (`releases.{канал}.json.sig`, ключ — секрет CI `UPDATE_SIGNING_KEY`):
//!   [`SignedSource`] без верной подписи фид не принимает. Пакеты Velopack сверяет по SHA256 из
//!   фида, поэтому подпись фида покрывает и их.
//! - Откат: новая версия, которая [`MAX_FAILED_STARTS`] раз подряд не продержалась до отметки
//!   «здорова» ([`Health`]), меняется на предыдущую — фид без неё и `AllowVersionDowngrade`.

use std::path::Path;
use std::sync::mpsc::Sender;

use velopack::bundle::Manifest;
use velopack::sources::{HttpSource, UpdateSource};
use velopack::{
    Error, UpdateCheck, UpdateManager, UpdateOptions, VelopackAsset, VelopackAssetFeed,
};

/// Откуда берутся обновления.
pub const FEED_URL: &str = "https://github.com/Zaxar1231/GSPC/releases/download/updates";

/// Открытый ключ ed25519 подписи фида (закрытый — секрет CI `UPDATE_SIGNING_KEY`).
pub const PUBLIC_KEY: [u8; 32] = [
    0x10, 0x07, 0xa1, 0xa6, 0x58, 0xeb, 0x99, 0x6c, 0xd6, 0x18, 0x18, 0x42, 0xec, 0xc8, 0x1e, 0xbc,
    0x85, 0x87, 0x10, 0x01, 0xac, 0x99, 0x32, 0x65, 0xe0, 0xd8, 0x9a, 0x1a, 0x8e, 0xd9, 0x09, 0xe2,
];

/// Столько неудачных запусков новой версии подряд — и она откатывается.
pub const MAX_FAILED_STARTS: u32 = 3;

/// Источник обновлений: фид проверяется по подписи, пакеты качаются как у [`HttpSource`].
#[derive(Clone)]
pub struct SignedSource {
    base: String,
    http: HttpSource,
    key: [u8; 32],
    /// Версия, которой в фиде «нет» (откат с неё).
    exclude: Option<String>,
}

impl SignedSource {
    /// Источник по адресу `base` с открытым ключом `key`; `exclude` — версия для отката.
    #[must_use]
    pub fn new(base: &str, key: [u8; 32], exclude: Option<String>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http: HttpSource::new(base),
            key,
            exclude,
        }
    }

    /// Скачивает файл фида; при обрыве соединения — ещё до двух попыток.
    fn fetch(&self, name: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}/{name}", self.base);
        let mut last = None;
        for attempt in 0..3_u64 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(attempt));
            }
            match ureq::get(&url)
                .call()
                .and_then(|mut r| r.body_mut().read_to_vec())
            {
                Ok(body) => return Ok(body),
                Err(e) => last = Some(e),
            }
        }
        Err(last.map_or_else(|| Error::Other(url), Error::from))
    }
}

/// Проверяет подпись `sig` текста фида `feed` и разбирает его, убирая версию `exclude`.
///
/// # Errors
/// Подпись неверна или фид не разбирается.
pub fn verify_feed(
    feed: &[u8],
    sig: &[u8],
    key: &[u8; 32],
    exclude: Option<&str>,
) -> Result<VelopackAssetFeed, Error> {
    let pk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key);
    pk.verify(feed, sig)
        .map_err(|_| Error::Other("подпись фида обновлений неверна".into()))?;
    let mut parsed: VelopackAssetFeed = serde_json::from_slice(feed)?;
    if let Some(v) = exclude {
        parsed.Assets.retain(|a| a.Version != v);
    }
    Ok(parsed)
}

impl UpdateSource for SignedSource {
    fn get_release_feed(
        &self,
        channel: &str,
        _app: &Manifest,
        _staged_user_id: &str,
    ) -> Result<VelopackAssetFeed, Error> {
        let name = format!("releases.{channel}.json");
        let feed = self.fetch(&name)?;
        let sig = self.fetch(&format!("{name}.sig"))?;
        verify_feed(&feed, &sig, &self.key, self.exclude.as_deref())
    }

    fn download_release_entry(
        &self,
        asset: &VelopackAsset,
        local_file: &Path,
        progress_sender: Option<Sender<i16>>,
    ) -> Result<(), Error> {
        self.http
            .download_release_entry(asset, local_file, progress_sender)
    }
}

/// Менеджер обновлений установленной копии (`None` — портативная или запуск из сборки).
#[must_use]
pub fn manager(rollback_from: Option<String>) -> Option<UpdateManager> {
    let downgrade = rollback_from.is_some();
    let source = SignedSource::new(FEED_URL, PUBLIC_KEY, rollback_from);
    let options = UpdateOptions {
        AllowVersionDowngrade: downgrade,
        ..UpdateOptions::default()
    };
    UpdateManager::new(source, Some(options), None).ok()
}

/// Что нашлось при проверке.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Обновлений нет.
    UpToDate,
    /// Есть версия.
    Version(String),
}

/// Проверяет и, если `download`, скачивает обновление (применится при следующем запуске).
///
/// # Errors
/// Сеть, подпись фида, пакет.
pub fn check(um: &UpdateManager, download: bool) -> Result<Found, Error> {
    match um.check_for_updates()? {
        UpdateCheck::RemoteIsEmpty | UpdateCheck::NoUpdateAvailable => Ok(Found::UpToDate),
        UpdateCheck::UpdateAvailable(info) => {
            if download {
                um.download_updates(&info, None)?;
            }
            Ok(Found::Version(info.TargetFullRelease.Version.clone()))
        }
    }
}

/// Скачивает обновление (или откат) и перезапускает программу с ним.
///
/// # Errors
/// Сеть, подпись фида, пакет, запуск установщика.
pub fn apply(um: &UpdateManager) -> Result<Found, Error> {
    match um.check_for_updates()? {
        UpdateCheck::RemoteIsEmpty | UpdateCheck::NoUpdateAvailable => Ok(Found::UpToDate),
        UpdateCheck::UpdateAvailable(info) => {
            um.download_updates(&info, None)?;
            um.apply_updates_and_restart(&*info)?;
            Ok(Found::Version(info.TargetFullRelease.Version.clone()))
        }
    }
}

/// «Здоровье» установленной версии: сколько запусков подряд не дошли до отметки «здорова».
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Health {
    /// Версия.
    pub version: String,
    /// Запуски без отметки «здорова».
    pub failed_starts: u32,
    /// Версия хотя бы раз отработала нормально.
    pub healthy: bool,
}

impl Health {
    /// Разбирает файл здоровья (`версия`, `запуски`, `здорова` по строкам); мусор — пусто.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut l = text.lines();
        Self {
            version: l.next().unwrap_or_default().trim().to_owned(),
            failed_starts: l.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            healthy: l.next().is_some_and(|s| s.trim() == "1"),
        }
    }

    /// Текст файла.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{}\n{}\n{}\n",
            self.version,
            self.failed_starts,
            u8::from(self.healthy)
        )
    }

    /// Новый запуск версии `current`: учёт и решение, откатывать ли её.
    #[must_use]
    pub fn on_start(mut self, current: &str) -> (Self, bool) {
        if self.version != current {
            self = Self {
                version: current.to_owned(),
                ..Self::default()
            };
        }
        if self.healthy {
            return (self, false);
        }
        self.failed_starts += 1;
        let rollback = self.failed_starts > MAX_FAILED_STARTS;
        (self, rollback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Тестовый ключ RFC 8032 (тест 1): пустое сообщение.
    const RFC_PK: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const RFC_SIG: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn signature_is_checked_before_parsing() {
        let key: [u8; 32] = hex(RFC_PK).try_into().unwrap();
        // Подпись верна, но пустое сообщение — не JSON: ошибка разбора, а не подписи.
        let err = verify_feed(b"", &hex(RFC_SIG), &key, None).unwrap_err();
        assert!(matches!(err, Error::Json(_)), "{err:?}");
        // Чужой текст с той же подписью — отказ.
        let err = verify_feed(b"{\"Assets\":[]}", &hex(RFC_SIG), &key, None).unwrap_err();
        assert!(matches!(err, Error::Other(_)), "{err:?}");
    }

    #[test]
    fn health_counts_failed_starts_and_resets_on_new_version() {
        let (h, rb) = Health::default().on_start("1.1.0");
        assert_eq!((h.failed_starts, rb), (1, false));
        let (h, _) = h.on_start("1.1.0");
        let (h, rb) = h.on_start("1.1.0");
        assert_eq!((h.failed_starts, rb), (3, false));
        let (h, rb) = h.on_start("1.1.0");
        assert!(rb, "четвёртый запуск без отметки — откат");
        assert_eq!(Health::parse(&h.render()), h);
        // Здоровая версия не откатывается, новая версия считается заново.
        let healthy = Health {
            healthy: true,
            ..h.clone()
        };
        assert_eq!(healthy.clone().on_start("1.1.0"), (healthy, false));
        assert_eq!(h.on_start("1.2.0").0.failed_starts, 1);
        assert_eq!(
            Health::parse("мусор"),
            Health {
                version: "мусор".into(),
                ..Health::default()
            }
        );
    }
}
