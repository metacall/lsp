//! Open buffer overlay.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{Range, TextDocumentContentChangeEvent};

use crate::position::{self, Encoding};
use crate::types::{DocUri, DocVersion};

#[derive(Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Indexed,
    Unsupported,
}

pub struct OpenDoc {
    pub version: DocVersion,
    pub lang: meta_ast::LangId,
    pub text: String,
    pub path: Option<PathBuf>,
}

#[derive(Default)]
pub struct BufferStore {
    docs: HashMap<DocUri, OpenDoc>,
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
        self.docs.insert(
            uri.clone(),
            OpenDoc {
                version,
                lang,
                text,
                path: uri.to_path(),
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
    ) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        // A stale notification is a client protocol violation: rejected, never repaired.
        if !version.is_newer_than(doc.version) {
            return false;
        }
        for change in changes {
            match change.range {
                Some(range) => apply_patch(&mut doc.text, range, change.text.as_str(), encoding),
                None => doc.text.clone_from(&change.text),
            }
        }
        doc.version = version;
        true
    }

    pub fn save(&mut self, uri: &DocUri, text: &str) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        if doc.text == text {
            return false;
        }
        doc.text = text.to_string();
        true
    }

    pub fn close(&mut self, uri: &DocUri) -> bool {
        self.docs.remove(uri).is_some()
    }

    pub fn get(&self, uri: &DocUri) -> Option<&OpenDoc> {
        self.docs.get(uri)
    }

    pub fn by_path(&self, path: &Path) -> Option<&OpenDoc> {
        self.docs
            .values()
            .find(|doc| doc.path.as_deref() == Some(path))
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

fn apply_patch(text: &mut String, range: Range, replacement: &str, encoding: Encoding) {
    let index = position::LineIndex::new(text);
    let start = index
        .to_byte_offset(text, range.start, encoding)
        .unwrap_or(text.len());
    let end = index
        .to_byte_offset(text, range.end, encoding)
        .unwrap_or(text.len())
        .max(start);
    text.replace_range(start..end, replacement);
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
        assert!(store.change(
            &doc_uri("file:///a.py"),
            DocVersion::from(2),
            &[full_text("y = 2\n")],
            Encoding::Utf16
        ));
        assert_eq!(store.get(&doc_uri("file:///a.py")).unwrap().text, "y = 2\n");
    }

    #[test]
    fn empty_change_list_keeps_the_version() {
        let mut store = store();
        assert!(!store.change(
            &doc_uri("file:///a.py"),
            DocVersion::from(0),
            &[],
            Encoding::Utf16
        ));
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
        assert!(!store.change(
            &doc_uri("file:///a.py"),
            DocVersion::from(1),
            &[TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: "z".to_string(),
            }],
            Encoding::Utf16,
        ));
        assert_eq!(store.get(&doc_uri("file:///a.py")).unwrap().text, "x = 1\n");
        assert!(!store.change(
            &doc_uri("file:///a.py"),
            DocVersion::from(1),
            &[full_text("y = 2\n")],
            Encoding::Utf16
        ));
        assert_eq!(store.get(&doc_uri("file:///a.py")).unwrap().text, "x = 1\n");
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
        assert!(store.change(
            &doc_uri("file:///a.py"),
            DocVersion::from(2),
            &[TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: "y".to_string(),
            }],
            Encoding::Utf16,
        ));
        assert_eq!(store.get(&doc_uri("file:///a.py")).unwrap().text, "y = 1\n");
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
        assert!(store.change(
            &doc_uri("file:///a.py"),
            DocVersion::from(2),
            &[TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: "z".to_string(),
            }],
            Encoding::Utf16,
        ));
        assert_eq!(
            store.get(&doc_uri("file:///a.py")).unwrap().text,
            "x = \"z\"\n"
        );
    }

    #[test]
    fn save_reports_real_changes_only() {
        let mut store = store();
        assert!(!store.save(&doc_uri("file:///a.py"), "x = 1\n"));
        assert!(store.save(&doc_uri("file:///a.py"), "x = 2\n"));
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
