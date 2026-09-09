//! Type maps between meta-ast and LSP.
use std::path::{Path, PathBuf};

use lsp_types::{CompletionItemKind, DiagnosticSeverity, Range, SymbolKind, Uri};

use crate::position::{self, Encoding};

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let parsed = url::Url::parse(uri).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    parsed.to_file_path().ok()
}

pub fn path_to_uri(path: &Path) -> Option<Uri> {
    url::Url::from_file_path(path).ok()?.as_str().parse().ok()
}

pub fn symbol_kind(kind: meta_ast::SymbolKind) -> SymbolKind {
    match kind {
        meta_ast::SymbolKind::Function => SymbolKind::FUNCTION,
        meta_ast::SymbolKind::Method => SymbolKind::METHOD,
        meta_ast::SymbolKind::Class => SymbolKind::CLASS,
        meta_ast::SymbolKind::Struct => SymbolKind::STRUCT,
        meta_ast::SymbolKind::Interface | meta_ast::SymbolKind::Trait => SymbolKind::INTERFACE,
        meta_ast::SymbolKind::Enum => SymbolKind::ENUM,
        meta_ast::SymbolKind::Object => SymbolKind::OBJECT,
        meta_ast::SymbolKind::Constant => SymbolKind::CONSTANT,
        meta_ast::SymbolKind::Static => SymbolKind::VARIABLE,
        meta_ast::SymbolKind::Module => SymbolKind::MODULE,
        meta_ast::SymbolKind::Namespace => SymbolKind::NAMESPACE,
        meta_ast::SymbolKind::TypeAlias => SymbolKind::TYPE_PARAMETER,
        _ => SymbolKind::VARIABLE,
    }
}

pub fn completion_kind(kind: meta_ast::SymbolKind) -> CompletionItemKind {
    match kind {
        meta_ast::SymbolKind::Function => CompletionItemKind::FUNCTION,
        meta_ast::SymbolKind::Method => CompletionItemKind::METHOD,
        meta_ast::SymbolKind::Class => CompletionItemKind::CLASS,
        meta_ast::SymbolKind::Struct => CompletionItemKind::STRUCT,
        meta_ast::SymbolKind::Interface | meta_ast::SymbolKind::Trait => {
            CompletionItemKind::INTERFACE
        }
        meta_ast::SymbolKind::Enum => CompletionItemKind::ENUM,
        meta_ast::SymbolKind::Object => CompletionItemKind::MODULE,
        meta_ast::SymbolKind::Constant => CompletionItemKind::CONSTANT,
        meta_ast::SymbolKind::Module | meta_ast::SymbolKind::Namespace => {
            CompletionItemKind::MODULE
        }
        meta_ast::SymbolKind::TypeAlias => CompletionItemKind::TYPE_PARAMETER,
        _ => CompletionItemKind::VARIABLE,
    }
}

fn severity(severity: meta_ast::Severity) -> DiagnosticSeverity {
    match severity {
        meta_ast::Severity::Warning => DiagnosticSeverity::WARNING,
        meta_ast::Severity::Error => DiagnosticSeverity::ERROR,
        _ => DiagnosticSeverity::WARNING,
    }
}

pub fn diagnostic_to_lsp(
    text: Option<&str>,
    diagnostic: &meta_ast::Diagnostic,
    encoding: Encoding,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: diagnostic
            .source_range
            .as_ref()
            .map(|range| position::range(text, range, encoding))
            .unwrap_or(Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            }),
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

        let utf8 = diagnostic_to_lsp(Some(TEXT), &diagnostic, Encoding::Utf8);
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

        let utf16 = diagnostic_to_lsp(Some(TEXT), &diagnostic, Encoding::Utf16);
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
        let path = PathBuf::from("/tmp/poc_sample.py");
        let uri = path_to_uri(&path).unwrap();
        assert_eq!(uri_to_path(uri.as_str()).unwrap(), path);
    }

    #[test]
    fn uri_rejects_non_file_scheme() {
        assert!(uri_to_path("untitled:buffer.py").is_none());
    }
}
