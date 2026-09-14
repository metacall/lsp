//! Capability negotiation and dynamic registrations.
use anyhow::Context;
use lsp_server::{Connection, Message, Request as WireRequest};
use lsp_types::notification::DidChangeWatchedFiles;
use lsp_types::notification::Notification as _;
use lsp_types::request::{RegisterCapability, Request as _};
use lsp_types::{
    ClientCapabilities, CompletionOptions, DiagnosticOptions, DiagnosticServerCapabilities,
    FileSystemWatcher, GlobPattern, HoverProviderCapability, OneOf, Registration,
    RegistrationParams, SaveOptions, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
    WorkDoneProgressOptions,
};

use crate::position::Encoding;
use crate::server::ids::{WATCHED_FILES_REGISTRATION, watched_files_request_id};
use crate::types::{DocUri, RootDir};

/// Files whose contents shape engine resolver state for the process lifetime.
const RESOLVER_CONFIGS: [&str; 5] = [
    "tsconfig.json",
    "jsconfig.json",
    "go.mod",
    "pyproject.toml",
    "package.json",
];

pub(crate) fn capabilities(encoding: Encoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.as_lsp()),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string(), ":".to_string(), "/".to_string()]),
            ..Default::default()
        }),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("meta-ast".to_string()),
            inter_file_dependencies: false,
            workspace_diagnostics: false,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        ..Default::default()
    }
}

/// Pull only: a client without `textDocument.diagnostic` cannot ask, so initialize refuses instead of pushing.
pub(crate) fn supports_pull_diagnostics(caps: &ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|text_document| text_document.diagnostic.as_ref())
        .is_some()
}

/// Dynamic registration is the only portable way to receive external changes.
pub(crate) fn supports_watched_files(caps: &ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|workspace| workspace.did_change_watched_files.as_ref())
        .and_then(|watched| watched.dynamic_registration)
        .unwrap_or(false)
}

/// Glob patterns for every parseable extension, plus the resolver config files.
pub(crate) fn watched_globs() -> Vec<String> {
    let mut globs = Vec::new();
    for lang in meta_ast::LangId::all() {
        for extension in meta_ast::language::spec_for(lang).extensions {
            globs.push(format!("**/*.{extension}"));
        }
    }
    for name in RESOLVER_CONFIGS {
        globs.push(format!("**/{name}"));
    }
    globs.sort();
    globs.dedup();
    globs
}

pub(crate) fn is_resolver_config(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| RESOLVER_CONFIGS.contains(&name))
}

pub(crate) fn register_watched_files(connection: &Connection) -> anyhow::Result<()> {
    let params = watched_files_registration()?;
    let request = WireRequest {
        id: watched_files_request_id(),
        method: RegisterCapability::METHOD.to_string(),
        params: serde_json::to_value(params)?,
    };
    connection
        .sender
        .send(Message::Request(request))
        .context("send registerCapability")?;
    Ok(())
}

fn watched_files_registration() -> anyhow::Result<RegistrationParams> {
    let watchers = watched_globs()
        .into_iter()
        .map(|glob| FileSystemWatcher {
            glob_pattern: GlobPattern::String(glob),
            kind: None,
        })
        .collect();
    let options = lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers };
    Ok(RegistrationParams {
        registrations: vec![Registration {
            id: WATCHED_FILES_REGISTRATION.to_string(),
            method: DidChangeWatchedFiles::METHOD.to_string(),
            register_options: Some(serde_json::to_value(options)?),
        }],
    })
}

