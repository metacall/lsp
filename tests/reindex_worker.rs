#![expect(clippy::unwrap_used, reason = "a test may abort on setup failure")]
use std::time::Duration;

use meta_call_lsp::buffers::{BufferStore, OpenOutcome};
use meta_call_lsp::index::IndexSnapshot;
use meta_call_lsp::position::Encoding;
use meta_call_lsp::types::DocVersion;

mod common;

use common::{DiskSources, doc_uri};

use meta_call_lsp::reindex::{ReindexReq, spawn_worker};
use meta_call_lsp::{convert, handlers, index};

const APP: &str = "def greet(name):\n    return name\n\n\nresult = greet(\"x\")\n";

fn workspace() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("a.py");
    std::fs::write(&app, APP).unwrap();
    let uri = convert::path_to_uri(&app).unwrap().as_str().to_string();
    (dir, uri)
}

fn req(
    seq: u64,
    dir: &std::path::Path,
    buffers: &BufferStore,
    tx: &crossbeam_channel::Sender<ReindexReq>,
) {
    tx.send(ReindexReq {
        seq,
        root: dir.to_path_buf(),
        overlays: index::collect_inputs(dir, buffers),
    })
    .unwrap();
}

fn recv_until(
    rx: &crossbeam_channel::Receiver<meta_call_lsp::reindex::ReindexResp>,
    seq: u64,
) -> std::sync::Arc<IndexSnapshot> {
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
    assert_eq!(
        buffers.open(
            &doc_uri(uri.as_str()),
            DocVersion::from(1),
            "python",
            APP.to_string()
        ),
        OpenOutcome::Indexed
    );
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
    let worker = spawn_worker(req_rx, resp_tx, index::Reindexer::new());
    for version in 2..=6 {
        assert_eq!(
            buffers.open(
                &doc_uri(uri.as_str()),
                DocVersion::from(version),
                "python",
                format!("{APP}\n\ndef marker_{version}(): pass\n")
            ),
            OpenOutcome::Indexed
        );
        req(version as u64, dir.path(), &buffers, &req_tx);
    }
    let snapshot = recv_until(&resp_rx, 6);
    let sources = DiskSources;
    let mut ctx = handlers::QueryCtx::new(&snapshot, &sources, Encoding::Utf16);
    let symbols = handlers::document_symbols(&mut ctx, &doc_uri(uri.as_str()));
    assert!(symbols.iter().any(|symbol| symbol.name == "marker_6"));
    drop(req_tx);
    worker.join().unwrap();
}

#[test]
fn readers_hold_old_snapshot_during_reindex() {
    let (dir, uri) = workspace();
    let buffers = BufferStore::default();
    let old = index::rebuild_from_inputs(dir.path(), &index::collect_inputs(dir.path(), &buffers))
        .unwrap();
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();
    let worker = spawn_worker(req_rx, resp_tx, index::Reindexer::new());
    let mut next = BufferStore::default();
    assert_eq!(
        next.open(
            &doc_uri(uri.as_str()),
            DocVersion::from(2),
            "python",
            format!("{APP}\n\ndef extra(): pass\n")
        ),
        OpenOutcome::Indexed
    );
    req(1, dir.path(), &next, &req_tx);
    let pos = lsp_types::Position {
        line: 0,
        character: 5,
    };
    let sources = DiskSources;
    let mut ctx = handlers::QueryCtx::new(&old, &sources, Encoding::Utf16);
    assert!(handlers::hover_at(&mut ctx, &doc_uri(uri.as_str()), pos).is_some());
    let snapshot = recv_until(&resp_rx, 1);
    let mut ctx = handlers::QueryCtx::new(&old, &sources, Encoding::Utf16);
    assert!(handlers::hover_at(&mut ctx, &doc_uri(uri.as_str()), pos).is_some());
    let mut ctx = handlers::QueryCtx::new(&snapshot, &sources, Encoding::Utf16);
    let symbols = handlers::document_symbols(&mut ctx, &doc_uri(uri.as_str()));
    assert!(symbols.iter().any(|symbol| symbol.name == "extra"));
    drop(req_tx);
    worker.join().unwrap();
}
