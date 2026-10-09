# CI only: installs rodu the way Windows users do (irm | iex) from the freshly built release
# files, checks the user PATH survives, upgrades while rodu runs, then smoke-tests the binary.
$ErrorActionPreference = 'Stop'
$env:RODU_VERSION = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$env:RODU_INSTALL_DIR = "$env:RUNNER_TEMP\rodu"
$env:RODU_DOWNLOAD_BASE = "$env:GITHUB_WORKSPACE\dist\release"
# An expandable entry like the Windows default %USERPROFILE%\...\WindowsApps must survive.
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
$before = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
$key.SetValue('Path', ($before.TrimEnd(';') + ';%USERPROFILE%\rodu-ci-marker').TrimStart(';'), 'ExpandString')
Get-Content -Raw packaging/install.ps1 | Invoke-Expression
$after = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
if ($key.GetValueKind('Path') -ne 'ExpandString') { throw 'user PATH lost REG_EXPAND_SZ' }
if ($after -notlike '*%USERPROFILE%\rodu-ci-marker*') { throw "user PATH was expanded: $after" }
if ($after -notlike "*$env:RODU_INSTALL_DIR*") { throw 'install dir not on user PATH' }
foreach ($n in 'LICENSE', 'NOTICE', 'THIRD-PARTY-NOTICES.txt') {
  if (-not (Test-Path "$env:RODU_INSTALL_DIR\rodu-notices\$n")) { throw "$n was not installed" }
}
# A second run upgrades in place while rodu.exe is running.
$ws = New-Item -ItemType Directory -Force "$env:RUNNER_TEMP\ws"
Push-Location $ws; rodu init --name ci --key CI; Pop-Location
$running = Start-Process -PassThru -WindowStyle Hidden -WorkingDirectory $ws "$env:RODU_INSTALL_DIR\rodu.exe" -ArgumentList 'web', '--no-open', '--port', '0'
Start-Sleep -Seconds 2
if ($running.HasExited) { throw 'rodu web did not keep running' }
Get-Content -Raw packaging/install.ps1 | Invoke-Expression
# Again, while the first renamed copy is still running.
Get-Content -Raw packaging/install.ps1 | Invoke-Expression
Stop-Process -Id $running.Id -Force
rodu --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo xtask smoke "$env:RODU_INSTALL_DIR\rodu.exe"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
