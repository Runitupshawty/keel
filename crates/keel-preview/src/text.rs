use crate::{hex, Preview, Request};
use std::io::Read;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "c", "h", "cpp", "hpp", "cs", "go", "java", "js", "jsx", "ts", "tsx", "py", "rb", "php",
    "swift", "kt", "kts", "sh", "ps1", "psm1", "bat", "cmd", "html", "htm", "css", "scss", "xml",
    "json", "toml", "yaml", "yml", "ini", "cfg", "log", "txt", "md",
];
const MAX_LINES: usize = 5000;
/// Read at most this much, so a huge one-line minified file is still bounded.
const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// syntect runs ~1.5 s/MiB (release); past this budget lines are shown uncoloured.
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;

pub(crate) fn accepts(ext: &str) -> bool {
    TEXT_EXTENSIONS.contains(&ext)
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    // two-face = syntect defaults plus bat's extras (TOML, TypeScript, CSV, ...). PowerShell is
    // in two-face only for onig, which would mean a C build; .ps1 stays plain under fancy-regex.
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn themes() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

pub(crate) fn render(req: &Request, ext: &str, size: u64) -> Preview {
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&req.bytes_path)
        .and_then(|file| file.take(MAX_TEXT_BYTES).read_to_end(&mut bytes));
    if let Err(error) = read {
        return Preview::Error(error.to_string());
    }
    let Some(source) = decode(&bytes) else {
        return hex::render(req, size);
    };
    if ext == "md" {
        return Preview::Markdown(source);
    }
    // syntect's newline grammars choke on a trailing '\r' (strings turn background-coloured).
    let source = if source.contains('\r') {
        source.replace("\r\n", "\n")
    } else {
        source
    };

    let syntaxes = syntaxes();
    let syntax = syntaxes
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let theme = &themes().themes["base16-ocean.dark"];
    let plain = theme
        .settings
        .foreground
        .unwrap_or(syntect::highlighting::Color::WHITE);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut budget = MAX_HIGHLIGHT_BYTES;
    let mut lines = Vec::new();
    let mut truncated = size > MAX_TEXT_BYTES;
    for (index, line) in LinesWithEndings::from(&source).enumerate() {
        if index == MAX_LINES {
            truncated = true;
            break;
        }
        if line.len() > budget {
            budget = 0;
            lines.push(vec![(
                [plain.r, plain.g, plain.b, plain.a],
                line.to_owned(),
            )]);
            continue;
        }
        budget -= line.len();
        let spans = match highlighter.highlight_line(line, syntaxes) {
            Ok(spans) => spans
                .into_iter()
                .map(|(style, text)| {
                    let c = style.foreground;
                    ([c.r, c.g, c.b, c.a], text.to_owned())
                })
                .collect(),
            Err(error) => return Preview::Error(error.to_string()),
        };
        lines.push(spans);
    }
    Preview::Text {
        lines,
        language: syntax.name.clone(),
        truncated,
    }
}

/// UTF-16 with BOM, UTF-8, or (mostly printable) legacy text read as Windows-1252 (the
/// Western ANSI codepage; its printable range covers Latin-1).
/// `None` means binary: show hex instead.
fn decode(bytes: &[u8]) -> Option<String> {
    if let Some(units) = utf16_units(bytes) {
        return Some(
            char::decode_utf16(units)
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
        );
    }
    let sniff = &bytes[..bytes.len().min(8192)];
    if sniff.contains(&0) {
        return None;
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_owned()),
        // The byte cap cut a multi-byte character: keep the valid prefix.
        Err(error) if error.error_len().is_none() => {
            Some(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned())
        }
        Err(_) => {
            let printable = bytes
                .iter()
                .filter(|&&b| b >= 0x20 && b != 0x7f || matches!(b, b'\t' | b'\n' | b'\r' | 0x0c))
                .count();
            (printable * 10 >= bytes.len() * 9).then(|| {
                encoding_rs::WINDOWS_1252
                    .decode_without_bom_handling(bytes)
                    .0
                    .into_owned()
            })
        }
    }
}

fn utf16_units(bytes: &[u8]) -> Option<Vec<u16>> {
    let (body, little_endian) = match bytes {
        [0xFF, 0xFE, rest @ ..] => (rest, true),
        [0xFE, 0xFF, rest @ ..] => (rest, false),
        _ => return None,
    };
    let (pairs, _) = body.as_chunks::<2>();
    Some(
        pairs
            .iter()
            .map(|&pair| match little_endian {
                true => u16::from_le_bytes(pair),
                false => u16::from_be_bytes(pair),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn windows_1252_text_decodes() {
        // "Café — 5€" saved by Notepad in the ANSI codepage.
        let bytes = b"Caf\xe9 \x97 5\x80\r\n";
        assert_eq!(decode(bytes).as_deref(), Some("Café — 5€\r\n"));
        assert_eq!(
            decode("Café".as_bytes()).as_deref(),
            Some("Café"),
            "UTF-8 first"
        );
        assert_eq!(decode(b"\x00\x01binary"), None);
    }
}
