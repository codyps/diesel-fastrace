# diesel-fastrace

fastrace tracing for Diesel PostgreSQL queries.

```rust
fn main() -> diesel::QueryResult<()> {
    diesel_fastrace::install_default_postgres_instrumentation()?;
    // Establish Diesel PostgreSQL connections after installing instrumentation.
    Ok(())
}
```

Configure a fastrace reporter and parent span in your application. Connection and query spans describe database/network metadata, error categories and schema identifiers. Nested transactions get lifetime spans; cache insertions emit fastrace events.

Query spans record SQL and bind arguments in `db.query.text`, using Diesel's display format, for example `SELECT $1 -- binds: [42]`. Query capture is enabled by default. Database, host, table, column and constraint names are also recorded.

Disable SQL and argument capture with the builder-style option:

```rust
use diesel_fastrace::FastraceInstrumentation;

diesel::connection::set_default_instrumentation(|| {
    Some(Box::new(
        FastraceInstrumentation::postgres("").with_query_capture(false),
    ))
})?;
```

Install this factory before establishing connections. Disabling capture keeps query timing, error metadata, and transaction spans. SQL and arguments are controlled together because Diesel provides them as one formatted value.

## Development

Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.

The `tests/postgres` workspace consumer crate executes real Diesel operations against PostgreSQL and checks the recorded spans, SQL and arguments, cache events, errors, transaction parentage, and disabled capture. Run it with:

```sh
DIESEL_FASTRACE_TEST_DATABASE_URL=postgres://localhost/diesel_fastrace_test \
  cargo test --manifest-path tests/postgres/Cargo.toml --locked
```

Plain `cargo test` runs the library tests only. The integration suite requires PostgreSQL and the libpq development library. It uses per-connection temporary tables and fails if the database URL is missing or PostgreSQL is unavailable. CI provisions PostgreSQL 18 and runs this suite on pushes, pull requests, and release PRs; automatic releases require it to pass.

## Releases

Release-plz creates release PRs; merging one publishes to crates.io and creates a GitHub release. Ordinary commits do not publish. A daily job can merge a release PR 14 days after the previous release (or 14 days after the first release PR was created), once CI passes. Manual merges can release earlier. Failed publication blocks subsequent automatic merges.

Automation uses the repository GITHUB_TOKEN and explicitly dispatches CI and publishing workflows. No personal GitHub token is needed. The automatic release workflow defaults to a read-only preview when run manually.

Publishing uses crates.io trusted publishing (GitHub Actions OIDC), bound to `codyps/diesel-fastrace` and `.github/workflows/release-plz.yml`, with no GitHub environment. The publishing job has `id-token: write`; release-plz exchanges the job identity for a short-lived crates.io token. No `CARGO_REGISTRY_TOKEN` secret or environment variable is used.
