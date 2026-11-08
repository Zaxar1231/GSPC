//! Подготовка гостя для этапа 1: ядро GKI с ci.android.com и initramfs с busybox.
//!
//! Скачивание идёт через `curl` (есть в Windows 10/11 из коробки), каждый файл проверяется по
//! sha256. Файлы складываются в `guest/cache/` и в репозиторий не коммитятся.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::cpio::CpioWriter;

/// Статический busybox (GPL-2.0, скачивается, не распространяется с RustDroid).
const BUSYBOX_URL: &str = "https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox";
const BUSYBOX_SHA256: &str = "6e123e7f3202a8c1e9b1f94d8941580a25135382b99e8d3e34fb858bba311348";

const KERNEL_BRANCH: &str = "aosp_kernel-common-android15-6.6";
const KERNEL_TARGET: &str = "kernel_virt_x86_64";

/// `/init`: монтирует псевдо-ФС и запускает шелл на консоли.
const INIT_SCRIPT: &str = r#"#!/bin/busybox sh
/bin/busybox --install -s /bin
mkdir -p /proc /sys /dev /tmp
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
echo
echo "RustDroid stage 1: initramfs готов"
echo "Ядро: $(uname -r), CPU: $(grep -c ^processor /proc/cpuinfo), RAM: $(grep MemTotal /proc/meminfo)"
echo "Выключение: poweroff -f или reboot -f"
echo
exec setsid cttyhack sh
"#;

pub(crate) fn cache_dir() -> PathBuf {
    PathBuf::from("guest").join("cache")
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn download(url: &str, dest: &Path) -> Result<()> {
    println!("скачивание {url}");
    let status = Command::new("curl")
        .args(["-fL", "--retry", "3", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context("не удалось запустить curl")?;
    if !status.success() {
        bail!("загрузка {url} не удалась: {status}");
    }
    Ok(())
}

fn fetch_verified(url: &str, dest: &Path, sha256: Option<&str>) -> Result<Vec<u8>> {
    if let Ok(existing) = fs::read(dest)
        && sha256.is_none_or(|h| sha256_hex(&existing) == h)
    {
        println!("уже скачано: {}", dest.display());
        return Ok(existing);
    }
    download(url, dest)?;
    let data = fs::read(dest).with_context(|| format!("чтение {}", dest.display()))?;
    let actual = sha256_hex(&data);
    if let Some(expected) = sha256
        && actual != expected
    {
        let _ = fs::remove_file(dest);
        bail!("sha256 не совпал для {url}: ожидался {expected}, получен {actual}");
    }
    println!("sha256 {actual}  {}", dest.display());
    Ok(data)
}

/// `cargo xtask guest-fetch --kernel-build <ID> [--sha256 <hex>]`.
pub(crate) fn fetch_kernel(args: &[String]) -> Result<()> {
    let mut build = None;
    let mut sha = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--kernel-build" => build = it.next().cloned(),
            "--sha256" => sha = it.next().cloned(),
            other => bail!("неизвестный аргумент: {other}"),
        }
    }
    let Some(build) =
        build.filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric()))
    else {
        bail!(
            "укажите --kernel-build <ID>: номер сборки ветки {KERNEL_BRANCH}, таргет {KERNEL_TARGET} \
             на https://ci.android.com (см. docs/guest-image.md)"
        );
    };
    let dir = cache_dir();
    fs::create_dir_all(&dir)?;
    let url = format!(
        "https://ci.android.com/builds/submitted/{build}/{KERNEL_TARGET}/latest/raw/bzImage"
    );
    let dest = dir.join(format!("bzImage-{build}"));
    let data = fetch_verified(&url, &dest, sha.as_deref())?;
    if data.get(0x202..0x206) != Some(b"HdrS".as_slice()) {
        bail!(
            "{} не похож на bzImage (нет сигнатуры HdrS)",
            dest.display()
        );
    }
    println!("ядро: {}", dest.display());
    Ok(())
}

/// Собирает initramfs с busybox в `out`.
pub(crate) fn build_initramfs(busybox: &[u8], out: &Path) -> Result<()> {
    let file = fs::File::create(out).with_context(|| format!("создание {}", out.display()))?;
    let mut w = CpioWriter::new(std::io::BufWriter::new(file));
    for d in ["bin", "dev", "proc", "sys", "tmp", "etc", "root"] {
        w.dir(d)?;
    }
    w.char_device("dev/console", 0o600, 5, 1)?;
    w.char_device("dev/ttyS0", 0o600, 4, 64)?;
    w.char_device("dev/null", 0o666, 1, 3)?;
    w.file("bin/busybox", 0o755, busybox)?;
    w.symlink("bin/sh", "busybox")?;
    w.file("init", 0o755, INIT_SCRIPT.as_bytes())?;
    w.finish()?;
    Ok(())
}

/// `cargo xtask busybox-initramfs [--out <path>]`.
pub(crate) fn busybox_initramfs(args: &[String]) -> Result<()> {
    let dir = cache_dir();
    let mut out = dir.join("initramfs-busybox.cpio");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(it.next().context("--out требует путь")?),
            other => bail!("неизвестный аргумент: {other}"),
        }
    }
    fs::create_dir_all(&dir)?;
    let busybox = fetch_verified(BUSYBOX_URL, &dir.join("busybox"), Some(BUSYBOX_SHA256))?;
    build_initramfs(&busybox, &out)?;
    println!("initramfs: {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn initramfs_contains_init_and_busybox() {
        let path = std::env::temp_dir().join(format!("rd-initramfs-{}.cpio", std::process::id()));
        build_initramfs(b"\x7fELF-fake", &path).unwrap();
        let data = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let s = String::from_utf8_lossy(&data);
        assert!(s.contains("bin/busybox"));
        assert!(s.contains("cttyhack"));
        assert!(s.contains("dev/console"));
    }
}
