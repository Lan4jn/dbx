param(
  [string]$Target = "x86_64-win7-windows-msvc",
  [string]$RustToolchain = "nightly-2026-07-22",
  [string]$WebView2Source = "",
  [string]$WebView2InstallerSource = "",
  [switch]$SkipFrontendBuild
)

$ErrorActionPreference = "Stop"

$argsList = @(
  "-ExecutionPolicy", "Bypass",
  "-File", (Join-Path $PSScriptRoot "build-windows-portable.ps1"),
  "-BuildSet", "Legacy",
  "-LegacyTarget", $Target,
  "-LegacyRustToolchain", $RustToolchain
)
if ($WebView2Source) {
  $argsList += @("-WebView2Source", $WebView2Source)
}
if ($WebView2InstallerSource) {
  $argsList += @("-WebView2InstallerSource", $WebView2InstallerSource)
}
if ($SkipFrontendBuild) {
  $argsList += "-SkipFrontendBuild"
}

& pwsh @argsList
exit $LASTEXITCODE
