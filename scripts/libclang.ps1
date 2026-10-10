# libclang for bindgen, which the `winfsp` mount backend needs to build (winfsp-sys reads
# the WinFsp headers it ships). Installs LLVM with Chocolatey when it is missing (GitHub's
# Windows runners usually have it) and sets LIBCLANG_PATH for the next CI steps.
$bin = Join-Path $env:ProgramFiles "LLVM\bin"
if (-not (Test-Path (Join-Path $bin "libclang.dll"))) {
    choco install llvm -y --no-progress
}
if (-not (Test-Path (Join-Path $bin "libclang.dll"))) {
    throw "libclang.dll not found in $bin"
}
if ($env:GITHUB_ENV) {
    "LIBCLANG_PATH=$bin" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
} else {
    "Set LIBCLANG_PATH=$bin before cargo build --features winfsp"
}
