//! Open buffer overlay.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{Range, TextDocumentContentChangeEvent};

use crate::position::{self, Encoding};
use crate::types::{DocUri, DocVersion};

#[derive(Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Indexed,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Stale,
    Unknown,
}

impl ApplyOutcome {
    pub fn applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

pub struct OpenDoc {
    pub version: DocVersion,
    pub lang: meta_ast::LangId,
    pub text: Arc<str>,
    pub path: Option<PathBuf>,
}

#[derive(Default)]
pub struct BufferStore {
    docs: HashMap<DocUri, OpenDoc>,
    by_path: HashMap<PathBuf, DocUri>,
}

impl BufferStore {
    pub fn open(
        &mut self,
        uri: &DocUri,
        version: DocVersion,
        language_id: &str,
        text: String,
    ) -> OpenOutcome {
        let Some(lang) = lang_for(uri, language_id) else {
            return OpenOutcome::Unsupported;
        };
        let path = uri.to_path();
        if let Some(old) = self.docs.get(uri)
            && old.path.as_deref() != path.as_deref()
            && let Some(old_path) = old.path.clone()
        {
            self.by_path.remove(&old_path);
        }
        if let Some(path) = &path {
            self.by_path.insert(path.clone(), uri.clone());
        }
        self.docs.insert(
            uri.clone(),
            OpenDoc {
                version,
                lang,
                text: Arc::from(text),
                path,
            },
        );
        OpenOutcome::Indexed
    }

    pub fn change(
        &mut self,
        uri: &DocUri,
        version: DocVersion,
        changes: &[TextDocumentContentChangeEvent],
        encoding: Encoding,
    ) -> ApplyOutcome {
        let Some(doc) = self.docs.get_mut(uri) else {
            return ApplyOutcome::Unknown;
        };
        if version <= doc.version {
            return ApplyOutcome::Stale;
        }
        if let Some((last, rest)) = changes.split_last()
            && rest.iter().all(|change| change.range.is_none())
            && last.range.is_none()
        {
            doc.text = Arc::from(last.text.as_str());
            doc.version = version;
            return ApplyOutcome::Applied;
        }
        for change in changes {
            match change.range {
                Some(range) => apply_patch(&mut doc.text, range, change.text.as_str(), encoding),
                None => doc.text = Arc::from(change.text.as_str()),
            }
        }
        doc.version = version;
        ApplyOutcome::Applied
    }

    pub fn save(&mut self, uri: &DocUri, text: &str) -> ApplyOutcome {
        let Some(doc) = self.docs.get_mut(uri) else {
            return ApplyOutcome::Unknown;
        };
        if doc.text.as_ref() == text {
            return ApplyOutcome::Stale;
        }
        doc.text = Arc::from(text);
        ApplyOutcome::Applied
    }

    pub fn close(&mut self, uri: &DocUri) -> ApplyOutcome {
        let Some(doc) = self.docs.remove(uri) else {
            return ApplyOutcome::Unknown;
        };
        if let Some(path) = doc.path {
            self.by_path.remove(&path);
        }
        ApplyOutcome::Applied
    }

    pub fn get(&self, uri: &DocUri) -> Option<&OpenDoc> {
        self.docs.get(uri)
    }

