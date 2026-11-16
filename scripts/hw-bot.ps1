<#
.SYNOPSIS
    Бот проверки на железе: сам гоняет hw-test.ps1 на каждой новой nightly-сборке.

.DESCRIPTION
    Раз в несколько часов (задача планировщика Windows) смотрит pre-release nightly-<ветка>,
    обновлённые за последнюю неделю. Если коммит сборки ещё не проверялся на этом компьютере,
    запускает hw-test.ps1 того же коммита без участия человека (-Yes, сеть и гостевой агент)
    и отправляет отчёт в issue «hw-test (авто): …». Проверенные коммиты помнит в
    %LOCALAPPDATA%\RustDroid\hw-bot\tested.txt, журнал — hw-bot.log там же.

    Не запускается, если ноутбук работает от батареи или RustDroid уже запущен. Окно Android
    на время проверки (до ~8 мин) появляется на экране.

    Нужен GitHub CLI (gh), авторизованный один раз: gh auth login.

    Установка (из клона репозитория, PowerShell не от администратора):
      powershell -ExecutionPolicy Bypass -File scripts\hw-bot.ps1 -Install
    Удаление:
      powershell -ExecutionPolicy Bypass -File scripts\hw-bot.ps1 -Uninstall
    Один прогон сейчас:
      powershell -ExecutionPolicy Bypass -File scripts\hw-bot.ps1

.PARAMETER EveryHours
    Как часто проверять наличие новых сборок (для -Install), часов.
#>
[CmdletBinding()]
param(
    [switch]$Install,
    [switch]$Uninstall,
    [int]$EveryHours = 2
)
$ErrorActionPreference = 'Stop'
$Repo = 'Zaxar1231/GSPC'
$TaskName = 'RustDroid hw-bot'
$BotDir = Join-Path $env:LOCALAPPDATA 'RustDroid\hw-bot'
New-Item -ItemType Directory -Force -Path $BotDir | Out-Null
$LogFile = Join-Path $BotDir 'hw-bot.log'
$Tested = Join-Path $BotDir 'tested.txt'

function Log([string]$Text) {
    $line = '{0:yyyy-MM-dd HH:mm:ss}  {1}' -f (Get-Date), $Text
    Write-Host $line
    Add-Content -Path $LogFile -Value $line -Encoding UTF8
}

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Log 'задача планировщика удалена'
    return
}

if ($Install) {
    # Копия скрипта: задача не зависит от того, какая ветка сейчас в клоне.
    $self = Join-Path $BotDir 'hw-bot.ps1'
    Copy-Item -LiteralPath $PSCommandPath -Destination $self -Force
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Minimized -File `"$self`""
    $trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(5) `
        -RepetitionInterval (New-TimeSpan -Hours $EveryHours)
    # Только при входе пользователя: окну Android и видеокарте нужен рабочий стол.
    $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive
    $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -MultipleInstances IgnoreNew `
        -ExecutionTimeLimit (New-TimeSpan -Hours 1) -DisallowStartIfOnBatteries -StopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal `
        -Settings $settings -Force | Out-Null
    Log "задача «$TaskName» установлена: раз в $EveryHours ч, первый запуск через 5 мин"
    return
}

# ------------------------------------------------------------- один прогон ---

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Log 'нет GitHub CLI (gh) — пропуск'; return }
$battery = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue
if ($battery -and $battery.BatteryStatus -ne 2) { Log 'работа от батареи — пропуск'; return }
if (Get-Process rustdroid -ErrorAction SilentlyContinue) { Log 'RustDroid уже запущен — пропуск'; return }

$done = if (Test-Path $Tested) { @(Get-Content $Tested) } else { @() }
$since = (Get-Date).ToUniversalTime().AddDays(-7)
$releases = gh release list -R $Repo --limit 50 --json tagName,publishedAt | ConvertFrom-Json |
    Where-Object { $_.tagName -like 'nightly-*' -and [datetime]$_.publishedAt -gt $since }

foreach ($rel in $releases) {
    $tag = $rel.tagName
    $commitUrl = "https://github.com/$Repo/releases/download/$tag/commit.txt"
    try { $commit = (Invoke-WebRequest -UseBasicParsing $commitUrl).Content.Trim() }
    catch { Log "${tag}: нет commit.txt — пропуск"; continue }
    if ($done -contains $commit) { continue }

    # Ветка в имени тега: «/» заменён на «-»; hw-test строит тот же тег из такого имени.
    $branch = $tag.Substring('nightly-'.Length)
    Log "${tag}: новый коммит $($commit.Substring(0, 7)) — запускаю hw-test"
    $script = Join-Path $BotDir 'hw-test.ps1'
    Invoke-WebRequest -UseBasicParsing "https://raw.githubusercontent.com/$Repo/$commit/scripts/hw-test.ps1" -OutFile $script
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $script -Branch $branch -Yes -Agent -Stage3Seconds 420 *> (Join-Path $BotDir ('run-{0:yyyyMMdd-HHmmss}.log' -f (Get-Date)))
    Log "${tag}: hw-test завершён, код $LASTEXITCODE"
    # Отмечаем коммит и при неудаче: иначе бот гонял бы сломанную сборку каждые N часов.
    Add-Content -Path $Tested -Value $commit -Encoding ASCII
    $done += $commit
}
Log 'проверка завершена'
