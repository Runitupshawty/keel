use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VPath {
    pub scheme: String,
    pub authority: String,
    pub path: String,
}

impl VPath {
    pub fn local(p: impl AsRef<Path>) -> Self {
        Self {
            scheme: "file".into(),
            authority: String::new(),
            path: p.as_ref().to_string_lossy().replace('\\', "/"),
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| anyhow::anyhow!("invalid virtual path"))?;
        let (authority, path) = if scheme == "file" {
            ("", rest.trim_start_matches('/'))
        } else {
            rest.split_once('/').unwrap_or((rest, ""))
        };
        Ok(Self {
            scheme: scheme.to_owned(),
            authority: authority.to_owned(),
            path: path.replace('\\', "/"),
        })
    }

    pub fn to_local_path(&self) -> Option<PathBuf> {
        (self.scheme == "file").then(|| PathBuf::from(self.path.replace('/', "\\")))
    }

    pub fn parent(&self) -> Option<Self> {
        let trimmed = self.path.trim_end_matches('/');
        let split = trimmed.rfind('/')?;
        if split == 2 && trimmed.as_bytes().get(1) == Some(&b':') {
            return None;
        }
        Some(Self {
            scheme: self.scheme.clone(),
            authority: self.authority.clone(),
            path: trimmed[..split].to_owned(),
        })
    }

    pub fn join(&self, name: &str) -> Self {
        let mut path = self.path.trim_end_matches('/').to_owned();
        path.push('/');
        path.push_str(name);
        Self {
            scheme: self.scheme.clone(),
            authority: self.authority.clone(),
            path,
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
        if self.scheme == "file" {
            self.path.replace('/', "\\")
        } else {
            format!("{}://{}/{}", self.scheme, self.authority, self.path)
        }
    }
}
