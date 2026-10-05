//! The database: one connection pool per role, and the tenancy context every transaction
//! sets first.
//!
//! **One login per role.** Each role connects with its own PostgreSQL login (`norbelys_app`
//! for the API, `norbelys_worker` for the background roles, `norbelys_tracking`,
//! `norbelys_system`), and each login is granted only what that role does. A bug in one
//! role therefore cannot read or write what the role was never meant to touch.
//!
//! **Tenancy by row security.** Every table that belongs to a workspace has PostgreSQL row
//! security enabled and forced, with policies that admit only the rows whose `workspace_id`
//! equals the transaction's `norbelys.workspace_id` setting
//! (<https://www.postgresql.org/docs/current/ddl-rowsecurity.html>). A transaction that
//! sets no workspace sees no tenant rows. [`Database::begin_in`] is how a tenant
//! transaction starts: it sets the workspace with `set_config(…, true)`, which lasts only
//! until the end of the transaction, so a pooled connection never carries one tenant's
//! setting into the next. This module is the only place those settings are written.
//!
//! **The scheduler's view.** Background claims need to see due work across workspaces
//! before they know which workspace a row belongs to. The worker login may switch, inside
//! one transaction, to the `norbelys_scheduler` role ([`as_scheduler`]), which sees only the
//! routing and lease columns of every workspace; it then switches back ([`reset_role`]) and
//! sets the claimed row's workspace for the tenant work. Row locks taken under the
//! scheduler role survive the switch.
//!
//! **Transactions.** A use case begins and commits its transaction; the functions it calls
//! take `&mut Tx` and never commit, so the order of writes and the commit point are visible
//! in one function. A transaction that must end sooner than its login's own bound tightens it
//! with [`set_transaction_timeout`], the one place a transaction changes its own bound.

#[cfg(test)]
mod pruning_tests;
pub(crate) mod schema;
#[cfg(test)]
mod schema_tests;
mod types;

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use secrecy::{ExposeSecret as _, SecretString};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions as _, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::domain::ids::{Id, User, WorkspaceId};

/// A transaction of this crate.
pub type Tx = Transaction<'static, Postgres>;

static POOL_WAIT: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_db_pool_wait_seconds")
        .with_unit("s")
        .with_description("How long a transaction waited for a connection of its pool, by pool.")
        .with_boundaries(vec![
            0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 2.5,
        ])
        .build()
});

/// Reports, whenever the metrics are collected, how many of `pool`'s connections are in use,
/// as `norbelys_db_pool_in_use{pool}` under the pool's application name.
fn observe_pool(pool: &PgPool, name: &'static str) {
    let pool = pool.clone();
    let _ = opentelemetry::global::meter("norbelys")
        .u64_observable_gauge("norbelys_db_pool_in_use")
        .with_description("Connections of the pool in use, by pool.")
        .with_callback(move |observer| {
            let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
            observer.observe(
                u64::from(pool.size().saturating_sub(idle)),
                &[KeyValue::new("pool", name)],
            );
        })
        .build();
}

/// How a role's pool is sized and bounded.
#[derive(Debug, Clone, Copy)]
pub struct PoolSettings {
    /// The `application_name` PostgreSQL shows for these connections.
    pub application_name: &'static str,
    /// Connections in the pool; small and fixed per role, so the database's connection
    /// budget is the sum of the replicas' pools.
    pub max_connections: u32,
    /// `statement_timeout` for every statement of the pool.
    pub statement_timeout: Duration,
    /// How long opening or waiting for a connection may take in the pool itself.
    pub acquire_timeout: Duration,
    /// Connections opened at start and kept open.
    pub min_connections: u32,
    /// The tighter wait for a free connection on a request path. Past it the caller fails
    /// early with `503 service_unavailable` instead of queueing behind a saturated pool:
    /// a client can retry against another replica, while a queue only adds latency to
    /// every request behind it.
    pub request_deadline: Option<Duration>,
}

/// A role's connection pool.
#[derive(Clone, Debug)]
pub struct Database {
    pool: PgPool,
    request_deadline: Option<Duration>,
    /// The pool's application name, its label in the pool metrics.
    name: &'static str,
}

