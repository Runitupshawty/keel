//! Regenerate the checked-in SDK from Rust types and the operation registry.
fn main() -> std::io::Result<()> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/typescript/keel.ts");
    std::fs::write(path, keel_api::typescript::generate())
}
