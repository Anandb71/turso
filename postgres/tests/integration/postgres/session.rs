use std::sync::Arc;

use crate::common::TempDatabase;
use turso_core::{
    ConnectOpts, Database, DatabaseOpts, EncryptionKey, EncryptionOpts, Numeric, OpenOptions,
    SqliteDialect, Value,
};
use turso_pg::{PgConnection, PgDatabase};

#[test]
fn schema_created_by_one_session_is_visible_to_another() {
    let (_db, conn1, conn2) = two_sessions();
    conn2.execute("SELECT 1").unwrap();

    conn1.execute("CREATE SCHEMA s").unwrap();
    conn1.execute("CREATE TABLE s.t (x int)").unwrap();
    conn1.execute("INSERT INTO s.t VALUES (1)").unwrap();

    assert_eq!(query_integers(&conn2, "SELECT x FROM s.t"), vec![1]);
}

#[test]
fn schema_dropped_by_one_session_is_gone_for_another() {
    let (_db, conn1, conn2) = two_sessions();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn1.execute("CREATE TABLE s.t (x int)").unwrap();
    assert_eq!(query_integers(&conn2, "SELECT count(*) FROM s.t"), vec![0]);

    conn1.execute("DROP SCHEMA s CASCADE").unwrap();

    assert!(conn2.execute("SELECT count(*) FROM s.t").is_err());
}

#[test]
fn schema_dropped_and_created_again_by_one_session_is_the_new_schema_for_another() {
    let (_db, conn1, conn2) = two_sessions();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn1.execute("CREATE TABLE s.t (x int)").unwrap();
    assert_eq!(query_integers(&conn2, "SELECT count(*) FROM s.t"), vec![0]);

    conn1.execute("DROP SCHEMA s CASCADE").unwrap();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn2.execute("CREATE TABLE s.u (x int)").unwrap();
    conn2.execute("INSERT INTO s.u VALUES (1)").unwrap();

    assert_eq!(query_integers(&conn1, "SELECT x FROM s.u"), vec![1]);
}

#[test]
fn session_in_a_transaction_cannot_use_a_schema_that_another_session_dropped() {
    let (_db, conn1, conn2) = two_sessions();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn1.execute("CREATE TABLE s.t (x int)").unwrap();
    assert_eq!(query_integers(&conn2, "SELECT count(*) FROM s.t"), vec![0]);
    conn2.execute("BEGIN").unwrap();

    conn1.execute("DROP SCHEMA s CASCADE").unwrap();
    conn1.execute("CREATE SCHEMA s").unwrap();

    let error = conn2.execute("CREATE TABLE s.u (x int)").unwrap_err();
    assert!(error.to_string().contains("dropped"), "{error}");
    conn2.execute("ROLLBACK").unwrap();
    conn2.execute("CREATE TABLE s.u (x int)").unwrap();
    conn2.execute("INSERT INTO s.u VALUES (1)").unwrap();
    assert_eq!(query_integers(&conn1, "SELECT x FROM s.u"), vec![1]);
}

#[test]
fn schema_with_uncommitted_writes_of_another_session_cannot_be_dropped() {
    let (_db, conn1, conn2) = two_sessions();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn2.execute("BEGIN").unwrap();
    conn2.execute("CREATE TABLE s.t (x int)").unwrap();
    conn2.execute("INSERT INTO s.t VALUES (1)").unwrap();

    let error = conn1.execute("DROP SCHEMA s").unwrap_err();
    assert!(error.to_string().contains("busy"), "{error}");

    conn2.execute("COMMIT").unwrap();
    assert_eq!(query_integers(&conn1, "SELECT x FROM s.t"), vec![1]);
}

#[test]
fn schema_created_by_one_session_cannot_be_created_by_another_in_a_transaction() {
    let (_db, conn1, conn2) = two_sessions();
    conn2.execute("BEGIN").unwrap();

    conn1.execute("CREATE SCHEMA s").unwrap();

    let error = conn2.execute("CREATE SCHEMA s").unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
}

