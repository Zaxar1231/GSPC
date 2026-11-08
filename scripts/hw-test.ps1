<#
.SYNOPSIS
    Проверка RustDroid на реальном железе: подготовка, сборка, этапы 1–3, архив с логами.

.DESCRIPTION
    Скрипт делает всё сам:
      1. проверяет систему и при необходимости ставит Git, Rust и Visual Studio Build Tools (winget);
      2. проверяет Windows Hypervisor Platform и предлагает включить её (UAC, нужна перезагрузка);
      3. клонирует или обновляет репозиторий и собирает release;
      4. скачивает ядро GKI, busybox и образы Cuttlefish x86_64 (последние удачные сборки);
      5. этап 1: ядро Linux + busybox в serial-консоли (1 vCPU и SMP);
      6. этап 2: Android в консоли (без окна);
      7. этап 3: Android в окне — нужно ваше участие (тап, свайп, клавиатура), скриншоты окна
         делаются автоматически;
      8. собирает всё в архив на рабочем столе.

    Запуск (PowerShell, не от администратора):
      irm https://raw.githubusercontent.com/Zaxar1231/GSPC/feat/stage-3-display-input/scripts/hw-test.ps1 -OutFile $env:TEMP\hw-test.ps1
      powershell -ExecutionPolicy Bypass -File $env:TEMP\hw-test.ps1

    Или из клона репозитория:
      powershell -ExecutionPolicy Bypass -File scripts\hw-test.ps1

.PARAMETER Stages
    Какие этапы проверять: 1, 2, 3 (по умолчанию все).
#>
[CmdletBinding()]
param(
    [int[]]$Stages = @(1, 2, 3),
    [string]$Branch = 'feat/stage-3-display-input',
    [string]$RepoDir = '',
    [int]$Memory = 4096,
    [int]$Cpus = 4,
    [string]$Display = '1280x720@60',
    [int]$Stage1Seconds = 30,
    [int]$Stage2Seconds = 300,
    [int]$Stage3Seconds = 600,
    [string]$CfBuild = 'latest',
    [string]$KernelBuild = 'latest',
    [switch]$SkipBuild,
    [switch]$Yes
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding = [System.Text.Encoding]::UTF8
} catch { }

$RepoUrl = 'https://github.com/Zaxar1231/GSPC.git'
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$Desktop = [Environment]::GetFolderPath('Desktop')
if (-not $Desktop) { $Desktop = $HOME }
$Out = Join-Path $Desktop "RustDroid-test-$Stamp"
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Summary = [System.Collections.Generic.List[string]]::new()

# ---------------------------------------------------------------- вывод ---

function Say([string]$Text, [string]$Color = 'Gray') { Write-Host $Text -ForegroundColor $Color }
function Step([string]$Text) { Write-Host ''; Write-Host "=== $Text ===" -ForegroundColor Cyan }
function Ok([string]$Text) { Say "  [OK]   $Text" 'Green'; $Summary.Add("OK    $Text") }
function Warn([string]$Text) { Say "  [!]    $Text" 'Yellow'; $Summary.Add("WARN  $Text") }
function Fail([string]$Text) { Say "  [FAIL] $Text" 'Red'; $Summary.Add("FAIL  $Text") }

function Ask([string]$Question) {
    if ($Yes) { return $true }
    $a = Read-Host "$Question [Y/n]"
    return ($a -eq '' -or $a -match '^(y|д|yes|да)')
}

function Refresh-Path {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user;$HOME\.cargo\bin"
}

function Has([string]$Cmd) { return [bool](Get-Command $Cmd -ErrorAction SilentlyContinue) }

# Запуск внешней программы с выводом на экран; возвращает код выхода. Предупреждения в stderr
# (прогресс git, cargo) не считаются ошибками.
function Native([string]$File, [string[]]$Arguments) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $File @Arguments 2>&1 | ForEach-Object { "$_" } | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
}

# Запуск внешней программы с логом; возвращает код выхода.
function Run-Logged([string]$File, [string[]]$Arguments, [string]$Log) {
    Say "  > $File $($Arguments -join ' ')" 'DarkGray'
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $File @Arguments *>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $Log -Append | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
}

