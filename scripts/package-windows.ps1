# Builds the Windows installer and a zip of ZP into dist\.
# Needs Inno Setup (iscc.exe) on PATH for the installer. The icon is the committed
# packaging\windows\zp.ico (generated from assets\icon.png).
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"').Matches[0].Groups[1].Value
$numericVersion = ($version -split '-')[0]
cargo build --release -p zp-app
if ($LASTEXITCODE -ne 0) { throw "Build failed" }
New-Item -ItemType Directory -Force dist | Out-Null
$exe = Resolve-Path "target\release\zp.exe"

Compress-Archive -Force -Path $exe -DestinationPath "dist\ZP-$version-windows-x86_64.zip"

if (Get-Command iscc -ErrorAction SilentlyContinue) {
    $source = $exe.Path
    $output = (Resolve-Path dist).Path
    iscc "/DVersion=$version" "/DNumericVersion=$numericVersion" "/DSource=$source" "/DOutput=$output" packaging\windows\zp.iss
    if ($LASTEXITCODE -ne 0) { throw "Installer build failed" }
} else {
    Write-Warning "iscc (Inno Setup) not found; only the zip was built"
}
Get-ChildItem dist
