//! Набор профилей и выбор по активному приложению (автопереключение).
//!
//! Профиль привязан к пакету (`Profile::package`). Когда guest-agent сообщает пакет на переднем
//! плане, [`ProfileBook::for_package`] отдаёт подходящий профиль или профиль по умолчанию.
//! Сам приём событий переднего плана — в хостовом слое (этап 5.6, после проверки агента).

use crate::profile::Profile;

/// Профили, которые можно переключать между собой.
#[derive(Debug, Default, Clone)]
pub struct ProfileBook {
    profiles: Vec<Profile>,
    /// Индекс профиля по умолчанию (без пакета или для неизвестного приложения).
    default: Option<usize>,
}

impl ProfileBook {
    /// Пустой набор.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавляет профиль. Профиль без `package` становится профилем по умолчанию, если его ещё нет.
    pub fn add(&mut self, profile: Profile) {
        let no_package = profile.package.is_none();
        self.profiles.push(profile);
        if no_package && self.default.is_none() {
            self.default = Some(self.profiles.len() - 1);
        }
    }

    /// Профиль для пакета `package` (первый подходящий), иначе по умолчанию.
    #[must_use]
    pub fn for_package(&self, package: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.package.as_deref() == Some(package))
            .or_else(|| self.default.and_then(|i| self.profiles.get(i)))
    }

    /// Число профилей.
    #[must_use]
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Пусто ли.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(package: Option<&str>, name: &str) -> Profile {
        let pkg = package.map_or(String::new(), |p| format!(r#","package":"{p}""#));
        Profile::from_json(&format!(
            r#"{{"version":1,"name":"{name}"{pkg},"controls":[]}}"#
        ))
        .expect("профиль")
    }

    #[test]
    fn picks_by_package_then_default() {
        let mut b = ProfileBook::new();
        assert!(b.is_empty());
        b.add(profile(None, "общий"));
        b.add(profile(Some("com.game.a"), "A"));
        assert_eq!(b.len(), 2);
        assert_eq!(
            b.for_package("com.game.a").map(|p| p.name.as_str()),
            Some("A")
        );
        assert_eq!(
            b.for_package("com.other").map(|p| p.name.as_str()),
            Some("общий")
        );
    }

    #[test]
    fn no_default_means_none() {
        let mut b = ProfileBook::new();
        b.add(profile(Some("com.game.a"), "A"));
        assert!(b.for_package("com.other").is_none());
    }
}