# Запуск rustdroid без консольного ввода: stdout/stderr в файлы, страховочный таймаут.
function Run-Vm([string]$Name, [string[]]$Arguments, [int]$Seconds, [scriptblock]$WhileRunning) {
    $stdout = Join-Path $Out "$Name-stdout.log"
    $stderr = Join-Path $Out "$Name-stderr.log"
    $stdin = Join-Path $Out '.empty-stdin'
    Set-Content -Path $stdin -Value '' -NoNewline
    Say "  > rustdroid $($Arguments -join ' ')" 'DarkGray'
    # Start-Process в Windows PowerShell склеивает аргументы без кавычек: пути с пробелами
    # нужно экранировать самим.
    $argLine = ($Arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $p = Start-Process -FilePath $script:Exe -ArgumentList $argLine -WorkingDirectory $script:Repo `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr -RedirectStandardInput $stdin `
        -NoNewWindow -PassThru
    $null = $p.Handle  # иначе ExitCode после выхода может быть пустым
    $deadline = (Get-Date).AddSeconds($Seconds + 90)
    while (-not $p.HasExited) {
        if ($WhileRunning) { & $WhileRunning $p }
        if ((Get-Date) -gt $deadline) {
            Warn "$Name не завершился сам за $($Seconds + 90) с — процесс остановлен"
            Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
            break
        }
        Start-Sleep -Seconds 2
    }
    $p.WaitForExit()
    return $p.ExitCode
}

function Log-Has([string]$Path, [string]$Pattern) {
    if (-not (Test-Path $Path)) { return $false }
    return [bool](Select-String -Path $Path -Pattern $Pattern -SimpleMatch -Quiet -Encoding UTF8)
}

# ------------------------------------------------------- снимок окна ---

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class RdShot {
    [StructLayout(LayoutKind.Sequential)] struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr h);
    public static void Init() { SetProcessDPIAware(); }
    public static bool Capture(IntPtr hwnd, string path) {
        RECT r;
        if (hwnd == IntPtr.Zero || IsIconic(hwnd) || !GetWindowRect(hwnd, out r)) return false;
        int w = r.R - r.L, h = r.B - r.T;
        if (w <= 0 || h <= 0) return false;
        using (var bmp = new Bitmap(w, h))
        using (var g = Graphics.FromImage(bmp)) {
            g.CopyFromScreen(r.L, r.T, 0, 0, new Size(w, h));
            bmp.Save(path, ImageFormat.Png);
        }
        return true;
    }
}
'@
[RdShot]::Init()

# ------------------------------------------------------------ система ---

Step 'Сведения о системе'
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$cs = Get-CimInstance Win32_ComputerSystem
$gpus = Get-CimInstance Win32_VideoController
$whp = Get-CimInstance Win32_OptionalFeature -Filter "Name='HypervisorPlatform'" -ErrorAction SilentlyContinue
$hv = Get-CimInstance Win32_OptionalFeature -Filter "Name='Microsoft-Hyper-V-All'" -ErrorAction SilentlyContinue
$dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard -ErrorAction SilentlyContinue
$sys = @(
    "Дата: $(Get-Date -Format s)"
    "Windows: $($os.Caption) $($os.Version) (сборка $($os.BuildNumber)), $($os.OSArchitecture)"
    "CPU: $($cpu.Name.Trim()) — $($cpu.NumberOfCores) ядер / $($cpu.NumberOfLogicalProcessors) потоков"
    "Виртуализация в UEFI (по данным ОС): $($cpu.VirtualizationFirmwareEnabled); гипервизор запущен: $($cs.HypervisorPresent)"
    "RAM: $([math]::Round($cs.TotalPhysicalMemory / 1GB, 1)) ГБ"
    "Windows Hypervisor Platform: $(if ($whp) { @{1='включена';2='выключена'}[[int]$whp.InstallState] } else { 'нет данных' })"
    "Hyper-V: $(if ($hv) { @{1='включён';2='выключен'}[[int]$hv.InstallState] } else { 'нет данных' })"
    "VBS/Memory Integrity: $(if ($dg) { "VBS=$($dg.VirtualizationBasedSecurityStatus) services=$($dg.SecurityServicesRunning -join ',')" } else { 'нет данных' })"
)
foreach ($g in $gpus) { $sys += "GPU: $($g.Name), драйвер $($g.DriverVersion), $($g.CurrentHorizontalResolution)x$($g.CurrentVerticalResolution)@$($g.CurrentRefreshRate)" }
$sys | Set-Content -Path (Join-Path $Out 'system.txt') -Encoding UTF8
$sys | ForEach-Object { Say "  $_" }

if ([int]$os.BuildNumber -lt 22000) { Fail 'Нужна Windows 11 (сборка 22000+).'; }

# ---------------------------------------------------- компоненты WHPX ---

