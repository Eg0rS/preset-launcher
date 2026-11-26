# Builds the release exe and the Inno Setup installer.
# Output: installer\Output\PresetLauncher-Setup-<version>.exe
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$env:Path += ";$env:USERPROFILE\.cargo\bin"
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)" }

& $iscc /Qp "installer\setup.iss"
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
Get-ChildItem installer\Output\*.exe | Select-Object FullName, Length
