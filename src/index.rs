//! Snapshot build plus read queries.
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meta_ast::model::{FileId, SnapshotId, SymbolId};
use meta_ast::{
    CodeGraph, FileExtraction, FlattenedScopeCache, GraphBuilder, Overlay, WatchState,
    reanalyze_extractions,
};

use crate::buffers::BufferStore;
use crate::convert;
use crate::shards;

pub struct IndexSnapshot {
    pub id: SnapshotId,
    pub root: PathBuf,
    pub extractions: Vec<Arc<FileExtraction>>,
    pub graph: CodeGraph,
    pub scope: FlattenedScopeCache,
    pub diagnostics: Vec<meta_ast::Diagnostic>,
    by_path: HashMap<PathBuf, usize>,
    file_ids: HashMap<PathBuf, FileId>,
    symbols: HashMap<SymbolId, (usize, usize)>,
    refs_out: HashMap<SymbolId, Vec<(SymbolId, f32)>>,
    refs_in: HashMap<SymbolId, Vec<(SymbolId, f32)>>,
}

impl IndexSnapshot {
    /// File extraction for a path.
    pub fn file_by_path(&self, path: &Path) -> Option<&FileExtraction> {
        self.by_path
            .get(path)
            .and_then(|&index| self.extractions.get(index))
            .map(Arc::as_ref)
    }

