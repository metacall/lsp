//! `.meta-ast` index persistence. Crash-safe write order: buckets, then manifest, then header; the engine loader refuses the whole
//! cache on a hard error, one refused record costs its file; the header version refuses foreign caches; overlays are never persisted.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meta_ast::model::IdGenerator;
use meta_ast::output::shard::ShardSkip;
use meta_ast::{
    FileExtraction, Fingerprint, INDEX_DIR_NAME, IndexLoadOptions, ShardFile, ShardHeader,
    ShardManifestRecord, WatchState, load_index, read_manifest, write_header, write_manifest,
    write_shard,
};

use self::io::{bucket_for, hex, mtime_seconds, timestamp, write_atomic};
use crate::index::IndexSnapshot;

mod io;

const HEADER_FILE: &str = "header.json";
const MANIFEST_FILE: &str = "manifest.jsonl";

/// Cold-start outcome: entries reused, records refused (re-extract next pass), or the cache rejection reason.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CacheLoad {
    pub reused: usize,
    pub skipped: Vec<ShardSkip>,
    pub rejected: Option<String>,
}

/// Write the snapshot to `.meta-ast`, skipping overlays; unchanged files reuse their manifest record.
pub fn save(
    root: &Path,
    snapshot: &IndexSnapshot,
    overlays: &HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let dir = root.join(INDEX_DIR_NAME);
    fs::create_dir_all(dir.join("shards"))?;
    let previous: HashMap<PathBuf, ShardManifestRecord> = File::open(dir.join(MANIFEST_FILE))
        .map(|file| read_manifest(BufReader::new(file)))
        .unwrap_or_else(|_| Ok(Vec::new()))?
        .into_iter()
        .map(|record| (record.path.clone(), record))
        .collect();

    let mut records: Vec<ShardManifestRecord> = Vec::with_capacity(snapshot.extractions.len());
    let mut by_bucket: HashMap<String, Vec<usize>> = HashMap::new();
    let mut touched: HashSet<String> = HashSet::new();
    let mut kept_paths: HashSet<PathBuf> = HashSet::new();

    for (index, file) in snapshot.extractions.iter().enumerate() {
        if overlays.contains(&file.path) || !file.path.starts_with(root) {
            continue;
        }
        // Manifest and payload paths are stored relative to the root, so the
        // cache stays portable across machines and mounts.
        let stored = relative(root, &file.path);
        kept_paths.insert(stored.to_path_buf());
        let indexed = snapshot.content_hash(&file.path);
        let Some(record) = manifest_record(stored, file, indexed, &previous, &mut touched) else {
            continue;
        };
        by_bucket
            .entry(record.shard.clone())
            .or_default()
            .push(index);
        records.push(record);
    }

    // Buckets losing entries must be rewritten without them.
    for (path, record) in &previous {
        if !kept_paths.contains(path) {
            touched.insert(record.shard.clone());
        }
    }

    let mut referenced: HashSet<String> = HashSet::new();
    for (shard, indices) in &by_bucket {
        referenced.insert(shard.clone());
        if !touched.contains(shard) {
            continue;
        }
        let mut shard_files = Vec::with_capacity(indices.len());
        for &index in indices {
            let file = &snapshot.extractions[index];
            match ShardFile::from_extraction(file, &snapshot.graph) {
                Ok(mut shard_file) => {
                    shard_file.path = relative(root, &file.path).to_path_buf();
                    shard_files.push(shard_file);
                }
                Err(error) => {
                    tracing::warn!(path = %file.path.display(), %error, "shard record skipped");
                }
            }
        }
        let mut shard_bytes = Vec::new();
        write_shard(&mut shard_bytes, &shard_files)?;
        write_atomic(&dir.join(shard), &shard_bytes)?;
    }
    for record in &records {
        referenced.insert(record.shard.clone());
    }

    let mut manifest = Vec::new();
    write_manifest(&mut manifest, &records)?;
    let mut header = Vec::new();
    write_header(&mut header, &ShardHeader::new(timestamp()))?;
    write_atomic(&dir.join(MANIFEST_FILE), &manifest)?;
    write_atomic(&dir.join(HEADER_FILE), &header)?;
    prune(&dir.join("shards"), &referenced);
    Ok(())
}

/// Root-relative spelling of one indexed path; every path reaching a record is under the root.
fn relative<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

