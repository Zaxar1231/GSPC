# Changelog

Все заметные изменения проекта документируются в этом файле.

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/),
версии — [Semantic Versioning](https://semver.org/lang/ru/).
Теги `vX.Y.Z` — stable, `vX.Y.Z-beta.N` — канал beta (pre-release).

## [Unreleased]

### Fixed

По результатам первого запуска на реальном железе (Core i5-8300H, Windows 11 26200):

- Этап 1: вывод программ гостя не доходил до консоли. В GKI 8250 по умолчанию не
  регистрирует порты, `/dev/console` указывал на `ttynull`; в базовую cmdline добавлен
  `8250.nr_uarts=1`.
- Этап 2: Android падал в первом этапе init («invalid geometry magic»): `super.img` и
  `userdata.img` в архивах Cuttlefish — Android sparse. Добавлен `SparseDisk`: чтение sparse-образов
  без распаковки, индекс чанков строится за доли миллисекунды.
- Окно до первого кадра гостя было белым, теперь чёрное.
- system_server падал каждые ~13 секунд: `PersistentDataBlockService` (защита от сброса, FRP)
  не находила раздел `/dev/block/by-name/frp`. На диск добавлены пустой `frp` (1 МиБ) и
  `custom` из `cuttlefish_example_custom.img`.
- system_server убивал собственный Watchdog через 66 секунд: при создании `SensorManager`
  он ждал HAL датчиков, а тот — ответа хоста на `list-sensors` по `hvc18`.
- Android доходил до загрузочной анимации, но не загружался: HAL Cuttlefish (подсветка, датчики,
  oemlock, UWB, Thread, BT, NFC, RIL) завершались без своих каналов `/dev/hvcN`, vsock и
  параметров `ro.boot.*`, и после четырёх падений до конца загрузки init перезапускал zygote.
- Android 17 перезагружался с `bootstrap-apexd-failed`: у Cuttlefish часть HAL поставляется
  несколькими вариантами одного APEX, и выбор задаёт `launch_cvd`. Теперь bootconfig по
  умолчанию выбирает `gatekeeper.nonsecure`, `keymint.rust_nonsecure`, `composer.ranchu` и
  базовый HAL камеры (`androidboot.vendor.apex.*`). В режиме окна добавлены
  `androidboot.cpuvulkan.version` и `androidboot.lcd_density`.

### Added

- Оверлей и лог `кадры гостя`: рабочий набор памяти эмулятора (`RAM 4.3 GB`), загрузка vCPU
  гостя по счётчикам гипервизора (`VCPU 87%`, `WHvGetVirtualProcessorCounters`), загрузка CPU
  потоками эмулятора и всей системой (`CPU 38% ALL 61%`), обновление раз в секунду. Время
  исполнения кода гостя Windows не засчитывает ни процессу, ни системе, поэтому реальную
  нагрузку от Android показывает только `VCPU`.
- Службы хоста на консолях Cuttlefish (`cf_services`): управляющий канал HAL датчиков
  (`hvc18`, ответ на `list-sensors`) и HAL oemlock (`hvc10`). Транспорт Cuttlefish —
  заголовок «команда + длина» и данные.
- virtio-vsock (CID гостя 3). Хост пока принимает подключения гостя и молча читает данные:
  этого хватает HAL Cuttlefish (подсветка, модем RIL), настоящие сервисы хоста — этап 5.
- Android: 20 консолей virtio как у Cuttlefish (`hvc0`…`hvc19`); консоль ядра и шелл — `hvc0`
  вместо COM1 (вывод пачками, без выхода vCPU на каждый байт), logcat из `hvc2` пишется в файл
  (`--logcat`, по умолчанию `logcat.txt` в каталоге инстанса).
- bootconfig: `vsock_lights_port`, `modem_simulator_ports`, `openthread_node_id`,
  `wifi_mac_prefix`; в режиме окна `androidboot.qemu.vsync` = частота дисплея гостя.
- CMOS RTC (порты `0x70`/`0x71`): гость получает текущее время, ядро больше не опрашивает
  несуществующие часы (около 140 тысяч выходов vCPU за загрузку).
- `cf-fetch` не распаковывает образы повторно, если они уже на месте.

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
