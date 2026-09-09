use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use meta_call_lsp::buffers::BufferStore;
use meta_call_lsp::index::{IndexSnapshot, SourceText};
use meta_call_lsp::position::Encoding;
use meta_call_lsp::reindex::{ReindexReq, spawn_worker};
use meta_call_lsp::{convert, handlers, index};

const APP: &str = "def greet(name):\n    return name\n\n\nresult = greet(\"x\")\n";

struct DiskSources;

impl SourceText for DiskSources {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        std::fs::read_to_string(path).ok().map(Cow::Owned)
    }
}

fn workspace() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("a.py");
    std::fs::write(&app, APP).unwrap();
    let uri = convert::path_to_uri(&app).unwrap().as_str().to_string();
    (dir, uri)
}

fn req(
    seq: u64,
    raw: u32,
    dir: &std::path::Path,
    buffers: &BufferStore,
    tx: &crossbeam_channel::Sender<ReindexReq>,
) {
    tx.send(ReindexReq {
        seq,
        snapshot_raw: raw,
        root: dir.to_path_buf(),
        overlays: index::collect_inputs(dir, buffers),
    })
    .unwrap();
}

fn recv_until(
    rx: &crossbeam_channel::Receiver<meta_call_lsp::reindex::ReindexResp>,
    seq: u64,
) -> IndexSnapshot {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for seq {seq}");
        let resp = rx.recv_timeout(remaining).unwrap();
        if resp.seq == seq {
            return resp.result.unwrap();
        }
    }
}

#[test]
fn burst_coalesces_to_latest() {
    let (dir, uri) = workspace();
    let mut buffers = BufferStore::default();
    assert!(buffers.open(uri.as_str(), 1, "python", APP.to_string()));
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
    let worker = spawn_worker(req_rx, resp_tx);
    for version in 2..=6 {
        assert!(buffers.open(
            uri.as_str(),
            version,
            "python",
            format!("{APP}\n\ndef marker_{version}(): pass\n")
        ));
        req(
            version as u64,
            version as u32,
            dir.path(),
            &buffers,
            &req_tx,
        );
    }
    let snapshot = recv_until(&resp_rx, 6);
    let sources = DiskSources;
    let symbols = handlers::document_symbols(&snapshot, &sources, uri.as_str(), Encoding::Utf16);
    assert!(symbols.iter().any(|symbol| symbol.name == "marker_6"));
    drop(req_tx);
    worker.join().unwrap();
}

#[test]
fn readers_hold_old_snapshot_during_reindex() {
    let (dir, uri) = workspace();
    let buffers = BufferStore::default();
    let old = Arc::new(
        index::rebuild_from_inputs(dir.path(), &index::collect_inputs(dir.path(), &buffers), 1)
            .unwrap(),
    );
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
    let worker = spawn_worker(req_rx, resp_tx);
    let mut next = BufferStore::default();
    assert!(next.open(
        uri.as_str(),
        2,
        "python",
        format!("{APP}\n\ndef extra(): pass\n")
    ));
    req(1, 2, dir.path(), &next, &req_tx);
    let pos = lsp_types::Position {
        line: 0,
        character: 5,
    };
    let sources = DiskSources;
    assert!(handlers::hover_at(&old, &sources, uri.as_str(), pos, Encoding::Utf16).is_some());
    let snapshot = recv_until(&resp_rx, 1);
    assert!(handlers::hover_at(&old, &sources, uri.as_str(), pos, Encoding::Utf16).is_some());
    let symbols = handlers::document_symbols(&snapshot, &sources, uri.as_str(), Encoding::Utf16);
    assert!(symbols.iter().any(|symbol| symbol.name == "extra"));
    drop(req_tx);
    worker.join().unwrap();
}
