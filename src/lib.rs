//! fastrace tracing for Diesel PostgreSQL, MySQL, and SQLite queries.
//!
//! Query spans record SQL and bind arguments in `db.query.text` using Diesel's display
//! format by default. Use [`FastraceInstrumentation::with_query_capture`] to disable
//! capture. Connection and transaction spans include database and network metadata.

use diesel::connection::{Instrumentation, InstrumentationEvent, set_default_instrumentation};
use diesel::result::{ConnectionError, DatabaseErrorInformation, DatabaseErrorKind, Error};
use fastrace::{Event, Span};
use url::Url;

/// Instruments one Diesel connection with OpenTelemetry-compatible fastrace query spans.
pub struct FastraceInstrumentation {
    backend: Backend,
    infer_backend: bool,
    capture_queries: bool,
    active_connection: Option<Span>,
    active_query: Option<Span>,
    properties: Vec<(&'static str, String)>,
    transactions: Vec<ActiveTransaction>,
    pending_transaction: Option<TransactionCommand>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Unknown,
    Postgres,
    Mysql,
    Sqlite,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Unknown => "Database",
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Sqlite => "SQLite",
        }
    }

    fn system(self) -> &'static str {
        match self {
            Self::Unknown => "other_sql",
            Self::Postgres => "postgresql",
            Self::Mysql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }
}

impl Default for FastraceInstrumentation {
    fn default() -> Self {
        Self::new("")
    }
}

fn infer_backend(url: &str) -> Backend {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Backend::Postgres
    } else if url.starts_with("mysql://") {
        Backend::Mysql
    } else if url == ":memory:" || url.starts_with("file:") {
        Backend::Sqlite
    } else {
        Backend::Unknown
    }
}

impl FastraceInstrumentation {
    /// Creates instrumentation that infers the backend from a connection string.
    ///
    /// Recognizes PostgreSQL/MySQL URL schemes and SQLite `file:` URIs or `:memory:`.
    /// Other strings (including bare filenames and libpq keyword strings) produce
    /// generic database spans without connection-string metadata. Use an explicit
    /// backend constructor when those strings need backend-specific metadata.
    /// A connection-establishment event updates the inferred backend automatically.
    pub fn new(database_url: &str) -> Self {
        let mut instrumentation = Self::for_backend(infer_backend(database_url), database_url);
        instrumentation.infer_backend = true;
        instrumentation
    }

    /// Creates PostgreSQL instrumentation from a connection URL without retaining credentials.
    pub fn postgres(database_url: &str) -> Self {
        Self::for_backend(Backend::Postgres, database_url)
    }

    /// Creates MySQL instrumentation from a connection URL without retaining credentials.
    pub fn mysql(database_url: &str) -> Self {
        Self::for_backend(Backend::Mysql, database_url)
    }

    /// Creates SQLite instrumentation from a filename, file URI, or `:memory:`.
    pub fn sqlite(database_url: &str) -> Self {
        Self::for_backend(Backend::Sqlite, database_url)
    }

    fn for_backend(backend: Backend, database_url: &str) -> Self {
        Self {
            backend,
            infer_backend: false,
            capture_queries: true,
            active_connection: None,
            active_query: None,
            properties: database_properties(backend, database_url, "QUERY"),
            transactions: Vec::new(),
            pending_transaction: None,
        }
    }

    /// Enables or disables capture of SQL and bind arguments. Enabled by default.
    ///
    /// Diesel exposes SQL and arguments together, so this option controls both.
    /// Disabling capture preserves query timing, errors, and transaction spans.
    ///
    /// ```
    /// use fastrace_diesel::FastraceInstrumentation;
    /// let instrumentation = FastraceInstrumentation::postgres("postgres:///example_db")
    ///     .with_query_capture(false);
    /// ```
    #[must_use]
    pub fn with_query_capture(mut self, enabled: bool) -> Self {
        self.capture_queries = enabled;
        self
    }

