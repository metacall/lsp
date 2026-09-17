//! Cursor resolution over the snapshot.
use std::cmp::Ordering;

use meta_ast::FileExtraction;
use meta_ast::model::{SourceRange, SymbolId};

use super::IndexSnapshot;
use crate::types::DocUri;

fn file_for_uri<'a>(snapshot: &'a IndexSnapshot, uri: &DocUri) -> Option<&'a FileExtraction> {
    snapshot.file_by_path(&uri.to_path()?)
}

fn contains_byte(range: &SourceRange, byte: usize) -> bool {
    if range.byte_end > range.byte_start {
        range.byte_start <= byte && byte < range.byte_end
    } else {
        byte == range.byte_start
    }
}

fn range_key(range: &SourceRange) -> (usize, usize, usize) {
    (
        range.byte_end.saturating_sub(range.byte_start),
        range.byte_start,
        range.byte_end,
    )
}

/// Innermost symbol containing `byte`; ties break on range key then id, never on engine order.
pub(super) fn smallest_symbol_at(file: &FileExtraction, byte: usize) -> Option<&meta_ast::Symbol> {
    file.symbols
        .iter()
        .filter(|symbol| contains_byte(&symbol.source_range, byte))
        .min_by(|a, b| {
            range_key(&a.source_range)
                .cmp(&range_key(&b.source_range))
                .then_with(|| a.id.cmp(&b.id))
        })
}

pub fn symbol_at<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &DocUri,
    byte: usize,
) -> Option<&'a meta_ast::Symbol> {
    let file = file_for_uri(snapshot, uri)?;
    smallest_symbol_at(file, byte)
}

/// Innermost unresolved reference at `byte`; the reference list is the authority, so unresolvable ones still match.
fn reference_at(file: &FileExtraction, byte: usize) -> Option<&meta_ast::UnresolvedReference> {
    file.references
        .iter()
        .filter(|reference| contains_byte(&reference.range, byte))
        .min_by(|a, b| {
            range_key(&a.range)
                .cmp(&range_key(&b.range))
                .then_with(|| a.name.cmp(&b.name))
        })
}

fn call_site_at(
    file: &FileExtraction,
    byte: usize,
) -> Option<&meta_ast::deploy::scanner::CallSite> {
    file.call_sites
        .iter()
        .filter(|site| {
            site.variant == meta_ast::deploy::scanner::CallSiteVariant::ClientCall
                && site
                    .source_range
                    .as_ref()
                    .is_some_and(|range| contains_byte(range, byte))
        })
        .min_by(|a, b| {
            a.source_range
                .as_ref()
                .map(range_key)
                .cmp(&b.source_range.as_ref().map(range_key))
                .then_with(|| a.function_name.cmp(&b.function_name))
        })
}

/// Total order over resolved targets: declaring path, declaration byte, id; confidence is not in the key.
fn compare_targets(snapshot: &IndexSnapshot, a: (SymbolId, f32), b: (SymbolId, f32)) -> Ordering {
    let key = |(id, _): (SymbolId, f32)| {
        snapshot.symbol_by_id(id).map(|symbol| {
            (
                symbol.file_path.as_path(),
                symbol.source_range.byte_start,
                id,
            )
        })
    };
    key(a).cmp(&key(b))
}

/// Dedup by id keeping the highest confidence, then order by declaration site; sets are tiny, so a linear merge wins.
fn dedup_targets(snapshot: &IndexSnapshot, targets: Vec<(SymbolId, f32)>) -> Vec<(SymbolId, f32)> {
    let mut merged: Vec<(SymbolId, f32)> = Vec::with_capacity(targets.len());
    for (id, confidence) in targets {
        match merged.iter_mut().find(|(existing, _)| *existing == id) {
            Some((_, existing)) => *existing = existing.max(confidence),
            None => merged.push((id, confidence)),
        }
    }
    merged.sort_by(|a, b| compare_targets(snapshot, *a, *b));
    merged
}

/// Targets of one reference: engine records for its byte, else the scope cache.
pub(super) fn reference_targets(
    snapshot: &IndexSnapshot,
    file_index: usize,
    reference: &meta_ast::UnresolvedReference,
) -> Vec<(SymbolId, f32)> {
    let recorded = snapshot.recorded_targets(file_index, reference.range.byte_start);
    if !recorded.is_empty() {
        return dedup_targets(snapshot, recorded.to_vec());
    }
    scoped_targets(snapshot, file_index, &reference.name)
}

/// Targets of one client call site; a resolved call always carries a record.
pub(super) fn client_call_targets(
    snapshot: &IndexSnapshot,
    file_index: usize,
    range: &SourceRange,
) -> Vec<(SymbolId, f32)> {
    let Some(path) = snapshot.path_of(file_index) else {
        return Vec::new();
    };
    let targets: Vec<(SymbolId, f32)> = snapshot
        .client_calls_at(path, range)
        .map(|call| (call.target, call.confidence))
        .collect();
    dedup_targets(snapshot, targets)
}

fn scoped_targets(snapshot: &IndexSnapshot, file_index: usize, name: &str) -> Vec<(SymbolId, f32)> {
    let scoped: Vec<(SymbolId, f32)> = snapshot
        .file_id_at(file_index)
        .and_then(|file_id| snapshot.scope.resolve(file_id, name))
        .map(<[(SymbolId, f32)]>::to_vec)
        .unwrap_or_default();
    dedup_targets(snapshot, scoped)
}

