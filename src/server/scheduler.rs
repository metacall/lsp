//! Reindex request pipeline: one request per drained protocol batch.
use std::path::Path;

use crossbeam_channel::{Sender, TrySendError};

use crate::buffers::BufferStore;
use crate::index;
use crate::reindex::ReindexReq;

/// One request per drained batch; a queued-full seq is retried with its own retry-time buffer state.
pub(crate) struct Scheduler {
    req_tx: Sender<ReindexReq>,
    seq: u64,
    /// Seq of the newest request the worker had no room for.
    queued: Option<u64>,
    gone: bool,
}

impl Scheduler {
    pub(crate) fn new(req_tx: Sender<ReindexReq>) -> Self {
        Self {
            req_tx,
            seq: 0,
            queued: None,
            gone: false,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// `None` means the channel is full (the batch stays dirty) or the worker is gone.
    pub(crate) fn submit(&mut self, root: &Path, buffers: &BufferStore) -> Option<u64> {
        if self.gone {
            return None;
        }
        let seq = self.queued.take().unwrap_or_else(|| self.next_seq());
        let req = ReindexReq {
            seq,
            root: root.to_path_buf(),
            overlays: index::collect_inputs(root, buffers),
        };
        match self.req_tx.try_send(req) {
            Ok(()) => Some(seq),
            Err(TrySendError::Full(req)) => {
                // Held as a seq, not a request: a retry carries its own send-time buffers.
                self.queued = Some(req.seq);
                None
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("reindex worker gone");
                self.gone = true;
                None
            }
        }
    }

    pub(crate) fn is_gone(&self) -> bool {
        self.gone
    }

    pub(crate) fn reset(&mut self) {
        self.queued = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DocVersion;

    use crate::testutil::doc_uri;

    fn buffers(dir: &Path) -> BufferStore {
        let mut buffers = BufferStore::default();
        let uri = crate::convert::path_to_uri(&dir.join("a.py")).unwrap();
        assert_eq!(
            buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(1),
                "python",
                "def greet(): pass\n".to_string()
            ),
            crate::buffers::OpenOutcome::Indexed
        );
        buffers
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def greet(): pass\n").unwrap();
        dir
    }

    #[test]
    fn a_full_channel_keeps_the_newest_seq_for_the_next_submit() {
        let dir = workspace();
        let buffers = buffers(dir.path());
        let (req_tx, req_rx) = crossbeam_channel::bounded(1);
        let mut scheduler = Scheduler::new(req_tx);

        assert_eq!(scheduler.submit(dir.path(), &buffers), Some(1));
        assert_eq!(scheduler.submit(dir.path(), &buffers), None);
        assert_eq!(scheduler.queued, Some(2));

        let first = req_rx.try_recv().expect("first request");
        assert_eq!(first.seq, 1);

        assert_eq!(scheduler.submit(dir.path(), &buffers), Some(2));
        let second = req_rx.try_recv().expect("retried request");
        assert_eq!(second.seq, 2);
        assert!(scheduler.queued.is_none());
    }

    #[test]
    fn a_retry_carries_the_buffers_of_its_own_send_time() {
        let dir = workspace();
        let mut buffers = buffers(dir.path());
        let (req_tx, req_rx) = crossbeam_channel::bounded(1);
        let mut scheduler = Scheduler::new(req_tx);

        assert_eq!(scheduler.submit(dir.path(), &buffers), Some(1));
        assert_eq!(scheduler.submit(dir.path(), &buffers), None);
        let first = req_rx.recv().expect("first request");
        assert_eq!(first.overlays[0].text, "def greet(): pass\n");

        assert_eq!(
            buffers.open(
                &doc_uri(
                    crate::convert::path_to_uri(&dir.path().join("a.py"))
                        .unwrap()
                        .as_str()
                ),
                DocVersion::from(2),
                "python",
                "def second(): pass\n".to_string()
            ),
            crate::buffers::OpenOutcome::Indexed
        );

        assert_eq!(scheduler.submit(dir.path(), &buffers), Some(2));
        let retried = req_rx.recv().expect("retried request");
        assert_eq!(
            retried.overlays[0].text, "def second(): pass\n",
            "a retry must carry buffers of its own send time"
        );
    }

    #[test]
    fn a_disconnected_worker_stops_submitting() {
        let dir = workspace();
        let buffers = buffers(dir.path());
        let (req_tx, req_rx) = crossbeam_channel::bounded(1);
        drop(req_rx);
        let mut scheduler = Scheduler::new(req_tx);

        assert_eq!(scheduler.submit(dir.path(), &buffers), None);
        assert!(scheduler.is_gone());

        assert_eq!(scheduler.submit(dir.path(), &buffers), None);
    }
}
