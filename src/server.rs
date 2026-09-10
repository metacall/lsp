//! Sync LSP loop.
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, select};
use lsp_server::{Connection, Message};
use lsp_server::{Notification as WireNotification, Request as WireRequest, RequestId, Response};
use lsp_types::notification::Notification as LspNotification;
use lsp_types::request::Request as LspRequest;
use lsp_types::{CancelParams, NumberOrString};
use lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentSymbolParams, DocumentSymbolResponse, FileSystemWatcher, GlobPattern,
    GotoDefinitionParams, GotoDefinitionResponse, HoverParams, HoverProviderCapability,
    InitializeParams, MessageType, OneOf, ProgressParams, ProgressParamsValue,
    PublishDiagnosticsParams, ReferenceParams, Registration, RegistrationParams, SaveOptions,
    ServerCapabilities, ShowMessageParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use lsp_types::{notification, request};

use crate::buffers::BufferStore;
use crate::cancel::Cancellation;
use crate::convert;
use crate::error::ServerError;
use crate::handlers;
use crate::index::{self, IndexSnapshot, SourceText};
use crate::position::{self, Encoding};
use crate::reindex::{self, ReindexReq, ReindexResp};

const SERVER_NAME: &str = "meta-ast-lsp";
const PROGRESS_THRESHOLD_MS: u128 = 500;
const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// Files whose contents shape engine resolver state for the process lifetime.
const RESOLVER_CONFIGS: [&str; 5] = [
    "tsconfig.json",
    "jsconfig.json",
    "go.mod",
    "pyproject.toml",
    "package.json",
];

pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    meta_ast::language::validate_queries();
    let (connection, io_threads) = Connection::stdio();
    let result = run_connection(connection);
    io_threads.join()?;
    result
}

/// Serve one client over an existing connection. Blocks until exit.
///
/// Use with `Connection::memory` in tests.
pub fn run_connection(connection: Connection) -> anyhow::Result<()> {
    let (request_id, init_value) = connection
        .initialize_start()
        .context("wait for initialize")?;
    let params: InitializeParams =
        serde_json::from_value(init_value).context("parse initialize params")?;
    let root = root_from_params(&params)?;
    let encoding = position::negotiate(&params.capabilities);
    let progress_supported = params
        .capabilities
        .window
        .as_ref()
        .and_then(|window| window.work_done_progress)
        .unwrap_or(false);
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
    let mut reindexer = index::Reindexer::new();
    let first = Arc::new(reindexer.rebuild(&root, &[], 1)?);
    let worker = reindex::spawn_worker(req_rx, resp_tx, reindexer);
    let mut state = State::new(root, req_tx, encoding, first, progress_supported);
    let mut shutdown = false;
    'serve: loop {
        let timeout = state.pending_remaining().unwrap_or(IDLE_TIMEOUT);
        select! {
            recv(connection.receiver) -> msg => {
                let Ok(msg) = msg else { break 'serve };
                match handle_client_message(&connection, &mut state, msg)? {
                    LoopControl::Continue => {}
                    LoopControl::Shutdown => {
                        shutdown = true;
                        break 'serve;
                    }
                    LoopControl::Exit => break 'serve,
                }
            }
            recv(resp_rx) -> resp => {
                let Ok(resp) = resp else {
                    worker_gone(&connection, &mut state);
                    continue 'serve;
                };
                apply_resp(&connection, &mut state, resp);
            }
            default(timeout) => {}
        }
        drain_resps(&connection, &mut state, &resp_rx);
        if state.flush_due() {
            state.flush();
        }
        state.pump_progress(&connection);
    }
    drop(state.req_tx);
    let _ = worker.join();
    if !shutdown {
        tracing::warn!("client exited without shutdown");
    }
    Ok(())
}

/// Control flow returned by a handled client message.
enum LoopControl {
    Continue,
    Shutdown,
    Exit,
}

fn handle_client_message(
    connection: &Connection,
    state: &mut State,
    message: Message,
) -> anyhow::Result<LoopControl> {
    match message {
        Message::Request(request) => {
            if connection
                .handle_shutdown(&request)
                .context("answer shutdown")?
            {
                return Ok(LoopControl::Shutdown);
            }
            handle_request(connection, state, request);
            Ok(LoopControl::Continue)
        }
        Message::Notification(notification) => {
            if notification.method == "exit" {
                return Ok(LoopControl::Exit);
            }
            if let Some(uri) = handle_notification(connection, state, notification)
                && let Err(error) = publish_clear(connection, uri.as_str())
            {
                tracing::warn!(%error, "clear failed");
            }
            state.pump_progress(connection);
            Ok(LoopControl::Continue)
        }
        Message::Response(_) => Ok(LoopControl::Continue),
    }
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
    cancel: Cancellation,
    progress_supported: bool,
    progress_active: bool,
    progress_pending: bool,
    progress_seq: u64,
    progress_token: NumberOrString,
    last_reindex_ms: u128,
    warned_resolver: HashSet<PathBuf>,
}

