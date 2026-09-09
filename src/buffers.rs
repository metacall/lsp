//! Open buffer overlay.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{Range, TextDocumentContentChangeEvent};

use crate::convert;
use crate::position::{self, Encoding};

pub struct OpenDoc {
    pub version: i32,
    pub lang: meta_ast::LangId,
    pub text: String,
    pub path: Option<PathBuf>,
}

#[derive(Default)]
pub struct BufferStore {
    docs: HashMap<String, OpenDoc>,
}

impl BufferStore {
    pub fn open(&mut self, uri: &str, version: i32, language_id: &str, text: String) -> bool {
        let Some(lang) = lang_for(uri, language_id) else {
            return false;
        };
        self.docs.insert(
            uri.to_string(),
            OpenDoc {
                version,
                lang,
                text,
                path: convert::uri_to_path(uri),
            },
        );
        true
    }

    pub fn change(
        &mut self,
        uri: &str,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
        encoding: Encoding,
    ) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        if version <= doc.version {
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

    pub fn save(&mut self, uri: &str, text: &str) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        if doc.text == text {
            return false;
        }
        doc.text = text.to_string();
        true
    }

    pub fn close(&mut self, uri: &str) -> bool {
        self.docs.remove(uri).is_some()
    }

    pub fn get(&self, uri: &str) -> Option<&OpenDoc> {
        self.docs.get(uri)
    }

    pub fn by_path(&self, path: &Path) -> Option<&OpenDoc> {
        self.docs
            .values()
            .find(|doc| doc.path.as_deref() == Some(path))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &OpenDoc)> {
        self.docs.iter()
    }
}

fn lang_for(uri: &str, language_id: &str) -> Option<meta_ast::LangId> {
    if let Some(lang) = lang_from_id(language_id) {
        return Some(lang);
    }
    let path = convert::uri_to_path(uri)?;
    meta_ast::detect_language(&path)
}

fn lang_from_id(id: &str) -> Option<meta_ast::LangId> {
    use meta_ast::LangId;
    match id.to_ascii_lowercase().as_str() {
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
    let start = position::to_byte_offset(text, range.start, encoding).unwrap_or(text.len());
    let end = position::to_byte_offset(text, range.end, encoding)
        .unwrap_or(text.len())
        .max(start);
    text.replace_range(start..end, replacement);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;

    fn store() -> BufferStore {
        let mut store = BufferStore::default();
        assert!(store.open("file:///a.py", 1, "python", "x = 1\n".to_string()));
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
        assert!(!store.open("file:///a.zzz", 1, "zzz", "x".to_string()));
        assert!(store.get("file:///a.zzz").is_none());
    }

    #[test]
    fn change_applies_full_text() {
        let mut store = store();
        assert!(store.change("file:///a.py", 2, &[full_text("y = 2\n")], Encoding::Utf16));
        assert_eq!(store.get("file:///a.py").unwrap().text, "y = 2\n");
    }

    #[test]
    fn change_ignores_stale_version() {
        let mut store = store();
        assert!(!store.change("file:///a.py", 1, &[full_text("y = 2\n")], Encoding::Utf16));
        assert_eq!(store.get("file:///a.py").unwrap().text, "x = 1\n");
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
            "file:///a.py",
            2,
            &[TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: "y".to_string(),
            }],
            Encoding::Utf16,
        ));
        assert_eq!(store.get("file:///a.py").unwrap().text, "y = 1\n");
    }

    #[test]
    fn range_patch_honors_utf16_columns() {
        let mut store = BufferStore::default();
        assert!(store.open("file:///a.py", 1, "python", "x = \"🐍\"\n".to_string()));
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
            "file:///a.py",
            2,
            &[TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: "z".to_string(),
            }],
            Encoding::Utf16,
        ));
        assert_eq!(store.get("file:///a.py").unwrap().text, "x = \"z\"\n");
    }

    #[test]
    fn save_reports_real_changes_only() {
        let mut store = store();
        assert!(!store.save("file:///a.py", "x = 1\n"));
        assert!(store.save("file:///a.py", "x = 2\n"));
    }

    #[test]
    fn by_path_finds_open_doc() {
        let store = store();
        assert!(store.by_path(Path::new("/a.py")).is_some());
        assert!(store.by_path(Path::new("/b.py")).is_none());
    }
}