    fn postgres_without_url() -> Self {
        Self::postgres("")
    }

    fn start_connection(&mut self, database_url: &str) {
        if self.infer_backend {
            self.backend = infer_backend(database_url);
        }
        self.properties = database_properties(self.backend, database_url, "QUERY");
        let span = Span::enter_with_local_parent(format!("{} connect", self.backend.name()));
        let properties = database_properties(self.backend, database_url, "CONNECT");
        span.add_properties(move || properties);
        self.active_connection = Some(span);
    }

    fn finish_connection(&mut self, error: Option<&ConnectionError>) {
        if let Some(span) = self.active_connection.take() {
            if let Some(error) = error {
                span.add_properties(|| {
                    [
                        ("span.status_code", "error".to_owned()),
                        ("error.type", connection_error_type(error).to_owned()),
                    ]
                });
            } else {
                span.add_property(|| ("span.status_code", "ok"));
            }
        }
    }

    fn start_query(&mut self) {
        let operation = self
            .pending_transaction
            .map_or("QUERY", TransactionCommand::query_operation);
        let name = if operation == "QUERY" {
            format!("{} query", self.backend.name())
        } else {
            format!("{} {operation}", self.backend.name())
        };
        let span = if let Some(transaction) = self.transactions.last() {
            Span::enter_with_parent(name, &transaction.span)
        } else {
            Span::enter_with_local_parent(name)
        };
        let mut properties = properties_with_operation(&self.properties, operation);
        if let Some(command) = self.pending_transaction {
            properties.push((
                "db.transaction.operation",
                command.action.as_str().to_owned(),
            ));
            properties.push(("db.transaction.depth", command.depth.to_string()));
        } else if let Some(transaction) = self.transactions.last() {
            properties.push(("db.transaction.depth", transaction.depth.to_string()));
        }
        span.add_properties(move || properties);
        self.active_query = Some(span);
    }

    fn finish_query(&mut self, error: Option<&Error>) {
        if let Some(span) = self.active_query.take() {
            if let Some(error) = error {
                let properties = database_error_properties(error);
                span.add_properties(move || properties);
            } else {
                span.add_property(|| ("span.status_code", "ok"));
            }
        }

        if let Some(command) = self.pending_transaction.take() {
            self.finish_transaction_command(command, error);
        }
    }

    fn start_transaction_command(&mut self, action: TransactionAction, depth: u32) {
        let command = TransactionCommand { action, depth };
        if action == TransactionAction::Begin {
            let span = if let Some(parent) = self.transactions.last() {
                Span::enter_with_parent(
                    format!("{} transaction", self.backend.name()),
                    &parent.span,
                )
            } else {
                Span::enter_with_local_parent(format!("{} transaction", self.backend.name()))
            };
            let mut properties = properties_with_operation(&self.properties, "TRANSACTION");
            if let Some(kind) = properties
                .iter_mut()
                .find_map(|(key, value)| (*key == "span.kind").then_some(value))
            {
                *kind = "internal".to_owned();
            }
            properties.push(("db.transaction.depth", depth.to_string()));
            span.add_properties(move || properties);
            self.transactions.push(ActiveTransaction { depth, span });
        }
        self.pending_transaction = Some(command);
    }

    fn finish_transaction_command(&mut self, command: TransactionCommand, error: Option<&Error>) {
        let should_finish = match command.action {
            TransactionAction::Begin => error.is_some(),
            TransactionAction::Commit | TransactionAction::Rollback => true,
        };
        if !should_finish {
            return;
        }

        let Some(transaction) = self
            .transactions
            .last()
            .filter(|transaction| transaction.depth == command.depth)
        else {
            return;
        };
        if let Some(error) = error {
            let properties = database_error_properties(error);
            transaction.span.add_properties(move || properties);
        } else {
            transaction.span.add_property(|| ("span.status_code", "ok"));
        }
        let outcome = command.outcome(error.is_some());
        transaction
            .span
            .add_property(move || ("db.transaction.outcome", outcome));
        self.transactions.pop();
    }
}

