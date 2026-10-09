#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VPath {
    pub scheme: String,
    pub authority: String,
    pub path: String,
}

impl VPath {
    pub fn local(p: impl AsRef<std::path::Path>) -> Self {
        let mut path = p.as_ref().to_string_lossy().into_owned();
        // Only Windows uses '\' as a separator; on Unix it is a legal filename character.
        if cfg!(windows) {
            path = path.replace('\\', "/");
            if let Some(unc) = path.strip_prefix("//?/UNC/") {
                path = format!("//{unc}");
            } else if let Some(plain) = path.strip_prefix("//?/") {
                path = plain.to_owned();
            }
        }
        Self {
            scheme: "file".into(),
            authority: String::new(),
            path,
        }
    }
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| anyhow::anyhow!("invalid VPath: {s}"))?;
        anyhow::ensure!(
            !scheme.is_empty()
                && scheme
                    .bytes()
                    .enumerate()
                    .all(|(i, c)| c.is_ascii_alphabetic()
                        || (i > 0 && (c.is_ascii_digit() || b"+.-".contains(&c)))),
            "invalid VPath scheme: {s}"
        );
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if scheme.eq_ignore_ascii_case("file") {
            return Ok(Self::local(if !authority.is_empty() {
                format!("//{authority}/{path}")
            } else if cfg!(windows) {
                path.to_owned()
            } else {
                format!("/{path}")
            }));
        }
        Ok(Self {
            scheme: scheme.to_ascii_lowercase(),
            authority: authority.into(),
            path: format!("/{path}"),
        })
    }
    pub fn to_local_path(&self) -> Option<std::path::PathBuf> {
        (self.scheme == "file").then(|| {
            let path = if self.authority.is_empty() {
                self.path.clone()
            } else {
                format!("//{}/{}", self.authority, self.path.trim_start_matches('/'))
            };
            std::path::PathBuf::from(if cfg!(windows) {
                path.replace('/', "\\")
            } else {
                path
            })
        })
    }
    pub fn parent(&self) -> Option<VPath> {
        let path = self.path.trim_end_matches('/');
        if path.is_empty()
            || (self.scheme == "file"
                && cfg!(windows)
                && (path.ends_with(':')
                    || (path.starts_with("//") && path[2..].split('/').count() <= 2)))
        {
            return None;
        }
        let (parent, _) = path.rsplit_once('/')?;
        let parent = if parent.is_empty() {
            "/".into()
        } else if cfg!(windows) && parent.ends_with(':') {
            format!("{parent}/")
        } else {
            parent.into()
        };
        Some(Self {
            path: parent,
            ..self.clone()
        })
    }
    pub fn join(&self, name: &str) -> VPath {
        let name = if cfg!(windows) && self.scheme == "file" {
            name.replace('\\', "/")
        } else {
            name.to_owned()
        };
        Self {
            path: format!(
                "{}/{}",
                self.path.trim_end_matches('/'),
                name.trim_start_matches('/')
            ),
            ..self.clone()
        }
    }
    pub fn name(&self) -> &str {
        self.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
    }
    pub fn display(&self) -> String {
        match self.to_local_path() {
            Some(p) => p.to_string_lossy().into_owned(),
            None => format!("{}://{}{}", self.scheme, self.authority, self.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VPath;
    use std::path::PathBuf;

    #[cfg(windows)]
    #[test]
    fn local_display() {
        assert_eq!(VPath::local(r"D:\Work\x").display(), r"D:\Work\x");
    }
    #[cfg(windows)]
    #[test]
    fn parse_file_uri() {
        assert_eq!(
            VPath::parse("file:///D:/Work").unwrap().to_local_path(),
            Some(PathBuf::from(r"D:\Work"))
        );
    }
    #[cfg(windows)]
    #[test]
    fn drive_root_has_no_parent() {
        assert_eq!(VPath::local("D:\\").parent(), None);
    }
    #[cfg(windows)]
    #[test]
    fn join_preserves_spaces() {
        assert_eq!(
            VPath::local(r"D:\Work").join("a b").display(),
            r"D:\Work\a b"
        );
    }
    #[cfg(windows)]
    #[test]
    fn last_component_is_name() {
        assert_eq!(VPath::local(r"C:\Users\example").name(), "example");
    }
    #[test]
    fn remote_authority() {
        let p = VPath::parse("sftp://box/a/b").unwrap();
        assert_eq!(p.authority, "box");
        assert_eq!(p.name(), "b");
        assert_eq!(p.parent().unwrap().display(), "sftp://box/a");
        assert_eq!(p.to_local_path(), None);
    }
    #[cfg(windows)]
    #[test]
    fn unc_and_extended_paths() {
        for raw in [r"\\server\share\a", r"\\?\UNC\server\share\a"] {
            let p = VPath::local(raw);
            assert_eq!(p.display(), r"\\server\share\a");
            assert!(p.parent().unwrap().parent().is_none());
        }
        assert_eq!(VPath::local(r"\\?\D:\Work").display(), r"D:\Work");
        assert_eq!(
            VPath::parse("file://server/share/a").unwrap().display(),
            r"\\server\share\a"
        );
    }
    #[cfg(windows)]
    #[test]
    fn parent_of_drive_child_is_drive_root() {
        assert_eq!(VPath::local(r"D:\Work").parent().unwrap().display(), "D:\\");
    }
    #[test]
    fn parse_errors_and_remote_root() {
        assert!(VPath::parse("sftp://box/").unwrap().parent().is_none());
        assert!(VPath::parse("not a uri").is_err());
        assert!(VPath::parse("://host/path").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn unix_backslash_is_part_of_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = tmp.path().join("a\\b.txt");
        std::fs::write(&raw, b"x").unwrap();
        let p = VPath::local(&raw);
        assert_eq!(p.name(), "a\\b.txt");
        assert_eq!(p.display(), raw.to_string_lossy());
        assert_eq!(p.to_local_path(), Some(raw.clone()));
        let joined = VPath::local(tmp.path()).join("a\\b.txt");
        assert_eq!(joined, p);
        assert!(joined.to_local_path().unwrap().exists());
        assert_eq!(
            VPath::parse("file:///tmp/x").unwrap().to_local_path(),
            Some(PathBuf::from("/tmp/x"))
        );
        assert_eq!(VPath::local("/").parent(), None);
        assert_eq!(VPath::local("/a").parent().unwrap().display(), "/");
    }
}
