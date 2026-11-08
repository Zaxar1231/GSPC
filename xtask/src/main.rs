//! `cargo xtask` — автоматизация сборки RustDroid.
//!
//! Команды:
//! - `cargo xtask ci` — те же проверки, что в CI: fmt, clippy `-D warnings`, тесты;
//! - `cargo xtask bench-check [--threshold <проценты>] [--dir <каталог criterion>]` —
//!   проверяет результаты `cargo bench -- --baseline <имя>` и падает, если среднее время
//!   любого бенчмарка выросло больше порога (по умолчанию 5 %);
//! - `cargo xtask guest-fetch --kernel-build <ID>` — скачать ядро GKI (android15-6.6,
//!   `kernel_virt_x86_64`) с ci.android.com в `guest/cache/`;
//! - `cargo xtask busybox-initramfs` — собрать initramfs с busybox для проверки этапа 1;
//! - `cargo xtask cf-fetch --build <ID>` — скачать и распаковать образы Cuttlefish x86_64.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI-утилита пишет в консоль"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod cpio;
mod guest;

const DEFAULT_THRESHOLD_PERCENT: f64 = 5.0;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ci") => ci(),
        Some("bench-check") => bench_check(args.get(1..).unwrap_or_default()),
        Some("guest-fetch") => guest::fetch_kernel(args.get(1..).unwrap_or_default()),
        Some("busybox-initramfs") => guest::busybox_initramfs(args.get(1..).unwrap_or_default()),
        Some("cf-fetch") => guest::fetch_cuttlefish(args.get(1..).unwrap_or_default()),
        _ => {
            eprintln!(
                "использование: cargo xtask <ci | bench-check [--threshold N] [--dir PATH] | \
                 guest-fetch --kernel-build ID [--sha256 HEX] | busybox-initramfs [--out PATH] | \
                 cf-fetch --build ID [--target T] [--sha256 HEX]>"
            );
            bail!("неизвестная команда")
        }
    }
}

fn ci() -> Result<()> {
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run("cargo", &["test", "--workspace", "--locked"])?;
    println!("xtask ci: всё зелёное");
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    println!("> {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("не удалось запустить {program}"))?;
    if !status.success() {
        bail!(
            "{program} {} завершился с ошибкой: {status}",
            args.join(" ")
        );
    }
    Ok(())
}

/// Результат сравнения одного бенчмарка с базовой линией.
#[derive(Debug)]
struct BenchChange {
    id: String,
    /// Относительное изменение среднего времени: 0.05 = +5 %.
    mean_change: f64,
}

fn bench_check(args: &[String]) -> Result<()> {
    let mut threshold = DEFAULT_THRESHOLD_PERCENT;
    let mut dir = PathBuf::from("target").join("criterion");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--threshold" => {
                let v = it.next().context("--threshold требует значение")?;
                threshold = v.parse().with_context(|| format!("неверный порог: {v}"))?;
            }
            "--dir" => dir = PathBuf::from(it.next().context("--dir требует значение")?),
            other => bail!("неизвестный аргумент: {other}"),
        }
    }

    let mut changes = Vec::new();
    collect_changes(&dir, &dir, &mut changes)?;
    if changes.is_empty() {
        bail!(
            "в {} нет результатов сравнения; запустите `cargo bench -- --baseline <имя>`",
            dir.display()
        );
    }
    changes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut regressions = 0_usize;
    println!("{:<50} {:>10}", "бенчмарк", "изменение");
    for c in &changes {
        let percent = c.mean_change * 100.0;
        let mark = if percent > threshold {
            regressions += 1;
            "  РЕГРЕССИЯ"
        } else {
            ""
        };
        println!("{:<50} {:>+9.2}%{mark}", c.id, percent);
    }
    if regressions > 0 {
        bail!("{regressions} бенчмарк(ов) медленнее базовой линии больше чем на {threshold}%");
    }
    println!("регрессий нет (порог {threshold}%)");
    Ok(())
}

/// Ищет файлы `<бенчмарк>/change/estimates.json`, которые criterion пишет при сравнении.
fn collect_changes(root: &Path, dir: &Path, out: &mut Vec<BenchChange>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("чтение {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let is_change_dir = path.file_name().is_some_and(|n| n == "change");
        let estimates = path.join("estimates.json");
        if is_change_dir && estimates.is_file() {
            let id = dir
                .strip_prefix(root)
                .unwrap_or(dir)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(BenchChange {
                id,
                mean_change: read_mean_change(&estimates)?,
            });
        } else if !is_change_dir {
            collect_changes(root, &path, out)?;
        }
    }
    Ok(())
}

fn read_mean_change(path: &Path) -> Result<f64> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("чтение {}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("разбор {}", path.display()))?;
    json.pointer("/mean/point_estimate")
        .and_then(serde_json::Value::as_f64)
        .with_context(|| format!("в {} нет mean.point_estimate", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_change_estimates() {
        let root = std::env::temp_dir().join(format!("xtask-bench-{}", std::process::id()));
        let change = root.join("group").join("bench").join("change");
        std::fs::create_dir_all(&change).unwrap();
        std::fs::write(
            change.join("estimates.json"),
            r#"{"mean":{"point_estimate":0.031}}"#,
        )
        .unwrap();
        // Каталог base без change не должен учитываться.
        std::fs::create_dir_all(root.join("group").join("bench").join("base")).unwrap();

        let mut out = Vec::new();
        collect_changes(&root, &root, &mut out).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "group/bench");
        assert!((out[0].mean_change - 0.031).abs() < 1e-9);
    }
}
