# AstralGuard - Portable Installer (Windows)
#
# Registers astralguard.exe (next to this script) as a logon task that runs elevated. With no
# arguments astralguard.exe is the watchdog, which runs the protection service and the tray/
# dashboard and restarts either if it crashes.
#   -StartNow   start it right away without asking (for scripted installs)
param([switch]$StartNow)
$ErrorActionPreference = "Stop"

function Write-Info($msg) { Write-Host "[INFO]  $msg" -ForegroundColor Cyan }
function Write-Warn($msg) { Write-Host "[WARN]  $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "[ERROR] $msg" -ForegroundColor Red }

$IsAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $IsAdmin) {
    Write-Err "Administrator rights required (Task Scheduler / Event Log registration)."
    Write-Err "Right-click install.bat and choose 'Run as administrator'."
    exit 1
}

$BaseDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ExePath = Join-Path $BaseDir "astralguard.exe"
$ConfigPath = Join-Path $BaseDir "config.ini"
$LogsDir = Join-Path $BaseDir "logs"

$TaskName = "AstralGuard"
$EventSource = "AstralGuard"

# Upgrades: a still-running guard keeps the old program file locked and holds the
# single-instance lock against the new one. Stop it before touching anything.
foreach ($Name in @($TaskName)) {
    if (Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue) {
        Write-Info "Stopping the running guard ($Name) to upgrade it."
        try { Stop-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue } catch {}
    }
}
Start-Sleep -Seconds 2

if (!(Test-Path $ExePath)) {
    $Build = Join-Path $BaseDir "rust\build.ps1"
    if ((Test-Path $Build) -and (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Info "astralguard.exe not found - building it from source."
        & $Build
    }
}
if (!(Test-Path $ExePath)) {
    Write-Err "astralguard.exe not found next to install.ps1."
    Write-Err "Download a release, or build it with rust\build.ps1 (needs Rust from https://rustup.rs)."
    exit 1
}

if (!(Test-Path $LogsDir)) { New-Item -ItemType Directory -Path $LogsDir | Out-Null }

if (!(Test-Path $ConfigPath)) {
@"
[Settings]
threshold_amps = 9.5
sustained_seconds_required = 15
response_mode = tiered
"@ | Set-Content -Path $ConfigPath -Encoding UTF8
}

try {
    if (-not [System.Diagnostics.EventLog]::SourceExists($EventSource)) {
        New-EventLog -LogName Application -Source $EventSource
    }
} catch {}

foreach ($Name in @($TaskName)) {
    try { Unregister-ScheduledTask -TaskName $Name -Confirm:$false -ErrorAction SilentlyContinue } catch {}
}

$UserId = "$env:USERDOMAIN\$env:USERNAME"
$Action = New-ScheduledTaskAction -Execute $ExePath -WorkingDirectory $BaseDir
$Trigger = New-ScheduledTaskTrigger -AtLogOn -User $UserId
$Principal = New-ScheduledTaskPrincipal -UserId $UserId -LogonType Interactive -RunLevel Highest
# RestartCount is deliberately high: a watchdog that gives up after a few failures stops
# protecting the card exactly when something is already going wrong.
$Settings = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew

# Without this the task defaults to a 72-hour limit and Task Scheduler kills the guard
# after 3 days of uptime. A time-limit stop is not a "failure", so RestartCount never fires.
$Settings.ExecutionTimeLimit = "PT0S"

Register-ScheduledTask -TaskName $TaskName -Action $Action -Trigger $Trigger -Principal $Principal -Settings $Settings | Out-Null

if ($StartNow) {
    Start-ScheduledTask -TaskName $TaskName
} else {
    $answer = Read-Host "Start AstralGuard now? (y/N)"
    if ($answer -match '^(y|yes)$') {
        Start-ScheduledTask -TaskName $TaskName
    }
}

Write-Info "Install complete."
