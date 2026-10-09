//! Virtual filesystem providers and shared file operation types for Keel.

use std::path::Path;

/// A provider-neutral path.
///
/// This is the minimal Task 2 interface needed by `keel-search`; Task 2 will
/// replace it with the complete path implementation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VPath {
    pub scheme: String,
    pub authority: String,
    pub path: String,
}

impl VPath {
    /// Creates a local path, storing separators in provider-neutral form.
    pub fn local(path: impl AsRef<Path>) -> Self {
        Self {
            scheme: "file".to_owned(),
            authority: String::new(),
            path: path.as_ref().to_string_lossy().replace('\\', "/"),
        }
    }

    /// Formats local paths with Windows separators and other paths as URIs.
    pub fn display(&self) -> String {
        if self.scheme == "file" && self.authority.is_empty() {
            self.path.replace('/', "\\")
        } else {
            format!("{}://{}{}", self.scheme, self.authority, self.path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VPath;

    #[test]
    fn local_path_has_the_task_two_display_form() {
        let path = VPath::local(r"D:\Work\x");

        assert_eq!(path.scheme, "file");
        assert_eq!(path.authority, "");
        assert_eq!(path.path, "D:/Work/x");
        assert_eq!(path.display(), r"D:\Work\x");
    }
}
