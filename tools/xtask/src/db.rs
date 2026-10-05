//! Local-only database reset and setup. Ordinary tests never call reset.
//! `db migrate` uses SQLx directly and preserves existing data; cluster role passwords
//! are development-only. External manual maintenance has a separate explicit command.

use std::io::Write as _;
use std::net::IpAddr;

use anyhow::{Context as _, anyhow};
use sqlx::{AssertSqlSafe, Connection as _, PgConnection};
use url::{Host, Url};

/// The development password of the login roles, the one the test harness logs in with.
pub const DEVELOPMENT_PASSWORD: &str = "norbelys";
/// The server's own databases, never reset.
const RESERVED: [&str; 3] = ["postgres", "template0", "template1"];

/// What a reset acts on, decided from `DATABASE_URL` before anything is touched.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// The database to drop and create.
    pub database: String,
    /// The same server and login on its `postgres` database, from which the other is dropped.
    pub maintenance: String,
    /// The server, for messages: `host:port`, or the socket directory.
    pub server: String,
}

impl Target {
    /// Decides the target of a reset from a database URL, refusing what is not a local
    /// development database.
    ///
    /// # Errors
    ///
    /// A message saying why the URL is refused.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let url = Url::parse(raw).map_err(|error| format!("DATABASE_URL is not a URL: {error}"))?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err("DATABASE_URL is not a postgres:// URL".to_owned());
        }
        // SQLx accepts repeated host/dbname options; checking only the first would validate
        // a different target from the one eventually connected to.
        if url.fragment().is_some()
            || url
                .query_pairs()
                .any(|(key, _)| !matches!(key.as_ref(), "host" | "sslmode"))
            || url.query_pairs().filter(|(key, _)| key == "host").count() > 1
        {
            return Err("local database URLs accept only one host override and sslmode".to_owned());
        }
        let socket = url
            .query_pairs()
            .find(|(key, _)| key == "host")
            .map(|(_, value)| value.into_owned());
        let server = match (&socket, url.host()) {
            (Some(directory), _) if directory.starts_with('/') => directory.clone(),
            (Some(host), _) if is_local(host) => host.clone(),
            (Some(host), _) => return Err(refuse_remote(host)),
            (None, Some(host)) if is_local_host(&host) => {
                format!("{host}:{}", url.port().unwrap_or(5432))
            }
            (None, Some(host)) => return Err(refuse_remote(&host.to_string())),
            (None, None) => return Err("DATABASE_URL names no server".to_owned()),
        };
        let database = url.path().trim_start_matches('/').to_owned();
        if !is_identifier(&database) {
            return Err(format!(
                "`{database}` is not a database name this command resets: lowercase letters, \
                 digits and underscores, starting with a letter or an underscore"
            ));
        }
        if RESERVED.contains(&database.as_str()) {
            return Err(format!(
                "`{database}` is one of the server's own databases and is never reset"
            ));
        }
        let mut maintenance = url;
        maintenance.set_path("/postgres");
        Ok(Self {
            database,
            maintenance: maintenance.into(),
            server,
        })
    }
}

/// The refusal for a server that is not on this machine.
fn refuse_remote(host: &str) -> String {
    format!(
        "DATABASE_URL names the server `{host}`, which is not on this machine: `db reset` drops \
         databases, so it only acts on the loopback interface or a Unix socket"
    )
}

/// Whether a `host` query value names this machine.
fn is_local(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether a URL's host names this machine. `postgres://` is not one of the schemes whose
/// addresses the URL standard parses, so `127.0.0.1` arrives as a name and is parsed here.
fn is_local_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => is_local(domain),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// Whether `name` is a lowercase SQL identifier that needs no escaping inside double quotes.
fn is_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && name.len() <= 63
}

/// Reads the local development target, loading `.env` just as the server does.
fn target() -> anyhow::Result<Target> {
    let _ = dotenvy::dotenv();
    let raw = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is not set, in the environment or in a .env file")?;
    Target::parse(&raw).map_err(|refusal| anyhow!(refusal))
}

/// Resets the local database `DATABASE_URL` names to an empty database.
///
/// # Errors
///
/// `DATABASE_URL` is missing or refused, or the server refuses a statement (the login may not
/// create databases, another session holds a lock).
pub fn reset() -> anyhow::Result<()> {
    let target = target()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(apply(&target))?;
    writeln!(
        std::io::stdout(),
        "{} on {}: dropped and created empty",
        target.database,
        target.server
    )?;
    Ok(())
}

/// Provisions local roles and applies the authoritative SQLx sequence, without resetting data.
/// Development passwords are assigned during explicit cluster provisioning.
///
/// # Errors
/// The target is not local, role provisioning or migration fails, or passwords cannot be set.
pub fn migrate() -> anyhow::Result<()> {
    let target = target()?;
    let raw = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(prepare(&raw))?;
    writeln!(
        std::io::stdout(),
        "{} on {}: schema ready; local login passwords configured",
        target.database,
        target.server
    )?;
    Ok(())
}