impl State {
    fn new(
        root: PathBuf,
        req_tx: Sender<ReindexReq>,
        encoding: Encoding,
        snapshot: Arc<IndexSnapshot>,
        progress_supported: bool,
    ) -> Self {
        Self {
            root,
            buffers: BufferStore::default(),
            snapshot,
            counter: 1,
            seq: 0,
            applied_seq: 0,
            req_tx,
            pending: None,
            encoding,
            cancel: Cancellation::default(),
            progress_supported,
            progress_active: false,
            progress_pending: false,
            progress_seq: 0,
            progress_token: NumberOrString::String(String::new()),
            last_reindex_ms: 0,
            warned_resolver: HashSet::new(),
        }
    }

    fn next_ids(&mut self) -> (u64, u32) {
        self.seq = self.seq.wrapping_add(1);
        self.counter = self.counter.wrapping_add(1);
        if self.counter == 0 {
            self.counter = 1;
        }
        (self.seq, self.counter)
    }

    fn send_req(&mut self, seq: u64, raw: u32) {
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
        self.progress_pending =
            self.progress_supported && self.last_reindex_ms > PROGRESS_THRESHOLD_MS;
    }

    /// Start a progress operation when the previous reindex was slow.
    fn pump_progress(&mut self, connection: &Connection) {
        if self.progress_pending && !self.progress_active {
            self.progress_pending = false;
            self.start_progress(connection);
        }
    }

    fn start_progress(&mut self, connection: &Connection) {
        self.progress_seq += 1;
        let token = NumberOrString::String(format!("meta-ast-reindex-{}", self.progress_seq));
        let create = WireRequest {
            id: RequestId::from(format!("meta-ast-progress-{}", self.progress_seq)),
            method: request::WorkDoneProgressCreate::METHOD.to_string(),
            params: serde_json::to_value(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .unwrap_or(serde_json::Value::Null),
        };
        let _ = connection.sender.send(Message::Request(create));
        let begin = ProgressParams {
            token: token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "Indexing workspace".to_string(),
                cancellable: Some(false),
                message: None,
                percentage: None,
            })),
        };
        let _ = connection
            .sender
            .send(Message::Notification(WireNotification::new(
                "$/progress".to_string(),
                begin,
            )));
        self.progress_active = true;
        self.progress_token = token;
    }

    fn end_progress(&mut self, connection: &Connection) {
        if !self.progress_active {
            return;
        }
        let end = ProgressParams {
            token: self.progress_token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: None,
            })),
        };
        let _ = connection
            .sender
            .send(Message::Notification(WireNotification::new(
                "$/progress".to_string(),
                end,
            )));
        self.progress_active = false;
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

fn worker_gone(connection: &Connection, state: &mut State) {
    tracing::warn!("reindex worker gone");
    state.pending = None;
    state.end_progress(connection);
}

fn drain_resps(connection: &Connection, state: &mut State, resp_rx: &Receiver<ReindexResp>) {
    while let Ok(resp) = resp_rx.try_recv() {
        apply_resp(connection, state, resp);
    }
}

fn apply_resp(connection: &Connection, state: &mut State, resp: ReindexResp) {
    if resp.seq <= state.applied_seq {
        state.end_progress(connection);
        return;
    }
    state.applied_seq = resp.seq;
    state.last_reindex_ms = resp.elapsed_ms;
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
    state.end_progress(connection);
}

