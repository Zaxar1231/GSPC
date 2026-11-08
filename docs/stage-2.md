# Этап 2: virtio, диски и загрузка Android до шелла

Цель этапа: Android 15 из образов Cuttlefish x86_64 загружается на нашем VMM до шелла в консоли.

## Что реализовано

### 2a. Платформа и virtio

- **PCI:** ECAM для шины 0, таблица ACPI MCFG, корневой мост `PCI0` в DSDT (`PNP0A08`) с окном
  MMIO для BAR. BAR назначаются заранее, а если ядро их переназначит, шина находит устройство
  по новому адресу.
- **virtio-pci (modern):**
  - capability-структуры: common, notify, ISR, device;
  - MSI-X с маскированием и PBA, прерывания идут в LAPIC гипервизора напрямую через
    `WHvRequestInterrupt`, минуя IOAPIC;
  - `VIRTIO_F_EVENT_IDX` и indirect-дескрипторы.
- **Doorbell-события WHPX:** запись гостя в notify-регистр очереди будит worker устройства без
  выхода vCPU в user mode. Если doorbell недоступен, работает запасной путь через выход vCPU.
- **virtio-blk:**
  - read, write, flush, get_id;
  - бэкенды: обычный файл, copy-on-write оверлей (формат `RDCOWv1`, кластеры по 64 КиБ) и
    составной диск с синтезированной таблицей GPT.
- **virtio-console** (hvc0).
- **SMP (экспериментально):** `--cpus N`. AP-процессоры стартуют в состоянии ожидания SIPI, а
  INIT/SIPI обрабатывает LAPIC гипервизора.

### 2b. Загрузка Android

`rustdroid boot-android --images <каталог>`:

1. Разбирает `boot.img`, `init_boot.img` и `vendor_boot.img` (boot image v3/v4).
2. Собирает initrd: vendor ramdisk + generic ramdisk + bootconfig. Bootconfig — это параметры из
   `vendor_boot` и наши поверх них (без дубликатов ключей).
3. Собирает составной GPT-диск: `misc`, `boot_a`, `init_boot_a`, `vendor_boot_a`, `vbmeta*_a`,
   `super`, `userdata`, `metadata`. Каждый раздел — copy-on-write поверх исходного образа,
   оверлеи лежат в `%LOCALAPPDATA%\RustDroid\instances\<имя>\overlays`. `--reset` возвращает
   инстанс к исходным образам.
4. Диск — устройство PCI `00:01.0` (`androidboot.boot_devices=pci0000:00/0000:00:01.0`), консоль
   Android — `ttyS0` (или `hvc0` с `--console hvc`).

## Как запустить

### Linux с диском (проверка virtio-blk)

```powershell
cargo build --release
# Любой образ ФС, например ext4, созданный в WSL: mkfs.ext4 test.img 256M
.\target\release\rustdroid.exe boot `
    --kernel guest\cache\bzImage-<НОМЕР> `
    --initrd guest\cache\initramfs-busybox.cpio `
    --disk test.img,cow=test.rdcow --cpus 2 --serial-log boot.log -v
```

В шелле гостя: `ls /sys/bus/pci/devices`, `cat /proc/interrupts | grep virtio`, `ls /dev/vd*`.
Если virtio-драйверы в ядре собраны модулями (так в GKI `kernel_virt_x86_64`), их нужно
загрузить из `vendor_ramdisk` — проще проверить сразу на Android (ниже) или на ядре со встроенными
`CONFIG_VIRTIO_PCI` и `CONFIG_VIRTIO_BLK`.

### Android

1. По умолчанию берётся ветка `aosp-android-latest-release`, таргет
   `aosp_cf_x86_64_only_phone-userdebug` (Android 17: публичных сборок Cuttlefish с Android 15
   на ci.android.com больше нет). `latest` — последняя удачная сборка.
2. Скачайте образы:

   ```powershell
   cargo xtask cf-fetch --build <НОМЕР|latest> [--branch <ВЕТКА>] [--target <ТАРГЕТ>]
   # → guest\cache\cf-<НОМЕР>\ (boot.img, init_boot.img, vendor_boot.img, super.img, …)
   ```

3. Запустите:

   ```powershell
   .\target\release\rustdroid.exe boot-android `
       --images guest\cache\cf-<НОМЕР> --memory 4096 --cpus 2 --serial-log android.log -v
   ```

`userdata.img` из архива Cuttlefish — заготовка на несколько МиБ (её расширяет `launch_cvd`), поэтому
вместо неё подаётся пустой раздел 8 ГиБ: Android форматирует его при первой загрузке. В bootconfig
по умолчанию добавляются `androidboot.fstab_suffix=cf.f2fs.hctr2` (иначе первый этап init не
найдёт fstab) и `androidboot.vbmeta.device_state=unlocked`.

Ожидаемый результат: лог ядра, `init: first stage`, монтирование `super`, второй этап `init` и
приглашение шелла `console:/ $` на ttyS0 (в userdebug-сборках).

## Что прислать после запуска

1. `android.log` и логи терминала (stderr), запуск с `-v`.
2. Строку `ВМ остановлена` со счётчиками выходов и `exits_per_sec`.
3. Время до приглашения шелла.

## Известные риски

| Риск | Что будет видно | План |
|---|---|---|
| virtio-модули не загрузились в first stage | `init: … Failed to find …` | проверить `modules.load` в vendor ramdisk; при необходимости добавить ядро со встроенными драйверами |
| first stage не находит разделы | `Unable to find device … by-name` | сверить путь PCI в `androidboot.boot_devices` |
| AVB отклоняет vbmeta | `avb: … verification failed` | `androidboot.verifiedbootstate=orange` уже задан; при необходимости отключить проверку в vbmeta (`avbtool --flags 2`) |
| Сервисы Cuttlefish ждут устройств crosvm (vsock, gpu) | ошибки HAL в логе | не мешает шеллу; устройства — этапы 3–5 |
| SMP | зависание на `smpboot` | запустить с `--cpus 1` и прислать лог |
| Нет графики | — | SurfaceFlinger не стартует до этапа 3, на шелл не влияет |
