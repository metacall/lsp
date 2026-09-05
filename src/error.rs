//! Typed server errors.
use lsp_server::{RequestId, Response, ResponseError};

const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("protocol: {0}")]
    Protocol(String),
    #[error(transparent)]
    Engine(#[from] meta_ast::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ServerError {
    pub fn to_response(&self, id: RequestId) -> Response {
        let code = match self {
            ServerError::Protocol(_) => INVALID_PARAMS,
            ServerError::Engine(_) | ServerError::Io(_) => INTERNAL_ERROR,
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