struct ActiveTransaction {
    depth: u32,
    span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransactionAction {
    Begin,
    Commit,
    Rollback,
}

impl TransactionAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Begin => "begin",
            Self::Commit => "commit",
            Self::Rollback => "rollback",
        }
    }
}

#[derive(Clone, Copy)]
struct TransactionCommand {
    action: TransactionAction,
    depth: u32,
}

impl TransactionCommand {
    fn query_operation(self) -> &'static str {
        match (self.action, self.depth) {
            (TransactionAction::Begin, 1) => "BEGIN",
            (TransactionAction::Begin, _) => "SAVEPOINT",
            (TransactionAction::Commit, 1) => "COMMIT",
            (TransactionAction::Commit, _) => "RELEASE SAVEPOINT",
            (TransactionAction::Rollback, 1) => "ROLLBACK",
            (TransactionAction::Rollback, _) => "ROLLBACK TO SAVEPOINT",
        }
    }

    fn outcome(self, failed: bool) -> &'static str {
        match (self.action, failed) {
            (TransactionAction::Begin, true) => "begin_error",
            (TransactionAction::Begin, false) => "open",
            (TransactionAction::Commit, true) => "commit_error",
            (TransactionAction::Commit, false) => "commit",
            (TransactionAction::Rollback, true) => "rollback_error",
            (TransactionAction::Rollback, false) => "rollback",
        }
    }
}

/// Installs PostgreSQL instrumentation before Diesel establishes new connections.
///
/// Installing a default is required to observe connection latency and failed connection attempts;
/// calling `Connection::set_instrumentation` after `establish` is too late for those events.
/// The default factory is global; use [`install_default_instrumentation`] for mixed backends.
pub fn install_default_postgres_instrumentation() -> diesel::QueryResult<()> {
    set_default_instrumentation(|| Some(Box::new(FastraceInstrumentation::postgres_without_url())))
}

/// Installs instrumentation for all new Diesel connections, regardless of backend.
///
/// Call once before establishing connections. Each connection gets its own state.
/// Backend metadata is inferred as described by [`FastraceInstrumentation::new`].
/// For custom capture options, install a factory returning
/// `FastraceInstrumentation::default().with_query_capture(false)` through Diesel's
/// `set_default_instrumentation` function.
pub fn install_default_instrumentation() -> diesel::QueryResult<()> {
    set_default_instrumentation(|| Some(Box::new(FastraceInstrumentation::default())))
}

/// Installs instrumentation before Diesel establishes new MySQL connections.
///
/// Diesel's default instrumentation factory is global; installing this replaces any
/// previously installed factory. For mixed backends, use [`install_default_instrumentation`].
pub fn install_default_mysql_instrumentation() -> diesel::QueryResult<()> {
    set_default_instrumentation(|| Some(Box::new(FastraceInstrumentation::mysql(""))))
}

/// Installs instrumentation before Diesel establishes new SQLite connections.
///
/// Diesel's default instrumentation factory is global; installing this replaces any
/// previously installed factory. For mixed backends, use [`install_default_instrumentation`].
pub fn install_default_sqlite_instrumentation() -> diesel::QueryResult<()> {
    set_default_instrumentation(|| Some(Box::new(FastraceInstrumentation::sqlite(""))))
}

