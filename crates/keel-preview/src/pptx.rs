use crate::office::{attr, entity, Package, MAX_BLOCKS, MAX_SLIDES, TRUNCATED};
use crate::{DocBlock, Preview, Request};
use quick_xml::events::Event;
use quick_xml::Reader;

pub(crate) fn accepts(ext: &str) -> bool {
    ext == "pptx"
}

pub(crate) fn render(req: &Request) -> Preview {
    match build(&req.bytes_path) {
        Ok(blocks) => Preview::Doc { blocks },
        Err(error) => Preview::Error(error),
    }
}

pub(crate) fn build(path: &std::path::Path) -> Result<Vec<DocBlock>, String> {
    let mut pkg = Package::open(path)?;
    // ponytail: slide file order, not the order in presentation.xml (they match unless
    // slides were reordered and the file never renumbered).
    let mut slides: Vec<(u32, String)> = pkg
        .names()
        .into_iter()
        .filter_map(|n| {
            let i = n
                .strip_prefix("ppt/slides/slide")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((i, n))
        })
        .collect();
    if slides.is_empty() {
        return Err("no slides found (not a PowerPoint file?)".into());
    }
    slides.sort();
    let mut blocks = Vec::new();
    for (n, (_, name)) in slides.iter().enumerate() {
        if n >= MAX_SLIDES || blocks.len() >= MAX_BLOCKS {
            blocks.push(DocBlock::Para(TRUNCATED.into()));
            break;
        }
        let xml = pkg.read(name)?.unwrap_or_default();
        blocks.push(DocBlock::Heading(2, format!("Slide {}", n + 1)));
        for (title, text) in slide_text(&xml, MAX_BLOCKS - blocks.len() + 1)
            .map_err(|e| format!("malformed XML in {name}: {e}"))?
        {
            if blocks.len() == MAX_BLOCKS {
                blocks.push(DocBlock::Para(TRUNCATED.into()));
                return Ok(blocks);
            }
            blocks.push(if title {
                DocBlock::Heading(3, text)
            } else {
                DocBlock::Para(text)
            });
        }
    }
    Ok(blocks)
}

/// (is_title, text) per paragraph, in shape order. Footer, date and slide-number
/// placeholders are left out.
fn slide_text(xml: &[u8], limit: usize) -> Result<Vec<(bool, String)>, quick_xml::Error> {
    let mut reader = Reader::from_reader(xml);
    let mut out = Vec::new();
    let (mut title, mut skip, mut in_t) = (false, false, false);
    let mut para: Option<String> = None;
    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => match e.local_name().as_ref() {
                "sp" => (title, skip) = (false, false),
                "ph" => placeholder(&e, &mut title, &mut skip),
                "p" => para = Some(String::new()),
                "t" => in_t = true,
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                "ph" => placeholder(&e, &mut title, &mut skip),
                "br" => para.iter_mut().for_each(|p| p.push(' ')),
                _ => {}
            },
            Event::Text(t) if in_t => {
                if let Some(p) = &mut para {
                    p.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) if in_t => {
                if let Some(p) = &mut para {
                    p.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) if in_t => {
                if let (Some(p), Some(s)) = (&mut para, entity(&r)) {
                    p.push_str(&s);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_t = false,
                "p" => {
                    if let Some(p) = para.take() {
                        let p = p.trim();
                        if !skip && !p.is_empty() {
                            out.push((title, p.to_owned()));
                            if out.len() == limit {
                                return Ok(out);
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(out)
}

fn placeholder(e: &quick_xml::events::BytesStart, title: &mut bool, skip: &mut bool) {
    match attr(e, "type").as_deref() {
        Some("title" | "ctrTitle") => *title = true,
        Some("dt" | "ftr" | "sldNum") => *skip = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn bughunt_one_slide_cannot_exceed_block_budget() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(file.reopen().unwrap());
        zip.start_file(
            "ppt/slides/slide1.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        let xml = format!(
            "<sld><sp>{}</sp></sld>",
            "<p><t>hello</t></p>".repeat(MAX_BLOCKS + 10)
        );
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap();
        let blocks = build(file.path()).unwrap();
        assert!(blocks.len() <= MAX_BLOCKS + 1, "{} blocks", blocks.len());
        assert!(matches!(blocks.last(), Some(DocBlock::Para(t)) if t == TRUNCATED));
    }
}
