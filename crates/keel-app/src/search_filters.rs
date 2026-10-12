//! Search filters (Task 76): Everything's built-in filters (Everything, Audio, Compressed,
//! Document, Executable, Folder, Picture, Video) and the profile's own (`[[filters]]`: a
//! name, search text and match toggles), picked in the combo at the right of the search
//! field. A filter narrows what the search finds without touching the text typed: with the
//! System backend a built-in filter is an extension list (sent to the backend when it
//! understands one, and always checked on the hits), with the Library backend it is the
//! matching Kind chip's term; a custom filter's search text is put in front of the query
//! and its match toggles are added to the tab's.

use keel_core::{KindFilter, MatchToggles};

/// The built-in filters, in the combo's order; the first is no filter.
pub const BUILTIN: [&str; 8] = [
    "Everything",
    "Audio",
    "Compressed",
    "Document",
    "Executable",
    "Folder",
    "Picture",
    "Video",
];

/// Programs and scripts that run when opened.
const EXECUTABLE: &[&str] = &[
    "exe", "msi", "bat", "cmd", "com", "ps1", "vbs", "scr", "cpl", "msc", "jar", "app", "sh",
    "run", "appimage", "deb", "rpm", "pkg", "dmg",
];

/// A profile's own filter (`[[filters]]` in config.toml).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct CustomFilter {
    pub name: String,
    /// Put in front of the search text (`ext:psd;ai dm:thisyear`).
    pub search: String,
    /// Turned on while the filter is picked.
    pub matching: MatchToggles,
}

/// Longest filter name and search text (characters).
const NAME_MAX: usize = 100;
const SEARCH_MAX: usize = 2048;

