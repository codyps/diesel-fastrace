use diesel::sql_types::{Integer, Text};
use fastrace_diesel::FastraceInstrumentation;
type TestConnection = diesel::SqliteConnection;
const BACKEND: &str = "SQLite";
const SYSTEM: &str = "sqlite";
const BOUND_SQL: &str = "SELECT ?, ?";
const CREATE_TABLE: &str = "CREATE TEMPORARY TABLE trace_items (id INTEGER PRIMARY KEY)";
use fastrace_diesel::install_default_sqlite_instrumentation as install_default;
fn instrumentation(url: &str) -> FastraceInstrumentation {
    FastraceInstrumentation::sqlite(url)
}
fn database_url() -> String {
    ":memory:".to_owned()
}
fn invalid_database_url(_: &str) -> String {
    "file:/fastrace-diesel-nonexistent-directory/database.sqlite?mode=ro".to_owned()
}
include!("../common/suite.rs");

#[test]
fn immediate_and_exclusive_transactions_are_traced() {
    let spans = capture(true, |url| {
        let mut conn = TestConnection::establish(url).unwrap();
        conn.immediate_transaction::<(), Error, _>(|conn| {
            bound_query(conn, 8, "immediate body");
            Ok(())
        })
        .unwrap();
        conn.exclusive_transaction::<(), Error, _>(|conn| {
            bound_query(conn, 9, "exclusive body");
            Ok(())
        })
        .unwrap();
    });
    let transactions = named(&spans, "SQLite transaction");
    assert_eq!(transactions.len(), 2);
    for sql in ["BEGIN IMMEDIATE", "BEGIN EXCLUSIVE"] {
        let begin = spans
            .iter()
            .find(|s| property(s, "db.query.text") == Some(sql))
            .unwrap();
        let tx = transactions
            .iter()
            .find(|s| s.span_id == begin.parent_id)
            .unwrap();
        assert_child(tx, begin);
        assert_eq!(property(tx, "db.transaction.outcome"), Some("commit"));
    }
}

#[test]
fn file_database_can_be_reopened_via_uri() {
    struct TemporaryDirectory(std::path::PathBuf);
    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = TemporaryDirectory(
        std::env::temp_dir().join(format!("fastrace-diesel-{}-{unique}", std::process::id())),
    );
    std::fs::create_dir(&dir.0).unwrap();
    let file = dir.0.join("test.db");
    let spans = capture(true, |_| {
        let mut conn = TestConnection::establish(file.to_str().unwrap()).unwrap();
        conn.batch_execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY); INSERT INTO items VALUES (1)",
        )
        .unwrap();
        drop(conn);
        let mut uri = url::Url::from_file_path(&file).unwrap();
        uri.query_pairs_mut().append_pair("mode", "ro");
        let mut conn = TestConnection::establish(uri.as_str()).unwrap();
        bound_query(&mut conn, 10, "file query");
    });
    let connections = named(&spans, "SQLite connect");
    assert_eq!(connections.len(), 2);
    for connection in connections {
        assert_eq!(property(connection, "span.status_code"), Some("ok"));
        assert_eq!(property(connection, "db.namespace"), file.to_str());
        assert!(
            !connection
                .properties
                .iter()
                .any(|(key, _)| key.starts_with("server."))
        );
    }
}

fn connect(url: &str) -> TestConnection {
    TestConnection::establish(url).unwrap()
}

fn bound_query(conn: &mut TestConnection, number: i32, text: &str) {
    let result: (i32, String) =
        diesel::select((number.into_sql::<Integer>(), text.into_sql::<Text>()))
            .get_result(conn)
            .unwrap();
    assert_eq!(result, (number, text.to_owned()));
}
