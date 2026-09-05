//! Snapshot build plus read queries.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::Position;
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

impl IndexSnapshot {
    pub fn empty(root: PathBuf, id: meta_ast::model::SnapshotId) -> Self {
        Self {
            id,
            root,
            extractions: Vec::new(),
            graph: meta_ast::CodeGraph::new(id),
            diagnostics: Vec::new(),
        }
    }
}

pub fn rebuild(
    root: &Path,
    buffers: &BufferStore,
    snapshot_raw: u32,
) -> anyhow::Result<IndexSnapshot> {
    let discovered = meta_ast::input::discover_files(root, None)?;
    let mut overlay: HashMap<PathBuf, String> = HashMap::new();
    for (uri, _) in buffers.iter() {
        if let Some(path) = convert::uri_to_path(uri)
            && path.starts_with(root)
        {
            overlay.insert(path, uri.clone());
        }
    }
    let id_gens = ExtractionIdGenerators::new();
    let disk: Vec<(PathBuf, meta_ast::LangId)> = discovered
        .into_iter()
        .filter(|(path, _)| !overlay.contains_key(path))
        .collect();
    let mut files =
        meta_ast::extract_with_id_gen(&disk, &ExtractOptions::default(), &id_gens).files;
    let mut names: Vec<(PathBuf, String)> = overlay.into_iter().collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, uri) in &names {
        let Some(doc) = buffers.get(uri.as_str()) else {
            continue;
        };
        match meta_ast::extract_text_with_id_gen(
            InMemorySource {
                uri: uri.as_str(),
                text: doc.text.as_str(),
                version: doc.version,
                language: doc.lang,
            },
            &ExtractOptions::default(),
            &id_gens,
        ) {
            Ok(versioned) => files.push(versioned.file),
            Err(error) => files.push(meta_ast::FileExtraction::failed(
                path.clone(),
                doc.lang,
                error.to_string(),
            )),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
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

pub fn symbol_at<'a>(
    snapshot: &'a IndexSnapshot,
    uri: &str,
    pos: Position,
) -> Option<&'a meta_ast::Symbol> {
    let file = file_for_uri(snapshot, uri)?;
    file.symbols
        .iter()
        .filter(|symbol| convert::contains(&symbol.source_range, pos))
        .min_by_key(|symbol| {
            symbol
                .source_range
                .byte_end
                .saturating_sub(symbol.source_range.byte_start)
        })
}

pub fn definition_target(
    snapshot: &IndexSnapshot,
    uri: &str,
    pos: Position,
) -> Option<(String, meta_ast::model::SourceRange)> {
    let file = file_for_uri(snapshot, uri)?;
    if let Some(symbol) = file
        .symbols
        .iter()
        .filter(|symbol| convert::contains(&symbol.source_range, pos))
        .min_by_key(|symbol| {
            symbol
                .source_range
                .byte_end
                .saturating_sub(symbol.source_range.byte_start)
        })
    {
        let target = convert::path_to_uri(&symbol.file_path)?
            .as_str()
            .to_string();
        return Some((target, symbol.source_range.clone()));
    }
    let reference = file
        .references
        .iter()
        .find(|reference| convert::contains(&reference.range, pos))?;
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
    let target_uri = convert::path_to_uri(&target.file_path)?
        .as_str()
        .to_string();
    Some((target_uri, target.source_range.clone()))
}
