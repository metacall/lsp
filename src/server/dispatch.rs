//! Request dispatch and notification handling.
use std::path::Path;

use lsp_server::{
    Connection, Message, Notification as WireNotification, Request as WireRequest, RequestId,
    Response,
};
use lsp_types::notification::Notification as _;
use lsp_types::notification::{
    Cancel, DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
};
use lsp_types::request::{
    Completion, DocumentDiagnosticRequest, DocumentSymbolRequest, GotoDefinition, HoverRequest,
    References, Request as _, WorkspaceSymbolRequest,
};
use lsp_types::{
    CancelParams, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentDiagnosticReportResult, DocumentSymbolParams, DocumentSymbolResponse,
    FullDocumentDiagnosticReport, GotoDefinitionParams, HoverParams, MessageType, NumberOrString,
    ReferenceParams, RelatedFullDocumentDiagnosticReport, RelatedUnchangedDocumentDiagnosticReport,
    ShowMessageParams, UnchangedDocumentDiagnosticReport, WorkspaceSymbolParams,
    WorkspaceSymbolResponse, notification,
};
use serde::de::DeserializeOwned;

use crate::buffers::OpenOutcome;
use crate::error::ServerError;
use crate::handlers::{self, QueryCtx};
use crate::server::capabilities::is_resolver_config;
use crate::server::session::Session;
use crate::types::{DocUri, DocVersion};

pub(crate) fn handle_request(
    connection: &Connection,
    cancel: &crate::cancel::Cancellation,
    session: &Session,
    request: WireRequest,
) {
    let WireRequest { id, method, params } = request;
    let registration = cancel.register(&id);
    let outcome = if registration.is_cancelled() {
        Err(ServerError::Cancelled)
    } else {
        dispatch_request(session, &method, params)
    };
    drop(registration);
    match outcome {
        Ok(value) => respond(connection, id, value),
        Err(error) => send_response_error(connection, id, &error),
    }
}

fn dispatch_request(
    session: &Session,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ServerError> {
    if method == DocumentSymbolRequest::METHOD {
        let params: DocumentSymbolParams = parse_params(params)?;
        let uri = DocUri::try_from(&params.text_document.uri)?;
        query_document(session, &uri, |ctx| {
            DocumentSymbolResponse::Nested(handlers::document_symbols(ctx, &uri))
        })
    } else if method == HoverRequest::METHOD {
        let params: HoverParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::try_from(&params.text_document_position_params.text_document.uri)?;
        query_document(session, &uri, |ctx| handlers::hover_at(ctx, &uri, position))
    } else if method == GotoDefinition::METHOD {
        let params: GotoDefinitionParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::try_from(&params.text_document_position_params.text_document.uri)?;
        query_document(session, &uri, |ctx| {
            handlers::definition_at(ctx, &uri, position, session.definition_links)
        })
    } else if method == References::METHOD {
        let params: ReferenceParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::try_from(&params.text_document_position.text_document.uri)?;
        query_document(session, &uri, |ctx| {
            handlers::references_at(ctx, &uri, position, params.context.include_declaration)
        })
    } else if method == WorkspaceSymbolRequest::METHOD {
        let params: WorkspaceSymbolParams = parse_params(params)?;
        let snapshot = session.ready()?;
        let mut ctx = QueryCtx::new(snapshot, session, session.encoding);
        let symbols = handlers::workspace_symbols(&mut ctx, params.query.as_str());
        to_value(WorkspaceSymbolResponse::Nested(symbols))
    } else if method == Completion::METHOD {
        let params: CompletionParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::try_from(&params.text_document_position.text_document.uri)?;
        query_document(session, &uri, |ctx| {
            CompletionResponse::Array(handlers::completion_at(ctx, &uri, position))
        })
    } else if method == DocumentDiagnosticRequest::METHOD {
        let params: DocumentDiagnosticParams = parse_params(params)?;
        let uri = DocUri::try_from(&params.text_document.uri)?;
        let snapshot = session.ready_for(&uri)?;
        let result_id = diagnostic_result_id(snapshot, &uri);
        if params.previous_result_id.as_deref() == Some(result_id.as_str()) {
            return to_value(DocumentDiagnosticReportResult::Report(
                DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
                    related_documents: None,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                        result_id,
                    },
                }),
            ));
        }
        let mut ctx = QueryCtx::new(snapshot, session, session.encoding);
        let items = handlers::diagnostics_for(&mut ctx, &uri);
        to_value(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: Some(result_id),
                    items,
                },
            }),
        ))
    } else {
        Err(ServerError::MethodNotFound(method.to_string()))
    }
}

