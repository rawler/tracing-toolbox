//! Client-side subscriber.

use core::{
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};

use tracing_core::{
    Event, Interest, Metadata, Subscriber,
    span::{Attributes, Id, Record},
};

#[cfg(feature = "std")]
pub use self::sync::Synced;
use crate::{CallSiteData, MetadataId, ROOT_SPAN_ID, RawSpanId, TracedValues, TracingEvent};

#[cfg(feature = "std")]
mod stack;
#[cfg(feature = "std")]
mod sync;

impl TracingEvent {
    fn new_span(
        span: &Attributes<'_>,
        parent_id: Option<RawSpanId>,
        metadata_id: MetadataId,
        id: RawSpanId,
    ) -> Self {
        Self::NewSpan {
            id,
            parent_id,
            metadata_id,
            values: TracedValues::from_values(span.values()),
        }
    }

    fn values_recorded(id: RawSpanId, values: &Record<'_>) -> Self {
        Self::ValuesRecorded {
            id,
            values: TracedValues::from_record(values),
        }
    }

    fn new_event(event: &Event<'_>, parent: Option<RawSpanId>, metadata_id: MetadataId) -> Self {
        Self::NewEvent {
            metadata_id,
            parent,
            values: TracedValues::from_event(event),
        }
    }
}

/// Event synchronization used by [`TracingEventSender`].
///
/// Synchronization might be necessary in a multithreaded environments, where events may arrive from
/// different threads out of order, and where the current span is thread-specific.
/// This functionality is encapsulated in the (`std`-dependent) [`Synced`] implementation.
///
/// For single-threaded environments (e.g., WASM), there is the no-op `()` implementation.
pub trait EventSync: 'static + Send + Sync {
    /// Called when a new callsite event arrives.
    #[doc(hidden)] // implementation detail
    fn register_callsite(
        &self,
        metadata: &'static Metadata<'static>,
        sender: impl Fn(TracingEvent),
    );

    /// Called when a new span or event arrives. Ensures that its call site is registered,
    /// and returns the parent ID to send for the span / event.
    #[doc(hidden)] // implementation detail
    fn on_new_span_or_event(
        &self,
        metadata: &'static Metadata<'static>,
        explicit_parent: Option<&Id>,
        is_contextual: bool,
        sender: impl Fn(TracingEvent),
    ) -> Option<RawSpanId>;

    /// Called when a span is entered.
    #[doc(hidden)] // implementation detail
    fn enter(&self, _span: &Id) {
        // Do nothing by default
    }

    /// Called when a span is exited.
    #[doc(hidden)] // implementation detail
    fn exit(&self, _span: &Id) {
        // Do nothing by default
    }
}

/// Default implementation that does not perform any synchronization.
impl EventSync for () {
    fn register_callsite(
        &self,
        metadata: &'static Metadata<'static>,
        sender: impl Fn(TracingEvent),
    ) {
        sender(TracingEvent::NewCallSite {
            id: metadata_id(metadata),
            data: CallSiteData::from(metadata),
        });
    }

    /// Does not register the call site, and leaves contextual parents for the receiver to resolve.
    fn on_new_span_or_event(
        &self,
        _metadata: &'static Metadata<'static>,
        explicit_parent: Option<&Id>,
        is_contextual: bool,
        _sender: impl Fn(TracingEvent),
    ) -> Option<RawSpanId> {
        match explicit_parent {
            Some(parent) => Some(parent.into_u64()),
            None if is_contextual => None,
            None => Some(ROOT_SPAN_ID),
        }
    }
}

fn metadata_id(metadata: &'static Metadata<'static>) -> MetadataId {
    ptr::from_ref(metadata) as MetadataId
}

