//! One served workspace: documents, index state, batching, progress.
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::Sender;
use lsp_server::{Connection, Response};

use crate::buffers::BufferStore;
use crate::error::ServerError;
use crate::index::{IndexSnapshot, SourceText};
use crate::position::Encoding;
use crate::reindex::{ReindexReq, ReindexResp};
use crate::server::progress::ProgressTracker;
use crate::server::scheduler::Scheduler;
use crate::types::{DocUri, RootDir};

/// Buffers win over disk; disk text is used only when it matches the snapshot fingerprint.
impl SourceText for Session {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        if let Some(doc) = self.buffers.by_path(path) {
            return Some(Cow::Borrowed(doc.text.as_str()));
        }
        let IndexState::Ready(snapshot) = &self.index else {
            return None;
        };
        let bytes = std::fs::read(path).ok()?;
        let analyzed = snapshot.content_hash(path)?;
        if meta_ast::Fingerprint::of(&bytes) != analyzed {
            tracing::debug!(path = %path.display(), "source text diverged from the snapshot");
            return None;
        }
        String::from_utf8(bytes).ok().map(Cow::Owned)
    }
}

/// Index availability; an index that cannot describe the requested version never answers.
enum IndexState {
    Ready(Arc<IndexSnapshot>),
    Unavailable { reason: String },
}

pub(crate) struct Session {
    pub(crate) root: RootDir,
    pub(crate) encoding: Encoding,
    pub(crate) definition_links: bool,
    pub(crate) buffers: BufferStore,
    pub(crate) warned_resolvers: BTreeSet<String>,
    index: IndexState,
    applied_seq: u64,
    scheduler: Scheduler,
    progress: ProgressTracker,
    /// True when workspace state changed and no request carries it yet.
    dirty: bool,
}

impl Session {
    pub(crate) fn new(
        root: RootDir,
        encoding: Encoding,
        snapshot: Arc<IndexSnapshot>,
        req_tx: Sender<ReindexReq>,
        progress_supported: bool,
    ) -> Self {
        Self {
            root,
            encoding,
            definition_links: false,
            buffers: BufferStore::default(),
            warned_resolvers: BTreeSet::new(),
            index: IndexState::Ready(snapshot),
            applied_seq: 0,
            scheduler: Scheduler::new(req_tx),
            progress: ProgressTracker::new(progress_supported),
            dirty: false,
        }
    }

    pub(crate) fn ready(&self) -> Result<&Arc<IndexSnapshot>, ServerError> {
        match &self.index {
            IndexState::Ready(snapshot) => Ok(snapshot),
            IndexState::Unavailable { reason } => Err(ServerError::RequestFailed(format!(
                "index unavailable: {reason}"
            ))),
        }
    }

