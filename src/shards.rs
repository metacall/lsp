//! `.meta-ast` index persistence.
//!
//! The worker writes one shard file, a manifest, and a header after every
//! rebuild. Cold start seeds the engine extraction cache from records whose
//! BLAKE3 hash matches the disk bytes. Open buffers are never persisted.
//!
//! The directory is a local cache. It stores absolute paths, so it stays out
//! of source control.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use meta_ast::model::IdGenerator;
use meta_ast::{
    ShardFile, ShardHeader, ShardManifestRecord, WatchState, fingerprint, read_header,
    read_manifest, read_shard, write_header, write_manifest, write_shard,
};

use crate::index::IndexSnapshot;

/// Directory name for the generated index under the workspace root.
pub const INDEX_DIR: &str = ".meta-ast";
const HEADER_FILE: &str = "header.json";
const MANIFEST_FILE: &str = "manifest.jsonl";
const SHARD_FILE: &str = "shards/000.jsonl";

/// Outcome of a cold-start cache seeding pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    /// Records whose hash matched and whose extraction entered the cache.
    pub loaded: usize,
    /// Records that were missing, stale, or unusable.
    pub skipped: usize,
}

/// Write the snapshot to `.meta-ast`. Paths in `overlays` are skipped.
///
/// A single unreadable or unrepresentable file is skipped, not fatal.
pub fn save(
    root: &Path,
    snapshot: &IndexSnapshot,
    overlays: &HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let dir = root.join(INDEX_DIR);
    fs::create_dir_all(dir.join("shards"))?;
    let mut records = Vec::with_capacity(snapshot.extractions.len());
    let mut shards = Vec::with_capacity(snapshot.extractions.len());
    for file in &snapshot.extractions {
        if overlays.contains(&file.path) || !file.path.starts_with(root) {
            continue;
        }
        let Ok(bytes) = fs::read(&file.path) else {
            continue;
        };
        let mut shard = match ShardFile::from_extraction(file, &snapshot.graph) {
            Ok(shard) => shard,
            Err(error) => {
                tracing::warn!(path = %file.path.display(), %error, "shard record skipped");
                continue;
            }
        };
        shard.path = file.path.clone();
        records.push(ShardManifestRecord::from_file_bytes(
            file.path.clone(),
            &bytes,
            mtime_seconds(&file.path),
            SHARD_FILE.to_string(),
        ));
        shards.push(shard);
    }
    let mut header = Vec::new();
    write_header(&mut header, &ShardHeader::new(timestamp()))?;
    let mut manifest = Vec::new();
    write_manifest(&mut manifest, &records)?;
    let mut shard_bytes = Vec::new();
    write_shard(&mut shard_bytes, &shards)?;
    write_atomic(&dir.join(HEADER_FILE), &header)?;
    write_atomic(&dir.join(MANIFEST_FILE), &manifest)?;
    write_atomic(&dir.join(SHARD_FILE), &shard_bytes)?;
    Ok(())
}

/// Seed the engine cache from `.meta-ast`. Failures leave the cache empty.
pub fn load(root: &Path, watch: &mut WatchState) -> LoadStats {
    match load_inner(root, watch) {
        Ok(stats) => stats,
        Err(error) => {
            tracing::warn!(%error, "shard cache ignored");
            LoadStats::default()
        }
    }
}

fn load_inner(root: &Path, watch: &mut WatchState) -> anyhow::Result<LoadStats> {
    let dir = root.join(INDEX_DIR);
    let header = read_header(BufReader::new(File::open(dir.join(HEADER_FILE))?))?;
    let engine = ShardHeader::new(String::new());
    if header.tool_version != engine.tool_version {
        anyhow::bail!("index written by meta-ast {}", header.tool_version);
    }
    let manifest = read_manifest(BufReader::new(File::open(dir.join(MANIFEST_FILE))?))?;

    let mut by_path: HashMap<PathBuf, ShardFile> = HashMap::new();
    let mut loaded_shards: HashSet<String> = HashSet::new();
    for record in &manifest {
        if !is_safe_shard_name(&record.shard) || !loaded_shards.insert(record.shard.clone()) {
            continue;
        }
        let files = read_shard(BufReader::new(File::open(dir.join(&record.shard))?))?;
        for file in files {
            by_path.insert(file.path.clone(), file);
        }
    }

    let id_gen = IdGenerator::with_start(1);
    let mut stats = LoadStats::default();
    for record in manifest {
        let Some(shard) = by_path.remove(&record.path) else {
            stats.skipped += 1;
            continue;
        };
        let Some(absolute) = absolute_under_root(root, &record.path) else {
            stats.skipped += 1;
            continue;
        };
        let Ok(bytes) = fs::read(&absolute) else {
            stats.skipped += 1;
            continue;
        };
        if ShardManifestRecord::compute_hash(&bytes) != record.content_hash {
            stats.skipped += 1;
            continue;
        }
        let Ok(loaded) = shard.load(&id_gen) else {
            stats.skipped += 1;
            continue;
        };
        watch
            .cache_mut()
            .update(absolute, fingerprint(&bytes), Arc::new(loaded.file));
        stats.loaded += 1;
    }
    Ok(stats)
}

/// Shard names must stay inside `shards/`, never `..` or absolute.
fn is_safe_shard_name(name: &str) -> bool {
    let path = Path::new(name);
    !path.is_absolute()
        && path.starts_with("shards")
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
}

/// Accept relative and absolute records, but keep reads under the root.
fn absolute_under_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    absolute.starts_with(root).then_some(absolute)
}

fn mtime_seconds(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// UTC timestamp in ISO 8601 form, without a date library.
fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let clock = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn timestamp_is_iso_8601() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
    }

    #[test]
    fn unsafe_shard_names_are_rejected() {
        assert!(is_safe_shard_name("shards/000.jsonl"));
        assert!(!is_safe_shard_name("shards/../secret"));
        assert!(!is_safe_shard_name("/etc/passwd"));
        assert!(!is_safe_shard_name("000.jsonl"));
    }
}
