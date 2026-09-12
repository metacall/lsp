//! Snapshot build from engine reanalysis.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meta_ast::model::{SnapshotId, SymbolId};
use meta_ast::{
    FileExtraction, Fingerprint, GraphAnalysis, Overlay, WatchState, reanalyze_extractions,
};

use super::{IndexSnapshot, Occurrence};
use crate::buffers::BufferStore;
use crate::shards;
use crate::types::DocVersion;

/// Whether rebuilds write `.meta-ast` shards: off for tests, on for the server worker.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Persistence {
    #[default]
    Disabled,
    Enabled,
}

/// Reusable reindexer. Owns the engine extraction cache across passes.
pub struct Reindexer {
    state: WatchState,
    persistence: Persistence,
    last: Option<Arc<IndexSnapshot>>,
    snapshot_counter: u32,
}

impl Reindexer {
    pub fn new() -> Self {
        Self::with_persistence(Persistence::Disabled)
    }

    pub fn with_persistence(persistence: Persistence) -> Self {
        Self {
            state: WatchState::new(),
            persistence,
            last: None,
            snapshot_counter: 0,
        }
    }

    /// Seed the engine cache from `.meta-ast`; a rejected record costs only its file.
    pub fn seed_from_shards(&mut self, root: &Path) -> shards::CacheLoad {
        shards::load(root, &mut self.state)
    }

    /// Rebuild, reusing unchanged extractions; a no-change pass returns the retained
    /// snapshot unless a buffer version is unrecorded, which forces the rebuild.
    pub fn rebuild(
        &mut self,
        root: &Path,
        overlays: &[Overlay],
    ) -> anyhow::Result<Arc<IndexSnapshot>> {
        let (extractions, change, diagnostics) =
            reanalyze_extractions(root, None, overlays, &mut self.state)?;
        let versions = overlay_versions(overlays);
        let no_file_changed =
            change.files_added + change.files_modified + change.files_removed == 0;
        if no_file_changed
            && let Some(last) = &self.last
            && last.version_map() == &versions
        {
            return Ok(Arc::clone(last));
        }
        self.snapshot_counter = self.snapshot_counter.wrapping_add(1);
        let snapshot = Arc::new(finish_snapshot(
            root,
            extractions,
            self.snapshot_counter,
            diagnostics,
            &self.state,
            versions,
        )?);
        if self.persistence == Persistence::Enabled {
            let overlay_paths: HashSet<PathBuf> = overlays
                .iter()
                .map(|overlay| overlay.path.clone())
                .collect();
            if let Err(error) = shards::save(root, &snapshot, &overlay_paths) {
                tracing::warn!(%error, "shard save failed, keeping prior index");
            }
        }
        self.last = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }
}

impl Default for Reindexer {
    fn default() -> Self {
        Self::new()
    }
}

/// Cold rebuild with a fresh cache: tests and the first build.
pub fn rebuild_from_inputs(
    root: &Path,
    overlays: &[Overlay],
) -> anyhow::Result<Arc<IndexSnapshot>> {
    Reindexer::new().rebuild(root, overlays)
}

pub fn collect_inputs(root: &Path, buffers: &BufferStore) -> Vec<Overlay> {
    let mut inputs = Vec::new();
    for (uri, doc) in buffers.iter() {
        if let Some(path) = uri.to_path()
            && path.starts_with(root)
        {
            inputs.push(Overlay {
                uri: uri.as_str().to_string(),
                path,
                text: doc.text.clone(),
                version: doc.version.get(),
                lang: doc.lang,
            });
        }
    }
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    inputs
}

