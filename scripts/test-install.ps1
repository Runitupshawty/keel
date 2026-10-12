# Self-test of install.ps1's checks (no download, nothing installed; temp files only):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-install.ps1
# Runs in CI on Windows with pwsh; Windows PowerShell 5.1 works too.
$ErrorActionPreference = 'Stop'
$env:KEEL_INSTALL_FUNCTIONS_ONLY = '1'
try { . (Join-Path $PSScriptRoot 'install.ps1') } finally { Remove-Item Env:KEEL_INSTALL_FUNCTIONS_ONLY }

$script:failed = 0
function Check([string]$What, [scriptblock]$Test) {
  try { & $Test; Write-Host "ok   $What" } catch { $script:failed++; Write-Host "FAIL ${What}: $_" }
}
function Throws([scriptblock]$Block, [string]$Like) {
  try { & $Block } catch {
    if ($_.Exception.Message -like $Like) { return }
    throw "threw '$($_.Exception.Message)', expected '$Like'"
  }
  throw "did not throw (expected '$Like')"
}

$dir = Join-Path ([IO.Path]::GetTempPath()) ('keel-install-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $dir | Out-Null
try {
  $zip = Join-Path $dir 'keel-v1.0.0-win64.zip'
  [IO.File]::WriteAllText($zip, 'hello')
  $hash = '2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824' # SHA-256 of "hello"
  $sums = Join-Path $dir 'SHA256SUMS'
  Check 'checksum: the listed hash' {
    Set-Content $sums @("$('f' * 64)  keel-v1.0.0-macos-x64.tar.gz", "$hash  keel-v1.0.0-win64.zip")
    Assert-Checksum $zip $sums
  }
  Check 'checksum: binary-mode, upper-case and path entries' {
    Set-Content $sums "$($hash.ToUpper()) *dist/keel-v1.0.0-win64.zip"
    Assert-Checksum $zip $sums
  }
  Check 'checksum: a mismatch is refused' {
    Set-Content $sums "$('0' * 64)  keel-v1.0.0-win64.zip"
    Throws { Assert-Checksum $zip $sums } 'Checksum mismatch*'
  }
  Check 'checksum: no entry is refused' {
    Set-Content $sums "$hash  keel-v1.0.0-win64.zip.old"
    Throws { Assert-Checksum $zip $sums } '*no entry*'
  }

  $unsigned = Join-Path $dir 'unsigned.ps1'
  Set-Content $unsigned 'Write-Output 1'
  # Signed by Microsoft (catalog-signed, which Get-AuthenticodeSignature accepts too).
  $ms = Join-Path $env:windir 'System32\cmd.exe'
  $msCert = (Get-AuthenticodeSignature $ms).SignerCertificate
  Check 'signature: an unsigned build is reported as unsigned' {
    if (Test-KeelSignature @($unsigned, $unsigned) '') { throw 'reported as signed' }
  }
  Check 'signature: valid, no signer pinned' {
    if (-not (Test-KeelSignature @($ms) '')) { throw 'reported as unsigned' }
  }
  Check 'signature: pinned by thumbprint' { if (-not (Test-KeelSignature @($ms) $msCert.Thumbprint)) { throw 'not accepted' } }
  Check 'signature: pinned by subject' { if (-not (Test-KeelSignature @($ms) $msCert.Subject)) { throw 'not accepted' } }
  Check 'signature: another signer is refused' {
    Throws { Test-KeelSignature @($ms) ('AB' * 20) } '*not by Keel*'
  }
  Check 'signature: a partly signed build is refused' {
    Throws { Test-KeelSignature @($ms, $unsigned) '' } 'Only some*'
  }
  # A tampered copy of a file with an embedded signature (most drivers have one).
  $embedded = Get-ChildItem (Join-Path $env:windir 'System32\drivers\*.sys') |
    Where-Object { $_.Length -gt 0x2000 -and (Get-AuthenticodeSignature $_.FullName).SignatureType -eq 'Authenticode' } |
    Select-Object -First 1
  if ($embedded) {
    Check 'signature: a tampered file is refused' {
      $bytes = [IO.File]::ReadAllBytes($embedded.FullName)
      $bytes[0x1000] = $bytes[0x1000] -bxor 0xff
      $copy = Join-Path $dir 'tampered.sys'
      [IO.File]::WriteAllBytes($copy, $bytes)
      Throws { Test-KeelSignature @($copy) '' } '*does not accept*'
    }
  } else { Write-Host 'skip signature: a tampered file is refused (no embedded-signed driver found)' }

  # The installer kept next to the app (the update banner runs it).
  $unpacked = Join-Path $dir 'unpacked'; $app = Join-Path $dir 'app'
  New-Item -ItemType Directory $unpacked, $app | Out-Null
  $self = Join-Path $app 'install.ps1'
  $running = Join-Path $dir 'running.ps1'; Set-Content $running 'running'
  Check 'installer: an older release without one keeps the running script' {
    if ((Save-Installer $unpacked $self $running) -ne 'running') { throw 'not the running script' }
    if ((Get-Content $self) -ne 'running') { throw "kept '$(Get-Content $self)'" }
  }
  Check 'installer: the running copy next to the app stays' {
    if ((Save-Installer $unpacked $self $self) -ne 'running') { throw 'not the running script' }
    if ((Get-Content $self) -ne 'running') { throw "kept '$(Get-Content $self)'" }
  }
  Check "installer: the release's own copy wins over the running script" {
    Set-Content (Join-Path $unpacked 'install.ps1') 'release'
    if ((Save-Installer $unpacked $self $self) -ne 'release') { throw "not the release's" }
    if ((Get-Content $self) -ne 'release') { throw "kept '$(Get-Content $self)'" }
  }
  Check 'update: only programs inside the install folder count' {
    if (-not (Test-Inside 'C:\Apps\Keel\keel.exe' 'C:\Apps\Keel')) { throw 'inside' }
    if (-not (Test-Inside 'C:\Apps\Keel\keel.exe' 'C:\Apps\Keel\')) { throw 'trailing backslash' }
    if (Test-Inside 'C:\Apps\Keel2\keel-daemon.exe' 'C:\Apps\Keel') { throw 'a sibling folder matched' }
    if (Test-Inside '' 'C:\Apps\Keel') { throw 'no path matched' }
  }
  Check "update: a daemon's arguments are read from its command line" {
    $cases = @{
      '"C:\Apps\Keel\keel-daemon.exe" --profile=work --web 0.0.0.0:7421' = '--profile=work --web 0.0.0.0:7421'
      'C:\Apps\Keel\keel-daemon.exe --profile=work' = '--profile=work'
      '"C:\Program Files\Keel\keel-daemon.exe"' = ''
      'C:\Apps\Keel\keel-daemon.exe' = ''
    }
    foreach ($c in $cases.Keys) {
      $got = Get-CommandArgs $c
      if ($got -ne $cases[$c]) { throw "'$c' gave '$got'" }
    }
  }
  # A stand-in daemon (a .cmd) that writes the arguments it got.
  $log = Join-Path $dir 'started.txt'
  $fake = Join-Path $dir 'keel-daemon.cmd'
  Set-Content $fake "@echo [%*]>> `"$log`""
  function Wait-Log([int]$Lines) {
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (@(Get-Content $log -ErrorAction SilentlyContinue).Count -lt $Lines -and [DateTime]::UtcNow -lt $deadline) {
      Start-Sleep -Milliseconds 100
    }
    @(Get-Content $log -ErrorAction SilentlyContinue)
  }
  Check 'update: a daemon with arguments starts again with them, one without with none' {
    Remove-Item $log -ErrorAction SilentlyContinue
    # The stand-in ends at once, so the installer says it exited: that names it too.
    $said = Restart-KeelDaemons $fake @('--profile=work --web 127.0.0.1:7421', '') *>&1 | Out-String
    $got = Wait-Log 2 | Sort-Object
    if (($got -join '|') -ne '[]|[--profile=work --web 127.0.0.1:7421]') { throw "started '$($got -join '|')'" }
    if ($said -notlike '*keel-daemon --profile=work --web 127.0.0.1:7421*') { throw "said '$said'" }
  }
  Check 'update: no daemon ran, none starts' {
    Remove-Item $log -ErrorAction SilentlyContinue
    $said = Restart-KeelDaemons $fake @() *>&1 | Out-String
    Start-Sleep -Milliseconds 500
    if (Test-Path $log) { throw "started '$(Get-Content $log)'" }
    if ($said.Trim()) { throw "said '$said'" }
  }
  Check "update: the install's running daemon is found with its arguments and restarted with them" {
    # A stand-in keel-daemon.exe (ping, kept busy) in the install.
    $inst = Join-Path $dir 'daemon-inst'
    New-Item -ItemType Directory $inst | Out-Null
    $exe = Join-Path $inst 'keel-daemon.exe'
    Copy-Item (Join-Path $env:SystemRoot 'System32\PING.EXE') $exe
    $first = Start-Process $exe '-n 120 127.0.0.1' -PassThru -WindowStyle Hidden
    $saved = $script:InstallDir
    try {
      $script:InstallDir = Split-Path (Get-Process -Id $first.Id).Path
      $daemons = Get-KeelDaemonArgs
      if (($daemons -join '|') -ne '-n 120 127.0.0.1') { throw "read '$($daemons -join '|')'" }
      Stop-Keel
      if (-not $first.HasExited) { throw 'still runs' }
      $null = Restart-KeelDaemons $exe $daemons *>&1
      $again = Get-KeelDaemonArgs
      if (($again -join '|') -ne '-n 120 127.0.0.1') { throw "restarted with '$($again -join '|')'" }
    } finally {
      $script:InstallDir = $saved
      Get-Process keel-daemon -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$inst*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue
      $first | Stop-Process -Force -ErrorAction SilentlyContinue
    }
  }
  Check 'update: Stop-Keel stops the programs of the install, and only those' {
    # Two stand-ins named keel.exe (ping, kept busy): one in the install, one elsewhere.
    $ping = Join-Path $env:SystemRoot 'System32\PING.EXE'
    $run = foreach ($name in 'inst', 'other') {
      $d = Join-Path $dir $name
      New-Item -ItemType Directory $d | Out-Null
      Copy-Item $ping (Join-Path $d 'keel.exe')
      Start-Process (Join-Path $d 'keel.exe') '-n 120 127.0.0.1' -PassThru -WindowStyle Hidden
    }
    $saved = $script:InstallDir
    try {
      # The folder as Windows reports the running program (a temp path may be 8.3).
      $script:InstallDir = Split-Path (Get-Process -Id $run[0].Id).Path
      Stop-Keel
      if (-not $run[0].HasExited) { throw "the install's keel still runs" }
      if ($run[1].HasExited) { throw 'a keel outside the install was stopped' }
    } finally {
      $script:InstallDir = $saved
      $run | Stop-Process -Force -ErrorAction SilentlyContinue
      $run | ForEach-Object { try { [void]$_.WaitForExit(5000) } catch {} }
    }
  }
} finally {
  Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
}
if ($script:failed) { Write-Host "$script:failed check(s) failed"; exit 1 }
Write-Host 'install.ps1 checks: all passed'
