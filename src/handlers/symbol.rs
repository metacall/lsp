//! documentSymbol plus workspace/symbol.
use lsp_types::{DocumentSymbol, OneOf, WorkspaceSymbol};

use super::QueryCtx;
use crate::convert;
use crate::types::DocUri;

const CAP_WORKSPACE_SYMBOLS: usize = 512;

/// Match tier: exact name outranks case-insensitive substring; no fuzzy tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    Exact,
    Substring,
}

/// Empty query matches every symbol; otherwise exact first, then case-insensitive substring.
fn match_tier(name: &str, folded_name: &str, query: &str, folded_query: &str) -> Option<MatchTier> {
    if query.is_empty() || name == query {
        return Some(MatchTier::Exact);
    }
    if name.contains(query) || folded_name.contains(folded_query) {
        return Some(MatchTier::Substring);
    }
    None
}

/// One flat level; `selectionRange` must sit inside `range`: the identifier when captured, else the symbol range.
#[allow(deprecated)]
pub fn document_symbols(ctx: &mut QueryCtx, uri: &DocUri) -> Vec<DocumentSymbol> {
    let Some(file) = ctx.document(uri) else {
        return Vec::new();
    };
    file.symbols
        .iter()
        .map(|symbol| {
            let (range, selection_range) = ctx.symbol_ranges(symbol);
            DocumentSymbol {
                name: symbol.name.clone(),
                detail: symbol.signature.clone(),
                kind: convert::symbol_kind(symbol.kind),
                tags: None,
                deprecated: None,
                range,
                selection_range,
                children: None,
            }
        })
        .collect()
}

/// Ordered by (tier, path, declaration byte, name, id), capped after that order; `containerName` omitted.
pub fn workspace_symbols(
    ctx: &mut QueryCtx,
    query: &str,
    cancelled: &dyn Fn() -> bool,
) -> Vec<WorkspaceSymbol> {
    let folded_query = query.to_lowercase();
    let mut matches: Vec<(&meta_ast::Symbol, MatchTier)> = Vec::new();
    for symbol in ctx.snapshot().symbols() {
        if matches.len().is_multiple_of(256) && cancelled() {
            break;
        }
        let folded_name = symbol.name.to_lowercase();
        if let Some(tier) = match_tier(&symbol.name, &folded_name, query, &folded_query) {
            matches.push((symbol, tier));
        }
    }
    if matches.len() > CAP_WORKSPACE_SYMBOLS * 4 {
        let (_, _, _) =
            matches.select_nth_unstable_by(CAP_WORKSPACE_SYMBOLS, |(a, a_tier), (b, b_tier)| {
                a_tier
                    .cmp(b_tier)
                    .then_with(|| a.file_path.cmp(&b.file_path))
                    .then_with(|| a.source_range.byte_start.cmp(&b.source_range.byte_start))
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.id.cmp(&b.id))
            });
        matches.truncate(CAP_WORKSPACE_SYMBOLS);
    }
    matches.sort_by(|(a, a_tier), (b, b_tier)| {
        a_tier
            .cmp(b_tier)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.source_range.byte_start.cmp(&b.source_range.byte_start))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    if matches.len() > CAP_WORKSPACE_SYMBOLS {
        tracing::debug!(total = matches.len(), "workspace symbols truncated");
        matches.truncate(CAP_WORKSPACE_SYMBOLS);
    }

    matches
        .into_iter()
        .filter_map(|(symbol, _)| {
            let location = ctx.location(symbol)?;
            Some(WorkspaceSymbol {
                name: symbol.name.clone(),
                kind: convert::symbol_kind(symbol.kind),
                tags: None,
                container_name: None,
                location: OneOf::Left(location),
                data: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(name: &str, query: &str) -> Option<MatchTier> {
        let folded_name = name.to_lowercase();
        match_tier(name, &folded_name, query, &query.to_lowercase())
    }

    #[test]
    fn match_tiers_are_exact_then_substring() {
        assert_eq!(tier("greet", "greet"), Some(MatchTier::Exact));
        assert_eq!(tier("greeting", "greet"), Some(MatchTier::Substring));
        assert_eq!(tier("GREETING", "greet"), Some(MatchTier::Substring));
        assert_eq!(tier("greet", ""), Some(MatchTier::Exact));
        assert_eq!(tier("greet", "grt"), None);
        assert!(MatchTier::Exact < MatchTier::Substring);
    }
}
