//! Sync LSP loop over a phase-typed connection.
mod capabilities;
mod dispatch;
pub(crate) mod ids;
mod progress;
mod scheduler;
pub(crate) mod session;

use anyhow::Context;
use crossbeam_channel::{Receiver, TryRecvError, select};
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::InitializeParams;
use lsp_types::request::Request as _;

use crate::cancel::Cancellation;
use crate::index;
use crate::index::Persistence;
use crate::position;
use crate::reindex::{self, ReindexResp};
use crate::server::capabilities::{
    capabilities, register_watched_files, root_from_params, supports_pull_diagnostics,
    supports_watched_files,
};
use crate::server::dispatch::{handle_notification, handle_request};
use crate::server::ids::SERVER_NAME;
use crate::server::session::Session;
use crate::types::LogLevel;

pub fn run(log_level: Option<LogLevel>) -> anyhow::Result<()> {
    let filter = match log_level {
        // The flag sets the default; RUST_LOG still wins when it is set.
        Some(level) => tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.as_str())),
        None => tracing_subscriber::EnvFilter::from_default_env(),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    meta_ast::language::validate_queries();
    let (connection, io_threads) = Connection::stdio();
    let result = Server::new(connection).initialize().and_then(Server::serve);
    io_threads.join()?;
    result
}

/// Serve one client over an existing connection. Tests use `Connection::memory`.
pub fn run_connection(connection: Connection) -> anyhow::Result<()> {
    Server::new(connection).initialize().and_then(Server::serve)
}

/// Connection paired with its lifecycle phase; queries exist only after [`Ready`].
pub(crate) struct Server<S> {
    connection: Connection,
    phase: S,
}

pub(crate) struct Uninitialized;

/// Handshake complete: workspace indexed, worker running.
pub(crate) struct Ready {
    session: Session,
    worker: std::thread::JoinHandle<()>,
    resp_rx: Receiver<ReindexResp>,
}

impl Server<Uninitialized> {
    fn new(connection: Connection) -> Self {
        Self {
            connection,
            phase: Uninitialized,
        }
    }

