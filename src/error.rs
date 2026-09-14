//! Typed server errors.
use lsp_server::{RequestId, Response, ResponseError};

/// Failure of one reindex pass.
#[derive(Debug, thiserror::Error)]
pub enum ReindexError {
    /// The engine refused the pass: discovery, extraction or resolution failed.
    #[error("engine reanalysis failed: {0}")]
    Engine(#[source] meta_ast::Error),
    /// The snapshot id space is exhausted; the pass cannot be recorded.
    #[error("snapshot counter exhausted")]
    Exhausted,
}

const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const REQUEST_CANCELLED: i32 = -32800;
const CONTENT_MODIFIED: i32 = -32801;
const REQUEST_FAILED: i32 = -32803;

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
    pub fn to_response(&self, id: RequestId) -> Response {
        let code = match self {
            ServerError::MethodNotFound(_) => METHOD_NOT_FOUND,
            ServerError::InvalidParams(_) => INVALID_PARAMS,
            ServerError::Internal(_) => INTERNAL_ERROR,
            ServerError::Cancelled => REQUEST_CANCELLED,
            ServerError::ContentModified(_) => CONTENT_MODIFIED,
            ServerError::RequestFailed(_) => REQUEST_FAILED,
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