    /// Graph file id for a path.
    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.file_ids.get(path).copied()
    }

    /// Symbol for an id.
    pub fn symbol_by_id(&self, id: SymbolId) -> Option<&meta_ast::Symbol> {
        let &(file_index, symbol_index) = self.symbols.get(&id)?;
        self.extractions.get(file_index)?.symbols.get(symbol_index)
    }

    /// Every symbol in path order.
    pub fn symbols(&self) -> impl Iterator<Item = &meta_ast::Symbol> {
        self.extractions.iter().flat_map(|file| file.symbols.iter())
    }

    /// Symbols that `id` references, with confidence.
    pub fn references_out(&self, id: SymbolId) -> &[(SymbolId, f32)] {
        self.refs_out.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Symbols that reference `id`, with confidence.
    pub fn references_in(&self, id: SymbolId) -> &[(SymbolId, f32)] {
        self.refs_in.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Source text lookup for range conversion.
pub trait SourceText {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>>;
}

/// Reusable reindexer. Owns the engine extraction cache across passes.
pub struct Reindexer {
    state: WatchState,
    persist: bool,
}

impl Reindexer {
    pub fn new() -> Self {
        Self {
            state: WatchState::new(),
            persist: false,
        }
    }

    /// Reindexer that writes `.meta-ast` shards after every rebuild.
    pub fn with_persistence() -> Self {
        Self {
            state: WatchState::new(),
            persist: true,
        }
    }

    /// Seed the engine cache from `.meta-ast`. Missing or stale shards are
    /// ignored; the next rebuild re-extracts what the cache lacks.
    pub fn seed_from_shards(&mut self, root: &Path) -> shards::LoadStats {
        shards::load(root, &mut self.state)
    }

    /// Number of extractions currently held in the engine cache.
    pub fn cached_len(&self) -> usize {
        self.state.cache().len()
    }

    /// Rebuild the snapshot, reusing unchanged extractions.
    pub fn rebuild(
        &mut self,
        root: &Path,
        overlays: &[Overlay],
        snapshot_raw: u32,
    ) -> anyhow::Result<IndexSnapshot> {
        let (extractions, _change, diagnostics) =
            reanalyze_extractions(root, None, overlays, &mut self.state)?;
        let snapshot = finish_snapshot(root, extractions, snapshot_raw, diagnostics)?;
        if self.persist {
            let overlay_paths: HashSet<PathBuf> = overlays
                .iter()
                .map(|overlay| overlay.path.clone())
                .collect();
            if let Err(error) = shards::save(root, &snapshot, &overlay_paths) {
                tracing::warn!(%error, "shard save failed, keeping prior index");
            }
        }
        Ok(snapshot)
    }
}

impl Default for Reindexer {
    fn default() -> Self {
        Self::new()
    }
}

/// Cold rebuild with a fresh cache. Use in tests and the first build.
pub fn rebuild_from_inputs(
    root: &Path,
    overlays: &[Overlay],
    snapshot_raw: u32,
) -> anyhow::Result<IndexSnapshot> {
    Reindexer::new().rebuild(root, overlays, snapshot_raw)
}

pub fn collect_inputs(root: &Path, buffers: &BufferStore) -> Vec<Overlay> {
    let mut inputs = Vec::new();
    for (uri, doc) in buffers.iter() {
        if let Some(path) = convert::uri_to_path(uri)
            && path.starts_with(root)
        {
            inputs.push(Overlay {
                uri: uri.clone(),
                path,
                text: doc.text.clone(),
                version: doc.version,
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
) -> anyhow::Result<IndexSnapshot> {
    let raw = if snapshot_raw == 0 { 1 } else { snapshot_raw };
    let Some(id) = SnapshotId::new(raw) else {
        anyhow::bail!("snapshot counter exhausted");
    };
    let (graph, _scc, scope) =
        GraphBuilder::from_extractions_with_scope(&extractions, root, id, &mut diagnostics);
    diagnostics.sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));

    let mut by_path = HashMap::with_capacity(extractions.len());
    let mut symbols = HashMap::new();
    for (file_index, file) in extractions.iter().enumerate() {
        by_path.insert(file.path.clone(), file_index);
        for (symbol_index, symbol) in file.symbols.iter().enumerate() {
            symbols.insert(symbol.id, (file_index, symbol_index));
        }
    }

    let mut refs_out: HashMap<SymbolId, Vec<(SymbolId, f32)>> = HashMap::new();
    let mut refs_in: HashMap<SymbolId, Vec<(SymbolId, f32)>> = HashMap::new();
    for (source_id, target_id, confidence) in graph.reference_edges() {
        refs_out
            .entry(source_id)
            .or_default()
            .push((target_id, confidence));
        refs_in
            .entry(target_id)
            .or_default()
            .push((source_id, confidence));
    }

    let mut file_ids = HashMap::with_capacity(extractions.len());
    for (file_id, file) in graph.files() {
        file_ids.insert(file.path.clone(), file_id);
    }

    Ok(IndexSnapshot {
        id,
        root: root.to_path_buf(),
        extractions,
        graph,
        scope,
        diagnostics,
        by_path,
        file_ids,
        symbols,
        refs_out,
        refs_in,
    })
}

pub fn file_for_uri<'a>(snapshot: &'a IndexSnapshot, uri: &str) -> Option<&'a FileExtraction> {
    let path = convert::uri_to_path(uri)?;
    snapshot.file_by_path(&path)
}

fn contains_byte(range: &meta_ast::model::SourceRange, byte: usize) -> bool {
    if range.byte_end > range.byte_start {
        range.byte_start <= byte && byte < range.byte_end
    } else {
        byte == range.byte_start
    }
}

fn smallest_symbol_at(file: &FileExtraction, byte: usize) -> Option<&meta_ast::Symbol> {
    file.symbols
        .iter()
        .filter(|symbol| contains_byte(&symbol.source_range, byte))
        .min_by_key(|symbol| {
            symbol
                .source_range
                .byte_end
                .saturating_sub(symbol.source_range.byte_start)
        })
}

pub fn symbol_at<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &str,
    byte: usize,
) -> Option<&'a meta_ast::Symbol> {
    let file = file_for_uri(snapshot, uri)?;
    smallest_symbol_at(file, byte)
}

/// Reference at a byte offset.
pub fn reference_at<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &str,
    byte: usize,
) -> Option<&'a meta_ast::UnresolvedReference> {
    let file = file_for_uri(snapshot, uri)?;
    file.references
        .iter()
        .find(|reference| contains_byte(&reference.range, byte))
}

