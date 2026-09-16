# Changelog

## [Unreleased]

## [0.1.1](https://github.com/codyps/diesel-fastrace/compare/diesel-fastrace-0.1.0...diesel-fastrace-0.1.1) - 2026-09-16

### Added

- support and test MySQL and SQLite tracing
- capture query SQL and arguments with a default-on option

### Other

- verify Diesel tracing against PostgreSQL in CI
- remove OpenTelemetry metrics dependency and cache counter
- use crates.io trusted publishing
- remove application-specific references

### Added

- PostgreSQL connection, query and nested transaction spans, safe error metadata, and prepared-statement cache events.
