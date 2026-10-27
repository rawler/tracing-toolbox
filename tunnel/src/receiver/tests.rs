//! Tests for tracing event receiver.

use std::borrow::Cow;

use assert_matches::assert_matches;

use super::*;
use crate::{CallSiteKind, TracingLevel};

const CALL_SITE_DATA: CallSiteData = create_call_site(Vec::new());

const fn create_call_site(fields: Vec<Cow<'static, str>>) -> CallSiteData {
    CallSiteData {
        kind: CallSiteKind::Span,
        name: Cow::Borrowed("test"),
        target: Cow::Borrowed("tracing_tunnel"),
        level: TracingLevel::Error,
        module_path: Some(Cow::Borrowed("receiver::tests")),
        file: Some(Cow::Borrowed("tests")),
        line: Some(42),
        fields,
    }
}

#[test]
fn duplicate_call_site_definitions_are_allowed() {
    let events = [
        TracingEvent::NewCallSite {
            id: 0,
            data: CALL_SITE_DATA,
        },
        TracingEvent::NewCallSite {
            id: 0,
            data: CALL_SITE_DATA,
        },
    ];

    let mut receiver = TracingEventReceiver::default();
    for event in events {
        receiver.receive(event);
    }

    let metadata = receiver.persist_metadata();
    assert_eq!(metadata.inner.len(), 1);
}

#[test]
fn unknown_metadata_error() {
    let event = TracingEvent::NewSpan {
        id: 0,
        parent_id: None,
        metadata_id: 0,
        values: TracedValues::new(),
    };
    let mut receiver = TracingEventReceiver::default();
    let err = receiver.try_receive(event).unwrap_err();
    assert_matches!(err, ReceiveError::UnknownMetadataId(0));
}

#[test]
fn unknown_span_errors() {
    let bogus_events = [
        TracingEvent::SpanEntered { id: 1 },
        TracingEvent::SpanExited { id: 1 },
        TracingEvent::SpanDropped { id: 1 },
        TracingEvent::NewSpan {
            id: 42,
            parent_id: Some(1),
            metadata_id: 0,
            values: TracedValues::new(),
        },
        TracingEvent::NewEvent {
            metadata_id: 0,
            parent: Some(1),
            values: TracedValues::new(),
        },
        TracingEvent::ValuesRecorded {
            id: 1,
            values: TracedValues::new(),
        },
    ];

    let mut receiver = TracingEventReceiver::default();
    receiver.receive(TracingEvent::NewCallSite {
        id: 0,
        data: CALL_SITE_DATA,
    });
    for bogus_event in bogus_events {
        let err = receiver.try_receive(bogus_event).unwrap_err();
        assert_matches!(err, ReceiveError::UnknownSpanId(1));
    }
}

#[test]
fn spans_with_allowed_value_lengths() {
    for values_len in 0..=32 {
        println!("values length: {values_len}");

        let mut receiver = TracingEventReceiver::default();
        let fields = (0..values_len)
            .map(|i| Cow::Owned(format!("field{i}")))
            .collect();
        receiver.receive(TracingEvent::NewCallSite {
            id: 0,
            data: create_call_site(fields),
        });

        let values = (0..values_len)
            .map(|i| (format!("field{i}"), TracedValue::Int(i.into())))
            .collect();
        receiver.receive(TracingEvent::NewSpan {
            id: 0,
            parent_id: None,
            metadata_id: 0,
            values,
        });
        receiver.receive(TracingEvent::SpanDropped { id: 0 });
    }
}

#[test]
fn too_many_values_error() {
    let mut receiver = TracingEventReceiver::default();
    receiver.receive(TracingEvent::NewCallSite {
        id: 0,
        data: CALL_SITE_DATA,
    });

    let values = (0..33)
        .map(|i| (format!("field{i}"), TracedValue::Int(i.into())))
        .collect();
    let bogus_event = TracingEvent::NewSpan {
        id: 0,
        parent_id: None,
        metadata_id: 0,
        values,
    };
    let err = receiver.try_receive(bogus_event).unwrap_err();
    assert_matches!(
        err,
        ReceiveError::TooManyValues {
            actual: 33,
            max: 32
        }
    );
}

