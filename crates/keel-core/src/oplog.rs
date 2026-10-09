//! The operation log: one row per executed operation in `library.db`. Payload and result are
//! redacted before they are written: no secret, and no location outside the library's
//! sources (each such location is replaced, up to the next quote, bracket, `: ` or ` -> `).

use crate::library::Shared;
use anyhow::Result;
use regex::{Captures, Regex};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

pub(crate) const OUTSIDE: &str = "<redacted: location outside the library>";
const SECRET: &str = "<redacted>";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpLogEntry {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub payload: Value,
    pub result: String,
    /// None while running; whether the operation completed.
    pub ok: Option<bool>,
}

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)pass|secret|token|api.?key|access.?key|private.?key|auth|cookie|credential")
        .expect("valid regex")
});
static SECRET_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|pwd|passphrase|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|bearer)(\s*[:=]\s*|\s+)("[^"]*"|'[^']*'|\S+)"#,
    )
    .expect("valid regex")
});
/// Headers whose whole value is a credential.
static SECRET_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(authorization|proxy-authorization|cookie|set-cookie)(\s*[:=]\s*)[^\r\n]*")
        .expect("valid regex")
});
/// Drive paths, UNC paths, URLs/VPaths (any scheme) and absolute or home-relative unix paths.
static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"[A-Za-z]:[\\/]|\\\\[^\\\s]|[A-Za-z][A-Za-z0-9+.-]*://|(?:^|[\s'"(\[=,])~?/[^\s/]"#,
    )
    .expect("valid regex")
});
const MASK: char = '\u{1}';

/// Every spelling of each source root that may appear in a message, longest first.
pub(crate) fn roots(lib: &Shared) -> Vec<String> {
    let mut roots: Vec<String> = lib
        .sources
        .read()
        .iter()
        .flat_map(|s| {
            let display = s.def.root.display();
            let slashed = display.replace('\\', "/");
            [display, slashed]
        })
        .map(|r| r.trim_end_matches(['/', '\\']).to_owned())
        .filter(|r| !r.is_empty())
        .collect();
    roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
    roots.dedup();
    roots
}

/// Where a location span that starts at `from` ends: the next quote, bracket, `: ` or ` -> `,
/// else the end of the text (spaces belong to the path).
fn span_end(s: &str, from: usize) -> usize {
    let rest = &s[from..];
    ["\"", "'", "`", ")", "]", ": ", " -> ", "\n"]
        .iter()
        .filter_map(|t| rest.find(t))
        .min()
        .map_or(s.len(), |i| from + i)
}

