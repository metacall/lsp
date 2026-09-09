//! LSP query handlers.
use lsp_types::{
    CompletionItem, Hover, HoverContents, Location, MarkupContent, MarkupKind, Position,
    SymbolInformation,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::convert;
use crate::index::{self, IndexSnapshot, SourceText};
use crate::position::{self, Encoding};

/// Source text memoized once for each distinct request path.
struct TextCache<'a> {
    lookup: &'a dyn SourceText,
    texts: HashMap<PathBuf, Option<Arc<str>>>,
}

impl<'a> TextCache<'a> {
    fn new(lookup: &'a dyn SourceText) -> Self {
        Self {
            lookup,
            texts: HashMap::new(),
        }
    }

    fn text(&mut self, path: &Path) -> Option<&str> {
        let lookup = self.lookup;
        let cached = self
            .texts
            .entry(path.to_path_buf())
            .or_insert_with_key(|key| {
                lookup
                    .source(key)
                    .map(|text| Arc::<str>::from(text.into_owned()))
            });
        cached.as_deref()
    }
}

fn location_for_symbol(
    cache: &mut TextCache,
    symbol: &meta_ast::Symbol,
    encoding: Encoding,
) -> Option<Location> {
    let uri = convert::path_to_uri(&symbol.file_path)?;
    let range = {
        let text = cache.text(&symbol.file_path);
        position::range(text, &symbol.source_range, encoding)
    };
    Some(Location { uri, range })
}

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
    let mut cache = TextCache::new(sources);
    let Some(file_uri) = convert::path_to_uri(&file.path) else {
        return Vec::new();
    };
    let range_text = cache.text(&file.path).map(str::to_owned);
    file.symbols
        .iter()
        .map(|symbol| SymbolInformation {
            name: symbol.name.clone(),
            kind: convert::symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            location: Location {
                uri: file_uri.clone(),
                range: position::range(range_text.as_deref(), &symbol.source_range, encoding),
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
    let mut cache = TextCache::new(sources);
    let file = index::file_for_uri(snapshot, uri)?;
    let byte = {
        let text = cache.text(&file.path)?;
        position::to_byte_offset(text, pos, encoding)?
    };
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
            cache.text(&file.path),
            &symbol.source_range,
            encoding,
        )),
    })
}

/// Resolve the cursor through the engine scope cache.
pub fn definition_at(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    pos: Position,
    encoding: Encoding,
) -> Option<Location> {
    let mut cache = TextCache::new(sources);
    let file = index::file_for_uri(snapshot, uri)?;
    let byte = {
        let text = cache.text(&file.path)?;
        position::to_byte_offset(text, pos, encoding)?
    };
    let target = index::resolve_at(snapshot, uri, byte)?;
    let symbol = snapshot.symbol_by_id(target)?;
    location_for_symbol(&mut cache, symbol, encoding)
}

/// Every use site of the symbol under the cursor.
pub fn references_at(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    pos: Position,
    encoding: Encoding,
    include_declaration: bool,
) -> Vec<Location> {
    let mut cache = TextCache::new(sources);
    let Some(file) = index::file_for_uri(snapshot, uri) else {
        return Vec::new();
    };
    let Some(byte) = (|| {
        let text = cache.text(&file.path)?;
        position::to_byte_offset(text, pos, encoding)
    })() else {
        return Vec::new();
    };
    let Some(target) = index::resolve_at(snapshot, uri, byte) else {
        return Vec::new();
    };
    let mut located: Vec<(f32, PathBuf, meta_ast::model::SymbolId)> = snapshot
        .references_in(target)
        .iter()
        .filter_map(|(id, confidence)| {
            let symbol = snapshot.symbol_by_id(*id)?;
            Some((*confidence, symbol.file_path.clone(), *id))
        })
        .collect();
    if include_declaration && let Some(symbol) = snapshot.symbol_by_id(target) {
        located.push((f32::INFINITY, symbol.file_path.clone(), target));
    }
    located.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.to_raw().cmp(&b.2.to_raw()))
    });
    let mut seen = HashSet::new();
    located.retain(|(_, _, id)| seen.insert(*id));
    located.truncate(2000);
    located
        .iter()
        .filter_map(|(_, _, id)| {
            let symbol = snapshot.symbol_by_id(*id)?;
            location_for_symbol(&mut cache, symbol, encoding)
        })
        .collect()
}

