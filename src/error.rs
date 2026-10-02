//! Typed server errors.
use std::any::Any;

use lsp_server::{ErrorCode, RequestId, Response, ResponseError};

/// Failure of one reindex pass.
#[derive(Debug, thiserror::Error)]
pub enum ReindexError {
    /// The engine refused the pass: discovery, extraction or resolution failed.
    #[error("engine reanalysis failed: {0}")]
    Engine(#[source] meta_ast::Error),
    /// The snapshot id space is exhausted; the pass cannot be recorded.
    #[error("snapshot counter exhausted")]
    Exhausted,
    #[error("reindex worker panicked: {0}")]
    Panicked(String),
}

pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .copied()
        .map(str::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .or_else(|| {
            payload
                .downcast_ref::<Box<dyn std::error::Error + Send>>()
                .map(|error| error.to_string())
        })
        .unwrap_or_else(|| "unknown panic payload".to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("protocol: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("request cancelled")]
    Cancelled,
    /// The index does not describe the document version the request saw.
    #[error("content modified: {0}")]
    ContentModified(String),
    /// The index cannot answer at all, for example after a worker loss.
    #[error("request failed: {0}")]
    RequestFailed(String),
}

impl ServerError {
    pub fn not_indexed(uri: &crate::types::DocUri) -> Self {
        Self::RequestFailed(format!("document is not indexed: {uri}"))
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::RequestFailed(format!("index unavailable: {}", reason.into()))
    }

    pub fn outside_root(uri: &crate::types::DocUri) -> Self {
        Self::RequestFailed(format!("document is outside the indexed root: {uri}"))
    }

    pub fn to_response(&self, id: RequestId) -> Response {
        let code = match self {
            ServerError::MethodNotFound(_) => ErrorCode::MethodNotFound as i32,
            ServerError::InvalidParams(_) => ErrorCode::InvalidParams as i32,
            ServerError::Internal(_) => ErrorCode::InternalError as i32,
            ServerError::Cancelled => ErrorCode::RequestCanceled as i32,
            ServerError::ContentModified(_) => ErrorCode::ContentModified as i32,
            ServerError::RequestFailed(_) => ErrorCode::RequestFailed as i32,
        };
        Response {
            id,
            response_result: Err(ResponseError {
                code,
                message: self.to_string(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_code(error: &ServerError) -> i32 {
        let response = error.to_response(RequestId::from(1));
        let Err(error) = response.response_result else {
            panic!("error must produce a response error");
        };
        error.code
    }

    #[test]
    fn every_variant_maps_to_its_json_rpc_code() {
        assert_eq!(
            error_code(&ServerError::MethodNotFound("x".to_string())),
            -32601
        );
        assert_eq!(
            error_code(&ServerError::InvalidParams("x".to_string())),
            -32602
        );
        assert_eq!(error_code(&ServerError::Internal("x".to_string())), -32603);
        assert_eq!(error_code(&ServerError::Cancelled), -32800);
        assert_eq!(
            error_code(&ServerError::ContentModified("x".to_string())),
            -32801
        );
        assert_eq!(
            error_code(&ServerError::RequestFailed("x".to_string())),
            -32803
        );
    }

    #[test]
    fn cancelled_response_carries_the_request_id() {
        let id = RequestId::from(7);
        let response = ServerError::Cancelled.to_response(id.clone());
        assert_eq!(response.id, id);
        let Err(error) = response.response_result else {
            panic!("cancelled error must be a response error");
        };
        assert_eq!(error.message, "request cancelled");
    }
}