    /// An open document must be indexable and at the exact version the client sent.
    pub(crate) fn ready_for(&self, uri: &DocUri) -> Result<&Arc<IndexSnapshot>, ServerError> {
        let snapshot = self.ready()?;
        let Some(path) = uri.to_path() else {
            return Err(ServerError::RequestFailed(format!(
                "document is not indexed: {uri}"
            )));
        };
        if let Some(doc) = self.buffers.get(uri) {
            if !self.indexable(uri) {
                return Err(ServerError::RequestFailed(format!(
                    "document is outside the indexed root: {uri}"
                )));
            }
            if snapshot.document_version(&path) != Some(doc.version) {
                return Err(ServerError::ContentModified(format!(
                    "{uri} at version {} was not indexed by snapshot {}",
                    doc.version.get(),
                    snapshot.generation()
                )));
            }
            return Ok(snapshot);
        }
        if snapshot.file_by_path(&path).is_none() {
            return Err(ServerError::RequestFailed(format!(
                "document is not indexed: {uri}"
            )));
        }
        Ok(snapshot)
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn flush_batch(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(seq) = self.scheduler.submit(self.root.as_path(), &self.buffers) {
            self.dirty = false;
            self.progress.arm(seq);
        } else if self.scheduler.is_gone() {
            self.dirty = false;
        }
    }

    pub(crate) fn pump_progress(&mut self, connection: &Connection) {
        self.progress.pump(connection);
    }

    pub(crate) fn on_progress_ack(&mut self, connection: &Connection, response: &Response) {
        self.progress.on_ack(connection, response, self.applied_seq);
    }

    pub(crate) fn shutdown(&mut self, connection: &Connection) {
        self.progress.abandon(connection);
    }

    /// Apply one reindex response: stale seqs drop, newest wins; a failed pass disables the index.
    pub(crate) fn on_reindex_response(&mut self, connection: &Connection, resp: ReindexResp) {
        if resp.seq <= self.applied_seq {
            return;
        }
        self.applied_seq = resp.seq;
        match resp.result {
            Ok(snapshot) => {
                tracing::info!(
                    seq = resp.seq,
                    elapsed_ms = resp.elapsed_ms,
                    "snapshot swap"
                );
                self.index = IndexState::Ready(snapshot);
            }
            Err(error) => {
                tracing::warn!(%error, "reindex failed; index unavailable");
                self.index = IndexState::Unavailable {
                    reason: error.to_string(),
                };
            }
        }
        self.progress.finish(resp.seq, connection);
    }

    /// A lost worker cannot maintain the index: disable it.
    pub(crate) fn worker_lost(&mut self, connection: &Connection) {
        self.scheduler.reset();
        self.index = IndexState::Unavailable {
            reason: "reindex worker gone".to_string(),
        };
        self.progress.abandon(connection);
    }

    /// A save only marks the workspace dirty; the worker compares fingerprints.
    pub(crate) fn on_save(&mut self, uri: &DocUri, text: Option<&str>) {
        if let Some(text) = text {
            self.buffers.save(uri, text);
        }
        if self.indexable(uri) {
            self.mark_dirty();
        }
    }

    pub(crate) fn indexable(&self, uri: &DocUri) -> bool {
        uri.to_path().is_some_and(|path| self.root.contains(&path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffers::OpenOutcome;
    use crate::convert;
    use crate::reindex::ReindexReq;
    use crate::types::DocVersion;

    use crate::testutil::doc_uri;

    fn session(dir: &Path) -> Session {
        let snapshot = crate::index::rebuild_from_inputs(dir, &[]).expect("rebuild");
        let (req_tx, _req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        Session::new(
            crate::types::RootDir::try_from(dir).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            true,
        )
    }

    fn session_with_file(dir: &Path) -> Session {
        std::fs::write(dir.join("a.py"), "def greet(): pass\n").unwrap();
        session(dir)
    }

    #[test]
    fn cold_start_builds_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_file(dir.path());
        let snapshot = session.ready().expect("index");
        assert_eq!(snapshot.extractions.len(), 1);
        assert!(
            snapshot.extractions[0]
                .symbols
                .iter()
                .any(|s| s.name == "greet")
        );
    }

    #[test]
    fn an_out_of_root_open_document_is_request_failed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut session = session_with_file(root.path());
        let outside_file = outside.path().join("b.py");
        std::fs::write(&outside_file, "def other(): pass\n").unwrap();
        let uri = convert::path_to_uri(&outside_file).unwrap().to_string();
        assert_eq!(
            session.buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(1),
                "python",
                "def other(): pass\n".to_string()
            ),
            OpenOutcome::Indexed
        );

        let error = session
            .ready_for(&doc_uri(&uri))
            .err()
            .expect("an out-of-root document cannot be indexed");

        assert!(
            matches!(error, ServerError::RequestFailed(_)),
            "a permanent condition must not ask the client to retry: {error}"
        );
    }

    #[test]
    fn a_lost_worker_makes_the_index_unavailable() {
        use lsp_types::request::{DocumentSymbolRequest, Request as _};

        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_file(dir.path());
        let (server, client) = Connection::memory();
        let cancel = crate::cancel::Cancellation::default();
        let uri = convert::path_to_uri(&dir.path().join("a.py"))
            .unwrap()
            .to_string();

        session.worker_lost(&server);

        crate::server::dispatch::handle_request(
            &server,
            &cancel,
            &session,
            lsp_server::Request {
                id: lsp_server::RequestId::from(1),
                method: DocumentSymbolRequest::METHOD.to_string(),
                params: serde_json::json!({"textDocument": {"uri": uri}}),
            },
        );

        let message = client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("response after worker loss");
        let lsp_server::Message::Response(response) = message else {
            panic!("expected a response");
        };
        assert_eq!(response.id, lsp_server::RequestId::from(1));
        let Err(error) = response.response_result else {
            panic!("a lost worker must not answer from a stale snapshot");
        };
        assert_eq!(error.code, -32803);
    }

    #[test]
    fn a_failed_pass_makes_the_index_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_file(dir.path());
        let (server, _client) = Connection::memory();

        session.on_reindex_response(
            &server,
            ReindexResp {
                seq: 2,
                elapsed_ms: 4,
                result: Err(anyhow::anyhow!("failed reindex")),
            },
        );

        assert_eq!(session.applied_seq, 2);
        assert!(session.ready().is_err());
    }

    #[test]
    fn stale_reindex_responses_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_file(dir.path());
        let (server, _client) = Connection::memory();
        session.applied_seq = 5;

        session.on_reindex_response(
            &server,
            ReindexResp {
                seq: 3,
                elapsed_ms: 1,
                result: Err(anyhow::anyhow!("stale")),
            },
        );

        assert_eq!(session.applied_seq, 5, "stale responses change nothing");
        assert!(session.ready().is_ok(), "stale responses change no index");
    }

