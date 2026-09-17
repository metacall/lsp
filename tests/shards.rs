//! Cold start from `.meta-ast` shards. The engine's loader verifies the header,
//! manifest and content hashes: a refused record costs only its own file, and a
//! hard error rejects the whole cache.

#![expect(clippy::unwrap_used, reason = "a test may abort on setup failure")]
use std::path::PathBuf;

use meta_ast::model::IdGenerator;
use meta_ast::{IndexLoadOptions, Overlay, SHARD_SCHEMA_VERSION, ShardHeader};
use meta_call_lsp::convert;
use meta_call_lsp::index::{Persistence, Reindexer};

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

fn write(dir: &std::path::Path) -> Reindexer {
    let mut writer = Reindexer::with_persistence(Persistence::Enabled);
    writer.rebuild(dir, &[]).unwrap();
    writer
}

fn bucket_of(dir: &std::path::Path) -> PathBuf {
    let manifest = std::fs::read_to_string(dir.join(".meta-ast/manifest.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
    dir.join(".meta-ast")
        .join(record["shard"].as_str().unwrap())
}

fn rewrite_manifest(dir: &std::path::Path, edit: impl Fn(&mut serde_json::Value)) -> PathBuf {
    let path = dir.join(".meta-ast/manifest.jsonl");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let mut records: Vec<serde_json::Value> = manifest
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for record in &mut records {
        edit(record);
    }
    let mut out = String::new();
    for record in &records {
        out.push_str(&serde_json::to_string(record).unwrap());
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
    path
}

fn rewrite_record(dir: &std::path::Path, file_name: &str, edit: impl Fn(&mut serde_json::Value)) {
    rewrite_manifest(dir, |record| {
        if record["path"]
            .as_str()
            .is_some_and(|path| path.ends_with(file_name))
        {
            edit(record);
        }
    });
}

fn edit_header(dir: &std::path::Path, edit: impl Fn(&mut serde_json::Value)) {
    let path = dir.join(".meta-ast/header.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    edit(&mut value);
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
}

#[test]
fn cold_start_reuses_matching_records() {
    let (dir, _file) = workspace("def greet(): pass\n");
    let writer = write(dir.path());
    assert!(dir.path().join(".meta-ast/header.json").is_file());
    assert!(dir.path().join(".meta-ast/manifest.jsonl").is_file());
    let shards = dir.path().join(".meta-ast/shards");
    assert_eq!(std::fs::read_dir(&shards).unwrap().count(), 1);
    drop(writer);

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.reused, 1);
    assert_eq!(stats.rejected, None);
    assert!(stats.skipped.is_empty(), "a clean cache skips nothing");

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
}

#[test]
fn a_missing_cache_is_not_a_rejection() {
    let (dir, _file) = workspace("def greet(): pass\n");

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());

    assert_eq!(stats.reused, 0);
    assert_eq!(
        stats.rejected, None,
        "a fresh workspace has no cache to reject"
    );
}

#[test]
fn a_corrupt_bucket_costs_only_its_file() {
    let (dir, _file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    let bucket = bucket_of(dir.path());
    let mut corrupted = std::fs::read_to_string(&bucket).unwrap();
    corrupted.push_str("{not json}\n");
    std::fs::write(&bucket, corrupted).unwrap();

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());

    assert_eq!(stats.reused, 0);
    assert_eq!(
        stats.rejected, None,
        "an unreadable bucket costs its own records, not the whole cache"
    );
    assert_eq!(stats.skipped.len(), 1, "the record names its own file");
    assert!(
        !stats.skipped[0].reason.is_empty(),
        "a skipped record carries the reason"
    );

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
}

#[test]
fn manifest_paths_are_relative_to_the_root() {
    let (dir, _file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    let manifest = std::fs::read_to_string(dir.path().join(".meta-ast/manifest.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
    let path = record["path"].as_str().unwrap();
    assert!(
        !path.starts_with('/'),
        "manifest paths must be portable across machines: {path}"
    );
    assert!(path.ends_with("a.py"), "the record names its file: {path}");
}

#[test]
fn a_stale_record_costs_only_its_file() {
    let (dir, _file) = workspace("def one(): pass\n");
    std::fs::write(dir.path().join("b.py"), "def two(): pass\n").unwrap();
    drop(write(dir.path()));

    rewrite_record(dir.path(), "a.py", |record| {
        record["content_hash"] = serde_json::json!("00");
    });

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());

    assert_eq!(stats.reused, 1, "the intact record is still reused");
    assert_eq!(stats.rejected, None);
    assert_eq!(stats.skipped.len(), 1);
    assert!(
        stats.skipped[0].path.ends_with("a.py"),
        "the stale record names its own file"
    );

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    let names = symbol_names(&snapshot);
    assert!(names.contains(&"one".to_string()), "re-extracted from disk");
    assert!(names.contains(&"two".to_string()));
}

#[test]
fn a_safe_bucket_name_is_accepted_without_derivation() {
    let (dir, _file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    // The engine's loader keys a record by the manifest, so the bucket name only
    // has to be a safe name under shards/.
    let bucket = bucket_of(dir.path());
    let target = if bucket.ends_with("shards/000.jsonl") {
        "shards/001.jsonl"
    } else {
        "shards/000.jsonl"
    };
    std::fs::rename(&bucket, dir.path().join(".meta-ast").join(target)).unwrap();
    rewrite_manifest(dir.path(), |record| {
        record["shard"] = serde_json::json!(target);
    });

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());

    assert_eq!(stats.reused, 1);
    assert_eq!(stats.rejected, None);
    assert!(stats.skipped.is_empty());
}

#[test]
fn an_unsafe_shard_name_is_a_rejection() {
    let (dir, _file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    rewrite_manifest(dir.path(), |record| {
        record["shard"] = serde_json::json!("../evil.jsonl");
    });

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());

    assert_eq!(stats.reused, 0);
    assert!(
        stats.rejected.is_some(),
        "a shard name outside shards/ rejects the cache"
    );
    assert!(stats.skipped.is_empty(), "a hard error skips no record");

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
}

#[test]
fn a_foreign_header_field_is_a_rejection() {
    for (field, value) in [
        ("schema_version", serde_json::json!(999)),
        ("tool_version", serde_json::json!("0.0.0")),
    ] {
        let (dir, file) = workspace("def greet(): pass\n");
        drop(write(dir.path()));

        edit_header(dir.path(), |header| header[field] = value.clone());

        let mut reader = Reindexer::with_persistence(Persistence::Enabled);
        let stats = reader.seed_from_shards(dir.path());

        assert_eq!(stats.reused, 0, "a {field} mismatch must not reuse records");
        assert!(
            stats.rejected.is_some(),
            "a {field} mismatch rejects the cache"
        );

        let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
        assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
        assert!(file.is_file());
    }
}

#[test]
fn a_missing_payload_costs_only_its_record() {
    let (dir, _file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    std::fs::remove_file(bucket_of(dir.path())).unwrap();

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());

    assert_eq!(stats.reused, 0);
    assert_eq!(stats.rejected, None);
    assert_eq!(stats.skipped.len(), 1);

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    assert!(symbol_names(&snapshot).contains(&"greet".to_string()));
}

#[test]
fn stale_manifest_over_fresh_bucket_costs_only_its_record() {
    let (dir, file) = workspace("def old(): pass\n");
    let writer = write(dir.path());
    let stale_manifest = std::fs::read(dir.path().join(".meta-ast/manifest.jsonl")).unwrap();
    let stale_bucket = std::fs::read(bucket_of(dir.path())).unwrap();
    drop(writer);

    std::fs::write(&file, "def new(): pass\n").unwrap();
    drop(write(dir.path()));

    // A crash between the shard rename and the manifest rename: the size or hash
    // check must refuse the payload.
    std::fs::write(dir.path().join(".meta-ast/manifest.jsonl"), stale_manifest).unwrap();
    std::fs::write(bucket_of(dir.path()), stale_bucket).unwrap();

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.reused, 0, "stale record must not enter the cache");
    assert_eq!(stats.rejected, None);
    assert_eq!(stats.skipped.len(), 1);

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    let names = symbol_names(&snapshot);
    assert!(names.contains(&"new".to_string()));
    assert!(!names.contains(&"old".to_string()));
}

#[test]
fn unreferenced_shard_files_are_pruned() {
    let (dir, _file) = workspace("def greet(): pass\n");
    let mut writer = write(dir.path());
    std::fs::write(dir.path().join(".meta-ast/shards/999.jsonl"), "junk\n").unwrap();

    std::fs::write(dir.path().join("b.py"), "def other(): pass\n").unwrap();
    writer.rebuild(dir.path(), &[]).unwrap();
    drop(writer);

    assert!(!dir.path().join(".meta-ast/shards/999.jsonl").exists());
}

#[test]
fn second_save_without_changes_is_idempotent() {
    let (dir, _file) = workspace("def greet(): pass\n");
    let mut writer = write(dir.path());
    let first = std::fs::read(dir.path().join(".meta-ast/manifest.jsonl")).unwrap();
    writer.rebuild(dir.path(), &[]).unwrap();
    let second = std::fs::read(dir.path().join(".meta-ast/manifest.jsonl")).unwrap();
    drop(writer);
    assert_eq!(first, second, "unchanged files keep their records");
}

#[test]
fn changed_disk_file_is_not_reused() {
    let (dir, file) = workspace("def disk(): pass\n");
    drop(write(dir.path()));
    std::fs::write(&file, "def changed(): pass\n").unwrap();

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.reused, 0);
    assert_eq!(stats.rejected, None);
    assert_eq!(
        stats.skipped.len(),
        1,
        "the changed file's record is refused"
    );

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
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
    let mut writer = Reindexer::with_persistence(Persistence::Enabled);
    let snapshot = writer
        .rebuild(dir.path(), std::slice::from_ref(&overlay))
        .unwrap();
    assert!(symbol_names(&snapshot).contains(&"buffer".to_string()));
    drop(writer);

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    let stats = reader.seed_from_shards(dir.path());
    assert_eq!(stats.reused, 0);
    assert_eq!(stats.rejected, None);

    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();
    let names = symbol_names(&snapshot);
    assert!(names.contains(&"disk".to_string()));
    assert!(!names.contains(&"buffer".to_string()));
}

#[test]
fn seeded_ids_do_not_collide_with_new_files() {
    let (dir, _file) = workspace("def one(): pass\n");
    drop(write(dir.path()));
    std::fs::write(dir.path().join("b.py"), "def two(): pass\n").unwrap();

    let mut reader = Reindexer::with_persistence(Persistence::Enabled);
    assert_eq!(reader.seed_from_shards(dir.path()).reused, 1);
    let snapshot = reader.rebuild(dir.path(), &[]).unwrap();

    let mut ids: Vec<u32> = snapshot
        .symbols()
        .map(|symbol| symbol.id.to_raw())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 2, "seeded and new symbols must have unique IDs");
}

#[test]
fn a_seeded_record_skips_extraction() {
    let (dir, file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    // The seeded fingerprint has to describe the bytes on disk, or the pass
    // re-extracts instead of reusing the record.
    let mut watch = meta_ast::WatchState::new();
    let stats = meta_call_lsp::shards::load(dir.path(), &mut watch);
    assert_eq!(stats.reused, 1);
    let bytes = std::fs::read(&file).unwrap();
    assert_eq!(
        watch.cache().fingerprint_of(&file),
        Some(meta_ast::Fingerprint::of(&bytes)),
        "the seeded fingerprint must match the bytes it was verified against"
    );

    let (extractions, change, _) =
        meta_ast::reanalyze_extractions(dir.path(), None, &[], &mut watch).unwrap();
    assert_eq!(extractions.len(), 1);
    assert_eq!(change.files_added, 0);
    assert_eq!(change.files_modified, 0, "the seeded file is not re-parsed");
    assert_eq!(change.files_unchanged, 1);
}

#[test]
fn the_written_header_names_the_engine_build() {
    let (dir, _file) = workspace("def greet(): pass\n");
    drop(write(dir.path()));

    let header: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".meta-ast/header.json")).unwrap(),
    )
    .unwrap();
    let engine = ShardHeader::new("");
    assert_eq!(
        header["tool_version"].as_str(),
        Some(engine.tool_version.as_str()),
        "the loader compares the header against the engine's own version"
    );
    assert_eq!(
        header["schema_version"].as_u64(),
        Some(u64::from(SHARD_SCHEMA_VERSION))
    );

    let loaded = meta_ast::load_index(
        dir.path(),
        &IdGenerator::with_start(1),
        &IndexLoadOptions::default(),
    )
    .unwrap();
    assert_eq!(loaded.stats.loaded, 1);
    assert_eq!(loaded.stats.skipped, 0);
}