/// `s` with secrets blanked and every location that is not inside one of `roots` replaced
/// by [`OUTSIDE`].
pub(crate) fn redact_text(s: &str, roots: &[String]) -> String {
    let s = s
        .replace(MASK, "")
        .replace(r"\\?\UNC\", r"\\")
        .replace(r"\\?\", "");
    let s = SECRET_LINE.replace_all(&s, |c: &Captures| format!("{}{}{SECRET}", &c[1], &c[2]));
    let s = SECRET_VALUE.replace_all(&s, |c: &Captures| format!("{}{}{SECRET}", &c[1], &c[2]));
    // Hide the roots (at a path boundary), then look for any location that is left.
    let mut masked = s.into_owned();
    for (i, root) in roots.iter().enumerate() {
        let pattern = format!(
            r#"{}{}([\\/'"`)\],;:]|$)"#,
            if cfg!(windows) { "(?i)" } else { "" },
            regex::escape(root)
        );
        let Ok(re) = Regex::new(&pattern) else {
            return OUTSIDE.into();
        };
        masked = re
            .replace_all(&masked, |c: &Captures| format!("{MASK}{i}{MASK}{}", &c[1]))
            .into_owned();
    }
    // Replace each outside location; `OUTSIDE` itself never matches.
    while let Some(m) = LOCATION.find(&masked) {
        // The unix-path pattern includes the character before the path.
        let start = m.start()
            + masked[m.start()..]
                .find(['~', '/', '\\'])
                .filter(|_| !masked[m.start()..].starts_with(|c: char| c.is_ascii_alphabetic()))
                .unwrap_or(0);
        let end = span_end(&masked, start + 1);
        masked.replace_range(start..end, OUTSIDE);
    }
    let mut out = masked;
    for (i, root) in roots.iter().enumerate() {
        out = out.replace(&format!("{MASK}{i}{MASK}"), root);
    }
    out
}

pub(crate) fn redact(v: &Value, roots: &[String]) -> Value {
    match v {
        Value::String(s) => Value::String(redact_text(s, roots)),
        Value::Array(items) => Value::Array(items.iter().map(|i| redact(i, roots)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if SECRET_KEY.is_match(k) {
                        Value::String(SECRET.into())
                    } else {
                        redact(v, roots)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Logs an operation (redacted); returns its row id.
pub(crate) fn record(lib: &Shared, kind: &str, payload: &Value, result: &str) -> Result<i64> {
    let roots = roots(lib);
    let conn = lib.db.get()?;
    conn.execute(
        "INSERT INTO op_log(ts, kind, payload, result) VALUES (?1, ?2, ?3, ?4)",
        params![
            crate::now(),
            kind,
            redact(payload, &roots).to_string(),
            redact_text(result, &roots)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn set_result(lib: &Shared, id: i64, result: &str, ok: bool) -> Result<()> {
    let result = redact_text(result, &roots(lib));
    lib.db.get()?.execute(
        "UPDATE op_log SET result = ?2, ts = ?3, ok = ?4 WHERE id = ?1",
        params![id, result, crate::now(), ok],
    )?;
    Ok(())
}

/// The newest `limit` entries, newest first.
pub(crate) fn entries(lib: &Shared, limit: usize) -> Result<Vec<OpLogEntry>> {
    let conn = lib.db.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, ts, kind, payload, result, ok FROM op_log ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(OpLogEntry {
            id: r.get(0)?,
            ts: r.get(1)?,
            kind: r.get(2)?,
            payload: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
            result: r.get(4)?,
            ok: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_keeps_library_paths_and_drops_everything_else() {
        let roots = vec![
            if cfg!(windows) { r"D:\Lib" } else { "/srv/lib" }.to_owned(),
            "sftp://box/home/me".to_owned(),
        ];
        let inside = if cfg!(windows) {
            r"D:\Lib\a b\c.txt"
        } else {
            "/srv/lib/a b/c.txt"
        };
        assert_eq!(redact_text(inside, &roots), inside);
        let msg = format!("copy {inside}: access denied");
        assert_eq!(redact_text(&msg, &roots), msg);
        assert_eq!(
            redact_text("sftp://box/home/me/x.txt", &roots),
            "sftp://box/home/me/x.txt"
        );
        for outside in [
            r"C:\Users\james\secret.txt",
            r"D:\Lib secret\x",
            r"D:\Library\x",
            r"\\server\share\x",
            "/etc/passwd",
            "sftp://box/home/meow",
            "sftp://bob:pw@box/home/me/x",
            "cloud://acct/x",
        ] {
            assert_eq!(redact_text(outside, &roots), OUTSIDE, "{outside}");
        }
        // Only the location goes; the rest of the message stays.
        assert_eq!(
            redact_text("copy ~/My notes.txt", &roots),
            format!("copy {OUTSIDE}")
        );
        assert_eq!(
            redact_text(&format!("{inside} -> C:/else where/c.txt"), &roots),
            format!("{inside} -> {OUTSIDE}")
        );
        assert_eq!(
            redact_text(r"open C:\Users\james\x y.txt: access denied", &roots),
            format!("open {OUTSIDE}: access denied")
        );
        assert_eq!(
            redact_text(
                r#"from "\\nas\share\a b" to (/srv/x) at sftp://h/p"#,
                &roots
            ),
            format!(r#"from "{OUTSIDE}" to ({OUTSIDE}) at {OUTSIDE}"#)
        );
        assert_eq!(
            redact_text("login failed: password=hunter2 token: abc123", &roots),
            "login failed: password=<redacted> token: <redacted>"
        );
        assert_eq!(
            redact_text("Authorization: Bearer xyz", &roots),
            "Authorization: <redacted>"
        );
        assert_eq!(
            redact_text("got bearer xyz", &roots),
            "got bearer <redacted>"
        );
    }

    #[test]
    fn json_keys_and_strings_are_redacted() {
        let roots = vec!["sftp://box/home/me".to_owned()];
        let v = redact(
            &json!({
                "src": ["sftp://box/home/me/a", "sftp://other/b"],
                "password": "hunter2",
                "api_key": {"nested": "x"},
                "n": 3,
            }),
            &roots,
        );
        assert_eq!(
            v,
            json!({
                "src": ["sftp://box/home/me/a", OUTSIDE],
                "password": SECRET,
                "api_key": SECRET,
                "n": 3,
            })
        );
    }
}
