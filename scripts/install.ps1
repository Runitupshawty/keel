<#
.SYNOPSIS
  Install, update or remove Keel for the current Windows user (no admin needed).
.DESCRIPTION
  Downloads the latest GitHub release win64 zip, verifies it against the release's SHA256SUMS,
  and installs it. One-liner:
    irm https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.ps1 | iex
  With options:
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.ps1))) -Desktop -AddToPath
.PARAMETER Desktop     Also create a Desktop shortcut.
.PARAMETER AddToPath   Add the install folder to your user PATH, so `keel` works in a terminal.
.PARAMETER Uninstall   Remove Keel: files, shortcuts, PATH entry and the Settings -> Apps entry.
.PARAMETER InstallDir  Default: %LOCALAPPDATA%\Programs\Keel
.PARAMETER NoRegister  Skip the Settings -> Apps (uninstall) registry entry.
.PARAMETER RegistryKeyName  Name of that entry under HKCU\...\Uninstall (default Keel). A non-default name
                   also gives the shortcuts that name, so a test install never touches a real one.
.PARAMETER SkipVerify  Install a release that has no SHA256SUMS (older than this installer). Not recommended.
#>
param(
  [switch]$Desktop,
  [switch]$AddToPath,
  [switch]$Uninstall,
  [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Keel'),
  [switch]$NoRegister,
  [string]$RegistryKeyName = 'Keel',
  [switch]$SkipVerify
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$Repo = 'Runitupshawty/keel'
$RawUrl = "https://raw.githubusercontent.com/$Repo/main/scripts/install.ps1"
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$RegPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$RegistryKeyName"
$lnkName = if ($RegistryKeyName -eq 'Keel') { 'Keel' } else { $RegistryKeyName }
$Lnk = Join-Path ([Environment]::GetFolderPath('Programs')) "$lnkName.lnk"
$DesktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) "$lnkName.lnk"

function Stop-Keel {
  Get-Process keel -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } |
    Stop-Process -Force
  Start-Sleep -Milliseconds 300
}

function Set-UserPath([string]$Dir, [bool]$Add) {
  $cur = [Environment]::GetEnvironmentVariable('Path', 'User')
  $all = @($cur -split ';')
  $keep = @($all | Where-Object { $_ -and ($_.TrimEnd('') -ine $Dir.TrimEnd('')) })
  $removed = ($all | Where-Object { $_ -and ($_.TrimEnd('') -ieq $Dir.TrimEnd('')) }).Count -gt 0
  if ($Add) { if ($removed) { return }; $new = (@($all | Where-Object { $_ }) + $Dir) -join ';' }
  elseif ($removed) { $new = $keep -join ';' }
  else { return }
  [Environment]::SetEnvironmentVariable('Path', $new, 'User')
}

function New-Shortcut([string]$Path, [string]$Target) {
  $s = (New-Object -ComObject WScript.Shell).CreateShortcut($Path)
  $s.TargetPath = $Target
  $s.WorkingDirectory = Split-Path $Target
  $s.IconLocation = "$Target,0"
  $s.Save()
}

if ($Uninstall) {
  Stop-Keel
  foreach ($f in $Lnk, $DesktopLnk) { Remove-Item $f -Force -ErrorAction SilentlyContinue }
  Set-UserPath $InstallDir $false
  Remove-Item $RegPath -Recurse -Force -ErrorAction SilentlyContinue
  # Everything but this script first (it may be the running one), then the folder itself.
  if (Test-Path $InstallDir) {
    Get-ChildItem $InstallDir -Force | Where-Object { $_.Name -ne 'install.ps1' } | Remove-Item -Recurse -Force
    Remove-Item $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
  }
  Write-Host "Keel removed from $InstallDir. Your settings and library index are kept."
  return
}

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'Keel for Windows is x64 only.' }
$rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'keel-install' }
$tag = $rel.tag_name
$zip = $rel.assets | Where-Object { $_.name -like '*-win64.zip' } | Select-Object -First 1
if (-not $zip) { throw "Release $tag has no win64 zip." }
$sumAsset = $rel.assets | Where-Object { $_.name -eq 'SHA256SUMS' } | Select-Object -First 1

