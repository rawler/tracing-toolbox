# Changelog

All notable changes to this project will be documented in this file.
The project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Bump minimum supported Rust version to 1.85 and switch to 2024 edition.
- Extend the event format: a parent of `ROOT_SPAN_ID` (0) marks spans and events without a parent
  on the sender side. The receiver attaches them to its current non-relayed span (if any),
  as opposed to `None`, which is resolved against the receiver's current span including relayed spans.
  `TracingEventSender` sends `ROOT_SPAN_ID` for spans and events without a parent on the sender side
  that it knows of: explicit roots (e.g., `info_span!(parent: None, ..)`), and, with `sync()`,
  also contextual spans and events outside any span.
  Receivers from previous versions reject `ROOT_SPAN_ID` as an unknown span ID.
- Make `TracingEventSender::sync()` track the current span for each thread and resolve
  contextual parents of spans and events on the sender side.

### Fixed

- Fix spans and events being attributed to spans from other threads when `TracingEventSender::sync()`
  is used from multiple threads concurrently.
- Fix `TracingEventReceiver` ignoring parents of spans and events if the parent span
  was restored from the persisted state, but not yet entered.
- Fix `TracingEventReceiver` exiting a span only once on drop if it was entered multiple times
  concurrently.
- Fix `TracingEventReceiver` not exiting a relayed span that is dropped while still entered.
- Return `ReceiveError::SpanParentCycle` instead of overflowing the stack if spans restored
  from the persisted state have cyclic parents.

## 0.2.0-beta.1 - 2024-03-03

### Added

- Expose `TracingEvent::normalize()` to transform a sequence of events so that
  it does not contain information that changes between program runs (e.g., metadata IDs)
  or due to minor refactoring (source code lines).

### Changed

- Bump minimum supported Rust version to 1.70.

## 0.1.0 - 2022-12-09

The initial release of `tracing-tunnel`.