fn capabilities(encoding: Encoding) -> ServerCapabilities {
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

/// Glob patterns for every extension meta-ast can parse, plus resolver
/// configuration files.
fn watched_globs() -> Vec<String> {
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

/// True when the path names a resolver configuration file.
fn is_resolver_config(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| RESOLVER_CONFIGS.contains(&name))
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

fn handle_request(connection: &Connection, state: &mut State, request: WireRequest) {
    let WireRequest { id, method, params } = request;
    let token = state.cancel.register(&id);
    let outcome = if token.is_cancelled() {
        Err(ServerError::Cancelled)
    } else {
        dispatch_request(state, &method, params)
    };
    state.cancel.remove(&id);
    let outcome = if token.is_cancelled() {
        Err(ServerError::Cancelled)
    } else {
        outcome
    };
    match outcome {
        Ok(value) => respond_or_warn(connection, id, &value),
        Err(error) => send_response_error(connection, id, error),
    }
}

fn dispatch_request(
    state: &State,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ServerError> {
    if method == request::DocumentSymbolRequest::METHOD {
        let params: DocumentSymbolParams = parse_params(params)?;
        let symbols = handlers::document_symbols(
            &state.snapshot,
            state,
            params.text_document.uri.as_str(),
            state.encoding,
        );
        to_value(DocumentSymbolResponse::Flat(symbols))
    } else if method == request::HoverRequest::METHOD {
        let params: HoverParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let hover = handlers::hover_at(
            &state.snapshot,
            state,
            uri.as_str(),
            position,
            state.encoding,
        );
        to_value(hover)
    } else if method == request::GotoDefinition::METHOD {
        let params: GotoDefinitionParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let target = handlers::definition_at(
            &state.snapshot,
            state,
            uri.as_str(),
            position,
            state.encoding,
        );
        let result: Option<GotoDefinitionResponse> = target.map(GotoDefinitionResponse::Scalar);
        to_value(result)
    } else if method == request::References::METHOD {
        let params: ReferenceParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let locations = handlers::references_at(
            &state.snapshot,
            state,
            uri.as_str(),
            position,
            state.encoding,
            params.context.include_declaration,
        );
        to_value(locations)
    } else if method == request::WorkspaceSymbolRequest::METHOD {
        let params: WorkspaceSymbolParams = parse_params(params)?;
        let symbols = handlers::workspace_symbols(
            &state.snapshot,
            state,
            params.query.as_str(),
            state.encoding,
        );
        to_value(WorkspaceSymbolResponse::Flat(symbols))
    } else if method == request::Completion::METHOD {
        let params: CompletionParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let items = handlers::completion_at(
            &state.snapshot,
            state,
            uri.as_str(),
            position,
            state.encoding,
        );
        to_value(CompletionResponse::Array(items))
    } else {
        Err(ServerError::Protocol(format!("unknown method {method}")))
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, ServerError> {
    serde_json::from_value(params).map_err(|error| ServerError::Protocol(error.to_string()))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<serde_json::Value, ServerError> {
    serde_json::to_value(value).map_err(|error| ServerError::Protocol(error.to_string()))
}

fn send_response_error(connection: &Connection, id: RequestId, error: ServerError) {
    if let Err(send) = connection
        .sender
        .send(Message::Response(error.to_response(id)))
    {
        tracing::warn!(%send, "send failed");
    }
}

fn handle_notification(
    connection: &Connection,
    state: &mut State,
    notification: WireNotification,
) -> Option<String> {
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
    } else if method == notification::Cancel::METHOD {
        let Ok(params) = serde_json::from_value::<CancelParams>(params) else {
            tracing::warn!("bad cancel params");
            return None;
        };
        let id = match params.id {
            NumberOrString::Number(number) => RequestId::from(number),
            NumberOrString::String(value) => RequestId::from(value),
        };
        state.cancel.cancel(&id);
        None
    } else if method == notification::DidChangeWatchedFiles::METHOD {
        let Ok(params) = serde_json::from_value::<DidChangeWatchedFilesParams>(params) else {
            tracing::warn!("bad didChangeWatchedFiles params");
            return None;
        };
        handle_watched_files(connection, state, &params);
        None
    } else {
        None
    }
}

/// Reindex on source changes. Warn once per resolver config change.
fn handle_watched_files(
    connection: &Connection,
    state: &mut State,
    params: &DidChangeWatchedFilesParams,
) {
    let mut source_changed = false;
    for event in &params.changes {
        let Some(path) = convert::uri_to_path(event.uri.as_str()) else {
            continue;
        };
        if !path.starts_with(&state.root) {
            continue;
        }
        if meta_ast::detect_language(&path).is_some() {
            source_changed = true;
        } else if is_resolver_config(&path) && state.warned_resolver.insert(path.clone()) {
            warn_resolver_change(connection, &path);
        }
    }
    if source_changed {
        state.schedule(true);
    }
}

/// Tell the user that resolver state needs a restart. The engine caches
/// resolver filesystem state for the process lifetime.
fn warn_resolver_change(connection: &Connection, path: &Path) {
    let params = ShowMessageParams {
        typ: MessageType::WARNING,
        message: format!(
            "{} changed. Restart the server to apply the new resolver configuration.",
            path.display()
        ),
    };
    if let Err(error) = connection
        .sender
        .send(Message::Notification(WireNotification::new(
            "window/showMessage".to_string(),
            params,
        )))
    {
        tracing::warn!(%error, "resolver warning failed");
    }
}

fn respond_or_warn<T: serde::Serialize>(connection: &Connection, id: RequestId, result: &T) {
    if let Err(error) = respond(connection, id, result) {
        tracing::warn!(%error, "respond failed");
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
        let snapshot = Arc::new(index::rebuild_from_inputs(dir.path(), &[], 1).unwrap());
        let state = State::new(
            dir.path().to_path_buf(),
            req_tx,
            Encoding::Utf16,
            snapshot,
            false,
        );
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
        let (server, _client) = Connection::memory();
        handle_notification(
            &server,
            &mut state,
            did_open("file:///notes.txt", "plaintext"),
        );
        assert!(state.buffers.get("file:///notes.txt").is_none());
        assert!(rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn did_open_supported_language_requests_reindex() {
        let (mut state, rx, _dir) = state();
        let (server, _client) = Connection::memory();
        handle_notification(&server, &mut state, did_open("file:///a.py", "python"));
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
        let (server, _client) = Connection::memory();
        let uri = convert::path_to_uri(&dir.path().join("a.py")).unwrap();
        handle_notification(&server, &mut state, watched(uri.as_str()));
        assert!(rx.try_recv().is_ok(), "reindex expected");
    }

    #[test]
    fn watched_change_outside_root_is_ignored() {
        let (mut state, rx, _dir) = state();
        let (server, _client) = Connection::memory();
        handle_notification(&server, &mut state, watched("file:///elsewhere/a.py"));
        assert!(rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn watched_change_for_unsupported_extension_is_ignored() {
        let (mut state, rx, dir) = state();
        let (server, _client) = Connection::memory();
        let uri = convert::path_to_uri(&dir.path().join("README.md")).unwrap();
        handle_notification(&server, &mut state, watched(uri.as_str()));
        assert!(rx.try_recv().is_err(), "no reindex expected");
    }

    #[test]
    fn watched_resolver_change_warns_once() {
        let (mut state, rx, dir) = state();
        let (server, client) = Connection::memory();
        let uri = convert::path_to_uri(&dir.path().join("tsconfig.json")).unwrap();

        handle_notification(&server, &mut state, watched(uri.as_str()));
        assert!(rx.try_recv().is_err(), "resolver change must not reindex");

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

        handle_notification(&server, &mut state, watched(uri.as_str()));
        assert!(
            client
                .receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "second change must not warn again"
        );
    }

    #[test]
    fn watched_globs_cover_resolver_configs() {
        let globs = watched_globs();
        assert!(globs.iter().any(|glob| glob == "**/tsconfig.json"));
        assert!(globs.iter().any(|glob| glob == "**/go.mod"));
    }

    #[test]
    fn precancelled_request_returns_request_cancelled() {
        let (server, client) = Connection::memory();
        let (mut state, _rx, _dir) = state();
        let id = RequestId::from(2);
        state.cancel.cancel(&id);

        handle_request(
            &server,
            &mut state,
            WireRequest {
                id: id.clone(),
                method: request::DocumentSymbolRequest::METHOD.to_string(),
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
        assert!(state.cancel.is_empty());
    }

    #[test]
    fn stale_reindex_response_ends_progress() {
        let (server, client) = Connection::memory();
        let (mut state, _rx, _dir) = state();
        state.progress_active = true;
        state.progress_seq = 3;
        state.progress_token = NumberOrString::String("meta-ast-reindex-3".to_string());
        state.applied_seq = 5;

        apply_resp(
            &server,
            &mut state,
            ReindexResp {
                seq: 3,
                elapsed_ms: 1,
                result: Err(anyhow::anyhow!("stale reindex")),
            },
        );

        assert!(!state.progress_active);
        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("progress end");
        let Message::Notification(notification) = message else {
            panic!("expected a progress notification");
        };
        assert_eq!(notification.method, "$/progress");
        let params: ProgressParams = serde_json::from_value(notification.params).unwrap();
        assert_eq!(
            params.token,
            NumberOrString::String("meta-ast-reindex-3".to_string())
        );
        let ProgressParamsValue::WorkDone(WorkDoneProgress::End(_)) = params.value else {
            panic!("expected a progress end");
        };
    }

    #[test]
    fn worker_disconnect_ends_progress() {
        let (server, client) = Connection::memory();
        let (mut state, _rx, _dir) = state();
        state.progress_active = true;
        state.progress_seq = 4;
        state.progress_token = NumberOrString::String("meta-ast-reindex-4".to_string());

        worker_gone(&server, &mut state);

        assert!(!state.progress_active);
        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("progress end");
        let Message::Notification(notification) = message else {
            panic!("expected a progress notification");
        };
        assert_eq!(notification.method, "$/progress");
    }
}
