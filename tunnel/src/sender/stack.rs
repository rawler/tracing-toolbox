//! Per-thread stack of entered spans.
//!
//! Adapted from the span stack of the `tracing-subscriber` registry
//! (`tracing-subscriber/src/registry/stack.rs`, MIT license, Copyright (c) Tokio Contributors).

use tracing_core::span::Id;

#[derive(Debug)]
struct ContextId {
    id: Id,
    /// Whether the span was already entered when this entry was pushed (i.e., it is re-entered).
    duplicate: bool,
}

/// Stack of spans entered on a single thread.
#[derive(Debug, Default)]
pub(super) struct SpanStack {
    stack: Vec<ContextId>,
}

impl SpanStack {
    pub(super) fn push(&mut self, id: Id) {
        let duplicate = self.stack.iter().any(|ctx_id| ctx_id.id == id);
        self.stack.push(ContextId { id, duplicate });
    }

    /// Removes the latest entry for the span, which is not necessarily on top of the stack
    /// (spans may be exited out of order).
    pub(super) fn pop(&mut self, id: &Id) {
        if let Some(idx) = self.stack.iter().rposition(|ctx_id| ctx_id.id == *id) {
            self.stack.remove(idx);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// Returns the current span, skipping re-entered spans (same as the `tracing-subscriber`
    /// registry).
    pub(super) fn current(&self) -> Option<&Id> {
        self.stack
            .iter()
            .rev()
            .find_map(|ctx_id| (!ctx_id.duplicate).then_some(&ctx_id.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_last_span() {
        let mut stack = SpanStack::default();
        let id = Id::from_u64(1);
        stack.push(id.clone());
        assert_eq!(stack.current(), Some(&id));

        stack.pop(&id);
        assert!(stack.is_empty());
        assert_eq!(stack.current(), None);
    }

    #[test]
    fn pop_first_span() {
        let mut stack = SpanStack::default();
        stack.push(Id::from_u64(1));
        stack.push(Id::from_u64(2));

        stack.pop(&Id::from_u64(1));
        assert_eq!(stack.current(), Some(&Id::from_u64(2)));
    }

    #[test]
    fn reentered_span_is_skipped() {
        let mut stack = SpanStack::default();
        stack.push(Id::from_u64(1));
        stack.push(Id::from_u64(2));
        stack.push(Id::from_u64(1));
        assert_eq!(stack.current(), Some(&Id::from_u64(2)));

        stack.pop(&Id::from_u64(1));
        assert_eq!(stack.current(), Some(&Id::from_u64(2)));
        stack.pop(&Id::from_u64(2));
        assert_eq!(stack.current(), Some(&Id::from_u64(1)));
    }
}