#[test]
fn receiver_does_not_panic_on_bogus_field() {
    let events = [
        TracingEvent::NewCallSite {
            id: 0,
            data: CALL_SITE_DATA,
        },
        TracingEvent::NewSpan {
            id: 0,
            parent_id: None,
            metadata_id: 0,
            values: TracedValues::from_iter([("i".to_owned(), TracedValue::from(42_i64))]),
        },
    ];

    let mut receiver = TracingEventReceiver::default();
    for event in events {
        receiver.receive(event);
    }
}

#[test]
fn restoring_spans() {
    let metadata = PersistedMetadata {
        inner: HashMap::from_iter([(0, CALL_SITE_DATA)]),
    };
    let spans = PersistedSpans {
        inner: HashMap::from_iter([(
            1,
            SpanData {
                metadata_id: 0,
                parent_id: None,
                ref_count: 1,
                values: TracedValues::new(),
            },
        )]),
    };
    let local_spans = LocalSpans::default();

    let mut receiver = TracingEventReceiver::new(metadata, spans, local_spans);
    visit_and_drop_span(&mut receiver);
}

fn visit_and_drop_span(receiver: &mut TracingEventReceiver) {
    receiver.receive(TracingEvent::SpanEntered { id: 1 });
    assert!(receiver.local_spans.inner.contains_key(&1));

    receiver.receive(TracingEvent::SpanExited { id: 1 });
    receiver.receive(TracingEvent::SpanDropped { id: 1 });
    assert!(!receiver.spans.inner.contains_key(&1));
    assert!(!receiver.local_spans.inner.contains_key(&1));
}

#[test]
fn restoring_span_after_recording_values() {
    let call_site = create_call_site(vec!["i".into()]);
    let metadata = PersistedMetadata {
        inner: HashMap::from_iter([(0, call_site)]),
    };
    let spans = PersistedSpans {
        inner: HashMap::from_iter([(
            1,
            SpanData {
                metadata_id: 0,
                parent_id: None,
                ref_count: 1,
                values: TracedValues::new(),
            },
        )]),
    };
    let local_spans = LocalSpans::default();

    let mut receiver = TracingEventReceiver::new(metadata, spans, local_spans);
    receiver.receive(TracingEvent::ValuesRecorded {
        id: 1,
        values: TracedValues::from_iter([("i".to_owned(), TracedValue::from(42_i64))]),
    });
    assert_eq!(receiver.spans.inner[&1].values["i"], 42_i64);
    assert!(!receiver.local_spans.inner.contains_key(&1));

    visit_and_drop_span(&mut receiver);
}

fn receiver_with_persisted_span() -> TracingEventReceiver {
    let metadata = PersistedMetadata {
        inner: HashMap::from_iter([(0, CALL_SITE_DATA)]),
    };
    let spans = PersistedSpans {
        inner: HashMap::from_iter([(
            1,
            SpanData {
                metadata_id: 0,
                parent_id: None,
                ref_count: 1,
                values: TracedValues::new(),
            },
        )]),
    };
    TracingEventReceiver::new(metadata, spans, LocalSpans::default())
}

#[test]
fn restored_parent_span_is_created_for_child_span() {
    let mut receiver = receiver_with_persisted_span();
    receiver.receive(TracingEvent::NewSpan {
        id: 2,
        parent_id: Some(1),
        metadata_id: 0,
        values: TracedValues::new(),
    });
    assert!(receiver.local_spans.inner.contains_key(&1));
    assert!(receiver.local_spans.inner.contains_key(&2));
}

#[test]
fn restored_parent_span_is_created_for_event() {
    let mut receiver = receiver_with_persisted_span();
    receiver.receive(TracingEvent::NewEvent {
        metadata_id: 0,
        parent: Some(1),
        values: TracedValues::new(),
    });
    assert!(receiver.local_spans.inner.contains_key(&1));
}

