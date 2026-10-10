use crate::office::{attr, entity, Package, MAX_BLOCKS, MAX_SLIDES, TRUNCATED};
use crate::{DocBlock, Preview, Request};
use quick_xml::events::Event;
use quick_xml::Reader;

pub(crate) fn accepts(ext: &str) -> bool {
    matches!(ext, "odt" | "ods" | "odp")
}

pub(crate) fn render(req: &Request, ext: &str) -> Preview {
    let xml = match Package::open(&req.bytes_path).and_then(|mut p| p.read("content.xml")) {
        Ok(Some(xml)) => xml,
        Ok(None) => {
            return Preview::Error("content.xml is missing (not an OpenDocument file?)".into())
        }
        Err(error) => return Preview::Error(error),
    };
    match parse(&xml, ext) {
        Ok(blocks) => Preview::Doc { blocks },
        Err(error) => Preview::Error(format!("malformed XML in content.xml: {error}")),
    }
}

#[derive(Default)]
struct Table {
    name: String,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: Option<String>,
}

fn parse(xml: &[u8], ext: &str) -> Result<Vec<DocBlock>, quick_xml::Error> {
    let mut reader = Reader::from_reader(xml);
    let mut blocks: Vec<DocBlock> = Vec::new();
    let mut table = Table::default();
    // Open paragraph: heading level (0 = plain) and text; `pdepth` handles text boxes
    // nested inside a paragraph.
    let mut para: Option<(u8, String)> = None;
    let (mut pdepth, mut tdepth, mut lists, mut skip, mut slides) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut rows = 0usize;
    let mut truncated = false;
    loop {
        let event = reader.read_event()?;
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                Event::Eof => break,
                _ => {}
            }
            continue;
        }
        match event {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e)
                if matches!(e.name().as_ref(), "text:s" | "text:tab" | "text:line-break") =>
            {
                if let Some((_, t)) = &mut para {
                    match e.name().as_ref() {
                        "text:tab" => t.push('\t'),
                        "text:line-break" => t.push('\n'),
                        _ => {
                            let n = attr(&e, "text:c").and_then(|c| c.parse().ok());
                            t.extend(std::iter::repeat_n(' ', n.unwrap_or(1usize).min(256)));
                        }
                    }
                }
            }
            Event::Start(e) => match e.name().as_ref() {
                "text:note" | "office:annotation" | "presentation:notes" => skip = 1,
                "text:p" | "text:h" => {
                    pdepth += 1;
                    if pdepth == 1 {
                        let level = if e.name().as_ref() == "text:h" {
                            attr(&e, "text:outline-level")
                                .and_then(|l| l.parse().ok())
                                .unwrap_or(1u8)
                                .clamp(1, 6)
                        } else {
                            0
                        };
                        para = Some((level, String::new()));
                    }
                }
                "text:list" => lists += 1,
                "draw:page" => {
                    slides += 1;
                    if slides > MAX_SLIDES {
                        truncated = true;
                        break;
                    }
                    blocks.push(DocBlock::Heading(2, format!("Slide {slides}")));
                }
                "table:table" => {
                    tdepth += 1;
                    if tdepth == 1 {
                        table = Table {
                            name: attr(&e, "table:name").unwrap_or_default(),
                            ..Table::default()
                        };
                    }
                }
                "table:table-row" if tdepth == 1 => table.row = Vec::new(),
                "table:table-cell" | "table:covered-table-cell" if tdepth == 1 => {
                    table.cell = Some(String::new())
                }
                _ => {}
            },
            Event::Empty(e) => {
                if matches!(
                    e.name().as_ref(),
                    "table:table-cell" | "table:covered-table-cell"
                ) && tdepth == 1
                {
                    table.row.push(String::new());
                }
            }
            Event::Text(t) => {
                if let Some((_, p)) = &mut para {
                    p.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some((_, p)) = &mut para {
                    p.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                if let (Some((_, p)), Some(s)) = (&mut para, entity(&r)) {
                    p.push_str(&s);
                }
            }
            Event::End(e) => match e.name().as_ref() {
                "text:p" | "text:h" => {
                    pdepth = pdepth.saturating_sub(1);
                    if pdepth == 0 {
                        if let Some((level, text)) = para.take() {
                            let text = text.trim();
                            if text.is_empty() {
                                continue;
                            }
                            if let Some(cell) = &mut table.cell {
                                if !cell.is_empty() {
                                    cell.push('\n');
                                }
                                cell.push_str(text);
                            } else if level > 0 {
                                blocks.push(DocBlock::Heading(level, text.to_owned()));
                            } else if lists > 0 {
                                let indent = "  ".repeat(lists - 1);
                                blocks.push(DocBlock::Para(format!("{indent}• {text}")));
                            } else {
                                blocks.push(DocBlock::Para(text.to_owned()));
                            }
                        }
                    }
                }
                "text:list" => lists = lists.saturating_sub(1),
                "table:table-cell" | "table:covered-table-cell" if tdepth == 1 => {
                    table.row.push(table.cell.take().unwrap_or_default());
                }
                "table:table-row" if tdepth == 1 => {
                    let mut row = std::mem::take(&mut table.row);
                    while row.last().is_some_and(String::is_empty) {
                        row.pop();
                    }
                    rows += 1;
                    if rows <= MAX_BLOCKS {
                        table.rows.push(row);
                    }
                }
                "table:table" => {
                    tdepth = tdepth.saturating_sub(1);
                    if tdepth == 0 {
                        let mut grid = std::mem::take(&mut table.rows);
                        while grid.last().is_some_and(Vec::is_empty) {
                            grid.pop();
                        }
                        if ext == "ods" {
                            blocks.push(DocBlock::Heading(2, std::mem::take(&mut table.name)));
                        }
                        if !grid.is_empty() {
                            blocks.push(DocBlock::Table(grid));
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        // Paragraphs and rows both count toward the 500-page budget.
        if blocks.len() + rows > MAX_BLOCKS {
            truncated = true;
            break;
        }
    }
    if truncated {
        blocks.push(DocBlock::Para(TRUNCATED.into()));
    }
    Ok(blocks)
}
