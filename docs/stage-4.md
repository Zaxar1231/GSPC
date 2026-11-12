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
| 4.3b | Общая память (shm-область PCI) для `RESOURCE_MAP_BLOB`: host-visible blob-ресурсы, нужные Vulkan гостя | unit-тесты, отображение в WHPX | ⏳ |
| 4.4 | Параметры гостя для gfxstream (`gpu_mode=gfxstream` у Cuttlefish), выбор уровня графики | загрузочная анимация через GPU | ⏳ |
| 4.5 | Zero-copy: Vulkan-образ gfxstream → NT handle → `OpenSharedHandle` в D3D12 | нет копии кадра через CPU | ⏳ |
| 4.6 | Уровень 2: gfxstream поверх ANGLE-on-D3D11 для GPU без Vulkan | i3-3220 рисует на видеокарте | ⏳ |
| 4.7 | Шейдерный проход композитора: letterbox (сохранение пропорций), масштабирование | пропорции при любом размере окна | ⏳ |
| 4.8 | Кэш шейдеров и пайплайнов на диске, VSync от хоста, VRR | нет статтеров при повторном запуске | ⏳ |
| 4.9 | Замеры: FPS, 1% low, время кадра на ноутбуке и i3-3220 до/после | таблица в этом документе | ⏳ |

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

## Проверки на железе

`hw-test.ps1 -CheckOnly` (Vulkan хоста и запуск gfxstream на Vulkan, без Android):

| Машина | Vulkan | gfxstream | Примечание |
|---|---|---|---|
| i3-10110U, Intel UHD, драйвер 27.20.100.8984 | 1.2.154 | ✅ после патча 0002; capsets vulkan (116 Б), gles, composer | до патча: `VK_ERROR_OUT_OF_POOL_MEMORY` в композиторе → `abort` |
| i5-8300H | — | — | не проверено |
| i3-3220, Intel HD 2500 | — | — | не проверено (ожидается: нет Vulkan) |

## Риски

- **Сборка gfxstream под Windows.** Хостовая часть gfxstream собирается для Android Emulator на
  Windows, но вне его дерева сборки это мало кто делает. Если сборка в CI окажется слишком
  тяжёлой, запасной путь — взять готовую DLL из Android Emulator (лицензия Apache-2.0) и
  ограничиться её C API.
- **Совместимость гостя.** Образы Cuttlefish рассчитаны на gfxstream из той же ветки AOSP;
  версия хостовой части должна совпадать по протоколу.
- **Старые видеокарты.** У Intel HD 2500 нет Vulkan; уровень 2 зависит от того, насколько
  хорошо ANGLE-on-D3D11 работает с драйвером 2015 года.
