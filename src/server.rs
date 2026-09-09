//! Sync LSP loop.
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, select};
use lsp_server::{Connection, Message};
use lsp_server::{Notification as WireNotification, Request as WireRequest, RequestId, Response};
use lsp_types::notification::Notification as LspNotification;
use lsp_types::request::Request as LspRequest;
use lsp_types::{
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, FileSystemWatcher, GlobPattern, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, HoverProviderCapability, InitializeParams, OneOf,
    PublishDiagnosticsParams, Registration, RegistrationParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use lsp_types::{notification, request};

use crate::buffers::BufferStore;
use crate::convert;
use crate::error::ServerError;
use crate::handlers;
use crate::index::{self, IndexSnapshot, SourceText};
use crate::position::{self, Encoding};
use crate::reindex::{self, ReindexReq, ReindexResp};

const SERVER_NAME: &str = "meta-ast-lsp";

pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    meta_ast::language::validate_queries();
    let (connection, io_threads) = Connection::stdio();
    let (request_id, init_value) = connection
        .initialize_start()
        .context("wait for initialize")?;
    let params: InitializeParams =
        serde_json::from_value(init_value).context("parse initialize params")?;
    let root = root_from_params(&params)?;
    let encoding = position::negotiate(&params.capabilities);
    connection
        .initialize_finish(
            request_id,
            serde_json::json!({
                "capabilities": capabilities(encoding),
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .context("send initialize result")?;
    tracing::info!(root = %root.display(), ?encoding, "serving");
    if supports_watched_files(&params.capabilities) {
        if let Err(error) = register_watched_files(&connection) {
            tracing::warn!(%error, "watched files registration failed");
        }
    } else {
        tracing::info!(
            "client has no dynamic watched-file registration; external changes stay untracked"
        );
    }
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
    let worker = reindex::spawn_worker(req_rx, resp_tx);
    let mut state = State::new(root, req_tx, encoding)?;
    let mut shutdown = false;
    loop {
        if let Some(remaining) = state.pending_remaining() {
            select! {
                recv(connection.receiver) -> msg => {
                    let Ok(msg) = msg else { break };
                    match msg {
                        Message::Request(request) => {
                            if connection
                                .handle_shutdown(&request)
                                .context("answer shutdown")?
                            {
                                shutdown = true;
                                continue;
                            }
                            handle_request(&connection, &state, request);
                        }
                        Message::Notification(notification) => {
                            if notification.method == "exit" {
                                break;
                            }
                            if let Some(uri) = handle_notification(&mut state, notification)
                                && let Err(error) = publish_clear(&connection, uri.as_str())
                            {
                                tracing::warn!(%error, "clear failed");
                            }
                        }
                        Message::Response(_) => {}
                    }
                }
                recv(resp_rx) -> resp => {
                    let Ok(resp) = resp else {
                        tracing::warn!("reindex worker gone");
                        state.pending = None;
                        continue;
                    };
                    apply_resp(&connection, &mut state, resp);
                }
                default(remaining) => {
                    state.flush();
                    drain_resps(&connection, &mut state, &resp_rx);
                }
            }
        } else {
            select! {
                recv(connection.receiver) -> msg => {
                    let Ok(msg) = msg else { break };
                    match msg {
                        Message::Request(request) => {
                            if connection
                                .handle_shutdown(&request)
                                .context("answer shutdown")?
                            {
                                shutdown = true;
                                continue;
                            }
                            handle_request(&connection, &state, request);
                        }
                        Message::Notification(notification) => {
                            if notification.method == "exit" {
                                break;
                            }
                            if let Some(uri) = handle_notification(&mut state, notification)
                                && let Err(error) = publish_clear(&connection, uri.as_str())
                            {
                                tracing::warn!(%error, "clear failed");
                            }
                        }
                        Message::Response(_) => {}
                    }
                }
                recv(resp_rx) -> resp => {
                    let Ok(resp) = resp else {
                        tracing::warn!("reindex worker gone");
                        continue;
                    };
                    apply_resp(&connection, &mut state, resp);
                }
            }
        }
        drain_resps(&connection, &mut state, &resp_rx);
        if state.flush_due() {
            state.flush();
        }
    }
    drop(state.req_tx);
    let _ = worker.join();
    drop(connection);
    io_threads.join()?;
    if !shutdown {
        tracing::warn!("client exited without shutdown");
    }
    Ok(())
}

struct Pending {
    seq: u64,
    raw: u32,
    deadline: Instant,
}

struct State {
    root: PathBuf,
    buffers: BufferStore,
    snapshot: Arc<IndexSnapshot>,
    counter: u32,
    seq: u64,
    applied_seq: u64,
    req_tx: Sender<ReindexReq>,
    pending: Option<Pending>,
    encoding: Encoding,
}

impl State {
    fn new(root: PathBuf, req_tx: Sender<ReindexReq>, encoding: Encoding) -> anyhow::Result<Self> {
        // Build the first snapshot before the loop. Clients request document
        // symbols right after didOpen, so an empty initial snapshot is not usable.
        let buffers = BufferStore::default();
        let inputs = index::collect_inputs(&root, &buffers);
        let snapshot = Arc::new(index::rebuild_from_inputs(&root, &inputs, 1)?);
        Ok(Self {
            root,
            buffers,
            snapshot,
            counter: 1,
            seq: 0,
            applied_seq: 0,
            req_tx,
            pending: None,
            encoding,
        })
    }

    fn next_ids(&mut self) -> (u64, u32) {
        self.seq = self.seq.wrapping_add(1);
        self.counter = self.counter.wrapping_add(1);
        if self.counter == 0 {
            self.counter = 1;
        }
        (self.seq, self.counter)
    }

    fn send_req(&self, seq: u64, raw: u32) {
        let overlays = index::collect_inputs(&self.root, &self.buffers);
        let req = ReindexReq {
            seq,
            snapshot_raw: raw,
            root: self.root.clone(),
            overlays,
        };
        if self.req_tx.send(req).is_err() {
            tracing::warn!("reindex worker gone, keeping prior snapshot");
        }
    }

    fn schedule(&mut self, immediate: bool) {
        let (seq, raw) = self.next_ids();
        if immediate {
            self.pending = None;
            self.send_req(seq, raw);
        } else {
            self.pending = Some(Pending {
                seq,
                raw,
                deadline: Instant::now() + reindex::DEBOUNCE,
            });
        }
    }

    fn flush(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.send_req(pending.seq, pending.raw);
        }
    }

    fn flush_due(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
    }

    fn pending_remaining(&self) -> Option<std::time::Duration> {
        self.pending
            .as_ref()
            .map(|pending| pending.deadline.saturating_duration_since(Instant::now()))
    }
}

impl SourceText for State {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        if let Some(doc) = self.buffers.by_path(path) {
            return Some(Cow::Borrowed(doc.text.as_str()));
        }
        std::fs::read_to_string(path).ok().map(Cow::Owned)
    }
}

fn drain_resps(connection: &Connection, state: &mut State, resp_rx: &Receiver<ReindexResp>) {
    while let Ok(resp) = resp_rx.try_recv() {
        apply_resp(connection, state, resp);
    }
}

fn apply_resp(connection: &Connection, state: &mut State, resp: ReindexResp) {
    if resp.seq <= state.applied_seq {
        return;
    }
    state.applied_seq = resp.seq;
    match resp.result {
        Ok(snapshot) => {
            tracing::info!(
                seq = resp.seq,
                elapsed_ms = resp.elapsed_ms,
                "snapshot swap"
            );
            state.snapshot = Arc::new(snapshot);
            if let Err(error) = publish_all(connection, state) {
                tracing::warn!(%error, "publish failed");
            }
        }
        Err(error) => tracing::warn!(%error, "reindex failed, keeping prior snapshot"),
    }
}

fn capabilities(encoding: Encoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.as_lsp()),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        ..Default::default()
    }
}