/// Shared local fixture setup for development and crash gates. The target guard remains at
/// this boundary even when a caller already validated its own database name.
pub async fn prepare(raw: &str) -> anyhow::Result<()> {
    let target = Target::parse(raw).map_err(|refusal| anyhow!(refusal))?;
    let mut maintenance = PgConnection::connect(&target.maintenance).await?;
    let passwords = crate::migrate::LOGINS
        .iter()
        .map(|(role, _)| (*role, DEVELOPMENT_PASSWORD.to_owned()))
        .collect::<Vec<_>>();
    crate::migrate::provision_roles(&mut maintenance, &passwords).await?;
    maintenance.close().await?;
    let mut connection = PgConnection::connect(raw).await?;
    let result = crate::migrate::apply(&mut connection, &crate::migrate::MIGRATIONS).await;
    let closed = connection.close().await;
    result?;
    closed?;
    Ok(())
}

/// Acknowledgement applies to the whole cluster: tests create logins and disposable databases.
pub fn test_url() -> anyhow::Result<String> {
    anyhow::ensure!(
        std::env::var("NORBELYS_TEST_DATABASE_DISPOSABLE").as_deref() == Ok("1"),
        "set NORBELYS_TEST_DATABASE_DISPOSABLE=1 only for a dedicated disposable cluster"
    );
    let raw = std::env::var("TEST_DATABASE_URL").context("set TEST_DATABASE_URL explicitly")?;
    Target::parse(&raw).map_err(|refusal| anyhow!(refusal))?;
    Ok(raw)
}

/// Drops and creates the local database.
async fn apply(target: &Target) -> anyhow::Result<()> {
    let mut conn = PgConnection::connect(&target.maintenance)
        .await
        .with_context(|| {
            format!(
                "cannot connect to the `postgres` database on {}",
                target.server
            )
        })?;
    let database = &target.database;
    // Audited: the name is a lowercase identifier (checked by `Target::parse`), quoted.
    sqlx::raw_sql(AssertSqlSafe(format!(
        r#"DROP DATABASE IF EXISTS "{database}" WITH (FORCE)"#
    )))
    .execute(&mut conn)
    .await
    .with_context(|| format!("cannot drop {database}"))?;
    sqlx::raw_sql(AssertSqlSafe(format!(r#"CREATE DATABASE "{database}""#)))
        .execute(&mut conn)
        .await
        .with_context(|| format!("cannot create {database}"))?;
    conn.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Target;

    /// A local URL resets its database through the same server's `postgres` database, keeping
    /// the login, the port and the options, so the reset acts on exactly what `migrate` will use.
    #[test]
    fn a_local_url_resets_its_own_database() {
        let target =
            Target::parse("postgres://norbelys:secret@127.0.0.1:5433/norbelys_dev?sslmode=disable")
                .unwrap();
        assert_eq!(
            target,
            Target {
                database: "norbelys_dev".to_owned(),
                maintenance: "postgres://norbelys:secret@127.0.0.1:5433/postgres?sslmode=disable"
                    .to_owned(),
                server: "127.0.0.1:5433".to_owned(),
            }
        );
        for local in [
            "postgresql://u@localhost/db_1",
            "postgres://u@[::1]:5432/db",
            "postgres://u@127.0.0.2/db",
            "postgres:///db?host=/var/run/postgresql",
            "postgres://ignored.example/db?host=localhost",
        ] {
            assert!(Target::parse(local).is_ok(), "{local} was refused");
        }
    }

    /// Anything that is not a local development database is refused before a connection is
    /// made: another machine (as a host or as a `host` option), another scheme, a name that
    /// would need escaping, and the server's own databases.
    #[test]
    fn anything_else_is_refused() {
        for refused in [
            "postgres://u@db.example.com/norbelys",
            "postgres://u@10.0.0.5:5432/norbelys",
            "postgres://u@127.0.0.1/norbelys?host=db.example.com",
            "postgres://u@127.0.0.1/norbelys?host=localhost&host=db.example.com",
            "postgres://u@127.0.0.1/norbelys?dbname=postgres",
            "postgres://u@127.0.0.1/norbelys?hostaddr=10.0.0.1",
            "mysql://u@127.0.0.1/norbelys",
            "postgres://u@127.0.0.1/",
            "postgres://u@127.0.0.1/Norbelys",
            "postgres://u@127.0.0.1/norbelys-dev",
            "postgres://u@127.0.0.1/postgres",
            "postgres://u@127.0.0.1/template1",
            "not a url",
        ] {
            assert!(Target::parse(refused).is_err(), "{refused} was accepted");
        }
    }
}
