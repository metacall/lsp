//! textDocument/completion.
use std::collections::HashSet;
use std::path::PathBuf;

use lsp_types::{CompletionItem, Position};
use meta_ast::ConfidenceTier;

use super::QueryCtx;
use crate::convert;
use crate::index;
use crate::types::DocUri;

const CAP_COMPLETION: usize = 1000;

/// Bucket by exact engine ladder equality; undefined is `Unknown`, last. 0 own or direct, 1 transitive, 2 cross or global, 3 computed.
fn tier_rank(confidence: f32) -> u8 {
    match meta_ast::confidence_tier(confidence) {
        ConfidenceTier::OwnOrDirect | ConfidenceTier::DefUse => 0,
        ConfidenceTier::Transitive => 1,
        ConfidenceTier::CrossLanguage | ConfidenceTier::ClientMultiGlobal => 2,
        ConfidenceTier::Computed | ConfidenceTier::Unknown => 3,
    }
}

struct Candidate {
    tier: u8,
    label: String,
    path: PathBuf,
    byte_start: usize,
    id: meta_ast::model::SymbolId,
    item: CompletionItem,
}

/// Names visible from the cursor, ordered by tier and then by name and site.
pub fn completion_at(ctx: &mut QueryCtx, uri: &DocUri, pos: Position) -> Vec<CompletionItem> {
    let Some((path, byte)) = ctx.cursor(uri, pos) else {
        return Vec::new();
    };
    let Some(prefix) = ctx.text(&path).map(|text| identifier_prefix(text, byte)) else {
        return Vec::new();
    };
    let prefix = Prefix::new(&prefix);
    let snapshot = ctx.snapshot();
    let Some(file_id) = snapshot.file_id(&path) else {
        return Vec::new();
    };
    let Some(scope) = snapshot.scope.scope(file_id) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for case_sensitive in [true, false] {
        for (name, scoped) in scope {
            if !prefix.matches(name, case_sensitive) {
                continue;
            }
            for (id, confidence) in scoped {
                push_candidate(&mut candidates, &mut seen, snapshot, name, *id, *confidence);
            }
        }
        if let Some(enclosing) = index::symbol_at(snapshot, uri, byte) {
            for (id, confidence) in snapshot.references_out(enclosing.id) {
                let Some(symbol) = snapshot.symbol_by_id(*id) else {
                    continue;
                };
                if prefix.matches(&symbol.name, case_sensitive) {
                    push_candidate(
                        &mut candidates,
                        &mut seen,
                        snapshot,
                        &symbol.name,
                        *id,
                        *confidence,
                    );
                }
            }
        }
        if !candidates.is_empty() || !case_sensitive {
            break;
        }
    }

    candidates.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.byte_start.cmp(&b.byte_start))
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(CAP_COMPLETION);
    candidates
        .into_iter()
        .map(|candidate| candidate.item)
        .collect()
}

struct Prefix<'a> {
    raw: &'a str,
    folded: String,
}

impl<'a> Prefix<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            folded: raw.to_lowercase(),
        }
    }

    fn matches(&self, name: &str, case_sensitive: bool) -> bool {
        if self.raw.is_empty() {
            return true;
        }
        if case_sensitive {
            return name.starts_with(self.raw);
        }
        if self.raw.is_ascii() && name.len() >= self.raw.len() {
            return name.as_bytes()[..self.raw.len()].eq_ignore_ascii_case(self.raw.as_bytes());
        }
        name.to_lowercase().starts_with(&self.folded)
    }
}

fn push_candidate(
    candidates: &mut Vec<Candidate>,
    seen: &mut HashSet<meta_ast::model::SymbolId>,
    snapshot: &index::IndexSnapshot,
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
    let tier = tier_rank(confidence);
    candidates.push(Candidate {
        tier,
        label: name.to_string(),
        path: symbol.file_path.clone(),
        byte_start: symbol.source_range.byte_start,
        id,
        item: CompletionItem {
            label: name.to_string(),
            kind: Some(convert::completion_kind(symbol.kind)),
            detail: symbol.signature.clone(),
            sort_text: Some(format!("{tier:02}-{name}")),
            ..Default::default()
        },
    });
}

fn identifier_prefix(text: &str, byte: usize) -> String {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) && byte > 0 {
        byte -= 1;
    }
    if !text.is_char_boundary(byte) {
        return String::new();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use meta_ast::graph::edge::{
        CONFIDENCE_CLIENT_MULTI_GLOBAL, CONFIDENCE_CLIENT_MULTI_LOAD,
        CONFIDENCE_CLIENT_UNIQUE_GLOBAL, CONFIDENCE_CLIENT_UNIQUE_LOAD, CONFIDENCE_COMPUTED,
        CONFIDENCE_CROSS_LANGUAGE, CONFIDENCE_DEF_USE, CONFIDENCE_OWN_OR_DIRECT,
        CONFIDENCE_TRANSITIVE,
    };

    #[test]
    fn confidence_tiers_use_exact_ladder_values() {
        assert_eq!(tier_rank(CONFIDENCE_OWN_OR_DIRECT), 0);
        assert_eq!(tier_rank(CONFIDENCE_DEF_USE), 0);
        assert_eq!(tier_rank(CONFIDENCE_CLIENT_UNIQUE_LOAD), 0);
        assert_eq!(tier_rank(CONFIDENCE_TRANSITIVE), 1);
        assert_eq!(tier_rank(CONFIDENCE_CLIENT_MULTI_LOAD), 1);
        assert_eq!(tier_rank(CONFIDENCE_CROSS_LANGUAGE), 2);
        assert_eq!(tier_rank(CONFIDENCE_CLIENT_UNIQUE_GLOBAL), 2);
        assert_eq!(tier_rank(CONFIDENCE_CLIENT_MULTI_GLOBAL), 2);
        assert_eq!(tier_rank(CONFIDENCE_COMPUTED), 3);
    }

    #[test]
    fn unknown_confidence_lands_in_the_last_tier() {
        assert_eq!(
            tier_rank(0.7),
            3,
            "a value outside the ladder must not land between two tiers"
        );
        assert_eq!(tier_rank(0.0), 3);
        assert_eq!(tier_rank(1.5), 3);
    }
}
