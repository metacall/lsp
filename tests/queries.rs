use std::borrow::Cow;
use std::cell::Cell;
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
    reads: Cell<usize>,
}

impl TestSources {
    fn disk() -> Self {
        Self {
            overlay: HashMap::new(),
            reads: Cell::new(0),
        }
    }

    fn from_buffers(buffers: &BufferStore) -> Self {
        let mut overlay = HashMap::new();
        for (uri, doc) in buffers.iter() {
            if let Some(path) = convert::uri_to_path(uri) {
                overlay.insert(path, doc.text.clone());
            }
        }
        Self {
            overlay,
            reads: Cell::new(0),
        }
    }
}

impl SourceText for TestSources {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        self.reads.set(self.reads.get() + 1);
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

const UTIL: &str = "def helper(value):\n    return value\n";
const APP_IMPORT: &str = "from util import helper\n\n\ndef run():\n    return helper(1)\n";

#[test]
fn cross_file_definition_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let util = dir.path().join("util.py");
    let app = dir.path().join("app.py");
    std::fs::write(&util, UTIL).unwrap();
    std::fs::write(&app, APP_IMPORT).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    // The call in app.py resolves to the definition in util.py.
    let location = handlers::definition_at(
        &snapshot,
        &sources,
        uri_of(&app).as_str(),
        pos(4, 13),
        Encoding::Utf16,
    )
    .expect("definition");
    assert_eq!(location.uri.as_str(), uri_of(&util).as_str());
    assert_eq!(location.range.start.line, 0);

    // The per-request cache must read each distinct file once.
    sources.reads.set(0);
    // References from the definition include the declaration and the call.
    let references = handlers::references_at(
        &snapshot,
        &sources,
        uri_of(&util).as_str(),
        pos(0, 6),
        Encoding::Utf16,
        true,
    );
    assert!(
        references
            .iter()
            .any(|location| location.uri.as_str() == uri_of(&app).as_str())
    );
    assert!(
        references
            .iter()
            .any(|location| location.uri.as_str() == uri_of(&util).as_str())
    );
    assert_eq!(sources.reads.get(), 2);
}

#[test]
fn repeated_references_collapse_to_one_edge() {
    let dir = tempfile::tempdir().unwrap();
    let util = dir.path().join("util.py");
    let app = dir.path().join("app.py");
    std::fs::write(&util, "def helper(value):\n    return value\n").unwrap();
    std::fs::write(
        &app,
        "from util import helper\n\n\ndef run():\n    a = helper(1)\n    b = helper(2)\n    return a + b\n",
    )
    .unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let helper = snapshot
        .symbols()
        .find(|symbol| symbol.name == "helper")
        .expect("helper");
    let run = snapshot
        .symbols()
        .find(|symbol| symbol.name == "run")
        .expect("run");

    // Two use sites from one caller collapse to a single edge. The graph
    // max-merges confidence for the same (source, target) pair.
    let callers: Vec<_> = snapshot
        .references_in(helper.id)
        .iter()
        .filter(|(id, _)| *id == run.id)
        .collect();
    assert_eq!(callers.len(), 1, "two use sites must collapse to one edge");
    assert_eq!(callers[0].1, 1.0);

    let references = handlers::references_at(
        &snapshot,
        &sources,
        uri_of(&util).as_str(),
        pos(0, 6),
        Encoding::Utf16,
        true,
    );
    assert_eq!(
        references.len(),
        2,
        "declaration plus one collapsed use site"
    );
}

#[test]
fn workspace_symbols_read_each_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let alpha_beta = dir.path().join("alpha_beta.py");
    let gamma = dir.path().join("gamma.py");
    std::fs::write(&alpha_beta, "def alpha(): pass\n\n\ndef beta(): pass\n").unwrap();
    std::fs::write(&gamma, "def gamma(): pass\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let symbols = handlers::workspace_symbols(&snapshot, &sources, "", Encoding::Utf16);
    let mut names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["alpha", "beta", "gamma"]);
    assert_eq!(sources.reads.get(), 2);
}

#[test]
fn completion_matches_case_insensitively_when_exact_matches_are_absent() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("cased.py");
    std::fs::write(
        &app,
        "def GREET(): pass\n\n\ndef use_greeting():\n    selection = gre\n",
    )
    .unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let items = handlers::completion_at(
        &snapshot,
        &sources,
        uri_of(&app).as_str(),
        pos(4, 15),
        Encoding::Utf16,
    );
    let item = items
        .iter()
        .find(|item| item.label == "GREET")
        .expect("case-insensitive completion");
    assert_eq!(item.sort_text.as_deref(), Some("0GREET"));
}

#[test]
fn workspace_symbols_filter_by_query() {
    let (dir, _, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let all = handlers::workspace_symbols(&snapshot, &sources, "", Encoding::Utf16);
    assert!(all.iter().any(|symbol| symbol.name == "greet"));
    assert!(all.iter().any(|symbol| symbol.name == "add"));

    let filtered = handlers::workspace_symbols(&snapshot, &sources, "gre", Encoding::Utf16);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "greet");
}

#[test]
fn completion_lists_visible_symbols() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    // Cursor after `def gre` on line 0: prefix "gre".
    let items = handlers::completion_at(
        &snapshot,
        &sources,
        uri_of(&app).as_str(),
        pos(0, 7),
        Encoding::Utf16,
    );
    assert!(items.iter().any(|item| item.label == "greet"));
    assert!(items.iter().all(|item| {
        item.sort_text
            .as_deref()
            .is_some_and(|sort| sort.starts_with(['0', '1', '2']))
    }));
}

const MIXED_JS: &str = "'use strict';\n\nfunction multiply(a, b) {\n\treturn a * b;\n}\n\nmodule.exports = { multiply };\n";
const MIXED_PY: &str = "from metacall import metacall, metacall_load_from_file\n\nmetacall_load_from_file(\"node\", [\"math.js\"])\n\n\ndef compute_total(units, price):\n    return metacall(\"multiply\", units, price)\n";

#[test]
fn metacall_cross_language_definition_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let js = dir.path().join("math.js");
    let py = dir.path().join("orchestrator.py");
    std::fs::write(&js, MIXED_JS).unwrap();
    std::fs::write(&py, MIXED_PY).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    // The metacall("multiply", ...) call resolves to the JavaScript function.
    let location = handlers::definition_at(
        &snapshot,
        &sources,
        uri_of(&py).as_str(),
        pos(6, 25),
        Encoding::Utf16,
    )
    .expect("definition");
    assert_eq!(location.uri.as_str(), uri_of(&js).as_str());
    assert_eq!(location.range.start.line, 2);

    // References from the definition include the orchestrator call site.
    let references = handlers::references_at(
        &snapshot,
        &sources,
        uri_of(&js).as_str(),
        pos(2, 12),
        Encoding::Utf16,
        true,
    );
    assert!(
        references
            .iter()
            .any(|location| location.uri.as_str() == uri_of(&py).as_str())
    );

    // Completion inside the metacall("multiply", ...) call offers the target.
    // The cursor sits after "mu", so the prefix is "mu".
    let items = handlers::completion_at(
        &snapshot,
        &sources,
        uri_of(&py).as_str(),
        pos(6, 23),
        Encoding::Utf16,
    );
    assert!(items.iter().any(|item| item.label == "multiply"));
}