/// Client call site at a byte offset.
pub fn call_site_at<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &str,
    byte: usize,
) -> Option<&'a meta_ast::deploy::scanner::CallSite> {
    let file = file_for_uri(snapshot, uri)?;
    file.call_sites.iter().find(|site| {
        site.variant == meta_ast::deploy::scanner::CallSiteVariant::ClientCall
            && site
                .source_range
                .as_ref()
                .is_some_and(|range| contains_byte(range, byte))
    })
}

fn best_reference_out(
    snapshot: &IndexSnapshot,
    source: SymbolId,
    name: &str,
) -> Option<(SymbolId, f32)> {
    snapshot
        .references_out(source)
        .iter()
        .filter(|(id, _)| {
            snapshot
                .symbol_by_id(*id)
                .is_some_and(|symbol| symbol.name == name)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .copied()
}

/// Resolve the cursor to a definition target symbol.
///
/// A `metacall()` call site resolves through reference edges. An ordinary
/// reference resolves through the engine scope cache first, then through
/// reference edges. A declaration resolves to itself.
pub fn resolve_at(snapshot: &IndexSnapshot, uri: &str, byte: usize) -> Option<SymbolId> {
    let file = file_for_uri(snapshot, uri)?;
    let enclosing = smallest_symbol_at(file, byte);

    if let Some(site) = call_site_at(snapshot, uri, byte)
        && let Some(name) = site.function_name.as_deref()
        && let Some(enclosing) = enclosing
        && let Some((id, _)) = best_reference_out(snapshot, enclosing.id, name)
    {
        return Some(id);
    }

    if let Some(reference) = reference_at(snapshot, uri, byte) {
        if let Some(file_id) = snapshot.file_id(&file.path)
            && let Some(candidates) = snapshot.scope.resolve(file_id, &reference.name)
            && let Some((id, _)) = candidates.iter().max_by(|a, b| a.1.total_cmp(&b.1))
        {
            return Some(*id);
        }
        if let Some(enclosing) = enclosing
            && let Some((id, _)) = best_reference_out(snapshot, enclosing.id, &reference.name)
        {
            return Some(id);
        }
        return None;
    }

    enclosing.map(|symbol| symbol.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    #[test]
    fn unresolved_reference_does_not_fall_back_to_caller() {
        let (dir, file) = workspace("def caller():\n    return unknown_target(\"x\")\n");
        let snapshot = rebuild_from_inputs(dir.path(), &[], 1).unwrap();
        let extraction = &snapshot.extractions[0];
        let reference = extraction
            .references
            .iter()
            .find(|reference| reference.name == "unknown_target")
            .expect("reference");
        let uri = convert::path_to_uri(&file).unwrap().to_string();
        assert_eq!(
            resolve_at(&snapshot, &uri, reference.range.byte_start),
            None
        );
    }

    #[test]
    fn reindexer_reuses_unchanged_extractions() {
        let (dir, _file) = workspace("def greet(): pass\n");
        let mut reindexer = Reindexer::new();

        let first = reindexer.rebuild(dir.path(), &[], 1).unwrap();
        let second = reindexer.rebuild(dir.path(), &[], 2).unwrap();

        assert_eq!(first.extractions.len(), 1);
        assert_eq!(second.extractions.len(), 1);
        assert!(
            Arc::ptr_eq(&first.extractions[0], &second.extractions[0]),
            "unchanged file must reuse the cached extraction"
        );
        assert_eq!(second.scope.iter_scopes().count(), 1);
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
        let snapshot = reindexer.rebuild(dir.path(), &[overlay], 1).unwrap();

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
        let snapshot = rebuild_from_inputs(dir.path(), &[], 1).unwrap();

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
                .references_in(greet.id)
                .iter()
                .any(|(id, _)| *id == caller.id)
        );
        assert!(
            snapshot
                .references_out(caller.id)
                .iter()
                .any(|(id, _)| *id == greet.id)
        );
        assert!(snapshot.file_by_path(&greet.file_path).is_some());
    }
}
