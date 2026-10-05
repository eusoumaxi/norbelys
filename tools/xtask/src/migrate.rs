//! Manually invoked SQLx maintenance, executed outside the deployed application.
//! Provisioning cluster roles and applying database migrations are separate operations.
//! No application dependency, legacy-schema adoption, SQL splitting or custom ledger exists here.

use std::str::FromStr as _;

use anyhow::{Context as _, ensure};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{AssertSqlSafe, ConnectOptions as _, Connection as _, PgConnection, migrate::Migrator};

pub static MIGRATIONS: Migrator = sqlx::migrate!("../../crates/server/migrations");
pub const PROVISION: &str = include_str!("../../../crates/server/provision.sql");
pub const LOGINS: [(&str, &str); 5] = [
    ("norbelys_app", "APP_DATABASE_PASSWORD"),
    ("norbelys_worker", "WORKER_DATABASE_PASSWORD"),
    ("norbelys_tracking", "TRACKING_DATABASE_PASSWORD"),
    ("norbelys_system", "SYSTEM_DATABASE_PASSWORD"),
    ("norbelys_metrics", "METRICS_DATABASE_PASSWORD"),
];

/// Explicit operator input; no fallback to a role's DATABASE_URL or an ambient .env file.
/// Remote database connections require verified TLS. Local development keeps its own guard.
pub fn run(check_only: bool, provision: bool) -> anyhow::Result<()> {
    let raw = std::env::var("MIGRATION_DATABASE_URL")
        .context("set MIGRATION_DATABASE_URL explicitly in the external maintenance environment")?;
    let mut options =
        PgConnectOptions::from_str(&raw).context("invalid maintenance connection options")?;
    let local = crate::db::Target::parse(&raw).is_ok();
    if !local {
        options = options.ssl_mode(PgSslMode::VerifyFull);
    }
    if provision {
        options = options.database("postgres");
    }
    let passwords = if provision {
        LOGINS
            .iter()
            .filter_map(|(role, variable)| {
                std::env::var(variable)
                    .ok()
                    .map(|password| (*role, password))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    ensure!(
        passwords.iter().all(|(_, password)| !password.is_empty()),
        "role passwords must not be empty"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut connection = options
            .disable_statement_logging()
            .connect()
            .await
            .context("cannot connect to the maintenance target")?;
        let result = if provision {
            provision_roles(&mut connection, &passwords).await
        } else if check_only {
            check(&mut connection, &MIGRATIONS).await
        } else {
            apply(&mut connection, &MIGRATIONS).await
        };
        // SQLx's session advisory lock is released even when migration validation fails.
        let closed = connection.close().await;
        result?;
        closed?;
        anyhow::Ok(())
    })
}

/// Provision roles only with a cluster administrator connection to `postgres`.
/// One transaction and a cluster-maintenance database lock serialize concurrent setup.
pub async fn provision_roles(
    connection: &mut PgConnection,
    passwords: &[(&str, String)],
) -> anyhow::Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *connection)
        .await?;
    ensure!(
        database == "postgres",
        "cluster provisioning must connect to the postgres maintenance database"
    );
    let mut tx = connection.begin().await?;
    sqlx::raw_sql(PROVISION).execute(&mut *tx).await?;
    for (role, password) in passwords {
        ensure!(
            LOGINS.iter().any(|(allowed, _)| allowed == role),
            "unrecognized runtime login"
        );
        // PostgreSQL quotes both identifier and literal. Passwords never become command-line
        // arguments; statement logging is disabled for this explicit provisioning connection.
        let statement: String =
            sqlx::query_scalar("SELECT format('ALTER ROLE %I PASSWORD %L', $1::text, $2::text)")
                .bind(role)
                .bind(password)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::raw_sql(AssertSqlSafe(statement))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Refuse unrelated or previous internal schemas before SQLx creates any history table.
/// An empty retry after a rolled-back first migration may already have an empty SQLx ledger.
pub async fn apply(connection: &mut PgConnection, migrations: &Migrator) -> anyhow::Result<()> {
    let (ledger, occupied): (bool, bool) = sqlx::query_as(
        "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL,
         EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname = 'public' AND c.relname <> '_sqlx_migrations'
                  AND c.relkind IN ('r','p','v','m','f','S'))
         OR EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                   WHERE n.nspname = 'public' AND NOT EXISTS(
                     SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass
                       AND d.objid = p.oid AND d.deptype = 'e'))
         OR EXISTS(SELECT 1 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                   WHERE n.nspname = 'public' AND t.typtype IN ('e','d'))",
    )
    .fetch_one(&mut *connection)
    .await?;
    ensure!(
        ledger || !occupied,
        "unrecognized existing schema; no adoption or reset is performed"
    );
    if ledger && occupied {
        let initialized: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public._sqlx_migrations)")
                .fetch_one(&mut *connection)
                .await?;
        ensure!(
            initialized,
            "unrecognized schema with empty migration history; no adoption is performed"
        );
    }
    migrations
        .run(connection)
        .await
        .context("SQLx migration failed")?;
    Ok(())
}

/// Read-only release readiness. Expected checksums come from SQLx; the SQL contract is shared
/// with the server's independent schema probe. Pending, changed and unknown versions all fail.
pub async fn check(connection: &mut PgConnection, migrations: &Migrator) -> anyhow::Result<()> {
    let mut tx = connection.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    ensure!(present, "database migrations are pending");
    let versions: Vec<i64> = migrations.iter().map(|m| m.version).collect();
    let checksums: Vec<Vec<u8>> = migrations.iter().map(|m| m.checksum.to_vec()).collect();
    let matches: bool = sqlx::query_scalar(include_str!("../../../crates/server/schema-check.sql"))
        .bind(versions)
        .bind(checksums)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        matches,
        "database migrations are pending or incompatible with this release"
    );
    tx.rollback().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
