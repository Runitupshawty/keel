//! Regenerate the checked-in Swift SDK from Rust types and the operation registry.
fn main() -> std::io::Result<()> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/swift/Keel.swift");
    std::fs::write(path, keel_api::swift::generate())
}