/// Every definition target at `byte`: call site records, else reference records, else the declaration; no score picks a winner.
pub fn resolve_targets<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &DocUri,
    byte: usize,
) -> Vec<(SymbolId, Option<&'a SourceRange>)> {
    let Some(path) = uri.to_path() else {
        return Vec::new();
    };
    let Some(file_index) = snapshot.file_index(&path) else {
        return Vec::new();
    };
    let file = &snapshot.extractions[file_index];

    if let Some(range) = call_site_at(file, byte).and_then(|site| site.source_range.as_ref()) {
        let ordered = client_call_targets(snapshot, file_index, range);
        if !ordered.is_empty() {
            return ordered
                .into_iter()
                .map(|(id, _)| (id, Some(range)))
                .collect();
        }
    }

    if let Some(reference) = reference_at(file, byte) {
        return reference_targets(snapshot, file_index, reference)
            .into_iter()
            .map(|(id, _)| (id, Some(&reference.range)))
            .collect();
    }

    smallest_symbol_at(file, byte)
        .map(|symbol| (symbol.id, None))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert;
    use crate::index::rebuild_from_inputs;

    use crate::testutil::doc_uri;

    #[test]
    fn unresolved_reference_does_not_fall_back_to_caller() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def caller():\n    return unknown_target(\"x\")\n").unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let extraction = &snapshot.extractions[0];
        let reference = extraction
            .references
            .iter()
            .find(|reference| reference.name == "unknown_target")
            .expect("reference");
        let uri = doc_uri(convert::path_to_uri(&file).unwrap().as_str());
        assert!(
            resolve_targets(&snapshot, &uri, reference.range.byte_start).is_empty(),
            "an unresolved reference must not fall back to the caller"
        );
    }

    #[test]
    fn declaration_resolves_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def greet(): pass\n").unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let symbol = &snapshot.extractions[0].symbols[0];
        let uri = doc_uri(convert::path_to_uri(&file).unwrap().as_str());

        let targets = resolve_targets(&snapshot, &uri, symbol.source_range.byte_start + 1);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, symbol.id);
        assert!(targets[0].1.is_none());
    }

    #[test]
    fn target_sets_order_by_declaration_site_not_score() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        let b = dir.path().join("b.py");
        std::fs::write(&a, "def dup(): pass\n").unwrap();
        std::fs::write(&b, "def dup(): pass\n").unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let id_a = snapshot.file_by_path(&a).unwrap().symbols[0].id;
        let id_b = snapshot.file_by_path(&b).unwrap().symbols[0].id;

        // The higher-scoring candidate is listed second; the path must win.
        let ordered = dedup_targets(&snapshot, vec![(id_b, 1.0), (id_a, 0.6)]);

        assert_eq!(ordered, vec![(id_a, 0.6), (id_b, 1.0)]);
    }

    #[test]
    fn duplicate_targets_merge_to_the_highest_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        std::fs::write(&a, "def dup(): pass\n").unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let id = snapshot.file_by_path(&a).unwrap().symbols[0].id;

        let ordered = dedup_targets(&snapshot, vec![(id, 0.6), (id, 1.0), (id, 0.8)]);

        assert_eq!(ordered, vec![(id, 1.0)]);
    }

    #[test]
    fn a_resolved_reference_answers_from_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        let b = dir.path().join("b.py");
        std::fs::write(&a, "def helper():\n    return 1\n").unwrap();
        std::fs::write(
            &b,
            "from a import helper\n\n\ndef caller():\n    return helper()\n",
        )
        .unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let helper = snapshot.file_by_path(&a).unwrap().symbols[0].id;
        let file_b = snapshot.file_by_path(&b).unwrap();
        let reference = file_b
            .references
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("reference");
        let uri = doc_uri(convert::path_to_uri(&b).unwrap().as_str());

        assert!(
            !snapshot
                .recorded_targets(snapshot.file_index(&b).unwrap(), reference.range.byte_start)
                .is_empty(),
            "a reference inside a symbol has a record"
        );
        let targets = resolve_targets(&snapshot, &uri, reference.range.byte_start);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, helper);
        assert_eq!(targets[0].1, Some(&reference.range));
    }

    #[test]
    fn a_module_level_reference_uses_the_scope_cache() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        let b = dir.path().join("b.py");
        std::fs::write(&a, "def helper():\n    return 1\n").unwrap();
        std::fs::write(&b, "from a import helper\n\n\nhelper()\n").unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let helper = snapshot.file_by_path(&a).unwrap().symbols[0].id;
        let file_b = snapshot.file_by_path(&b).unwrap();
        let reference = file_b
            .references
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("reference");
        let uri = doc_uri(convert::path_to_uri(&b).unwrap().as_str());

        assert!(
            snapshot
                .recorded_targets(snapshot.file_index(&b).unwrap(), reference.range.byte_start)
                .is_empty(),
            "the engine keeps no record without a source symbol"
        );
        let targets = resolve_targets(&snapshot, &uri, reference.range.byte_start);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, helper);
        assert_eq!(targets[0].1, Some(&reference.range));
    }

    #[test]
    fn a_self_recursive_call_keeps_its_use_site() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def loop():\n    return loop()\n").unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let loop_id = snapshot.extractions[0].symbols[0].id;

        let sites = snapshot.occurrences_of(loop_id);
        assert_eq!(
            sites.len(),
            1,
            "a recursive call is a real use site: {sites:?}"
        );    }
}
