//! Sync LSP loop.
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
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, HoverProviderCapability, InitializeParams, OneOf,
    PositionEncodingKind, PublishDiagnosticsParams, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use lsp_types::{notification, request};

use crate::buffers::BufferStore;
use crate::convert;
use crate::error::ServerError;
use crate::handlers;
use crate::index::{self, IndexSnapshot};
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
    connection
        .initialize_finish(
            request_id,
            serde_json::json!({
                "capabilities": capabilities(),
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .context("send initialize result")?;
    tracing::info!(root = %root.display(), "serving");
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
    let worker = reindex::spawn_worker(req_rx, resp_tx);
    let mut state = State::new(root, req_tx)?;
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
}

impl State {
    fn new(root: PathBuf, req_tx: Sender<ReindexReq>) -> anyhow::Result<Self> {
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

fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF8),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        ..Default::default()
    }
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
                let symbols =
                    handlers::document_symbols(&state.snapshot, params.text_document.uri.as_str());
                respond_or_warn(connection, id, &DocumentSymbolResponse::Flat(symbols));
            }
            Err(error) => respond_protocol_error(connection, id, error.to_string()),
        }
    } else if method == request::HoverRequest::METHOD {
        match serde_json::from_value::<HoverParams>(params) {
            Ok(params) => {
                let position = params.text_document_position_params.position;
                let uri = params.text_document_position_params.text_document.uri;
                let hover = handlers::hover_at(&state.snapshot, uri.as_str(), position);
                respond_or_warn(connection, id, &hover);
            }
            Err(error) => respond_protocol_error(connection, id, error.to_string()),
        }
    } else if method == request::GotoDefinition::METHOD {
        match serde_json::from_value::<GotoDefinitionParams>(params) {
            Ok(params) => {
                let position = params.text_document_position_params.position;
                let uri = params.text_document_position_params.text_document.uri;
                let target = handlers::definition_at(&state.snapshot, uri.as_str(), position);
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
        state.buffers.open(
            uri.as_str(),
            params.text_document.version,
            params.text_document.language_id.as_str(),
            params.text_document.text,
        );
        state.schedule(true);
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
        if changed {
            state.schedule(true);
        } else {
            state.flush();
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
            diagnostics: handlers::diagnostics_for(&state.snapshot, uri),
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