Step 'Windows Hypervisor Platform'
if ($whp -and [int]$whp.InstallState -eq 1) {
    Ok 'Windows Hypervisor Platform включена'
} else {
    Warn 'Windows Hypervisor Platform выключена'
    if (Ask 'Включить её сейчас? (запросит права администратора, потом нужна перезагрузка)') {
        Start-Process powershell -Verb RunAs -Wait -ArgumentList @(
            '-NoProfile', '-Command',
            'Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All -NoRestart; Read-Host "Готово. Нажмите Enter"'
        )
        Say ''
        Say 'Перезагрузите компьютер и запустите этот скрипт ещё раз.' 'Yellow'
        return
    }
    Say 'Без Windows Hypervisor Platform ВМ не запустится; продолжаю только сборку.' 'Yellow'
    $Stages = @()
}
if (-not $cs.HypervisorPresent -and -not $cpu.VirtualizationFirmwareEnabled) {
    Warn 'Похоже, виртуализация (VT-x / SVM) выключена в UEFI — см. README, раздел FAQ.'
}

# ---------------------------------------------------- инструменты ---

Step 'Инструменты сборки'
Refresh-Path
function Ensure-Tool([string]$Cmd, [string]$WingetId, [string]$Title, [string[]]$Extra = @()) {
    if (Has $Cmd) { Ok "$Title найден"; return }
    if (-not (Has 'winget')) { Fail "$Title не найден, а winget недоступен — установите вручную"; throw "нет $Title" }
    if (-not (Ask "$Title не найден. Установить через winget?")) { throw "нет $Title" }
    $null = Native 'winget' (@('install', '--id', $WingetId, '-e', '--accept-source-agreements', '--accept-package-agreements') + $Extra)
    Refresh-Path
    if (-not (Has $Cmd)) { Fail "$Title не появился в PATH — перезапустите PowerShell и скрипт"; throw "нет $Title" }
    Ok "$Title установлен"
}
Ensure-Tool 'git' 'Git.Git' 'Git'

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vc = $null
if (Test-Path $vswhere) {
    $vc = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
if ($vc) {
    Ok "MSVC найден: $vc"
} else {
    if (-not (Ask 'Нет Visual Studio Build Tools (C++). Установить через winget? (~5 ГБ, 10–30 минут)')) { throw 'нет MSVC' }
    $null = Native 'winget' @('install', '--id', 'Microsoft.VisualStudio.2022.BuildTools', '-e',
        '--accept-source-agreements', '--accept-package-agreements',
        '--override', '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
    Ok 'Visual Studio Build Tools установлены'
}
Ensure-Tool 'cargo' 'Rustlang.Rustup' 'Rust (rustup)'

# ---------------------------------------------------- репозиторий ---

Step 'Исходники'
$here = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { '' }
if (-not $RepoDir) {
    if ($here -and (Test-Path (Join-Path $here 'Cargo.toml'))) { $RepoDir = $here }
    else { $RepoDir = Join-Path $HOME 'RustDroid' }
}
$script:Repo = $RepoDir
if (-not (Test-Path (Join-Path $Repo '.git'))) {
    Say "  клонирую в $Repo"
    if ((Native 'git' @('clone', '--branch', $Branch, $RepoUrl, $Repo)) -ne 0) { Fail 'git clone не удался'; throw 'git clone' }
} else {
    $dirty = & git -C $Repo status --porcelain --untracked-files=no
    if ($dirty) {
        Warn "В $Repo есть локальные изменения — обновление пропущено"
    } else {
        $null = Native 'git' @('-C', $Repo, 'fetch', 'origin', $Branch)
        $null = Native 'git' @('-C', $Repo, 'checkout', $Branch)
        if ((Native 'git' @('-C', $Repo, 'pull', '--ff-only', 'origin', $Branch)) -ne 0) {
            Warn 'git pull не удался — проверяю текущую версию'
        }
    }
}
$commit = (& git -C $Repo rev-parse --short HEAD).Trim()
Ok "Репозиторий $Repo, ветка $Branch, коммит $commit"
"commit: $commit`nbranch: $Branch" | Add-Content -Path (Join-Path $Out 'system.txt') -Encoding UTF8
Set-Location $Repo

# ------------------------------------------------------------ сборка ---

Step 'Сборка release'
$script:Exe = Join-Path $Repo 'target\release\rustdroid.exe'
if (-not $SkipBuild) {
    $rc = Run-Logged 'cargo' @('build', '--release', '--locked') (Join-Path $Out 'build.log')
    if ($rc -ne 0) { Fail 'Сборка не удалась (build.log)'; }
    else { Ok 'Сборка прошла' }
}
if (-not (Test-Path $Exe)) { Fail 'rustdroid.exe не найден'; $Stages = @() }

if (Test-Path $Exe) {
    Step 'rustdroid check'
    $rc = Run-Logged $Exe @('check') (Join-Path $Out 'check.log')
    if ($rc -eq 0) { Ok 'rustdroid check: система готова' } else { Fail "rustdroid check: код $rc (check.log)" }
}

# ------------------------------------------------------------ гость ---

$cache = Join-Path $Repo 'guest\cache'
$kernel = $null; $initrd = $null; $images = $null
if ($Stages -contains 1) {
    Step 'Ядро GKI и busybox'
    $rc = Run-Logged 'cargo' @('xtask', 'busybox-initramfs') (Join-Path $Out 'fetch.log')
    $rc2 = Run-Logged 'cargo' @('xtask', 'guest-fetch', '--kernel-build', $KernelBuild) (Join-Path $Out 'fetch.log')
    $initrd = Join-Path $cache 'initramfs-busybox.cpio'
    $kernel = Get-ChildItem $cache -Filter 'bzImage-*' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty FullName
    if ($rc -ne 0 -or $rc2 -ne 0 -or -not $kernel) { Fail 'Не удалось скачать ядро или busybox (fetch.log)' }
    else { Ok "Ядро: $(Split-Path -Leaf $kernel)" }
}
if ($Stages -contains 2 -or $Stages -contains 3) {
    Step 'Образы Android (Cuttlefish x86_64, ~1,2 ГБ + ~4 ГБ после распаковки)'
    $rc = Run-Logged 'cargo' @('xtask', 'cf-fetch', '--build', $CfBuild) (Join-Path $Out 'fetch.log')
    $images = Get-ChildItem $cache -Directory -Filter 'cf-*' -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'super.img') } |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty FullName
    if ($rc -ne 0 -or -not $images) { Fail 'Не удалось скачать образы Android (fetch.log)'; $images = $null }
    else { Ok "Образы: $(Split-Path -Leaf $images)" }
}

