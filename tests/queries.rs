#![expect(clippy::unwrap_used, reason = "a test may abort on setup failure")]
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::Position;
use meta_call_lsp::buffers::{BufferStore, OpenOutcome};
use meta_call_lsp::handlers::QueryCtx;
use meta_call_lsp::index::SourceText;
use meta_call_lsp::position::Encoding;
use meta_call_lsp::types::{DocUri, DocVersion};
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
            if let Some(path) = uri.to_path() {
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

fn doc_uri(value: &str) -> DocUri {
    DocUri::try_from(value).expect("document URI")
}

fn uri_of(path: &Path) -> DocUri {
    DocUri::try_from(convert::path_to_uri(path).unwrap().as_str()).expect("document URI")
}

fn snapshot(dir: &Path, buffers: &BufferStore) -> std::sync::Arc<index::IndexSnapshot> {
    index::rebuild_from_inputs(dir, &index::collect_inputs(dir, buffers)).unwrap()
}

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn definition_locations(
    snapshot: &std::sync::Arc<index::IndexSnapshot>,
    sources: &TestSources,
    uri: &DocUri,
    position: Position,
) -> Vec<(String, lsp_types::Range)> {
    match handlers::definition_at(
        &mut QueryCtx::new(snapshot, sources, Encoding::Utf16),
        uri,
        position,
        false,
    ) {
        Some(lsp_types::GotoDefinitionResponse::Array(locations)) => locations
            .into_iter()
            .map(|location| (location.uri.as_str().to_string(), location.range))
            .collect(),
        Some(other) => panic!("expected plain locations, got {other:?}"),
        None => Vec::new(),
    }
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
    let symbols = handlers::document_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&app),
    );
    let greet = symbols
        .iter()
        .find(|symbol| symbol.name == "greet")
        .expect("greet symbol");
    assert_eq!(
        greet.selection_range,
        lsp_types::Range {
            start: pos(0, 4),
            end: pos(0, 9),
        },
        "selectionRange names the identifier, not the declaration"
    );
    assert_eq!(
        greet.range.start,
        pos(0, 0),
        "range still covers the whole declaration"
    );
}

// A definition link selects the identifier inside the target, not the declaration.
#[test]
fn definition_links_select_the_identifier() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&app);

    let response = handlers::definition_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        pos(5, 10),
        true,
    );

    let Some(lsp_types::GotoDefinitionResponse::Link(links)) = response else {
        panic!("link support yields location links");
    };
    assert_eq!(links.len(), 1);
    let link = &links[0];
    assert_eq!(link.target_selection_range.start, pos(0, 4));
    assert_eq!(link.target_selection_range.end, pos(0, 9));
    assert_eq!(link.target_range.start, pos(0, 0));
    assert!(
        link.target_range.end.line > link.target_selection_range.end.line,
        "the target range covers the declaration, the selection only the name: {link:?}"
    );
}

#[test]
fn hover_shows_signature() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let hover = handlers::hover_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&app),
        pos(0, 5),
    )
    .unwrap();
    let lsp_types::HoverContents::Markup(content) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(content.value.contains("greet"));
    assert!(
        content.value.contains("(function)"),
        "the kind renders as a fixed word, not engine debug text: {}",
        content.value
    );
}

#[test]
fn definition_resolves_reference_to_def() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&app);
    let locations = definition_locations(&snapshot, &sources, &uri, pos(5, 10));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].0, uri.as_str());
    assert_eq!(locations[0].1.start.line, 0);
}

