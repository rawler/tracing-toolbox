//! `Mutex`-based synchronization.

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, MutexGuard, PoisonError},
    thread::{self, ThreadId},
};

use tracing_core::{Metadata, span::Id};

use super::{EventSync, metadata_id, stack::SpanStack};
use crate::{CallSiteData, MetadataId, ROOT_SPAN_ID, RawSpanId, TracingEvent};

/// Mutex-based [`EventSync`] implementation that can be used by [`TracingEventSender`](super::TracingEventSender).
///
/// Besides ordering call site registration, this implementation tracks the current span
/// for each thread and resolves contextual parents of spans and events on the sender side.
/// Thus, emitted events always specify their parent explicitly ([`ROOT_SPAN_ID`] for spans / events
/// without a parent), and the receiver can reconstruct the span hierarchy even if spans and events
/// are emitted from multiple threads concurrently.
///
/// # Performance
///
/// All state is guarded by a single mutex, which is locked once per new span / event, and once
/// per span entry / exit. Thus, heavily multi-threaded tracing (e.g., async runtimes entering
/// instrumented spans on each poll) may contend on this mutex.
///
/// Span stacks are tracked per thread and removed once a thread exits all its spans. If a thread
/// terminates with spans still entered (e.g., because a span guard was leaked), its stack is
/// retained for the lifetime of the sender.
#[derive(Debug, Default)]
pub struct Synced(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    registered_callsites: HashSet<MetadataId>,
    /// Stacks of entered spans per thread. Threads without entered spans are not present.
    span_stacks: HashMap<ThreadId, SpanStack>,
}

impl State {
    /// Ensures that the callsite for the given metadata is registered. This prevents race conditions
    /// where `NewSpan` or `NewEvent` events arrive before their `NewCallSite` dependencies.
    fn ensure_callsite_registered(
        &mut self,
        metadata: &'static Metadata<'static>,
        sender: impl Fn(TracingEvent),
    ) {
        let metadata_id = metadata_id(metadata);
        if !self.registered_callsites.contains(&metadata_id) {
            // Send NewCallSite event before marking as registered
            sender(TracingEvent::NewCallSite {
                id: metadata_id,
                data: CallSiteData::from(metadata),
            });
            self.registered_callsites.insert(metadata_id);
        }
    }

    fn resolve_parent(&self, explicit: Option<&Id>, is_contextual: bool) -> RawSpanId {
        if let Some(explicit) = explicit {
            explicit.into_u64()
        } else if is_contextual {
            let current = self
                .span_stacks
                .get(&thread::current().id())
                .and_then(SpanStack::current);
            current.map_or(ROOT_SPAN_ID, Id::into_u64)
        } else {
            ROOT_SPAN_ID
        }
    }
}

impl Synced {
    /// Locks the state, ignoring poisoning. The state stays consistent if the `on_event` hook
    /// panics (at worst, a call site is registered again), while panicking here could abort
    /// the process, since `exit()` is called when dropping span guards, possibly during unwinding.
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EventSync for Synced {
    fn register_callsite(
        &self,
        metadata: &'static Metadata<'static>,
        sender: impl Fn(TracingEvent),
    ) {
        // We treat both trait methods in the same way since they may arrive out of order.
        self.state().ensure_callsite_registered(metadata, sender);
    }

    fn on_new_span_or_event(
        &self,
        metadata: &'static Metadata<'static>,
        explicit_parent: Option<&Id>,
        is_contextual: bool,
        sender: impl Fn(TracingEvent),
    ) -> Option<RawSpanId> {
        let mut state = self.state();
        state.ensure_callsite_registered(metadata, sender);
        Some(state.resolve_parent(explicit_parent, is_contextual))
    }

    fn enter(&self, span: &Id) {
        let mut state = self.state();
        state
            .span_stacks
            .entry(thread::current().id())
            .or_default()
            .push(span.clone());
    }

    fn exit(&self, span: &Id) {
        let mut state = self.state();
        let thread_id = thread::current().id();
        if let Some(stack) = state.span_stacks.get_mut(&thread_id) {
            stack.pop(span);
            if stack.is_empty() {
                state.span_stacks.remove(&thread_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_parent(sync: &Synced, explicit: Option<&Id>, is_contextual: bool) -> RawSpanId {
        sync.state().resolve_parent(explicit, is_contextual)
    }

    #[test]
    fn span_stacks_are_removed_when_empty() {
        let sync = Synced::default();
        let (outer, inner) = (Id::from_u64(1), Id::from_u64(2));
        sync.enter(&outer);
        sync.enter(&inner);
        assert_eq!(resolve_parent(&sync, None, true), 2);

        sync.exit(&inner);
        assert_eq!(resolve_parent(&sync, None, true), 1);
        sync.exit(&outer);
        assert_eq!(resolve_parent(&sync, None, true), ROOT_SPAN_ID);
        assert!(sync.state().span_stacks.is_empty());
    }

    #[test]
    fn poisoned_state_is_still_usable() {
        static METADATA: Metadata<'static> = tracing_core::metadata! {
            name: "test",
            target: module_path!(),
            level: tracing_core::Level::INFO,
            fields: &[],
            callsite: &CALLSITE,
            kind: tracing_core::metadata::Kind::EVENT,
        };
        struct Callsite;
        impl tracing_core::Callsite for Callsite {
            fn set_interest(&self, _: tracing_core::Interest) {}
            fn metadata(&self) -> &Metadata<'_> {
                &METADATA
            }
        }
        static CALLSITE: Callsite = Callsite;

        let sync = Synced::default();
        let span = Id::from_u64(1);
        sync.enter(&span);
        // Panic in the `on_event` hook while the state is locked, which poisons the mutex.
        let panic_result = std::panic::catch_unwind(|| {
            sync.register_callsite(&METADATA, |_| panic!("on_event panicked"));
        });
        assert!(panic_result.is_err());
        assert!(sync.0.is_poisoned());

        // Must not panic: this would abort the process if called from a span guard during unwinding.
        sync.exit(&span);
        assert!(sync.state().span_stacks.is_empty());
        // The call site was not registered, so it is registered again.
        let registered = std::cell::Cell::new(0);
        sync.register_callsite(&METADATA, |_| registered.set(registered.get() + 1));
        assert_eq!(registered.get(), 1);
    }

    #[test]
    fn explicit_parents_are_preserved() {
        let sync = Synced::default();
        sync.enter(&Id::from_u64(1));
        assert_eq!(resolve_parent(&sync, Some(&Id::from_u64(5)), false), 5);
        assert_eq!(resolve_parent(&sync, None, false), ROOT_SPAN_ID);
    }
}
