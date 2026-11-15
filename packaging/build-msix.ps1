<#
.SYNOPSIS
  Builds the MSIX package of RustPlay (Microsoft Store or sideloading).

.EXAMPLE
  # Store upload: use the identity Partner Center shows under "Product identity". Store packages are not signed here.
  ./packaging/build-msix.ps1 -IdentityName "12345Name.RustPlay" -Publisher "CN=ABCD-1234" -PublisherDisplayName "Your name"

.EXAMPLE
  # Local test: signs with a self-signed certificate (created on first use).
  ./packaging/build-msix.ps1 -Sign
#>
param(
    [string]$IdentityName = "RustPlay",
    [string]$Publisher = "CN=RustPlay Dev",
    [string]$PublisherDisplayName = "RustPlay",
    [switch]$Sign,
    [string]$CertificatePath,
    [string]$CertificatePassword = "rustplay",
    [switch]$SkipBuild
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root "target\msix"
$layout = Join-Path $out "layout"

$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Matches[0].Groups[1].Value
$msixVersion = "$version.0"

function Find-SdkTool([string]$name) {
    $bin = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    $tool = Get-ChildItem -Path $bin -Filter $name -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "\\x64\\" } |
        Sort-Object { [version]($_.FullName -replace '.*\\bin\\([0-9.]+)\\.*', '$1') } -Descending |
        Select-Object -First 1
    if (-not $tool) { throw "No se encontró $name. Instala el SDK de Windows 10/11." }
    $tool.FullName
}

if (-not $SkipBuild) {
    Write-Host "Compilando RustPlay $version…"
    cargo build --release -p rustplay --manifest-path (Join-Path $root "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "La compilación falló." }
}

Remove-Item $layout -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $layout "Assets") | Out-Null
Copy-Item (Join-Path $root "target\release\RustPlay.exe") $layout
Copy-Item (Join-Path $root "LICENSE"), (Join-Path $root "NOTICE") $layout
Copy-Item (Join-Path $PSScriptRoot "msix\Assets\*.png") (Join-Path $layout "Assets")

$manifest = Get-Content (Join-Path $PSScriptRoot "msix\AppxManifest.xml") -Raw
$manifest = $manifest.Replace('$(IdentityName)', $IdentityName).Replace('$(Publisher)', $Publisher).
    Replace('$(PublisherDisplayName)', $PublisherDisplayName).Replace('$(Version)', $msixVersion)
Set-Content -Path (Join-Path $layout "AppxManifest.xml") -Value $manifest -Encoding UTF8

# resources.pri lets Windows pick Assets\*.scale-200.png and friends from the plain names in the manifest.
$makepri = Find-SdkTool "makepri.exe"
$priConfig = Join-Path $out "priconfig.xml"
& $makepri createconfig /cf $priConfig /dq es-ES_en-US /pv 10.0.0 /o | Out-Null
& $makepri new /pr $layout /cf $priConfig /mn (Join-Path $layout "AppxManifest.xml") /of (Join-Path $layout "resources.pri") /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri falló." }

$package = Join-Path $out "RustPlay_${msixVersion}_x64.msix"
$makeappx = Find-SdkTool "makeappx.exe"
& $makeappx pack /o /d $layout /p $package | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx falló." }

if ($Sign) {
    if (-not $CertificatePath) {
        $CertificatePath = Join-Path $out "RustPlay-dev.pfx"
        if (-not (Test-Path $CertificatePath)) {
            Write-Host "Creando un certificado autofirmado para «$Publisher»…"
            $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher -KeyUsage DigitalSignature `
                -FriendlyName "RustPlay (desarrollo)" -CertStoreLocation "Cert:\CurrentUser\My" `
                -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
            $secure = ConvertTo-SecureString $CertificatePassword -AsPlainText -Force
            Export-PfxCertificate -Cert $cert -FilePath $CertificatePath -Password $secure | Out-Null
            Export-Certificate -Cert $cert -FilePath (Join-Path $out "RustPlay-dev.cer") | Out-Null
        }
    }
    $signtool = Find-SdkTool "signtool.exe"
    & $signtool sign /fd SHA256 /f $CertificatePath /p $CertificatePassword $package | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool falló." }
    Write-Host "Paquete firmado. Para instalarlo en este PC, importa una vez el certificado (como administrador):"
    Write-Host "  Import-Certificate -FilePath `"$(Join-Path $out 'RustPlay-dev.cer')`" -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
}

Write-Host "Listo: $package"
