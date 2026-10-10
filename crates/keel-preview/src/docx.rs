use crate::office::{Package, MAX_BLOCKS, MAX_UNZIPPED, TRUNCATED};
use crate::{DocBlock, Preview, Request};
use docx_rs::{
    DocumentChild, InsertChild, Paragraph, ParagraphChild, Run, RunChild, StructuredDataTag,
    StructuredDataTagChild, Table, TableCellContent, TableChild, TableRowChild,
};
use std::collections::HashMap;

pub(crate) fn accepts(ext: &str) -> bool {
    ext == "docx"
}

pub(crate) fn render(req: &Request) -> Preview {
    let result = Package::open(&req.bytes_path)
        .and_then(|mut pkg| {
            if pkg.declared_size() > MAX_UNZIPPED {
                return Err(format!(
                    "document exceeds the {} MiB decompressed limit",
                    MAX_UNZIPPED >> 20
                ));
            }
            Ok(())
        })
        .and_then(|()| std::fs::read(&req.bytes_path).map_err(|error| error.to_string()))
        .and_then(|bytes| docx_rs::read_docx(&bytes).map_err(|error| error.to_string()));
    let docx = match result {
        Ok(docx) => docx,
        Err(error) => return Preview::Error(error),
    };
    // Lists and page breaks first; on any panic in that walk fall back to plain text.
    let rich = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk(&docx, true)));
    let blocks = rich.unwrap_or_else(|_| walk(&docx, false));
    Preview::Doc { blocks }
}

struct Ctx<'a> {
    docx: &'a docx_rs::Docx,
    rich: bool,
    /// Next number per (numbering id, level).
    counters: HashMap<(usize, usize), usize>,
}

fn walk(docx: &docx_rs::Docx, rich: bool) -> Vec<DocBlock> {
    let mut ctx = Ctx {
        docx,
        rich,
        counters: HashMap::new(),
    };
    let mut blocks = Vec::new();
    for child in &docx.document.children {
        if blocks.len() >= MAX_BLOCKS {
            blocks.push(DocBlock::Para(TRUNCATED.into()));
            break;
        }
        match child {
            DocumentChild::Paragraph(paragraph) => push_paragraph(&mut ctx, &mut blocks, paragraph),
            DocumentChild::Table(table) => blocks.push(DocBlock::Table(table_text(table))),
            DocumentChild::StructuredDataTag(tag) => push_tag(&mut ctx, &mut blocks, tag),
            _ => {}
        }
    }
    blocks
}

fn has_page_break(paragraph: &Paragraph) -> bool {
    paragraph.children.iter().any(|child| match child {
        ParagraphChild::Run(run) => run
            .children
            .iter()
            .any(|c| matches!(c, RunChild::Break(b) if format!("{b:?}").contains("Page"))),
        _ => false,
    })
}

/// "• " or "1. " for a list paragraph, indented by level.
fn list_marker(ctx: &mut Ctx, paragraph: &Paragraph) -> Option<String> {
    let np = paragraph.property.numbering_property.as_ref()?;
    let id = np.id.as_ref()?.id;
    if id == 0 {
        return None;
    }
    let level = np.level.as_ref().map_or(0, |l| l.val);
    let abstract_id = ctx
        .docx
        .numberings
        .numberings
        .iter()
        .find(|n| n.id == id)
        .map(|n| n.abstract_num_id);
    let bullet = abstract_id
        .and_then(|a| ctx.docx.numberings.abstract_nums.iter().find(|n| n.id == a))
        .and_then(|a| a.levels.iter().find(|l| l.level == level))
        .is_none_or(|l| l.format.val == "bullet");
    let indent = "  ".repeat(level);
    if bullet {
        return Some(format!("{indent}• "));
    }
    let n = ctx.counters.entry((id, level)).or_insert(0);
    *n += 1;
    let n = *n;
    ctx.counters.retain(|&(i, l), _| i != id || l <= level);
    Some(format!("{indent}{n}. "))
}

fn push_paragraph(ctx: &mut Ctx, blocks: &mut Vec<DocBlock>, paragraph: &Paragraph) {
    let mut value = String::new();
    children_text(&paragraph.children, &mut value);
    let rich = ctx.rich;
    if rich {
        value.truncate(value.trim_end().len());
    }
    if rich && paragraph.property.page_break_before == Some(true) {
        blocks.push(DocBlock::Para("---".into()));
    }
    let marker = if rich && !value.is_empty() {
        list_marker(ctx, paragraph)
    } else {
        None
    };
    if !value.is_empty() {
        let style = paragraph
            .property
            .style
            .as_ref()
            .map(|style| style.val.as_str());
        blocks.push(match (marker, style) {
            (Some(marker), _) => DocBlock::Para(format!("{marker}{value}")),
            (None, Some("Heading1") | Some("heading 1")) => DocBlock::Heading(1, value),
            (None, Some("Heading2") | Some("heading 2")) => DocBlock::Heading(2, value),
            (None, Some("Heading3") | Some("heading 3")) => DocBlock::Heading(3, value),
            _ => DocBlock::Para(value),
        });
    }
    if rich && has_page_break(paragraph) {
        blocks.push(DocBlock::Para("---".into()));
    }
}

/// Block-level content control: its paragraphs and tables become ordinary blocks.
fn push_tag(ctx: &mut Ctx, blocks: &mut Vec<DocBlock>, tag: &StructuredDataTag) {
    let mut inline = String::new();
    for child in &tag.children {
        match child {
            StructuredDataTagChild::Paragraph(paragraph) => push_paragraph(ctx, blocks, paragraph),
            StructuredDataTagChild::Table(table) => blocks.push(DocBlock::Table(table_text(table))),
            StructuredDataTagChild::StructuredDataTag(inner) => push_tag(ctx, blocks, inner),
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
