//! Open buffer overlay.
use std::collections::HashMap;

use lsp_types::{Position, Range, TextDocumentContentChangeEvent};

use crate::convert;

pub struct OpenDoc {
    pub version: i32,
    pub lang: meta_ast::LangId,
    pub text: String,
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
            },
        );
        true
    }

    pub fn change(
        &mut self,
        uri: &str,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
    ) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        if version <= doc.version {
            return false;
        }
        for change in changes {
            match change.range {
                Some(range) => apply_patch(&mut doc.text, range, change.text.as_str()),
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

    pub fn iter(&self) -> impl Iterator<Item = (&String, &OpenDoc)> {
        self.docs.iter()
    }
}

pub fn lang_for(uri: &str, language_id: &str) -> Option<meta_ast::LangId> {
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

fn apply_patch(text: &mut String, range: Range, replacement: &str) {
    let start = offset_of(text, range.start);
    let end = offset_of(text, range.end).max(start);
    text.replace_range(start..end, replacement);
}

fn offset_of(text: &str, pos: Position) -> usize {
    let line = pos.line as usize;
    let mut column = pos.character as usize;
    let mut offset = 0usize;
    for (index, content) in text.split('\n').enumerate() {
        if index == line {
            column = column.min(content.len());
            while !content.is_char_boundary(column) {
                column -= 1;
            }
            return offset + column;
        }
        offset += content.len() + 1;
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> BufferStore {
        let mut store = BufferStore::default();
        assert!(store.open("file:///a.py", 1, "python", "x = 1\n".to_string()));
        store
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
        assert!(store.change(
            "file:///a.py",
            2,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "y = 2\n".to_string(),
            }],
        ));
        assert_eq!(store.get("file:///a.py").unwrap().text, "y = 2\n");
    }

    #[test]
    fn change_ignores_stale_version() {
        let mut store = store();
        assert!(!store.change(
            "file:///a.py",
            1,
            &[TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "y = 2\n".to_string(),
            }],
        ));
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
        ));
        assert_eq!(store.get("file:///a.py").unwrap().text, "y = 1\n");
    }

    #[test]
    fn save_reports_real_changes_only() {
        let mut store = store();
        assert!(!store.save("file:///a.py", "x = 1\n"));
        assert!(store.save("file:///a.py", "x = 2\n"));
    }
}
