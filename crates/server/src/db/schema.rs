//! Read-only verification of externally prepared SQLx migration history.
//! The application embeds expected metadata but never applies migrations at runtime.

use anyhow::ensure;
use sqlx::{Connection as _, PgConnection, migrate::Migrator};

pub(crate) static MIGRATIONS: Migrator = sqlx::migrate!("./migrations");

/// Requires exactly the release's successful versions and SQLx checksums. No ledger is
/// created, and the explicit read-only transaction also protects against accidental writes.
pub(crate) async fn check(connection: &mut PgConnection) -> anyhow::Result<()> {
    let mut tx = connection.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    ensure!(
        present,
        "database is not prepared; run external maintenance migrations before starting this release"
    );
    let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
    let checksums: Vec<Vec<u8>> = MIGRATIONS.iter().map(|m| m.checksum.to_vec()).collect();
    let matches: bool = sqlx::query_scalar(include_str!("../../schema-check.sql"))
        .bind(versions)
        .bind(checksums)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        matches,
        "database migration history does not match this release; use external maintenance tooling"
    );
    tx.rollback().await?;
    Ok(())
}