fn finish_snapshot(
    root: &Path,
    extractions: Vec<Arc<FileExtraction>>,
    snapshot_raw: u32,
    mut diagnostics: Vec<meta_ast::Diagnostic>,
    state: &WatchState,
    versions: HashMap<PathBuf, DocVersion>,
) -> anyhow::Result<IndexSnapshot> {
    let raw = if snapshot_raw == 0 { 1 } else { snapshot_raw };
    let Some(id) = SnapshotId::new(raw) else {
        anyhow::bail!("snapshot counter exhausted");
    };
    // One engine pass gives the graph, the SCC, the scope cache and the records.
    let (analysis, mut graph_diagnostics) =
        meta_ast::pipeline::build_analysis(extractions, root, id);
    diagnostics.append(&mut graph_diagnostics);
    diagnostics.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    let GraphAnalysis {
        graph,
        scope,
        references,
        extractions,
        ..
    } = analysis;

    // The builder resolves call sites for edges but hides the records; one more pass produces them, and diagnostics stay the builder's.
    let call_sites: Vec<meta_ast::deploy::scanner::CallSite> = extractions
        .iter()
        .flat_map(|file| file.call_sites.iter().cloned())
        .collect();
    let client_calls = if call_sites.is_empty() {
        Vec::new()
    } else {
        meta_ast::deploy::client_call::resolve_client_call_projections(
            &graph,
            &extractions,
            &call_sites,
            root,
        )
        .resolved
    };

    let mut by_path = HashMap::with_capacity(extractions.len());
    let mut symbols = HashMap::new();
    for (file_index, file) in extractions.iter().enumerate() {
        by_path.insert(file.path.clone(), file_index);
        for (symbol_index, symbol) in file.symbols.iter().enumerate() {
            symbols.insert(symbol.id, (file_index, symbol_index));
        }
    }

    let mut refs_out: HashMap<SymbolId, Vec<(SymbolId, f32)>> = HashMap::new();
    for (source_id, target_id, confidence) in graph.reference_edges() {
        refs_out
            .entry(source_id)
            .or_default()
            .push((target_id, confidence));
    }

    let mut records_by_ref: HashMap<(PathBuf, usize), Vec<(SymbolId, f32)>> = HashMap::new();
    for record in &references {
        let targets = records_by_ref
            .entry((record.file_path.clone(), record.range.byte_start))
            .or_default();
        if !targets.iter().any(|(id, _)| *id == record.target) {
            targets.push((record.target, record.confidence));
        }
    }

    let mut file_ids = HashMap::with_capacity(extractions.len());
    for (file_id, file) in graph.files() {
        file_ids.insert(file.path.clone(), file_id);
    }

    let mut snapshot = IndexSnapshot {
        extractions,
        graph,
        scope,
        references,
        client_calls,
        diagnostics,
        by_path,
        file_ids,
        symbols,
        refs_out,
        records_by_ref,
        occurrences: HashMap::new(),
        content_hashes: content_hashes(state),
        versions,
    };
    fill_occurrences(&mut snapshot);
    Ok(snapshot)
}

/// Buffer version of every overlay: the version the diagnostics describe.
fn overlay_versions(overlays: &[Overlay]) -> HashMap<PathBuf, DocVersion> {
    overlays
        .iter()
        .map(|overlay| (overlay.path.clone(), DocVersion::from(overlay.version)))
        .collect()
}

fn content_hashes(state: &WatchState) -> HashMap<PathBuf, Fingerprint> {
    state
        .cache()
        .paths()
        .filter_map(|path| {
            state
                .cache()
                .fingerprint_of(path)
                .map(|fp| (path.clone(), fp))
        })
        .collect()
}

