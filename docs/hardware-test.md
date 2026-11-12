# Проверка на реальном железе одним скриптом

`scripts/hw-test.ps1` проходит этапы 1–3 сам и собирает результаты в архив. От вас нужно
запустить его, ответить на пару вопросов и на этапе 3 потрогать мышью окно Android.

## Запуск

Откройте **PowerShell** (обычный, не от администратора) и выполните:

```powershell
irm https://raw.githubusercontent.com/Zaxar1231/GSPC/feat/stage-3-display-input/scripts/hw-test.ps1 -OutFile $env:TEMP\hw-test.ps1
powershell -ExecutionPolicy Bypass -File $env:TEMP\hw-test.ps1
```

Если репозиторий уже склонирован: `powershell -ExecutionPolicy Bypass -File scripts\hw-test.ps1`.

## Что делает скрипт

По умолчанию — быстрый режим: ничего не собирается на вашем компьютере.

| Шаг | Что происходит | Время |
|---|---|---|
| Система | Windows, CPU, RAM, GPU, Hyper-V / WHPX / VBS → `system.txt` | секунды |
| WHPX | если «Платформа низкоуровневой оболочки Windows» выключена — предлагает включить | UAC, перезагрузка |
| Программа | готовый `rustdroid.exe` из pre-release `nightly-<ветка>` (собирается GitHub Actions после каждого изменения), проверка sha256 | ~10 с |
| Образы | Cuttlefish x86_64 с ci.android.com, распаковка; дальше — из кэша | первый раз 5–15 мин, потом 0 |
| Этап 3 | Android в окне 1280×720 @ 60 Гц; когда Android загрузится — звук и сообщение; скриншоты окна раз в 30 с | пока не закроете окно, максимум 10 мин |
| Итог | всё в `Рабочий стол\RustDroid-test-<дата>.zip` | — |

С `-FromSource` скрипт работает как раньше: ставит Git, Rust и Visual Studio Build Tools, собирает
из исходников и может прогнать этапы 1–2 (`-Stages 1,2,3`). Это нужно, только если готовой
сборки нет или проверяется локальное изменение.

## Параметры

```powershell
powershell -ExecutionPolicy Bypass -File scripts\hw-test.ps1 -FromSource -Stages 1,2,3  # все этапы из исходников
powershell -ExecutionPolicy Bypass -File scripts\hw-test.ps1 -Display 1920x1080@120 -Cpus 6
powershell -ExecutionPolicy Bypass -File scripts\hw-test.ps1 -FromSource -SkipBuild   # из исходников без пересборки
```

| Параметр | По умолчанию | |
|---|---|---|
| `-Stages` | `3` | какие этапы проверять (этап 1 — только с `-FromSource`) |
| `-FromSource` | — | собрать из исходников вместо готовой сборки |
| `-Memory` / `-Cpus` | `4096` / `4` | RAM (МиБ) и vCPU гостя Android |
| `-Display` | `1280x720@60` | окно этапа 3 |
| `-Stage2Seconds` / `-Stage3Seconds` | `300` / `600` | сколько работает ВМ |
| `-CfBuild` / `-KernelBuild` | `latest` | номер сборки ci.android.com |
| `-Branch` | `feat/stage-3-display-input` | ветка репозитория |
| `-Yes` | — | не задавать вопросов (без ответов на этапе 3) |

## Что в архиве

- `summary.txt` — краткий итог `OK` / `WARN` / `FAIL` по шагам;
- `system.txt` — железо и версия Windows (без имени компьютера); имя пользователя в путях заменено на `<user>`;
- `build.log`, `check.log`, `fetch.log`;
- `stageN-serial.log` — консоль гостя, `stageN-logcat.txt` — logcat Android, `stageN-stderr.log` — лог RustDroid (`-v`);
- `stage3-frames.txt` — строки `кадры гостя` (FPS, 1% low, время кадра, задержка);
- `stage3-shot-NN.png` — скриншоты окна RustDroid (только окна, не всего экрана);
- `stage3-answers.txt` — ваши ответы.

## Образы гостя

Скрипт берёт с ci.android.com последние удачные сборки:

- ядро: ветка `aosp_kernel-common-android15-6.6`, таргет `kernel_virt_x86_64` (этап 1);
- Android: ветка `aosp-android-latest-release`, таргет `aosp_cf_x86_64_only_phone-userdebug`.

Публичных сборок Cuttlefish x86_64 с Android 15 на ci.android.com больше нет. Последний релиз AOSP
сейчас Android 17 (ядро 6.12 в `boot.img`), и проверка VMM идёт на нём. Образ Android 15 на
GKI 6.6 из ТЗ появится вместе со своей сборкой AOSP (см. [guest-image.md](guest-image.md)).
