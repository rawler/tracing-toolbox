//! `TracingEvent` receiver.

use std::{
    collections::{HashMap, HashSet},
    error, fmt, mem,
};

use serde::{Deserialize, Serialize};
use tracing_core::{
    Event, Field, Metadata,
    dispatcher::{self, Dispatch},
    field::{self, FieldSet, Value, ValueSet},
    span::{Attributes, Id, Record},
};

use self::arena::ARENA;
use crate::{
    CallSiteData, MetadataId, ROOT_SPAN_ID, RawSpanId, TracedValue, TracedValues, TracingEvent,
};

mod arena;
#[cfg(test)]
mod tests;

enum CowValue<'a> {
    Borrowed(&'a dyn Value),
    Owned(Box<dyn Value + 'a>),
}

impl fmt::Debug for CowValue<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Borrowed(_) => formatter.debug_struct("Borrowed").finish_non_exhaustive(),
            Self::Owned(_) => formatter.debug_struct("Owned").finish_non_exhaustive(),
        }
    }
}

impl<'a> CowValue<'a> {
    fn as_ref(&self) -> &(dyn Value + 'a) {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(boxed) => boxed.as_ref(),
        }
    }
}

impl TracedValue {
    fn as_value(&self) -> CowValue<'_> {
        CowValue::Borrowed(match self {
            Self::Bool(value) => value,
            Self::Int(value) => value,
            Self::UInt(value) => value,
            Self::Float(value) => value,
            Self::String(value) => value,
            Self::Object(value) => return CowValue::Owned(Box::new(field::debug(value))),
            Self::Error(err) => {
                let err = err as &(dyn error::Error + 'static);
                return CowValue::Owned(Box::new(err));
            }
        })
    }
}

/// Local parent of a span or event being relayed.
#[derive(Debug)]
enum LocalParent {
    /// Contextual parent, i.e. the current span on the receiver side.
    Contextual,
    /// No parent at all. Used for spans / events without a parent on the sender side
    /// if there was no host span to attach them to.
    None,
    Span(Id),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpanData {
    metadata_id: MetadataId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_id: Option<RawSpanId>,
    ref_count: usize,
    values: TracedValues<String>,
}

/// Information about span / event [`Metadata`] that is [serializable] and thus
/// can be persisted across multiple [`TracingEventReceiver`] lifetimes.
///
/// `PersistedMetadata` logically corresponds to a program executable (e.g., a WASM module),
/// not to its particular execution (e.g., a WASM module instance).
/// Multiple executions of the same executable can (and optimally should)
/// share `PersistedMetadata`.
///
/// [serializable]: https://docs.rs/serde/1/serde
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PersistedMetadata {
    inner: HashMap<MetadataId, CallSiteData>,
}

impl PersistedMetadata {
    /// Returns the number of metadata entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Checks whether this metadata collection is empty (i.e., no metadata was recorded yet).
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Iterates over contained call site metadata together with the corresponding
    /// [`MetadataId`]s.
    pub fn iter(&self) -> impl Iterator<Item = (MetadataId, &CallSiteData)> + '_ {
        self.inner.iter().map(|(id, data)| (*id, data))
    }

    /// Merges entries from another `PersistedMetadata` instance.
    pub fn extend(&mut self, other: Self) {
        self.inner.extend(other.inner);
    }
}

/// Information about alive tracing spans for a particular execution that is (de)serializable and
/// can be persisted across multiple [`TracingEventReceiver`] lifetimes.
///
/// Unlike [`PersistedMetadata`], `PersistedSpans` are specific to an executable invocation
/// (e.g., a WASM module instance). Compared to [`LocalSpans`], `PersistedSpans` have
/// the lifetime of the execution and not the host [`Subscriber`].
///
/// [`Subscriber`]: tracing_core::Subscriber
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PersistedSpans {
    inner: HashMap<RawSpanId, SpanData>,
}

