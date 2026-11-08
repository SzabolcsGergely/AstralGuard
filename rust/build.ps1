# Builds astralguard.exe (release) and copies it to the project folder, next to config.ini
# and logs\, where the scheduled task runs it.
#
# Toolchain: rustup's x86_64-pc-windows-gnu with the llvm-tools component. That toolchain
# ships no GNU assembler, so raw-dylib import libraries are made by LLVM's dlltool; this
# script provides one (a copy of llvm-ar, which acts as dlltool under that name) and
# .cargo\config.toml links with rust-lld. No Visual Studio needed.
$ErrorActionPreference = "Stop"

$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Split-Path -Parent $Here
$Cargo = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path (Join-Path $Cargo "cargo.exe")) { $env:PATH = "$Cargo;$env:PATH" }

$Sysroot = (& rustc --print sysroot).Trim()
$RustlibBin = Join-Path $Sysroot "lib\rustlib\x86_64-pc-windows-gnu\bin"
$LlvmAr = Join-Path $RustlibBin "llvm-ar.exe"
if (!(Test-Path $LlvmAr)) {
    & rustup component add llvm-tools
}
$DlltoolDir = Join-Path $env:USERPROFILE ".cargo\dlltool"
$Dlltool = Join-Path $DlltoolDir "dlltool.exe"
if (!(Test-Path $Dlltool) -or (Get-Item $Dlltool).Length -ne (Get-Item $LlvmAr).Length) {
    New-Item -ItemType Directory -Force $DlltoolDir | Out-Null
    Copy-Item $LlvmAr $Dlltool -Force
}
$env:PATH = "$DlltoolDir;$(Join-Path $RustlibBin 'self-contained');$env:PATH"

Push-Location $Here
try {
    # cargo reports progress on stderr, which Windows PowerShell turns into errors.
    $ErrorActionPreference = "Continue"
    & cargo build --release 2>&1 | ForEach-Object { "$_" }
    $ErrorActionPreference = "Stop"
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

$Exe = Join-Path $Here "target\release\astralguard.exe"
Copy-Item $Exe (Join-Path $Root "astralguard.exe") -Force
Write-Host "Built $(Join-Path $Root 'astralguard.exe')"
