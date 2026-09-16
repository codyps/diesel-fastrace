use diesel::connection::{SimpleConnection, set_default_instrumentation};
use diesel::prelude::*;
use diesel::result::Error;
use diesel_fastrace::{FastraceInstrumentation, install_default_instrumentation};
use fastrace::Span;
use fastrace::collector::{Config, Reporter, SpanContext, SpanRecord};
use std::sync::{Arc, Mutex};

struct Capture(Arc<Mutex<Vec<SpanRecord>>>);
impl Reporter for Capture {
    fn report(&mut self, spans: Vec<SpanRecord>) {
        self.0.lock().unwrap().extend(spans);
    }
}
fn property<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a str> {
    span.properties
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_ref())
}

#[test]
fn one_factory_traces_all_backends_and_applies_capture_options() {
    let pg_url = std::env::var("DIESEL_FASTRACE_TEST_DATABASE_URL").expect("PostgreSQL test URL");
    let mysql_url = std::env::var("DIESEL_FASTRACE_TEST_MYSQL_URL").expect("MySQL test URL");
    let records = Arc::new(Mutex::new(Vec::new()));
    fastrace::set_reporter(Capture(records.clone()), Config::default());
    for enabled in [true, false] {
        if enabled {
            install_default_instrumentation().unwrap();
        } else {
            set_default_instrumentation(|| {
                Some(Box::new(
                    FastraceInstrumentation::default().with_query_capture(false),
                ))
            })
            .unwrap();
        }
        {
            let root = Span::root("mixed", SpanContext::random());
            let _parent = root.set_local_parent();
            // SQLite accepts an empty filename as a temporary database. Its backend
            // is deliberately ambiguous to the factory, but tracing must still work.
            let mut unknown = SqliteConnection::establish("").unwrap();
            unknown.batch_execute("SELECT 1").unwrap();
            let mut pg = PgConnection::establish(&pg_url).unwrap();
            let mut mysql = MysqlConnection::establish(&mysql_url).unwrap();
            let mut sqlite = SqliteConnection::establish(":memory:").unwrap();
            // Keep transactions on all three connections open at the same time.
            pg.transaction::<(), Error, _>(|pg| {
                mysql.transaction::<(), Error, _>(|mysql| {
                    sqlite.transaction::<(), Error, _>(|sqlite| {
                        macro_rules! query {
                            ($conn:expr) => {{
                                let value: i32 =
                                    diesel::select(42.into_sql::<diesel::sql_types::Integer>())
                                        .get_result($conn)?;
                                assert_eq!(value, 42);
                            }};
                        }
                        query!(pg);
                        query!(mysql);
                        query!(sqlite);
                        Ok(())
                    })
                })
            })
            .unwrap();
            pg.batch_execute("SELECT 1").unwrap();
            mysql.batch_execute("SELECT 1").unwrap();
            sqlite.batch_execute("SELECT 1").unwrap();
        }
        fastrace::flush();
        let spans = std::mem::take(&mut *records.lock().unwrap());
        let root = spans.iter().find(|s| s.name == "mixed").unwrap();
        let unknown_connect = spans.iter().find(|s| s.name == "Database connect").unwrap();
        let unknown_query = spans.iter().find(|s| s.name == "Database query").unwrap();
        for span in [unknown_connect, unknown_query] {
            assert_eq!(span.parent_id, root.span_id);
            assert_eq!(property(span, "span.status_code"), Some("ok"));
            assert_eq!(property(span, "db.system.name"), Some("other_sql"));
            assert_eq!(property(span, "db.namespace"), None);
        }
        assert_eq!(
            property(unknown_query, "db.query.text"),
            enabled.then_some("SELECT 1")
        );
        for (backend, system) in [
            ("PostgreSQL", "postgresql"),
            ("MySQL", "mysql"),
            ("SQLite", "sqlite"),
        ] {
            let find_one = |suffix: &str| {
                let matches: Vec<_> = spans
                    .iter()
                    .filter(|s| s.name == format!("{backend} {suffix}"))
                    .collect();
                assert_eq!(matches.len(), 1, "{spans:#?}");
                matches[0]
            };
            let connect = find_one("connect");
            let tx = find_one("transaction");
            assert_eq!(connect.parent_id, root.span_id);
            assert_eq!(tx.parent_id, root.span_id);
            assert_eq!(property(tx, "db.transaction.outcome"), Some("commit"));
            let queries: Vec<_> = spans
                .iter()
                .filter(|s| s.name == format!("{backend} query"))
                .collect();
            assert!(queries.iter().any(|q| q.parent_id == tx.span_id));
            assert!(queries.iter().any(|q| q.parent_id == root.span_id));
            for span in spans.iter().filter(|s| s.name.starts_with(backend)) {
                assert_eq!(span.trace_id, root.trace_id);
                assert_eq!(property(span, "db.system.name"), Some(system));
                assert_eq!(property(span, "span.status_code"), Some("ok"));
            }
            if enabled {
                assert!(queries.iter().any(|q| {
                    property(q, "db.query.text").is_some_and(|v| v.contains("-- binds: [42]"))
                }));
            }
        }
        if !enabled {
            assert!(spans.iter().all(|s| property(s, "db.query.text").is_none()));
        }
    }
}
