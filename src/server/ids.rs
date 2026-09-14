//! Canonical transport identifiers: one spelling per concern.
use lsp_server::RequestId;

pub(crate) const SERVER_NAME: &str = "meta-call-lsp";

/// The engine attribution used as diagnostic source and result-id prefix.
pub(crate) const DIAGNOSTIC_SOURCE: &str = "meta-ast";

pub(crate) const WATCHED_FILES_REGISTRATION: &str = "meta-call-lsp-watched-files";

pub(crate) fn watched_files_request_id() -> RequestId {
    RequestId::from("meta-call-lsp-register-watched-files".to_string())
}

pub(crate) fn reindex_progress_token(number: u64) -> String {
    format!("{SERVER_NAME}-reindex-{number}")
}

pub(crate) fn reindex_progress_ack(number: u64) -> RequestId {
    RequestId::from(format!("{SERVER_NAME}-progress-{number}"))
}