/// The root comes from `workspaceFolders` only: the first folder wins, must exist as a directory; the process CWD is never used.
pub(crate) fn root_from_params(params: &lsp_types::InitializeParams) -> anyhow::Result<RootDir> {
    let Some(folders) = &params.workspace_folders else {
        anyhow::bail!(
            "client did not send workspaceFolders; metacall-lsp requires one workspace folder"
        );
    };
    if folders.len() > 1 {
        tracing::warn!(
            count = folders.len(),
            "more than one workspace folder; serving the first"
        );
    }
    for folder in folders {
        let path = DocUri::try_from(&folder.uri)
            .ok()
            .and_then(|uri| uri.to_path());
        match path {
            Some(path) => return RootDir::try_from(path.as_path()),
            None => {
                tracing::warn!(uri = %folder.uri.as_str(), "workspace folder is not a file URI")
            }
        }
    }
    anyhow::bail!("no workspace folder resolves to a file path")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert;

    #[test]
    fn watched_globs_cover_supported_extensions() {
        let globs = watched_globs();
        assert!(globs.iter().any(|glob| glob == "**/*.py"));
        assert!(globs.iter().any(|glob| glob == "**/*.ts"));
    }

    #[test]
    fn watched_files_registration_is_well_formed() {
        let params = watched_files_registration().unwrap();
        let value = serde_json::to_value(&params).unwrap();
        let registration = &value["registrations"][0];
        assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
        let watchers = registration["registerOptions"]["watchers"]
            .as_array()
            .unwrap();
        assert!(watchers.len() > 1);
    }

    #[test]
    fn watched_globs_cover_resolver_configs() {
        let globs = watched_globs();
        assert!(globs.iter().any(|glob| glob == "**/tsconfig.json"));
        assert!(globs.iter().any(|glob| glob == "**/go.mod"));
    }

    #[test]
    fn a_missing_workspace_folder_is_an_error() {
        let params = lsp_types::InitializeParams::default();

        let error = root_from_params(&params).unwrap_err();

        assert!(error.to_string().contains("workspaceFolders"));
    }

    #[test]
    fn the_first_usable_workspace_folder_wins() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let params = lsp_types::InitializeParams {
            workspace_folders: Some(vec![
                lsp_types::WorkspaceFolder {
                    uri: convert::path_to_uri(first.path()).unwrap(),
                    name: "first".to_string(),
                },
                lsp_types::WorkspaceFolder {
                    uri: convert::path_to_uri(second.path()).unwrap(),
                    name: "second".to_string(),
                },
            ]),
            ..Default::default()
        };

        assert_eq!(root_from_params(&params).unwrap().as_path(), first.path());
    }

    #[test]
    fn a_nonexistent_workspace_folder_is_rejected() {
        let missing = tempfile::tempdir().unwrap();
        let path = missing.path().join("gone");
        std::fs::create_dir(&path).unwrap();
        std::fs::remove_dir(&path).unwrap();
        let params = lsp_types::InitializeParams {
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: convert::path_to_uri(&path).unwrap(),
                name: "missing".to_string(),
            }]),
            ..Default::default()
        };

        let error = root_from_params(&params).unwrap_err();

        assert!(error.to_string().contains("not an existing directory"));
    }

    #[test]
    fn a_workspace_folder_that_is_a_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def greet(): pass\n").unwrap();
        let params = lsp_types::InitializeParams {
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: convert::path_to_uri(&file).unwrap(),
                name: "file".to_string(),
            }]),
            ..Default::default()
        };

        let error = root_from_params(&params).unwrap_err();

        assert!(
            error.to_string().contains("not an existing directory"),
            "a file is not a workspace root, and the parent is never guessed: {error}"
        );
    }

    #[test]
    fn a_non_file_workspace_folder_is_rejected() {
        let params = lsp_types::InitializeParams {
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: "untitled:Untitled-1".parse().unwrap(),
                name: "draft".to_string(),
            }]),
            ..Default::default()
        };

        assert!(root_from_params(&params).is_err());
    }

    #[test]
    fn pull_diagnostics_require_the_client_capability() {
        let mut caps = ClientCapabilities::default();
        assert!(!supports_pull_diagnostics(&caps));

        caps.text_document = Some(lsp_types::TextDocumentClientCapabilities {
            diagnostic: Some(lsp_types::DiagnosticClientCapabilities::default()),
            ..Default::default()
        });
        assert!(supports_pull_diagnostics(&caps));
    }

    #[test]
    fn capabilities_advertise_pull_diagnostics() {
        let caps = capabilities(Encoding::Utf16);
        let Some(DiagnosticServerCapabilities::Options(options)) = caps.diagnostic_provider else {
            panic!("diagnostic provider must be advertised");
        };
        assert_eq!(options.identifier.as_deref(), Some("meta-ast"));
        assert!(!options.inter_file_dependencies);
        assert!(!options.workspace_diagnostics);
    }
}
