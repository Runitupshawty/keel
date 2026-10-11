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
  Check "update: the default profile's daemon starts again" {
    $log = Join-Path $dir 'calls.txt'
    $fake = Join-Path $dir 'keel.cmd'
    Set-Content $fake "@echo %*>> `"$log`""
    $said = Restart-KeelDaemon $fake 6>&1 | Out-String
    if ((Get-Content $log) -ne 'daemon start') { throw "ran '$(Get-Content $log)'" }
    if ($said -notlike '*keel --profile <name> daemon start*') { throw "said '$said'" }
  }
} finally {
  Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
}
if ($script:failed) { Write-Host "$script:failed check(s) failed"; exit 1 }
Write-Host 'install.ps1 checks: all passed'
