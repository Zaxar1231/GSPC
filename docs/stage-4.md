# Этап 4: 3D на видеокарте хоста

Цель этапа: Android и игры рисуют через видеокарту компьютера, а не программно; кадр попадает в
окно без копирования через CPU; на слабых видеокартах без Vulkan работает запасной путь через
D3D11 ([ADR 0009](adr/0009-weak-gpu-support.md)).

Решения, на которых этап построен: [ADR 0002](adr/0002-gfxstream.md) (gfxstream),
[ADR 0003](adr/0003-d3d12-compositor.md) (композитор на D3D12),
[ADR 0009](adr/0009-weak-gpu-support.md) (уровни графики).

## Как устроен путь кадра

```
Игра (GLES/Vulkan) → ANGLE или драйвер gfxstream в госте
    → команды в virtio-gpu (контекст gfxstream, blob-ресурсы)
    → rustdroid-gpu → libgfxstream_backend.dll на хосте
    → Vulkan хоста (уровень 1) или ANGLE-on-D3D11 (уровень 2)
    → общая текстура (NT handle) → D3D12-композитор → окно
```

## Задачи

| # | Задача | Проверка | Статус |
|---|---|---|---|
| 4.0 | Ускорение программного выполнения `MOVBE`/AES-NI: читать из WHPX только нужные регистры | меньше нагрузка на i3-3220 | 🚧 |
| 4.1 | Сборка gfxstream (хостовая часть) под Windows в GitHub Actions, публикация DLL | DLL собирается в CI | ✅ |
| 4.2 | `rustdroid-gfxstream`: загрузка DLL, безопасная обёртка над C API `stream_renderer_*` | тесты обёртки, `rustdroid check` видит DLL | 🚧 |
| 4.3 | virtio-gpu 3D: `CONTEXT_INIT`, capsets, контексты, `SUBMIT_3D`, blob-ресурсы, фенсы по контекстам | unit-тесты протокола | ✅ |
| 4.3b | Общая память (shm-область PCI) для `RESOURCE_MAP_BLOB`: host-visible blob-ресурсы, нужные Vulkan гостя | unit-тесты, отображение в WHPX | ✅ (железо — в 4.4) |
| 4.4 | Параметры гостя для gfxstream (`gpu_mode=gfxstream` у Cuttlefish), выбор уровня графики | загрузочная анимация через GPU | ✅ i3-10110U: Android загружается за 49 с, ввод работает |
| 4.5 | Zero-copy: Vulkan-образ gfxstream → NT handle → `OpenSharedHandle` в D3D12 | нет копии кадра через CPU | ⏳ |
| 4.6 | Уровень 2: gfxstream поверх ANGLE-on-D3D11 для GPU без Vulkan | i3-3220 рисует на видеокарте | 🚧 WGL не работает (см. ниже), следующий шаг — ANGLE |
| 4.7 | Шейдерный проход композитора: letterbox (сохранение пропорций), масштабирование | пропорции при любом размере окна | 🚧 сделано, ждёт проверки на железе |
| 4.8 | Кэш шейдеров и пайплайнов на диске, VSync от хоста, VRR | нет статтеров при повторном запуске | ⏳ |
| 4.9 | Замеры: FPS, 1% low, время кадра на ноутбуке и i3-3220 до/после | таблица в этом документе | 🚧 ноутбук — есть, i3-3220 — после 4.6 |

## Сделано

**4.1.** Workflow `gfxstream` собирает `gfxstream_backend.dll` (≈6,7 МБ) из коммита
`google/gfxstream`, указанного в `GFXSTREAM_COMMIT`. Для Windows понадобились
`WIN32_LEAN_AND_MEAN` (конфликт `winsock.h`/`winsock2.h` в новых SDK) и патч
`0001-vk-getReason-c_str.patch`. DLL экспортирует весь C API `stream_renderer_*`, которым
пользуется обёртка, и зависит только от системных DLL и рантайма Visual C++ 2015–2022.

**4.3.** Протокол 3D в `crates/gpu/src/device.rs`, исполнитель — трейт
`rustdroid_gpu::Backend3d`:

- устройство с бэкендом объявляет `VIRGL`, `RESOURCE_BLOB`, `CONTEXT_INIT` и число capsets;
- `GET_CAPSET_INFO`/`GET_CAPSET`, `CTX_CREATE` (тип контекста из `context_init`),
  `CTX_DESTROY`, `CTX_ATTACH/DETACH_RESOURCE`, `RESOURCE_CREATE_3D`,
  `TRANSFER_TO/FROM_HOST_3D`, `SUBMIT_3D` (входные фенсы пропускаются), `RESOURCE_CREATE_BLOB`,
  `SET_SCANOUT_BLOB`; `ATTACH/DETACH_BACKING`, `UNREF`, `TRANSFER_TO_HOST_2D`, `SET_SCANOUT` и
  `RESOURCE_FLUSH` работают и с 3D-ресурсами (так ядро создаёт dumb-буферы при `VIRGL`);
