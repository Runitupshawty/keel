//! The disk usage report (Phase 17 Task 86): one self-contained HTML file (inline CSS and
//! an inline SVG bar chart, no scripts, no external assets) with the totals, the top
//! folders, the file types and the largest files; and a `mailto:` link that opens a new
//! mail in the default mail client with a summary and where the report was saved.

use super::export::{self, Options, Row};
use super::{NodeId, NodeKind, Tree, TOP};
use anyhow::Result;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

/// `s` escaped for HTML text and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Bytes in decimal units (`1.23 GB`), as Keel shows sizes.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 999.995 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    format!("{v:.2} {}", UNITS[unit])
}

/// The roots' biggest folders (their direct subfolders), at most `options.top`.
pub fn top_folders(tree: &Tree, options: Options) -> Vec<Row> {
    let mut ids: Vec<NodeId> = (tree.roots().iter())
        .flat_map(|r| tree.children(r.node))
        .filter(|&id| tree.node(id).kind == NodeKind::Dir)
        .collect();
    export::rank(tree, &mut ids, options);
    ids.into_iter().map(|id| export::row(tree, id)).collect()
}

const STYLE: &str = "\
:root{color-scheme:light dark;--fg:#17212b;--muted:#5b6670;--line:#d5dbe1;--bar:#2f6fb5;--bg:#fff}\
@media (prefers-color-scheme:dark){:root{--fg:#e6eaee;--muted:#9aa5af;--line:#38424c;--bar:#6ea8e8;--bg:#15191d}}\
body{font:15px/1.45 system-ui,-apple-system,'Segoe UI',sans-serif;color:var(--fg);background:var(--bg);max-width:1100px;margin:0 auto;padding:24px 16px}\
h1{font-size:1.6em;margin:0 0 .2em}h2{font-size:1.2em;margin:1.8em 0 .5em}\
p.meta{color:var(--muted);margin:.2em 0}\
table{border-collapse:collapse;width:100%;margin:.4em 0}\
th,td{text-align:left;padding:.35em .6em;border-bottom:1px solid var(--line);vertical-align:top}\
td.n,th.n{text-align:right;white-space:nowrap;font-variant-numeric:tabular-nums}\
td.p{overflow-wrap:anywhere}\
svg{width:100%;height:auto;display:block;margin:.6em 0}\
svg rect{fill:var(--bar)}svg text{fill:var(--fg);font:13px system-ui,sans-serif}\
footer{color:var(--muted);margin-top:2em;font-size:.9em}";