/// Index every resolved use site by target, with the cursor path's rule, so both directions agree.
fn fill_occurrences(snapshot: &mut IndexSnapshot) {
    use super::query::{client_call_targets, reference_targets};

    let mut occurrences: HashMap<SymbolId, Vec<Occurrence>> = HashMap::new();
    for file in &snapshot.extractions {
        for reference in &file.references {
            for (id, _) in reference_targets(snapshot, file, reference) {
                occurrences.entry(id).or_default().push(Occurrence {
                    path: file.path.clone(),
                    range: reference.range.clone(),
                });
            }
        }
    }
    // One call site expands once through the helper: per-record expansion is the count squared.
    let mut visited: HashSet<(PathBuf, usize)> = HashSet::new();
    for call in &snapshot.client_calls {
        let Some(range) = call.source_range.clone() else {
            continue;
        };
        if !visited.insert((call.source_file.clone(), range.byte_start)) {
            continue;
        }
        for (id, _) in client_call_targets(snapshot, &call.source_file, &range) {
            occurrences.entry(id).or_default().push(Occurrence {
                path: call.source_file.clone(),
                range: range.clone(),
            });
        }
    }
    snapshot.occurrences = occurrences;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert;

    fn workspace(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    #[test]
    fn reindexer_reuses_unchanged_extractions() {
        let (dir, _file) = workspace("def greet(): pass\n");
        let mut reindexer = Reindexer::new();

        let first = reindexer.rebuild(dir.path(), &[]).unwrap();
        let second = reindexer.rebuild(dir.path(), &[]).unwrap();

        assert_eq!(first.extractions.len(), 1);
        assert_eq!(second.extractions.len(), 1);
        assert!(
            Arc::ptr_eq(&first.extractions[0], &second.extractions[0]),
            "unchanged file must reuse the cached extraction"
        );
        assert_eq!(second.scope.iter_scopes().count(), 1);
    }

    #[test]
    fn a_pass_that_changes_no_file_returns_the_same_snapshot() {
        let (dir, _file) = workspace("def greet(): pass\n");
        let mut reindexer = Reindexer::new();

        let first = reindexer.rebuild(dir.path(), &[]).unwrap();
        let second = reindexer.rebuild(dir.path(), &[]).unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "a no-op pass must not rebuild the graph, occurrences, or shards"
        );
    }

    #[test]
    fn a_new_overlay_version_rebuilds_the_snapshot() {
        let (dir, file) = workspace("def greet(): pass\n");
        let overlay = Overlay {
            uri: convert::path_to_uri(&file).unwrap().to_string(),
            path: file.clone(),
            text: "def greet(): pass\n".to_string(),
            version: 1,
            lang: meta_ast::LangId::Python,
        };
        let mut reindexer = Reindexer::new();
        let first = reindexer.rebuild(dir.path(), &[]).unwrap();
        assert_eq!(first.document_version(&file), None);

        // The text is unchanged; the version map still differs, so the snapshot is rebuilt.
        let versioned = reindexer
            .rebuild(dir.path(), std::slice::from_ref(&overlay))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &versioned));
        assert_eq!(versioned.document_version(&file), Some(DocVersion::from(1)));

        let again = reindexer
            .rebuild(dir.path(), std::slice::from_ref(&overlay))
            .unwrap();
        assert!(Arc::ptr_eq(&versioned, &again));
    }

    #[test]
    fn reindexer_applies_overlay_over_disk() {
        let (dir, file) = workspace("def disk(): pass\n");
        let overlay = Overlay {
            uri: convert::path_to_uri(&file).unwrap().to_string(),
            path: file.clone(),
            text: "def buffer(): pass\n".to_string(),
            version: 2,
            lang: meta_ast::LangId::Python,
        };
        let mut reindexer = Reindexer::new();
        let snapshot = reindexer.rebuild(dir.path(), &[overlay]).unwrap();

        let names: Vec<&str> = snapshot.extractions[0]
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert!(names.contains(&"buffer"));
        assert!(!names.contains(&"disk"));
    }

    #[test]
    fn derived_indices_resolve_symbols_and_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.py"),
            "def greet(name):\n    return name\n\n\ndef caller():\n    return greet(\"x\")\n",
        )
        .unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();

        let greet = snapshot
            .symbols()
            .find(|symbol| symbol.name == "greet")
            .expect("greet");
        let caller = snapshot
            .symbols()
            .find(|symbol| symbol.name == "caller")
            .expect("caller");
        assert!(snapshot.symbol_by_id(greet.id).is_some());
        assert!(snapshot.file_id(&greet.file_path).is_some());
        assert!(
            snapshot
                .references_out(caller.id)
                .iter()
                .any(|(id, _)| *id == greet.id)
        );
        assert!(snapshot.file_by_path(&greet.file_path).is_some());
    }

    #[test]
    fn overlay_versions_reach_the_snapshot() {
        let (dir, file) = workspace("def disk(): pass\n");
        let overlay = Overlay {
            uri: convert::path_to_uri(&file).unwrap().to_string(),
            path: file.clone(),
            text: "def buffer(): pass\n".to_string(),
            version: 7,
            lang: meta_ast::LangId::Python,
        };
        let snapshot = rebuild_from_inputs(dir.path(), &[overlay]).unwrap();

        assert_eq!(snapshot.document_version(&file), Some(DocVersion::from(7)));
        assert_eq!(
            snapshot.document_version(&dir.path().join("missing.py")),
            None,
            "only overlay documents carry a version"
        );
    }
}
