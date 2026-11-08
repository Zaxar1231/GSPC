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

/// Скачивает текст по `url` (через `curl`).
fn fetch_text(url: &str) -> Result<String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--retry", "3", url])
        .output()
        .context("не удалось запустить curl")?;
    if !out.status.success() {
        bail!("запрос {url} не удался: {}", out.status);
    }
    String::from_utf8(out.stdout).context("ответ не в UTF-8")
}

/// Номер последней удачной сборки таргета из `status.json` ветки ci.android.com.
fn last_good_build(status_json: &str, target: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(status_json).context("разбор status.json")?;
    let build = v
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .and_then(|ts| {
            ts.iter()
                .find(|t| t.get("name").and_then(serde_json::Value::as_str) == Some(target))
        })
        .and_then(|t| t.get("last_known_good_build"))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("в status.json нет таргета {target}"))?;
    if build.is_empty() || build == "0" || !build.chars().all(|c| c.is_ascii_digit()) {
        bail!("у таргета {target} нет удачных сборок");
    }
    Ok(build.to_owned())
}

/// Разрешает `latest` в номер последней удачной сборки, остальное возвращает как есть.
fn resolve_build(build: String, branch: &str, target: &str) -> Result<String> {
    if build != "latest" {
        return Ok(build);
    }
    let url = format!("https://ci.android.com/builds/branches/{branch}/status.json");
    let id = last_good_build(&fetch_text(&url)?, target)?;
    println!("последняя удачная сборка {branch}/{target}: {id}");
    Ok(id)
}

fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
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

/// `cargo xtask guest-fetch --kernel-build <ID|latest> [--sha256 <hex>]`.
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
            "укажите --kernel-build <ID|latest>: номер сборки ветки {KERNEL_BRANCH}, таргет \
             {KERNEL_TARGET} на https://ci.android.com (см. docs/guest-image.md)"
        );
    };
    let build = resolve_build(build, KERNEL_BRANCH, KERNEL_TARGET)?;
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

/// Ветка ci.android.com с образами Cuttlefish по умолчанию (публичная, последний релиз AOSP).
const CF_BRANCH: &str = "aosp-android-latest-release";
/// Таргет Cuttlefish x86_64 по умолчанию (имя таргета зависит от ветки ci.android.com).
const CF_TARGET: &str = "aosp_cf_x86_64_only_phone-userdebug";

/// `cargo xtask cf-fetch --build <ID|latest> [--branch <BRANCH>] [--target <TARGET>]
/// [--sha256 <hex>]`: образы Cuttlefish (`*-img-<ID>.zip`) в `guest/cache/cf-<ID>/`.
pub(crate) fn fetch_cuttlefish(args: &[String]) -> Result<()> {
    let mut build = None;
    let mut target = CF_TARGET.to_owned();
    let mut branch = CF_BRANCH.to_owned();
    let mut sha = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--build" => build = it.next().cloned(),
            "--target" => target = it.next().cloned().context("--target требует значение")?,
            "--branch" => branch = it.next().cloned().context("--branch требует значение")?,
            "--sha256" => sha = it.next().cloned(),
            other => bail!("неизвестный аргумент: {other}"),
        }
    }
    let Some(build) =
        build.filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric()))
    else {
        bail!(
            "укажите --build <ID|latest>: номер сборки таргета {target} на https://ci.android.com \
             (см. docs/stage-2.md)"
        );
    };
    if !is_valid_name(&target) || !is_valid_name(&branch) {
        bail!("некорректное имя ветки или таргета: {branch} / {target}");
    }
    let build = resolve_build(build, &branch, &target)?;
    let product = target.split('-').next().unwrap_or("aosp_cf_x86_64_phone");
    let dir = cache_dir().join(format!("cf-{build}"));
    fs::create_dir_all(&dir)?;
    let zip_name = format!("{product}-img-{build}.zip");
    let url =
        format!("https://ci.android.com/builds/submitted/{build}/{target}/latest/raw/{zip_name}");
    let zip = dir.join(&zip_name);
    fetch_verified(&url, &zip, sha.as_deref())?;
    let unpacked = ["boot.img", "vendor_boot.img", "super.img"]
        .iter()
        .all(|n| dir.join(n).is_file());
    if unpacked {
        println!("образы уже распакованы: {}", dir.display());
        return Ok(());
    }
    // tar из Windows 10/11 (bsdtar) распаковывает zip.
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&dir)
        .status()
        .context("не удалось запустить tar")?;
    if !status.success() {
        bail!("распаковка {} не удалась: {status}", zip.display());
    }
    for needed in ["boot.img", "vendor_boot.img", "super.img"] {
        if !dir.join(needed).is_file() {
            bail!("в архиве нет {needed} — проверьте таргет сборки");
        }
    }
    println!("образы Android: {}", dir.display());
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
    fn last_good_build_finds_target() {
        let json = r#"{"targets":[
            {"name":"a","last_known_good_build":"1"},
            {"name":"kernel_virt_x86_64","last_known_good_build":"16864189"},
            {"name":"z","last_known_good_build":"0"}]}"#;
        assert_eq!(
            last_good_build(json, "kernel_virt_x86_64").unwrap(),
            "16864189"
        );
        assert!(last_good_build(json, "z").is_err());
        assert!(last_good_build(json, "missing").is_err());
    }

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