/// Writes the report of `tree` to `w`.
pub fn write(tree: &Tree, options: Options, w: impl Write) -> Result<()> {
    let mut w = w;
    let mut s = String::new();
    let t = tree.node(TOP);
    let by = |r: &Row| export::metric(r.bytes, r.allocated, options.size_on_disk);
    s.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    s.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    s.push_str("<meta name=\"generator\" content=\"Keel\">");
    s.push_str("<title>Disk usage report</title><style>");
    s.push_str(STYLE);
    s.push_str("</style></head><body>\n<h1>Disk usage report</h1>\n");
    let scanned = (tree.info.started > 0)
        .then(|| export::time(tree.info.started.saturating_mul(1_000_000_000)))
        .flatten()
        .map(|t| format!("Scanned {t}, "))
        .unwrap_or_default();
    let _ = writeln!(
        s,
        "<p class=\"meta\">{scanned}{:.1} s. {} ({} on disk) in {} files and {} folders.</p>",
        tree.info.elapsed.as_secs_f64(),
        human(t.size),
        human(t.allocated),
        t.files,
        t.folders.saturating_sub(tree.roots().len() as u64),
    );
    let mut notes = Vec::new();
    if !tree.info.complete {
        notes.push("the scan was cancelled, not everything was scanned".to_owned());
    }
    if tree.info.unreadable > 0 {
        notes.push(format!(
            "{} folders could not be read",
            tree.info.unreadable
        ));
    }
    if tree.info.excluded > 0 {
        notes.push(format!("{} items were left out by filters", tree.info.excluded));
    }
    if !notes.is_empty() {
        let _ = writeln!(s, "<p class=\"meta\">Note: {}.</p>", escape(&notes.join("; ")));
    }
    let ranked = match options.size_on_disk {
        true => "size on disk",
        false => "size",
    };
    let _ = writeln!(s, "<p class=\"meta\">Lists are ranked by {ranked}.</p>");

    s.push_str("<h2>Scanned</h2>\n<table><thead><tr><th>Folder or drive</th>");
    s.push_str("<th class=\"n\">Size</th><th class=\"n\">On disk</th><th class=\"n\">Files</th>");
    s.push_str("<th class=\"n\">Free</th></tr></thead><tbody>\n");
    for r in tree.roots() {
        let n = tree.node(r.node);
        let free = r.space.map(|sp| human(sp.free)).unwrap_or_default();
        let _ = writeln!(
            s,
            "<tr><td class=\"p\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
             <td class=\"n\">{}</td><td class=\"n\">{free}</td></tr>",
            escape(&r.path.to_string_lossy()),
            human(n.size),
            human(n.allocated),
            n.files,
        );
    }
    s.push_str("</tbody></table>\n");

    let folders = top_folders(tree, options);
    s.push_str("<h2>Top folders</h2>\n");
    if folders.is_empty() {
        s.push_str("<p>No folders.</p>\n");
    } else {
        chart(&mut s, &folders, by);
        rows_table(&mut s, &folders, tree, options, "Folder");
    }

    s.push_str("<h2>File types</h2>\n");
    let types = export::file_types(tree, options.size_on_disk);
    if types.is_empty() {
        s.push_str("<p>No files.</p>\n");
    } else {
        s.push_str("<table><thead><tr><th>Extension</th><th class=\"n\">Files</th>");
        s.push_str("<th class=\"n\">Size</th><th class=\"n\">On disk</th>");
        s.push_str("<th class=\"n\">Share</th></tr></thead><tbody>\n");
        let shown = match options.top {
            0 => types.len(),
            n => n.min(types.len()),
        };
        for ty in &types[..shown] {
            let ext = match ty.extension.is_empty() {
                true => "(none)".to_owned(),
                false => format!(".{}", ty.extension),
            };
            let _ = writeln!(
                s,
                "<tr><td>{}</td><td class=\"n\">{}</td><td class=\"n\" title=\"{} bytes\">{}</td>\
                 <td class=\"n\">{}</td><td class=\"n\">{:.2} %</td></tr>",
                escape(&ext),
                ty.files,
                ty.bytes,
                human(ty.bytes),
                human(ty.allocated),
                ty.percent,
            );
        }
        if shown < types.len() {
            let rest = &types[shown..];
            let sum = |f: fn(&export::FileType) -> u64| rest.iter().map(f).fold(0, u64::saturating_add);
            let _ = writeln!(
                s,
                "<tr><td>{} more types</td><td class=\"n\">{}</td><td class=\"n\">{}</td>\
                 <td class=\"n\">{}</td><td class=\"n\">{:.2} %</td></tr>",
                rest.len(),
                sum(|t| t.files),
                human(sum(|t| t.bytes)),
                human(sum(|t| t.allocated)),
                rest.iter().map(|t| t.percent).sum::<f64>(),
            );
        }
        s.push_str("</tbody></table>\n");
    }

    s.push_str("<h2>Largest files</h2>\n");
    let files = export::largest(tree, options);
    if files.is_empty() {
        s.push_str("<p>No files.</p>\n");
    } else {
        rows_table(&mut s, &files, tree, options, "File");
    }
    s.push_str(
        "<footer>Made by Keel. Hard links count once unless the scan said otherwise. \
         This report lists file and folder names: check it before you share it.</footer>\n\
         </body></html>\n",
    );
    w.write_all(s.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// A table of items: path, size, on disk, files, share of everything scanned.
fn rows_table(s: &mut String, rows: &[Row], tree: &Tree, options: Options, what: &str) {
    let t = tree.node(TOP);
    let total = export::metric(t.size, t.allocated, options.size_on_disk);
    let _ = writeln!(
        s,
        "<table><thead><tr><th>{what}</th><th class=\"n\">Size</th><th class=\"n\">On disk</th>\
         <th class=\"n\">Files</th><th class=\"n\">Share</th></tr></thead><tbody>"
    );
    for r in rows {
        let share = match total {
            0 => 0.0,
            _ => export::metric(r.bytes, r.allocated, options.size_on_disk) as f64 / total as f64
                * 100.0,
        };
        let files = match r.kind {
            "file" => String::new(),
            _ => r.files.to_string(),
        };
        let _ = writeln!(
            s,
            "<tr><td class=\"p\">{}</td><td class=\"n\" title=\"{} bytes\">{}</td>\
             <td class=\"n\">{}</td><td class=\"n\">{files}</td><td class=\"n\">{share:.2} %</td></tr>",
            escape(r.path.as_deref().unwrap_or(&r.name)),
            r.bytes,
            human(r.bytes),
            human(r.allocated),
        );
    }
    s.push_str("</tbody></table>\n");
}

/// A horizontal bar per row, the longest for the biggest; names on the left.
fn chart(s: &mut String, rows: &[Row], by: impl Fn(&Row) -> u64) {
    const ROW: usize = 26;
    const LABEL: f64 = 300.0;
    const BAR: f64 = 560.0;
    let height = rows.len() * ROW + 4;
    let max = rows.iter().map(&by).max().unwrap_or(0).max(1);
    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1000 {height}\" role=\"img\" \
         aria-labelledby=\"chart-title\"><title id=\"chart-title\">Top folders by size \
         (the table below has the numbers)</title>"
    );
    for (i, r) in rows.iter().enumerate() {
        let y = i * ROW;
        let width = (by(r) as f64 / max as f64 * BAR).max(1.0);
        let mut name: String = r.name.chars().take(40).collect();
        if r.name.chars().count() > 40 {
            name.push('…');
        }
        let _ = write!(
            s,
            "<g><title>{}: {}</title><text x=\"{:.0}\" y=\"{}\" text-anchor=\"end\">{}</text>\
             <rect x=\"{:.0}\" y=\"{}\" width=\"{width:.1}\" height=\"18\" rx=\"2\"/>\
             <text x=\"{:.0}\" y=\"{}\">{}</text></g>",
            escape(r.path.as_deref().unwrap_or(&r.name)),
            human(by(r)),
            LABEL - 8.0,
            y + 16,
            escape(&name),
            LABEL,
            y + 2,
            LABEL + width + 6.0,
            y + 16,
            human(by(r)),
        );
    }
    s.push_str("</svg>\n");
}

/// Writes the report of `tree` to `path` (replaced in one step).
pub fn save(tree: &Tree, options: Options, path: &Path) -> Result<()> {
    export::atomic_write(path, |w| write(tree, options, w))
}

/// A `mailto:` link (RFC 6068) for a new mail with no recipient: the subject, a short
/// summary of the scan and where the report is (`report`, made absolute). Nothing is
/// attached or sent; the user attaches the report in their mail client.
pub fn mailto(tree: &Tree, report: &Path) -> Result<String> {
    let path = std::path::absolute(report)?;
    let t = tree.node(TOP);
    let roots: Vec<String> = (tree.roots().iter())
        .map(|r| r.path.display().to_string())
        .collect();
    let mut body = format!(
        "Keel disk usage report\r\n\r\nScanned: {}\r\nTotal: {} ({} on disk) in {} files.\r\n",
        roots.join(", "),
        human(t.size),
        human(t.allocated),
        t.files,
    );
    if !tree.info.complete {
        body.push_str("The scan was cancelled: not everything was scanned.\r\n");
    }
    if tree.info.unreadable > 0 {
        let _ = write!(body, "{} folders could not be read.\r\n", tree.info.unreadable);
    }
    let _ = write!(
        body,
        "\r\nThe full report is saved at:\r\n{}\r\n(attach it to this mail before sending)\r\n",
        path.display()
    );
    Ok(format!(
        "mailto:?subject={}&body={}",
        percent_encode("Disk usage report"),
        percent_encode(&body)
    ))
}

/// Every byte but RFC 3986's unreserved characters as `%XX` (UTF-8).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
