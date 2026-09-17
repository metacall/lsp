//! MetaCall polyglot language server.
pub mod buffers;
pub mod cancel;
pub mod convert;
pub mod error;
pub mod handlers;
pub mod index;
pub mod position;
pub mod reindex;
pub mod server;
pub mod shards;
pub mod types;

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::Path;
    use std::sync::Arc;

    use crossbeam_channel::{Receiver, unbounded};

    use crate::index::{IndexSnapshot, rebuild_from_inputs};
    use crate::position::Encoding;
    use crate::reindex::ReindexReq;
    use crate::server::session::Session;
    use crate::types::{DocUri, RootDir};

    pub(crate) fn doc_uri(value: &str) -> DocUri {
        DocUri::try_from(value).expect("document URI")
    }

    /// Session over a cold rebuild of `dir`, plus the receiver its scheduler feeds.
    pub(crate) fn session(dir: &Path, progress_supported: bool) -> (Session, Receiver<ReindexReq>) {
        session_with(
            dir,
            rebuild_from_inputs(dir, &[]).expect("rebuild"),
            progress_supported,
        )
    }

    /// Session over one explicit snapshot, plus the receiver its scheduler feeds.
    pub(crate) fn session_with(
        dir: &Path,
        snapshot: Arc<IndexSnapshot>,
        progress_supported: bool,
    ) -> (Session, Receiver<ReindexReq>) {
        let (req_tx, req_rx) = unbounded();
        let session = Session::new(
            RootDir::try_from(dir).expect("root"),
            Encoding::Utf16,
            snapshot,
            req_tx,
            progress_supported,
        );
        (session, req_rx)
    }
}
