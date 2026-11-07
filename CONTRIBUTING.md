# Участие в разработке RustDroid

Спасибо за интерес к проекту! Здесь описано, как собрать проект, какие правила действуют для
кода и как оформлять изменения.

## Окружение

- Windows 11 x64 (основная и единственная целевая платформа).
- [rustup](https://rustup.rs). Версия Rust зафиксирована в `rust-toolchain.toml` и ставится автоматически.
- Visual Studio 2022 Build Tools с рабочей нагрузкой «Разработка классических приложений на C++»
  (линковщик MSVC, позже — сборка gfxstream).
- Включённый компонент Windows Hypervisor Platform (для запуска VMM, начиная с этапа 1).
- Для профилирования: [Tracy](https://github.com/wolfpld/tracy), [PresentMon](https://github.com/GameTechDev/PresentMon).

Логика без Windows-зависимостей (`rustdroid-input`, `rustdroid-telemetry`, `rustdroid-instance`,
`xtask`) собирается и тестируется и на Linux. Это удобно для быстрых итераций, но CI и релизы —
только Windows.

## Сборка и проверки

```powershell
cargo build                       # отладочная сборка (opt-level=1, зависимости — opt-level=3)
cargo build --release             # LTO fat, codegen-units=1
cargo build --profile profiling   # release + полные символы для Tracy
cargo xtask ci                    # fmt --check, clippy -D warnings, тесты
```

## Тесты и бенчмарки

```powershell
cargo test --workspace

# Микробенчмарки (criterion): сохранить базовую линию на main и сравнить с веткой
git switch main
cargo bench --workspace --bench '*' -- --save-baseline base
git switch my-branch
cargo bench --workspace --bench '*' -- --baseline base
cargo xtask bench-check --threshold 5     # падает, если что-то стало медленнее больше чем на 5 %
```

В PR бенчмарки запускаются на self-hosted раннере по метке `run-bench` (ставит мейнтейнер).
С этапа 3 добавятся сценарные бенчмарки (снапшоты игр + PresentMon). См. [docs/performance.md](docs/performance.md).

## Правила кода

- **Производительность — приоритет №1.** Изменение, ухудшающее FPS, время кадра или задержку
  ввода ради удобства кода, принимается только по согласованию с мейнтейнером.
- **Горячие пути** (vCPU loop, virtio, ввод, present): без аллокаций, без блокирующих мьютексов,
  только lock-free очереди и заранее выделенные буферы.
- **`unsafe`** — только там, где без него никак. Каждый блок с комментарием `// SAFETY:`,
  объясняющим, почему инварианты соблюдены (`clippy::undocumented_unsafe_blocks = deny`).
  Весь `unsafe` для гипервизора живёт в `rustdroid-whpx`.
- **Ошибки:** `thiserror` в библиотеках, `anyhow` в бинарниках. `unwrap`/`expect`/`panic!` в рабочем
  коде запрещены линтами (в тестах можно).
- **Логи:** только `tracing`. `println!` в рабочем коде запрещён.
- **Форматирование:** `cargo fmt` (настройки в `rustfmt.toml`).
- **Документация:** у публичных элементов есть doc-комментарии (`missing_docs`).

## Коммиты: Conventional Commits

```
<тип>(<область>): <кратко, в повелительном наклонении>
```

Типы: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `build`, `ci`, `chore`.
Область — крейт или подсистема: `whpx`, `vmm`, `virtio`, `gpu`, `present`, `input`, `installer`, `guest`…

Примеры:

```
feat(vmm): load bzImage via 64-bit boot protocol
perf(virtio): enable EVENT_IDX for blk queues
fix(input): keep touch slot on orientation change
```

Коммиты небольшие и осмысленные: один коммит — одно логическое изменение.

## Ветки и PR

- `main` защищена: изменения только через PR с зелёным CI.
- Каждый этап roadmap — отдельная ветка (`feat/stage-N-<имя>`) и PR, привязанный к milestone и issues.
- Заполняйте чек-лист из шаблона PR, обновляйте `CHANGELOG.md` (раздел `Unreleased`).

## Лицензия вклада

Отправляя изменения, вы соглашаетесь, что они распространяются на условиях MIT OR Apache-2.0,
как и весь проект. Код из других проектов (crosvm — BSD-3-Clause, Firecracker и cloud-hypervisor —
Apache-2.0 / BSD-3-Clause) переносите только с сохранением их уведомлений об авторских правах.
