//! Cross-language fixture: a TypeScript caller resolving a Python declaration.
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use lsp_types::Position;
use meta_call_lsp::handlers::{self, QueryCtx};
use meta_call_lsp::index::{self, IndexSnapshot, SourceText};
use meta_call_lsp::position::{Encoding, LineIndex};
use meta_call_lsp::types::DocUri;

const MIXED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mixed");
const PY: &str = "pricing.py";
const TS: &str = "caller.ts";

fn fixture(name: &str) -> PathBuf {
    Path::new(MIXED).join(name)
}

fn doc_uri(path: &Path) -> DocUri {
    let uri = meta_call_lsp::convert::path_to_uri(path).expect("uri");
    DocUri::try_from(uri.as_str()).expect("document URI")
}

struct DiskSources;

impl SourceText for DiskSources {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        std::fs::read_to_string(path).ok().map(Cow::Owned)
    }
}

fn snapshot() -> std::sync::Arc<IndexSnapshot> {
    index::rebuild_from_inputs(Path::new(MIXED), &[]).expect("snapshot")
}

/// Position `offset` bytes into the first occurrence of `needle`.
fn position_in(name: &str, needle: &str, offset: usize) -> Position {
    let text = std::fs::read_to_string(fixture(name)).expect("fixture text");
    let byte = text.find(needle).expect("needle in fixture") + offset;
    LineIndex::new(&text).to_position(&text, byte, Encoding::Utf16)
}

fn symbol_in(snapshot: &IndexSnapshot, name: &str, file: &str) -> meta_ast::model::SymbolId {
    snapshot
        .symbols()
        .find(|symbol| symbol.name == name && symbol.file_path == fixture(file))
        .expect("symbol in fixture")
        .id
}

fn targets_of(
    snapshot: &IndexSnapshot,
    id: meta_ast::model::SymbolId,
) -> Vec<(meta_ast::model::SymbolId, f32)> {
    snapshot.references_out(id).to_vec()
}

#[test]
fn the_cross_language_call_resolves_to_the_python_declaration() {
    let snapshot = snapshot();

    let calls = &snapshot.client_calls;
    assert_eq!(
        calls.len(),
        2,
        "a call site in the caller and one in pricing.py"
    );
    let from_caller = calls
        .iter()
        .find(|call| call.source_file == fixture(TS))
        .expect("the TypeScript call site");
    let target = snapshot
        .symbol_by_id(from_caller.target)
        .expect("resolved target");
    assert_eq!(target.name, "multiply");
    assert_eq!(target.file_path, fixture(PY));
    assert!(
        from_caller.source_range.is_some(),
        "the call site carries its range"
    );
}

#[test]
fn definition_from_the_typescript_call_reaches_the_python_declaration() {
    let snapshot = snapshot();
    let sources = DiskSources;
    let uri = doc_uri(&fixture(TS));
    let position = position_in(TS, "multiply", 3);

    let locations = match handlers::definition_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        position,
        false,
    ) {
        Some(lsp_types::GotoDefinitionResponse::Array(locations)) => locations,
        other => panic!("expected plain locations, got {other:?}"),
    };

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri.as_str(), doc_uri(&fixture(PY)).as_str());
    let declaration_line = std::fs::read_to_string(fixture(PY))
        .expect("fixture text")
        .lines()
        .position(|line| line.starts_with("def multiply"))
        .expect("the declaration line") as u32;
    assert_eq!(locations[0].range.start.line, declaration_line);
}

#[test]
fn references_from_the_python_declaration_include_the_typescript_call() {
    let snapshot = snapshot();
    let sources = DiskSources;
    let uri = doc_uri(&fixture(PY));
    let position = position_in(PY, "multiply", 3);

    let references = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        position,
        true,
    );

    let mut uris: Vec<&str> = references
        .iter()
        .map(|location| location.uri.as_str())
        .collect();
    uris.sort();
    uris.dedup();
    let mut expected = [doc_uri(&fixture(PY)), doc_uri(&fixture(TS))];
    expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(
        uris,
        expected.iter().map(|uri| uri.as_str()).collect::<Vec<_>>(),
        "the declaration with its local use, and the TypeScript call site"
    );
    assert_eq!(references[0].uri.as_str(), doc_uri(&fixture(PY)).as_str());
    assert_eq!(
        references.len(),
        4,
        "the declaration, the direct call, the client call, and the TypeScript site"
    );
}

#[test]
fn workspace_symbol_finds_the_python_declaration() {
    let snapshot = snapshot();
    let sources = DiskSources;
    let mut ctx = QueryCtx::new(&snapshot, &sources, Encoding::Utf16);

    let symbols = handlers::workspace_symbols(&mut ctx, "multiply");

    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "multiply");
}

#[test]
fn completion_at_the_call_site_offers_the_python_symbol() {
    let snapshot = snapshot();
    let sources = DiskSources;
    let uri = doc_uri(&fixture(TS));
    let position = position_in(TS, "multiply", 2);

    let items = handlers::completion_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        position,
    );

    let multiply = items
        .iter()
        .find(|item| item.label == "multiply")
        .expect("the cross-language symbol is offered");
    assert_eq!(
        multiply.sort_text.as_deref(),
        Some("2multiply"),
        "cross-language is the third bucket"
    );
}

/// One target reached at two confidences folds once, keeping the maximum.
#[test]
fn the_fold_keeps_one_entry_per_target_and_the_highest_confidence() {
    let snapshot = snapshot();
    let multiply = symbol_in(&snapshot, "multiply", PY);
    let total = symbol_in(&snapshot, "total", PY);

    let call = snapshot
        .client_calls
        .iter()
        .find(|call| call.source_file == fixture(PY))
        .expect("the client call inside total");
    assert_eq!(call.target, multiply);
    assert_eq!(call.confidence, 0.6, "a unique global call");

    let recorded: Vec<f32> = snapshot
        .references
        .iter()
        .filter(|record| record.target == multiply && record.source == total)
        .map(|record| record.confidence)
        .collect();
    assert_eq!(recorded, vec![1.0], "the local call resolves at 1.0");

    let folded = targets_of(&snapshot, total);
    assert!(
        folded.contains(&(multiply, 1.0)),
        "the pair folds to the maximum of 1.0 and 0.6, got {folded:?}"
    );
    assert_eq!(
        targets_of(&snapshot, symbol_in(&snapshot, "compute_total", TS)),
        vec![(multiply, 0.6)],
        "a call site with no other evidence keeps the ladder value"
    );

    for symbol in snapshot.symbols() {
        let mut targets: Vec<_> = targets_of(&snapshot, symbol.id)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let count = targets.len();
        targets.sort();
        targets.dedup();
        assert_eq!(
            targets.len(),
            count,
            "no target appears twice for one source"
        );
    }
}

#[test]
fn hover_snapshot_for_the_python_symbol() {
    let snapshot = snapshot();
    let sources = DiskSources;
    let uri = doc_uri(&fixture(PY));
    let position = position_in(PY, "multiply", 3);

    let hover = handlers::hover_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        position,
    )
    .expect("hover");

    insta::assert_json_snapshot!(hover);
}

#[test]
fn completion_ranking_snapshot_at_the_call_site() {
    let snapshot = snapshot();
    let sources = DiskSources;
    let uri = doc_uri(&fixture(TS));
    let position = position_in(TS, "multiply", 2);

    let items = handlers::completion_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        position,
    );

    insta::assert_json_snapshot!(items);
}