impl Instrumentation for FastraceInstrumentation {
    fn on_connection_event(&mut self, event: InstrumentationEvent<'_>) {
        match event {
            InstrumentationEvent::StartEstablishConnection { url, .. } => {
                self.start_connection(url);
            }
            InstrumentationEvent::FinishEstablishConnection { error, .. } => {
                self.finish_connection(error);
            }
            InstrumentationEvent::StartQuery { query, .. } => {
                self.start_query();
                if self.capture_queries
                    && let Some(span) = self.active_query.as_ref()
                {
                    span.add_properties(|| {
                        use std::fmt::Write;
                        let mut text = String::new();
                        // A query that fails to build can also fail to format. Instrumentation
                        // must not turn Diesel's query error into a formatting panic.
                        if write!(&mut text, "{query}").is_ok() {
                            Some(("db.query.text", text))
                        } else {
                            None
                        }
                    });
                }
            }
            InstrumentationEvent::CacheQuery { .. } => {
                if let Some(span) = self.active_query.as_ref() {
                    span.add_property(|| ("db.query.prepared_cache_inserted", "true"));
                    span.add_event(Event::new(format!(
                        "{} prepared statement cached",
                        self.backend.name()
                    )));
                }
            }
            InstrumentationEvent::FinishQuery { error, .. } => self.finish_query(error),
            InstrumentationEvent::BeginTransaction { depth, .. } => {
                self.start_transaction_command(TransactionAction::Begin, depth.get());
            }
            InstrumentationEvent::CommitTransaction { depth, .. } => {
                self.start_transaction_command(TransactionAction::Commit, depth.get());
            }
            InstrumentationEvent::RollbackTransaction { depth, .. } => {
                self.start_transaction_command(TransactionAction::Rollback, depth.get());
            }
            _ => {}
        }
    }
}

fn properties_with_operation(
    properties: &[(&'static str, String)],
    operation: &str,
) -> Vec<(&'static str, String)> {
    properties
        .iter()
        .map(|(key, value)| {
            if *key == "db.operation.name" {
                (*key, operation.to_owned())
            } else {
                (*key, value.clone())
            }
        })
        .collect()
}

fn database_properties(
    backend: Backend,
    database_url: &str,
    operation: &str,
) -> Vec<(&'static str, String)> {
    let mut properties = vec![
        ("span.kind", "client".to_owned()),
        // Keep both the stable semantic-convention name and Uptrace's legacy-compatible key.
        ("db.system.name", backend.system().to_owned()),
        ("db.system", backend.system().to_owned()),
        // Diesel does not expose a structured operation for ordinary queries.
        ("db.operation.name", operation.to_owned()),
    ];

    if backend == Backend::Unknown {
        return properties;
    }

    if backend == Backend::Sqlite {
        // SQLite filenames and file URIs describe a local database, not a server.
        let database = if let Some(path) = database_url.strip_prefix("file:") {
            if path.starts_with("//") {
                Url::parse(database_url)
                    .ok()
                    .map(|url| url.path().to_owned())
            } else {
                Some(path.split(['?', '#']).next().unwrap_or_default().to_owned())
            }
        } else {
            Some(database_url.to_owned())
        };
        if let Some(database) = database.filter(|value| !value.is_empty()) {
            properties.push(("db.namespace", database.clone()));
            properties.push(("db.name", database));
        }
        return properties;
    }

    let Ok(url) = Url::parse(database_url) else {
        return properties;
    };

    if let Some(database) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|database| !database.is_empty())
    {
        properties.push(("db.namespace", database.to_owned()));
        properties.push(("db.name", database.to_owned()));
    }
    if let Some(host) = url.host_str() {
        properties.push(("server.address", host.to_owned()));
        properties.push(("server.socket.domain", host.to_owned()));
    }
    if let Some(port) = url.port_or_known_default() {
        properties.push(("server.port", port.to_string()));
    }

    properties
}

fn database_error_properties(error: &Error) -> Vec<(&'static str, String)> {
    let mut properties = vec![
        ("span.status_code", "error".to_owned()),
        ("error.type", database_error_type(error).to_owned()),
    ];

    if let Error::DatabaseError(_, information) = error {
        add_database_error_information(&mut properties, information.as_ref());
    }

    properties
}

fn add_database_error_information(
    properties: &mut Vec<(&'static str, String)>,
    information: &dyn DatabaseErrorInformation,
) {
    if let Some(table) = information.table_name() {
        properties.push(("db.collection.name", table.to_owned()));
    }
    if let Some(column) = information.column_name() {
        properties.push(("diesel.error.column", column.to_owned()));
    }
    if let Some(constraint) = information.constraint_name() {
        properties.push(("diesel.error.constraint", constraint.to_owned()));
    }
    if let Some(position) = information.statement_position() {
        properties.push(("diesel.error.statement_position", position.to_string()));
    }
}

fn database_error_type(error: &Error) -> &'static str {
    match error {
        Error::DatabaseError(kind, ..) => database_error_kind(*kind),
        Error::NotFound => "not_found",
        Error::RollbackErrorOnCommit { .. } => "rollback_error_on_commit",
        Error::RollbackTransaction => "rollback_transaction",
        Error::AlreadyInTransaction => "already_in_transaction",
        Error::NotInTransaction => "not_in_transaction",
        Error::BrokenTransactionManager => "broken_transaction_manager",
        Error::SerializationError(_) => "serialization_error",
        Error::DeserializationError(_) => "deserialization_error",
        Error::QueryBuilderError(_) => "query_builder_error",
        Error::InvalidCString(_) => "invalid_c_string",
        _ => "diesel_error",
    }
}

fn database_error_kind(kind: DatabaseErrorKind) -> &'static str {
    match kind {
        DatabaseErrorKind::UniqueViolation => "unique_violation",
        DatabaseErrorKind::ForeignKeyViolation => "foreign_key_violation",
        DatabaseErrorKind::UnableToSendCommand => "unable_to_send_command",
        DatabaseErrorKind::SerializationFailure => "serialization_failure",
        DatabaseErrorKind::ReadOnlyTransaction => "read_only_transaction",
        DatabaseErrorKind::RestrictViolation => "restrict_violation",
        DatabaseErrorKind::NotNullViolation => "not_null_violation",
        DatabaseErrorKind::CheckViolation => "check_violation",
        DatabaseErrorKind::ExclusionViolation => "exclusion_violation",
        DatabaseErrorKind::ClosedConnection => "closed_connection",
        DatabaseErrorKind::Unknown => "database_error",
        _ => "database_error",
    }
}