#[test]
fn span_entered_multiple_times_is_tracked() {
    let mut receiver = receiver_with_persisted_span();
    receiver.receive(TracingEvent::SpanEntered { id: 1 });
    receiver.receive(TracingEvent::SpanEntered { id: 1 });
    receiver.receive(TracingEvent::SpanExited { id: 1 });
    assert_eq!(receiver.current_execution.entered_span_ids[&1], 1);

    receiver.receive(TracingEvent::SpanExited { id: 1 });
    assert!(receiver.current_execution.entered_span_ids.is_empty());
}

#[test]
fn dropping_entered_span_exits_it() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    #[derive(Default)]
    struct ExitCounter(Arc<AtomicUsize>);

    impl<S: tracing_core::Subscriber> Layer<S> for ExitCounter {
        fn on_exit(&self, _id: &Id, _ctx: Context<'_, S>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let counter = ExitCounter::default();
    let exit_count = counter.0.clone();
    let subscriber = tracing_subscriber::registry().with(counter);
    tracing::subscriber::with_default(subscriber, || {
        let mut receiver = receiver_with_persisted_span();
        receiver.receive(TracingEvent::SpanEntered { id: 1 });
        receiver.receive(TracingEvent::SpanEntered { id: 1 });
        receiver.receive(TracingEvent::SpanDropped { id: 1 });
        assert!(receiver.current_execution.entered_span_ids.is_empty());
        assert_eq!(exit_count.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn parent_cycle_in_persisted_spans_is_an_error() {
    let metadata = PersistedMetadata {
        inner: HashMap::from_iter([(0, CALL_SITE_DATA)]),
    };
    let span = |parent_id| SpanData {
        metadata_id: 0,
        parent_id: Some(parent_id),
        ref_count: 1,
        values: TracedValues::new(),
    };
    let spans = PersistedSpans {
        inner: HashMap::from_iter([(1, span(2)), (2, span(1))]),
    };
    let mut receiver = TracingEventReceiver::new(metadata, spans, LocalSpans::default());

    let err = receiver
        .try_receive(TracingEvent::SpanEntered { id: 1 })
        .unwrap_err();
    assert_matches!(err, ReceiveError::SpanParentCycle(1));
}

#[test]
fn event_with_unknown_metadata_does_not_create_parent_spans() {
    let mut receiver = receiver_with_persisted_span();
    let err = receiver
        .try_receive(TracingEvent::NewEvent {
            metadata_id: 42,
            parent: Some(1),
            values: TracedValues::new(),
        })
        .unwrap_err();
    assert_matches!(err, ReceiveError::UnknownMetadataId(42));
    assert!(!receiver.local_spans.inner.contains_key(&1));
}

#[test]
fn host_span_outlives_its_scope_while_relayed_spans_are_entered() {
    tracing::subscriber::with_default(tracing_subscriber::registry(), || {
        let mut receiver = receiver_with_persisted_span();
        // Create the local span for relayed span 1 outside the host span, so that it doesn't
        // keep the host span alive as its parent.
        receiver.receive(TracingEvent::NewEvent {
            metadata_id: 0,
            parent: Some(1),
            values: TracedValues::new(),
        });
        let host_span = tracing::info_span!("host");
        let host_guard = host_span.enter();
        receiver.receive(TracingEvent::SpanEntered { id: 1 });
        // Close the host span; it must stay alive while relayed span 1 is entered.
        drop(host_guard);
        drop(host_span);

        receiver.receive(TracingEvent::NewSpan {
            id: 2,
            parent_id: Some(ROOT_SPAN_ID),
            metadata_id: 0,
            values: TracedValues::new(),
        });
        receiver.receive(TracingEvent::SpanExited { id: 1 });
        assert!(receiver.current_execution.host_span.is_none());
    });
}