#[test]
fn buffer_override_adds_symbol() {
    let (dir, app, _) = workspace();
    let uri = uri_of(&app);
    let mut buffers = BufferStore::default();
    assert_eq!(
        buffers.open(
            &uri,
            DocVersion::from(2),
            "python",
            format!("{APP}\n\ndef extra(): pass\n")
        ),
        OpenOutcome::Indexed
    );
    let snapshot = snapshot(dir.path(), &buffers);
    let sources = TestSources::from_buffers(&buffers);
    let symbols = handlers::document_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
    );
    assert!(symbols.iter().any(|symbol| symbol.name == "extra"));
    assert!(
        handlers::hover_at(
            &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
            &uri,
            pos(0, 5)
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
        handlers::document_symbols(
            &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
            &doc_uri("file:///missing.py")
        )
        .is_empty()
    );
    assert!(
        handlers::hover_at(
            &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
            &doc_uri("file:///missing.py"),
            pos(0, 0)
        )
        .is_none()
    );
    assert!(
        definition_locations(
            &snapshot,
            &sources,
            &doc_uri("file:///missing.py"),
            pos(0, 0)
        )
        .is_empty()
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

    let utf8 = handlers::document_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf8),
        &uri,
    );
    let utf16 = handlers::document_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
    );
    let add_utf8 = utf8
        .iter()
        .find(|symbol| symbol.name == "add")
        .expect("add symbol");
    let add_utf16 = utf16
        .iter()
        .find(|symbol| symbol.name == "add")
        .expect("add symbol");

    assert_eq!(add_utf8.range.start.character, 22);
    assert_eq!(add_utf16.range.start.character, 20);

    assert!(
        handlers::hover_at(
            &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
            &uri,
            pos(0, 20)
        )
        .is_some()
    );
    assert!(
        handlers::hover_at(
            &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf8),
            &uri,
            pos(0, 22)
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

    let locations = definition_locations(&snapshot, &sources, &uri_of(&app), pos(4, 13));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].0, uri_of(&util).as_str());
    assert_eq!(locations[0].1.start.line, 0);

    // The per-request cache must read each distinct file once.
    sources.reads.set(0);
    let references = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&util),
        pos(0, 6),
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

    // Two use sites from one caller collapse into one edge, with max confidence.
    let targets: Vec<_> = snapshot
        .references_out(run.id)
        .iter()
        .filter(|(id, _)| *id == helper.id)
        .collect();
    assert_eq!(targets.len(), 1, "two use sites must collapse to one edge");
    assert_eq!(targets[0].1, 1.0);

    let references = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&util),
        pos(0, 6),
        true,
    );
    assert_eq!(
        references.len(),
        3,
        "declaration plus one location per use site"
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

    let symbols =
        handlers::workspace_symbols(&mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16), "");
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
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&app),
        pos(4, 15),
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

    let all =
        handlers::workspace_symbols(&mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16), "");
    assert!(all.iter().any(|symbol| symbol.name == "greet"));
    assert!(all.iter().any(|symbol| symbol.name == "add"));

    let filtered = handlers::workspace_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        "gre",
    );
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "greet");
}

#[test]
fn workspace_symbols_match_exactly_then_by_substring() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.py");
    let z = dir.path().join("z.py");
    std::fs::write(&a, "def greeting(): pass\n").unwrap();
    std::fs::write(&z, "def greet(): pass\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let matched = handlers::workspace_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        "greet",
    );
    let names: Vec<&str> = matched.iter().map(|symbol| symbol.name.as_str()).collect();
    assert_eq!(
        names,
        ["greet", "greeting"],
        "the exact name outranks the substring match even though a.py sorts first"
    );
}

#[test]
fn workspace_symbols_have_no_fuzzy_tier() {
    let (dir, _, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let matched = handlers::workspace_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        "grt",
    );

    assert!(
        matched.is_empty(),
        "a non-contiguous query must not match, got {matched:?}"
    );
}

#[test]
fn workspace_symbols_match_non_ascii_case_insensitively() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("naive.py");
    std::fs::write(&file, "def naïve(): pass\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let matched = handlers::workspace_symbols(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        "NAÏVE",
    );

    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].name, "naïve");
}

#[test]
fn workspace_symbols_empty_query_returns_all_in_documented_order() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.py");
    let b = dir.path().join("b.py");
    std::fs::write(&a, "def zeta(): pass\n\n\ndef alpha(): pass\n").unwrap();
    std::fs::write(&b, "def beta(): pass\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let symbols =
        handlers::workspace_symbols(&mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16), "");

    let order: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
    assert_eq!(
        order,
        ["zeta", "alpha", "beta"],
        "the empty query returns the whole set in (path, byte) order"
    );
    assert!(
        symbols.iter().all(|symbol| symbol.container_name.is_none()),
        "the nested shape omits the inferred container name"
    );
    let lsp_types::OneOf::Left(location) = &symbols[0].location else {
        panic!("a workspace symbol carries a full location");
    };
    assert_eq!(location.uri.as_str(), uri_of(&a).as_str());
}

#[test]
fn completion_lists_visible_symbols() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    // Cursor after `def gre` on line 0: prefix "gre".
    let items = handlers::completion_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&app),
        pos(0, 7),
    );
    assert!(items.iter().any(|item| item.label == "greet"));
    assert!(items.iter().all(|item| {
        item.sort_text
            .as_deref()
            .is_some_and(|sort| sort.starts_with(['0', '1', '2', '3']))
    }));
}

const MIXED_JS: &str = "'use strict';\n\nfunction multiply(a, b) {\n\treturn a * b;\n}\n\nmodule.exports = { multiply };\n";
const MIXED_PY: &str = "from metacall import metacall, metacall_load_from_file\n\nmetacall_load_from_file(\"node\", [\"math.js\"])\n\n\ndef compute_total(units, price):\n    return metacall(\"multiply\", units, price)\n";

