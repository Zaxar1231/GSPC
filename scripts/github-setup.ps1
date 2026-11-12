<#
.SYNOPSIS
  Одноразовая настройка GitHub-репозитория RustDroid: описание, topics, метки, milestones,
  привязка issues к milestones, доска Projects, защита ветки main.

.DESCRIPTION
  Скрипт идемпотентен: повторный запуск ничего не ломает.
  Требуется GitHub CLI (https://cli.github.com) с авторизацией владельца репозитория:
      gh auth login
      gh auth refresh -s project      # нужно для доски Projects

.EXAMPLE
  pwsh scripts/github-setup.ps1
  pwsh scripts/github-setup.ps1 -Repo Zaxar1231/GSPC -SkipProject
#>
param(
    [string]$Repo = "Zaxar1231/GSPC",
    [switch]$SkipProject,
    [switch]$SkipProtection
)

$ErrorActionPreference = "Stop"
$Owner = $Repo.Split("/")[0]

function Invoke-Gh {
    & gh @args
    if ($LASTEXITCODE -ne 0) { throw "gh $($args -join ' ') завершился с кодом $LASTEXITCODE" }
}

Write-Host "== Описание и topics" -ForegroundColor Cyan
Invoke-Gh repo edit $Repo `
    --description "RustDroid — Android-эмулятор для игр на Windows 11: собственный VMM на Rust поверх WHPX, virtio, D3D12, keymapping" `
    --add-topic rust --add-topic android-emulator --add-topic windows --add-topic whpx `
    --add-topic hypervisor --add-topic virtio --add-topic gaming --add-topic keymapping `
    --enable-issues --enable-projects --delete-branch-on-merge --enable-squash-merge `
    --enable-merge-commit=false --enable-rebase-merge

Write-Host "== Приватные отчёты об уязвимостях (SECURITY.md)" -ForegroundColor Cyan
Invoke-Gh api -X PUT "repos/$Repo/private-vulnerability-reporting" | Out-Null

Write-Host "== Метки" -ForegroundColor Cyan
$labels = @(
    @{ n = "bug";              c = "d73a4a"; d = "Что-то работает неправильно" },
    @{ n = "feature";          c = "a2eeef"; d = "Новая возможность" },
    @{ n = "performance";      c = "f9a825"; d = "FPS, время кадра, задержка ввода" },
    @{ n = "compat";           c = "c5def5"; d = "Совместимость игр и приложений" },
    @{ n = "vmm";              c = "5319e7"; d = "WHPX, vCPU, память, прерывания" },
    @{ n = "gpu";              c = "0e8a16"; d = "virtio-gpu, gfxstream, D3D12-композитор" },
    @{ n = "input";            c = "1d76db"; d = "Ввод, keymapping, профили" },
    @{ n = "installer";        c = "bfd4f2"; d = "Установщик и автообновление" },
    @{ n = "devices";          c = "006b75"; d = "virtio-blk/net/snd/vsock/console" },
    @{ n = "guest";            c = "e99695"; d = "Гостевой образ Android, ядро, HAL" },
    @{ n = "docs";             c = "0075ca"; d = "Документация" },
    @{ n = "ci";               c = "ededed"; d = "CI, релизный пайплайн" },
    @{ n = "good first issue"; c = "7057ff"; d = "Подходит для первого вклада" },
    @{ n = "run-bench";        c = "fbca04"; d = "Запустить бенчмарки на self-hosted раннере" }
)
for ($i = 0; $i -le 11; $i++) {
    $labels += @{ n = "stage:$i"; c = "cccccc"; d = "Задача этапа $i" }
}
foreach ($l in $labels) {
    Invoke-Gh label create $l.n --repo $Repo --color $l.c --description $l.d --force | Out-Null
}

Write-Host "== Milestones" -ForegroundColor Cyan
$milestones = [ordered]@{
    0  = "Этап 0 — Репозиторий, workspace, CI"
    1  = "Этап 1 — Минимальный VMM на WHPX"
    2  = "Этап 2 — virtio-blk/console, AOSP до шелла"
    3  = "Этап 3 — virtio-gpu 2D, ввод, оверлей"
    4  = "Этап 4 — 3D: gfxstream"
    5  = "Этап 5 — Сеть и vsock-агент"
    6  = "Этап 6 — Keymapping и редактор"
    7  = "Этап 7 — Установщик и автообновление"
    8  = "Этап 8 — Импорт BlueStacks"
    9  = "Этап 9 — Звук"
    10 = "Этап 10 — Снапшоты, мультиинстанс, задержка"
    11 = "Этап 11 — ARM64→x86_64 транслятор"
}
$existing = (& gh api "repos/$Repo/milestones?state=all&per_page=100" | ConvertFrom-Json).title
foreach ($k in $milestones.Keys) {
    $title = $milestones[$k]
    if ($existing -notcontains $title) {
        Invoke-Gh api "repos/$Repo/milestones" -f "title=$title" | Out-Null
        Write-Host "  создан: $title"
    }
}

Write-Host "== Привязка issues к milestones по меткам stage:N" -ForegroundColor Cyan
foreach ($k in $milestones.Keys) {
    $issues = & gh issue list --repo $Repo --label "stage:$k" --state all --limit 200 --json number | ConvertFrom-Json
    foreach ($issue in $issues) {
        Invoke-Gh issue edit $issue.number --repo $Repo --milestone $milestones[$k] | Out-Null
    }
}

if (-not $SkipProject) {
    Write-Host "== Доска Projects" -ForegroundColor Cyan
    $projects = & gh project list --owner $Owner --format json | ConvertFrom-Json
    $project = $projects.projects | Where-Object { $_.title -eq "RustDroid" } | Select-Object -First 1
    if (-not $project) {
        $project = & gh project create --owner $Owner --title "RustDroid" --format json | ConvertFrom-Json
        Invoke-Gh project field-create $project.number --owner $Owner --name "Workflow" `
            --data-type SINGLE_SELECT --single-select-options "Backlog,In progress,Review,Done" | Out-Null
        Invoke-Gh project link $project.number --owner $Owner --repo $Repo | Out-Null
        Write-Host "  создана доска RustDroid (поле Workflow: Backlog / In progress / Review / Done)"
        Write-Host "  В веб-интерфейсе: New view → Board → Group by: Workflow."
    }
    $issues = & gh issue list --repo $Repo --state open --limit 500 --json url | ConvertFrom-Json
    foreach ($issue in $issues) {
        & gh project item-add $project.number --owner $Owner --url $issue.url 2>$null | Out-Null
    }
}

