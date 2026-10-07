//! Request cancellation tokens. Only an in-flight request is cancellable: the
//! transport is FIFO, so a remembered cancel set would be unbounded state.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use lsp_server::RequestId;

#[derive(Clone, Default)]
struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

pub struct Registration<'a> {
    cancel: &'a Cancellation,
    id: RequestId,
    token: CancelToken,
}

impl Registration<'_> {
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.cancel.remove(&self.id);
    }
}

#[derive(Default)]
pub struct Cancellation {
    tokens: Mutex<HashMap<RequestId, CancelToken>>,
}

impl Cancellation {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RequestId, CancelToken>> {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn register(&self, id: RequestId) -> Registration<'_> {
        let token = self.lock().entry(id.clone()).or_default().clone();
        Registration {
            cancel: self,
            id,
            token,
        }
    }

    /// Mark an in-flight request as cancelled; any other id is a no-op.
    pub fn cancel(&self, id: &RequestId) {
        match self.lock().get(id) {
            Some(token) => token.cancel(),
            None => tracing::debug!(?id, "cancel for a request that is not in flight"),
        }
    }

    fn remove(&self, id: &RequestId) {
        self.lock().remove(id);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_sets_only_the_target_token() {
        let cancellation = Cancellation::default();
        let first = cancellation.register(RequestId::from(1));
        let second = cancellation.register(RequestId::from(2));

        cancellation.cancel(&RequestId::from(1));
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());

        drop(first);
        assert_eq!(cancellation.len(), 1);
    }

    #[test]
    fn an_unknown_id_cancel_is_a_no_op() {
        let cancellation = Cancellation::default();
        let id = RequestId::from(99);

        cancellation.cancel(&id);
        cancellation.cancel(&id);

        assert_eq!(cancellation.len(), 0);
        let later = cancellation.register(id);
        assert!(
            !later.is_cancelled(),
            "a cancel for a request that never arrived must not cancel a later one"
        );
    }

    #[test]
    fn dropping_a_registration_cleans_the_registry() {
        let cancellation = Cancellation::default();
        let guard = cancellation.register(RequestId::from(5));
        assert_eq!(cancellation.len(), 1);
        drop(guard);
        assert_eq!(cancellation.len(), 0);
    }
}
