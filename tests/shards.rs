//! Cold start from `.meta-ast` shards.

use std::path::PathBuf;

use meta_ast::{Overlay, ShardHeader, write_header};
use meta_call_lsp::convert;
use meta_call_lsp::index::Reindexer;

fn workspace(content: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.py");
    std::fs::write(&file, content).unwrap();
    (dir, file)
}

fn symbol_names(snapshot: &meta_call_lsp::index::IndexSnapshot) -> Vec<String> {
    snapshot
        .extractions
        .iter()
        .flat_map(|file| file.symbols.iter().map(|symbol| symbol.name.clone()))
        .collect()
}

#[test]
fn cold_start_reuses_matching_records() {
    let (dir, _file) = workspace("def greet(): pass\n");
    let mut writer = Reindexer::with_persistence();
    writer.rebuild(dir.path(), &[], 1).unwrap();
    assert!(dir.path().join(".meta-ast/header.json").is_file());
    assert!(dir.path().join(".meta-ast/manifest.jsonl").is_file());
    assert!(dir.path().join(".meta-ast/shards/000.jsonl").is_file());
    drop(writer);

    let mut reader = Reindexer::with_persistence();
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.loaded, 1);
    assert_eq!(stats.skipped, 0);
    assert_eq!(reader.cached_len(), 1);

    let snapshot = reader.rebuild(dir.path(), &[], 2).unwrap();
    assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
}

#[test]
fn changed_disk_file_is_not_reused() {
    let (dir, file) = workspace("def disk(): pass\n");
    let mut writer = Reindexer::with_persistence();
    writer.rebuild(dir.path(), &[], 1).unwrap();
    drop(writer);
    std::fs::write(&file, "def changed(): pass\n").unwrap();

    let mut reader = Reindexer::with_persistence();
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.loaded, 0);
    assert_eq!(stats.skipped, 1);

    let snapshot = reader.rebuild(dir.path(), &[], 2).unwrap();
    let names = symbol_names(&snapshot);
    assert!(names.contains(&"changed".to_string()));
    assert!(!names.contains(&"disk".to_string()));
}

#[test]
fn overlay_documents_are_not_persisted() {
    let (dir, file) = workspace("def disk(): pass\n");
    let overlay = Overlay {
        uri: convert::path_to_uri(&file).unwrap().to_string(),
        path: file.clone(),
        text: "def buffer(): pass\n".to_string(),
        version: 2,
        lang: meta_ast::LangId::Python,
    };
    let mut writer = Reindexer::with_persistence();
    let snapshot = writer
        .rebuild(dir.path(), std::slice::from_ref(&overlay), 1)
        .unwrap();
    assert!(symbol_names(&snapshot).contains(&"buffer".to_string()));
    drop(writer);

    let mut reader = Reindexer::with_persistence();
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.loaded, 0);

    let snapshot = reader.rebuild(dir.path(), &[], 2).unwrap();
    let names = symbol_names(&snapshot);
    assert!(names.contains(&"disk".to_string()));
    assert!(!names.contains(&"buffer".to_string()));
}

#[test]
fn stale_tool_version_is_ignored() {
    let (dir, file) = workspace("def greet(): pass\n");
    let mut writer = Reindexer::with_persistence();
    writer.rebuild(dir.path(), &[], 1).unwrap();
    drop(writer);

    let header = ShardHeader::with_tool_version("0.0.0", "2026-01-01T00:00:00Z");
    let mut file_handle = std::fs::File::create(dir.path().join(".meta-ast/header.json")).unwrap();
    write_header(&mut file_handle, &header).unwrap();

    let mut reader = Reindexer::with_persistence();
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats, Default::default());

    let snapshot = reader.rebuild(dir.path(), &[], 2).unwrap();
    assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
    assert!(file.is_file());
}

#[test]
fn seeded_ids_do_not_collide_with_new_files() {
    let (dir, _file) = workspace("def one(): pass\n");
    let mut writer = Reindexer::with_persistence();
    writer.rebuild(dir.path(), &[], 1).unwrap();
    drop(writer);
    std::fs::write(dir.path().join("b.py"), "def two(): pass\n").unwrap();

    let mut reader = Reindexer::with_persistence();
    assert_eq!(reader.seed_from_shards(dir.path()).loaded, 1);
    let snapshot = reader.rebuild(dir.path(), &[], 2).unwrap();

    let mut ids: Vec<u32> = snapshot
        .symbols()
        .map(|symbol| symbol.id.to_raw())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 2, "seeded and new symbols must have unique IDs");
}
