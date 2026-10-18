//! Interface languages: one TOML table of `key = "text"` per language in `crates/keel-app/i18n/`,
//! embedded in the binary and parsed on first use. `tr("settings.title")` is the text in
//! the current language, else in English, else the key itself. `{name}` style placeholders
//! are filled by the caller (`tr(..).replace("{name}", ..)`).
//!
//! The current language is per thread: the UI thread sets it from Settings → Language
//! every frame (`set`), so tests on their own threads never see each other's language.
//! Text made on worker threads (job messages, toasts from workers) stays English.
//!
//! The command palette's entries are the command table's (`keymap::commands`): English
//! there, other languages translate them by command id in their `[command]` table
//! (`command("view.refresh", "Refresh")`).
//!
//! Adding a language: copy `en.toml` to `<code>.toml`, translate every value (keep the
//! placeholders), add a `[command]` table with every palette command, and add the file to
//! `FILES`. `every_language_has_every_key` checks the rest.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;

/// (code, table). English first: the fallback.
const FILES: [(&str, &str); 2] = [
    ("en", include_str!("../i18n/en.toml")),
    ("es", include_str!("../i18n/es.toml")),
];

struct Lang {
    code: &'static str,
    strings: HashMap<String, String>,
}

fn langs() -> &'static [Lang] {
    static LANGS: OnceLock<Vec<Lang>> = OnceLock::new();
    LANGS.get_or_init(|| {
        (FILES.iter())
            .map(|(code, text)| {
                let mut strings = HashMap::new();
                match toml::from_str::<toml::Table>(text) {
                    Ok(table) => flatten("", table, &mut strings),
                    Err(e) => tracing::error!("i18n/{code}.toml: {e}"),
                }
                Lang { code, strings }
            })
            .collect()
    })
}

/// `[a] b = "x"` is the key `a.b`.
fn flatten(prefix: &str, table: toml::Table, out: &mut HashMap<String, String>) {
    for (k, v) in table {
        let key = format!("{prefix}{k}");
        match v {
            toml::Value::String(s) => {
                out.insert(key, s);
            }
            toml::Value::Table(t) => flatten(&format!("{key}."), t, out),
            other => tracing::error!("i18n: {key} is not text: {other}"),
        }
    }
}

thread_local! {
    static CURRENT: Cell<usize> = const { Cell::new(0) };
}

/// Makes `code` this thread's language (an unknown code: English).
pub fn set(code: &str) {
    let i = langs().iter().position(|l| l.code == code).unwrap_or(0);
    CURRENT.with(|c| c.set(i));
}

/// (code, the language's own name) for Settings → Language.
pub fn languages() -> impl Iterator<Item = (&'static str, &'static str)> {
    (langs().iter()).map(|l| {
        (
            l.code,
            l.strings
                .get("language.name")
                .map_or(l.code, String::as_str),
        )
    })
}

/// The text for `key` in the current language, else English, else `key`.
pub fn tr(key: &str) -> &str {
    let langs = langs();
    let lang = &langs[CURRENT.with(Cell::get)];
    (lang.strings.get(key))
        .or_else(|| langs[0].strings.get(key))
        .map_or(key, String::as_str)
}

/// The palette name of command `id` in the current language, else `label` (its English
/// name in `keymap::commands`).
pub fn command<'a>(id: &str, label: &'a str) -> &'a str {
    let langs = langs();
    (langs[CURRENT.with(Cell::get)]
        .strings
        .get(&format!("command.{id}")))
    .map_or(label, String::as_str)
}

/// "1 item" / "5 items".
pub fn items(n: usize) -> String {
    match n {
        1 => tr("status.item_one").to_owned(),
        n => tr("status.items").replace("{n}", &n.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(text: &str) -> BTreeSet<&str> {
        let re = regex::Regex::new(r"\{[a-z_]+\}").unwrap();
        re.find_iter(text).map(|m| m.as_str()).collect()
    }

    /// Every file parses, has the same keys as English, no empty text, and the same
    /// placeholders as the English text; its `[command]` table names every palette
    /// command and nothing else.
    #[test]
    fn every_language_has_every_key() {
        for (code, text) in FILES {
            toml::from_str::<toml::Table>(text).unwrap_or_else(|e| panic!("{code}.toml: {e}"));
        }
        let en = &langs()[0].strings;
        for lang in &langs()[1..] {
            for (key, text) in en {
                let t = (lang.strings.get(key))
                    .unwrap_or_else(|| panic!("{}.toml lacks {key}", lang.code));
                assert!(!t.trim().is_empty(), "{}.toml: {key} is empty", lang.code);
                assert_eq!(
                    placeholders(t),
                    placeholders(text),
                    "{}.toml: {key} placeholders",
                    lang.code
                );
            }
            let commands = crate::keymap::commands();
            for key in lang.strings.keys() {
                if let Some(id) = key.strip_prefix("command.") {
                    assert!(
                        commands.iter().any(|c| c.id == id)
                            || crate::keymap::PLATFORM_ONLY_COMMANDS.contains(&id),
                        "{}.toml: {key} is not a command",
                        lang.code
                    );
                    continue;
                }
                assert!(
                    en.contains_key(key),
                    "{}.toml: {key} is not in en.toml",
                    lang.code
                );
            }
            let missing: Vec<_> = (commands.iter())
                .filter(|c| c.palette != crate::keymap::Pal::None)
                .filter(|c| !lang.strings.contains_key(&format!("command.{}", c.id)))
                .map(|c| format!("\"{}\" = \"{}\"", c.id, c.label))
                .collect();
            assert!(
                missing.is_empty(),
                "{}.toml lacks in [command]:
{}",
                lang.code,
                missing.join(
                    "
"
                )
            );
        }
    }

    /// Every key in the files that translate (a `"table.key"` literal whose table is one
    /// of en.toml's) is in en.toml: a typo would show the key.
    #[test]
    fn every_key_used_exists() {
        let en = &langs()[0].strings;
        let tables: BTreeSet<&str> = en.keys().filter_map(|k| k.split('.').next()).collect();
        let re = regex::Regex::new(r#""([a-z_]+)\.([a-z_.]+)""#).unwrap();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut used = 0;
        for f in std::fs::read_dir(src).unwrap() {
            let path = f.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            if !text.contains("i18n::") {
                continue;
            }
            for c in re.captures_iter(&text) {
                let key = c[0].trim_matches('"');
                if tables.contains(&c[1]) && &c[1] != "language" {
                    used += 1;
                    assert!(
                        langs()[0].strings.contains_key(key),
                        "{}: {key} is not in en.toml",
                        path.display()
                    );
                }
            }
        }
        assert!(used > 100, "{used} keys used");
    }

    #[test]
    fn spanish_then_fallbacks() {
        set("es");
        assert_eq!(tr("settings.title"), "Ajustes");
        assert_eq!(items(1), "1 elemento");
        assert_eq!(items(3), "3 elementos");
        assert_eq!(tr("no.such.key"), "no.such.key");
        assert_eq!(command("view.refresh", "Refresh"), "Actualizar");
        assert_eq!(command("no.such.command", "Fallback"), "Fallback");
        set("xx");
        assert_eq!(tr("settings.title"), "Settings");
        assert_eq!(items(2), "2 items");
        let names: Vec<_> = languages().collect();
        assert_eq!(names, [("en", "English"), ("es", "Español")]);
        set("en");
    }
}
