param(
  [string]$Target = "x86_64-pc-windows-msvc",
  [string]$RustToolchain = "1.77.2",
  [string]$WebView2Source = "",
  [string]$WebView2InstallerSource = "",
  [switch]$SkipFrontendBuild
)

$ErrorActionPreference = "Stop"

$argsList = @(
  "-ExecutionPolicy", "Bypass",
  "-File", (Join-Path $PSScriptRoot "build-windows-portable.ps1"),
  "-BuildSet", "Legacy",
  "-Target", $Target,
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

& powershell @argsList
exit $LASTEXITCODE