/// Why `f` cannot be saved among `others` (the filters it does not replace).
pub fn check(f: &CustomFilter, others: &[CustomFilter]) -> Result<(), String> {
    let name = f.name.trim();
    if name.is_empty() || name != f.name || name.chars().count() > NAME_MAX {
        return Err(format!(
            "The name must not be empty, start or end with spaces, or be longer than {NAME_MAX} characters"
        ));
    }
    if BUILTIN.iter().any(|b| b.eq_ignore_ascii_case(name))
        || others.iter().any(|o| o.name.eq_ignore_ascii_case(name))
    {
        return Err(format!("A filter named \"{name}\" exists already"));
    }
    if f.search.chars().count() > SEARCH_MAX || f.search.chars().any(char::is_control) {
        return Err(format!(
            "The search must be one line of at most {SEARCH_MAX} characters"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Builtin {
    Audio,
    Compressed,
    Document,
    Executable,
    Folder,
    Picture,
    Video,
}

impl Builtin {
    fn of(name: &str) -> Option<Builtin> {
        Some(match name {
            "Audio" => Builtin::Audio,
            "Compressed" => Builtin::Compressed,
            "Document" => Builtin::Document,
            "Executable" => Builtin::Executable,
            "Folder" => Builtin::Folder,
            "Picture" => Builtin::Picture,
            "Video" => Builtin::Video,
            _ => return None,
        })
    }

    /// The extensions a file must have (empty for Folder): the library's Kind lists, so
    /// both backends agree.
    fn exts(self) -> &'static [&'static str] {
        match self {
            Builtin::Audio => KindFilter::Audio.extensions(),
            Builtin::Compressed => KindFilter::Archive.extensions(),
            Builtin::Document => KindFilter::Document.extensions(),
            Builtin::Executable => EXECUTABLE,
            Builtin::Folder => &[],
            Builtin::Picture => KindFilter::Image.extensions(),
            Builtin::Video => KindFilter::Video.extensions(),
        }
    }

    /// The library query term (a Kind chip's, or an `ext:` list).
    fn library_term(self) -> String {
        match self {
            Builtin::Audio => "kind:audio".into(),
            Builtin::Compressed => "kind:archive".into(),
            Builtin::Document => "kind:document".into(),
            Builtin::Executable => format!("ext:{}", EXECUTABLE.join(";")),
            Builtin::Folder => "kind:folder".into(),
            Builtin::Picture => "kind:image".into(),
            Builtin::Video => "kind:video".into(),
        }
    }
}

/// Where the search runs, for how a filter is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The library index (`LibraryQuery` syntax).
    Library,
    /// Everything (voidtools): `ext:` and `folder:` work.
    Everything,
    /// The Keel index (NTFS or walk index): `regex:` terms and `folder:` work.
    KeelIndex,
    /// Spotlight, locate or a remote walk: the text goes as typed; hits are checked.
    Plain,
}

impl Target {
    /// The System backend's target by its searcher's name.
    pub fn system(searcher: &str) -> Target {
        match searcher {
            "Everything" => Target::Everything,
            "Keel index" => Target::KeelIndex,
            _ => Target::Plain,
        }
    }
}

/// A search with its filter applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// What the backend is asked.
    pub text: String,
    pub folders_only: bool,
    pub matching: MatchToggles,
    /// Hits are kept only when they are folders (`Some(empty)`) or files with one of these
    /// lowercase extensions; None keeps every hit.
    pub keep: Option<Vec<String>>,
}

impl Applied {
    /// Whether the hit `name` (a folder when `dir`) passes the filter.
    pub fn keeps(&self, name: &str, dir: bool) -> bool {
        match &self.keep {
            None => true,
            Some(exts) if exts.is_empty() => dir,
            Some(exts) => {
                !dir && name.rsplit_once('.').is_some_and(|(_, ext)| {
                    let ext = ext.to_ascii_lowercase();
                    exts.iter().any(|e| *e == ext)
                })
            }
        }
    }
}

/// `text` searched through `filter` (a built-in or custom filter's name; "" or unknown:
/// none) on `target`, with the tab's `matching` toggles.
pub fn apply(
    text: &str,
    filter: &str,
    custom: &[CustomFilter],
    target: Target,
    matching: MatchToggles,
) -> Applied {
    let text = text.trim();
    let join = |a: &str, b: &str| match (a.trim(), b.trim()) {
        ("", b) => b.to_owned(),
        (a, "") => a.to_owned(),
        (a, b) => format!("{a} {b}"),
    };
    if let Some(c) = custom.iter().find(|c| c.name == filter) {
        return Applied {
            text: join(&c.search, text),
            matching: matching.or(c.matching),
            ..Applied::default()
        };
    }
    let Some(b) = Builtin::of(filter) else {
        return Applied {
            text: text.to_owned(),
            matching,
            ..Applied::default()
        };
    };
    if target == Target::Library {
        return Applied {
            text: join(text, &b.library_term()),
            matching,
            ..Applied::default()
        };
    }
    let exts = b.exts();
    let folder = b == Builtin::Folder;
    // A regex query is one pattern on the Keel index: no term can go in front of it.
    let prefix = match target {
        Target::Everything if folder => "folder:".to_owned(),
        Target::Everything => format!("ext:{}", exts.join(";")),
        Target::KeelIndex if matching.regex => String::new(),
        Target::KeelIndex if folder => "folder:".to_owned(),
        Target::KeelIndex => format!("regex:\\.({})$", exts.join("|")),
        _ => String::new(),
    };
    Applied {
        text: join(&prefix, text),
        folders_only: folder,
        matching,
        keep: Some(exts.iter().map(|e| (*e).to_owned()).collect()),
    }
}

/// Every filter name for the combo: the built-ins, then the profile's.
pub fn names(custom: &[CustomFilter]) -> Vec<String> {
    (BUILTIN.iter().map(|b| (*b).to_owned()))
        .chain(custom.iter().map(|c| c.name.clone()))
        .collect()
}

/// The combo's text for a tab's filter ("" is Everything).
pub fn shown(filter: &str) -> &str {
    match filter {
        "" => BUILTIN[0],
        f => f,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> MatchToggles {
        MatchToggles::default()
    }

    #[test]
    fn builtins_become_extension_lists_or_kind_terms() {
        let a = apply("beatles", "Audio", &[], Target::Everything, none());
        assert!(a.text.starts_with("ext:mp3;flac;"), "{}", a.text);
        assert!(a.text.ends_with(" beatles"));
        assert!(a.keeps("Help.MP3", false));
        assert!(!a.keeps("help.txt", false));
        assert!(!a.keeps("music.mp3", true), "a folder is not audio");

        let k = apply("beatles", "Audio", &[], Target::KeelIndex, none());
        assert!(k.text.starts_with(r"regex:\.(mp3|flac|"), "{}", k.text);
        assert!(k.text.ends_with(")$ beatles"));
        // Regex mode: the pattern goes alone; the hits are still checked.
        let r = apply(
            "^bea",
            "Audio",
            &[],
            Target::KeelIndex,
            MatchToggles {
                regex: true,
                ..none()
            },
        );
        assert_eq!(r.text, "^bea");
        assert!(r.keep.is_some());

        let p = apply("beatles", "Audio", &[], Target::Plain, none());
        assert_eq!(p.text, "beatles");
        assert!(p.keeps("a.flac", false));

        let lib = apply("beatles", "Audio", &[], Target::Library, none());
        assert_eq!((lib.text.as_str(), lib.keep), ("beatles kind:audio", None));
        let exe = apply("", "Executable", &[], Target::Library, none());
        assert!(exe.text.starts_with("ext:exe;msi;"), "{}", exe.text);
        keel_core::LibraryQuery::parse(&exe.text, 0).unwrap();
        for b in &BUILTIN[1..] {
            let q = apply("x", b, &[], Target::Library, none()).text;
            keel_core::LibraryQuery::parse(&q, 0).unwrap();
        }

        let f = apply("src", "Folder", &[], Target::Everything, none());
        assert_eq!(f.text, "folder: src");
        assert!(f.folders_only && f.keeps("src", true) && !f.keeps("src.rs", false));

        // Everything (or an unknown name) changes nothing.
        let e = apply(" x ", "Everything", &[], Target::Everything, none());
        assert_eq!(
            e,
            Applied {
                text: "x".into(),
                ..Applied::default()
            }
        );
        assert_eq!(apply("x", "", &[], Target::Plain, none()).keep, None);
    }

    #[test]
    fn custom_filters_prefix_their_search_and_add_toggles() {
        let mine = CustomFilter {
            name: "Designs".into(),
            search: "ext:psd;ai".into(),
            matching: MatchToggles {
                case: true,
                ..none()
            },
        };
        let tab = MatchToggles {
            regex: true,
            ..none()
        };
        let a = apply("logo", "Designs", &[mine.clone()], Target::Everything, tab);
        assert_eq!(a.text, "ext:psd;ai logo");
        assert!(a.matching.case && a.matching.regex);
        assert_eq!(a.keep, None);
        assert_eq!(
            apply("", "Designs", &[mine.clone()], Target::Library, none()).text,
            "ext:psd;ai"
        );
        assert_eq!(names(&[mine.clone()]).last().unwrap(), "Designs");

        // The editor's checks.
        assert!(check(&mine, &[]).is_ok());
        assert!(check(&mine, &[mine.clone()]).is_err(), "duplicate name");
        for bad in [" Designs", "", "audio"] {
            let f = CustomFilter {
                name: bad.into(),
                ..mine.clone()
            };
            assert!(check(&f, &[]).is_err(), "{bad:?}");
        }
        let f = CustomFilter {
            search: "a\nb".into(),
            ..mine
        };
        assert!(check(&f, &[]).is_err());
    }
}
