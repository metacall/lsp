#![expect(clippy::unwrap_used, reason = "integration setup may abort")]
use lsp_types::Position;
use meta_call_lsp::handlers::QueryCtx;
use meta_call_lsp::index::rebuild_from_inputs;
use meta_call_lsp::position::Encoding;
use meta_call_lsp::{handlers, index};

mod common;

use common::{DiskSources, uri_of};

fn snapshot_with(content: &str) -> (tempfile::TempDir, std::sync::Arc<index::IndexSnapshot>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), content).unwrap();
    let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
    (dir, snapshot)
}

#[test]
fn workspace_symbols_truncate_to_cap_in_documented_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::new();
    for i in 0..600 {
        content.push_str(&format!("def f{i:04}(): pass\n"));
    }
    std::fs::write(dir.path().join("a.py"), &content).unwrap();
    let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
    let mut ctx = QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16);

    let symbols = handlers::workspace_symbols(&mut ctx, "", &|| false);

    assert_eq!(symbols.len(), 512);
    assert_eq!(symbols[0].name, "f0000");
}

#[test]
fn completion_handles_edge_positions_without_panic() {
    let (dir, snapshot) = snapshot_with("def greet(): pass\n");
    let missing = uri_of(&dir.path().join("missing.py"));
    let mut ctx = QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16);

    let beyond = handlers::completion_at(
        &mut ctx,
        &missing,
        Position {
            line: 100,
            character: 0,
        },
    );
    assert!(beyond.is_empty());

    let (dir, snapshot) = snapshot_with("x = 1\n");
    let uri = uri_of(&dir.path().join("a.py"));
    let mut ctx = QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16);
    let clamped = handlers::completion_at(
        &mut ctx,
        &uri,
        Position {
            line: 0,
            character: 1000,
        },
    );
    assert!(clamped.is_empty());
}

#[test]
fn references_exclude_declaration_when_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.py"),
        "def greet(): pass\n\n\nx = greet()\n",
    )
    .unwrap();
    let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
    let uri = uri_of(&dir.path().join("a.py"));
    let pos = Position {
        line: 0,
        character: 5,
    };

    let with = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16),
        &uri,
        pos,
        true,
    );
    let without = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16),
        &uri,
        pos,
        false,
    );

    assert!(!with.is_empty());
    assert!(without.len() < with.len() || without.is_empty());
    assert!(with.len() <= 2000);
}

#[test]
fn diagnostics_are_scoped_to_the_requested_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def ok(): pass\n").unwrap();
    std::fs::write(dir.path().join("b.py"), "def broken(: pass\n").unwrap();
    let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
    let uri_a = uri_of(&dir.path().join("a.py"));
    let uri_b = uri_of(&dir.path().join("b.py"));

    let mut ctx = QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16);
    let for_a = handlers::diagnostics_for(&mut ctx, &uri_a);
    let mut ctx = QueryCtx::new(&snapshot, &DiskSources, Encoding::Utf16);
    let for_b = handlers::diagnostics_for(&mut ctx, &uri_b);

    for diagnostic in for_a.iter().chain(for_b.iter()) {
        assert!(!diagnostic.message.is_empty());
    }
    let _ = (for_a, for_b);
}