$work = Join-Path ([IO.Path]::GetTempPath()) ('keel-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $work | Out-Null
try {
  Write-Host "Downloading Keel $tag ($($zip.name))..."
  $zipPath = Join-Path $work $zip.name
  Invoke-WebRequest $zip.browser_download_url -OutFile $zipPath -UseBasicParsing

  if ($sumAsset) {
    $sumsPath = Join-Path $work 'SHA256SUMS'
    Invoke-WebRequest $sumAsset.browser_download_url -OutFile $sumsPath -UseBasicParsing
    $pat = '^([0-9a-fA-F]{64})\s+\*?(\S*/)?' + [regex]::Escape($zip.name) + '$'
    $want = Get-Content $sumsPath | ForEach-Object { $_.Trim() } |
      Where-Object { $_ -match $pat } | ForEach-Object { $Matches[1].ToLower() } | Select-Object -First 1
    if (-not $want) { throw "SHA256SUMS has no entry for $($zip.name)." }
    $got = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
    if ($got -ne $want) { throw "Checksum mismatch for $($zip.name): expected $want, got $got. Nothing was installed." }
    Write-Host 'Checksum OK.'
  } elseif ($SkipVerify) {
    Write-Warning "Release $tag has no SHA256SUMS; installing unverified (-SkipVerify)."
  } else {
    throw "Release $tag has no SHA256SUMS, so the download cannot be verified. Install a newer release, or pass -SkipVerify."
  }

  Expand-Archive $zipPath -DestinationPath (Join-Path $work 'x')
  $src = Get-ChildItem (Join-Path $work 'x') -Directory | Select-Object -First 1
  if (-not $src -or -not (Test-Path (Join-Path $src.FullName 'keel.exe'))) { throw 'The archive does not contain keel.exe.' }

  Stop-Keel
  New-Item -ItemType Directory -Force $InstallDir | Out-Null
  Copy-Item (Join-Path $src.FullName '*') $InstallDir -Recurse -Force
  $exe = Join-Path $InstallDir 'keel.exe'

  # Keep a copy of this installer next to the app: the Settings -> Apps entry runs it with -Uninstall.
  # Run via `irm | iex` there is no file, so fetch it.
  $self = Join-Path $InstallDir 'install.ps1'
  if ($PSCommandPath -and (Test-Path $PSCommandPath)) {
    if ((Resolve-Path $PSCommandPath).Path -ne $self) { Copy-Item $PSCommandPath $self -Force }
  } else {
    Invoke-WebRequest $RawUrl -OutFile $self -UseBasicParsing
  }

  New-Shortcut $Lnk $exe
  if ($Desktop) { New-Shortcut $DesktopLnk $exe }
  if ($AddToPath) { Set-UserPath $InstallDir $true }

  if (-not $NoRegister) {
    $ver = $tag.TrimStart('v')
    $cl = Join-Path $InstallDir 'CHANGELOG.md'
    if (Test-Path $cl) {
      $m = Select-String -Path $cl -Pattern '^## \[(\d+\.\d+\.\d+[^\]]*)\]' | Select-Object -First 1
      if ($m) { $ver = $m.Matches[0].Groups[1].Value }
    }
    New-Item $RegPath -Force | Out-Null
    $props = @{
      DisplayName     = 'Keel'
      DisplayVersion  = $ver
      Publisher       = 'Keel'
      InstallLocation = $InstallDir
      DisplayIcon     = $exe
      UninstallString = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$self`" -Uninstall -InstallDir `"$InstallDir`" -RegistryKeyName $RegistryKeyName"
    }
    foreach ($k in $props.Keys) { Set-ItemProperty $RegPath -Name $k -Value $props[$k] }
    Set-ItemProperty $RegPath -Name NoModify -Value 1 -Type DWord
    Set-ItemProperty $RegPath -Name NoRepair -Value 1 -Type DWord
  }
  Write-Host "Keel $tag installed to $InstallDir. Find it in the Start menu."
  if (-not $AddToPath) { Write-Host 'Add -AddToPath to run keel from a terminal.' }
} finally {
  Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
