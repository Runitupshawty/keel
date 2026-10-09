use crate::{DocBlock, Preview, Request};
use docx_rs::{
    DocumentChild, Paragraph, ParagraphChild, RunChild, Table, TableCellContent, TableChild,
    TableRowChild,
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
            DocumentChild::Paragraph(paragraph) => {
                let value = paragraph_text(paragraph);
                if value.is_empty() {
                    continue;
                }
                let style = paragraph
                    .property
                    .style
                    .as_ref()
                    .map(|style| style.val.as_str());
                let heading = match style {
                    Some("Heading1") | Some("heading 1") => Some(1),
                    Some("Heading2") | Some("heading 2") => Some(2),
                    Some("Heading3") | Some("heading 3") => Some(3),
                    _ => None,
                };
                blocks.push(match heading {
                    Some(level) => DocBlock::Heading(level, value),
                    None => DocBlock::Para(value),
                });
            }
            DocumentChild::Table(table) => blocks.push(DocBlock::Table(table_text(table))),
            _ => {}
        }
    }
    Preview::Doc { blocks }
}

fn paragraph_text(paragraph: &Paragraph) -> String {
    let mut value = String::new();
    for child in &paragraph.children {
        if let ParagraphChild::Run(run) = child {
            for child in &run.children {
                match child {
                    RunChild::Text(text) => value.push_str(&text.text),
                    RunChild::Tab(_) => value.push('\t'),
                    RunChild::Break(_) | RunChild::CarriageReturn(_) => value.push('\n'),
                    _ => {}
                }
            }
        }
    }
    value
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
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect()
        })
        .collect()
}
