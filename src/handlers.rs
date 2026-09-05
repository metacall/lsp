//! LSP query handlers.
use lsp_types::{
    Hover, HoverContents, Location, MarkupContent, MarkupKind, Position, SymbolInformation,
};

use crate::convert;
use crate::index::{IndexSnapshot, definition_target, file_for_uri, symbol_at};

#[allow(deprecated)]
pub fn document_symbols(snapshot: &IndexSnapshot, uri: &str) -> Vec<SymbolInformation> {
    let Some(file) = file_for_uri(snapshot, uri) else {
        return Vec::new();
    };
    let Some(file_uri) = convert::path_to_uri(&file.path) else {
        return Vec::new();
    };
    file.symbols
        .iter()
        .map(|symbol| SymbolInformation {
            name: symbol.name.clone(),
            kind: convert::symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            location: Location {
                uri: file_uri.clone(),
                range: convert::range_to_lsp(&symbol.source_range),
            },
            container_name: None,
        })
        .collect()
}

pub fn hover_at(snapshot: &IndexSnapshot, uri: &str, pos: Position) -> Option<Hover> {
    let symbol = symbol_at(snapshot, uri, pos)?;
    let mut value = format!(
        "**{}** ({})\n\n```{}\n{}\n```",
        symbol.name,
        format!("{:?}", symbol.kind).to_ascii_lowercase(),
        symbol.language,
        symbol.signature.as_deref().unwrap_or(symbol.name.as_str())
    );
    if let Some(docstring) = symbol.docstring.as_deref() {
        value.push_str("\n\n");
        value.push_str(docstring);
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(convert::range_to_lsp(&symbol.source_range)),
    })
}

pub fn definition_at(snapshot: &IndexSnapshot, uri: &str, pos: Position) -> Option<Location> {
    let (target_uri, target_range) = definition_target(snapshot, uri, pos)?;
    Some(Location {
        uri: target_uri.parse().ok()?,
        range: convert::range_to_lsp(&target_range),
    })
}

pub fn diagnostics_for(snapshot: &IndexSnapshot, uri: &str) -> Vec<lsp_types::Diagnostic> {
    let Some(path) = convert::uri_to_path(uri) else {
        return Vec::new();
    };
    snapshot
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.path == path)
        .map(convert::diagnostic_to_lsp)
        .collect()
}