/// Dynamic registration is the only portable way to receive external changes.
fn supports_watched_files(caps: &lsp_types::ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|workspace| workspace.did_change_watched_files.as_ref())
        .and_then(|watched| watched.dynamic_registration)
        .unwrap_or(false)
}

/// Glob patterns for every extension meta-ast can parse.
fn watched_globs() -> Vec<String> {
    let mut globs = Vec::new();
    for lang in meta_ast::LangId::all() {
        for extension in meta_ast::language::spec_for(lang).extensions {
            globs.push(format!("**/*.{extension}"));
        }
    }
    globs.sort();
    globs.dedup();
    globs
}

fn register_watched_files(connection: &Connection) -> anyhow::Result<()> {
    let params = watched_files_registration()?;
    let request = WireRequest {
        id: RequestId::from("meta-ast-lsp-register-watched-files".to_string()),
        method: request::RegisterCapability::METHOD.to_string(),
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
    let options = DidChangeWatchedFilesRegistrationOptions { watchers };
    Ok(RegistrationParams {
        registrations: vec![Registration {
            id: "meta-ast-lsp-watched-files".to_string(),
            method: notification::DidChangeWatchedFiles::METHOD.to_string(),
            register_options: Some(serde_json::to_value(options)?),
        }],
    })
}

fn root_from_params(params: &InitializeParams) -> anyhow::Result<PathBuf> {
    if let Some(folders) = &params.workspace_folders
        && let Some(folder) = folders.first()
        && let Some(path) = convert::uri_to_path(folder.uri.as_str())
    {
        return normalize_root(&path);
    }
    #[allow(deprecated)]
    if let Some(uri) = &params.root_uri
        && let Some(path) = convert::uri_to_path(uri.as_str())
    {
        return normalize_root(&path);
    }
    std::env::current_dir().context("current dir")
}

fn normalize_root(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_file() {
        path.parent()
            .map(Path::to_path_buf)
            .context("file has no parent")
    } else {
        Ok(path.to_path_buf())
    }
}

fn handle_request(connection: &Connection, state: &State, request: WireRequest) {
    let WireRequest { id, method, params } = request;
    if method == request::DocumentSymbolRequest::METHOD {
        match serde_json::from_value::<DocumentSymbolParams>(params) {
            Ok(params) => {
                let symbols = handlers::document_symbols(
                    &state.snapshot,
                    state,
                    params.text_document.uri.as_str(),
                    state.encoding,
                );
                respond_or_warn(connection, id, &DocumentSymbolResponse::Flat(symbols));
            }
            Err(error) => respond_protocol_error(connection, id, error.to_string()),
        }
    } else if method == request::HoverRequest::METHOD {
        match serde_json::from_value::<HoverParams>(params) {
            Ok(params) => {
                let position = params.text_document_position_params.position;
                let uri = params.text_document_position_params.text_document.uri;
                let hover = handlers::hover_at(
                    &state.snapshot,
                    state,
                    uri.as_str(),
                    position,
                    state.encoding,
                );
                respond_or_warn(connection, id, &hover);
            }
            Err(error) => respond_protocol_error(connection, id, error.to_string()),
        }
    } else if method == request::GotoDefinition::METHOD {
        match serde_json::from_value::<GotoDefinitionParams>(params) {
            Ok(params) => {
                let position = params.text_document_position_params.position;
                let uri = params.text_document_position_params.text_document.uri;
                let target = handlers::definition_at(
                    &state.snapshot,
                    state,
                    uri.as_str(),
                    position,
                    state.encoding,
                );
                let result: Option<GotoDefinitionResponse> =
                    target.map(GotoDefinitionResponse::Scalar);
                respond_or_warn(connection, id, &result);
            }
            Err(error) => respond_protocol_error(connection, id, error.to_string()),
        }
    } else {
        let error = ServerError::Protocol(format!("unknown method {method}"));
        if let Err(send) = connection
            .sender
            .send(Message::Response(error.to_response(id)))
        {
            tracing::warn!(%send, "send failed");
        }
    }
}

fn handle_notification(state: &mut State, notification: WireNotification) -> Option<String> {
    let WireNotification { method, params } = notification;
    if method == notification::DidOpenTextDocument::METHOD {
        let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else {
            tracing::warn!("bad didOpen params");
            return None;
        };
        let uri = params.text_document.uri.as_str().to_string();
        let opened = state.buffers.open(
            uri.as_str(),
            params.text_document.version,
            params.text_document.language_id.as_str(),
            params.text_document.text,
        );
        if opened {
            state.schedule(true);
        } else {
            tracing::debug!(%uri, "didOpen ignored for unsupported language");
        }
        None
    } else if method == notification::DidChangeTextDocument::METHOD {
        let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(params) else {
            tracing::warn!("bad didChange params");
            return None;
        };
        let uri = params.text_document.uri.as_str().to_string();
        if state.buffers.change(
            uri.as_str(),
            params.text_document.version,
            &params.content_changes,
            state.encoding,
        ) {
            state.schedule(false);
        }
        None
    } else if method == notification::DidCloseTextDocument::METHOD {
        let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(params) else {
            tracing::warn!("bad didClose params");
            return None;
        };
        let uri = params.text_document.uri.as_str().to_string();
        if state.buffers.close(uri.as_str()) {
            state.schedule(true);
        }
        Some(uri)
    } else if method == notification::DidSaveTextDocument::METHOD {
        let Ok(params) = serde_json::from_value::<DidSaveTextDocumentParams>(params) else {
            tracing::warn!("bad didSave params");
            return None;
        };
        let uri = params.text_document.uri.as_str().to_string();
        let mut changed = false;
        if let Some(text) = params.text.as_deref() {
            changed = state.buffers.save(uri.as_str(), text);
        }
        if changed || params.text.is_none() {
            state.schedule(true);
        } else {
            state.flush();
        }
        None
    } else if method == notification::DidChangeWatchedFiles::METHOD {
        let Ok(params) = serde_json::from_value::<DidChangeWatchedFilesParams>(params) else {
            tracing::warn!("bad didChangeWatchedFiles params");
            return None;
        };
        let relevant = params.changes.iter().any(|event| {
            convert::uri_to_path(event.uri.as_str())
                .filter(|path| path.starts_with(&state.root))
                .and_then(|path| meta_ast::detect_language(&path))
                .is_some()
        });
        if relevant {
            state.schedule(true);
        }
        None
    } else {
        None
    }
}

fn respond_or_warn<T: serde::Serialize>(connection: &Connection, id: RequestId, result: &T) {
    if let Err(error) = respond(connection, id, result) {
        tracing::warn!(%error, "respond failed");
    }
}

fn respond_protocol_error(connection: &Connection, id: RequestId, message: String) {
    let error = ServerError::Protocol(message);
    if let Err(send) = connection
        .sender
        .send(Message::Response(error.to_response(id)))
    {
        tracing::warn!(%send, "send failed");
    }
}

fn respond<T: serde::Serialize>(
    connection: &Connection,
    id: RequestId,
    result: &T,
) -> anyhow::Result<()> {
    connection
        .sender
        .send(Message::Response(Response {
            id,
            response_result: Ok(serde_json::to_value(result)?),
        }))
        .context("send response")?;
    Ok(())
}

fn publish_all(connection: &Connection, state: &State) -> anyhow::Result<()> {
    for (uri, doc) in state.buffers.iter() {
        let lsp_uri: Uri = uri.parse().context("open uri")?;
        let params = PublishDiagnosticsParams {
            uri: lsp_uri,
            diagnostics: handlers::diagnostics_for(&state.snapshot, state, uri, state.encoding),
            version: Some(doc.version),
        };
        connection
            .sender
            .send(Message::Notification(lsp_server::Notification::new(
                "textDocument/publishDiagnostics".to_string(),
                &params,
            )))
            .context("publish diagnostics")?;
    }
    Ok(())
}

fn publish_clear(connection: &Connection, uri: &str) -> anyhow::Result<()> {
    let lsp_uri: Uri = uri.parse().context("closed uri")?;
    let params = PublishDiagnosticsParams {
        uri: lsp_uri,
        diagnostics: Vec::new(),
        version: None,
    };
    connection
        .sender
        .send(Message::Notification(lsp_server::Notification::new(
            "textDocument/publishDiagnostics".to_string(),
            &params,
        )))
        .context("clear diagnostics")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::Notification as WireNotification;

    fn state() -> (
        State,
        crossbeam_channel::Receiver<ReindexReq>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def greet(): pass\n").unwrap();
        let (req_tx, req_rx) = crossbeam_channel::unbounded();
        let state = State::new(dir.path().to_path_buf(), req_tx, Encoding::Utf16).unwrap();
        (state, req_rx, dir)
    }

    fn did_open(uri: &str, language_id: &str) -> WireNotification {
        WireNotification {
            method: notification::DidOpenTextDocument::METHOD.to_string(),
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
    fn cold_start_builds_index() {
        let (state, _rx, _dir) = state();
        assert_eq!(state.snapshot.extractions.len(), 1);
        assert!(
            state.snapshot.extractions[0]
                .symbols
                .iter()
                .any(|s| s.name == "greet")
        );
    }

    #[test]
    fn did_open_unknown_language_skips_reindex() {
        let (mut state, rx, _dir) = state();
        handle_notification(&mut state, did_open("file:///notes.txt", "plaintext"));
        assert!(state.buffers.get("file:///notes.txt").is_none());
        assert!(rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn did_open_supported_language_requests_reindex() {
        let (mut state, rx, _dir) = state();
        handle_notification(&mut state, did_open("file:///a.py", "python"));
        assert!(state.buffers.get("file:///a.py").is_some());
        assert!(rx.try_recv().is_ok(), "reindex expected");
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
    fn watched_source_change_requests_reindex() {
        let (mut state, rx, dir) = state();
        let uri = convert::path_to_uri(&dir.path().join("a.py")).unwrap();
        handle_notification(&mut state, watched(uri.as_str()));
        assert!(rx.try_recv().is_ok(), "reindex expected");
    }

    #[test]
    fn watched_change_outside_root_is_ignored() {
        let (mut state, rx, _dir) = state();
        handle_notification(&mut state, watched("file:///elsewhere/a.py"));
        assert!(rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn watched_change_for_unsupported_extension_is_ignored() {
        let (mut state, rx, dir) = state();
        let uri = convert::path_to_uri(&dir.path().join("README.md")).unwrap();
        handle_notification(&mut state, watched(uri.as_str()));
        assert!(rx.try_recv().is_err(), "no reindex expected");
    }
}
