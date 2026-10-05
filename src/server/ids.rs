//! Canonical transport identifiers: one spelling per concern.
use lsp_server::RequestId;
use lsp_types::NumberOrString;

pub(crate) const SERVER_NAME: &str = "meta-call-lsp";

/// The engine attribution used as diagnostic source and result-id prefix.
pub(crate) const DIAGNOSTIC_SOURCE: &str = "meta-ast";

pub(crate) const WATCHED_FILES_REGISTRATION: &str = "meta-call-lsp-watched-files";

pub(crate) fn watched_files_request_id() -> RequestId {
    RequestId::from("meta-call-lsp-register-watched-files".to_string())
}

pub(crate) struct ProgressId(pub u64);

impl ProgressId {
    pub(crate) fn token(self) -> NumberOrString {
        NumberOrString::String(format!("{SERVER_NAME}-reindex-{}", self.0))
    }

    pub(crate) fn ack(self) -> RequestId {
        RequestId::from(format!("{SERVER_NAME}-progress-{}", self.0))
    }
}

impl Copy for ProgressId {}
impl Clone for ProgressId {
    fn clone(&self) -> Self {
        *self
    }
}
