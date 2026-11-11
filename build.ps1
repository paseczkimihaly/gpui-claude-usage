# Builds the release exe into dist\claude-usage-<version>.exe (version from Cargo.toml)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Stop-Process -Name claude-usage* -ErrorAction SilentlyContinue # running exe locks the file
cargo build --release
if ($LASTEXITCODE) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\claude-usage.exe "dist\claude-usage-$version.exe" -Force
Get-Item "dist\claude-usage-$version.exe"
