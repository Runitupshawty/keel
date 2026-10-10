# Builds the web client (crates/keel-web) into crates/keel-web/dist; keel-daemon embeds that
# folder at build time, so build keel-daemon afterwards. Needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli of the wasm-bindgen version in Cargo.lock:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <cargo pkgid wasm-bindgen, after the @>
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$dist = Join-Path $root 'crates\keel-web\dist'
cargo build --release --locked -p keel-web --target wasm32-unknown-unknown --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force $dist | Out-Null
wasm-bindgen --target web --no-typescript --out-dir $dist (Join-Path $target 'wasm32-unknown-unknown\release\keel_web.wasm')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Copy-Item (Join-Path $root 'crates\keel-web\static\*') $dist
# The service worker's cache key: a new build replaces the old shell.
$ver = (Get-FileHash (Join-Path $dist 'keel_web_bg.wasm') -Algorithm SHA256).Hash.Substring(0, 16).ToLower()
$sw = Join-Path $dist 'sw.js'
[IO.File]::WriteAllText($sw, [IO.File]::ReadAllText($sw).Replace('__KEEL_BUILD__', $ver), (New-Object Text.UTF8Encoding $false))
Write-Output "web client in $dist; now build keel-daemon"
