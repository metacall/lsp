use std::path::{Path, PathBuf};

use lsp_types::Position;
use meta_call_lsp::buffers::BufferStore;
use meta_call_lsp::{convert, handlers, index};

const APP: &str =
    "def greet(name):\n    \"\"\"Say hi.\"\"\"\n    return name\n\n\nresult = greet(\"x\")\n";
const TS: &str = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";

fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("a.py");
    let ts = dir.path().join("b.ts");
    std::fs::write(&app, APP).unwrap();
    std::fs::write(&ts, TS).unwrap();
    (dir, app, ts)
}

fn uri_of(path: &Path) -> String {
    convert::path_to_uri(path).unwrap().as_str().to_string()
}

fn snapshot(dir: &Path, buffers: &BufferStore) -> index::IndexSnapshot {
    index::rebuild(dir, buffers, 1).unwrap()
}

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

#[test]
fn cold_rebuild_finds_symbols() {
    let (dir, _, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    assert_eq!(snapshot.extractions.len(), 2);
    let names: Vec<&str> = snapshot
        .extractions
        .iter()
        .flat_map(|file| file.symbols.iter().map(|symbol| symbol.name.as_str()))
        .collect();
    assert!(names.contains(&"greet"));
    assert!(names.contains(&"add"));
    assert!(snapshot.diagnostics.is_empty());
}

#[test]
fn document_symbols_lists_file_symbols() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let symbols = handlers::document_symbols(&snapshot, uri_of(&app).as_str());
    assert!(symbols.iter().any(|symbol| symbol.name == "greet"));
}

#[test]
fn hover_shows_signature() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let hover = handlers::hover_at(&snapshot, uri_of(&app).as_str(), pos(0, 5)).unwrap();
    let lsp_types::HoverContents::Markup(content) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(content.value.contains("greet"));
}

#[test]
fn definition_resolves_reference_to_def() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let uri = uri_of(&app);
    let location = handlers::definition_at(&snapshot, uri.as_str(), pos(5, 10)).unwrap();
    assert_eq!(location.uri.as_str(), uri.as_str());
    assert_eq!(location.range.start.line, 0);
}

#[test]
fn buffer_override_adds_symbol() {
    let (dir, app, _) = workspace();
    let uri = uri_of(&app);
    let mut buffers = BufferStore::default();
    assert!(buffers.open(
        uri.as_str(),
        2,
        "python",
        format!("{APP}\n\ndef extra(): pass\n")
    ));
    let snapshot = snapshot(dir.path(), &buffers);
    let symbols = handlers::document_symbols(&snapshot, uri.as_str());
    assert!(symbols.iter().any(|symbol| symbol.name == "extra"));
    assert!(handlers::hover_at(&snapshot, uri.as_str(), pos(0, 5)).is_some());
}

#[test]
fn unknown_uri_returns_empty() {
    let (dir, _, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    assert!(handlers::document_symbols(&snapshot, "file:///missing.py").is_empty());
    assert!(handlers::hover_at(&snapshot, "file:///missing.py", pos(0, 0)).is_none());
    assert!(handlers::definition_at(&snapshot, "file:///missing.py", pos(0, 0)).is_none());
}
