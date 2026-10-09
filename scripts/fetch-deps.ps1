$ErrorActionPreference = 'Stop'
$deps = Join-Path $PSScriptRoot '..\target\deps'; New-Item -ItemType Directory -Force $deps | Out-Null
$tmp = Join-Path $env:TEMP 'keel-deps'; New-Item -ItemType Directory -Force $tmp | Out-Null
# Everything SDK (voidtools, freeware)
Invoke-WebRequest 'https://www.voidtools.com/Everything-SDK.zip' -OutFile "$tmp\sdk.zip"
Expand-Archive "$tmp\sdk.zip" "$tmp\sdk" -Force
Copy-Item "$tmp\sdk\dll\Everything64.dll" $deps -Force
$evLic = Join-Path $deps 'licenses\everything'; New-Item -ItemType Directory -Force $evLic | Out-Null
Get-ChildItem "$tmp\sdk" -Recurse -File | Where-Object { $_.Name -match '^(license|licence|copying|readme)' } | Copy-Item -Destination $evLic -Force
# Some SDK zips ship no license file; the MIT notice is in the header comment of Everything.h.
if (-not (Get-ChildItem $evLic -File)) {
    Get-ChildItem "$tmp\sdk" -Recurse -Filter 'Everything.h' | Select-Object -First 1 | Copy-Item -Destination $evLic -Force
}
# pdfium (bblanchon/pdfium-binaries, BSD) - pinned to the release pdfium-render 0.8.37 targets (pdfium_7543)
Invoke-WebRequest 'https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7543/pdfium-win-x64.tgz' -OutFile "$tmp\pdfium.tgz"
tar -xzf "$tmp\pdfium.tgz" -C "$tmp"
Copy-Item "$tmp\bin\pdfium.dll" $deps -Force
$pdfiumLicenses = Join-Path $deps 'licenses\pdfium'
New-Item -ItemType Directory -Force "$pdfiumLicenses\third-party" | Out-Null
Copy-Item "$tmp\LICENSE" "$pdfiumLicenses\LICENSE" -Force
Copy-Item "$tmp\licenses\*" "$pdfiumLicenses\third-party" -Recurse -Force
Write-Host "deps in $deps"
