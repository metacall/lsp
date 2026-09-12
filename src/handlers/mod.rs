//! LSP query handlers over the snapshot.
mod complete;
mod diagnostics;
mod goto;
mod hover;
mod symbol;

pub use complete::completion_at;
pub use diagnostics::diagnostics_for;
pub use goto::{definition_at, references_at};
pub use hover::hover_at;
pub use symbol::{document_symbols, workspace_symbols};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{Location, Position, Range};

use crate::convert;
use crate::index::{IndexSnapshot, SourceText};
use crate::position::{self, Encoding, SourceFile};
use crate::types::DocUri;

/// Per-request context: snapshot, source lookup, encoding; text and line indices memoized per path.
pub struct QueryCtx<'a> {
    snapshot: &'a IndexSnapshot,
    sources: &'a dyn SourceText,
    encoding: Encoding,
    files: HashMap<PathBuf, Option<SourceFile>>,
}

impl<'a> QueryCtx<'a> {
    pub fn new(
        snapshot: &'a IndexSnapshot,
        sources: &'a dyn SourceText,
        encoding: Encoding,
    ) -> Self {
        Self {
            snapshot,
            sources,
            encoding,
            files: HashMap::new(),
        }
    }

    pub fn snapshot(&self) -> &'a IndexSnapshot {
        self.snapshot
    }

    /// Extraction for a document URI; the borrow outlives the context borrow.
    pub fn document(&self, uri: &DocUri) -> Option<&'a meta_ast::FileExtraction> {
        self.snapshot.file_by_path(&uri.to_path()?)
    }

    pub fn byte_at(&mut self, path: &Path, pos: Position) -> Option<usize> {
        let encoding = self.encoding;
        let source = self.file(path)?;
        source.to_byte_offset(pos, encoding)
    }

    pub fn text(&mut self, path: &Path) -> Option<&str> {
        self.file(path).map(SourceFile::text)
    }

    pub fn range_for(&mut self, path: &Path, range: &meta_ast::model::SourceRange) -> Range {
        let encoding = self.encoding;
        match self.file(path) {
            Some(source) => source.range(range, encoding),
            None => position::range_without_text(range),
        }
    }

    pub fn location(&mut self, symbol: &meta_ast::Symbol) -> Option<Location> {
        let uri = convert::path_to_uri(&symbol.file_path)?;
        let range = self.range_for(&symbol.file_path, &symbol.source_range);
        Some(Location { uri, range })
    }

    pub fn diagnostic(&mut self, diagnostic: &meta_ast::Diagnostic) -> lsp_types::Diagnostic {
        let encoding = self.encoding;
        let source = self.file(&diagnostic.path);
        convert::diagnostic_to_lsp(source, diagnostic, encoding)
    }

    fn file(&mut self, path: &Path) -> Option<&SourceFile> {
        let sources = self.sources;
        self.files
            .entry(path.to_path_buf())
            .or_insert_with_key(|key| {
                sources
                    .source(key)
                    .map(|text| SourceFile::new(text.into_owned()))
            })
            .as_ref()
    }
}