impl PersistedSpans {
    /// Returns the number of alive spans.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Checks whether this span collection is empty (i.e., no spans were recorded yet).
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// [`Subscriber`]-specific information about tracing spans for a particular execution
/// (e.g., a WASM module instance).
///
/// Unlike [`PersistedSpans`], this information is not serializable and lives along with
/// the host [`Subscriber`]. It is intended to be placed in something like
/// (an initially empty) `HashMap<K, LocalSpans>`, where `K` denotes the execution ID.
///
/// [`Subscriber`]: tracing_core::Subscriber
#[derive(Debug, Default)]
pub struct LocalSpans {
    inner: HashMap<RawSpanId, Id>,
}

/// Error processing a [`TracingEvent`] by a [`TracingEventReceiver`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ReceiveError {
    /// The event contains a reference to an unknown [`Metadata`] ID.
    UnknownMetadataId(MetadataId),
    /// The event contains a reference to an unknown span ID.
    UnknownSpanId(RawSpanId),
    /// The span with the specified ID (transitively) has itself as a parent. This can only happen
    /// if the receiver was restored from a bogus persisted state.
    SpanParentCycle(RawSpanId),
    /// The event contains too many values.
    TooManyValues {
        /// Maximum supported number of values per event.
        max: usize,
        /// Actual number of values.
        actual: usize,
    },
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMetadataId(id) => write!(formatter, "unknown metadata ID: {id}"),
            Self::UnknownSpanId(id) => write!(formatter, "unknown span ID: {id}"),
            Self::SpanParentCycle(id) => write!(formatter, "span {id} is its own ancestor"),
            Self::TooManyValues { max, actual } => write!(
                formatter,
                "too many values provided ({actual}), should be no more than {max}"
            ),
        }
    }
}

impl error::Error for ReceiveError {}

macro_rules! create_value_set {
    ($fields:ident, $values:ident, [$($i:expr,)+]) => {
        match $values.len() {
            0 => $fields.value_set(&[]),
            $(
            $i => $fields.value_set(<&[_; $i]>::try_from($values).unwrap()),
            )+
            _ => unreachable!(),
        }
    };
}

/// Container for non-persisted information specific to a particular traced execution.
#[derive(Debug, Default)]
struct CurrentExecution {
    uncommitted_span_ids: HashSet<RawSpanId>,
    /// Number of times each span is currently entered. A span may be entered multiple times
    /// concurrently, e.g. from different threads of the sender.
    entered_span_ids: HashMap<RawSpanId, usize>,
    /// Local span that was current before any relayed spans were entered (i.e., the span
    /// of the receiving host). Only set while `entered_span_ids` is non-empty. The span is cloned
    /// in the local dispatcher, so that it stays alive while it may be used as a parent.
    host_span: Option<Id>,
}

impl CurrentExecution {
    fn enter_span(&mut self, id: RawSpanId) {
        if self.entered_span_ids.is_empty() {
            self.host_span = TracingEventReceiver::dispatch(|dispatch| {
                let current = dispatch.current_span();
                current.id().map(|id| dispatch.clone_span(id))
            });
        }
        *self.entered_span_ids.entry(id).or_default() += 1;
    }

    fn exit_span(&mut self, id: RawSpanId) {
        if let Some(count) = self.entered_span_ids.get_mut(&id) {
            *count -= 1;
            if *count == 0 {
                self.entered_span_ids.remove(&id);
                self.release_host_span();
            }
        }
    }

    /// Releases the host span once no relayed spans are entered.
    fn release_host_span(&mut self) {
        if self.entered_span_ids.is_empty() {
            if let Some(host_span) = self.host_span.take() {
                TracingEventReceiver::dispatch(|dispatch| dispatch.try_close(host_span));
            }
        }
    }

    /// Removes the span from the execution, returning the number of times it is still entered.
    fn remove_span(&mut self, id: RawSpanId) -> usize {
        self.uncommitted_span_ids.remove(&id);
        let entered_count = self.entered_span_ids.remove(&id).unwrap_or(0);
        self.release_host_span();
        entered_count
    }

