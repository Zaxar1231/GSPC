# Changelog

Все заметные изменения проекта документируются в этом файле.

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/),
версии — [Semantic Versioning](https://semver.org/lang/ru/).
Теги `vX.Y.Z` — stable, `vX.Y.Z-beta.N` — канал beta (pre-release).

## [Unreleased]

### Added

- Этап 0: workspace из 14 крейтов (`whpx`, `vmm`, `virtio`, `devices`, `gpu`, `present`, `input`,
  `hostinput`, `bluestacks-import`, `instance`, `telemetry`, `updater`, `app`, `guest-agent`) и `xtask`.
- `rustdroid-telemetry`: окно времён кадра без аллокаций, средний FPS, 1% low, p99.
- `rustdroid-input`: координаты в долях экрана и их перевод в координаты сенсорной панели с учётом поворота.
- `rustdroid-instance`: раскладка каталогов данных (`%LOCALAPPDATA%\RustDroid`, `%APPDATA%\RustDroid`).
- `cargo xtask ci` и `cargo xtask bench-check` (порог регрессии бенчмарков).
- CI на `windows-latest` (fmt, clippy `-D warnings`, тесты, rustdoc, cargo-deny), бенчмарки на
  self-hosted раннере, релизный пайплайн, CodeQL, Dependabot.
- Документация: архитектура, roadmap, производительность, гостевой образ, форматы профилей и импорта, ADR.
- Шаблоны issues и PR, CODEOWNERS, SECURITY, CODE_OF_CONDUCT, CONTRIBUTING.
- Лицензия MIT OR Apache-2.0.

[Unreleased]: https://github.com/Zaxar1231/GSPC/commits/main
