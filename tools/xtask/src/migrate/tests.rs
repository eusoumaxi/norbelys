//! Migration acceptance on an explicitly disposable PostgreSQL cluster. These cases replace
//! the old server runner's tests: adoption is deliberately replaced by refusal, while rollback,
//! checksum validation, concurrency and read-only checks remain real database guarantees.

use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::{AssertSqlSafe, Connection as _, PgConnection, SqlSafeStr as _};

use super::{MIGRATIONS, apply, check, provision_roles};

static NEXT_DATABASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Database {
    name: String,
    maintenance: String,
    url: String,
}

impl Database {
    async fn new() -> Self {
        let raw = crate::db::test_url().expect("explicit disposable test cluster");
        let target = crate::db::Target::parse(&raw).unwrap();
        let name = format!(
            "norbelys_migrations_{}_{}",
            std::process::id(),
            NEXT_DATABASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut maintenance = PgConnection::connect(&target.maintenance).await.unwrap();
        provision_roles(&mut maintenance, &[]).await.unwrap();
        // The generated identifier contains only a constant prefix and numeric process/counter IDs.
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut maintenance)
            .await
            .unwrap();
        maintenance.close().await.unwrap();
        let mut url = url::Url::parse(&raw).unwrap();
        url.set_path(&name);
        Self {
            name,
            maintenance: target.maintenance,
            url: url.into(),
        }
    }

    async fn connect(&self) -> PgConnection {
        PgConnection::connect(&self.url).await.unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let url = self.maintenance.clone();
        let name = self.name.clone();
        // A separate runtime also runs cleanup during a failed async test's unwinding.
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                if let Ok(mut connection) = PgConnection::connect(&url).await {
                    let _ =
                        sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
                            .execute(&mut connection)
                            .await;
                    let _ = connection.close().await;
                }
            });
        })
        .join();
    }
}

fn migration(version: i64, sql: &'static str) -> Migration {
    Migration::new(
        version,
        "fixture".into(),
        MigrationType::Simple,
        sql.into_sql_str(),
        false,
    )
}

async fn ledger(connection: &mut PgConnection) -> serde_json::Value {
    sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY version), '[]') FROM public._sqlx_migrations m")
        .fetch_one(connection).await.unwrap()
}

