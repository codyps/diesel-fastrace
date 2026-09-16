# diesel-fastrace

Native fastrace instrumentation for Diesel PostgreSQL connections, with OpenTelemetry-compatible metadata and a prepared-statement cache counter.

```rust
fn main() -> diesel::QueryResult<()> {
    diesel_fastrace::install_default_postgres_instrumentation()?;
    // Establish Diesel PostgreSQL connections after installing instrumentation.
    Ok(())
}
```

Configure a fastrace reporter and parent span in your application. Connection and query spans describe database/network metadata, error categories and schema identifiers. Nested transactions get lifetime spans; cache insertions emit events and an OpenTelemetry counter. Configure the global OpenTelemetry meter provider before the first cache insertion.

SQL text, bind values, credentials and free-form database errors are never recorded. Database, host, table, column and constraint names are recorded.

Extracted from `codyps/beachout`, `crates/diesel-fastrace`, at commit `d3d1e5a9ad4cbc2162952613289de8d64dd8c7b2`.

## Development

Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.

## Releases

Release-plz creates release PRs; merging one publishes to crates.io and creates a GitHub release. Ordinary commits do not publish. A daily job can merge a release PR 14 days after the previous release (or 14 days after the first release PR was created), once CI passes. Manual merges can release earlier. Failed publication blocks subsequent automatic merges.

Automation uses the repository GITHUB_TOKEN and explicitly dispatches CI and publishing workflows. No personal GitHub token is needed. The automatic release workflow defaults to a read-only preview when run manually.

Publishing uses crates.io trusted publishing: configure `codyps/diesel-fastrace`, workflow `release-plz.yml`, without an environment. The first release requires a crates.io API token (repository secret `CARGO_REGISTRY_TOKEN`) until the crate exists and a trusted publisher is configured; remove that secret afterward.
