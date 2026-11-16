# Builds the Windows installer and a zip of ZP into dist\.
# Needs Inno Setup (iscc.exe) on PATH for the installer; ImageMagick (magick) for the icon.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"').Matches[0].Groups[1].Value
cargo build --release -p zp-app
New-Item -ItemType Directory -Force dist | Out-Null
$exe = Resolve-Path "target\release\zp.exe"

Compress-Archive -Force -Path $exe -DestinationPath "dist\ZP-$version-windows-x86_64.zip"

if (Get-Command magick -ErrorAction SilentlyContinue) {
    magick assets\icon.png -define icon:auto-resize=256,64,48,32,16 packaging\windows\zp.ico
}
if (Get-Command iscc -ErrorAction SilentlyContinue) {
    iscc /DVersion=$version /DSource=$exe /DOutput=(Resolve-Path dist) packaging\windows\zp.iss
} else {
    Write-Warning "iscc (Inno Setup) not found; only the zip was built"
}
Get-ChildItem dist