    /// Resolves the local parent for spans / events without a parent on the sender side.
    /// Such spans / events are attached to the host span; relayed spans that are currently entered
    /// are ignored, since the sender has explicitly specified that they are not the parent.
    fn root_parent(&self) -> LocalParent {
        if self.entered_span_ids.is_empty() {
            // No relayed spans are entered, so the current span is the host span.
            LocalParent::Contextual
        } else if let Some(host_span) = &self.host_span {
            LocalParent::Span(host_span.clone())
        } else {
            LocalParent::None
        }
    }

    fn finalize(&mut self, local_spans: &LocalSpans) {
        for (id, count) in mem::take(&mut self.entered_span_ids) {
            if let Some(local_id) = local_spans.inner.get(&id) {
                TracingEventReceiver::dispatch(|dispatch| {
                    for _ in 0..count {
                        dispatch.exit(local_id);
                    }
                });
            }
        }
        self.release_host_span();
        for id in mem::take(&mut self.uncommitted_span_ids) {
            if let Some(local_id) = local_spans.inner.get(&id) {
                TracingEventReceiver::dispatch(|dispatch| dispatch.try_close(local_id.clone()));
            }
        }
    }
}

/// Receiver of [`TracingEvent`]s produced by [`TracingEventSender`] that relays them
/// to the tracing infrastructure.
///
/// The receiver takes care of persisting [`Metadata`] / spans that can outlive
/// the lifetime of the host program (not just the `TracingEventReceiver` instance!).
/// As an example, in [the Tardigrade runtime], a consumer instance is created each time
/// a workflow is executed. It relays tracing events from the workflow logic (executed in WASM)
/// to the host.
///
/// In some cases, the execution tracked by `TracingEventReceiver` may finish abnormally.
/// (E.g., a WASM module instance panics while it has the `panic = abort` set
/// in the compilation options.) In these cases, all entered
/// spans are force-exited when the receiver is dropped. Additionally, spans created
/// by the execution are closed on drop as long as they are not [persisted](Self::persist()).
/// That is, persistence acts as a commitment of the execution results, while the default
/// behavior is rollback.
///
/// # ⚠ Resource consumption
///
/// To fit the API of the [`tracing-core`] crate, the receiver leaks string parts
/// of [`CallSiteData`]: we need a `&'static str` when we only have a `String`. Steps are taken
/// to limit the amount of leaked memory; we use a `static` string arena which checks whether
/// a particular string was already leaked, and reuses the existing `&'static str` if possible.
/// Still, this has negative implications regarding both memory consumption and performance,
/// so you probably should limit the number of executables to use with a `TracingEventReceiver`.
/// The number of *executions* of each executable is not a limiting factor.
///
/// # Examples
///
/// See [crate-level docs](index.html) for an example of usage.
///
/// [`TracingEventSender`]: crate::TracingEventSender
/// [the Tardigrade runtime]: https://github.com/slowli/tardigrade
/// [`tracing-core`]: https://docs.rs/tracing-core/
#[derive(Debug, Default)]
pub struct TracingEventReceiver {
    metadata: HashMap<MetadataId, &'static Metadata<'static>>,
    spans: PersistedSpans,
    local_spans: LocalSpans,
    current_execution: CurrentExecution,
}

impl TracingEventReceiver {
    /// Maximum supported number of values in a span or event.
    const MAX_VALUES: usize = 32;

    /// Restores the receiver from the persisted metadata and tracing spans.
    ///
    /// A receiver will work fine if `local_spans` information is lost (e.g., reset to the default
    /// empty value). However, this is likely to result in span leakage
    /// in the underlying [`Subscriber`]. On the other hand, mismatch between `metadata` / `spans`
    /// and the execution producing [`TracingEvent`]s is **bad**; it will most likely result
    /// in errors returned from [`Self::try_receive()`].
    ///
    /// [`Subscriber`]: tracing_core::Subscriber
    pub fn new(
        metadata: PersistedMetadata,
        spans: PersistedSpans,
        local_spans: LocalSpans,
    ) -> Self {
        let mut this = Self {
            metadata: HashMap::new(),
            spans,
            local_spans,
            current_execution: CurrentExecution::default(),
        };

        for (id, data) in metadata.inner {
            this.on_new_call_site(id, data);
        }
        this
    }