impl Database {
    /// Connects with the role's own login.
    ///
    /// # Errors
    ///
    /// The URL is invalid or the database cannot be reached.
    pub async fn connect(url: &SecretString, settings: PoolSettings) -> Result<Self, sqlx::Error> {
        let statement_timeout = format!("{}ms", settings.statement_timeout.as_millis());
        let options: PgConnectOptions = url
            .expose_secret()
            .parse::<PgConnectOptions>()?
            .log_statements(tracing::log::LevelFilter::Off)
            .log_slow_statements(tracing::log::LevelFilter::Off, Duration::ZERO)
            .application_name(settings.application_name)
            .options([("statement_timeout", statement_timeout.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .min_connections(settings.min_connections.min(settings.max_connections))
            .acquire_timeout(settings.acquire_timeout)
            .connect_with(options)
            .await?;
        observe_pool(&pool, settings.application_name);
        Ok(Self {
            pool,
            request_deadline: settings.request_deadline,
            name: settings.application_name,
        })
    }

    /// Like [`Database::connect`], but no connection is opened until a statement needs one: for a
    /// role that must start, and answer, while the database is unavailable (the tracking role,
    /// whose drain waits for the database while its routes keep answering).
    ///
    /// # Errors
    ///
    /// The URL is invalid.
    pub fn connect_lazy(url: &SecretString, settings: PoolSettings) -> Result<Self, sqlx::Error> {
        let statement_timeout = format!("{}ms", settings.statement_timeout.as_millis());
        let options: PgConnectOptions = url
            .expose_secret()
            .parse::<PgConnectOptions>()?
            .log_statements(tracing::log::LevelFilter::Off)
            .log_slow_statements(tracing::log::LevelFilter::Off, Duration::ZERO)
            .application_name(settings.application_name)
            .options([("statement_timeout", statement_timeout.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .min_connections(settings.min_connections.min(settings.max_connections))
            .acquire_timeout(settings.acquire_timeout)
            .connect_lazy_with(options);
        observe_pool(&pool, settings.application_name);
        Ok(Self {
            pool,
            request_deadline: settings.request_deadline,
            name: settings.application_name,
        })
    }

    /// Begins a transaction, waiting at most the request deadline for a connection when this pool
    /// has one. Only the acquisition is bounded: cutting `BEGIN` itself short could hand the
    /// connection back to the pool inside a transaction nobody ends, which PostgreSQL closes after
    /// its idle-in-transaction timeout, so the next request that draws it fails.
    #[tracing::instrument(name = "db.acquire", skip_all, fields(db.system.name = "postgresql", db.operation.name = "BEGIN", norbelys.db.pool = self.name))]
    async fn begin_bounded(&self) -> Result<Tx, sqlx::Error> {
        let started = std::time::Instant::now();
        let connection = match self.request_deadline {
            Some(deadline) => tokio::time::timeout(deadline, self.pool.acquire())
                .await
                .map_err(|_| sqlx::Error::PoolTimedOut)
                .and_then(|acquired| acquired),
            None => self.pool.acquire().await,
        };
        // Every wait counts, the ones that ended without a connection too: they are the
        // saturation the pool metrics exist to show.
        POOL_WAIT.record(
            started.elapsed().as_secs_f64(),
            &[KeyValue::new("pool", self.name)],
        );
        Transaction::begin(connection?, None).await
    }

    /// The pool, for reads that need no transaction and no tenant (health, lookups owned by
    /// `norbelys_lookup`).
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// True when the database answers within the pool's bounds.
    pub async fn ping(&self) -> bool {
        let ok = sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .is_ok();
        if !ok {
            opentelemetry::global::meter("norbelys")
                .u64_counter("norbelys_health_dependency_failures_total")
                .build()
                .add(1, &[KeyValue::new("pool", self.name)]);
        }
        ok
    }

    /// Begins a transaction in `workspace`: row security admits only its rows.
    ///
    /// # Errors
    ///
    /// The database is unavailable.
    pub async fn begin_in(&self, workspace: WorkspaceId) -> Result<Tx, sqlx::Error> {
        let mut tx = self.begin_bounded().await?;
        set_workspace(&mut tx, workspace).await?;
        Ok(tx)
    }

    /// Begins a transaction as `user`, before any workspace is chosen (the user policies).
    ///
    /// # Errors
    ///
    /// The database is unavailable.
    pub async fn begin_as_user(&self, user: Id<User>) -> Result<Tx, sqlx::Error> {
        let mut tx = self.begin_bounded().await?;
        set_user(&mut tx, user.uuid()).await?;
        Ok(tx)
    }

    /// Begins a transaction without a tenant: only the lookups and the system role see rows.
    ///
    /// # Errors
    ///
    /// The database is unavailable.
    pub async fn begin(&self) -> Result<Tx, sqlx::Error> {
        self.begin_bounded().await
    }
}

/// Sets the transaction's workspace; every statement after it sees only that workspace's
/// rows. Used at the start of a tenant transaction, and after `RESET ROLE` in a claim.
///
/// # Errors
///
/// The database is unavailable.
pub async fn set_workspace(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('norbelys.workspace_id', $1, true)")
        .bind(workspace.uuid().to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Sets the transaction's user, for the user-scoped policies.
///
/// # Errors
///
/// The database is unavailable.
pub async fn set_user(tx: &mut Tx, user: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('norbelys.user_id', $1, true)")
        .bind(user.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Tightens the transaction's bound: PostgreSQL ends the session (and so the transaction, which
/// releases its locks) once the transaction is still open `timeout` after this call. For a short
/// transaction whose locks other paths wait on, called right after it begins.
///
/// Each login already runs under its own `transaction_timeout` (60 seconds for the worker), and
/// PostgreSQL ignores a new value while that timer runs: a plain `SET LOCAL` would change nothing.
/// The setting therefore goes through `0`, which stops the running timer, and then to `timeout`,
/// which starts a new one from now. Both last until the end of the transaction only. Never pass
/// more than the login's own bound: the rollup's watermark counts on no transaction of a login
/// that writes increments outliving it.
///
/// # Errors
///
/// The database is unavailable.
pub async fn set_transaction_timeout(tx: &mut Tx, timeout: Duration) -> Result<(), sqlx::Error> {
    for value in ["0".to_owned(), format!("{}ms", timeout.as_millis())] {
        sqlx::query("SELECT set_config('transaction_timeout', $1, true)")
            .bind(value)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Switches the transaction to the scheduler's global routing view; only the worker login
/// may do this, because only it is a member of `norbelys_scheduler`. Row locks taken as the
/// scheduler survive [`reset_role`].
///
/// # Errors
///
/// The login is not a member of `norbelys_scheduler`, or the database is unavailable.
pub async fn as_scheduler(tx: &mut Tx) -> Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL ROLE norbelys_scheduler")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Switches the transaction to the schema's owner, for the maintenance only the owner may run
/// (`ANALYZE` of the partitioned parents); only the system login may do this, because only it is a
/// member of `norbelys_owner`. Return with [`reset_role`] before anything else in the transaction.
///
/// # Errors
///
/// The login is not a member of `norbelys_owner`, or the database is unavailable.
pub async fn as_owner(tx: &mut Tx) -> Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL ROLE norbelys_owner")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Returns from the scheduler's or the owner's view to the login role, inside the same transaction.
///
/// # Errors
///
/// The database is unavailable.
pub async fn reset_role(tx: &mut Tx) -> Result<(), sqlx::Error> {
    sqlx::query("RESET ROLE").execute(&mut **tx).await?;
    Ok(())
}

/// Switches the transaction to `workspace` and returns the workspace it was in (`None` when it
/// had none), for work that must write another workspace's rows inside the caller's transaction:
/// the platform's transactional mail, which belongs to the `system` workspace, is accepted inside
/// the sign-in or invitation transaction that asks for it. Pair it with [`restore_workspace`].
///
/// # Errors
///
/// The database is unavailable.
pub async fn switch_workspace(
    tx: &mut Tx,
    workspace: WorkspaceId,
) -> Result<Option<WorkspaceId>, sqlx::Error> {
    let previous: Option<String> =
        sqlx::query_scalar("SELECT current_setting('norbelys.workspace_id', true)")
            .fetch_one(&mut **tx)
            .await?;
    set_workspace(tx, workspace).await?;
    Ok(previous
        .and_then(|value| Uuid::parse_str(&value).ok())
        .map(WorkspaceId::trusted))
}

/// Puts the transaction back in the workspace [`switch_workspace`] returned, or in none.
///
/// # Errors
///
/// The database is unavailable.
pub async fn restore_workspace(
    tx: &mut Tx,
    previous: Option<WorkspaceId>,
) -> Result<(), sqlx::Error> {
    match previous {
        Some(workspace) => set_workspace(tx, workspace).await,
        None => {
            sqlx::query("SELECT set_config('norbelys.workspace_id', '', true)")
                .execute(&mut **tx)
                .await?;
            Ok(())
        }
    }
}
