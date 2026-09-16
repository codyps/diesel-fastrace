use diesel::connection::{SimpleConnection, set_default_instrumentation};
use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error};
use fastrace::Span;
use fastrace::collector::{Config, Reporter, SpanContext, SpanRecord};
use std::sync::{Arc, Mutex, OnceLock};

// Diesel's default factory and fastrace's reporter are process-global. Serialize
// these tests, including collection, so they cannot replace each other's state.
static TEST_LOCK: Mutex<()> = Mutex::new(());
static RECORDS: OnceLock<Arc<Mutex<Vec<SpanRecord>>>> = OnceLock::new();

struct Capture(Arc<Mutex<Vec<SpanRecord>>>);
impl Reporter for Capture {
    fn report(&mut self, spans: Vec<SpanRecord>) {
        self.0.lock().unwrap().extend(spans);
    }
}

fn capture(enabled: bool, body: impl FnOnce(&str)) -> Vec<SpanRecord> {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let records = RECORDS.get_or_init(|| {
        let records = Arc::new(Mutex::new(Vec::new()));
        fastrace::set_reporter(Capture(records.clone()), Config::default());
        records
    });
    fastrace::flush();
    records.lock().unwrap().clear();
    if enabled {
        install_default().unwrap();
    } else {
        set_default_instrumentation(|| {
            Some(Box::new(instrumentation("").with_query_capture(false)))
        })
        .unwrap();
    }
    {
        let root = Span::root("integration test", SpanContext::random());
        let _parent = root.set_local_parent();
        body(&database_url());
    }
    fastrace::flush();
    std::mem::take(&mut *records.lock().unwrap())
}

fn property<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a str> {
    span.properties
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_ref())
}

