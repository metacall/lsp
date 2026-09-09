//! Request cancellation tokens.
//!
//! The loop records a token per in-flight request. A `$/cancelRequest`
//! notification sets the flag.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lsp_server::RequestId;

/// Shared flag set when a request is cancelled.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// True when the client cancelled the request.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Registry of in-flight request tokens.
#[derive(Default)]
pub struct Cancellation {
    tokens: HashMap<RequestId, CancelToken>,
    pending: HashSet<RequestId>,
}

impl Cancellation {
    /// Register a request and return its token.
    ///
    /// A pending cancellation recorded before registration is honored.
    pub fn register(&mut self, id: &RequestId) -> CancelToken {
        let token = self.tokens.entry(id.clone()).or_default().clone();
        if self.pending.remove(id) {
            token.cancel();
        }
        token
    }

    /// Mark a request as cancelled. Returns true when a pending or in-flight
    /// request token was recorded.
    pub fn cancel(&mut self, id: &RequestId) -> bool {
        if let Some(token) = self.tokens.get(id) {
            token.cancel();
            return true;
        }
        self.pending.insert(id.clone());
        true
    }

    /// Drop a finished request and any pending cancellation for it.
    pub fn remove(&mut self, id: &RequestId) {
        self.tokens.remove(id);
        self.pending.remove(id);
    }

    /// Number of in-flight requests.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// True when no request is in flight.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_sets_only_the_target_token() {
        let mut cancellation = Cancellation::default();
        let first = cancellation.register(&RequestId::from(1));
        let second = cancellation.register(&RequestId::from(2));

        assert!(cancellation.cancel(&RequestId::from(1)));
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());

        cancellation.remove(&RequestId::from(1));
        assert_eq!(cancellation.len(), 1);
    }

    #[test]
    fn late_cancellation_survives_registration() {
        let mut cancellation = Cancellation::default();
        let id = RequestId::from(99);

        assert!(cancellation.cancel(&id));
        let pending = cancellation.register(&id);
        assert!(pending.is_cancelled());

        cancellation.remove(&id);
        let finished = cancellation.register(&id);
        assert!(!finished.is_cancelled());
    }
}
