//! Native fastrace instrumentation for Diesel connections.
//!
//! Query text and bind values are deliberately not collected: they can contain credentials,
//! collaboration tokens, and other user data. The emitted client spans contain only the stable
//! database and network metadata needed to classify the dependency in an OpenTelemetry service
//! graph.

use diesel::connection::{Instrumentation, InstrumentationEvent, set_default_instrumentation};
use diesel::result::{ConnectionError, DatabaseErrorInformation, DatabaseErrorKind, Error};
use fastrace::{Event, Span};
use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};
use std::sync::OnceLock;
use url::Url;

/// Instruments one Diesel connection with OpenTelemetry-compatible fastrace query spans.
pub struct FastraceInstrumentation {
    active_connection: Option<Span>,
    active_query: Option<Span>,
    properties: Vec<(&'static str, String)>,
    transactions: Vec<ActiveTransaction>,
    pending_transaction: Option<TransactionCommand>,
}

impl FastraceInstrumentation {
    /// Creates PostgreSQL instrumentation from a connection URL without retaining credentials.
    pub fn postgres(database_url: &str) -> Self {
        Self {
            active_connection: None,
            active_query: None,
            properties: postgres_properties(database_url, "QUERY"),
            transactions: Vec::new(),
            pending_transaction: None,
        }
    }

    fn postgres_without_url() -> Self {
        Self::postgres("")
    }

    fn start_connection(&mut self, database_url: &str) {
        self.properties = postgres_properties(database_url, "QUERY");
        let span = Span::enter_with_local_parent("PostgreSQL connect");
        let properties = postgres_properties(database_url, "CONNECT");
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
            "PostgreSQL query".to_owned()
        } else {
            format!("PostgreSQL {operation}")
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

    fn record_prepared_statement_cache_insertion(&self) {
        static PREPARATIONS: OnceLock<Counter<u64>> = OnceLock::new();
        let preparations = PREPARATIONS.get_or_init(|| {
            global::meter("diesel-fastrace")
                .u64_counter("diesel.statement_cache.preparations")
                .with_description("Statements added to Diesel's per-connection prepared cache")
                .build()
        });
        preparations.add(1, &metric_attributes(&self.properties));
    }

    fn start_transaction_command(&mut self, action: TransactionAction, depth: u32) {
        let command = TransactionCommand { action, depth };
        if action == TransactionAction::Begin {
            let span = if let Some(parent) = self.transactions.last() {
                Span::enter_with_parent("PostgreSQL transaction", &parent.span)
            } else {
                Span::enter_with_local_parent("PostgreSQL transaction")
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
pub fn install_default_postgres_instrumentation() -> diesel::QueryResult<()> {
    set_default_instrumentation(|| Some(Box::new(FastraceInstrumentation::postgres_without_url())))
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
            InstrumentationEvent::StartQuery { .. } => self.start_query(),
            InstrumentationEvent::CacheQuery { .. } => {
                self.record_prepared_statement_cache_insertion();
                if let Some(span) = self.active_query.as_ref() {
                    span.add_property(|| ("db.query.prepared_cache_inserted", "true"));
                    span.add_event(Event::new("PostgreSQL prepared statement cached"));
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

fn metric_attributes(properties: &[(&'static str, String)]) -> Vec<KeyValue> {
    properties
        .iter()
        .filter(|(key, _)| {
            matches!(
                *key,
                "db.system.name" | "db.namespace" | "server.address" | "server.port"
            )
        })
        .map(|(key, value)| KeyValue::new(*key, value.clone()))
        .collect()
}

fn postgres_properties(database_url: &str, operation: &str) -> Vec<(&'static str, String)> {
    let mut properties = vec![
        ("span.kind", "client".to_owned()),
        // Keep both the stable semantic-convention name and Uptrace's legacy-compatible key.
        ("db.system.name", "postgresql".to_owned()),
        ("db.system", "postgresql".to_owned()),
        // Diesel does not expose a structured operation without formatting the query. Keep the
        // operation generic rather than collecting SQL just to distinguish SELECT/INSERT/etc.
        ("db.operation.name", operation.to_owned()),
    ];

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
    fn postgres_metadata_excludes_credentials_and_includes_graph_attributes() {
        let properties =
            postgres_properties("postgres://alice:secret@postgres:5433/beachout", "QUERY");

        assert!(properties.contains(&("span.kind", "client".to_owned())));
        assert!(properties.contains(&("db.system.name", "postgresql".to_owned())));
        assert!(properties.contains(&("db.name", "beachout".to_owned())));
        assert!(properties.contains(&("db.operation.name", "QUERY".to_owned())));
        assert!(properties.contains(&("server.address", "postgres".to_owned())));
        assert!(properties.contains(&("server.port", "5433".to_owned())));
        assert!(!format!("{properties:?}").contains("alice"));
        assert!(!format!("{properties:?}").contains("secret"));

        let metric_attributes = metric_attributes(&properties);
        assert_eq!(metric_attributes.len(), 4);
        assert!(!format!("{metric_attributes:?}").contains("alice"));
        assert!(!format!("{metric_attributes:?}").contains("secret"));
    }

    #[test]
    fn invalid_url_still_classifies_postgresql_client_spans() {
        assert_eq!(
            postgres_properties("not a URL", "QUERY"),
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
        let mut instrumentation = FastraceInstrumentation::postgres("postgres:///beachout");

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
        let mut instrumentation = FastraceInstrumentation::postgres("postgres:///beachout");

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
        let mut instrumentation = FastraceInstrumentation::postgres("postgres:///beachout");
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