/// Tracing [`Subscriber`] that converts tracing events into (de)serializable [presentation]
/// that can be sent elsewhere using a customizable hook.
///
/// As an example, this subscriber is used in the [Tardigrade client library] to send
/// workflow traces to the host via a WASM import function.
///
/// # Examples
///
/// See [crate-level docs](index.html) for an example of usage.
///
/// [presentation]: TracingEvent
/// [Tardigrade client library]: https://github.com/slowli/tardigrade
#[derive(Debug)]
pub struct TracingEventSender<F = fn(TracingEvent), S = ()> {
    next_span_id: AtomicU32,
    on_event: F,
    sync: S,
}

impl<F: Fn(TracingEvent) + 'static> TracingEventSender<F> {
    /// Creates a subscriber with the specified "on event" hook.
    pub fn new(on_event: F) -> Self {
        Self {
            next_span_id: AtomicU32::new(1), // 0 is invalid span ID
            on_event,
            sync: (),
        }
    }
}

#[cfg(feature = "std")]
impl<F: Fn(TracingEvent) + 'static> TracingEventSender<F, Synced> {
    /// Creates a subscriber with the specified "on event" hook and synchronized event processing.
    ///
    /// The hook may be called while holding a lock shared by all threads (this ensures that call
    /// sites are registered before they are referenced), so it should not block for long,
    /// and must not emit tracing events / spans to this subscriber, which would deadlock.
    pub fn sync(on_event: F) -> Self {
        Self {
            next_span_id: AtomicU32::new(1), // 0 is invalid span ID
            on_event,
            sync: Synced::default(),
        }
    }
}

impl<F: Fn(TracingEvent) + 'static, S: EventSync> TracingEventSender<F, S> {
    fn send(&self, event: TracingEvent) {
        (self.on_event)(event);
    }
}

impl<F: Fn(TracingEvent) + 'static, S: EventSync> Subscriber for TracingEventSender<F, S> {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        // Ensure callsite is registered synchronously
        self.sync.register_callsite(metadata, &self.on_event);
        Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        // Ensure callsite is registered before sending NewSpan event.
        // Practice shows that the caller may not synchronize its register_callsite calls,
        // allowing a new_span call to take effect before the registration completes.
        // We guarantee that references are valid, even when multithreaded.
        let parent_id = self.sync.on_new_span_or_event(
            span.metadata(),
            span.parent(),
            span.is_contextual(),
            &self.on_event,
        );

        let metadata_id = metadata_id(span.metadata());
        let span_id = u64::from(self.next_span_id.fetch_add(1, Ordering::SeqCst));
        self.send(TracingEvent::new_span(
            span,
            parent_id,
            metadata_id,
            span_id,
        ));
        Id::from_u64(span_id)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        self.send(TracingEvent::values_recorded(span.into_u64(), values));
    }

    fn record_follows_from(&self, span: &Id, follows: &Id) {
        self.send(TracingEvent::FollowsFrom {
            id: span.into_u64(),
            follows_from: follows.into_u64(),
        });
    }

    fn event(&self, event: &Event<'_>) {
        // Ensure callsite is registered before sending NewEvent.
        // Practice shows that the caller may not synchronize its register_callsite calls,
        // allowing an event call to take effect before the registration completes.
        // We guarantee that references are valid, even when multi-threaded.
        let parent = self.sync.on_new_span_or_event(
            event.metadata(),
            event.parent(),
            event.is_contextual(),
            &self.on_event,
        );

        let metadata_id = metadata_id(event.metadata());
        self.send(TracingEvent::new_event(event, parent, metadata_id));
    }

    fn enter(&self, span: &Id) {
        self.sync.enter(span);
        self.send(TracingEvent::SpanEntered {
            id: span.into_u64(),
        });
    }

    fn exit(&self, span: &Id) {
        self.sync.exit(span);
        self.send(TracingEvent::SpanExited {
            id: span.into_u64(),
        });
    }

    fn clone_span(&self, span: &Id) -> Id {
        self.send(TracingEvent::SpanCloned {
            id: span.into_u64(),
        });
        span.clone()
    }

    fn try_close(&self, span: Id) -> bool {
        self.send(TracingEvent::SpanDropped {
            id: span.into_u64(),
        });
        false
    }
}
