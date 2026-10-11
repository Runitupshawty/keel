$ErrorActionPreference = 'Stop'
$deps = Join-Path $PSScriptRoot '..\target\deps'; New-Item -ItemType Directory -Force $deps | Out-Null
$tmp = Join-Path $env:TEMP 'keel-deps'; New-Item -ItemType Directory -Force $tmp | Out-Null
# Each download is refused unless its SHA-256 is the one pinned here. The pdfium-binaries tag
# and its hash change together, deliberately, here and in fetch-deps.sh (GitHub shows each
# asset's digest; check it against a download). voidtools serves the Everything SDK at one
# unversioned address: when that zip changes the fetch fails until the new hash is checked
# and pinned.
function Get-Pinned([string]$Url, [string]$OutFile, [string]$Sha256) {
    Invoke-WebRequest $Url -OutFile $OutFile -UseBasicParsing
    $got = (Get-FileHash -LiteralPath $OutFile -Algorithm SHA256).Hash.ToLower()
    if ($got -ne $Sha256) {
        Remove-Item -LiteralPath $OutFile -Force
        throw "${Url}: SHA-256 $got, expected $Sha256; nothing installed"
    }
}
# Everything SDK (voidtools, freeware)
Get-Pinned 'https://www.voidtools.com/Everything-SDK.zip' "$tmp\sdk.zip" 'f5716d9513cce6b462b5170a0a2e7e081e191d9bcac6774f5decc061497df443'
Expand-Archive "$tmp\sdk.zip" "$tmp\sdk" -Force
Copy-Item "$tmp\sdk\dll\Everything64.dll" $deps -Force
$evLic = Join-Path $deps 'licenses\everything'; New-Item -ItemType Directory -Force $evLic | Out-Null
Get-ChildItem "$tmp\sdk" -Recurse -File | Where-Object { $_.Name -match '^(license|licence|copying|readme)' } | Copy-Item -Destination $evLic -Force
# Some SDK zips ship no license file; the MIT notice is in the header comment of Everything.h.
if (-not (Get-ChildItem $evLic -File)) {
    Get-ChildItem "$tmp\sdk" -Recurse -Filter 'Everything.h' | Select-Object -First 1 | Copy-Item -Destination $evLic -Force
}
# pdfium (bblanchon/pdfium-binaries, BSD) - pinned to the release pdfium-render 0.8.37 targets (pdfium_7543)
$pdfiumTag = 'chromium/7543'
Get-Pinned "https://github.com/bblanchon/pdfium-binaries/releases/download/$pdfiumTag/pdfium-win-x64.tgz" "$tmp\pdfium.tgz" '0b08b606792a6cc593426efdefc6622611bce446d9e0270743846956ea1554ca'
tar -xzf "$tmp\pdfium.tgz" -C "$tmp"
Copy-Item "$tmp\bin\pdfium.dll" $deps -Force
$pdfiumLicenses = Join-Path $deps 'licenses\pdfium'
New-Item -ItemType Directory -Force "$pdfiumLicenses\third-party" | Out-Null
Copy-Item "$tmp\LICENSE" "$pdfiumLicenses\LICENSE" -Force
Copy-Item "$tmp\licenses\*" "$pdfiumLicenses\third-party" -Recurse -Force
Write-Host "deps in $deps"