if (-not $SkipProtection) {
    Write-Host "== Защита main (ruleset)" -ForegroundColor Cyan
    $rulesets = & gh api "repos/$Repo/rulesets" | ConvertFrom-Json
    if (-not ($rulesets | Where-Object { $_.name -eq "main" })) {
        $body = @{
            name        = "main"
            target      = "branch"
            enforcement = "active"
            conditions  = @{ ref_name = @{ include = @("~DEFAULT_BRANCH"); exclude = @() } }
            rules       = @(
                @{ type = "deletion" },
                @{ type = "non_fast_forward" },
                @{ type = "pull_request"; parameters = @{
                        required_approving_review_count   = 0
                        dismiss_stale_reviews_on_push     = $true
                        require_code_owner_review         = $false
                        require_last_push_approval        = $false
                        required_review_thread_resolution = $true
                    } },
                @{ type = "required_status_checks"; parameters = @{
                        strict_required_status_checks_policy = $true
                        required_status_checks = @(
                            @{ context = "rustfmt" },
                            @{ context = "clippy" },
                            @{ context = "tests" },
                            @{ context = "rustdoc" },
                            @{ context = "cargo-deny" }
                        )
                    } }
            )
        } | ConvertTo-Json -Depth 10
        $body | & gh api -X POST "repos/$Repo/rulesets" --input - | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "не удалось создать ruleset" }
        Write-Host "  main: только через PR, обязательный зелёный CI, без force-push и удаления"
    }
}

Write-Host "Готово." -ForegroundColor Green
