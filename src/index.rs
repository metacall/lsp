//! Snapshot build plus read queries.
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use meta_ast::{ExtractOptions, ExtractionIdGenerators, GraphBuilder, InMemorySource};

use crate::buffers::BufferStore;
use crate::convert;

pub struct IndexSnapshot {
    pub id: meta_ast::model::SnapshotId,
    pub root: PathBuf,
    pub extractions: Vec<meta_ast::FileExtraction>,
    pub graph: meta_ast::CodeGraph,
    pub diagnostics: Vec<meta_ast::Diagnostic>,
}

pub struct OverlayDoc {
    pub uri: String,
    pub path: PathBuf,
    pub text: String,
    pub version: i32,
    pub lang: meta_ast::LangId,
}

/// Source text lookup for range conversion.
pub trait SourceText {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>>;
}

/// Definition result. The path is the target file, not the request file.
pub struct DefinitionTarget {
    pub path: PathBuf,
    pub range: meta_ast::model::SourceRange,
}

pub fn collect_inputs(root: &Path, buffers: &BufferStore) -> Vec<OverlayDoc> {
    let mut inputs = Vec::new();
    for (uri, doc) in buffers.iter() {
        if let Some(path) = convert::uri_to_path(uri)
            && path.starts_with(root)
        {
            inputs.push(OverlayDoc {
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

pub fn rebuild_from_inputs(
    root: &Path,
    overlays: &[OverlayDoc],
    snapshot_raw: u32,
) -> anyhow::Result<IndexSnapshot> {
    let discovered = meta_ast::input::discover_files(root, None)?;
    let mut overlay_paths = std::collections::HashSet::new();
    for input in overlays {
        overlay_paths.insert(input.path.clone());
    }
    let id_gens = ExtractionIdGenerators::new();
    let disk: Vec<(PathBuf, meta_ast::LangId)> = discovered
        .into_iter()
        .filter(|(path, _)| !overlay_paths.contains(path))
        .collect();
    let mut files =
        meta_ast::extract_with_id_gen(&disk, &ExtractOptions::default(), &id_gens).files;
    for doc in overlays {
        match meta_ast::extract_text_with_id_gen(
            InMemorySource {
                uri: doc.uri.as_str(),
                text: doc.text.as_str(),
                version: doc.version,
                language: doc.lang,
            },
            &ExtractOptions::default(),
            &id_gens,
        ) {
            Ok(versioned) => files.push(versioned.file),
            Err(error) => files.push(meta_ast::FileExtraction::failed(
                doc.path.clone(),
                doc.lang,
                error.to_string(),
            )),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    finish_snapshot(root, files, snapshot_raw)
}

fn finish_snapshot(
    root: &Path,
    files: Vec<meta_ast::FileExtraction>,
    snapshot_raw: u32,
) -> anyhow::Result<IndexSnapshot> {
    let raw = if snapshot_raw == 0 { 1 } else { snapshot_raw };
    let Some(id) = meta_ast::model::SnapshotId::new(raw) else {
        anyhow::bail!("snapshot counter exhausted");
    };
    let mut diagnostics: Vec<meta_ast::Diagnostic> = files
        .iter()
        .flat_map(|file| file.diagnostics.iter().cloned())
        .collect();
    let (graph, _) = GraphBuilder::from_extractions(&files, root, id, &mut diagnostics);
    diagnostics.sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    Ok(IndexSnapshot {
        id,
        root: root.to_path_buf(),
        extractions: files,
        graph,
        diagnostics,
    })
}

pub fn file_for_uri<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &str,
) -> Option<&'a meta_ast::FileExtraction> {
    let path = convert::uri_to_path(uri)?;
    snapshot.extractions.iter().find(|file| file.path == path)
}

fn contains_byte(range: &meta_ast::model::SourceRange, byte: usize) -> bool {
    if range.byte_end > range.byte_start {
        range.byte_start <= byte && byte < range.byte_end
    } else {
        byte == range.byte_start
    }
}

fn smallest_symbol_at(file: &meta_ast::FileExtraction, byte: usize) -> Option<&meta_ast::Symbol> {
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

pub fn definition_target(
    snapshot: &IndexSnapshot,
    uri: &str,
    byte: usize,
) -> Option<DefinitionTarget> {
    let file = file_for_uri(snapshot, uri)?;
    if let Some(symbol) = smallest_symbol_at(file, byte) {
        return Some(DefinitionTarget {
            path: symbol.file_path.clone(),
            range: symbol.source_range.clone(),
        });
    }
    let reference = file
        .references
        .iter()
        .find(|reference| contains_byte(&reference.range, byte))?;
    let same_file = snapshot
        .extractions
        .iter()
        .flat_map(|file| file.symbols.iter())
        .find(|symbol| symbol.name == reference.name && symbol.file_path == file.path);
    let any_file = snapshot
        .extractions
        .iter()
        .flat_map(|file| file.symbols.iter())
        .find(|symbol| symbol.name == reference.name && symbol.language == file.lang);
    let target = same_file.or(any_file)?;
    Some(DefinitionTarget {
        path: target.file_path.clone(),
        range: target.source_range.clone(),
    })
}