fn named<'a>(spans: &'a [SpanRecord], name: &str) -> Vec<&'a SpanRecord> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn one<'a>(spans: &'a [SpanRecord], name: &str) -> &'a SpanRecord {
    let matches = named(spans, name);
    assert_eq!(matches.len(), 1, "expected one {name}: {spans:#?}");
    matches[0]
}

fn assert_child(parent: &SpanRecord, child: &SpanRecord) {
    assert_eq!(child.trace_id, parent.trace_id);
    assert_eq!(child.parent_id, parent.span_id);
    assert!(child.begin_time_unix_ns >= parent.begin_time_unix_ns);
    assert!(
        child.begin_time_unix_ns + child.duration_ns
            <= parent.begin_time_unix_ns + parent.duration_ns
    );
}

#[test]
fn default_factory_traces_successful_and_failed_connections() {
    let spans = capture(true, |url| {
        drop(TestConnection::establish(url).expect("connect to test database"));
        assert!(TestConnection::establish(&invalid_database_url(url)).is_err());
    });
    let root = one(&spans, "integration test");
    let connects = named(&spans, &format!("{BACKEND} connect"));
    assert_eq!(connects.len(), 2);
    let success = connects
        .iter()
        .find(|s| property(s, "span.status_code") == Some("ok"))
        .unwrap();
    let failure = connects
        .iter()
        .find(|s| property(s, "span.status_code") == Some("error"))
        .unwrap();
    assert_eq!(property(failure, "error.type"), Some("bad_connection"));
    for connect in &connects {
        assert_child(root, connect);
        assert_eq!(property(connect, "db.system.name"), Some(SYSTEM));
        assert_eq!(property(connect, "db.operation.name"), Some("CONNECT"));
        assert_eq!(property(connect, "span.kind"), Some("client"));
        assert!(property(connect, "db.namespace").is_some());
        if SYSTEM == "sqlite" {
            assert!(
                !connect
                    .properties
                    .iter()
                    .any(|(key, _)| key.starts_with("server."))
            );
        }
    }
    assert!(success.duration_ns > 0);
}

#[test]
fn real_queries_capture_arguments_and_report_only_cache_insertions() {
    let spans = capture(true, |url| {
        let mut conn = connect(url);
        bound_query(&mut conn, 42, "first 'argument'");
        bound_query(&mut conn, 7, "second -- binds: [value]\nline");
    });
    let root = one(&spans, "integration test");
    let queries: Vec<_> = named(&spans, &format!("{BACKEND} query"))
        .into_iter()
        .filter(|s| property(s, "db.query.text").is_some_and(|text| text.starts_with(BOUND_SQL)))
        .collect();
    assert_eq!(queries.len(), 2, "{spans:#?}");
    let first = queries
        .iter()
        .find(|s| {
            property(s, "db.query.text")
                == Some(format!("{BOUND_SQL} -- binds: [42, \"first 'argument'\"]").as_str())
        })
        .unwrap();
    let second = queries
        .iter()
        .find(|s| {
            property(s, "db.query.text")
                == Some(
                    format!("{BOUND_SQL} -- binds: [7, \"second -- binds: [value]\\nline\"]")
                        .as_str(),
                )
        })
        .unwrap();
    for query in &queries {
        assert_child(root, query);
        assert_eq!(property(query, "span.status_code"), Some("ok"));
        assert!(query.duration_ns > 0);
    }
    assert_eq!(
        property(first, "db.query.prepared_cache_inserted"),
        Some("true")
    );
    assert_eq!(
        first
            .events
            .iter()
            .filter(|e| e.name == format!("{BACKEND} prepared statement cached"))
            .count(),
        1
    );
    assert_eq!(property(second, "db.query.prepared_cache_inserted"), None);
    assert!(second.events.is_empty());
}

#[test]
fn database_errors_are_attached_to_the_failing_query() {
    let spans = capture(true, |url| {
        let mut conn = connect(url);
        conn.batch_execute(CREATE_TABLE).unwrap();
        diesel::sql_query("INSERT INTO trace_items VALUES (1)")
            .execute(&mut conn)
            .unwrap();
        let error = diesel::sql_query("INSERT INTO trace_items VALUES (1)")
            .execute(&mut conn)
            .unwrap_err();
        assert!(matches!(
            error,
            Error::DatabaseError(DatabaseErrorKind::UniqueViolation, _)
        ));
        bound_query(&mut conn, 10, "after error");
    });
    let failures: Vec<_> = named(&spans, &format!("{BACKEND} query"))
        .into_iter()
        .filter(|s| property(s, "span.status_code") == Some("error"))
        .collect();
    assert_eq!(failures.len(), 1);
    let failed = failures[0];
    assert_eq!(property(failed, "error.type"), Some("unique_violation"));
    if SYSTEM == "postgresql" {
        assert_eq!(property(failed, "db.collection.name"), Some("trace_items"));
        assert_eq!(
            property(failed, "diesel.error.constraint"),
            Some("trace_items_pkey")
        );
    }
    assert!(
        property(failed, "db.query.text")
            .unwrap()
            .contains("INSERT INTO trace_items")
    );
    let after = spans
        .iter()
        .find(|s| property(s, "db.query.text").is_some_and(|v| v.contains("after error")))
        .unwrap();
    assert_eq!(property(after, "span.status_code"), Some("ok"));
    assert_eq!(property(after, "error.type"), None);
}

#[test]
fn nested_transactions_have_correct_parentage_lifetimes_and_outcomes() {
    let spans = capture(true, |url| {
        let mut conn = connect(url);
        conn.transaction::<(), Error, _>(|conn| {
            bound_query(conn, 1, "outer body");
            conn.transaction::<(), Error, _>(|conn| {
                bound_query(conn, 2, "nested commit");
                Ok(())
            })?;
            let rollback = conn.transaction::<(), Error, _>(|conn| {
                bound_query(conn, 3, "nested rollback");
                Err(Error::RollbackTransaction)
            });
            assert!(matches!(rollback, Err(Error::RollbackTransaction)));
            bound_query(conn, 4, "outer after rollback");
            Ok(())
        })
        .unwrap();
        bound_query(&mut conn, 5, "after transaction");
    });
    let root = one(&spans, "integration test");
    let transactions = named(&spans, &format!("{BACKEND} transaction"));
    assert_eq!(transactions.len(), 3);
    let outer = transactions
        .iter()
        .find(|s| property(s, "db.transaction.depth") == Some("1"))
        .unwrap();
    assert_child(root, outer);
    assert_eq!(property(outer, "db.transaction.outcome"), Some("commit"));
    for (body, outcome) in [("nested commit", "commit"), ("nested rollback", "rollback")] {
        let tx = transactions
            .iter()
            .find(|s| {
                property(s, "db.transaction.depth") == Some("2")
                    && property(s, "db.transaction.outcome") == Some(outcome)
            })
            .unwrap();
        assert_child(outer, tx);
        let query = spans
            .iter()
            .find(|s| property(s, "db.query.text").is_some_and(|v| v.contains(body)))
            .unwrap();
        assert_child(tx, query);
    }
    let after_nested = spans
        .iter()
        .find(|s| property(s, "db.query.text").is_some_and(|v| v.contains("outer after rollback")))
        .unwrap();
    assert_child(outer, after_nested);
    for command in [&format!("{BACKEND} BEGIN"), &format!("{BACKEND} COMMIT")] {
        assert_child(outer, one(&spans, command));
    }
    assert_eq!(named(&spans, &format!("{BACKEND} SAVEPOINT")).len(), 2);
    assert_eq!(
        named(&spans, &format!("{BACKEND} RELEASE SAVEPOINT")).len(),
        1
    );
    assert_eq!(
        named(&spans, &format!("{BACKEND} ROLLBACK TO SAVEPOINT")).len(),
        1
    );
    let after = spans
        .iter()
        .find(|s| property(s, "db.query.text").is_some_and(|v| v.contains("after transaction")))
        .unwrap();
    assert_child(root, after);
    assert_eq!(property(after, "db.transaction.depth"), None);
}

#[test]
fn outer_rollback_closes_transaction_and_restores_query_parent() {
    let spans = capture(true, |url| {
        let mut conn = connect(url);
        let result = conn.transaction::<(), Error, _>(|conn| {
            bound_query(conn, 6, "rollback body");
            Err(Error::RollbackTransaction)
        });
        assert!(matches!(result, Err(Error::RollbackTransaction)));
        bound_query(&mut conn, 7, "after rollback");
    });
    let tx = one(&spans, &format!("{BACKEND} transaction"));
    assert_eq!(property(tx, "db.transaction.outcome"), Some("rollback"));
    assert_child(tx, one(&spans, &format!("{BACKEND} ROLLBACK")));
    let after = spans
        .iter()
        .find(|s| property(s, "db.query.text").is_some_and(|v| v.contains("after rollback")))
        .unwrap();
    assert_child(one(&spans, "integration test"), after);
}

#[test]
fn disabling_capture_keeps_query_and_transaction_spans_without_sql_or_arguments() {
    let spans = capture(false, |url| {
        let mut conn = connect(url);
        conn.transaction::<(), Error, _>(|conn| {
            bound_query(conn, 12345, "argument-must-not-appear");
            Ok(())
        })
        .unwrap();
    });
    assert_eq!(named(&spans, &format!("{BACKEND} connect")).len(), 1);
    let tx = one(&spans, &format!("{BACKEND} transaction"));
    let queries = named(&spans, &format!("{BACKEND} query"));
    assert!(!queries.is_empty());
    assert!(spans.iter().all(|s| property(s, "db.query.text").is_none()));
    assert!(!format!("{spans:?}").contains("argument-must-not-appear"));
    assert!(queries.iter().any(|q| q.parent_id == tx.span_id));
    assert_eq!(property(tx, "db.transaction.outcome"), Some("commit"));
}