# ------------------------------------------------------------ этап 1 ---

function Test-Stage1([string]$Name, [int]$N) {
    $serial = Join-Path $Out "$Name-serial.log"
    $rc = Run-Vm $Name @('boot', '--kernel', $kernel, '--initrd', $initrd, '--memory', '1024', '--cpus', "$N",
        '--timeout', "$Stage1Seconds", '--serial-log', $serial, '-v') $Stage1Seconds $null
    if (Log-Has $serial 'RustDroid stage 1') { Ok "$($Name): ядро загрузилось до шелла busybox ($N vCPU)" }
    elseif (Log-Has $serial 'Linux version') { Fail "$($Name): ядро стартовало, но до шелла не дошло (код $rc)" }
    else { Fail "$($Name): ядро не стартовало (код $rc, смотрите $Name-stderr.log)" }
}
if ($Stages -contains 1 -and $kernel -and (Test-Path $Exe)) {
    Step 'Этап 1: ядро Linux + busybox'
    Test-Stage1 'stage1' 1
    Test-Stage1 'stage1-smp' ([math]::Min($Cpus, 4))
}

# ------------------------------------------------------------ этап 2 ---

$markers = [ordered]@{
    'ядро стартовало'             = 'Linux version'
    'virtio-blk найден'           = 'virtio_blk'
    'init первого этапа'          = 'init first stage started'
    'init второго этапа'          = 'init second stage started'
    'SurfaceFlinger'              = 'surfaceflinger'
    'zygote'                      = 'zygote'
    'загрузка завершена'          = 'sys.boot_completed=1'
}
function Report-Android([string]$Name, [string]$Serial) {
    foreach ($m in $markers.GetEnumerator()) {
        if (Log-Has $Serial $m.Value) { Ok "$($Name): $($m.Key)" } else { Warn "$($Name): нет «$($m.Key)»" }
    }
}
if ($Stages -contains 2 -and $images -and (Test-Path $Exe)) {
    Step "Этап 2: Android в консоли ($Stage2Seconds с)"
    $serial = Join-Path $Out 'stage2-serial.log'
    $rc = Run-Vm 'stage2' @('boot-android', '--images', $images, '--instance', 'hwtest', '--reset',
        '--memory', "$Memory", '--cpus', "$Cpus", '--timeout', "$Stage2Seconds", '--serial-log', $serial,
        '--logcat', (Join-Path $Out 'stage2-logcat.txt'), '-v') $Stage2Seconds $null
    Say "  код выхода: $rc"
    Report-Android 'stage2' $serial
}

