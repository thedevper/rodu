# Installs the rodu binary on Windows (PowerShell 5.1 or 7):
#
#   irm https://raw.githubusercontent.com/TheDevper/rodu/v<version>/packaging/install.ps1 | iex
#
# The URL names a release tag, so what runs is the reviewed script of that release, not
# whatever is on main at the time.
#
# $env:RODU_VERSION = '0.2.0'       a specific release instead of the latest
# $env:RODU_INSTALL_DIR = 'C:\...'  where to put rodu.exe (default %LOCALAPPDATA%\Programs\rodu)
# $env:RODU_DOWNLOAD_BASE = '...'   where the release files are (a URL or folder, for testing)

# A script block keeps preferences and variables out of the caller's session under `iex`.
& {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'  # the progress bar makes Invoke-WebRequest very slow
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $Repo = 'TheDevper/rodu'
  # x64 also runs on Windows on ARM through emulation.
  $Target = 'windows-x64'
  $InstallDir = if ($env:RODU_INSTALL_DIR) { $env:RODU_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\rodu' }

  $Version = $env:RODU_VERSION
  if (-not $Version) {
    $Version = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest").tag_name
  }
  $Version = "$Version".TrimStart('v')
  if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Unexpected rodu version '$Version'" }
  $Base = if ($env:RODU_DOWNLOAD_BASE) { $env:RODU_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/download/v$Version" }
  $Archive = "rodu-v$Version-$Target.zip"

  $Tmp = Join-Path ([IO.Path]::GetTempPath()) ("rodu-" + [Guid]::NewGuid())
  New-Item -ItemType Directory -Path $Tmp | Out-Null
  try {
    $fetch = {
      param($Name)
      $to = Join-Path $Tmp $Name
      if ($Base -match '^https?://') { Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Name" -OutFile $to }
      else { Copy-Item -LiteralPath (Join-Path $Base $Name) -Destination $to }
      $to
    }
    Write-Host "Downloading rodu $Version ($Target)"
    $zip = & $fetch $Archive
    $expected = Get-Content (& $fetch 'SHA256SUMS') |
      ForEach-Object { $f = $_ -split '\s+'; if ($f[1] -eq $Archive) { $f[0] } } |
      Select-Object -First 1
    if (-not $expected) { throw "SHA256SUMS has no entry for $Archive" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLower()
    if ($expected -ne $actual) { throw "Checksum mismatch for $Archive" }

    Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $Tmp 'out') -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $exe = Join-Path $InstallDir 'rodu.exe'
    # A running rodu.exe (say, `rodu mcp` under an agent) cannot be overwritten but can be
    # renamed. Each run uses a fresh name, since an older renamed copy may still be running too.
    # Only names this installer makes (32 hex digits), never a user's own file in a chosen folder.
    Get-ChildItem -LiteralPath $InstallDir -Filter 'rodu.exe.old-*' |
      Where-Object { $_.Name -cmatch '^rodu\.exe\.old-[0-9a-f]{32}$' } |
      Remove-Item -Force -ErrorAction SilentlyContinue
    $old = $null
    if (Test-Path -LiteralPath $exe) {
      $old = "$exe.old-" + [Guid]::NewGuid().ToString('N')
      try {
        Move-Item -LiteralPath $exe -Destination $old
      } catch {
        throw "rodu.exe is in use and could not be replaced. Close rodu (rodu web, or the agent running rodu mcp) and run the installer again."
      }
    }
    try {
      Copy-Item -Force (Join-Path $Tmp 'out\rodu.exe') $exe
    } catch {
      # Put the working binary back rather than leave the user without rodu.
      if ($old) { Move-Item -Force -LiteralPath $old -Destination $exe -ErrorAction SilentlyContinue }
      throw
    }
    if ($old) { Remove-Item -Force -LiteralPath $old -ErrorAction SilentlyContinue }
    # The licences of Rodu and of the third-party code built into it travel with the binary,
    # in a folder of their own: the install folder may be shared and hold the user's own LICENSE.
    $notices = Join-Path $InstallDir 'rodu-notices'
    New-Item -ItemType Directory -Force -Path $notices | Out-Null
    foreach ($n in 'LICENSE', 'NOTICE', 'THIRD-PARTY-NOTICES.txt') {
      Copy-Item -Force (Join-Path $Tmp "out\$n") (Join-Path $notices $n)
    }
  } finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
  }

  # Add the folder to the user's PATH through the registry: the .NET Environment API would expand
  # entries like %USERPROFILE% and write the value back as a plain string.
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
  try {
    $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $parts = @($raw -split ';' | Where-Object { $_ })
    $known = $parts | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') }
    if ($known -notcontains $InstallDir.TrimEnd('\')) {
      $kind = if ($raw -and $key.GetValueKind('Path') -eq [Microsoft.Win32.RegistryValueKind]::String) { 'String' } else { 'ExpandString' }
      $key.SetValue('Path', (($parts + $InstallDir) -join ';'), $kind)
      # Tell running programs (Explorer, so terminals it starts) that the environment changed.
      try {
        if (-not ('RoduInstall.Env' -as [type])) {
          Add-Type -Namespace RoduInstall -Name Env -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint msg, System.UIntPtr wParam, string lParam, uint flags, uint timeout, out System.UIntPtr result);
'@
        }
        $result = [UIntPtr]::Zero
        # HWND_BROADCAST, WM_SETTINGCHANGE, "Environment", SMTO_ABORTIFHUNG, 5 s
        [void][RoduInstall.Env]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
      } catch {
        Write-Host 'Sign out and back in (or restart Explorer) if new terminals do not find rodu.'
      }
      Write-Host "Added $InstallDir to your PATH (new terminals pick it up)."
    }
  } finally {
    $key.Close()
  }
  if (($env:Path -split ';') -notcontains $InstallDir) { $env:Path = "$env:Path;$InstallDir" }

  $installed = & (Join-Path $InstallDir 'rodu.exe') --version
  Write-Host "Installed $installed to $InstallDir"
  Write-Host ''
  Write-Host 'Start: mkdir ~\rodu; cd ~\rodu; rodu init --name <you> --key <KEY>; rodu web'
}