    fn initialize(self) -> anyhow::Result<Server<Ready>> {
        let (request_id, init_value) = self
            .connection
            .initialize_start()
            .context("wait for initialize")?;
        let params: InitializeParams =
            serde_json::from_value(init_value).context("parse initialize params")?;
        let root = match root_from_params(&params) {
            Ok(root) => root,
            Err(error) => {
                return Err(reject_initialize(
                    &self.connection,
                    request_id,
                    &error.to_string(),
                ));
            }
        };
        if !supports_pull_diagnostics(&params.capabilities) {
            return Err(reject_initialize(
                &self.connection,
                request_id,
                "client must support textDocument/diagnostic; this server is pull only",
            ));
        }
        let encoding = position::negotiate(&params.capabilities);
        let progress_supported = params
            .capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);
        let definition_links = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|text_document| text_document.definition.as_ref())
            .and_then(|definition| definition.link_support)
            .unwrap_or(false);
        self.connection
            .initialize_finish(
                request_id,
                serde_json::json!({
                    "capabilities": capabilities(encoding),
                    "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .context("send initialize result")?;
        tracing::info!(root = %root.as_path().display(), ?encoding, "serving");
        if supports_watched_files(&params.capabilities) {
            if let Err(error) = register_watched_files(&self.connection) {
                tracing::warn!(%error, "watched files registration failed");
            }
        } else {
            tracing::info!(
                "client has no dynamic watched-file registration; external changes stay untracked"
            );
        }
        let (req_tx, req_rx) = crossbeam_channel::bounded(1);
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
        let mut reindexer = index::Reindexer::with_persistence(Persistence::Enabled);
        let warmed = reindexer.seed_from_shards(root.as_path());
        match &warmed.rejected {
            Some(reason) => tracing::warn!(%reason, "cold start without a shard cache"),
            None => {
                if warmed.reused > 0 {
                    tracing::info!(reused = warmed.reused, "cold start from .meta-ast");
                }
                if !warmed.skipped.is_empty() {
                    tracing::info!(
                        skipped = warmed.skipped.len(),
                        "refused shard records are re-extracted on the next pass"
                    );
                }
            }
        }
        for skip in warmed.skipped.iter().take(3) {
            tracing::warn!(path = %skip.path.display(), reason = %skip.reason, "shard record skipped");
        }
        let first = reindexer.rebuild(root.as_path(), &[])?;
        let worker = reindex::spawn_worker(req_rx, resp_tx, reindexer);
        let mut session = Session::new(root, encoding, first, req_tx, progress_supported);
        session.definition_links = definition_links;
        Ok(Server {
            connection: self.connection,
            phase: Ready {
                session,
                worker,
                resp_rx,
            },
        })
    }
}

impl Server<Ready> {
    fn serve(self) -> anyhow::Result<()> {
        let Server {
            connection,
            phase:
                Ready {
                    mut session,
                    worker,
                    mut resp_rx,
                },
        } = self;
        let cancel = Cancellation::default();
        let mut shutdown = false;
        'serve: loop {
            select! {
                recv(connection.receiver) -> msg => {
                    let Ok(msg) = msg else { break 'serve };
                    match handle_client_message(&connection, &mut session, &cancel, &mut shutdown, msg)? {
                        LoopControl::Continue => {}
                        LoopControl::Exit => break 'serve,
                    }
                }
                recv(resp_rx) -> resp => {
                    let Ok(resp) = resp else {
                        tracing::warn!("reindex worker gone");
                        session.worker_lost(&connection);
                        // A disconnected receiver stays ready and would spin the select; `never` never delivers and never disconnects.
                        resp_rx = crossbeam_channel::never();
                        continue 'serve;
                    };
                    session.on_reindex_response(&connection, resp);
                }
            }
            match drain_messages(&connection, &mut session, &cancel, &mut shutdown)? {
                LoopControl::Continue => {}
                LoopControl::Exit => break 'serve,
            }
            session.flush_batch();
            session.progress.pump(&connection);
        }
        session.progress.abandon(&connection);
        drop(session);
        if worker.join().is_err() {
            return Err(anyhow::anyhow!("reindex worker panicked"));
        }
        if !shutdown {
            // Spec: exit before shutdown is an error exit.
            return Err(anyhow::anyhow!("client exited without shutdown"));
        }
        Ok(())
    }
}

fn reject_initialize(
    connection: &Connection,
    request_id: RequestId,
    message: &str,
) -> anyhow::Error {
    let response = Response::new_err(
        request_id,
        lsp_server::ErrorCode::InvalidParams as i32,
        message.to_string(),
    );
    if let Err(error) = connection.sender.send(Message::Response(response)) {
        tracing::warn!(%error, "initialize error response failed");
    }
    anyhow::anyhow!(message.to_string())
}

enum LoopControl {
    Continue,
    Exit,
}

/// Handle every queued message; one drained batch sends at most one reindex request, no wall clock.
fn drain_messages(
    connection: &Connection,
    session: &mut Session,
    cancel: &Cancellation,
    shutdown: &mut bool,
) -> anyhow::Result<LoopControl> {
    loop {
        match connection.receiver.try_recv() {
            Ok(message) => {
                match handle_client_message(connection, session, cancel, shutdown, message)? {
                    LoopControl::Continue => {}
                    LoopControl::Exit => return Ok(LoopControl::Exit),
                }
            }
            Err(TryRecvError::Empty) => return Ok(LoopControl::Continue),
            Err(TryRecvError::Disconnected) => return Ok(LoopControl::Exit),
        }
    }
}

fn handle_client_message(
    connection: &Connection,
    session: &mut Session,
    cancel: &Cancellation,
    shutdown: &mut bool,
    message: Message,
) -> anyhow::Result<LoopControl> {
    match message {
        Message::Request(request) => {
            // Answered here, not via the crate helper: that blocks until `exit`.
            if request.method == lsp_types::request::Shutdown::METHOD {
                if let Err(error) =
                    connection
                        .sender
                        .send(Message::Response(lsp_server::Response::new_ok(
                            request.id,
                            (),
                        )))
                {
                    tracing::warn!(%error, "shutdown response failed");
                }
                *shutdown = true;
                return Ok(LoopControl::Continue);
            }
            if *shutdown {
                let _ = connection
                    .sender
                    .send(Message::Response(lsp_server::Response::new_err(
                        request.id,
                        lsp_server::ErrorCode::InvalidRequest as i32,
                        "server is shutting down".to_string(),
                    )));
                return Ok(LoopControl::Continue);
            }
            handle_request(connection, cancel, session, request);
            Ok(LoopControl::Continue)
        }
        Message::Notification(notification) => {
            if notification.method == "exit" {
                return Ok(LoopControl::Exit);
            }
            handle_notification(connection, session, cancel, notification);
            session.progress.pump(connection);
            Ok(LoopControl::Continue)
        }
        Message::Response(response) => {
            session
                .progress
                .on_ack(connection, &response, session.applied_seq);
            Ok(LoopControl::Continue)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lsp_server::{Notification, Request, RequestId};
    use lsp_types::request::{DocumentSymbolRequest, Request as _, Shutdown};

    use super::*;
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

    fn dead_response_receiver() -> Receiver<ReindexResp> {
        let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
        drop(resp_tx);
        resp_rx
    }

    /// A lost worker answers with an error and the session keeps serving.
    #[test]
    fn worker_loss_answers_request_failed_and_keeps_serving() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def greet(): pass\n").unwrap();
        let (session, _req_rx) = crate::testutil::session(dir.path(), false);
        let (connection, client) = Connection::memory();
        let server = Server {
            connection,
            phase: Ready {
                session,
                worker: std::thread::spawn(|| {}),
                resp_rx: dead_response_receiver(),
            },
        };
        let serving = std::thread::spawn(move || server.serve());

        let uri = crate::convert::path_to_uri(&file).expect("uri").to_string();
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: DocumentSymbolRequest::METHOD.to_string(),
                params: serde_json::json!({"textDocument": {"uri": uri}}),
            }))
            .expect("send documentSymbol");

        let message = client
            .receiver
            .recv_timeout(CLIENT_TIMEOUT)
            .expect("documentSymbol answer after worker loss");
        let Message::Response(response) = message else {
            panic!("expected a response");
        };
        assert_eq!(response.id, RequestId::from(1));
        let Err(error) = response.response_result else {
            panic!("a lost worker must not answer from a stale snapshot");
        };
        assert_eq!(error.code, -32803);
        assert!(error.message.contains("index unavailable"));

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: Shutdown::METHOD.to_string(),
                params: serde_json::Value::Null,
            }))
            .expect("send shutdown");
        let message = client
            .receiver
            .recv_timeout(CLIENT_TIMEOUT)
            .expect("shutdown answer");
        let Message::Response(response) = message else {
            panic!("expected a response");
        };
        assert!(response.response_result.is_ok());

        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: serde_json::Value::Null,
            }))
            .expect("send exit");
        let result = serving.join().expect("serve thread");
        assert!(result.is_ok(), "clean exit after shutdown: {result:?}");
    }

    /// Every message queued before the loop starts is one batch: one reindex request.
    #[test]
    fn a_drained_batch_sends_one_request() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, "def greet(): pass\n").unwrap();
        let uri = crate::convert::path_to_uri(&file).expect("uri").to_string();
        let (session, req_rx) = crate::testutil::session(dir.path(), false);
        let (connection, client) = Connection::memory();

        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: serde_json::json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": "python",
                        "version": 1,
                        "text": "x = 1\n"
                    }
                }),
            }))
            .expect("send didOpen");
        for version in 2..=4 {
            client
                .sender
                .send(Message::Notification(Notification {
                    method: "textDocument/didChange".to_string(),
                    params: serde_json::json!({
                        "textDocument": {"uri": uri, "version": version},
                        "contentChanges": [{"text": format!("x = {version}\n")}]
                    }),
                }))
                .expect("send didChange");
        }

        let server = Server {
            connection,
            phase: Ready {
                session,
                worker: std::thread::spawn(|| {}),
                resp_rx: crossbeam_channel::never(),
            },
        };
        let serving = std::thread::spawn(move || server.serve());

        let request = req_rx.recv_timeout(CLIENT_TIMEOUT).expect("one request");
        assert_eq!(request.seq, 1);
        assert!(
            req_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a drained batch must produce exactly one request"
        );
        assert_eq!(
            request.overlays[0].text, "x = 4\n",
            "the request carries the last state of the batch"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: Shutdown::METHOD.to_string(),
                params: serde_json::Value::Null,
            }))
            .expect("send shutdown");
        let _ = client.receiver.recv_timeout(CLIENT_TIMEOUT);
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: serde_json::Value::Null,
            }))
            .expect("send exit");
        let result = serving.join().expect("serve thread");
        assert!(result.is_ok(), "clean exit after shutdown: {result:?}");
    }
}