/// Case-insensitive substring search over every symbol.
#[allow(deprecated)]
pub fn workspace_symbols(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    query: &str,
    encoding: Encoding,
) -> Vec<SymbolInformation> {
    let mut cache = TextCache::new(sources);
    let needle = query.to_ascii_lowercase();
    snapshot
        .symbols()
        .filter(|symbol| needle.is_empty() || symbol.name.to_ascii_lowercase().contains(&needle))
        .take(512)
        .filter_map(|symbol| {
            let location = location_for_symbol(&mut cache, symbol, encoding)?;
            Some(SymbolInformation {
                name: symbol.name.clone(),
                kind: convert::symbol_kind(symbol.kind),
                tags: None,
                deprecated: None,
                location,
                container_name: symbol
                    .file_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Names visible from the cursor, ranked by engine confidence.
pub fn completion_at(
    snapshot: &IndexSnapshot,
    sources: &dyn SourceText,
    uri: &str,
    pos: Position,
    encoding: Encoding,
) -> Vec<CompletionItem> {
    let Some(file) = index::file_for_uri(snapshot, uri) else {
        return Vec::new();
    };
    let Some(text) = sources.source(&file.path) else {
        return Vec::new();
    };
    let Some(byte) = position::to_byte_offset(&text, pos, encoding) else {
        return Vec::new();
    };
    let prefix = identifier_prefix(&text, byte);
    let Some(file_id) = snapshot.file_id(&file.path) else {
        return Vec::new();
    };
    let Some(scope) = snapshot.scope.scope(file_id) else {
        return Vec::new();
    };

    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for case_sensitive in [true, false] {
        collect_scope_completions(
            snapshot,
            scope,
            &prefix,
            case_sensitive,
            &mut seen,
            &mut items,
        );
        if let Some(enclosing) = index::symbol_at(snapshot, uri, byte) {
            collect_reference_completions(
                snapshot,
                snapshot.references_out(enclosing.id),
                &prefix,
                case_sensitive,
                &mut seen,
                &mut items,
            );
        }
        if !items.is_empty() || !case_sensitive {
            break;
        }
        seen.clear();
    }

    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items.truncate(1000);
    items
}

fn completion_matches(name: &str, prefix: &str, case_sensitive: bool) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if case_sensitive {
        name.starts_with(prefix)
    } else {
        name.to_lowercase().starts_with(&prefix.to_lowercase())
    }
}

fn collect_scope_completions(
    snapshot: &IndexSnapshot,
    scope: &meta_ast::ScopeMap,
    prefix: &str,
    case_sensitive: bool,
    seen: &mut std::collections::HashSet<meta_ast::model::SymbolId>,
    items: &mut Vec<CompletionItem>,
) {
    for (name, candidates) in scope {
        if !completion_matches(name, prefix, case_sensitive) {
            continue;
        }
        for (id, confidence) in candidates {
            push_candidate(items, seen, snapshot, name, *id, *confidence);
        }
    }
}

fn collect_reference_completions(
    snapshot: &IndexSnapshot,
    references: &[(meta_ast::model::SymbolId, f32)],
    prefix: &str,
    case_sensitive: bool,
    seen: &mut std::collections::HashSet<meta_ast::model::SymbolId>,
    items: &mut Vec<CompletionItem>,
) {
    for (id, confidence) in references {
        let Some(symbol) = snapshot.symbol_by_id(*id) else {
            continue;
        };
        if completion_matches(&symbol.name, prefix, case_sensitive) {
            push_candidate(items, seen, snapshot, &symbol.name, *id, *confidence);
        }
    }
}

fn push_candidate(
    items: &mut Vec<CompletionItem>,
    seen: &mut std::collections::HashSet<meta_ast::model::SymbolId>,
    snapshot: &IndexSnapshot,
    name: &str,
    id: meta_ast::model::SymbolId,
    confidence: f32,
) {
    if !seen.insert(id) {
        return;
    }
    let Some(symbol) = snapshot.symbol_by_id(id) else {
        return;
    };
    items.push(CompletionItem {
        label: name.to_string(),
        kind: Some(convert::completion_kind(symbol.kind)),
        detail: symbol.signature.clone(),
        filter_text: Some(name.to_string()),
        sort_text: Some(format!("{}{name}", confidence_bucket(confidence))),
        ..Default::default()
    });
}

fn confidence_bucket(confidence: f32) -> char {
    if confidence >= 0.99 {
        '0'
    } else if confidence >= 0.7 {
        '1'
    } else {
        '2'
    }
}

fn identifier_prefix(text: &str, byte: usize) -> String {
    let mut start = byte;
    for (index, ch) in text[..byte].char_indices().rev() {
        if ch.is_alphanumeric() || ch == '_' {
            start = index;
        } else {
            break;
        }
    }
    text[start..byte].to_string()
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