/// Run one document query; the index must describe the version the client sent.
fn query_document<'a, T: serde::Serialize>(
    session: &'a Session,
    uri: &DocUri,
    handle: impl FnOnce(&mut QueryCtx<'a>) -> T,
) -> Result<serde_json::Value, ServerError> {
    let snapshot = session.ready_for(uri)?;
    let mut ctx = QueryCtx::new(snapshot, session, session.encoding);
    to_value(handle(&mut ctx))
}

/// Result id of one pull report: generation, plus the version for an open document.
fn diagnostic_result_id(snapshot: &crate::index::IndexSnapshot, uri: &DocUri) -> String {
    let version = uri
        .to_path()
        .and_then(|path| snapshot.document_version(&path));
    match version {
        Some(version) => format!("meta-ast:{}:{}", snapshot.generation(), version.get()),
        None => format!("meta-ast:{}", snapshot.generation()),
    }
}

fn parse_params<T: DeserializeOwned>(params: serde_json::Value) -> Result<T, ServerError> {
    serde_json::from_value(params).map_err(|error| ServerError::InvalidParams(error.to_string()))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<serde_json::Value, ServerError> {
    serde_json::to_value(value).map_err(|error| ServerError::Internal(error.to_string()))
}

fn notification_params<T: DeserializeOwned>(method: &str, params: serde_json::Value) -> Option<T> {
    match serde_json::from_value(params) {
        Ok(params) => Some(params),
        Err(error) => {
            tracing::warn!(%error, %method, "bad notification params");
            None
        }
    }
}

fn send(connection: &Connection, message: Message) {
    if let Err(error) = connection.sender.send(message) {
        tracing::warn!(%error, "send failed");
    }
}

fn respond(connection: &Connection, id: RequestId, value: serde_json::Value) {
    send(
        connection,
        Message::Response(Response {
            id,
            response_result: Ok(value),
        }),
    );
}

pub(crate) fn send_response_error(connection: &Connection, id: RequestId, error: &ServerError) {
    send(connection, Message::Response(error.to_response(id)));
}

/// Handle one notification: no response; anything unactionable is logged and dropped.
pub(crate) fn handle_notification(
    connection: &Connection,
    session: &mut Session,
    cancel: &crate::cancel::Cancellation,
    notification: WireNotification,
) {
    let WireNotification { method, params } = notification;
    if method == DidOpenTextDocument::METHOD {
        let Some(params) = notification_params::<DidOpenTextDocumentParams>(&method, params) else {
            return;
        };
        let Ok(uri) = DocUri::try_from(&params.text_document.uri) else {
            tracing::warn!("didOpen with an unparsable document URI");
            return;
        };
        if session.buffers.open(
            &uri,
            DocVersion::from(params.text_document.version),
            params.text_document.language_id.as_str(),
            params.text_document.text,
        ) == OpenOutcome::Indexed
        {
            // Untitled and out-of-root buffers never enter the index, so no pass.
            if session.indexable(&uri) {
                session.mark_dirty();
            }
        } else {
            tracing::debug!(%uri, "didOpen ignored for unsupported language");
        }
    } else if method == DidChangeTextDocument::METHOD {
        let Some(params) = notification_params::<DidChangeTextDocumentParams>(&method, params)
        else {
            return;
        };
        let Ok(uri) = DocUri::try_from(&params.text_document.uri) else {
            tracing::warn!("didChange with an unparsable document URI");
            return;
        };
        if session.buffers.change(
            &uri,
            DocVersion::from(params.text_document.version),
            &params.content_changes,
            session.encoding,
        ) && session.indexable(&uri)
        {
            session.mark_dirty();
        }
    } else if method == DidCloseTextDocument::METHOD {
        let Some(params) = notification_params::<DidCloseTextDocumentParams>(&method, params)
        else {
            return;
        };
        let Ok(uri) = DocUri::try_from(&params.text_document.uri) else {
            tracing::warn!("didClose with an unparsable document URI");
            return;
        };
        if session.buffers.close(&uri) && session.indexable(&uri) {
            session.mark_dirty();
        }
    } else if method == DidSaveTextDocument::METHOD {
        let Some(params) = notification_params::<DidSaveTextDocumentParams>(&method, params) else {
            return;
        };
        let Ok(uri) = DocUri::try_from(&params.text_document.uri) else {
            tracing::warn!("didSave with an unparsable document URI");
            return;
        };
        session.on_save(&uri, params.text.as_deref());
    } else if method == Cancel::METHOD {
        let Some(params) = notification_params::<CancelParams>(&method, params) else {
            return;
        };
        let id = match params.id {
            NumberOrString::Number(number) => RequestId::from(number),
            NumberOrString::String(value) => RequestId::from(value),
        };
        cancel.cancel(&id);
    } else if method == notification::DidChangeWatchedFiles::METHOD {
        let Some(params) = notification_params::<DidChangeWatchedFilesParams>(&method, params)
        else {
            return;
        };
        handle_watched_files(connection, session, &params);
    }
}

/// Reindex on source changes. Warn once per resolver config change.
fn handle_watched_files(
    connection: &Connection,
    session: &mut Session,
    params: &DidChangeWatchedFilesParams,
) {
    let mut source_changed = false;
    for event in &params.changes {
        let Some(path) = DocUri::try_from(event.uri.as_str())
            .ok()
            .and_then(|uri| uri.to_path())
        else {
            continue;
        };
        if !session.root.contains(&path) {
            continue;
        }
        if meta_ast::detect_language(&path).is_some() {
            // The worker compares fingerprints; the event only marks the workspace dirty.
            source_changed = true;
        } else if is_resolver_config(&path) && resolver_unwarned(session, &path) {
            warn_resolver_change(connection, &path);
        }
    }
    if source_changed {
        session.mark_dirty();
    }
}

/// True when this config kind is unwarned this session; keyed by name, so directories share one warning.
fn resolver_unwarned(session: &mut Session, path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    session.warned_resolvers.insert(name.to_string())
}

/// The engine caches resolver state for the process lifetime; a changed config needs a restart.
fn warn_resolver_change(connection: &Connection, path: &Path) {
    let params = ShowMessageParams {
        typ: MessageType::WARNING,
        message: format!(
            "{} changed. Restart the server to apply the new resolver configuration.",
            path.display()
        ),
    };
    send(
        connection,
        Message::Notification(WireNotification::new(
            "window/showMessage".to_string(),
            params,
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::Cancellation;
    use crate::index;
    use crate::position::Encoding;
    use crate::reindex::ReindexReq;
    use crate::server::session::Session;

    use crate::testutil::doc_uri;

    fn did_open(uri: &str, language_id: &str) -> WireNotification {
        WireNotification {
            method: DidOpenTextDocument::METHOD.to_string(),
            params: serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": "x = 1\n",
                }
            }),
        }
    }

    #[test]
    fn the_pull_diagnostic_id_names_generation_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.py");
        std::fs::write(&path, "x = 1\n").unwrap();
        let uri = crate::convert::path_to_uri(&path).unwrap().to_string();
        let overlay = meta_ast::Overlay {
            uri: uri.clone(),
            path,
            text: "x = 1\n".to_string(),
            version: 1,
            lang: meta_ast::LangId::Python,
        };
        let snapshot =
            index::rebuild_from_inputs(dir.path(), std::slice::from_ref(&overlay)).unwrap();
        let (req_tx, _req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_open(uri.as_str(), "python"),
        );

        let snapshot = session.ready().unwrap();
        let open_id = diagnostic_result_id(snapshot, &doc_uri(&uri));
        assert_eq!(
            open_id,
            format!("meta-ast:{}:1", snapshot.generation()),
            "an open document carries the version that produced the report"
        );
        let other = crate::convert::path_to_uri(&dir.path().join("missing.py"))
            .unwrap()
            .to_string();
        assert_eq!(
            diagnostic_result_id(snapshot, &doc_uri(&other)),
            format!("meta-ast:{}", snapshot.generation()),
            "a document with no version carries the generation alone"
        );

        let echoed = dispatch_request(
            &session,
            DocumentDiagnosticRequest::METHOD,
            serde_json::json!({
                "textDocument": { "uri": uri },
                "previousResultId": open_id,
            }),
        )
        .unwrap();
        assert_eq!(echoed["kind"], "unchanged");
        assert_eq!(echoed["resultId"], open_id, "an echo is answered unchanged");

        let full = dispatch_request(
            &session,
            DocumentDiagnosticRequest::METHOD,
            serde_json::json!({ "textDocument": { "uri": uri } }),
        )
        .unwrap();
        assert_eq!(full["kind"], "full");
        assert_eq!(full["resultId"].as_str(), Some(open_id.as_str()));
    }

    fn did_close(uri: &str) -> WireNotification {
        WireNotification {
            method: DidCloseTextDocument::METHOD.to_string(),
            params: serde_json::json!({
                "textDocument": { "uri": uri }
            }),
        }
    }

    #[test]
    fn did_open_unknown_language_skips_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();

        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_open("file:///notes.txt", "plaintext"),
        );

        assert!(session.buffers.get(&doc_uri("file:///notes.txt")).is_none());
        assert!(req_rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn did_open_supported_language_requests_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();

        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_open(uri.as_str(), "python"),
        );

        assert!(session.buffers.get(&doc_uri(uri.as_str())).is_some());
        session.flush_batch();
        assert!(req_rx.try_recv().is_ok(), "reindex expected");
    }

    #[test]
    fn did_close_drops_only_tracked_documents() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, _req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();

        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_open("file:///a.py", "python"),
        );
        handle_notification(&server, &mut session, &cancel, did_close("file:///a.py"));
        assert!(session.buffers.get(&doc_uri("file:///a.py")).is_none());

        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_close("file:///notes.txt"),
        );
        assert!(session.buffers.get(&doc_uri("file:///notes.txt")).is_none());
    }

    fn watched(uri: &str) -> WireNotification {
        WireNotification {
            method: notification::DidChangeWatchedFiles::METHOD.to_string(),
            params: serde_json::json!({
                "changes": [ { "uri": uri, "type": 2 } ]
            }),
        }
    }

    #[test]
    fn watched_source_event_requests_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def greet(): pass\n").unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();

        // Editors echo their own saves here; the worker drops the pass when nothing changed.
        handle_notification(&server, &mut session, &cancel, watched(uri.as_str()));
        session.flush_batch();

        assert!(
            req_rx.try_recv().is_ok(),
            "a watched source event must request a pass"
        );
    }

    #[test]
    fn watched_deletion_of_indexed_file_reindexes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def greet(): pass\n").unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();

        std::fs::remove_file(dir.path().join("a.py")).unwrap();
        handle_notification(&server, &mut session, &cancel, watched(uri.as_str()));
        session.flush_batch();

        assert!(
            req_rx.try_recv().is_ok(),
            "a deletion must trigger a reindex"
        );
    }

    #[test]
    fn watched_change_outside_root_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();

        handle_notification(
            &server,
            &mut session,
            &cancel,
            watched("file:///elsewhere/a.py"),
        );

        assert!(req_rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn watched_change_for_unsupported_extension_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("README.md")).unwrap();

        handle_notification(&server, &mut session, &cancel, watched(uri.as_str()));

        assert!(req_rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn watched_resolver_change_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("tsconfig.json")).unwrap();

        handle_notification(&server, &mut session, &cancel, watched(uri.as_str()));
        assert!(
            req_rx.try_recv().is_err(),
            "resolver change must not reindex"
        );

        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("resolver warning");
        let Message::Notification(notification) = message else {
            panic!("expected a warning notification");
        };
        assert_eq!(notification.method, "window/showMessage");
        let params: ShowMessageParams = serde_json::from_value(notification.params).unwrap();
        assert_eq!(params.typ, MessageType::WARNING);
        assert!(params.message.contains("tsconfig.json"));

        handle_notification(&server, &mut session, &cancel, watched(uri.as_str()));
        assert!(
            client
                .receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "second change must not warn again"
        );

        let nested = crate::convert::path_to_uri(&dir.path().join("sub/tsconfig.json")).unwrap();
        handle_notification(&server, &mut session, &cancel, watched(nested.as_str()));
        assert!(
            client
                .receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "a second config path of the same kind must not warn again"
        );

        let gomod = crate::convert::path_to_uri(&dir.path().join("go.mod")).unwrap();
        handle_notification(&server, &mut session, &cancel, watched(gomod.as_str()));
        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("second config kind warning");
        let Message::Notification(notification) = message else {
            panic!("expected a warning notification");
        };
        let params: ShowMessageParams = serde_json::from_value(notification.params).unwrap();
        assert!(params.message.contains("go.mod"));
    }

    #[test]
    fn a_cancelled_in_flight_request_returns_request_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, _req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, client) = Connection::memory();
        let cancel = Cancellation::default();
        let id = RequestId::from(2);
        // The guard keeps the request in flight while the cancel arrives.
        let in_flight = cancel.register(&id);
        cancel.cancel(&id);

        handle_request(
            &server,
            &cancel,
            &session,
            WireRequest {
                id: id.clone(),
                method: DocumentSymbolRequest::METHOD.to_string(),
                params: serde_json::Value::Null,
            },
        );

        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("cancel response");
        let Message::Response(response) = message else {
            panic!("expected a cancelled response");
        };
        assert_eq!(response.id, id);
        let Err(error) = response.response_result else {
            panic!("expected a cancelled response");
        };
        assert_eq!(error.code, -32800);
        assert_eq!(error.message, "request cancelled");
        drop(in_flight);
        assert!(cancel.is_empty());
    }

    #[test]
    fn a_cancel_for_an_unknown_id_leaves_no_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def greet(): pass\n").unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, _req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, client) = Connection::memory();
        let cancel = Cancellation::default();
        let id = RequestId::from(77);
        let uri = crate::convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();

        handle_notification(
            &server,
            &mut session,
            &cancel,
            WireNotification {
                method: Cancel::METHOD.to_string(),
                params: serde_json::json!({ "id": 77 }),
            },
        );

        assert!(cancel.is_empty(), "an unknown id must not be remembered");

        handle_request(
            &server,
            &cancel,
            &session,
            WireRequest {
                id: id.clone(),
                method: DocumentSymbolRequest::METHOD.to_string(),
                params: serde_json::json!({"textDocument": {"uri": uri}}),
            },
        );

        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("documentSymbol response");
        let Message::Response(response) = message else {
            panic!("expected a response");
        };
        assert_eq!(response.id, id);
        assert!(
            response.response_result.is_ok(),
            "an unknown-id cancel must not affect the request"
        );
    }

    fn did_save(uri: &str, text: Option<&str>) -> WireNotification {
        let mut params = serde_json::json!({ "textDocument": { "uri": uri } });
        if let Some(text) = text {
            params["text"] = serde_json::json!(text);
        }
        WireNotification {
            method: DidSaveTextDocument::METHOD.to_string(),
            params,
        }
    }

    #[test]
    fn did_save_with_changed_text_reindexes_now() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();
        session.buffers.open(
            &doc_uri(uri.as_str()),
            DocVersion::from(1),
            "python",
            "def one(): pass\n".to_string(),
        );

        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_save(uri.as_str(), Some("def two(): pass\n")),
        );
        session.flush_batch();

        let req = req_rx
            .try_recv()
            .expect("save marks the workspace dirty and the batch sends");
        assert_eq!(req.overlays.len(), 1);
        assert_eq!(req.overlays[0].text, "def two(): pass\n");
    }

    #[test]
    fn did_save_carries_the_newest_buffer_into_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let mut session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, _client) = Connection::memory();
        let cancel = Cancellation::default();
        let uri = crate::convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();

        session.buffers.open(
            &doc_uri(uri.as_str()),
            DocVersion::from(1),
            "python",
            "def one(): pass\n".to_string(),
        );
        let content = serde_json::json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [ { "text": "def two(): pass\n" } ]
        });
        let params: DidChangeTextDocumentParams = serde_json::from_value(content).unwrap();
        assert!(session.buffers.change(
            &doc_uri(uri.as_str()),
            DocVersion::from(params.text_document.version),
            &params.content_changes,
            session.encoding,
        ));
        session.mark_dirty();

        handle_notification(
            &server,
            &mut session,
            &cancel,
            did_save(uri.as_str(), Some("def two(): pass\n")),
        );
        session.flush_batch();

        let req = req_rx.try_recv().expect("the batch sends one request");
        assert_eq!(req.overlays[0].text, "def two(): pass\n");
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = index::rebuild_from_inputs(dir.path(), &[]).unwrap();
        let (req_tx, _req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        let session = Session::new(
            crate::types::RootDir::try_from(dir.path()).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            false,
        );
        let (server, client) = Connection::memory();
        let cancel = Cancellation::default();

        handle_request(
            &server,
            &cancel,
            &session,
            WireRequest {
                id: RequestId::from(9),
                method: "no/suchMethod".to_string(),
                params: serde_json::Value::Null,
            },
        );

        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("error response");
        let Message::Response(response) = message else {
            panic!("expected an error response");
        };
        let Err(error) = response.response_result else {
            panic!("expected an error response");
        };
        assert_eq!(error.code, -32601);
    }
}