- команда с `VIRTIO_GPU_FLAG_FENCE` получает ответ сразу, но в used ring попадает только после
  того, как бэкенд сообщит о фенсе через `FenceSink` (из любого потока; приёмник будит worker
  устройства). Фенс с `INFO_RING_IDX` завершает команды своего контекста и кольца с номером не
  больше, без него — глобальную шкалу;
- `reset` освобождает ресурсы и контексты у бэкенда;
- `rustdroid_gpu::gfxstream::GfxstreamBackend` переводит это в `stream_renderer_*`: backing
  ресурсов — `iovec` с указателями на гостевую RAM (`GuestMemory::host_range`), кадр
  scanout-ресурса читается `transfer_read_iov` в буфер хоста.

Ограничения: `RESOURCE_MAP_BLOB` отвечает ошибкой, пока нет shm-области (4.3b); кадр 3D
копируется через CPU (до 4.5); в эмуляторе устройство с бэкендом ещё не включается (4.4).
Проверка — 10 unit-тестов протокола на заглушке бэкенда; на реальном gfxstream проверится на
железе в 4.4.

**4.3b.** Область общей памяти virtio-gpu — 64-битный prefetchable BAR2 (4 ГиБ) с capability
`VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` (`shmid` 1, host-visible). BAR лежит в окне 32–40 ГиБ:
ниже 64 ГиБ, потому что у старых CPU (i3-3220) физический адрес 36-битный; окно объявлено в
`_CRS` корневого моста ACPI (`QWordMemory`), иначе ядро не примет BAR. RAM гостя ограничена,
чтобы не заходить в окно (до 29 ГиБ).

`RESOURCE_MAP_BLOB`: бэкенд отображает blob на хосте (`stream_renderer_resource_map`; память
выделяет gfxstream с выравниванием на страницу — функция `VulkanAllocateHostMemory`), устройство
проверяет выравнивание, границы и пересечения и отображает её в BAR2 через `WHvMapGpaRange`.
`map_info` (кэшируемость) передаётся гостю как есть. Отображения снимаются при `UNMAP_BLOB`,
`RESOURCE_UNREF`, сбросе устройства и до удаления бэкенда.

**4.4.** `boot-android --display … --gpu gfxstream`: gfxstream запускается на Vulkan без окна
(`USE_VK | USE_SURFACELESS`), virtio-gpu получает 3D-бэкенд и общую память, фенсы будят worker
устройства. Параметры гостя — как `gpu_mode=gfxstream_guest_angle` у Cuttlefish: ANGLE поверх
Vulkan, Vulkan — драйвер gfxstream `ranchu`, клиентская композиция, кадр через DRM virtio-gpu
(на хосте пока читается через CPU). Если gfxstream не запустился, `boot-android` завершается с
ошибкой: параметры гостя уже выбраны под gfxstream. По умолчанию `--gpu software` (этап 3).
`hw-test.ps1 -Gpu auto` берёт gfxstream, если его прошла проверка 3D.

**4.4 на железе.** На Intel UHD (драйвер 27.20.100.8984, Vulkan 1.2) понадобились патчи
gfxstream 0003–0006: у драйвера нет synchronization2 и `vkQueueSubmit2`, а функции core 1.3
есть только под именами расширений; `vkCmdBindVertexBuffers2EXT` в драйвере падает, поэтому
`VK_EXT_extended_dynamic_state(2)` гостю не показывается (подробности —
`third_party/gfxstream-patches/README.md`). После этого Android загружается до
`sys.boot_completed` за 49 с с чистого диска, ANGLE в госте работает на
«Vulkan 1.2.154 (Virtio-GPU GFXStream (Intel UHD Graphics))». FPS пока низкий — разбор в
профиле (строки `профиль CPU` и `время команд virtio-gpu` в логе).

**4.7.** Композитор рисует кадр шейдерным проходом (`crates/present/src/blit.rs`): кадр
копируется в текстуру, back buffer — размера окна, кадр вписывается с сохранением пропорций
(`layout::fit`) с билинейной фильтрацией, поля чёрные. Касания пересчитываются через тот же
прямоугольник (`layout::to_frame`): клик по полю попадает на ближний край экрана. Шейдеры
компилируются при запуске (`D3DCompile`); если конвейер не создался, остаётся прежняя копия с
растяжением DXGI, и касания считаются по всему окну.

## Проверки на железе

`hw-test.ps1 -CheckOnly` (Vulkan хоста и запуск gfxstream на Vulkan, без Android):

| Машина | Vulkan | gfxstream | Примечание |
|---|---|---|---|
| i3-10110U, Intel UHD, драйвер 27.20.100.8984 | 1.2.154 | ✅ после патча 0002; capsets vulkan (116 Б), gles, composer | до патча: `VK_ERROR_OUT_OF_POOL_MEMORY` в композиторе → `abort` |
| i5-8300H | — | — | не проверено |
| i3-3220, Intel HD 2500 | — | — | не проверено (ожидается: нет Vulkan) |

