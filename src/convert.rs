//! Type maps between meta-ast and LSP.
use std::path::{Path, PathBuf};

use lsp_types::{DiagnosticSeverity, Position, Range, SymbolKind, Uri};

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

pub fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

pub fn range_to_lsp(range: &meta_ast::model::SourceRange) -> Range {
    Range {
        start: Position {
            line: to_u32(range.start.line),
            character: to_u32(range.start.column),
        },
        end: Position {
            line: to_u32(range.end.line),
            character: to_u32(range.end.column),
        },
    }
}

pub fn contains(range: &meta_ast::model::SourceRange, pos: Position) -> bool {
    let start = (range.start.line, range.start.column);
    let point = (pos.line as usize, pos.character as usize);
    let end = (range.end.line, range.end.column);
    start <= point && point <= end
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

pub fn severity(severity: meta_ast::Severity) -> DiagnosticSeverity {
    match severity {
        meta_ast::Severity::Warning => DiagnosticSeverity::WARNING,
        meta_ast::Severity::Error => DiagnosticSeverity::ERROR,
        _ => DiagnosticSeverity::WARNING,
    }
}

pub fn diagnostic_to_lsp(diagnostic: &meta_ast::Diagnostic) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: diagnostic
            .source_range
            .as_ref()
            .map(range_to_lsp)
            .unwrap_or(Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
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
    fn uri_round_trip() {
        let path = PathBuf::from("/tmp/poc_sample.py");
        let uri = path_to_uri(&path).unwrap();
        assert_eq!(uri_to_path(uri.as_str()).unwrap(), path);
    }

    #[test]
    fn uri_rejects_non_file_scheme() {
        assert!(uri_to_path("untitled:buffer.py").is_none());
    }

    #[test]
    fn contains_checks_bounds() {
        let range = meta_ast::model::SourceRange {
            byte_start: 0,
            byte_end: 9,
            start: meta_ast::model::LineColumn { line: 0, column: 4 },
            end: meta_ast::model::LineColumn { line: 0, column: 9 },
        };
        assert!(contains(
            &range,
            Position {
                line: 0,
                character: 5
            }
        ));
        assert!(!contains(
            &range,
            Position {
                line: 1,
                character: 5
            }
        ));
    }
}
