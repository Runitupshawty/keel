//! Fuzz entry points (docs/fuzzing.md), shared by `fuzz/` and the in-tree runs.

/// A Web Share Target upload (`POST /share`): the first line is the `Content-Type` header,
/// the rest the `multipart/form-data` body, streamed into a fresh folder.
pub fn multipart(data: &[u8]) {
    let (head, body) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let boundary =
        crate::share::boundary(&String::from_utf8_lossy(head)).unwrap_or_else(|| "b".to_owned());
    let dir = tempfile::tempdir().unwrap();
    let _ = crate::share::parse(body, &boundary, dir.path());
}

#[cfg(test)]
mod tests {
    use keel_vfs::fuzz_harness::{run, Capped};

    #[global_allocator]
    static ALLOC: Capped = Capped;

    #[test]
    fn fuzz_multipart() {
        run("multipart", super::multipart);
    }
}
