//! Sync LSP loop.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
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
    let mut state = State::new(root)?;
    let mut shutdown = false;
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection
                    .handle_shutdown(&request)
                    .context("answer shutdown")?
                {
                    shutdown = true;
                    continue;
                }
                if let Err(error) = handle_request(&connection, &mut state, request) {
                    tracing::warn!(%error, "request failed");
                }
            }
            Message::Notification(notification) => {
                if notification.method == "exit" {
                    break;
                }
                if let Err(error) = handle_notification(&connection, &mut state, notification) {
                    tracing::warn!(%error, "notification failed");
                }
            }
            Message::Response(_) => {}
        }
    }
    drop(connection);
    io_threads.join()?;
    if !shutdown {
        tracing::warn!("client exited without shutdown");
    }
    Ok(())
}

struct State {
    root: PathBuf,
    buffers: BufferStore,
    snapshot: Arc<IndexSnapshot>,
    counter: u32,
}

impl State {
    fn new(root: PathBuf) -> anyhow::Result<Self> {
        let buffers = BufferStore::default();
        let snapshot = Arc::new(index::rebuild(&root, &buffers, 1)?);
        Ok(Self {
            root,
            buffers,
            snapshot,
            counter: 1,
        })
    }

    fn reindex(&mut self) {
        self.counter = self.counter.wrapping_add(1);
        if self.counter == 0 {
            self.counter = 1;
        }
        match index::rebuild(&self.root, &self.buffers, self.counter) {
            Ok(snapshot) => self.snapshot = Arc::new(snapshot),
            Err(error) => tracing::warn!(%error, "reindex failed, keeping prior snapshot"),
        }
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

fn handle_request(
    connection: &Connection,
    state: &mut State,
    request: WireRequest,
) -> anyhow::Result<()> {
    let WireRequest { id, method, params } = request;
    if method == request::DocumentSymbolRequest::METHOD {
        let params: DocumentSymbolParams =
            serde_json::from_value(params).context("documentSymbol params")?;
        let symbols =
            handlers::document_symbols(&state.snapshot, params.text_document.uri.as_str());
        respond(connection, id, &DocumentSymbolResponse::Flat(symbols))?;
    } else if method == request::HoverRequest::METHOD {
        let params: HoverParams = serde_json::from_value(params).context("hover params")?;
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let hover = handlers::hover_at(&state.snapshot, uri.as_str(), position);
        respond(connection, id, &hover)?;
    } else if method == request::GotoDefinition::METHOD {
        let params: GotoDefinitionParams =
            serde_json::from_value(params).context("definition params")?;
        let position = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;
        let target = handlers::definition_at(&state.snapshot, uri.as_str(), position);
        let result: Option<GotoDefinitionResponse> = target.map(GotoDefinitionResponse::Scalar);
        respond(connection, id, &result)?;
    } else {
        let error = ServerError::Protocol(format!("unknown method {method}"));
        connection
            .sender
            .send(Message::Response(error.to_response(id)))
            .context("send")?;
    }
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    state: &mut State,
    notification: WireNotification,
) -> anyhow::Result<()> {
    let WireNotification { method, params } = notification;
    if method == notification::DidOpenTextDocument::METHOD {
        let params: DidOpenTextDocumentParams =
            serde_json::from_value(params).context("didOpen params")?;
        let uri = params.text_document.uri.as_str().to_string();
        state.buffers.open(
            uri.as_str(),
            params.text_document.version,
            params.text_document.language_id.as_str(),
            params.text_document.text,
        );
        state.reindex();
        publish_all(connection, state)?;
    } else if method == notification::DidChangeTextDocument::METHOD {
        let params: DidChangeTextDocumentParams =
            serde_json::from_value(params).context("didChange params")?;
        let uri = params.text_document.uri.as_str().to_string();
        if state.buffers.change(
            uri.as_str(),
            params.text_document.version,
            &params.content_changes,
        ) {
            state.reindex();
        }
        publish_all(connection, state)?;
    } else if method == notification::DidCloseTextDocument::METHOD {
        let params: DidCloseTextDocumentParams =
            serde_json::from_value(params).context("didClose params")?;
        let uri = params.text_document.uri.as_str().to_string();
        if state.buffers.close(uri.as_str()) {
            state.reindex();
        }
        publish_clear(connection, uri.as_str())?;
    } else if method == notification::DidSaveTextDocument::METHOD {
        let params: DidSaveTextDocumentParams =
            serde_json::from_value(params).context("didSave params")?;
        let uri = params.text_document.uri.as_str().to_string();
        let mut changed = false;
        if let Some(text) = params.text.as_deref() {
            changed = state.buffers.save(uri.as_str(), text);
        }
        if changed {
            state.reindex();
        }
        publish_all(connection, state)?;
    }
    Ok(())
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
