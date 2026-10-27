//! Tests for synced `TracingEventSender` used from multiple threads concurrently.

use std::{
    fmt, mem,
    sync::{Arc, Barrier, LazyLock, Mutex},
    thread,
};

use tracing_core::{
    Event, Field, Subscriber,
    span::{Attributes, Id},
};
use tracing_subscriber::{Layer, Registry, layer::Context, prelude::*, registry::LookupSpan};
use tracing_tunnel::{TracingEvent, TracingEventReceiver, TracingEventSender};

use super::EVENTS;

/// Emits spans and events from 3 concurrent threads. Threads 1 and 2 each create a span,
/// a child span and an event inside it while the other thread has its spans entered;
/// thread 3 emits an event outside any span.
fn sender_events_from_concurrent_threads() -> Vec<TracingEvent> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_event_sender = events.clone();
    let barrier = Barrier::new(3);
    let event_sender = Arc::new(TracingEventSender::sync(move |event| {
        events_event_sender.lock().unwrap().push(event);
    }));
    thread::scope(|s| {
        for thread_idx in 1..=2 {
            let (event_sender, barrier) = (event_sender.clone(), &barrier);
            s.spawn(move || {
                tracing::subscriber::with_default(event_sender, || {
                    tracing::info_span!("outer", thread = thread_idx).in_scope(|| {
                        barrier.wait();
                        tracing::info_span!("inner", thread = thread_idx).in_scope(|| {
                            tracing::info!(thread = thread_idx);
                        });
                        barrier.wait();
                    });
                });
            });
        }
        let event_sender = event_sender.clone();
        s.spawn(|| {
            tracing::subscriber::with_default(event_sender, || {
                barrier.wait();
                tracing::info!(thread = 3);
                barrier.wait();
            });
        });
    });
    // The sender may still be referenced by `tracing` internals (e.g., during call site
    // registration from other tests), so we don't rely on it being dropped.
    drop(event_sender);
    mem::take(&mut *events.lock().unwrap())
}

/// Records the value of the `thread` field, if present.
fn thread_field(record: impl FnOnce(&mut dyn tracing_core::field::Visit)) -> Option<String> {
    let mut value = None;
    record(&mut |field: &Field, val: &dyn fmt::Debug| {
        if field.name() == "thread" {
            value = Some(format!("{val:?}"));
        }
    });
    value
}

/// Thread a span originates from, stored in span extensions.
struct SpanThread(String);

/// Records the originating thread of the parent for each relayed span and event.
#[derive(Default)]
struct ParentRecorder {
    records: Arc<Mutex<Vec<String>>>,
}

impl ParentRecorder {
    fn record<'a, S: LookupSpan<'a>>(
        &self,
        kind: &str,
        thread: Option<String>,
        parent: Option<tracing_subscriber::registry::SpanRef<'a, S>>,
    ) {
        let thread = thread.unwrap_or_else(|| "?".to_owned());
        let parent_thread = parent.map(|parent| {
            let extensions = parent.extensions();
            extensions
                .get::<SpanThread>()
                .map_or_else(|| "?".to_owned(), |t| t.0.clone())
        });
        self.records.lock().unwrap().push(format!(
            "{kind} from thread {thread}: parent from thread {parent_thread:?}"
        ));
    }
}

impl<S> Layer<S> for ParentRecorder
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).unwrap();
        let thread = thread_field(|visitor| attrs.record(visitor));
        let kind = format!("span {}", span.name());
        self.record(&kind, thread.clone(), span.parent());
        if let Some(thread) = thread {
            span.extensions_mut().insert(SpanThread(thread));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let thread = thread_field(|visitor| event.record(visitor));
        self.record("event", thread, ctx.event_span(event));
    }
}

/// Relays `events` to a subscriber recording parents of the relayed spans / events,
/// optionally inside a host span (i.e., a span on the receiver side).
fn relay(events: Vec<TracingEvent>, host_span: bool) -> Vec<String> {
    let recorder = ParentRecorder::default();
    let records = recorder.records.clone();
    tracing::subscriber::with_default(Registry::default().with(recorder), || {
        let host_span = host_span.then(|| tracing::info_span!("host", thread = "host").entered());
        let mut receiver = TracingEventReceiver::default();
        for event in events {
            receiver.receive(event);
        }
        drop(host_span);
    });

    let mut records = mem::take(&mut *records.lock().unwrap());
    records.retain(|record| !record.starts_with("span host "));
    records.sort();
    records
}

#[test]
fn sender_concurrent_threads() {
    // Ensure `fib` events are recorded first; otherwise, call site events in the snapshot
    // may be missing.
    LazyLock::force(&EVENTS);

    let records = relay(sender_events_from_concurrent_threads(), false);
    assert_eq!(
        records,
        [
            "event from thread 1: parent from thread Some(\"1\")",
            "event from thread 2: parent from thread Some(\"2\")",
            "event from thread 3: parent from thread None",
            "span inner from thread 1: parent from thread Some(\"1\")",
            "span inner from thread 2: parent from thread Some(\"2\")",
            "span outer from thread 1: parent from thread None",
            "span outer from thread 2: parent from thread None",
        ]
    );
}

#[test]
fn sender_concurrent_threads_with_host_span() {
    LazyLock::force(&EVENTS);

    // Spans / events without a parent on the sender side are attached to the host span,
    // even while spans from other sender threads are entered.
    let records = relay(sender_events_from_concurrent_threads(), true);
    assert_eq!(
        records,
        [
            "event from thread 1: parent from thread Some(\"1\")",
            "event from thread 2: parent from thread Some(\"2\")",
            "event from thread 3: parent from thread Some(\"\\\"host\\\"\")",
            "span inner from thread 1: parent from thread Some(\"1\")",
            "span inner from thread 2: parent from thread Some(\"2\")",
            "span outer from thread 1: parent from thread Some(\"\\\"host\\\"\")",
            "span outer from thread 2: parent from thread Some(\"\\\"host\\\"\")",
        ]
    );
}

fn single_threaded_events(sender: impl Subscriber + Send + Sync) {
    tracing::subscriber::with_default(sender, || {
        tracing::info!(thread = 0);
        tracing::info_span!("outer", thread = 0).in_scope(|| {
            tracing::info!(thread = 0);
            tracing::info_span!(parent: None, "detached", thread = 0).in_scope(|| {
                tracing::info!(thread = 0);
            });
        });
    });
}

#[test]
fn synced_and_unsynced_senders_produce_same_hierarchy() {
    LazyLock::force(&EVENTS);

    for host_span in [false, true] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_ = events.clone();
        single_threaded_events(TracingEventSender::new(move |event| {
            events_.lock().unwrap().push(event);
        }));
        let unsynced_events = mem::take(&mut *events.lock().unwrap());

        let events_ = events.clone();
        single_threaded_events(TracingEventSender::sync(move |event| {
            events_.lock().unwrap().push(event);
        }));
        let synced_events = mem::take(&mut *events.lock().unwrap());

        assert_eq!(
            relay(unsynced_events, host_span),
            relay(synced_events, host_span),
            "host_span = {host_span}"
        );
    }
}