/// The record hash describes the content the payload came from, so a concurrent edit cannot validate a stale payload.
fn manifest_record(
    stored: &Path,
    file: &meta_ast::FileExtraction,
    indexed: Option<Fingerprint>,
    previous: &HashMap<PathBuf, ShardManifestRecord>,
    touched: &mut HashSet<String>,
) -> Option<ShardManifestRecord> {
    let indexed = indexed?;
    let reused = previous.get(stored).and_then(|prev| {
        (prev.content_hash == hex(indexed.as_bytes())
            && prev.shard == bucket_for(&prev.content_hash))
        .then(|| prev.clone())
    });
    if let Some(record) = reused {
        return Some(record);
    }

    let hash = hex(indexed.as_bytes());
    let shard = bucket_for(&hash);
    touched.insert(shard.clone());
    if let Some(prev) = previous.get(stored)
        && prev.shard != shard
    {
        touched.insert(prev.shard.clone());
    }
    Some(ShardManifestRecord::new(
        stored.to_path_buf(),
        hash,
        fs::metadata(&file.path).ok()?.len(),
        mtime_seconds(&file.path),
        shard,
    ))
}

fn prune(shards_dir: &Path, referenced: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(shards_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let relative = format!("shards/{name}");
        if !referenced.contains(&relative)
            && let Err(error) = fs::remove_file(&path)
        {
            tracing::warn!(path = %path.display(), %error, "shard prune failed");
        }
    }
}

/// Seed the engine cache from `.meta-ast`; a missing cache is not a rejection, a hard error leaves it empty.
pub fn load(root: &Path, watch: &mut WatchState) -> CacheLoad {
    if !root.join(INDEX_DIR_NAME).join(HEADER_FILE).is_file() {
        return CacheLoad::default();
    }
    let loaded = match load_index(
        root,
        &IdGenerator::with_start(1),
        &IndexLoadOptions::default(),
    ) {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::warn!(%error, "shard cache rejected");
            return CacheLoad {
                reused: 0,
                skipped: Vec::new(),
                rejected: Some(error.to_string()),
            };
        }
    };
    let reused = seed_cache(root, watch, loaded.extractions);
    CacheLoad {
        reused,
        skipped: loaded.skips,
        rejected: None,
    }
}

/// The loader returns no fingerprint, so each file is read once more: the stored fingerprint must describe the bytes in hand.
fn seed_cache(root: &Path, watch: &mut WatchState, extractions: Vec<Arc<FileExtraction>>) -> usize {
    let Some(records) = manifest_records(root) else {
        tracing::warn!("shard manifest unreadable after the load, nothing seeded");
        return 0;
    };
    let mut expected: HashMap<PathBuf, String> = records
        .into_iter()
        .map(|record| (record.path, record.content_hash))
        .collect();

    let mut seeded = 0;
    for extraction in extractions {
        let Some(hash) = expected.remove(&extraction.path) else {
            continue;
        };
        let absolute = if extraction.path.is_absolute() {
            extraction.path.clone()
        } else {
            root.join(&extraction.path)
        };
        let bytes = match fs::read(&absolute) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(path = %absolute.display(), %error, "shard record not seeded");
                continue;
            }
        };
        if ShardManifestRecord::compute_hash(&bytes) != hash {
            tracing::warn!(
                path = %absolute.display(),
                "shard record not seeded: the content changed after the load"
            );
            continue;
        }
        // The payload carries a root-relative path; the cache is keyed and read
        // by absolute paths everywhere else, so normalize before seeding.
        let mut extraction = (*extraction).clone();
        extraction.path = absolute.clone();
        watch
            .cache_mut()
            .update(absolute, Fingerprint::of(&bytes), Arc::new(extraction));
        seeded += 1;
    }
    seeded
}

fn manifest_records(root: &Path) -> Option<Vec<ShardManifestRecord>> {
    let path = root.join(INDEX_DIR_NAME).join(MANIFEST_FILE);
    let file = File::open(path).ok()?;
    read_manifest(BufReader::new(file)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_detects_unchanged_files_by_fingerprint() {
        let bytes = b"def greet(): pass\n";
        let hash = ShardManifestRecord::compute_hash(bytes);
        let fp = Fingerprint::of(bytes);
        assert_eq!(hex(fp.as_bytes()), hash);
        let other = ShardManifestRecord::compute_hash(b"def changed(): pass\n");
        assert_ne!(hex(fp.as_bytes()), other);
    }
}
