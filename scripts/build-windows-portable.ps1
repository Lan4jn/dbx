param(
  [ValidateSet("All", "Modern", "Legacy")]
  [string]$BuildSet = "All",
  [string]$Target = "x86_64-pc-windows-msvc",
  [string]$LegacyRustToolchain = "1.77.2",
  [string]$WebView2Source = "",
  [string]$ModernWebView2InstallerSource = "",
  [string]$WebView2InstallerSource = "",
  [switch]$SkipFrontendBuild,
  [switch]$SkipRustBuild
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$version = (Get-Content -LiteralPath (Join-Path $repoRoot "package.json") -Raw | ConvertFrom-Json).version
$legacyWebView2InstallerVersion = "109.0.1518.78"

function Move-ExistingBuildArtifact {
  param(
    [string]$Path,
    [string]$AllowedRoot,
    [string]$ArchiveRoot,
    [string]$Stamp
  )

  if (-not (Test-Path -LiteralPath $Path)) {
    return
  }

  $resolvedPath = (Resolve-Path -LiteralPath $Path).Path
  $resolvedRoot = (Resolve-Path -LiteralPath $AllowedRoot).Path
  if (-not $resolvedPath.StartsWith($resolvedRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Unexpected build artifact path: $resolvedPath"
  }

  New-Item -ItemType Directory -Path $ArchiveRoot -Force | Out-Null

  $leaf = Split-Path -Leaf $Path
  $destination = Join-Path $ArchiveRoot "$Stamp-$leaf"
  $suffix = 1
  while (Test-Path -LiteralPath $destination) {
    $destination = Join-Path $ArchiveRoot "$Stamp-$suffix-$leaf"
    $suffix += 1
  }

  Move-Item -LiteralPath $Path -Destination $destination -Force
}

function Invoke-DbxCargoBuild {
  param(
    [string]$Manifest,
    [string]$TargetDir,
    [string]$Toolchain
  )

  Push-Location $repoRoot
  try {
    $env:CARGO_TARGET_DIR = $TargetDir
    $env:CXXFLAGS = "/utf-8 /EHsc"
    $env:CXXFLAGS_x86_64_pc_windows_msvc = "/utf-8 /EHsc"
    if ($Toolchain) {
      cargo "+$Toolchain" build --manifest-path $Manifest --target $Target --release --features custom-protocol --ignore-rust-version
    } else {
      cargo build --manifest-path $Manifest --target $Target --release --features custom-protocol
    }
  } finally {
    Pop-Location
  }
}

function New-DbxPortablePackage {
  param(
    [string]$ReleaseDir,
    [string]$ExePath,
    [string]$PackageName,
    [ValidateSet("None", "FixedRuntime", "EvergreenInstaller")]
    [string]$WebView2Mode,
    [string]$WebView2InstallerSource,
    [string]$BuildLabel,
    [string]$RustLabel,
    [string]$ArchiveStamp
  )

  if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) {
    throw "Executable was not produced: $ExePath"
  }

  $portableRoot = Join-Path $ReleaseDir "bundle\portable"
  $oldReleaseRoot = Join-Path $ReleaseDir "bundle\old-release"
  $packageDir = Join-Path $portableRoot $PackageName
  $zipPath = Join-Path $portableRoot "$PackageName.zip"

  New-Item -ItemType Directory -Path $portableRoot -Force | Out-Null
  Move-ExistingBuildArtifact -Path $packageDir -AllowedRoot $portableRoot -ArchiveRoot $oldReleaseRoot -Stamp $ArchiveStamp
  Move-ExistingBuildArtifact -Path $zipPath -AllowedRoot $portableRoot -ArchiveRoot $oldReleaseRoot -Stamp $ArchiveStamp

  New-Item -ItemType Directory -Path $packageDir | Out-Null
  Copy-Item -LiteralPath $ExePath -Destination (Join-Path $packageDir "DBX.exe") -Force

  if ($WebView2Mode -eq "FixedRuntime") {
    if (-not (Test-Path -LiteralPath (Join-Path $WebView2Source "msedgewebview2.exe") -PathType Leaf)) {
      throw "WebView2 runtime source is missing: $WebView2Source"
    }
    Copy-Item -LiteralPath $WebView2Source -Destination (Join-Path $packageDir "WebView2") -Recurse -Force
    New-Item -ItemType File -Path (Join-Path $packageDir "bundled-webview2.dbx") -Force | Out-Null
  }

  if ($WebView2Mode -eq "EvergreenInstaller") {
    if (-not (Test-Path -LiteralPath $WebView2InstallerSource -PathType Leaf)) {
      throw "WebView2 Evergreen installer is missing: $WebView2InstallerSource"
    }
    $installerDir = Join-Path $packageDir "WebView2Installer"
    New-Item -ItemType Directory -Path $installerDir -Force | Out-Null
    Copy-Item -LiteralPath $WebView2InstallerSource -Destination (Join-Path $installerDir "MicrosoftEdgeWebView2RuntimeInstallerX64.exe") -Force
    New-Item -ItemType File -Path (Join-Path $packageDir "webview2-installer.dbx") -Force | Out-Null
    $uninstallScript = @(
      "@echo off",
      "cd /d ""%~dp0""",
      "DBX.exe --uninstall-webview2"
    )
    Set-Content -LiteralPath (Join-Path $packageDir "Uninstall-DBX-WebView2.cmd") -Value $uninstallScript -Encoding ASCII
  }

  $webview2Line = switch ($WebView2Mode) {
    "FixedRuntime" { "- This package includes WebView2 Fixed Runtime. DBX uses the bundled fixed WebView2 runtime from the WebView2 directory." }
    "EvergreenInstaller" { "- This package includes the WebView2 Evergreen Standalone Installer. DBX installs WebView2 if no system runtime is found." }
    default { "- This package does not include WebView2 Runtime. Install WebView2 on the target machine before running DBX." }
  }

  $readme = @(
    "DBX Windows portable package",
    "",
    "Build target: $BuildLabel",
    "",
    "Run DBX.exe directly. No cmd launcher is required.",
    "",
    "Data directory:",
    "- Default: ~/.dbx",
    "- You can change it in Settings > Appearance > Configuration directory.",
    "- This package does not use portable.dbx or a package-local data directory by default.",
    "",
    "WebView2:",
    $webview2Line,
    "- Startup diagnostics are written to dbx-webview2-startup.log next to DBX.exe.",
    "",
    "Build:",
    "- Built through $BuildLabel.",
    "- Rust toolchain: $RustLabel."
  )
  Set-Content -LiteralPath (Join-Path $packageDir "README-portable.txt") -Value $readme -Encoding UTF8

  Compress-Archive -Path (Join-Path $packageDir "*") -DestinationPath $zipPath -CompressionLevel Optimal -Force
  Get-FileHash -LiteralPath $zipPath -Algorithm SHA256 | Select-Object Path, Hash
}

function Test-WebView2RuntimeSource {
  param([string]$Path)

  return $Path -and (Test-Path -LiteralPath (Join-Path $Path "msedgewebview2.exe") -PathType Leaf)
}

function Set-WebView2SourceFromCandidate {
  param([string]$Path)

  if (-not (Test-WebView2RuntimeSource -Path $Path)) {
    $script:WebView2Source = $Path
    return
  }

  $resolvedSource = (Resolve-Path -LiteralPath $Path).Path
  $stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("dbx-webview2-runtime-{0}-{1}" -f $PID, (Get-Date -Format "yyyyMMddHHmmss"))
  New-Item -ItemType Directory -Path $stageRoot -Force | Out-Null
  Copy-Item -Path (Join-Path $resolvedSource "*") -Destination $stageRoot -Recurse -Force
  $script:WebView2Source = $stageRoot
}

function Resolve-WebView2Source {
  if ($WebView2Source) {
    Set-WebView2SourceFromCandidate -Path (Resolve-Path -LiteralPath $WebView2Source).Path
    return
  }

  $candidateRoots = @(
    (Join-Path $repoRoot "src-tauri\target-win-x64\$Target\release\bundle\portable"),
    (Join-Path $repoRoot "src-tauri\target-win-x64\$Target\release\bundle\old-release"),
    (Join-Path $repoRoot "src-tauri-legacy\target-win7-x64\$Target\release\bundle\portable"),
    (Join-Path $repoRoot "src-tauri-legacy\target-win7-x64\$Target\release\bundle\old-release")
  )

  foreach ($root in $candidateRoots) {
    if (-not (Test-Path -LiteralPath $root)) {
      continue
    }

    $candidate = Get-ChildItem -LiteralPath $root -Directory -Recurse -ErrorAction SilentlyContinue |
      Where-Object { $_.Name -eq "WebView2" -and (Test-WebView2RuntimeSource -Path $_.FullName) } |
      Sort-Object LastWriteTime -Descending |
      Select-Object -First 1

    if ($candidate) {
      Set-WebView2SourceFromCandidate -Path $candidate.FullName
      return
    }
  }

  $script:WebView2Source = Join-Path $repoRoot "WebView2"
}

function Resolve-WebView2InstallerSource {
  param(
    [string]$OverridePath,
    [string]$DefaultPath,
    [string]$MissingMessage
  )

  if ($OverridePath) {
    return (Resolve-Path -LiteralPath $OverridePath).Path
  }

  if (-not (Test-Path -LiteralPath $DefaultPath -PathType Leaf)) {
    throw $MissingMessage
  }
  return (Resolve-Path -LiteralPath $DefaultPath).Path
}

function Resolve-LegacyWebView2InstallerSource {
  if ($WebView2InstallerSource) {
    return Resolve-WebView2InstallerSource -OverridePath $WebView2InstallerSource -DefaultPath "" -MissingMessage ""
  }

  $installerRoot = Join-Path $repoRoot "third-party\webview2-evergreen-$legacyWebView2InstallerVersion"
  $installerPath = Join-Path $installerRoot "MicrosoftEdgeWebView2RuntimeInstallerX64.exe"
  return Resolve-WebView2InstallerSource `
    -OverridePath "" `
    -DefaultPath $installerPath `
    -MissingMessage "WebView2 Evergreen installer $legacyWebView2InstallerVersion is missing: $installerPath. Pass -WebView2InstallerSource to use a verified installer."
}

function Resolve-ModernWebView2InstallerSource {
  if ($ModernWebView2InstallerSource) {
    return Resolve-WebView2InstallerSource -OverridePath $ModernWebView2InstallerSource -DefaultPath "" -MissingMessage ""
  }

  $installerPath = Join-Path $repoRoot "third-party\webview2-evergreen\MicrosoftEdgeWebView2RuntimeInstallerX64.exe"
  return Resolve-WebView2InstallerSource `
    -OverridePath "" `
    -DefaultPath $installerPath `
    -MissingMessage "Modern WebView2 Evergreen installer is missing: $installerPath. Download the current x64 Evergreen Standalone Installer from Microsoft or pass -ModernWebView2InstallerSource."
}

function Move-StalePortableArtifacts {
  param(
    [string]$PortableRoot,
    [string[]]$KeepNames,
    [string]$ArchiveStamp
  )

  if (-not (Test-Path -LiteralPath $PortableRoot)) {
    return
  }

  $oldReleaseRoot = Join-Path (Split-Path -Parent $PortableRoot) "old-release"
  New-Item -ItemType Directory -Path $oldReleaseRoot -Force | Out-Null

  Get-ChildItem -LiteralPath $PortableRoot | Where-Object { $KeepNames -notcontains $_.Name } | ForEach-Object {
    Move-ExistingBuildArtifact -Path $_.FullName -AllowedRoot $PortableRoot -ArchiveRoot $oldReleaseRoot -Stamp $ArchiveStamp
  }
}

$buildModern = $BuildSet -eq "All" -or $BuildSet -eq "Modern"
$buildLegacy = $BuildSet -eq "All" -or $BuildSet -eq "Legacy"

if ($buildModern) {
  $modernResolvedWebView2InstallerSource = Resolve-ModernWebView2InstallerSource
}
if ($buildLegacy) {
  $legacyResolvedWebView2InstallerSource = Resolve-LegacyWebView2InstallerSource
}

if (-not $SkipFrontendBuild) {
  Push-Location $repoRoot
  try {
    pnpm build
  } finally {
    Pop-Location
  }
}

if ($buildModern) {
  $modernManifest = Join-Path $repoRoot "src-tauri\Cargo.toml"
  $modernTargetDir = Join-Path $repoRoot "src-tauri\target-win-x64"
  if (-not $SkipRustBuild) {
    Invoke-DbxCargoBuild -Manifest $modernManifest -TargetDir $modernTargetDir -Toolchain ""
  }
  $modernReleaseDir = Join-Path $modernTargetDir "$Target\release"
  $modernExePath = Join-Path $modernReleaseDir "dbx.exe"
  $modernStamp = Get-Date -Format "yyyyMMdd-HHmmss"
  New-DbxPortablePackage -ReleaseDir $modernReleaseDir -ExePath $modernExePath -PackageName "DBX_${version}_x64-portable-modern" -WebView2Mode "None" -BuildLabel "modern Windows" -RustLabel "default" -ArchiveStamp $modernStamp
  New-DbxPortablePackage -ReleaseDir $modernReleaseDir -ExePath $modernExePath -PackageName "DBX_${version}_x64-portable-modern-webview2" -WebView2Mode "EvergreenInstaller" -WebView2InstallerSource $modernResolvedWebView2InstallerSource -BuildLabel "modern Windows" -RustLabel "default" -ArchiveStamp $modernStamp
  $modernKeepNames = @(
    "DBX_${version}_x64-portable-modern",
    "DBX_${version}_x64-portable-modern.zip",
    "DBX_${version}_x64-portable-modern-webview2",
    "DBX_${version}_x64-portable-modern-webview2.zip"
  )
}

if ($buildLegacy) {
  $legacyManifest = Join-Path $repoRoot "src-tauri-legacy\Cargo.toml"
  $legacyTargetDir = Join-Path $repoRoot "src-tauri-legacy\target-win7-x64"
  if (-not $SkipRustBuild) {
    Invoke-DbxCargoBuild -Manifest $legacyManifest -TargetDir $legacyTargetDir -Toolchain $LegacyRustToolchain
  }
  $legacyReleaseDir = Join-Path $legacyTargetDir "$Target\release"
  $legacyExePath = Join-Path $legacyReleaseDir "dbx.exe"
  $legacyStamp = Get-Date -Format "yyyyMMdd-HHmmss"
  New-DbxPortablePackage -ReleaseDir $legacyReleaseDir -ExePath $legacyExePath -PackageName "DBX_${version}_x64-portable-win7-win8-legacy" -WebView2Mode "None" -BuildLabel "legacy Windows 7/8" -RustLabel $LegacyRustToolchain -ArchiveStamp $legacyStamp
  New-DbxPortablePackage -ReleaseDir $legacyReleaseDir -ExePath $legacyExePath -PackageName "DBX_${version}_x64-portable-win7-win8-legacy-webview2" -WebView2Mode "EvergreenInstaller" -WebView2InstallerSource $legacyResolvedWebView2InstallerSource -BuildLabel "legacy Windows 7/8" -RustLabel $LegacyRustToolchain -ArchiveStamp $legacyStamp
  $legacyKeepNames = @(
    "DBX_${version}_x64-portable-win7-win8-legacy",
    "DBX_${version}_x64-portable-win7-win8-legacy.zip",
    "DBX_${version}_x64-portable-win7-win8-legacy-webview2",
    "DBX_${version}_x64-portable-win7-win8-legacy-webview2.zip"
  )
}

$cleanupStamp = Get-Date -Format "yyyyMMdd-HHmmss"
if ($buildModern) {
  Move-StalePortableArtifacts -PortableRoot (Join-Path $repoRoot "src-tauri\target-win-x64\$Target\release\bundle\portable") -KeepNames $modernKeepNames -ArchiveStamp $cleanupStamp
}
if ($buildLegacy) {
  Move-StalePortableArtifacts -PortableRoot (Join-Path $repoRoot "src-tauri-legacy\target-win7-x64\$Target\release\bundle\portable") -KeepNames $legacyKeepNames -ArchiveStamp $cleanupStamp
}
