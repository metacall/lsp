//! definition plus references.
use std::path::PathBuf;

use lsp_types::{GotoDefinitionResponse, Location, LocationLink, Position};

use super::QueryCtx;
use crate::convert;
use crate::index;
use crate::types::DocUri;

const CAP_REFERENCES: usize = 2000;

/// Every definition target the engine resolved; no score picks a winner. `link_support` mirrors the client.
pub fn definition_at(
    ctx: &mut QueryCtx,
    uri: &DocUri,
    pos: Position,
    link_support: bool,
) -> Option<GotoDefinitionResponse> {
    let file = ctx.document(uri)?;
    let byte = ctx.byte_at(&file.path, pos)?;
    let source_path = file.path.clone();
    let targets = index::resolve_targets(ctx.snapshot(), uri, byte);
    if targets.is_empty() {
        return None;
    }

    if link_support {
        let mut links = Vec::with_capacity(targets.len());
        for (id, origin) in targets {
            let Some(symbol) = ctx.snapshot().symbol_by_id(id) else {
                continue;
            };
            let Some(target_uri) = convert::path_to_uri(&symbol.file_path) else {
                continue;
            };
            let (range, selection) = ctx.symbol_ranges(symbol);
            let origin_selection_range = origin.map(|range| ctx.range_for(&source_path, range));
            links.push(LocationLink {
                origin_selection_range,
                target_uri,
                target_range: range,
                target_selection_range: selection,
            });
        }
        return (!links.is_empty()).then_some(GotoDefinitionResponse::Link(links));
    }

    let mut locations = Vec::with_capacity(targets.len());
    for (id, _) in targets {
        let Some(symbol) = ctx.snapshot().symbol_by_id(id) else {
            continue;
        };
        if let Some(location) = ctx.location(symbol) {
            locations.push(location);
        }
    }
    (!locations.is_empty()).then_some(GotoDefinitionResponse::Array(locations))
}

struct ReferenceSite {
    path: PathBuf,
    byte: usize,
    location: Location,
}

pub fn references_at(
    ctx: &mut QueryCtx,
    uri: &DocUri,
    pos: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let Some(file) = ctx.document(uri) else {
        return Vec::new();
    };
    let Some(byte) = ctx.byte_at(&file.path, pos) else {
        return Vec::new();
    };
    let targets = index::resolve_targets(ctx.snapshot(), uri, byte);
    if targets.is_empty() {
        return Vec::new();
    }

    let mut declarations = Vec::new();
    let mut sites = Vec::new();
    for (id, _) in targets {
        if include_declaration
            && let Some(symbol) = ctx.snapshot().symbol_by_id(id)
            && let Some(location) = ctx.location(symbol)
        {
            declarations.push(ReferenceSite {
                path: symbol.file_path.clone(),
                byte: symbol.source_range.byte_start,
                location,
            });
        }
        for occurrence in ctx.snapshot().occurrences_of(id) {
            let Some(path) = ctx.snapshot().path_of(occurrence.file) else {
                continue;
            };
            let Some(owner_uri) = convert::path_to_uri(path) else {
                continue;
            };
            let location = Location {
                uri: owner_uri,
                range: ctx.range_for(path, &occurrence.range),
            };
            sites.push(ReferenceSite {
                path: path.to_path_buf(),
                byte: occurrence.range.byte_start,
                location,
            });
        }
    }

    order_sites(&mut declarations);
    order_sites(&mut sites);
    let mut locations: Vec<Location> = declarations.into_iter().map(|site| site.location).collect();
    locations.extend(sites.into_iter().map(|site| site.location));
    locations.truncate(CAP_REFERENCES);
    locations
}

fn order_sites(sites: &mut Vec<ReferenceSite>) {
    sites.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.byte.cmp(&b.byte)));
    sites.dedup_by(|a, b| a.path == b.path && a.byte == b.byte);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(path: &str, byte: usize) -> ReferenceSite {
        ReferenceSite {
            path: PathBuf::from(path),
            byte,
            location: Location {
                uri: "file:///a.py".parse().unwrap(),
                range: lsp_types::Range::default(),
            },
        }
    }

    #[test]
    fn reference_sites_order_by_path_then_byte() {
        let mut sites = vec![
            site("/w/a.py", 4),
            site("/w/b.py", 9),
            site("/w/a.py", 12),
            site("/w/a.py", 3),
        ];

        order_sites(&mut sites);

        let order: Vec<(&str, usize)> = sites
            .iter()
            .map(|site| (site.path.to_str().unwrap(), site.byte))
            .collect();
        assert_eq!(
            order,
            vec![
                ("/w/a.py", 3),
                ("/w/a.py", 4),
                ("/w/a.py", 12),
                ("/w/b.py", 9)
            ]
        );
    }

    #[test]
    fn duplicate_use_sites_collapse_to_one_entry() {
        let mut sites = vec![site("/w/a.py", 4), site("/w/a.py", 4), site("/w/a.py", 9)];

        order_sites(&mut sites);

        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].byte, 4);
        assert_eq!(sites[1].byte, 9);
    }
}
