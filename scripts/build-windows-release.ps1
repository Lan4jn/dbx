param(
  [ValidateSet("All", "Modern", "Legacy")]
  [string]$BuildSet = "All",
  [string]$ModernTarget = "x86_64-pc-windows-msvc",
  [string]$LegacyTarget = "x86_64-win7-windows-msvc",
  [string]$LegacyRustToolchain = "nightly-2026-07-22",
  [string]$SigningKeyPath = (Join-Path $HOME ".tauri\dbx-updater.key"),
  [string]$ModernBaseUrl = "https://server.sjserver.fun:880/dbx/modern",
  [string]$LegacyBaseUrl = "https://server.sjserver.fun:880/dbx/legacy",
  [switch]$SkipFrontendBuild,
  [switch]$SkipRustBuild
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$version = (Get-Content -LiteralPath (Join-Path $repoRoot "package.json") -Raw | ConvertFrom-Json).version
$buildModern = $BuildSet -eq "All" -or $BuildSet -eq "Modern"
$buildLegacy = $BuildSet -eq "All" -or $BuildSet -eq "Legacy"

function Assert-LastExitCode {
  param([string]$Operation)
  if ($LASTEXITCODE -ne 0) {
    throw "$Operation failed with exit code $LASTEXITCODE"
  }
}

function Write-LatestJson {
  param(
    [string]$InstallerPath,
    [string]$SignaturePath,
    [string]$BaseUrl,
    [string]$Notes
  )

  if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) {
    throw "Installer is missing: $InstallerPath"
  }
  if (-not (Test-Path -LiteralPath $SignaturePath -PathType Leaf)) {
    throw "Updater signature is missing: $SignaturePath"
  }

  $signature = (Get-Content -LiteralPath $SignaturePath -Raw).Trim()
  $fileName = Split-Path -Leaf $InstallerPath
  $latest = [ordered]@{
    version = $version
    notes = $Notes
    pub_date = [DateTime]::UtcNow.ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = [ordered]@{
      "windows-x86_64" = [ordered]@{
        signature = $signature
        url = "$($BaseUrl.TrimEnd('/'))/$fileName"
      }
    }
  }
  $jsonPath = Join-Path (Split-Path -Parent $InstallerPath) "latest.json"
  $latest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $jsonPath -Encoding UTF8
  return $jsonPath
}

function Ensure-Signature {
  param([string]$InstallerPath)

  $signaturePath = "$InstallerPath.sig"
  if (Test-Path -LiteralPath $signaturePath -PathType Leaf) {
    return $signaturePath
  }
  $password = $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD
  if ($null -eq $password) { $password = "" }
  pnpm tauri signer sign --private-key-path $SigningKeyPath --password $password $InstallerPath
  Assert-LastExitCode "Signing $InstallerPath"
  return $signaturePath
}

if (-not (Test-Path -LiteralPath $SigningKeyPath -PathType Leaf)) {
  throw "Tauri updater signing key is missing: $SigningKeyPath"
}

$portableArgs = @(
  "-ExecutionPolicy", "Bypass",
  "-File", (Join-Path $PSScriptRoot "build-windows-portable.ps1"),
  "-BuildSet", $BuildSet,
  "-Target", $ModernTarget,
  "-LegacyTarget", $LegacyTarget,
  "-LegacyRustToolchain", $LegacyRustToolchain
)
if ($SkipFrontendBuild) { $portableArgs += "-SkipFrontendBuild" }
if ($SkipRustBuild) { $portableArgs += "-SkipRustBuild" }
& pwsh @portableArgs
Assert-LastExitCode "Portable package build"

$previousTargetDir = $env:CARGO_TARGET_DIR
$previousSigningKey = $env:TAURI_SIGNING_PRIVATE_KEY
$previousSigningPassword = $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD
try {
  $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content -LiteralPath $SigningKeyPath -Raw
  if ($null -eq $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
  }

  if ($buildModern) {
    $env:CARGO_TARGET_DIR = Join-Path $repoRoot "src-tauri\target-win-x64"
    pnpm tauri build --bundles nsis --target $ModernTarget --ci
    Assert-LastExitCode "Modern NSIS build"

    $modernBundleDir = Join-Path $env:CARGO_TARGET_DIR "$ModernTarget\release\bundle\nsis"
    $modernInstaller = Join-Path $modernBundleDir "DBX_${version}_x64-setup.exe"
    $modernSignature = Ensure-Signature -InstallerPath $modernInstaller
    Write-LatestJson -InstallerPath $modernInstaller -SignaturePath $modernSignature -BaseUrl $ModernBaseUrl -Notes "DBX $version modern Windows x64 release" | Out-Null
  }

  if ($buildLegacy) {
    $env:CARGO_TARGET_DIR = Join-Path $repoRoot "src-tauri\target-win7-x64"
    & (Join-Path $repoRoot ".github\scripts\prepare-webview2-win7-runtime.ps1")
    Assert-LastExitCode "Preparing Windows 7 WebView2 runtime"

    pnpm tauri bundle --bundles nsis --target $LegacyTarget --config src-tauri/tauri.webview2-win7-offline.conf.json
    Assert-LastExitCode "Legacy NSIS build"

    $legacyBundleDir = Join-Path $env:CARGO_TARGET_DIR "$LegacyTarget\release\bundle\nsis"
    $generatedLegacyInstaller = Join-Path $legacyBundleDir "DBX_${version}_x64-setup.exe"
    $legacyInstaller = Join-Path $legacyBundleDir "DBX_${version}_x64-win7-win8-webview2-109-offline-setup.exe"
    Copy-Item -LiteralPath $generatedLegacyInstaller -Destination $legacyInstaller -Force
    Remove-Item -LiteralPath "$legacyInstaller.sig" -Force -ErrorAction SilentlyContinue
    $legacySignature = Ensure-Signature -InstallerPath $legacyInstaller
    Write-LatestJson -InstallerPath $legacyInstaller -SignaturePath $legacySignature -BaseUrl $LegacyBaseUrl -Notes "DBX $version Windows 7/8 x64 offline release" | Out-Null
  }
} finally {
  $env:CARGO_TARGET_DIR = $previousTargetDir
  $env:TAURI_SIGNING_PRIVATE_KEY = $previousSigningKey
  $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $previousSigningPassword
}

Write-Host "Windows release artifacts completed for DBX $version."
