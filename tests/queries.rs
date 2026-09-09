use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::Position;
use meta_call_lsp::buffers::BufferStore;
use meta_call_lsp::index::SourceText;
use meta_call_lsp::position::Encoding;
use meta_call_lsp::{convert, handlers, index};

const APP: &str =
    "def greet(name):\n    \"\"\"Say hi.\"\"\"\n    return name\n\n\nresult = greet(\"x\")\n";
const TS: &str = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";
const EMOJI_TS: &str =
    "const snake = \"\u{1f40d}\"; function add(a: number, b: number): number { return a + b; }\n";

/// Test source lookup. Mirrors the server: overlay text wins over disk.
struct TestSources {
    overlay: HashMap<PathBuf, String>,
}

impl TestSources {
    fn disk() -> Self {
        Self {
            overlay: HashMap::new(),
        }
    }

    fn from_buffers(buffers: &BufferStore) -> Self {
        let mut overlay = HashMap::new();
        for (uri, doc) in buffers.iter() {
            if let Some(path) = convert::uri_to_path(uri) {
                overlay.insert(path, doc.text.clone());
            }
        }
        Self { overlay }
    }
}

impl SourceText for TestSources {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        if let Some(text) = self.overlay.get(path) {
            return Some(Cow::Borrowed(text.as_str()));
        }
        std::fs::read_to_string(path).ok().map(Cow::Owned)
    }
}

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
    index::rebuild_from_inputs(dir, &index::collect_inputs(dir, buffers), 1).unwrap()
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
    let sources = TestSources::disk();
    let symbols =
        handlers::document_symbols(&snapshot, &sources, uri_of(&app).as_str(), Encoding::Utf16);
    assert!(symbols.iter().any(|symbol| symbol.name == "greet"));
}

#[test]
fn hover_shows_signature() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let hover = handlers::hover_at(
        &snapshot,
        &sources,
        uri_of(&app).as_str(),
        pos(0, 5),
        Encoding::Utf16,
    )
    .unwrap();
    let lsp_types::HoverContents::Markup(content) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(content.value.contains("greet"));
}

#[test]
fn definition_resolves_reference_to_def() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&app);
    let location = handlers::definition_at(
        &snapshot,
        &sources,
        uri.as_str(),
        pos(5, 10),
        Encoding::Utf16,
    )
    .unwrap();
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
    let sources = TestSources::from_buffers(&buffers);
    let symbols = handlers::document_symbols(&snapshot, &sources, uri.as_str(), Encoding::Utf16);
    assert!(symbols.iter().any(|symbol| symbol.name == "extra"));
    assert!(
        handlers::hover_at(
            &snapshot,
            &sources,
            uri.as_str(),
            pos(0, 5),
            Encoding::Utf16
        )
        .is_some()
    );
}

#[test]
fn unknown_uri_returns_empty() {
    let (dir, _, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    assert!(
        handlers::document_symbols(&snapshot, &sources, "file:///missing.py", Encoding::Utf16)
            .is_empty()
    );
    assert!(
        handlers::hover_at(
            &snapshot,
            &sources,
            "file:///missing.py",
            pos(0, 0),
            Encoding::Utf16
        )
        .is_none()
    );
    assert!(
        handlers::definition_at(
            &snapshot,
            &sources,
            "file:///missing.py",
            pos(0, 0),
            Encoding::Utf16
        )
        .is_none()
    );
}

#[test]
fn ranges_follow_negotiated_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("c.ts");
    std::fs::write(&ts, EMOJI_TS).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&ts);

    let utf8 = handlers::document_symbols(&snapshot, &sources, uri.as_str(), Encoding::Utf8);
    let utf16 = handlers::document_symbols(&snapshot, &sources, uri.as_str(), Encoding::Utf16);
    let add_utf8 = utf8
        .iter()
        .find(|symbol| symbol.name == "add")
        .expect("add symbol");
    let add_utf16 = utf16
        .iter()
        .find(|symbol| symbol.name == "add")
        .expect("add symbol");

    // "const snake = \"" is 15 bytes, the emoji is 4 bytes and 2 UTF-16 units.
    assert_eq!(add_utf8.location.range.start.character, 22);
    assert_eq!(add_utf16.location.range.start.character, 20);

    // Incoming positions use the same encoding.
    assert!(
        handlers::hover_at(
            &snapshot,
            &sources,
            uri.as_str(),
            pos(0, 20),
            Encoding::Utf16
        )
        .is_some()
    );
    assert!(
        handlers::hover_at(
            &snapshot,
            &sources,
            uri.as_str(),
            pos(0, 22),
            Encoding::Utf8
        )
        .is_some()
    );
}
