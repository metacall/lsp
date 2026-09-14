//! Request cancellation tokens. Only an in-flight request is cancellable: the
//! transport is FIFO, so a remembered cancel set would be unbounded state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lsp_server::RequestId;

#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
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
    tokens: RefCell<HashMap<RequestId, CancelToken>>,
}

impl Cancellation {
    pub fn register(&self, id: &RequestId) -> Registration<'_> {
        let token = self
            .tokens
            .borrow_mut()
            .entry(id.clone())
            .or_default()
            .clone();
        Registration {
            cancel: self,
            id: id.clone(),
            token,
        }
    }

    /// Mark an in-flight request as cancelled; any other id is a no-op.
    pub fn cancel(&self, id: &RequestId) {
        match self.tokens.borrow().get(id) {
            Some(token) => token.cancel(),
            None => tracing::debug!(?id, "cancel for a request that is not in flight"),
        }
    }

    fn remove(&self, id: &RequestId) {
        self.tokens.borrow_mut().remove(id);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.tokens.borrow().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tokens.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_sets_only_the_target_token() {
        let cancellation = Cancellation::default();
        let first = cancellation.register(&RequestId::from(1));
        let second = cancellation.register(&RequestId::from(2));

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

        assert!(
            cancellation.is_empty(),
            "an unknown id must not be remembered"
        );
        let later = cancellation.register(&id);
        assert!(
            !later.is_cancelled(),
            "a cancel for a request that never arrived must not cancel a later one"
        );
    }

    #[test]
    fn dropping_a_registration_cleans_the_registry() {
        let cancellation = Cancellation::default();
        let id = RequestId::from(5);
        let guard = cancellation.register(&id);
        assert_eq!(cancellation.len(), 1);
        drop(guard);
        assert!(cancellation.is_empty());
    }
}
