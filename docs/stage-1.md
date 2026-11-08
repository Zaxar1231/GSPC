# Этап 1: запуск минимального VMM

Цель этапа: ядро Linux с initramfs грузится на собственном VMM поверх WHPX, вывод и шелл —
в serial-консоли (COM1) прямо в терминале Windows.

## Что реализовано

- `rustdroid-whpx`: динамическая загрузка WinHvPlatform/WinHvEmulation, партиция, отображение
  памяти, vCPU, регистры, инъекция прерываний, быстрый путь для `in`/`out`, эмулятор для MMIO и
  строковых инструкций.
- `rustdroid-vmm`:
  - RAM: выделяется заранее, prefault, опционально большие страницы;
  - прямая загрузка bzImage + initramfs + cmdline (64-битная точка входа, без BIOS);
  - identity-mapping 0–4 ГиБ и GDT;
  - ACPI hardware-reduced: MADT, FADT, DSDT с COM1 (`PNP0501`);
  - IOAPIC, COM1 (16550), сброс через i8042;
  - поток vCPU на производительных ядрах с MMCSS «Games».
- `rustdroid check`: проверки Windows 11, виртуализации в UEFI, компонента WHPX, частот таймеров
  и гибридной топологии CPU.
- `rustdroid boot`: ВМ с консолью гостя в терминале. Выход — `Ctrl+A`, затем `X`.
- `cargo xtask busybox-initramfs`: initramfs с busybox и шеллом.
- `cargo xtask guest-fetch`: ядро GKI с ci.android.com с проверкой sha256.

Ограничения этапа: один vCPU (SMP — этап 2 вместе с PCI), нет virtio, PCI отключён (`pci=off`).

## Как запустить

### 1. Сборка и проверка системы

```powershell
cargo build --release
.\target\release\rustdroid.exe check
```

Все строки должны быть `[ OK ]`. Если есть `[FAIL]`, рядом будет написано, как исправить.

### 2. initramfs с шеллом

```powershell
cargo xtask busybox-initramfs
# → guest\cache\initramfs-busybox.cpio
```

### 3. Ядро

**Вариант А — GKI android15-6.6 (основной).** Откройте <https://ci.android.com>, ветка
`aosp_kernel-common-android15-6.6`, таргет `kernel_virt_x86_64`, возьмите номер любой зелёной
сборки и выполните:

```powershell
cargo xtask guest-fetch --kernel-build <НОМЕР>
# → guest\cache\bzImage-<НОМЕР>, sha256 печатается в консоль
```

**Вариант Б — любое ядро x86_64** с встроенными `CONFIG_SERIAL_8250`, `CONFIG_SERIAL_8250_CONSOLE`,
`CONFIG_ACPI`, `CONFIG_X86_X2APIC`/`X86_LOCAL_APIC` (например, `vmlinuz` из Ubuntu, скопированный
из WSL: `\\wsl$\Ubuntu\boot\vmlinuz-*`).

### 4. Загрузка

```powershell
.\target\release\rustdroid.exe boot `
    --kernel guest\cache\bzImage-<НОМЕР> `
    --initrd guest\cache\initramfs-busybox.cpio `
    --memory 2048 `
    --serial-log boot.log -v
```

Ожидаемый результат: лог ядра в терминале, затем

```
RustDroid stage 1: initramfs готов
Ядро: 6.6.x-android15-..., CPU: 1, RAM: MemTotal: ...
/ #
```

Параметры:

| Параметр | Назначение |
|---|---|
| `--append "loglevel=8"` | добавить параметры ядра |
| `--cmdline "..."` | полностью заменить командную строку |
| `--large-pages` | RAM на страницах 2 МиБ (нужна политика «Блокировка страниц в памяти») |
| `--guest-hz 1000` | если ядро собрано с другим `CONFIG_HZ` |
| `-v` / `-vv` | подробные логи (в stderr) |

## Что прислать после запуска

1. Вывод `rustdroid check`.
2. `boot.log` и вывод терминала (stderr с логами RustDroid).
3. Строку `vCPU остановлен` из лога: в ней счётчики выходов (`io`, `mmio`, `halt`) и `exits_per_sec`.
   Это базовая линия для оптимизаций следующих этапов.
4. Время от запуска до приглашения `/ #`.

## Если что-то пошло не так

| Симптом | Вероятная причина | Что сделать |
|---|---|---|
| `[FAIL] компонент Windows Hypervisor Platform не включён` | компонент выключен | команда из сообщения, перезагрузка |
| `[FAIL] виртуализация выключена в UEFI/BIOS` | VT-x/SVM выключены | включить в UEFI |
| Тишина после запуска, ни одной строки ядра | ядро не дошло до earlycon | прислать лог с `-vv`; попробовать `--append "earlyprintk=serial,ttyS0,115200"` |
| `тройная ошибка (triple fault)` | неверное начальное состояние или таблицы | прислать лог с RIP |
| Ядро останавливается на калибровке таймеров | не та `CONFIG_HZ` или нет LAPIC-таймера | `--guest-hz`, лог ядра |
| Нет реакции на ввод | IRQ COM1 не доставляется | прислать `boot.log`; шелл должен работать после `serial8250: ttyS0 at I/O 0x3f8 (irq = 4)` |