    #[test]
    fn an_open_document_requires_its_indexed_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_file(dir.path());
        let path = dir.path().join("a.py");
        let uri = convert::path_to_uri(&path).unwrap().to_string();

        assert_eq!(
            session.buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(2),
                "python",
                "x = 1\n".to_string()
            ),
            OpenOutcome::Indexed
        );
        let Err(error) = session.ready_for(&doc_uri(uri.as_str())) else {
            panic!("a buffer newer than the snapshot must not be answered");
        };
        assert_eq!(
            error.to_string().split(':').next(),
            Some("content modified")
        );
        let response = error.to_response(lsp_server::RequestId::from(1));
        let Err(payload) = response.response_result else {
            panic!("content modified must be an error response");
        };
        assert_eq!(payload.code, -32801);
    }

    #[test]
    fn an_unknown_document_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_file(dir.path());
        let uri = convert::path_to_uri(&dir.path().join("missing.py"))
            .unwrap()
            .to_string();

        let Err(error) = session.ready_for(&doc_uri(uri.as_str())) else {
            panic!("a document outside the snapshot must not be answered");
        };
        assert_eq!(error.to_string().split(':').next(), Some("request failed"));
        let response = error.to_response(lsp_server::RequestId::from(1));
        let Err(payload) = response.response_result else {
            panic!("request failed must be an error response");
        };
        assert_eq!(payload.code, -32803);
    }

    #[test]
    fn one_batch_sends_one_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_file(dir.path());
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<ReindexReq>();
        session.scheduler = Scheduler::new(req_tx);

        session.mark_dirty();
        session.flush_batch();
        session.flush_batch();

        let req = req_rx.try_recv().expect("one request for the batch");
        assert_eq!(req.seq, 1);
        assert!(req_rx.try_recv().is_err(), "a drained batch sends once");
    }

    #[test]
    fn source_prefers_buffers_over_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with_file(dir.path());
        let path = dir.path().join("a.py");
        let uri = convert::path_to_uri(&path).unwrap().to_string();
        let mut buffers = BufferStore::default();
        assert_eq!(
            buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(2),
                "python",
                "def from_buffer(): pass\n".to_string()
            ),
            OpenOutcome::Indexed
        );
        session.buffers = buffers;

        let text = session.source(&path).expect("source text");
        assert!(text.contains("from_buffer"));
    }

    #[test]
    fn source_rejects_disk_text_the_snapshot_never_saw() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_file(dir.path());
        let path = dir.path().join("a.py");
        assert!(
            session.source(&path).is_some(),
            "text the snapshot analyzed is used for range conversion"
        );

        std::fs::write(&path, "def changed(): pass\n").unwrap();

        assert!(
            session.source(&path).is_none(),
            "a file that changed after the pass must not answer range conversion"
        );
    }

    #[test]
    fn source_is_absent_for_a_file_the_index_never_saw() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_with_file(dir.path());
        let path = dir.path().join("b.py");
        std::fs::write(&path, "def other(): pass\n").unwrap();

        assert!(
            session.source(&path).is_none(),
            "an unindexed file has no analyzed text to convert against"
        );
    }
}
