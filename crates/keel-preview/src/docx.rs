use crate::{DocBlock, Preview, Request};
use docx_rs::{
    DocumentChild, InsertChild, Paragraph, ParagraphChild, Run, RunChild, StructuredDataTag,
    StructuredDataTagChild, Table, TableCellContent, TableChild, TableRowChild,
};

pub(crate) fn accepts(ext: &str) -> bool {
    ext == "docx"
}

pub(crate) fn render(req: &Request) -> Preview {
    let result = std::fs::read(&req.bytes_path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| docx_rs::read_docx(&bytes).map_err(|error| error.to_string()));
    let document = match result {
        Ok(document) => document,
        Err(error) => return Preview::Error(error),
    };
    let mut blocks = Vec::new();
    for child in &document.document.children {
        match child {
            DocumentChild::Paragraph(paragraph) => push_paragraph(&mut blocks, paragraph),
            DocumentChild::Table(table) => blocks.push(DocBlock::Table(table_text(table))),
            DocumentChild::StructuredDataTag(tag) => push_tag(&mut blocks, tag),
            _ => {}
        }
    }
    Preview::Doc { blocks }
}

fn push_paragraph(blocks: &mut Vec<DocBlock>, paragraph: &Paragraph) {
    let mut value = String::new();
    children_text(&paragraph.children, &mut value);
    if value.is_empty() {
        return;
    }
    let style = paragraph
        .property
        .style
        .as_ref()
        .map(|style| style.val.as_str());
    blocks.push(match style {
        Some("Heading1") | Some("heading 1") => DocBlock::Heading(1, value),
        Some("Heading2") | Some("heading 2") => DocBlock::Heading(2, value),
        Some("Heading3") | Some("heading 3") => DocBlock::Heading(3, value),
        _ => DocBlock::Para(value),
    });
}

/// Block-level content control: its paragraphs and tables become ordinary blocks.
fn push_tag(blocks: &mut Vec<DocBlock>, tag: &StructuredDataTag) {
    let mut inline = String::new();
    for child in &tag.children {
        match child {
            StructuredDataTagChild::Paragraph(paragraph) => push_paragraph(blocks, paragraph),
            StructuredDataTagChild::Table(table) => blocks.push(DocBlock::Table(table_text(table))),
            StructuredDataTagChild::StructuredDataTag(inner) => push_tag(blocks, inner),
            StructuredDataTagChild::Run(run) => run_text(run, &mut inline),
            _ => {}
        }
    }
    if !inline.is_empty() {
        blocks.push(DocBlock::Para(inline));
    }
}

fn paragraph_text(paragraph: &Paragraph) -> String {
    let mut value = String::new();
    children_text(&paragraph.children, &mut value);
    value
}

/// Runs, plus runs nested in hyperlinks, tracked insertions and inline content controls.
fn children_text(children: &[ParagraphChild], value: &mut String) {
    for child in children {
        match child {
            ParagraphChild::Run(run) => run_text(run, value),
            ParagraphChild::Hyperlink(link) => children_text(&link.children, value),
            ParagraphChild::Insert(insert) => {
                for child in &insert.children {
                    if let InsertChild::Run(run) = child {
                        run_text(run, value);
                    }
                }
            }
            ParagraphChild::StructuredDataTag(tag) => tag_text(tag, value),
            _ => {}
        }
    }
}

fn tag_text(tag: &StructuredDataTag, value: &mut String) {
    for child in &tag.children {
        match child {
            StructuredDataTagChild::Run(run) => run_text(run, value),
            StructuredDataTagChild::Paragraph(paragraph) => {
                children_text(&paragraph.children, value)
            }
            StructuredDataTagChild::StructuredDataTag(inner) => tag_text(inner, value),
            _ => {}
        }
    }
}

fn run_text(run: &Run, value: &mut String) {
    for child in &run.children {
        match child {
            RunChild::Text(text) => value.push_str(&text.text),
            RunChild::Tab(_) => value.push('\t'),
            RunChild::Break(_) | RunChild::CarriageReturn(_) => value.push('\n'),
            _ => {}
        }
    }
}

fn table_text(table: &Table) -> Vec<Vec<String>> {
    table
        .rows
        .iter()
        .map(|row| {
            let TableChild::TableRow(row) = row;
            row.cells
                .iter()
                .map(|cell| {
                    let TableRowChild::TableCell(cell) = cell;
                    cell.children
                        .iter()
                        .filter_map(|content| match content {
                            TableCellContent::Paragraph(paragraph) => {
                                Some(paragraph_text(paragraph))
                            }
                            TableCellContent::StructuredDataTag(tag) => {
                                let mut value = String::new();
                                tag_text(tag, &mut value);
                                Some(value)
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect()
        })
        .collect()
}
