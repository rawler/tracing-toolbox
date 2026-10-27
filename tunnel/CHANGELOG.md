# Changelog

All notable changes to this project will be documented in this file.
The project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Bump minimum supported Rust version to 1.85 and switch to 2024 edition.

### Fixed

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
