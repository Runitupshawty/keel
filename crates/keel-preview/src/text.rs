use crate::{hex, Preview, Request};
use std::io::Read;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "c", "h", "cpp", "hpp", "cs", "go", "java", "js", "jsx", "ts", "tsx", "py", "rb", "php",
    "swift", "kt", "kts", "sh", "ps1", "bat", "cmd", "html", "htm", "css", "scss", "xml", "json",
    "toml", "yaml", "yml", "ini", "cfg", "log", "txt", "md",
];

pub(crate) fn accepts(ext: &str) -> bool {
    TEXT_EXTENSIONS.contains(&ext)
}

pub(crate) fn render(req: &Request, size: u64) -> Preview {
    let mut file = match std::fs::File::open(&req.bytes_path) {
        Ok(file) => file,
        Err(error) => return Preview::Error(error.to_string()),
    };
    let mut sniff = [0_u8; 8192];
    let count = match file.read(&mut sniff) {
        Ok(count) => count,
        Err(error) => return Preview::Error(error.to_string()),
    };
    if sniff[..count].contains(&0) {
        return hex::render(req, size);
    }
    let source = match std::fs::read_to_string(&req.bytes_path) {
        Ok(source) => source,
        Err(_) => return hex::render(req, size),
    };
    if req.entry.ext.eq_ignore_ascii_case("md") {
        return Preview::Markdown(source);
    }

    let syntaxes = SyntaxSet::load_defaults_newlines();
    let themes = ThemeSet::load_defaults();
    let syntax = syntaxes
        .find_syntax_by_extension(&req.entry.ext)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &themes.themes["base16-ocean.dark"]);
    let mut lines = Vec::new();
    let mut truncated = false;
    for (index, line) in LinesWithEndings::from(&source).enumerate() {
        if index == 5000 {
            truncated = true;
            break;
        }
        let spans = match highlighter.highlight_line(line, &syntaxes) {
            Ok(spans) => spans
                .into_iter()
                .map(|(style, text)| {
                    (
                        [
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                            style.foreground.a,
                        ],
                        text.to_owned(),
                    )
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
