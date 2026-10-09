#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VPath {
    pub scheme: String,
    pub authority: String,
    pub path: String,
}

impl VPath {
    pub fn local(p: impl AsRef<std::path::Path>) -> Self {
        let mut path = p.as_ref().to_string_lossy().replace('\\', "/");
        if let Some(unc) = path.strip_prefix("//?/UNC/") {
            path = format!("//{unc}");
        } else if let Some(plain) = path.strip_prefix("//?/") {
            path = plain.to_owned();
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
            return Ok(Self::local(if authority.is_empty() {
                path.to_owned()
            } else {
                format!("//{authority}/{path}")
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
            std::path::PathBuf::from(path.replace('/', "\\"))
        })
    }
    pub fn parent(&self) -> Option<VPath> {
        let path = self.path.trim_end_matches('/');
        if path.is_empty()
            || (self.scheme == "file"
                && (path.ends_with(':')
                    || (path.starts_with("//") && path[2..].split('/').count() <= 2)))
        {
            return None;
        }
        let (parent, _) = path.rsplit_once('/')?;
        let parent = if parent.is_empty() {
            "/".into()
        } else if parent.ends_with(':') {
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
        Self {
            path: format!(
                "{}/{}",
                self.path.trim_end_matches('/'),
                name.replace('\\', "/").trim_start_matches('/')
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

    #[test]
    fn local_display() {
        assert_eq!(VPath::local(r"D:\Work\x").display(), r"D:\Work\x");
    }
    #[test]
    fn parse_file_uri() {
        assert_eq!(
            VPath::parse("file:///D:/Work").unwrap().to_local_path(),
            Some(PathBuf::from(r"D:\Work"))
        );
    }
    #[test]
    fn drive_root_has_no_parent() {
        assert_eq!(VPath::local("D:\\").parent(), None);
    }
    #[test]
    fn join_preserves_spaces() {
        assert_eq!(
            VPath::local(r"D:\Work").join("a b").display(),
            r"D:\Work\a b"
        );
    }
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
    #[test]
    fn parent_and_remote_root() {
        assert_eq!(VPath::local(r"D:\Work").parent().unwrap().display(), "D:\\");
        assert!(VPath::parse("sftp://box/").unwrap().parent().is_none());
        assert!(VPath::parse("not a uri").is_err());
        assert!(VPath::parse("://host/path").is_err());
    }
}
