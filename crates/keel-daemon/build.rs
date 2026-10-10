fn main() {
    // With the `winfsp` mount backend, WinFsp's DLL is delay-loaded: keel-mount loads it
    // from the WinFsp install folder when the first mount is made, so the daemon still
    // starts where WinFsp is not installed.
    if std::env::var_os("CARGO_FEATURE_WINFSP").is_some()
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("x86") => "winfsp-x86.dll",
            Ok("aarch64") => "winfsp-a64.dll",
            _ => "winfsp-x64.dll",
        };
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        println!("cargo:rustc-link-lib=dylib=delayimp");
    }
}
