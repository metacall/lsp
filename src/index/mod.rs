//! Snapshot type plus read queries.
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meta_ast::deploy::client_call::ResolvedClientCall;
use meta_ast::model::{FileId, SourceRange, SymbolId};
use meta_ast::{CodeGraph, FileExtraction, Fingerprint, FlattenedScopeCache, ResolvedReference};

use crate::types::DocVersion;

pub mod build;
pub mod query;

pub use build::{Persistence, Reindexer, collect_inputs, rebuild_from_inputs};
pub use query::{resolve_targets, symbol_at};

#[derive(Debug, Clone)]
pub struct Occurrence {
    pub path: PathBuf,
    pub range: SourceRange,
}

/// Immutable query surface over one analysis pass. `references` holds one record
/// per resolved use site; `client_calls` one per resolved metacall call site.
pub struct IndexSnapshot {
    pub extractions: Vec<Arc<FileExtraction>>,
    pub graph: CodeGraph,
    pub scope: FlattenedScopeCache,
    pub references: Vec<ResolvedReference>,
    pub client_calls: Vec<ResolvedClientCall>,
    pub diagnostics: Vec<meta_ast::Diagnostic>,
    by_path: HashMap<PathBuf, usize>,
    file_ids: HashMap<PathBuf, FileId>,
    symbols: HashMap<SymbolId, (usize, usize)>,
    refs_out: HashMap<SymbolId, Vec<(SymbolId, f32)>>,
    /// Targets of every recorded reference, keyed by path and reference byte.
    records_by_ref: HashMap<(PathBuf, usize), Vec<(SymbolId, f32)>>,
    occurrences: HashMap<SymbolId, Vec<Occurrence>>,
    content_hashes: HashMap<PathBuf, Fingerprint>,
    versions: HashMap<PathBuf, DocVersion>,
}

impl IndexSnapshot {
    pub fn file_by_path(&self, path: &Path) -> Option<&FileExtraction> {
        self.by_path
            .get(path)
            .and_then(|&index| self.extractions.get(index))
            .map(Arc::as_ref)
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.file_ids.get(path).copied()
    }

    pub fn symbol_by_id(&self, id: SymbolId) -> Option<&meta_ast::Symbol> {
        let &(file_index, symbol_index) = self.symbols.get(&id)?;
        self.extractions.get(file_index)?.symbols.get(symbol_index)
    }

    pub fn symbols(&self) -> impl Iterator<Item = &meta_ast::Symbol> {
        self.extractions.iter().flat_map(|file| file.symbols.iter())
    }

    pub fn references_out(&self, id: SymbolId) -> &[(SymbolId, f32)] {
        self.refs_out.get(&id).map_or(&[], Vec::as_slice)
    }

    pub fn occurrences_of(&self, id: SymbolId) -> &[Occurrence] {
        self.occurrences.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Engine records for one reference byte; empty when it sits outside every symbol, callers then use the scope cache.
    pub(super) fn recorded_targets(&self, path: &Path, byte_start: usize) -> &[(SymbolId, f32)] {
        self.records_by_ref
            .get(&(path.to_path_buf(), byte_start))
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn client_calls_at<'a>(
        &'a self,
        path: &Path,
        range: &SourceRange,
    ) -> impl Iterator<Item = &'a ResolvedClientCall> {
        self.client_calls.iter().filter(move |call| {
            call.source_file == path && call.source_range.as_ref() == Some(range)
        })
    }

    /// Buffer version of the content the index holds; only open documents carry one.
    pub fn document_version(&self, path: &Path) -> Option<DocVersion> {
        self.versions.get(path).copied()
    }

    pub fn content_hash(&self, path: &Path) -> Option<Fingerprint> {
        self.content_hashes.get(path).copied()
    }

    pub fn version_map(&self) -> &HashMap<PathBuf, DocVersion> {
        &self.versions
    }

    /// Numeric generation of this pass; result ids embed it.
    pub fn generation(&self) -> u32 {
        self.graph.snapshot_id.to_raw()
    }
}

/// Source text lookup for range conversion.
pub trait SourceText {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>>;
}
