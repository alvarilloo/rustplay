<#
.SYNOPSIS
  Builds dist\RustPlay-<version>-portable.zip: RustPlay.exe, rustplay-cli.exe, portable.txt, LICENSE, NOTICE and a README.

.DESCRIPTION
  The portable build is the normal executable plus an empty `portable.txt` next to it. With that file
  RustPlay keeps everything (identity, pairings, settings, logs) in a `data` folder next to the exe,
  puts recordings and screenshots in `Recordings` and `Screenshots` next to the exe, and writes nothing
  to the registry (no start-with-Windows entry, no notification identity). It does not update itself.
  Nothing here signs or uploads anything.
#>
param([switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Matches[0].Groups[1].Value

if (-not $SkipBuild) {
    Write-Host "Compilando RustPlay $version…"
    cargo build --release -p rustplay -p rustplay-cli --manifest-path (Join-Path $root "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "La compilación falló." }
}

$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
$release = Join-Path $targetDir "release"
foreach ($file in "RustPlay.exe", "rustplay-cli.exe") {
    if (-not (Test-Path (Join-Path $release $file))) { throw "Falta $file en $release. Compila sin -SkipBuild." }
}

$dist = Join-Path $root "dist"
$stage = Join-Path $dist "RustPlay-$version-portable"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item (Join-Path $release "RustPlay.exe") $stage
Copy-Item (Join-Path $release "rustplay-cli.exe") $stage
Copy-Item (Join-Path $root "LICENSE") $stage
Copy-Item (Join-Path $root "NOTICE") $stage
# The marker file: its presence is what switches RustPlay to portable mode.
Set-Content -Path (Join-Path $stage "portable.txt") -Encoding utf8 -Value @"
This file turns RustPlay into its portable version. Do not delete it.
Este archivo convierte RustPlay en su versión portátil. No lo borres.
"@

Set-Content -Path (Join-Path $stage "README.txt") -Encoding utf8 -Value @"
RustPlay $version (portable version / versión portátil)

ENGLISH
- Unzip this folder anywhere (a USB stick works) and run RustPlay.exe. No installer, no administrator rights.
- Because portable.txt sits next to RustPlay.exe, all data lives in the "data" folder here: identity, paired
  devices, settings and logs. Recordings go to "Recordings" and screenshots to "Screenshots" (change them in
  Settings). Delete the folder to remove everything.
- Nothing is written to the registry: no start-with-Windows option, no Start-menu shortcut, and notifications
  appear as notification-area balloons.
- It does not update itself. "Check for updates" only links to the release page; download the new zip and copy
  RustPlay.exe and rustplay-cli.exe over the old ones (keep "data" and portable.txt).
- Windows Firewall will ask to allow RustPlay the first time: allow it on private networks. Moving the folder
  makes Windows ask again.
- rustplay-cli.exe works the same way and shares the data folder.

ESPAÑOL
- Descomprime esta carpeta donde quieras (sirve un USB) y ejecuta RustPlay.exe. Sin instalador ni permisos de
  administrador.
- Como portable.txt está junto a RustPlay.exe, todos los datos viven en la carpeta "data": identidad,
  dispositivos emparejados, ajustes y registros. Las grabaciones van a "Recordings" y las capturas a
  "Screenshots" (se cambian en Ajustes). Borra la carpeta para eliminarlo todo.
- No se escribe nada en el registro: no hay inicio con Windows ni acceso directo en el menú Inicio, y las
  notificaciones salen como globos del área de notificación.
- No se actualiza solo. «Buscar actualizaciones» solo enlaza a la página de la versión; descarga el zip nuevo y
  copia RustPlay.exe y rustplay-cli.exe sobre los antiguos (conserva "data" y portable.txt).
- El Firewall de Windows pedirá permiso la primera vez: permítelo en redes privadas. Si mueves la carpeta,
  Windows volverá a preguntar.
- rustplay-cli.exe funciona igual y comparte la carpeta de datos.
"@

$zip = Join-Path $dist "RustPlay-$version-portable.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item -Recurse -Force $stage

$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
Set-Content -Path "$zip.sha256" -Encoding ascii -Value "$hash  $(Split-Path -Leaf $zip)"
Write-Host "Listo: $zip"
Write-Host "SHA-256: $hash"
