use fastrace_diesel::FastraceInstrumentation;
type TestConnection = diesel::MysqlConnection;
const BACKEND: &str = "MySQL";
const SYSTEM: &str = "mysql";
const BOUND_SQL: &str = "INSERT INTO `trace_args` (`id`, `value`) VALUES (?, ?)";
const CREATE_TABLE: &str = "CREATE TEMPORARY TABLE trace_items (id INTEGER PRIMARY KEY)";
use fastrace_diesel::install_default_mysql_instrumentation as install_default;
fn instrumentation(url: &str) -> FastraceInstrumentation {
    FastraceInstrumentation::mysql(url)
}
fn database_url() -> String {
    std::env::var("FASTRACE_DIESEL_TEST_MYSQL_URL").expect("set FASTRACE_DIESEL_TEST_MYSQL_URL")
}
fn invalid_database_url(url: &str) -> String {
    let mut url = url::Url::parse(url).unwrap();
    url.set_path("/fastrace_diesel_database_that_does_not_exist");
    url.into()
}
include!("../common/suite.rs");

diesel::table! {
    trace_args (id) {
        id -> Integer,
        value -> Text,
    }
}

fn connect(url: &str) -> TestConnection {
    let mut conn = TestConnection::establish(url).unwrap();
    conn.batch_execute(
        "CREATE TEMPORARY TABLE trace_args (id INTEGER PRIMARY KEY, value VARCHAR(255) NOT NULL)",
    )
    .unwrap();
    conn
}

fn bound_query(conn: &mut TestConnection, number: i32, text: &str) {
    diesel::insert_into(trace_args::table)
        .values((trace_args::id.eq(number), trace_args::value.eq(text)))
        .execute(conn)
        .unwrap();
    let result: (i32, String) = trace_args::table.find(number).first(conn).unwrap();
    assert_eq!(result, (number, text.to_owned()));
}