# ------------------------------------------------------------ этап 3 ---

if ($Stages -contains 3 -and $images -and (Test-Path $Exe)) {
    Step "Этап 3: Android в окне ($Display, до $Stage3Seconds с)"
    Say '  Сейчас откроется окно RustDroid. Первая загрузка с программным рендером занимает' 'Yellow'
    Say '  несколько минут. Когда появится экран Android:' 'Yellow'
    Say '    1) тапните левой кнопкой мыши по значку или кнопке;' 'Yellow'
    Say '    2) сделайте свайп (зажмите левую кнопку и проведите), например потяните шторку сверху;' 'Yellow'
    Say '    3) откройте поле ввода (поиск) и напечатайте что-нибудь с клавиатуры.' 'Yellow'
    Say '  Окно закроется само; можно закрыть раньше, когда всё проверите.' 'Yellow'
    Say '  Скриншоты окна делаются автоматически раз в 30 секунд (только окно RustDroid).' 'Yellow'
    if (-not $Yes) { Read-Host '  Нажмите Enter, чтобы начать' | Out-Null }

    $serial = Join-Path $Out 'stage3-serial.log'
    $script:shot = 0
    $script:nextShot = (Get-Date).AddSeconds(30)
    $snap = {
        param($p)
        if ((Get-Date) -lt $script:nextShot) { return }
        $script:nextShot = (Get-Date).AddSeconds(30)
        $p.Refresh()
        $script:shot++
        $png = Join-Path $Out ('stage3-shot-{0:D2}.png' -f $script:shot)
        try { $null = [RdShot]::Capture($p.MainWindowHandle, $png) } catch { }
    }
    $rc = Run-Vm 'stage3' @('boot-android', '--images', $images, '--instance', 'hwtest',
        '--memory', "$Memory", '--cpus', "$Cpus", '--display', $Display, '--timeout', "$Stage3Seconds",
        '--serial-log', $serial, '--logcat', (Join-Path $Out 'stage3-logcat.txt'), '-v') $Stage3Seconds $snap
    Say "  код выхода: $rc"
    Report-Android 'stage3' $serial

    $frames = Select-String -Path (Join-Path $Out 'stage3-stderr.log'), (Join-Path $Out 'stage3-stdout.log') `
        -Pattern 'кадры гостя' -SimpleMatch -Encoding UTF8 -ErrorAction SilentlyContinue
    if ($frames) {
        Ok "stage3: композитор получал кадры ($(@($frames).Count) отчётов)"
        $frames | ForEach-Object { $_.Line } | Set-Content (Join-Path $Out 'stage3-frames.txt') -Encoding UTF8
    } else {
        Warn 'stage3: нет строк «кадры гостя» — экран мог не появиться'
    }

    Step 'Ваши наблюдения'
    $answers = @()
    foreach ($q in @(
            'Появился ли экран Android в окне? (да/нет/что видно)',
            'Работает ли тап мышью?',
            'Работает ли свайп?',
            'Работает ли ввод с клавиатуры?',
            'Видны ли оверлей FPS и график в углу?',
            'Что-то ещё (подвисания, артефакты, ошибки)?')) {
        $a = if ($Yes) { '(не спрашивали: -Yes)' } else { Read-Host "  $q" }
        $answers += "$q`n  -> $a"
    }
    $answers | Set-Content (Join-Path $Out 'stage3-answers.txt') -Encoding UTF8
}

# ------------------------------------------------------------ итог ---

Step 'Итог'
$Summary | ForEach-Object { Say "  $_" }
$Summary | Set-Content (Join-Path $Out 'summary.txt') -Encoding UTF8

# Имя пользователя в путях заменяется на <user>.
$user = [Environment]::UserName
if ($user) {
    Get-ChildItem $Out -File -Include *.log, *.txt -Recurse | ForEach-Object {
        $t = Get-Content -LiteralPath $_.FullName -Raw -Encoding UTF8 -ErrorAction SilentlyContinue
        if ($t -and $t.Contains($user)) {
            Set-Content -LiteralPath $_.FullName -Value $t.Replace($user, '<user>') -Encoding UTF8 -NoNewline
        }
    }
}
Remove-Item (Join-Path $Out '.empty-stdin') -ErrorAction SilentlyContinue

$zip = "$Out.zip"
Compress-Archive -Path (Join-Path $Out '*') -DestinationPath $zip -Force
Say ''
Say "Готово. Архив с результатами: $zip" 'Green'
Say 'Пришлите его в чат (или приложите к issue на GitHub).' 'Green'
