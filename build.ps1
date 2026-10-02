# Builds the release exe into dist\claude-usage.exe
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
Stop-Process -Name claude-usage -ErrorAction SilentlyContinue # running exe locks the file
cargo build --release
if ($LASTEXITCODE) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\claude-usage.exe dist\ -Force
Get-Item dist\claude-usage.exe
