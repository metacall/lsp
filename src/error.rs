//! Typed server errors.
use lsp_server::{RequestId, Response, ResponseError};

const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const REQUEST_CANCELLED: i32 = -32800;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("request cancelled")]
    Cancelled,
    #[error(transparent)]
    Engine(#[from] meta_ast::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ServerError {
    pub fn to_response(&self, id: RequestId) -> Response {
        let code = match self {
            ServerError::Protocol(_) => INVALID_PARAMS,
            ServerError::Cancelled => REQUEST_CANCELLED,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_uses_request_cancelled_code() {
        let id = RequestId::from(7);
        let response = ServerError::Cancelled.to_response(id.clone());
        assert_eq!(response.id, id);
        let Err(error) = response.response_result else {
            panic!("cancelled error must be a response error");
        };
        assert_eq!(error.code, -32800);
        assert_eq!(error.message, "request cancelled");
    }
}
