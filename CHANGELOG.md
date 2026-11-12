# Changelog

Все заметные изменения проекта документируются в этом файле.

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.1.0/),
версии — [Semantic Versioning](https://semver.org/lang/ru/).
Теги `vX.Y.Z` — stable, `vX.Y.Z-beta.N` — канал beta (pre-release).

## [Unreleased]

### Added

- `gfxstream_backend.dll` собирается в GitHub Actions из зафиксированного коммита
  `google/gfxstream` (ClangCL, патчи в `third_party/gfxstream-patches`) и публикуется в
  pre-release `deps-gfxstream`.
- virtio-gpu 3D (задача 4.3): capsets, контексты с `CONTEXT_INIT`, `RESOURCE_CREATE_3D`,
  передачи 3D, `SUBMIT_3D`, blob-ресурсы и `SET_SCANOUT_BLOB`. Команды с фенсом возвращаются
  гостю только после выполнения фенса (по кольцам контекстов и глобальной шкале). Исполнитель —
  трейт `Backend3d`; реализация на gfxstream (`rustdroid_gpu::gfxstream`, Windows).
  В эмулятор пока не подключено (задача 4.4).
- Общая память virtio-gpu (задача 4.3b): 64-битный BAR2 с capability shared memory в окне
  32–40 ГиБ (`QWordMemory` в `_CRS`), `RESOURCE_MAP_BLOB`/`UNMAP_BLOB` через
  `WHvMapGpaRange` — host-visible blob-ресурсы для Vulkan гостя.
- `boot-android --gpu gfxstream` (задача 4.4): virtio-gpu с 3D на gfxstream и параметры гостя
  как `gfxstream_guest_angle` у Cuttlefish. По умолчанию `--gpu software`. `hw-test.ps1 -Gpu`
  (`auto` — gfxstream, если его прошла проверка 3D).
- Отчёт о падении: фильтр необработанных исключений пишет в stderr код, модуль и смещение
  инструкции, адрес возврата и адреса из стека; CI публикует `rustdroid.pdb` и
  `gfxstream_backend.pdb`, по которым эти адреса переводятся в функции.
- Диагностика 3D: команды virtio-gpu в логе (`-v`), сторож пишет, какая команда не завершается
  дольше 3 с (поток устройства ждёт gfxstream); `hw-test.ps1` сохраняет код выхода этапа 3 и
  запись о сбое процесса из журнала Windows (`stage3-crash.txt`).
- `rustdroid check` проверяет 3D: видеокарты и версию Vulkan (через `vulkan-1.dll`) и запуск
  gfxstream на Vulkan. Проверка идёт в дочернем процессе `rustdroid gpu-probe`, чтобы падение
  драйвера не роняло `check`; отсутствие 3D — предупреждение, а не ошибка. Если проверка
  не уложилась в 45 с, она снимается, и в итоге указан шаг, на котором она зависла; teardown
  gfxstream и выгрузка драйвера в проверке не выполняются.
- `hw-test.ps1` кладёт `gfxstream_backend.dll` рядом с `rustdroid.exe`, запускает
  `rustdroid -v check` и выносит итог по 3D в `summary.txt`; `-CheckOnly` — только проверка,
  без образов и Android.

### Changed

- Этап 4 начат ([docs/stage-4.md](docs/stage-4.md)).
- Программное выполнение `MOVBE`/AES-NI читает из WHPX только нужные регистры (регистры адреса и
  данных, FS/GS при префиксе; RIP — из контекста выхода; оба XMM одним вызовом) вместо 19 при
  каждой инструкции.

### Fixed

- Ошибка `WHvMapGpaRange … 0x800705AA` (Windows не хватило ресурсов под память гостя) теперь
  объясняется в сообщении: закрыть тяжёлые программы или уменьшить `--memory`.
- `hw-test.ps1`: этап 3 без этапа 2 запускался на старом инстансе `hwtest` без `--reset`. После
  аварийно завершённых прогонов на `/metadata/aconfig` оставались пустые файлы флагов, и
  system_server падал по кругу с `Invalid feature flag`. Теперь этап 3 сбрасывает инстанс, если
  этап 2 не запускался.
- gfxstream падал при запуске на Intel UHD (драйвер Intel под Windows): пул дескрипторов
  Vulkan-композитора был меньше нужного (`VK_ERROR_OUT_OF_POOL_MEMORY`), патч
  `0002-compositor-descriptor-pool-size.patch`.
- gfxstream падал по адресу 0 на драйверах Vulkan 1.2, когда Android вызывал `vkQueueSubmit2`:
  функции ядра Vulkan 1.3 у таких драйверов есть только под именами расширений (`…KHR`,
  `…EXT`), патч `0003-dispatch-core-1-3-aliases.patch` подставляет их.
- Тот же сбой на драйвере Intel UHD 27.20.100.8984: в нём нет VK_KHR_synchronization2 вообще
  (ни `vkQueueSubmit2`, ни `vkQueueSubmit2KHR`), а гостевой драйвер всегда отправляет
  `VkSubmitInfo2`. Патч `0004-queue-submit2-fallback.patch` переводит такую отправку в обычный
  `vkQueueSubmit` (семафоры, стадии ожидания, значения timeline-семафоров).
- Команды synchronization2 в буферах команд (`vkCmdPipelineBarrier2`, `vkCmdWaitEvents2`,
  `vkCmdSetEvent2`, `vkCmdResetEvent2`, `vkCmdWriteTimestamp2`) на таком драйвере тоже вели бы
  в пустой указатель; патч `0005-sync2-command-shims.patch` подставляет переходники на обычные
  команды Vulkan 1.0.
- Драйвер Intel UHD 27.20.100.x падал внутри `vkCmdBindVertexBuffers2EXT` (чтение по адресу 0),
  когда ANGLE в госте включал `VK_EXT_extended_dynamic_state`: гостевой драйвер шлёт команды под
  core-1.3 именами, а декодер отдаёт их EXT-функциям драйвера. Патч
  `0006-hide-extended-dynamic-state.patch` скрывает от гостя `VK_EXT_extended_dynamic_state` и
  `VK_EXT_extended_dynamic_state2`, если драйвер хоста младше Vulkan 1.3; ANGLE работает без них.

- KeyMint падал с SIGILL в `_aesni_ctr32_6x` на CPU с AVX, но без AES-NI (i3-3220): BoringSSL
  видел AVX и выбирал AES-GCM на VEX-инструкциях (`VAESENC`); system_server после этого падал
  с `Failed to encrypt blob`. На таких CPU AVX (и FMA, F16C, AVX2, AVX-512, VAES, VPCLMULQDQ)
  скрыт от гостя через выходы CPUID, и BoringSSL берёт путь на обычных AES-NI, которые VMM
  выполняет сам. В `ExtendedVmExits` включён бит `X64CpuidExit`, без которого список листов
  CPUID не действует. Счётчик `emulated_aes` в статистике выходов.
- `libcrypto` (BoringSSL) падал на CPU без AES-NI (Core i3 Sandy/Ivy Bridge и др.): он
  собран с `-maes` и не проверяет CPUID. VMM выполняет `AESENC`, `AESENCLAST`, `AESDEC`,
  `AESDECLAST`, `AESIMC`, `AESKEYGENASSIST` и `PCLMULQDQ` программно (проверено по FIPS-197).
- Android падал в `init` (`invalid opcode`) на CPU без `MOVBE` (Intel до Haswell, например
  Core i3-3220; AMD до Jaguar/Zen): образы Cuttlefish собраны под Silvermont. VMM перехватывает
  `#UD` и выполняет `MOVBE` сам (декодер `insn`, перевод адресов, `#PF` для неподгруженных
  страниц), остальные `#UD` возвращает гостю. Счётчики `emulated` и `invalid_opcode` в
  статистике выходов.
- `cf-fetch` не распаковывал образы, если `tar` не находится в PATH: теперь пробуется
  `%SystemRoot%\System32\tar.exe`, затем PowerShell `Expand-Archive`.

- Окно не открывалось на видеокартах без D3D12 (например, Intel HD Graphics с драйвером
  2015 года): композитор перебирает видеокарты и, если D3D12 нет ни у одной, выводит через
  WARP. Решение о слабом железе — ADR 0009.
- `hw-test.ps1`: если winget сломан (например, `0x8a15000f` при обновлении источников), Git,
  Rust и Visual Studio Build Tools ставятся установщиками с официальных сайтов; winget
  вызывается только с источником `winget`.

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

- Быстрая проверка на железе: workflow `Nightly` после каждого пуша публикует готовый
  `rustdroid.exe` в pre-release `nightly-<ветка>`; `hw-test.ps1` по умолчанию скачивает его
  (без Rust, Build Tools и сборки), качает образы средствами PowerShell, запускает только этап 3
  и подаёт звук, когда Android загрузился. Прежний режим — `-FromSource`.
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
- `userdata.img` из архива Cuttlefish (sparse-образ пустой ФС) заменяется пустым разделом 8 ГиБ;
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
