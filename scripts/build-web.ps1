# Builds the web client (crates/keel-web) into crates/keel-web/dist; keel-daemon embeds that
# folder at build time, so build keel-daemon afterwards. Needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli of the wasm-bindgen version in Cargo.lock:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <cargo pkgid wasm-bindgen, after the @>
# -Features e2e builds the browser-test bundle (tests/web-e2e) with its `window.__keel` hook;
# the release bundle (no -Features) must not carry it.
param([string]$Features = '')
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$dist = Join-Path $root 'crates\keel-web\dist'
$extra = if ($Features) { @('--features', $Features) } else { @() }
cargo build --release --locked -p keel-web --target wasm32-unknown-unknown --manifest-path (Join-Path $root 'Cargo.toml') @extra
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force $dist | Out-Null
wasm-bindgen --target web --no-typescript --out-dir $dist (Join-Path $target 'wasm32-unknown-unknown\release\keel_web.wasm')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item (Join-Path $root 'crates\keel-web\static\*') $dist
# The service worker's cache key: a hash of every file of the build (a change to any shell
# file, not only the wasm, replaces the old cache).
$all = New-Object IO.MemoryStream
Get-ChildItem $dist -File | Sort-Object Name | ForEach-Object {
    $bytes = [IO.File]::ReadAllBytes($_.FullName)
    $all.Write($bytes, 0, $bytes.Length)
}
$all.Position = 0
$ver = (Get-FileHash -InputStream $all -Algorithm SHA256).Hash.Substring(0, 16).ToLower()
$sw = Join-Path $dist 'sw.js'
[IO.File]::WriteAllText($sw, [IO.File]::ReadAllText($sw).Replace('__KEEL_BUILD__', $ver), (New-Object Text.UTF8Encoding $false))
if (-not ([IO.File]::ReadAllText($sw).Contains("const VERSION = `"$ver`";"))) { throw "sw.js was not stamped" }
if ($Features -notmatch 'e2e') {
    foreach ($f in Get-ChildItem $dist -File) {
        if ([Text.Encoding]::GetEncoding(28591).GetString([IO.File]::ReadAllBytes($f.FullName)).Contains('__keel')) {
            throw "the release bundle carries the e2e hook ($($f.Name))"
        }
    }
}
Write-Output "web client in $dist; now build keel-daemon"