#[test]
fn schema_attached_with_uppercase_name_does_not_break_later_statements() {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_attach(true))
        .build();
    let pg_db = PgDatabase::new(db.db.clone());
    let conn = pg_db.connect(ConnectOpts::new()).unwrap();
    let schema_path = db.path.with_file_name("schema-s.db");

    conn.attach_schema("S", schema_path.to_str().unwrap())
        .unwrap();

    conn.execute("SELECT 1").unwrap();
    pg_db
        .connect(ConnectOpts::new())
        .unwrap()
        .execute("SELECT 1")
        .unwrap();
}

#[test]
fn file_of_dropped_schema_is_deleted() {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_attach(true))
        .build();
    let conn = PgDatabase::new(db.db.clone())
        .connect(ConnectOpts::new())
        .unwrap();
    conn.execute("CREATE SCHEMA s").unwrap();

    conn.execute("DROP SCHEMA s").unwrap();

    assert_eq!(schema_files(&db), Vec::<String>::new());
}

#[test]
fn file_of_dropped_schema_is_deleted_when_the_last_session_detaches_it() {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_attach(true))
        .build();
    let pg_db = PgDatabase::new(db.db.clone());
    let conn1 = pg_db.connect(ConnectOpts::new()).unwrap();
    let conn2 = pg_db.connect(ConnectOpts::new()).unwrap();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn2.execute("SELECT 1").unwrap();

    conn1.execute("DROP SCHEMA s").unwrap();
    assert!(schema_files(&db)
        .iter()
        .all(|file| file.starts_with("turso-postgres-schema-s.db.dropped-")));

    conn2.execute("SELECT 1").unwrap();
    assert_eq!(schema_files(&db), Vec::<String>::new());
}

#[test]
fn schema_created_again_while_another_session_uses_the_dropped_one_is_empty() {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_attach(true))
        .build();
    let pg_db = PgDatabase::new(db.db.clone());
    let conn1 = pg_db.connect(ConnectOpts::new()).unwrap();
    let conn2 = pg_db.connect(ConnectOpts::new()).unwrap();
    conn1.execute("CREATE SCHEMA s").unwrap();
    conn1.execute("CREATE TABLE s.t (x int)").unwrap();
    conn2.execute("SELECT 1").unwrap();

    conn1.execute("DROP SCHEMA s").unwrap();
    conn1.execute("CREATE SCHEMA s").unwrap();

    assert!(conn1.execute("SELECT x FROM s.t").is_err());
    assert!(conn2.execute("SELECT x FROM s.t").is_err());
}

/// Names of the schema files next to the database file.
fn schema_files(db: &TempDatabase) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(db.path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("turso-postgres-schema-"))
        .collect();
    files.sort();
    files
}

const HEXKEY: &str = "b1bbfda4f589dc9daaf004fe21111e00dc00c98237102f5c7002a5669fc76327";

#[test]
fn session_reads_encrypted_database_with_the_key_it_is_given() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test.db");
    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::PlatformIO::new().unwrap());
    let db = Database::open(
        io,
        path.to_str().unwrap(),
        OpenOptions::new(Arc::new(SqliteDialect))
            .db_opts(DatabaseOpts::new().with_encryption(true))
            .encryption(EncryptionOpts {
                cipher: "aegis256".to_string(),
                hexkey: HEXKEY.to_string(),
            }),
    )
    .unwrap();
    let key = EncryptionKey::from_hex_string(HEXKEY).unwrap();
    let opts = ConnectOpts::new().with_encryption_key(Some(key));
    let db = PgDatabase::new(db);
    let conn = db.connect(opts.clone()).unwrap();
    conn.execute("CREATE TABLE t (x int)").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    let session = db.connect(opts).unwrap();

    assert_eq!(query_integers(&session, "SELECT count(*) FROM t"), vec![1]);
}

/// Opens a database with two sessions.
fn two_sessions() -> (TempDatabase, PgConnection, PgConnection) {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_attach(true))
        .build();
    let pg_db = PgDatabase::new(db.db.clone());
    let conn1 = pg_db.connect(ConnectOpts::new()).unwrap();
    let conn2 = pg_db.connect(ConnectOpts::new()).unwrap();
    (db, conn1, conn2)
}

/// Runs `sql` and returns the first column of each row.
fn query_integers(conn: &PgConnection, sql: &str) -> Vec<i64> {
    let mut stmt = conn.query(sql).unwrap().unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| match row[0] {
            Value::Numeric(Numeric::Integer(value)) => value,
            ref other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}
