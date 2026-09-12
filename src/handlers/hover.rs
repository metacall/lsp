//! textDocument/hover.
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use super::QueryCtx;
use crate::convert;
use crate::index;
use crate::types::DocUri;

pub fn hover_at(ctx: &mut QueryCtx, uri: &DocUri, pos: Position) -> Option<Hover> {
    let file = ctx.document(uri)?;
    let byte = ctx.byte_at(&file.path, pos)?;
    let symbol = index::symbol_at(ctx.snapshot(), uri, byte)?;
    let mut value = format!(
        "**{}** ({})\n\n```{}\n{}\n```",
        symbol.name,
        convert::kind_word(symbol.kind),
        symbol.language,
        symbol.signature.as_deref().unwrap_or(symbol.name.as_str())
    );
    if let Some(docstring) = symbol.docstring.as_deref() {
        value.push_str("\n\n");
        value.push_str(docstring);
    }
    let range = ctx.range_for(&file.path, &symbol.source_range);
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(range),
    })
}
