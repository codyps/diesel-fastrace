# fastrace-diesel

fastrace tracing for Diesel queries on PostgreSQL, MySQL, and SQLite.

```rust
fn main() -> diesel::QueryResult<()> {
    fastrace_diesel::install_default_instrumentation()?;
    // Establish Diesel connections to any supported backend after installing.
    Ok(())
}
```

One installation handles PostgreSQL, MySQL, and SQLite connections together, with independent state for each connection. `FastraceInstrumentation::default()` provides the same automatic behavior for a custom factory; `FastraceInstrumentation::new(url)` works when attaching instrumentation to an existing connection.

Backend inference recognizes `postgres://`, `postgresql://`, `mysql://`, SQLite `file:` URIs, and `:memory:`. Bare filenames, libpq keyword connection strings, and unrecognized strings produce generic `Database` spans (`db.system.name = other_sql`) without connection-string metadata. Queries, arguments, errors, and transactions are still traced. Use explicit selection when you need backend metadata for an ambiguous string:


| Backend | Default installer | Per-connection constructor |
| --- | --- | --- |
| PostgreSQL | `install_default_postgres_instrumentation()` | `FastraceInstrumentation::postgres(url)` |
| MySQL | `install_default_mysql_instrumentation()` | `FastraceInstrumentation::mysql(url)` |
| SQLite | `install_default_sqlite_instrumentation()` | `FastraceInstrumentation::sqlite(path_or_uri)` |

Diesel has one global default instrumentation factory; installing another replaces it. Use the generic installer for mixed backends. Explicit constructors can also be attached with `Connection::set_instrumentation`, but connection-establishment spans require a factory installed before connecting. Explicit SQLite instrumentation supports filenames, `file:` URIs, and `:memory:` and does not emit server address or port attributes.

Configure a fastrace reporter and parent span in your application. Connection and query spans describe database/network metadata, error categories and schema identifiers. Nested transactions get lifetime spans; cache insertions emit fastrace events.

Query spans record SQL and bind arguments in `db.query.text`, using Diesel's display format, for example `SELECT $1 -- binds: [42]`. Query capture is enabled by default. Database, host, table, column and constraint names are also recorded.

Disable SQL and argument capture with the builder-style option:

```rust
use fastrace_diesel::FastraceInstrumentation;

diesel::connection::set_default_instrumentation(|| {
    Some(Box::new(
        FastraceInstrumentation::default().with_query_capture(false),
    ))
})?;
```

Install this factory before establishing connections. Disabling capture keeps query timing, error metadata, and transaction spans. SQL and arguments are controlled together because Diesel provides them as one formatted value.

## Development

Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.

The workspace consumer crates run a shared tracing suite against real Diesel connections. They check SQL and arguments, prepared-statement cache events, errors, nested transactions and rollbacks, connection failures, and disabled capture. SQLite additionally tests file URIs and immediate/exclusive transactions. `tests/mixed` opens all three backends under one generic factory, verifying simultaneous transactions and capture options.

```sh
cargo test --manifest-path tests/sqlite/Cargo.toml --locked
FASTRACE_DIESEL_TEST_DATABASE_URL=postgres://localhost/fastrace_diesel_test \
  cargo test --manifest-path tests/postgres/Cargo.toml --locked
FASTRACE_DIESEL_TEST_MYSQL_URL=mysql://tester:password@127.0.0.1/fastrace_diesel_test \
  cargo test --manifest-path tests/mysql/Cargo.toml --locked
# With both server URL variables set:
cargo test --manifest-path tests/mixed/Cargo.toml --locked
```

Plain `cargo test` runs the library tests only. Integration tests require the corresponding client libraries (libpq, libmysqlclient, or SQLite). Server tests fail if their URL is missing or the server is unavailable. CI runs PostgreSQL 18, MySQL 8.4, and SQLite integration jobs on pushes and PRs, including release PRs; automatic releases require all three and the mixed-backend suite to pass.

## Releases

Release-plz creates release PRs; merging one publishes to crates.io and creates a GitHub release. Ordinary commits do not publish. A daily job can merge a release PR 14 days after the previous release (or 14 days after the first release PR was created), once CI passes. Manual merges can release earlier. Failed publication blocks subsequent automatic merges.

Automation uses the repository GITHUB_TOKEN and explicitly dispatches CI and publishing workflows. No personal GitHub token is needed. The automatic release workflow defaults to a read-only preview when run manually.

After the first manual publication of `fastrace-diesel`, configure its crates.io trusted publisher for the repository and workflow below. Subsequent publishing uses crates.io trusted publishing (GitHub Actions OIDC), bound to `codyps/fastrace-diesel` and `.github/workflows/release-plz.yml`, with no GitHub environment. The publishing job has `id-token: write`; release-plz exchanges the job identity for a short-lived crates.io token. No `CARGO_REGISTRY_TOKEN` secret or environment variable is used.
