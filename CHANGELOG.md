# Changelog

Все заметные изменения проекта документируются в этом файле.

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/),
версии — [Semantic Versioning](https://semver.org/lang/ru/).
Теги `vX.Y.Z` — stable, `vX.Y.Z-beta.N` — канал beta (pre-release).

## [Unreleased]

### Changed

- Образы Android по умолчанию: `aosp-android-latest-release` / `aosp_cf_x86_64_only_phone-userdebug`
  (публичных сборок Cuttlefish x86_64 с Android 15 больше нет).
- Слишком маленький `userdata.img` (заготовка Cuttlefish) заменяется пустым разделом 8 ГиБ;
  в bootconfig по умолчанию `androidboot.fstab_suffix=cf.f2fs.hctr2`,
  `androidboot.vbmeta.device_state=unlocked`, `androidboot.setupwizard_mode=DISABLED`.
- PowerShell-скрипты сохраняются в UTF-8 с BOM (иначе Windows PowerShell 5.1 ломает кириллицу).

### Added

- `scripts/hw-test.ps1`: проверка этапов 1–3 на реальном железе одним скриптом — установка
  инструментов, сборка, загрузка образов, прогоны с таймаутом, скриншоты окна, архив с логами
  ([docs/hardware-test.md](docs/hardware-test.md)).
- `--timeout СЕКУНДЫ` для `boot` и `boot-android`: ВМ останавливается сама.
- `cargo xtask guest-fetch --kernel-build latest`, `cargo xtask cf-fetch --build latest [--branch]`:
  последняя удачная сборка с ci.android.com.
- CI: проверка синтаксиса PowerShell-скриптов и UTF-8 BOM.
- Этап 3: экран и ввод.
  - virtio-gpu 2D: ресурсы, backing, transfer, scanout, flush, фенсы; EDID (CVT-RB) с частотой
    дисплея гостя 60–144+ Гц.
  - Почтовый ящик кадров с пулом буферов и пробуждением композитора по `RESOURCE_FLUSH`.
  - virtio-input: мультитач (протокол B, `INPUT_PROP_DIRECT`) и клавиатура; таблица скан-кодов
    Windows → коды Linux.
  - D3D12-композитор: flip model, waitable swapchain, задержка 1 кадр, tearing/VRR, лимит FPS,
    самый производительный адаптер.
  - Окно Win32 с DPI awareness v2, мышь как касание, клавиатура; UI отделён от потока вывода.
  - Оверлей: FPS, 1% low, время кадра, задержка flush → present, график времени кадра.
  - Экспорт `NvOptimusEnablement` / `AmdPowerXpressRequestHighPerformance`.
  - `boot`/`boot-android --display ШxВ@ГЦ --no-vsync --fps-limit --no-overlay --set КЛЮЧ=ЗНАЧЕНИЕ`.
- Этап 2: virtio и загрузка Android.
  - PCI: ECAM, ACPI MCFG, корневой мост `PCI0` в DSDT, предназначенные BAR.
  - `rustdroid-virtio`: split virtqueue (EVENT_IDX, indirect), MSI-X (маски, PBA), транспорт
    virtio-pci modern.
  - Doorbell-события WHPX: worker устройств просыпается без выхода vCPU в user mode.
  - virtio-blk с бэкендами raw, copy-on-write (`RDCOWv1`) и составным GPT-диском; virtio-console.
  - SMP (экспериментально): `--cpus N`.
  - `rustdroid boot --disk … --cpus … --console hvc`, `rustdroid boot-android --images …`:
    разбор boot/init_boot/vendor_boot v3/v4, initrd + bootconfig, CoW-оверлеи инстанса, `--reset`.
  - `cargo xtask cf-fetch` — образы Cuttlefish x86_64 с ci.android.com.
- Этап 1: минимальный VMM на WHPX.
  - `rustdroid-whpx`: динамическая загрузка WinHvPlatform/WinHvEmulation с понятными ошибками,
    партиция, отображение памяти, vCPU, регистры, инъекция прерываний, быстрый путь `in`/`out`,
    эмулятор инструкций для MMIO и строковых операций.
  - `rustdroid-vmm`: предвыделенная RAM с prefault и опциональными большими страницами, свой
    загрузчик bzImage (boot protocol 2.12+), identity-mapping и GDT, ACPI hardware-reduced
    (MADT, FADT, DSDT с COM1), IOAPIC, шина платформы, цикл vCPU со счётчиками выходов,
    привязка vCPU к производительным ядрам и MMCSS «Games».
  - `rustdroid-devices`: UART 16550 (COM1) на `vm-superio`.
  - `rustdroid check` — проверки системы; `rustdroid boot` — ВМ с консолью гостя в терминале.
  - `cargo xtask busybox-initramfs`, `cargo xtask guest-fetch` (ядро GKI с проверкой sha256).
- Решение об UI в стиле Material Design 3 (ADR 0006, docs/ui-design.md).

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