fn connection_error_type(error: &ConnectionError) -> &'static str {
    match error {
        ConnectionError::InvalidCString(_) => "invalid_connection_string",
        ConnectionError::BadConnection(_) => "bad_connection",
        ConnectionError::InvalidConnectionUrl(_) => "invalid_connection_url",
        ConnectionError::CouldntSetupConfiguration(_) => "connection_configuration_error",
        _ => "connection_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_inference_preserves_explicit_backend_and_handles_unknown_strings() {
        for (url, expected) in [
            ("postgres://localhost/db", Backend::Postgres),
            ("postgresql://localhost/db", Backend::Postgres),
            ("mysql://localhost/db", Backend::Mysql),
            (":memory:", Backend::Sqlite),
            ("file:example.db?mode=ro", Backend::Sqlite),
            ("relative.db", Backend::Unknown),
            ("host=localhost password=secret", Backend::Unknown),
            ("", Backend::Unknown),
            ("custom://user:secret@host/db", Backend::Unknown),
        ] {
            let mut instrumentation = FastraceInstrumentation::default();
            instrumentation.start_connection(url);
            assert!(instrumentation.backend == expected);
            if expected == Backend::Unknown {
                assert_eq!(instrumentation.backend.name(), "Database");
                assert!(
                    !instrumentation
                        .properties
                        .iter()
                        .any(|(k, _)| k.starts_with("server.")
                            || *k == "db.namespace"
                            || *k == "db.name")
                );
                assert!(!format!("{:?}", instrumentation.properties).contains("secret"));
            }
        }
        // Explicit selection wins even when a SQLite filename resembles another URL.
        let mut explicit = FastraceInstrumentation::sqlite("");
        explicit.start_connection("postgres://example.db");
        assert!(explicit.backend == Backend::Sqlite);
        let generic =
            FastraceInstrumentation::new("mysql://localhost/db").with_query_capture(false);
        assert!(generic.backend == Backend::Mysql);
        assert!(!generic.capture_queries);
    }

    #[test]
    fn mysql_metadata_uses_mysql_names_and_omits_url_credentials() {
        let properties = database_properties(
            Backend::Mysql,
            "mysql://alice:secret@db.example:3306/catalog?ssl_key=private-key",
            "QUERY",
        );
        for (key, value) in [
            ("db.system.name", "mysql"),
            ("db.namespace", "catalog"),
            ("server.address", "db.example"),
            ("server.port", "3306"),
        ] {
            assert!(properties.contains(&(key, value.to_owned())));
        }
        let formatted = format!("{properties:?}");
        for excluded in ["alice", "secret", "private-key"] {
            assert!(!formatted.contains(excluded));
        }
    }

    #[test]
    fn sqlite_metadata_accepts_paths_uris_and_memory_without_network_attributes() {
        for (url, database) in [
            (":memory:", ":memory:"),
            ("relative.db", "relative.db"),
            ("/tmp/example.db", "/tmp/example.db"),
            (
                "file:/tmp/example.db?mode=ro&cache=private",
                "/tmp/example.db",
            ),
            ("file::memory:?cache=shared", ":memory:"),
        ] {
            let properties = database_properties(Backend::Sqlite, url, "CONNECT");
            assert!(properties.contains(&("db.system.name", "sqlite".to_owned())));
            assert!(properties.contains(&("db.namespace", database.to_owned())));
            assert!(!properties.iter().any(|(key, _)| key.starts_with("server.")));
        }
    }

    #[test]
    fn query_events_record_sql_and_arguments_on_each_span() {
        use diesel::pg::Pg;
        use diesel::sql_types::{Integer, Text};
        use fastrace::collector::{Config, SpanContext, TestReporter};

        let (reporter, records) = TestReporter::new();
        fastrace::set_reporter(reporter, Config::default());
        {
            let root = Span::root("query tracing test", SpanContext::random());
            let _parent = root.set_local_parent();
            let mut instrumentation = FastraceInstrumentation::postgres("postgres:///example_db");
            for (number, argument) in [
                (42, "first 'argument'"),
                (7, "second -- binds: [value]\nline"),
            ] {
                let query = diesel::sql_query("SELECT $1, $2")
                    .bind::<Integer, _>(number)
                    .bind::<Text, _>(argument);
                let debug = diesel::debug_query::<Pg, _>(&query);
                instrumentation.on_connection_event(InstrumentationEvent::start_query(&debug));
                instrumentation
                    .on_connection_event(InstrumentationEvent::finish_query(&debug, None));
            }
            let mut disabled = FastraceInstrumentation::postgres("postgres:///example_db")
                .with_query_capture(false);
            disabled.on_connection_event(InstrumentationEvent::start_query(&NeverFormatQuery));
            disabled
                .on_connection_event(InstrumentationEvent::finish_query(&NeverFormatQuery, None));
            // Re-enabling capture restores the query attribute.
            disabled = disabled.with_query_capture(true);
            let query = diesel::sql_query("SELECT 1");
            let debug = diesel::debug_query::<Pg, _>(&query);
            disabled.on_connection_event(InstrumentationEvent::start_query(&debug));
            disabled.on_connection_event(InstrumentationEvent::finish_query(&debug, None));
            // Broken query formatting must not panic or leave a partial SQL attribute.
            let broken = BrokenQuery;
            instrumentation.on_connection_event(InstrumentationEvent::start_query(&broken));
            instrumentation.on_connection_event(InstrumentationEvent::finish_query(&broken, None));
        }
        fastrace::flush();
        let records = records.lock();
        let queries: Vec<_> = records
            .iter()
            .filter(|s| s.name == "PostgreSQL query")
            .collect();
        assert_eq!(queries.len(), 5);
        let texts: Vec<_> = queries
            .iter()
            .flat_map(|s| s.properties.iter())
            .filter(|(key, _)| key == "db.query.text")
            .map(|(_, value)| value.as_ref())
            .collect();
        assert_eq!(texts.len(), 3);
        assert!(texts.contains(&"SELECT 1 -- binds: []"));
        assert!(texts.contains(&"SELECT $1, $2 -- binds: [42, \"first 'argument'\"]"));
        assert!(
            texts.contains(&"SELECT $1, $2 -- binds: [7, \"second -- binds: [value]\\nline\"]")
        );
    }

    #[derive(Debug)]
    struct NeverFormatQuery;

    impl std::fmt::Display for NeverFormatQuery {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("disabled query capture must not format SQL or arguments")
        }
    }

    impl diesel::connection::DebugQuery for NeverFormatQuery {}

    #[derive(Debug)]
    struct BrokenQuery;

    impl std::fmt::Display for BrokenQuery {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("partial query")?;
            Err(std::fmt::Error)
        }
    }

    impl diesel::connection::DebugQuery for BrokenQuery {}

    #[test]
    fn postgres_metadata_excludes_credentials_and_includes_graph_attributes() {
        let properties = database_properties(
            Backend::Postgres,
            "postgres://alice:secret@postgres:5433/example_db",
            "QUERY",
        );

        assert!(properties.contains(&("span.kind", "client".to_owned())));
        assert!(properties.contains(&("db.system.name", "postgresql".to_owned())));
        assert!(properties.contains(&("db.name", "example_db".to_owned())));
        assert!(properties.contains(&("db.operation.name", "QUERY".to_owned())));
        assert!(properties.contains(&("server.address", "postgres".to_owned())));
        assert!(properties.contains(&("server.port", "5433".to_owned())));
        assert!(!format!("{properties:?}").contains("alice"));
        assert!(!format!("{properties:?}").contains("secret"));
    }

    #[test]
    fn invalid_url_still_classifies_postgresql_client_spans() {
        assert_eq!(
            database_properties(Backend::Postgres, "not a URL", "QUERY"),
            vec![
                ("span.kind", "client".to_owned()),
                ("db.system.name", "postgresql".to_owned()),
                ("db.system", "postgresql".to_owned()),
                ("db.operation.name", "QUERY".to_owned()),
            ]
        );
    }

    #[test]
    fn database_error_kind_is_specific_without_collecting_the_message() {
        let error = Error::DatabaseError(
            DatabaseErrorKind::UniqueViolation,
            Box::new("Key (token)=(secret) already exists".to_owned()),
        );

        let properties = database_error_properties(&error);

        assert!(properties.contains(&("span.status_code", "error".to_owned())));
        assert!(properties.contains(&("error.type", "unique_violation".to_owned())));
        assert!(!format!("{properties:?}").contains("secret"));
    }

    struct TestErrorInformation;

    impl DatabaseErrorInformation for TestErrorInformation {
        fn message(&self) -> &str {
            "sensitive message"
        }

        fn details(&self) -> Option<&str> {
            Some("sensitive details")
        }

        fn hint(&self) -> Option<&str> {
            Some("sensitive hint")
        }

        fn table_name(&self) -> Option<&str> {
            Some("rentals")
        }

        fn column_name(&self) -> Option<&str> {
            Some("property_code")
        }

        fn constraint_name(&self) -> Option<&str> {
            Some("rentals_property_code_key")
        }

        fn statement_position(&self) -> Option<i32> {
            Some(42)
        }
    }

    #[test]
    fn database_error_metadata_keeps_identifiers_but_excludes_free_text() {
        let error = Error::DatabaseError(
            DatabaseErrorKind::UniqueViolation,
            Box::new(TestErrorInformation),
        );

        let properties = database_error_properties(&error);

        assert!(properties.contains(&("db.collection.name", "rentals".to_owned())));
        assert!(properties.contains(&("diesel.error.column", "property_code".to_owned())));
        assert!(properties.contains(&(
            "diesel.error.constraint",
            "rentals_property_code_key".to_owned(),
        )));
        assert!(properties.contains(&("diesel.error.statement_position", "42".to_owned(),)));
        let debug = format!("{properties:?}");
        assert!(!debug.contains("sensitive message"));
        assert!(!debug.contains("sensitive details"));
        assert!(!debug.contains("sensitive hint"));
    }

    #[test]
    fn transaction_span_stays_open_across_body_queries_until_commit_finishes() {
        let mut instrumentation = FastraceInstrumentation::postgres("postgres:///example_db");

        instrumentation.start_transaction_command(TransactionAction::Begin, 1);
        assert_eq!(instrumentation.transactions.len(), 1);
        instrumentation.start_query();
        instrumentation.finish_query(None);

        instrumentation.start_query();
        instrumentation.finish_query(None);
        assert_eq!(instrumentation.transactions.len(), 1);

        instrumentation.start_transaction_command(TransactionAction::Commit, 1);
        assert_eq!(instrumentation.transactions.len(), 1);
        instrumentation.start_query();
        instrumentation.finish_query(None);
        assert!(instrumentation.transactions.is_empty());
    }

    #[test]
    fn nested_transaction_span_closes_without_closing_its_parent() {
        let mut instrumentation = FastraceInstrumentation::postgres("postgres:///example_db");

        instrumentation.start_transaction_command(TransactionAction::Begin, 1);
        instrumentation.start_query();
        instrumentation.finish_query(None);
        instrumentation.start_transaction_command(TransactionAction::Begin, 2);
        instrumentation.start_query();
        instrumentation.finish_query(None);
        assert_eq!(instrumentation.transactions.len(), 2);

        instrumentation.start_transaction_command(TransactionAction::Rollback, 2);
        instrumentation.start_query();
        instrumentation.finish_query(None);
        assert_eq!(instrumentation.transactions.len(), 1);

        instrumentation.start_transaction_command(TransactionAction::Commit, 1);
        instrumentation.start_query();
        instrumentation.finish_query(None);
        assert!(instrumentation.transactions.is_empty());
    }

    #[test]
    fn failed_begin_or_transaction_close_does_not_leave_a_span_open() {
        let mut instrumentation = FastraceInstrumentation::postgres("postgres:///example_db");
        let error = Error::RollbackTransaction;

        instrumentation.start_transaction_command(TransactionAction::Begin, 1);
        instrumentation.start_query();
        instrumentation.finish_query(Some(&error));
        assert!(instrumentation.transactions.is_empty());

        instrumentation.start_transaction_command(TransactionAction::Begin, 1);
        instrumentation.start_query();
        instrumentation.finish_query(None);
        instrumentation.start_transaction_command(TransactionAction::Commit, 1);
        instrumentation.start_query();
        instrumentation.finish_query(Some(&error));
        assert!(instrumentation.transactions.is_empty());
    }

    #[test]
    fn nested_transaction_commands_use_savepoint_operations() {
        assert_eq!(
            TransactionCommand {
                action: TransactionAction::Begin,
                depth: 2,
            }
            .query_operation(),
            "SAVEPOINT"
        );
        assert_eq!(
            TransactionCommand {
                action: TransactionAction::Commit,
                depth: 2,
            }
            .query_operation(),
            "RELEASE SAVEPOINT"
        );
        assert_eq!(
            TransactionCommand {
                action: TransactionAction::Rollback,
                depth: 2,
            }
            .query_operation(),
            "ROLLBACK TO SAVEPOINT"
        );
    }
}
