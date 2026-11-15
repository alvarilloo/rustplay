<#
.SYNOPSIS
  Builds RustPlay-Setup-<version>.exe with Inno Setup 6 (https://jrsoftware.org/isinfo.php).
#>
param([switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Matches[0].Groups[1].Value

$iscc = @(
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "No se encontró Inno Setup 6. Instálalo desde https://jrsoftware.org/isinfo.php o con: winget install JRSoftware.InnoSetup" }

if (-not $SkipBuild) {
    Write-Host "Compilando RustPlay $version…"
    cargo build --release -p rustplay -p rustplay-cli --manifest-path (Join-Path $root "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "La compilación falló." }
}

& $iscc "/DAppVersion=$version" (Join-Path $PSScriptRoot "inno\RustPlay.iss")
if ($LASTEXITCODE -ne 0) { throw "Inno Setup falló." }
Write-Host "Listo: $(Join-Path $root "target\installer\RustPlay-Setup-$version.exe")"
