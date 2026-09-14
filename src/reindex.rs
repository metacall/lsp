//! Background reindex worker.
use std::path::PathBuf;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use meta_ast::Overlay;

use crate::error::ReindexError;
use crate::index::{IndexSnapshot, Reindexer};
use std::sync::Arc;

pub struct ReindexReq {
    pub seq: u64,
    pub root: PathBuf,
    pub overlays: Vec<Overlay>,
}

pub struct ReindexResp {
    pub seq: u64,
    pub elapsed_ms: u128,
    pub result: Result<Arc<IndexSnapshot>, ReindexError>,
}

pub fn spawn_worker(
    req_rx: Receiver<ReindexReq>,
    resp_tx: Sender<ReindexResp>,
    mut reindexer: Reindexer,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(first) = req_rx.recv() {
            let mut latest = first;
            // Keep the highest seq: a retried held request may carry a lower seq than one queued behind it.
            while let Ok(newer) = req_rx.try_recv() {
                if newer.seq > latest.seq {
                    latest = newer;
                }
            }
            let start = Instant::now();
            // A panic here kills the worker; the loop then answers index requests with an error.
            let result = reindexer.rebuild(&latest.root, &latest.overlays);
            let resp = ReindexResp {
                seq: latest.seq,
                elapsed_ms: start.elapsed().as_millis(),
                result,
            };
            if resp_tx.send(resp).is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::buffers::BufferStore;
    use crate::types::DocVersion;

    use crate::testutil::doc_uri;

    fn workspace() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("a.py");
        std::fs::write(&app, "def greet(): pass\n").unwrap();
        let uri = crate::convert::path_to_uri(&app).unwrap();
        (dir, uri.as_str().to_string())
    }

    #[test]
    fn collect_filters_outside_root() {
        let (dir, uri) = workspace();
        let mut buffers = BufferStore::default();
        assert_eq!(
            buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(1),
                "python",
                "def greet(): pass\n".to_string()
            ),
            crate::buffers::OpenOutcome::Indexed
        );
        assert_eq!(
            buffers.open(
                &doc_uri("file:///other.py"),
                DocVersion::from(1),
                "python",
                "x = 1\n".to_string()
            ),
            crate::buffers::OpenOutcome::Indexed
        );
        let inputs = crate::index::collect_inputs(dir.path(), &buffers);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].uri, uri);
    }

    #[test]
    fn from_inputs_builds_symbols() {
        let (dir, uri) = workspace();
        let mut buffers = BufferStore::default();
        assert_eq!(
            buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(1),
                "python",
                "def greet(): pass\n".to_string()
            ),
            crate::buffers::OpenOutcome::Indexed
        );
        let inputs = crate::index::collect_inputs(dir.path(), &buffers);
        let snapshot = crate::index::rebuild_from_inputs(dir.path(), &inputs).unwrap();
        assert_eq!(snapshot.extractions.len(), 1);
        assert_eq!(snapshot.extractions[0].symbols.len(), 1);
        assert_eq!(snapshot.extractions[0].symbols[0].name, "greet");
    }

    #[test]
    fn worker_answers_single_req() {
        let (dir, uri) = workspace();
        let mut buffers = BufferStore::default();
        assert_eq!(
            buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(1),
                "python",
                "def greet(): pass\n".to_string()
            ),
            crate::buffers::OpenOutcome::Indexed
        );
        let (req_tx, req_rx) = crossbeam_channel::bounded(8);
        let (resp_tx, resp_rx) = crossbeam_channel::bounded(8);
        let handle = spawn_worker(req_rx, resp_tx, Reindexer::new());
        req_tx
            .send(ReindexReq {
                seq: 1,
                root: dir.path().to_path_buf(),
                overlays: crate::index::collect_inputs(dir.path(), &buffers),
            })
            .unwrap();
        let resp = resp_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(resp.seq, 1);
        assert!(resp.result.is_ok());
        drop(req_tx);
        handle.join().unwrap();
    }
}