**4.6, первый вариант: OpenGL драйвера.** gfxstream собран с GLES-рендерером хоста
(`GFXSTREAM_ENABLE_HOST_GLES`), и на Windows у него два бэкенда: OpenGL драйвера через WGL
(`opengl32.dll`) и EGL — ANGLE (`libEGL.dll`/`libGLESv2.dll`, флаг `USE_EGL`). Intel HD 2500
(i3-3220) умеет OpenGL 4.0, поэтому сначала — WGL, без лишних DLL; ANGLE-on-D3D11 — если драйвер
OpenGL подведёт.

- `--gpu gfxstream-gl`: рендерер `USE_GLES | USE_SURFACELESS` (без `USE_EGL` — значит WGL);
  гость — как `gpu_mode=gfxstream` у Cuttlefish: `egl=emulation` (драйвер GLES gfxstream),
  GLES 3.0 (для 3.1 нужен OpenGL 4.3);
- `rustdroid check`: если gfxstream на Vulkan не запустился, проверяет его на OpenGL в отдельном
  процессе (`gpu-probe --gl`; gfxstream инициализируется один раз на процесс);
- `hw-test.ps1 -Gpu auto`: Vulkan → `gfxstream`, иначе OpenGL → `gfxstream-gl`, иначе
  `software`.

Риск: Vulkan гостя (`ranchu`) без Vulkan хоста видеокарт не находит; если HWUI на образе
выбирает Vulkan, его нужно отключить параметром гостя (`--set`).

**Проверка на i3-10110U (`-Gpu gfxstream-gl`): не работает.** gfxstream падает при запуске
(`0xC0000409`) — не компилируется его собственный шейдер `TextureDraw`. Причина: с OpenGL
драйвера и GLES 3 gfxstream создаёт core profile (`shouldEnableCoreProfile`), а шейдеры GLSL ES
без транслятора ANGLE (`USE_ANGLE_SHADER_PARSER`, в CMake-сборке исходников нет) core profile не
принимает. Значит, уровень 2 — через ANGLE как EGL хоста (`USE_EGL`, ANGLE на D3D11): GLES идёт
в ANGLE напрямую, без перевода в desktop GL. Нужны `libEGL.dll`/`libGLESv2.dll` ANGLE — собирать
в CI. Пока `rustdroid check` не находит рабочего OpenGL (проверка падает в дочернем процессе), и
`-Gpu auto` выбирает программный рендер.

## Замеры (4.9)

`hw-test.ps1`, этап 3, gfxstream, экран гостя 1280×720@60, прокрутка списка 30 с; FPS — по
оверлею (со слов тестирующего) и строкам `кадры гостя` в логе (за 10 с, без пауз неподвижного
экрана). Инстанс сброшен перед прогоном — первые минуты Android компилирует приложения в фоне.

| Машина | Сборка | vCPU | FPS при прокрутке | CPU процесса | Занятость vCPU | Что изменилось |
|---|---|---|---|---|---|---|
| i3-10110U (2 ядра / 4 потока), Intel UHD | `a99aadd` | 4, MMCSS «Games» | 5–6 | 81–94 % | 68–83 % | исходное состояние |
| i3-10110U | `05581fe` | 2 (`--cpus 0`), без MMCSS | ~15 (лог: 9–16) | 35–55 % | 62–95 % | vCPU больше не отнимают процессор у gfxstream |
| i3-10110U | `5cf6227` | 2 | 30–45 (лог: 38–42 через ~5 мин после загрузки; 1% low 4–5, p99 кадра 200–500 мс) | 9–41 % | 18–56 % | тот же код, Android дольше работал после загрузки |

Выводы: virtio-gpu занят на 1–8 %, чтение кадра через CPU — 2,5–8 мс на кадр, то есть до 4.5
(zero-copy) это не узкое место. Сейчас ограничивает процессор гостя; что именно в нём работает,
покажет `stage3-guest-top.txt` (top из консоли гостя).

## Риски

- **Сборка gfxstream под Windows.** Хостовая часть gfxstream собирается для Android Emulator на
  Windows, но вне его дерева сборки это мало кто делает. Если сборка в CI окажется слишком
  тяжёлой, запасной путь — взять готовую DLL из Android Emulator (лицензия Apache-2.0) и
  ограничиться её C API.
- **Совместимость гостя.** Образы Cuttlefish рассчитаны на gfxstream из той же ветки AOSP;
  версия хостовой части должна совпадать по протоколу.
- **Старые видеокарты.** У Intel HD 2500 нет Vulkan; уровень 2 зависит от того, насколько
  хорошо ANGLE-on-D3D11 работает с драйвером 2015 года.
