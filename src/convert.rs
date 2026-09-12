//! Type maps between meta-ast and LSP.
use std::path::Path;

use lsp_types::{CompletionItemKind, DiagnosticSeverity, Range, SymbolKind, Uri};

use crate::position::{self, Encoding, SourceFile};

pub fn path_to_uri(path: &Path) -> Option<Uri> {
    let path = dunce::simplified(path);
    url::Url::from_file_path(path).ok()?.as_str().parse().ok()
}

pub fn symbol_kind(kind: meta_ast::SymbolKind) -> SymbolKind {
    kind_pair(kind).0
}

pub fn completion_kind(kind: meta_ast::SymbolKind) -> CompletionItemKind {
    kind_pair(kind).1
}

/// Display word for one engine symbol kind; kept beside `kind_pair` so the two cannot drift.
pub fn kind_word(kind: meta_ast::SymbolKind) -> &'static str {
    use meta_ast::SymbolKind as Engine;
    match kind {
        Engine::Function => "function",
        Engine::Method => "method",
        Engine::Class => "class",
        Engine::Struct => "struct",
        Engine::Interface => "interface",
        Engine::Trait => "trait",
        Engine::Enum => "enum",
        Engine::Object => "object",
        Engine::Constant => "constant",
        Engine::Module => "module",
        Engine::Namespace => "namespace",
        Engine::TypeAlias => "type alias",
        _ => "variable",
    }
}

/// LSP kind pair; LSP has no OBJECT completion kind, so an Object completes as MODULE.
fn kind_pair(kind: meta_ast::SymbolKind) -> (SymbolKind, CompletionItemKind) {
    use meta_ast::SymbolKind as Engine;
    match kind {
        Engine::Function => (SymbolKind::FUNCTION, CompletionItemKind::FUNCTION),
        Engine::Method => (SymbolKind::METHOD, CompletionItemKind::METHOD),
        Engine::Class => (SymbolKind::CLASS, CompletionItemKind::CLASS),
        Engine::Struct => (SymbolKind::STRUCT, CompletionItemKind::STRUCT),
        Engine::Interface | Engine::Trait => (SymbolKind::INTERFACE, CompletionItemKind::INTERFACE),
        Engine::Enum => (SymbolKind::ENUM, CompletionItemKind::ENUM),
        Engine::Object => (SymbolKind::OBJECT, CompletionItemKind::MODULE),
        Engine::Constant => (SymbolKind::CONSTANT, CompletionItemKind::CONSTANT),
        Engine::Module => (SymbolKind::MODULE, CompletionItemKind::MODULE),
        Engine::Namespace => (SymbolKind::NAMESPACE, CompletionItemKind::MODULE),
        Engine::TypeAlias => (
            SymbolKind::TYPE_PARAMETER,
            CompletionItemKind::TYPE_PARAMETER,
        ),
        _ => (SymbolKind::VARIABLE, CompletionItemKind::VARIABLE),
    }
}

fn severity(severity: meta_ast::Severity) -> DiagnosticSeverity {
    match severity {
        meta_ast::Severity::Error => DiagnosticSeverity::ERROR,
        // The enum is non-exhaustive upstream; warn is the safe default.
        _ => DiagnosticSeverity::WARNING,
    }
}

pub fn diagnostic_to_lsp(
    source: Option<&SourceFile>,
    diagnostic: &meta_ast::Diagnostic,
    encoding: Encoding,
) -> lsp_types::Diagnostic {
    let range = match diagnostic.source_range.as_ref() {
        Some(range) => match source {
            Some(source) => source.range(range, encoding),
            None => position::range_without_text(range),
        },
        None => Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 0,
                character: 1,
            },
        },
    };
    lsp_types::Diagnostic {
        range,
        severity: Some(severity(diagnostic.severity)),
        code: None,
        code_description: None,
        source: Some("meta-ast".to_string()),
        message: diagnostic.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DocUri;
    use std::path::PathBuf;

    #[test]
    fn diagnostic_to_lsp_follows_negotiated_encoding() {
        const TEXT: &str = "aé中🐍b";
        let diagnostic = meta_ast::Diagnostic {
            path: PathBuf::from("/tmp/sample.py"),
            severity: meta_ast::Severity::Warning,
            message: "broken symbol".to_string(),
            source_range: Some(meta_ast::model::SourceRange {
                byte_start: 1,
                byte_end: 10,
                start: meta_ast::model::LineColumn { line: 0, column: 1 },
                end: meta_ast::model::LineColumn {
                    line: 0,
                    column: 10,
                },
            }),
        };

        let utf8 = diagnostic_to_lsp(Some(&SourceFile::new(TEXT)), &diagnostic, Encoding::Utf8);
        assert_eq!(
            utf8.range,
            Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 1
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 10
                },
            }
        );

        let utf16 = diagnostic_to_lsp(Some(&SourceFile::new(TEXT)), &diagnostic, Encoding::Utf16);
        assert_eq!(
            utf16.range,
            Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 1
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5
                },
            }
        );
        assert_eq!(utf16.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(utf16.source.as_deref(), Some("meta-ast"));
        assert_eq!(utf16.message, "broken symbol");
    }

    #[test]
    fn uri_round_trip() {
        let path = std::env::temp_dir().join("poc_sample.py");
        let uri = path_to_uri(&path).unwrap();
        let doc = DocUri::try_from(&uri).unwrap();
        assert_eq!(doc.to_path().unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn path_to_uri_accepts_verbatim_paths() {
        let path = std::env::temp_dir().join("poc_sample.py");
        let verbatim = PathBuf::from(format!(r"\\?\{}", path.display()));
        assert!(path_to_uri(&verbatim).is_some());
    }

    #[test]
    fn kind_pair_keeps_the_object_mapping() {
        assert_eq!(
            symbol_kind(meta_ast::SymbolKind::Object),
            SymbolKind::OBJECT
        );
        assert_eq!(
            completion_kind(meta_ast::SymbolKind::Object),
            CompletionItemKind::MODULE,
            "LSP has no OBJECT completion kind"
        );
        assert_eq!(
            symbol_kind(meta_ast::SymbolKind::Namespace),
            SymbolKind::NAMESPACE
        );
        assert_eq!(
            completion_kind(meta_ast::SymbolKind::Namespace),
            CompletionItemKind::MODULE
        );
    }

    #[test]
    fn kind_word_covers_the_kind_pair_table() {
        assert_eq!(kind_word(meta_ast::SymbolKind::Function), "function");
        assert_eq!(kind_word(meta_ast::SymbolKind::Object), "object");
        assert_eq!(kind_word(meta_ast::SymbolKind::Namespace), "namespace");
        assert_eq!(kind_word(meta_ast::SymbolKind::TypeAlias), "type alias");
    }

    #[test]
    fn rangeless_diagnostic_points_at_the_file_head() {
        let diagnostic = meta_ast::Diagnostic {
            path: PathBuf::from("/tmp/sample.py"),
            severity: meta_ast::Severity::Error,
            message: "file level".to_string(),
            source_range: None,
        };

        let converted = diagnostic_to_lsp(None, &diagnostic, Encoding::Utf16);

        assert_eq!(
            converted.range,
            Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1
                },
            }
        );
        assert_eq!(converted.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(converted.message, "file level");
    }
}