#[test]
fn references_point_at_each_call_site() {
    let dir = tempfile::tempdir().unwrap();
    let util = dir.path().join("util.py");
    let app = dir.path().join("app.py");
    std::fs::write(&util, UTIL).unwrap();
    std::fs::write(
        &app,
        "from util import helper\n\n\ndef run():\n    a = helper(1)\n    b = helper(2)\n    return a + b\n",
    )
    .unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let references = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&util),
        pos(0, 6),
        false,
    );
    let mut app_lines: Vec<u32> = references
        .iter()
        .filter(|location| location.uri.as_str() == uri_of(&app).as_str())
        .map(|location| location.range.start.line)
        .collect();
    app_lines.sort_unstable();
    assert_eq!(
        app_lines,
        vec![4, 5],
        "one precise location per call site, on the calling lines"
    );
}

#[test]
fn metacall_cross_language_definition_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let js = dir.path().join("math.js");
    let py = dir.path().join("orchestrator.py");
    std::fs::write(&js, MIXED_JS).unwrap();
    std::fs::write(&py, MIXED_PY).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let locations = definition_locations(&snapshot, &sources, &uri_of(&py), pos(6, 25));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].0, uri_of(&js).as_str());
    assert_eq!(locations[0].1.start.line, 2);

    let references = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&js),
        pos(2, 12),
        true,
    );
    assert!(
        references
            .iter()
            .any(|location| location.uri.as_str() == uri_of(&py).as_str())
    );

    // The cursor sits after "mu", so completion offers the cross-language target.
    let items = handlers::completion_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&py),
        pos(6, 23),
    );
    assert!(items.iter().any(|item| item.label == "multiply"));
}

/// The second client-call resolution must not report a diagnostic twice.
#[test]
fn a_metacall_workspace_reports_no_duplicate_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("math.js"), MIXED_JS).unwrap();
    std::fs::write(dir.path().join("orchestrator.py"), MIXED_PY).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());

    let mut seen = std::collections::HashSet::new();
    for diagnostic in &snapshot.diagnostics {
        let key = (
            diagnostic.path.clone(),
            diagnostic.message.clone(),
            diagnostic
                .source_range
                .as_ref()
                .map(|range| (range.byte_start, range.byte_end)),
        );
        assert!(
            seen.insert(key),
            "each diagnostic appears once: {diagnostic:?}"
        );
    }
}

/// Expanding every record would index one ambiguous call site once per target squared.
#[test]
fn an_ambiguous_client_call_indexes_each_use_site_once() {
    let dir = tempfile::tempdir().unwrap();
    let js = MIXED_JS;
    std::fs::write(dir.path().join("helpers.js"), js).unwrap();
    std::fs::write(dir.path().join("utils.js"), js).unwrap();
    std::fs::write(
        dir.path().join("orchestrator.py"),
        "from metacall import metacall\n\n\ndef compute_total(units, price):\n    return metacall(\"multiply\", units, price)\n",
    )
    .unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());

    let targets: Vec<meta_ast::model::SymbolId> = snapshot
        .client_calls
        .iter()
        .map(|call| call.target)
        .collect();
    assert_eq!(
        targets.len(),
        2,
        "one call site with two global candidates records two entries"
    );
    for target in targets {
        let sites = snapshot.occurrences_of(target);
        assert_eq!(
            sites.len(),
            1,
            "the call site is one use site per target, got {sites:?}"
        );
    }
}

const TRANSITIVE_UTIL: &str = "def helper(value):\n    return value\n";
const TRANSITIVE_MID: &str = "from util import helper\n\n\ndef wrapper():\n    return helper(1)\n";
const TRANSITIVE_FAR: &str = "from mid import wrapper\n\n\ndef run():\n    return helper(2)\n";

#[test]
fn definition_links_carry_the_origin_range() {
    let (dir, app, _) = workspace();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&app);

    let response = handlers::definition_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        pos(5, 10),
        true,
    );

    let Some(lsp_types::GotoDefinitionResponse::Link(links)) = response else {
        panic!("expected definition links");
    };
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target_uri.as_str(), uri.as_str());
    assert!(
        links[0].origin_selection_range.is_some(),
        "a reference target carries the origin range"
    );
}

#[test]
fn definition_returns_every_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.py");
    let b = dir.path().join("b.py");
    let c = dir.path().join("c.py");
    std::fs::write(&a, "def dup(): pass\n").unwrap();
    std::fs::write(&b, "def dup(): pass\n").unwrap();
    std::fs::write(&c, "from a import dup\nfrom b import dup\n\n\nx = dup()\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let locations = definition_locations(&snapshot, &sources, &uri_of(&c), pos(4, 5));

    assert_eq!(
        locations.len(),
        2,
        "every candidate the engine resolved is returned, got {locations:?}"
    );
    let uris: Vec<&str> = locations.iter().map(|(uri, _)| uri.as_str()).collect();
    assert_eq!(
        uris,
        [uri_of(&a).as_str(), uri_of(&b).as_str()],
        "targets order by declaration site, not by score"
    );
}

