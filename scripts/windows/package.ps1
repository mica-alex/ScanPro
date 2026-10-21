# Build ScanPros Windows installers (NSIS setup.exe + MSI) into dist\.
# Needs: Rust (MSVC toolchain), cargo-packager (cargo install cargo-packager --locked).
# WiX (for MSI) and NSIS are downloaded by cargo-packager on first use.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..\..")
if (-not (Get-Command cargo-packager -ErrorAction SilentlyContinue)) {
    Write-Error "Install cargo-packager: cargo install cargo-packager --locked"
}
cargo packager --release --formats nsis,wix @args
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Get-ChildItem dist
