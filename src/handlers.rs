//! LSP query handlers.
use lsp_types::{
    Hover, HoverContents, Location, MarkupContent, MarkupKind, Position, SymbolInformation,
};

use crate::convert;
use crate::index::{self, IndexSnapshot, SourceText};
use crate::position::{self, Encoding};

#[allow(deprecated)]
pub fn document_symbols(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    encoding: Encoding,
) -> Vec<SymbolInformation> {
    let Some(file) = index::file_for_uri(snapshot, uri) else {
        return Vec::new();
    };
    let Some(file_uri) = convert::path_to_uri(&file.path) else {
        return Vec::new();
    };
    let text = sources.source(&file.path);
    file.symbols
        .iter()
        .map(|symbol| SymbolInformation {
            name: symbol.name.clone(),
            kind: convert::symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            location: Location {
                uri: file_uri.clone(),
                range: position::range(text.as_deref(), &symbol.source_range, encoding),
            },
            container_name: None,
        })
        .collect()
}

pub fn hover_at(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    pos: Position,
    encoding: Encoding,
) -> Option<Hover> {
    let file = index::file_for_uri(snapshot, uri)?;
    let text = sources.source(&file.path)?;
    let byte = position::to_byte_offset(&text, pos, encoding)?;
    let symbol = index::symbol_at(snapshot, uri, byte)?;
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
        range: Some(position::range(
            Some(text.as_ref()),
            &symbol.source_range,
            encoding,
        )),
    })
}

pub fn definition_at(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    pos: Position,
    encoding: Encoding,
) -> Option<Location> {
    let file = index::file_for_uri(snapshot, uri)?;
    let text = sources.source(&file.path)?;
    let byte = position::to_byte_offset(&text, pos, encoding)?;
    let target = index::definition_target(snapshot, uri, byte)?;
    let target_uri = convert::path_to_uri(&target.path)?;
    let target_text = sources.source(&target.path);
    Some(Location {
        uri: target_uri,
        range: position::range(target_text.as_deref(), &target.range, encoding),
    })
}

pub fn diagnostics_for(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    encoding: Encoding,
) -> Vec<lsp_types::Diagnostic> {
    let Some(path) = convert::uri_to_path(uri) else {
        return Vec::new();
    };
    let text = sources.source(&path);
    snapshot
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.path == path)
        .map(|diagnostic| convert::diagnostic_to_lsp(text.as_deref(), diagnostic, encoding))
        .collect()
}