/// A shadowing definition prunes the imported candidate; the server returns that set.
#[test]
fn a_shadowing_definition_prunes_the_imported_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.py");
    let b = dir.path().join("b.py");
    let c = dir.path().join("c.py");
    std::fs::write(&a, "def dup(): pass\n").unwrap();
    std::fs::write(&b, "from a import dup\n\n\ndef dup(): pass\n").unwrap();
    std::fs::write(&c, "from b import dup\n\n\nx = dup()\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let locations = definition_locations(&snapshot, &sources, &uri_of(&c), pos(3, 5));

    assert_eq!(
        locations.len(),
        1,
        "the shadowed import is not a candidate, got {locations:?}"
    );
    assert_eq!(locations[0].0, uri_of(&b).as_str());
    assert_eq!(
        locations[0].1.start.line, 3,
        "the target is b.py's own definition"
    );
}

#[test]
fn references_order_by_path_then_byte() {
    let dir = tempfile::tempdir().unwrap();
    let util = dir.path().join("util.py");
    let mid = dir.path().join("mid.py");
    let far = dir.path().join("far.py");
    std::fs::write(&util, TRANSITIVE_UTIL).unwrap();
    std::fs::write(&mid, TRANSITIVE_MID).unwrap();
    std::fs::write(&far, TRANSITIVE_FAR).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();

    let helper_from_util = snapshot
        .symbols()
        .find(|symbol| symbol.name == "helper" && symbol.file_path == util)
        .expect("helper");
    assert_eq!(
        snapshot.occurrences_of(helper_from_util.id).len(),
        2,
        "both the direct and the transitive use site are indexed"
    );

    // The declaration comes first, then use sites in path order: confidence never orders.
    let references = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri_of(&util),
        pos(0, 6),
        true,
    );
    let order: Vec<(&str, u32)> = references
        .iter()
        .map(|location| (location.uri.as_str(), location.range.start.line))
        .collect();
    assert_eq!(
        order,
        [
            (uri_of(&util).as_str(), 0),
            (uri_of(&far).as_str(), 4),
            (uri_of(&mid).as_str(), 4),
        ]
    );
}

#[test]
fn definition_shapes_carry_the_same_target_set() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.py");
    let b = dir.path().join("b.py");
    let c = dir.path().join("c.py");
    std::fs::write(&a, "def dup(): pass\n").unwrap();
    std::fs::write(&b, "def dup(): pass\n").unwrap();
    std::fs::write(&c, "from a import dup\nfrom b import dup\n\n\nx = dup()\n").unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&c);

    let plain = definition_locations(&snapshot, &sources, &uri, pos(4, 5));
    assert_eq!(
        plain.len(),
        2,
        "every candidate is returned as a plain location: {plain:?}"
    );

    let response = handlers::definition_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        pos(4, 5),
        true,
    );
    let Some(lsp_types::GotoDefinitionResponse::Link(links)) = response else {
        panic!("link support yields location links");
    };
    let linked: Vec<(String, lsp_types::Range)> = links
        .into_iter()
        .map(|link| (link.target_uri.as_str().to_string(), link.target_range))
        .collect();
    assert_eq!(
        linked, plain,
        "link support changes the shape, not the targets"
    );
}

#[test]
fn references_put_the_declaration_first_only_when_requested() {
    let dir = tempfile::tempdir().unwrap();
    let util = dir.path().join("util.py");
    let app = dir.path().join("app.py");
    std::fs::write(&util, UTIL).unwrap();
    std::fs::write(&app, APP_IMPORT).unwrap();
    let snapshot = snapshot(dir.path(), &BufferStore::default());
    let sources = TestSources::disk();
    let uri = uri_of(&util);

    let with_declaration = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        pos(0, 6),
        true,
    );
    let without_declaration = handlers::references_at(
        &mut QueryCtx::new(&snapshot, &sources, Encoding::Utf16),
        &uri,
        pos(0, 6),
        false,
    );
    let order = |locations: &[lsp_types::Location]| -> Vec<(String, u32)> {
        locations
            .iter()
            .map(|location| (location.uri.as_str().to_string(), location.range.start.line))
            .collect()
    };

    assert_eq!(
        order(&with_declaration),
        [
            (uri.as_str().to_string(), 0),
            (uri_of(&app).as_str().to_string(), 4)
        ],
        "the declaration is the first result when requested"
    );
    assert_eq!(
        order(&without_declaration),
        [(uri_of(&app).as_str().to_string(), 4)],
        "only use sites remain when the declaration is not requested"
    );
}