#[tokio::test]
async fn fresh_baseline_repeats_without_changes_and_runtime_logins_can_only_check() {
    let database = Database::new().await;
    let mut connection = database.connect().await;
    assert!(check(&mut connection, &MIGRATIONS).await.is_err());
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert!(absent, "read-only check must not create migration history");
    apply(&mut connection, &MIGRATIONS).await.unwrap();
    let before = ledger(&mut connection).await;
    apply(&mut connection, &MIGRATIONS).await.unwrap();
    assert_eq!(ledger(&mut connection).await, before);
    let ownership: (String, bool, bool) = sqlx::query_as(
        "SELECT pg_get_userbyid(relowner)::text, relrowsecurity, relforcerowsecurity
         FROM pg_class WHERE oid = 'people'::regclass",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(ownership, ("norbelys_owner".into(), true, true));
    for role in ["app", "worker", "tracking", "system"] {
        sqlx::raw_sql(AssertSqlSafe(format!("SET ROLE norbelys_{role}")))
            .execute(&mut connection)
            .await
            .unwrap();
        check(&mut connection, &MIGRATIONS).await.unwrap();
        let denied = sqlx::query("DELETE FROM public._sqlx_migrations")
            .execute(&mut connection)
            .await
            .unwrap_err();
        assert_eq!(
            denied.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501")
        );
        sqlx::query("RESET ROLE")
            .execute(&mut connection)
            .await
            .unwrap();
    }
    assert_eq!(ledger(&mut connection).await, before);
    connection.close().await.unwrap();
}

#[tokio::test]
async fn scripts_preserve_postgres_lexing_and_reject_failed_changed_or_unknown_history() {
    let database = Database::new().await;
    let first = migration(
        1,
        r"CREATE TABLE marker (value text);
        /* an outer ; comment /* nested ; */ still a comment */
        INSERT INTO marker VALUES (E'it\'s;fine');
        CREATE FUNCTION marker_text() RETURNS text LANGUAGE sql AS $$ SELECT ';body;'::text $$;",
    );
    let valid = Migrator::with_migrations(vec![first.clone()]);
    let broken = Migrator::with_migrations(vec![
        first.clone(),
        migration(2, "CREATE TABLE rolled_back (id integer); SELECT 1 / 0;"),
    ]);
    let mut connection = database.connect().await;
    assert!(apply(&mut connection, &broken).await.is_err());
    // SQLx retains its session lock on failure; close before a different invocation.
    connection.close().await.unwrap();
    let mut connection = database.connect().await;
    let values: (String, String, bool) = sqlx::query_as(
        "SELECT value, marker_text(), to_regclass('rolled_back') IS NULL FROM marker",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(values, ("it's;fine".into(), ";body;".into(), true));
    let before = ledger(&mut connection).await;
    assert_eq!(
        before.as_array().unwrap().len(),
        1,
        "earlier successful files remain applied"
    );
    check(&mut connection, &valid).await.unwrap();
    assert!(
        check(&mut connection, &broken).await.is_err(),
        "pending migration"
    );
    let changed = Migrator::with_migrations(vec![migration(1, "SELECT 'changed';")]);
    assert!(check(&mut connection, &changed).await.is_err());
    assert!(apply(&mut connection, &changed).await.is_err());
    connection.close().await.unwrap();
    let mut connection = database.connect().await;
    assert_eq!(ledger(&mut connection).await, before);
    let unknown = Migrator::with_migrations(Vec::new());
    assert!(check(&mut connection, &unknown).await.is_err());
    assert!(apply(&mut connection, &unknown).await.is_err());
    connection.close().await.unwrap();
    let mut connection = database.connect().await;
    assert_eq!(ledger(&mut connection).await, before);
    sqlx::query("UPDATE _sqlx_migrations SET success = false")
        .execute(&mut connection)
        .await
        .unwrap();
    assert!(
        check(&mut connection, &valid).await.is_err(),
        "dirty history"
    );
    assert!(apply(&mut connection, &valid).await.is_err());
    connection.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_invocations_apply_each_file_once() {
    let database = Database::new().await;
    let migrations = Migrator::with_migrations(vec![migration(
        1,
        "SELECT pg_sleep(0.1); CREATE TABLE applied_once (id integer); INSERT INTO applied_once VALUES (1);",
    )]);
    let mut left = database.connect().await;
    let mut right = database.connect().await;
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(
            apply(&mut left, &migrations),
            apply(&mut right, &migrations)
        )
    })
    .await
    .expect("migration lock must make progress");
    a.unwrap();
    b.unwrap();
    assert_eq!(ledger(&mut left).await.as_array().unwrap().len(), 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM applied_once")
        .fetch_one(&mut left)
        .await
        .unwrap();
    assert_eq!(count, 1);
    left.close().await.unwrap();
    right.close().await.unwrap();
}

#[tokio::test]
async fn previous_internal_or_unknown_schema_is_never_adopted() {
    let database = Database::new().await;
    let mut connection = database.connect().await;
    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (version bigint); INSERT INTO schema_migrations VALUES (1)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    assert!(apply(&mut connection, &MIGRATIONS).await.is_err());
    assert!(check(&mut connection, &MIGRATIONS).await.is_err());
    let preserved: (i64, bool) = sqlx::query_as(
        "SELECT version, to_regclass('public._sqlx_migrations') IS NULL FROM schema_migrations",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(preserved, (1, true));
    sqlx::raw_sql("DROP TABLE schema_migrations; CREATE FUNCTION unrecognized() RETURNS int LANGUAGE sql AS $$SELECT 1$$")
        .execute(&mut connection).await.unwrap();
    assert!(
        apply(&mut connection, &MIGRATIONS).await.is_err(),
        "function-only schemas are occupied"
    );
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert!(absent);
    connection.close().await.unwrap();
}