    fn dispatch<T>(dispatch_fn: impl FnOnce(&Dispatch) -> T) -> T {
        dispatch_fn(&dispatcher::get_default(Dispatch::clone))
    }

    fn metadata(&self, id: MetadataId) -> Result<&'static Metadata<'static>, ReceiveError> {
        self.metadata
            .get(&id)
            .copied()
            .ok_or(ReceiveError::UnknownMetadataId(id))
    }

    fn span(&self, id: RawSpanId) -> Result<&SpanData, ReceiveError> {
        self.spans
            .inner
            .get(&id)
            .ok_or(ReceiveError::UnknownSpanId(id))
    }

    fn span_mut(&mut self, id: RawSpanId) -> Result<&mut SpanData, ReceiveError> {
        self.spans
            .inner
            .get_mut(&id)
            .ok_or(ReceiveError::UnknownSpanId(id))
    }

    /// Returns `Ok(None)` if the local span ID is (validly) not set yet, and `Err(_)`
    /// if it must have been set by this point.
    fn map_span_id(&self, remote_id: RawSpanId) -> Result<Option<&Id>, ReceiveError> {
        match self.local_spans.inner.get(&remote_id) {
            Some(local_id) => Ok(Some(local_id)),
            None => {
                // Check if the the referenced span is alive.
                if self.spans.inner.contains_key(&remote_id) {
                    Ok(None)
                } else {
                    Err(ReceiveError::UnknownSpanId(remote_id))
                }
            }
        }
    }

    fn ensure_values_len(values: &TracedValues<String>) -> Result<(), ReceiveError> {
        if values.len() > Self::MAX_VALUES {
            return Err(ReceiveError::TooManyValues {
                actual: values.len(),
                max: Self::MAX_VALUES,
            });
        }
        Ok(())
    }

