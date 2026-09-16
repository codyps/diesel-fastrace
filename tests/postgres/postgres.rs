use diesel::sql_types::{Integer, Text};
use fastrace_diesel::FastraceInstrumentation;
type TestConnection = diesel::PgConnection;
const BACKEND: &str = "PostgreSQL";
const SYSTEM: &str = "postgresql";
const BOUND_SQL: &str = "SELECT $1, $2";
const CREATE_TABLE: &str = "CREATE TEMP TABLE trace_items (id INTEGER PRIMARY KEY)";
use fastrace_diesel::install_default_postgres_instrumentation as install_default;
fn instrumentation(url: &str) -> FastraceInstrumentation {
    FastraceInstrumentation::postgres(url)
}
fn database_url() -> String {
    std::env::var("FASTRACE_DIESEL_TEST_DATABASE_URL")
        .expect("set FASTRACE_DIESEL_TEST_DATABASE_URL")
}
fn invalid_database_url(url: &str) -> String {
    let mut url = url::Url::parse(url).unwrap();
    url.set_path("/fastrace_diesel_database_that_does_not_exist");
    url.query_pairs_mut().append_pair("connect_timeout", "2");
    url.into()
}
include!("../common/suite.rs");

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