    pub fn by_path(&self, path: &Path) -> Option<&OpenDoc> {
        self.by_path.get(path).and_then(|uri| self.docs.get(uri))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DocUri, &OpenDoc)> {
        self.docs.iter()
    }
}

fn lang_for(uri: &DocUri, language_id: &str) -> Option<meta_ast::LangId> {
    if let Some(lang) = lang_from_id(language_id) {
        return Some(lang);
    }
    uri.to_path()
        .and_then(|path| meta_ast::detect_language(&path))
}

/// Exact LSP language ids and aliases; an unknown spelling falls through to `detect_language`.
fn lang_from_id(id: &str) -> Option<meta_ast::LangId> {
    use meta_ast::LangId;
    match id {
        "python" | "py" => Some(LangId::Python),
        "javascript" | "js" | "jsx" => Some(LangId::JavaScript),
        "typescript" | "ts" => Some(LangId::TypeScript),
        "typescriptreact" | "tsx" => Some(LangId::Tsx),
        "c" => Some(LangId::C),
        "cpp" | "c++" | "cc" | "cxx" => Some(LangId::Cpp),
        "rust" | "rs" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "ruby" | "rb" => Some(LangId::Ruby),
        _ => None,
    }
}

fn apply_patch(text: &mut Arc<str>, range: Range, replacement: &str, encoding: Encoding) {
    let index = position::LineIndex::new(text);
    let start = index
        .to_byte_offset(text, range.start, encoding)
        .unwrap_or(text.len());
    let end = index
        .to_byte_offset(text, range.end, encoding)
        .unwrap_or(text.len())
        .max(start);
    let mut owned = text.to_string();
    owned.replace_range(start..end, replacement);
    *text = Arc::from(owned);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert;

    use crate::testutil::doc_uri;
    use lsp_types::Position;

    fn store() -> BufferStore {
        let mut store = BufferStore::default();
        assert_eq!(
            store.open(
                &doc_uri("file:///a.py"),
                DocVersion::from(1),
                "python",
                "x = 1\n".to_string()
            ),
            OpenOutcome::Indexed
        );
        store
    }

    fn full_text(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn open_rejects_unknown_language() {
        let mut store = BufferStore::default();
        assert_eq!(
            store.open(
                &doc_uri("file:///a.zzz"),
                DocVersion::from(1),
                "zzz",
                "x".to_string()
            ),
            OpenOutcome::Unsupported
        );
        assert!(store.get(&doc_uri("file:///a.zzz")).is_none());
    }

    #[test]
    fn change_applies_full_text() {
        let mut store = store();
        assert!(
            store
                .change(
                    &doc_uri("file:///a.py"),
                    DocVersion::from(2),
                    &[full_text("y = 2\n")],
                    Encoding::Utf16
                )
                .applied()
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().text.as_ref(),
            "y = 2\n"
        );
    }

    #[test]
    fn empty_change_list_keeps_the_version() {
        let mut store = store();
        assert!(
            !store
                .change(
                    &doc_uri("file:///a.py"),
                    DocVersion::from(0),
                    &[],
                    Encoding::Utf16
                )
                .applied()
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().version,
            DocVersion::from(1)
        );
    }

    #[test]
    fn change_ignores_every_stale_version() {
        let mut store = store();
        let range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        };
        assert!(
            !store
                .change(
                    &doc_uri("file:///a.py"),
                    DocVersion::from(1),
                    &[TextDocumentContentChangeEvent {
                        range: Some(range),
                        range_length: None,
                        text: "z".to_string(),
                    }],
                    Encoding::Utf16,
                )
                .applied()
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().text.as_ref(),
            "x = 1\n"
        );
        assert!(
            !store
                .change(
                    &doc_uri("file:///a.py"),
                    DocVersion::from(1),
                    &[full_text("y = 2\n")],
                    Encoding::Utf16
                )
                .applied()
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().text.as_ref(),
            "x = 1\n"
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().version,
            DocVersion::from(1)
        );
    }

    #[test]
    fn change_applies_range_patch() {
        let mut store = store();
        let range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        };
        assert!(
            store
                .change(
                    &doc_uri("file:///a.py"),
                    DocVersion::from(2),
                    &[TextDocumentContentChangeEvent {
                        range: Some(range),
                        range_length: None,
                        text: "y".to_string(),
                    }],
                    Encoding::Utf16,
                )
                .applied()
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().text.as_ref(),
            "y = 1\n"
        );
    }

    #[test]
    fn range_patch_honors_utf16_columns() {
        let mut store = BufferStore::default();
        assert_eq!(
            store.open(
                &doc_uri("file:///a.py"),
                DocVersion::from(1),
                "python",
                "x = \"🐍\"\n".to_string()
            ),
            OpenOutcome::Indexed
        );
        let range = Range {
            start: Position {
                line: 0,
                character: 5,
            },
            end: Position {
                line: 0,
                character: 7,
            },
        };
        assert!(
            store
                .change(
                    &doc_uri("file:///a.py"),
                    DocVersion::from(2),
                    &[TextDocumentContentChangeEvent {
                        range: Some(range),
                        range_length: None,
                        text: "z".to_string(),
                    }],
                    Encoding::Utf16,
                )
                .applied()
        );
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().text.as_ref(),
            "x = \"z\"\n"
        );
    }

    #[test]
    fn save_reports_real_changes_only() {
        let mut store = store();
        assert!(!store.save(&doc_uri("file:///a.py"), "x = 1\n").applied());
        assert!(store.save(&doc_uri("file:///a.py"), "x = 2\n").applied());
    }

    #[test]
    fn by_path_finds_open_doc() {
        let path = std::env::temp_dir().join("a.py");
        let uri = convert::path_to_uri(&path).unwrap();
        let mut store = BufferStore::default();
        assert_eq!(
            store.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(1),
                "python",
                "x = 1\n".to_string()
            ),
            OpenOutcome::Indexed
        );
        assert!(store.by_path(&path).is_some());
        assert!(store.by_path(&std::env::temp_dir().join("b.py")).is_none());
    }
}