    fn generate_fields<'a>(
        metadata: &'static Metadata<'static>,
        values: &'a TracedValues<String>,
    ) -> Vec<(Field, CowValue<'a>)> {
        let fields = metadata.fields();
        values
            .iter()
            .filter_map(|(field_name, value)| {
                fields
                    .field(field_name)
                    .map(|field| (field, value.as_value()))
            })
            .collect()
    }

    fn expand_fields<'a>(
        values: &'a [(Field, CowValue<'_>)],
    ) -> Vec<(&'a Field, Option<&'a dyn Value>)> {
        values
            .iter()
            .map(|(field, value)| (field, Some(value.as_ref())))
            .collect()
    }

    fn create_values<'a>(
        fields: &'a FieldSet,
        values: &'a [(&Field, Option<&dyn Value>)],
    ) -> ValueSet<'a> {
        create_value_set!(
            fields,
            values,
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25, 26, 27, 28, 29, 30, 31, 32,
            ]
        )
    }

    fn on_new_call_site(&mut self, id: MetadataId, data: CallSiteData) {
        let (metadata, is_new) = ARENA.alloc_metadata(data);
        self.metadata.insert(id, metadata);
        if is_new {
            Self::dispatch(|dispatch| dispatch.register_callsite(metadata));
        }
    }

    fn on_new_span(&mut self, id: RawSpanId, data: SpanData) -> Result<(), ReceiveError> {
        Self::ensure_values_len(&data.values)?;

        if !self.local_spans.inner.contains_key(&id) {
            let local_parent = self.local_parent(data.parent_id)?;
            let local_id = self.create_local_span(&data, &local_parent)?;
            self.local_spans.inner.insert(id, local_id);
        }
        self.spans.inner.insert(id, data);
        self.current_execution.uncommitted_span_ids.insert(id);
        Ok(())
    }

    /// Returns the local ID for the specified remote span, creating the local span if necessary
    /// (e.g., if the span was restored from the persisted state).
    fn local_span_id(&mut self, id: RawSpanId) -> Result<Id, ReceiveError> {
        self.local_span_id_checked(id, &mut HashSet::new())
    }

    /// Same as [`Self::local_span_id()`], but checks for cycles among the parents of the created
    /// local spans, which can only be present in a bogus persisted state.
    fn local_span_id_checked(
        &mut self,
        id: RawSpanId,
        created_ids: &mut HashSet<RawSpanId>,
    ) -> Result<Id, ReceiveError> {
        if let Some(local_id) = self.local_spans.inner.get(&id) {
            return Ok(local_id.clone());
        }
        if !created_ids.insert(id) {
            return Err(ReceiveError::SpanParentCycle(id));
        }
        let parent_id = self.span(id)?.parent_id;
        let local_parent = self.local_parent_checked(parent_id, created_ids)?;
        let local_id = self.create_local_span(self.span(id)?, &local_parent)?;
        self.local_spans.inner.insert(id, local_id.clone());
        Ok(local_id)
    }

    fn local_parent(&mut self, parent_id: Option<RawSpanId>) -> Result<LocalParent, ReceiveError> {
        self.local_parent_checked(parent_id, &mut HashSet::new())
    }

    fn local_parent_checked(
        &mut self,
        parent_id: Option<RawSpanId>,
        created_ids: &mut HashSet<RawSpanId>,
    ) -> Result<LocalParent, ReceiveError> {
        Ok(match parent_id {
            None => LocalParent::Contextual,
            Some(ROOT_SPAN_ID) => self.current_execution.root_parent(),
            Some(id) => LocalParent::Span(self.local_span_id_checked(id, created_ids)?),
        })
    }

    fn create_local_span(
        &self,
        data: &SpanData,
        local_parent: &LocalParent,
    ) -> Result<Id, ReceiveError> {
        let metadata = self.metadata(data.metadata_id)?;

        let value_set = Self::generate_fields(metadata, &data.values);
        let value_set = Self::expand_fields(&value_set);
        let value_set = Self::create_values(metadata.fields(), &value_set);
        let attributes = match local_parent {
            LocalParent::Contextual => Attributes::new(metadata, &value_set),
            LocalParent::None => Attributes::new_root(metadata, &value_set),
            LocalParent::Span(id) => Attributes::child_of(id.clone(), metadata, &value_set),
        };

        Ok(Self::dispatch(|dispatch| dispatch.new_span(&attributes)))
    }

    /// Tries to consume an event and relays it to the tracing infrastructure.
    ///
    /// # Errors
    ///
    /// Fails if the event contains a bogus reference to a call site or a span, or if it contains
    /// too many values. In general, an error can mean that the consumer was restored
    /// from an incorrect persisted state, or that the event generator is bogus (e.g.,
    /// not a [`TracingEventSender`]).
    ///
    /// [`TracingEventSender`]: crate::TracingEventSender
    #[allow(clippy::missing_panics_doc, clippy::map_entry)] // false positive
    pub fn try_receive(&mut self, event: TracingEvent) -> Result<(), ReceiveError> {
        match event {
            TracingEvent::NewCallSite { id, data } => {
                self.on_new_call_site(id, data);
            }

            TracingEvent::NewSpan {
                id,
                parent_id,
                metadata_id,
                values,
            } => {
                let data = SpanData {
                    metadata_id,
                    parent_id,
                    ref_count: 1,
                    values,
                };
                self.on_new_span(id, data)?;
            }

            TracingEvent::FollowsFrom { id, follows_from } => {
                let local_id = self.map_span_id(id)?;
                let local_follows_from = self.map_span_id(follows_from)?;

                // TODO: properly handle remaining cases
                if let (Some(id), Some(follows_from)) = (local_id, local_follows_from) {
                    Self::dispatch(|dispatch| {
                        dispatch.record_follows_from(id, follows_from);
                    });
                }
            }

            TracingEvent::SpanEntered { id } => {
                let local_id = self.local_span_id(id)?;
                self.current_execution.enter_span(id);
                Self::dispatch(|dispatch| dispatch.enter(&local_id));
            }
            TracingEvent::SpanExited { id } => {
                if let Some(local_id) = self.map_span_id(id)? {
                    Self::dispatch(|dispatch| dispatch.exit(local_id));
                }
                self.current_execution.exit_span(id);
            }

            TracingEvent::SpanCloned { id } => {
                let span = self.span_mut(id)?;
                span.ref_count += 1;
                // Dispatcher is intentionally not called: we handle ref counting locally.
            }
            TracingEvent::SpanDropped { id } => {
                let span = self.span_mut(id)?;
                span.ref_count -= 1;
                if span.ref_count == 0 {
                    self.spans.inner.remove(&id);
                    let entered_count = self.current_execution.remove_span(id);
                    if let Some(local_id) = self.local_spans.inner.remove(&id) {
                        // Keep the local dispatcher consistent if the sender dropped the span
                        // without exiting it (e.g., because the sender has crashed).
                        Self::dispatch(|dispatch| {
                            for _ in 0..entered_count {
                                dispatch.exit(&local_id);
                            }
                            dispatch.try_close(local_id.clone())
                        });
                    }
                }
            }

            TracingEvent::ValuesRecorded { id, values } => {
                Self::ensure_values_len(&values)?;

                if let Some(local_id) = self.map_span_id(id)? {
                    let metadata = self.metadata(self.spans.inner[&id].metadata_id)?;
                    let values = Self::generate_fields(metadata, &values);
                    let values = Self::expand_fields(&values);
                    let values = Self::create_values(metadata.fields(), &values);
                    let values = Record::new(&values);
                    Self::dispatch(|dispatch| dispatch.record(local_id, &values));
                }
                let span = self.span_mut(id)?;
                span.values.extend(values);
            }

            TracingEvent::NewEvent {
                metadata_id,
                parent,
                values,
            } => {
                Self::ensure_values_len(&values)?;

                let metadata = self.metadata(metadata_id)?;
                let parent = self.local_parent(parent)?;
                let values = Self::generate_fields(metadata, &values);
                let values = Self::expand_fields(&values);
                let values = Self::create_values(metadata.fields(), &values);
                let event = match parent {
                    LocalParent::Contextual => Event::new(metadata, &values),
                    LocalParent::None => Event::new_child_of(None, metadata, &values),
                    LocalParent::Span(id) => Event::new_child_of(id, metadata, &values),
                };
                Self::dispatch(|dispatch| dispatch.event(&event));
            }
        }
        Ok(())
    }

    /// Consumes an event and relays it to the tracing infrastructure.
    ///
    /// # Panics
    ///
    /// Panics in the same cases when [`Self::try_receive()`] returns an error.
    pub fn receive(&mut self, event: TracingEvent) {
        self.try_receive(event)
            .expect("received bogus tracing event");
    }

    /// Persists [`Metadata`] produced by the previously consumed events. The returned
    /// metadata should be merged into the metadata provided to [`Self::new()`].
    pub fn persist_metadata(&self) -> PersistedMetadata {
        let inner = self
            .metadata
            .iter()
            .map(|(&id, &metadata)| (id, CallSiteData::from(metadata)))
            .collect();
        PersistedMetadata { inner }
    }

    /// Returns persisted and local spans.
    pub fn persist(mut self) -> (PersistedSpans, LocalSpans) {
        self.current_execution.uncommitted_span_ids.clear();
        let spans = mem::take(&mut self.spans);
        let local_spans = mem::take(&mut self.local_spans);
        self.current_execution.finalize(&local_spans);
        (spans, local_spans)
    }
}

impl Drop for TracingEventReceiver {
    fn drop(&mut self) {
        self.current_execution.finalize(&self.local_spans);
    }
}
