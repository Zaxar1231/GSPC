# Этап 3: экран Android в окне, ввод, оверлей производительности

Цель этапа: экран гостя в окне Windows, управление мышью и клавиатурой, оверлей с FPS, 1% low,
временем кадра и задержкой.

## Что реализовано

- **virtio-gpu 2D** (`rustdroid-gpu`):
  - ресурсы, backing (scatter-список), `TRANSFER_TO_HOST_2D`, `SET_SCANOUT`, `RESOURCE_FLUSH`;
  - фенсы, защита от гигантских ресурсов (лимит 1 ГиБ).
- **EDID:** режим дисплея по CVT reduced blanking. Гость видит заданные разрешение и частоту
  (60/90/120/144+ Гц), поэтому игры открывают высокий FPS.
- **Почтовый ящик кадров:**
  - композитор берёт самый свежий кадр и просыпается сразу после `RESOURCE_FLUSH`;
  - буферы переиспользуются;
  - номер и кадр меняются атомарно.
- **virtio-input** (`rustdroid-devices`):
  - мультитач-экран (протокол B, `INPUT_PROP_DIRECT`, 10 касаний) и клавиатура;
  - событие уходит гостю сразу, без ожидания кадра.
- **D3D12-композитор** (`rustdroid-present`):
  - самый производительный адаптер с D3D12; без D3D12 (старые видеокарты, драйверы WDDM 1.x) —
    WARP, программный D3D12 из Windows;
  - flip model, waitable swapchain, `SetMaximumFrameLatency(1)`;
  - tearing без VSync (VRR в окне), лимит FPS;
  - копия кадра через persistently-mapped upload-буфер, шейдеров нет: масштабирует DXGI.
- **Окно Win32:**
  - DPI awareness v2;
  - левая кнопка мыши — касание, клавиатура передаёт скан-коды, которые переводятся в коды
    Linux;
  - UI-поток отделён от потока композитора.
- **Оверлей:**
  - FPS гостя, 1% low, среднее время кадра, задержка `RESOURCE_FLUSH → Present`, график
    времени кадра;
  - память эмулятора (рабочий набор) и загрузка CPU: процессом эмулятора (доля всех логических
    процессоров) и всей системой;
  - рисуется в одном проходе только в углу кадра, без чтения write-combined памяти.
- **Дискретная видеокарта на ноутбуках:** экспорт `NvOptimusEnablement` и
  `AmdPowerXpressRequestHighPerformance`.

## Как запустить

```powershell
cargo build --release
.\target\release\rustdroid.exe boot-android `
    --images guest\cache\cf-<НОМЕР> --memory 4096 --cpus 4 `
    --display 1920x1080@120 --serial-log android.log -v
```

| Параметр | Назначение |
|---|---|
| `--display ШxВ@ГЦ` | окно с экраном гостя, разрешение и частота дисплея гостя |
| `--no-vsync` | вывод без VSync (tearing; для G-Sync/FreeSync) |
| `--fps-limit N` | лимит FPS композитора |
| `--no-overlay` | без оверлея |
| `--set КЛЮЧ=ЗНАЧЕНИЕ` | параметр bootconfig поверх параметров по умолчанию |

Вместе с `--display` включаются параметры программного рендера в госте (как
`gpu_mode=guest_swiftshader` у Cuttlefish):

```
androidboot.hardware.gralloc = minigbm
androidboot.hardware.hwcomposer = ranchu
androidboot.hardware.hwcomposer.mode = client
androidboot.hardware.hwcomposer.display_finder_mode = drm
androidboot.hardware.egl = angle
androidboot.hardware.vulkan = pastel
androidboot.opengles.version = 196609
```

Эти значения нужно сверить на реальных образах. Если SurfaceFlinger или hwcomposer не стартует,
переопределите их через `--set` и пришлите `android.log`.

## Что прислать после запуска

1. `android.log`, вывод терминала с `-v`.
2. Строки `кадры гостя` из лога (раз в 10 секунд): `fps`, `low_1`, `frame_ms`, `p99_ms`,
   `flush_to_present_ms`.
3. Скриншот окна с оверлеем.
4. Работает ли касание мышью (тап, свайп) и ввод с клавиатуры.

## Ограничения этапа

- Гость рисует программно (SwiftShader): FPS в играх будет низким. Это ожидаемо, 3D на хостовой
  видеокарте — этап 4.
- Кадр копируется через CPU: 2D-путь временный, zero-copy появится с gfxstream на этапе 4.
- Масштабирование выполняет DXGI, пропорции не сохраняются. Letterbox и шейдерный проход — на
  этапе 4.
- Ввод сейчас идёт через оконные сообщения. Raw Input в отдельном потоке и захват мыши для
  прицела — этап 6 (keymapping).
- Аппаратный курсор virtio-gpu не рисуется.
