# Гостевой образ Android

Гость — Android 15 x86_64 на ядре GKI android15-6.6, по конфигурации близкий к Cuttlefish.

## Фаза 1: готовые артефакты (этапы 1–3)

Своего сервера для сборки AOSP пока нет, поэтому используем готовые артефакты с
[ci.android.com](https://ci.android.com):

| Что | Ветка / таргет | Файлы |
|---|---|---|
| Ядро | `aosp_kernel-common-android15-6.6`, таргет `kernel_virt_x86_64` | `bzImage`, `initramfs.img`, модули |
| Образы Android | `aosp-android-latest-release`, таргет `aosp_cf_x86_64_only_phone-userdebug` | `*-img-*.zip` (super, vendor_boot, init_boot, userdata…), `cvd-host_package.tar.gz` не нужен |

На ci.android.com публичные сборки Cuttlefish x86_64 остались только в ветке
`aosp-android-latest-release`: сейчас это Android 17 с ядром 6.12 в `boot.img`. Android 15 на
GKI 6.6 из ТЗ потребует своей сборки (фаза 2). Для проверки VMM на этапах 1–3 версия Android не
важна: загрузчик, virtio и bootconfig одинаковы.

Загрузка автоматизирована: `cargo xtask guest-fetch --kernel-build latest` и
`cargo xtask cf-fetch --build latest`. Номер последней удачной сборки берётся из
`https://ci.android.com/builds/branches/<ветка>/status.json`, после скачивания печатается sha256.
В репозиторий образы не коммитятся.

Что даёт фаза 1:
- ядро с virtio-драйверами (virtio-pci, blk, console, gpu, input, net, vsock, snd);
- загрузка до шелла (этап 2) и экран через SwiftShader (этап 3).

Ограничения: HAL Cuttlefish рассчитаны на crosvm и его устройства. Отличия (например, другой
composer или отсутствие части устройств) закрываем совместимостью на стороне VMM, пока не
появится своя сборка.

## Фаза 2: своя сборка (этап 4 и далее)

Нужна, когда понадобятся: gfxstream guest + ANGLE как системный GLES, composer с VSync от хоста,
IDC мультитача, guest-agent, отключённые лишние сервисы, governor `performance`, microG как опция.

### Конфигурация продукта (план)

- `device/rustdroid/x86_64`: наследует `aosp_cf_x86_64_phone`, без телефонии, NFC, BT, камеры.
- HAL: graphics (gfxstream + ANGLE), composer (VSync от хоста), audio (virtio-snd через ALSA),
  input (IDC `touch.deviceType=touchScreen`), сенсоры (заглушки), health (батарея-заглушка).
- `ro.dalvik.vm.native.bridge` для подключения транслятора ARM пользователем.
- Свойства профиля устройства (модель, производитель) — настраиваемые для игр, открывающих
  высокий FPS только на определённых моделях.
- Ядро: GKI 6.6 + наш фрагмент конфигурации (virtio, hyperv clocksource/stimer, haltpoll).
  Исходники ядра и патчи публикуются вместе с образами (GPL-2.0).

### Сервер для сборки

Требования AOSP: Linux x86_64, ≥ 64 ГБ RAM, ≥ 400 ГБ SSD/NVMe, чем больше ядер, тем лучше.
Конкретный провайдер и актуальная цена будут расписаны в момент перехода к фазе 2: цены и
линейки серверов меняются, а фиксировать их сейчас бессмысленно. Кандидаты: выделенные серверы
с почасовой или помесячной оплатой (Hetzner, OVH), облачные инстансы с почасовой оплатой
(включаются только на время сборки, кэш `out/` на отдельном диске).

## Распространение образов

- Образы скачиваются приложением отдельно от установщика.
- Манифест образов: версия, файлы, sha256, размер, ed25519-подпись манифеста.
- Докачка через HTTP Range, проверка хешей после загрузки.
- Обновление образа не затрагивает userdata инстансов (CoW-оверлеи).
