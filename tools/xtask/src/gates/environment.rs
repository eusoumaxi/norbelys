//! The world a crash gate runs in, and the steps that act on it: a database of its own created
//! from the authoritative SQLx migrations, the roles as processes with their own logins, the api reached through
//! `curl`, and the database read as its superuser.
//!
//! A role runs `norbelys-server <role>` from the gate's work directory (under the system's
//! temporary directory, so no `.env` is found above it) with nothing of this process's
//! environment but `PATH` and `HOME`: it reads exactly the variables the gate gives it. Its output
//! goes to `<name>.log` in the work directory, beside the local object store the api and the
//! worker share.
//!
//! Dropping an [`Environment`] kills every process it started and drops its database; the work
//! directory is removed when the gate passed and kept for its logs otherwise. `GATES_KEEP=1`
//! keeps both.

use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use sqlx::{AssertSqlSafe, Connection as _, PgConnection, PgPool};
use tokio::runtime::Runtime;

use crate::db::{DEVELOPMENT_PASSWORD, Target};

/// Where the api listens.
const API: &str = "http://127.0.0.1:3961";
/// The api's own address, the one in [`API`].
const API_ADDR: &str = "127.0.0.1:3961";
/// Where each role answers its health probe.
const API_HEALTH: &str = "127.0.0.1:3962";
const WORKER_HEALTH: &str = "127.0.0.1:3963";
const SENDER_HEALTH: &str = "127.0.0.1:3964";
/// How long a role may take to answer its readiness probe after it starts.
const STARTUP: Duration = Duration::from_secs(60);

/// Prints one line of the gate's progress.
pub fn note(line: impl Display) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// `admin`, a superuser URL, pointed at `database`, as `login` with the development password when
/// one is given.
///
/// # Errors
///
/// `admin` is not a URL that takes a user name and a password.
pub fn database_url(admin: &str, login: Option<&str>, database: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(admin).context("the database URL is not a URL")?;
    if let Some(login) = login {
        url.set_username(login)
            .map_err(|()| anyhow!("the database URL takes no user name"))?;
        url.set_password(Some(DEVELOPMENT_PASSWORD))
            .map_err(|()| anyhow!("the database URL takes no password"))?;
    }
    url.set_path(&format!("/{database}"));
    Ok(url.into())
}

/// The status and the body of `curl`'s output, which `-w '\n%{http_code}'` ends with the status
/// on a line of its own.
#[must_use]
pub fn answered(output: &str) -> Option<(u16, &str)> {
    let (body, status) = output.rsplit_once('\n')?;
    Some((status.trim().parse().ok()?, body))
}

/// The body of a request to the api.
pub enum Body<'a> {
    /// No body.
    Empty,
    /// A JSON document.
    Json(&'a Value),
    /// A file's bytes, with their media type.
    File(&'a Path, &'a str),
}

/// A role process the gate started, by the name the gate gave it.
struct Role {
    name: String,
    child: Child,
}

/// One gate's database, work directory, deployment key, workspace and processes.
pub struct Environment {
    /// The server binary.
    binary: PathBuf,
    /// The superuser URL, as configured.
    admin: String,
    /// Its server, checked to be this machine, and its maintenance database.
    target: Target,
    /// The gate's own database.
    database: String,
    /// The roles' logs and the local object store.
    pub work: PathBuf,
    /// The deployment key every role and `admin` share.
    key: String,
    /// The API key of the gate's workspace, once created.
    api_key: String,
    /// Requests sent, for fresh idempotency keys.
    requests: u32,
    roles: Vec<Role>,
    runtime: Runtime,
    /// The superuser's pool on the gate's database, once it exists.
    pool: Option<PgPool>,
    passed: bool,
}

impl Environment {
    /// Builds the server, creates the gate's database `norbelys_gates_<name>_<pid>` on the local
    /// explicitly disposable local cluster `TEST_DATABASE_URL` names, applies SQLx migrations
    /// directly through maintenance tooling and makes a deployment key.
    ///
    /// # Errors
    ///
    /// `curl` is missing, the URL is missing or names another machine, the server does not build,
    /// or the database refuses the database or the schema.
    pub fn new(root: &Path, name: &str) -> anyhow::Result<Self> {
        Command::new("curl")
            .arg("--version")
            .output()
            .context("the gates call the api with curl, which is not installed")?;
        let admin = crate::db::test_url()?;
        let target = Target::parse(&admin).map_err(|refusal| anyhow!(refusal))?;
        // `cargo xtask` runs under Cargo, which names itself in `CARGO`.
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let built = Command::new(cargo)
            .args(["build", "--locked", "-q", "-p", "norbelys-server"])
            .env("SQLX_OFFLINE", "true")
            .current_dir(root)
            .status()
            .context("cannot run cargo build")?;
        if !built.success() {
            bail!("the server does not build");
        }
        // The server is built into the same directory as this program.
        let binary = std::env::current_exe()?.with_file_name("norbelys-server");
        let database = format!("norbelys_gates_{name}_{}", std::process::id());
        let work =
            std::env::temp_dir().join(format!("norbelys-gates-{name}-{}", std::process::id()));
        fs::create_dir_all(work.join("objects"))
            .with_context(|| format!("cannot create {}", work.display()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut environment = Self {
            binary,
            admin,
            target,
            database,
            work,
            key: String::new(),
            api_key: String::new(),
            requests: 0,
            roles: Vec::new(),
            runtime,
            pool: None,
            passed: false,
        };
        environment.create_database()?;
        environment.migrate()?;
        environment.key = environment.deployment_key()?;
        let url = database_url(&environment.admin, None, &environment.database)?;
        environment.pool = Some(
            environment
                .runtime
                .block_on(PgPoolOptions::new().max_connections(2).connect(&url))
                .context("cannot connect to the gate's database")?,
        );
        note(format!(
            "database {}, work directory {}",
            environment.database,
            environment.work.display()
        ));
        Ok(environment)
    }

    /// Creates the gate's database, empty, through the server's maintenance database.
    fn create_database(&self) -> anyhow::Result<()> {
        let database = &self.database;
        self.runtime.block_on(async {
            let mut connection = PgConnection::connect(&self.target.maintenance)
                .await
                .with_context(|| format!("cannot connect to {}", self.target.server))?;
            // Audited: the name is our own lowercase identifier, quoted.
            sqlx::raw_sql(AssertSqlSafe(format!(r#"CREATE DATABASE "{database}""#)))
                .execute(&mut connection)
                .await
                .with_context(|| format!("cannot create {database}"))?;
            connection.close().await?;
            anyhow::Ok(())
        })
    }

    /// `norbelys-server` from the work directory, with nothing of this environment but `PATH`
    /// and `HOME`.
    fn server(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.current_dir(&self.work).env_clear();
        for variable in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }
        command
    }

    /// A new log file in the work directory.
    fn log(&self, name: &str) -> anyhow::Result<File> {
        let path = self.work.join(format!("{name}.log"));
        File::create(&path).with_context(|| format!("cannot create {}", path.display()))
    }

    /// Creates the schema in the gate's database, as the superuser.
    fn migrate(&self) -> anyhow::Result<()> {
        let url = database_url(&self.admin, None, &self.database)?;
        self.runtime.block_on(crate::db::prepare(&url))
    }

    /// A fresh deployment key, from the binary, the one place keys are made.
    fn deployment_key(&self) -> anyhow::Result<String> {
        let output = self
            .server()
            .args(["admin", "deployment-key"])
            .env("RUST_LOG", "off")
            .output()
            .context("cannot run norbelys-server admin deployment-key")?;
        let printed = String::from_utf8_lossy(&output.stdout);
        let key = printed
            .lines()
            .next_back()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if !output.status.success() || key.is_empty() {
            bail!("admin deployment-key printed no key");
        }
        Ok(key)
    }

    /// Starts `norbelys-server <role>` as `name`, with the deployment key, private mail hosts
    /// allowed (the gate's SMTP server is on the loopback interface), the local object store and
    /// `variables`.
    fn start(
        &mut self,
        name: &str,
        role: &str,
        variables: &[(&str, String)],
    ) -> anyhow::Result<()> {
        let log = self.log(name)?;
        let mut command = self.server();
        command
            .arg(role)
            .env("RUST_LOG", "info")
            .env("NORBELYS_DEPLOYMENT_KEY", &self.key)
            .env("MAIL_ALLOW_PRIVATE_HOSTS", "true")
            .env(
                "OBJECT_STORE_URL",
                format!("file://{}", self.work.join("objects").display()),
            )
            .stdout(log.try_clone()?)
            .stderr(log);
        for (variable, value) in variables {
            command.env(variable, value);
        }
        let child = command
            .spawn()
            .with_context(|| format!("cannot start the {role} role"))?;
        self.roles.push(Role {
            name: name.to_owned(),
            child,
        });
        Ok(())
    }

    /// Waits until the health probe at `address` answers, failing as soon as the role `name`
    /// exits.
    fn ready(&mut self, name: &str, address: &str) -> anyhow::Result<()> {
        let url = format!("http://{address}/health/ready");
        let deadline = Instant::now() + STARTUP;
        loop {
            let up = Command::new("curl")
                .args(["-fsS", "-o", "/dev/null", "--max-time", "2", url.as_str()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if up {
                note(format!("started: {name}"));
                return Ok(());
            }
            if let Some(role) = self.roles.iter_mut().find(|role| role.name == name)
                && let Some(status) = role.child.try_wait()?
            {
                bail!(
                    "{name} exited as it started ({status}): see {}/{name}.log",
                    self.work.display()
                );
            }
            if Instant::now() > deadline {
                bail!(
                    "{name} was not ready within {STARTUP:?}: see {}/{name}.log",
                    self.work.display()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Starts the api as `norbelys_app` and waits until it is ready.
    pub fn start_api(&mut self) -> anyhow::Result<()> {
        let database = database_url(&self.admin, Some("norbelys_app"), &self.database)?;
        self.start(
            "api",
            "api",
            &[
                ("DATABASE_URL", database),
                ("HTTP_ADDR", API_ADDR.to_owned()),
                ("PUBLIC_API_URL", API.to_owned()),
                ("HEALTH_ADDR", API_HEALTH.to_owned()),
            ],
        )?;
        self.ready("api", API_ADDR)
    }

    /// Starts a worker as `name` (`norbelys_worker`, its system pool as `norbelys_system`) and
    /// waits until it is ready. A worker started again after [`Self::kill`] takes the health port
    /// of the one it replaces, which `kill` has waited to see gone.
    pub fn start_worker(&mut self, name: &str) -> anyhow::Result<()> {
        let database = database_url(&self.admin, Some("norbelys_worker"), &self.database)?;
        let system = database_url(&self.admin, Some("norbelys_system"), &self.database)?;
        self.start(
            name,
            "worker",
            &[
                ("DATABASE_URL", database),
                ("SYSTEM_DATABASE_URL", system),
                ("HEALTH_ADDR", WORKER_HEALTH.to_owned()),
            ],
        )?;
        self.ready(name, WORKER_HEALTH)
    }

    /// Starts a sender as `name` (`norbelys_worker`) and waits until it is ready; a sender started
    /// again takes the health port of the one it replaces, as a worker does.
    pub fn start_sender(&mut self, name: &str) -> anyhow::Result<()> {
        let database = database_url(&self.admin, Some("norbelys_worker"), &self.database)?;
        self.start(
            name,
            "sender",
            &[
                ("DATABASE_URL", database),
                ("HEALTH_ADDR", SENDER_HEALTH.to_owned()),
            ],
        )?;
        self.ready(name, SENDER_HEALTH)
    }

    /// Kills the role `name` with SIGKILL, as a crash or an out-of-memory kill would, and waits
    /// until it is gone.
    ///
    /// # Errors
    ///
    /// No role of that name runs, or the signal could not be sent.
    pub fn kill(&mut self, name: &str) -> anyhow::Result<()> {
        let index = self
            .roles
            .iter()
            .position(|role| role.name == name)
            .with_context(|| format!("no role named {name} runs"))?;
        let mut role = self.roles.swap_remove(index);
        role.child.kill()?;
        role.child.wait()?;
        note(format!("killed: {name}"));
        Ok(())
    }

    /// Creates the live workspace `slug` with its owner through `admin create-workspace`, as
    /// `norbelys_system`; the gate's requests then carry its API key.
    ///
    /// # Errors
    ///
    /// The command failed or printed no key.
    pub fn workspace(&mut self, slug: &str) -> anyhow::Result<()> {
        let owner = format!("owner@{slug}.example");
        let output = self
            .server()
            .args([
                "admin",
                "create-workspace",
                "--slug",
                slug,
                "--name",
                slug,
                "--owner-email",
                owner.as_str(),
            ])
            .env("RUST_LOG", "off")
            .env("NORBELYS_DEPLOYMENT_KEY", &self.key)
            .env(
                "DATABASE_URL",
                database_url(&self.admin, Some("norbelys_system"), &self.database)?,
            )
            .output()
            .context("cannot run norbelys-server admin create-workspace")?;
        if !output.status.success() {
            bail!(
                "admin create-workspace failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let created: Value = serde_json::from_slice(&output.stdout)
            .context("admin create-workspace printed no JSON")?;
        self.api_key = created
            .get("api_key_secret")
            .and_then(Value::as_str)
            .context("admin create-workspace printed no API key")?
            .to_owned();
        Ok(())
    }

    /// Sends `method path` to the api through `curl`, with the workspace's key and a fresh
    /// idempotency key; answers the JSON body of a status below 400.
    ///
    /// # Errors
    ///
    /// `curl` failed, or the api answered 400 or more, or no JSON.
    pub fn api(&mut self, method: &str, path: &str, body: Body<'_>) -> anyhow::Result<Value> {
        self.requests = self.requests.saturating_add(1);
        let mut command = Command::new("curl");
        command
            .args(["-sS", "-X", method, "-w", "\n%{http_code}", "-H"])
            .arg(format!("Authorization: Bearer {}", self.api_key))
            .arg("-H")
            .arg(format!(
                "Idempotency-Key: gates-{}-{}",
                std::process::id(),
                self.requests
            ));
        match body {
            Body::Empty => {}
            Body::Json(json) => {
                command
                    .args(["-H", "Content-Type: application/json", "--data-binary"])
                    .arg(json.to_string());
            }
            Body::File(file, media_type) => {
                command
                    .arg("-H")
                    .arg(format!("Content-Type: {media_type}"))
                    .arg("--data-binary")
                    .arg(format!("@{}", file.display()));
            }
        }
        let output = command
            .arg(format!("{API}{path}"))
            .output()
            .context("cannot run curl")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let (status, answer) = answered(&text).with_context(|| {
            format!(
                "{method} {path}: no answer ({})",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })?;
        if status >= 400 {
            bail!("{method} {path} answered {status}: {answer}");
        }
        serde_json::from_str(answer)
            .with_context(|| format!("{method} {path} answered {status} without JSON"))
    }

    /// The superuser's pool on the gate's database.
    ///
    /// # Errors
    ///
    /// The database is not open yet.
    pub fn pool(&self) -> anyhow::Result<&PgPool> {
        self.pool
            .as_ref()
            .context("the gate's database is not open")
    }

    /// Runs `future` (a read or a write of the gate's database) to its end.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Runs a statement of the gate's own on its database as the superuser, with `id` as `$1`.
    ///
    /// # Errors
    ///
    /// The database refused it.
    pub fn execute(&self, statement: &'static str, id: Uuid) -> anyhow::Result<()> {
        let pool = self.pool()?;
        self.block_on(sqlx::query(statement).bind(id).execute(pool))
            .with_context(|| statement.to_owned())?;
        Ok(())
    }

    /// Polls `query`, a boolean about `id` (`$1`), every 200 ms until it is true; false when
    /// `within` passes first.
    ///
    /// # Errors
    ///
    /// The database refused the query.
    pub fn until(&self, within: Duration, query: &'static str, id: Uuid) -> anyhow::Result<bool> {
        let pool = self.pool()?;
        let deadline = Instant::now() + within;
        loop {
            let now: Option<Option<bool>> = self
                .block_on(sqlx::query_scalar(query).bind(id).fetch_optional(pool))
                .with_context(|| query.to_owned())?;
            if now.flatten() == Some(true) {
                return Ok(true);
            }
            if Instant::now() > deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Wakes the claim loops of `queue` (`imports`, `webhooks`, …), as the `NOTIFY` that follows
    /// an enqueue does, on the server's wake-up channel `norbelys_work`.
    ///
    /// # Errors
    ///
    /// The database refused the notification.
    pub fn wake(&self, queue: &str) -> anyhow::Result<()> {
        let pool = self.pool()?;
        self.block_on(
            sqlx::query("SELECT pg_notify('norbelys_work', $1)")
                .bind(queue)
                .execute(pool),
        )
        .context("cannot send the wake-up")?;
        Ok(())
    }

    /// Records that every assertion of the gate held: its work directory goes with it.
    pub fn pass(&mut self) {
        self.passed = true;
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for role in &mut self.roles {
            let _ = role.child.kill();
            let _ = role.child.wait();
        }
        if let Some(pool) = self.pool.take() {
            self.runtime.block_on(pool.close());
        }
        if std::env::var("GATES_KEEP").is_ok_and(|keep| keep == "1") {
            note(format!(
                "kept: database {}, work directory {}",
                self.database,
                self.work.display()
            ));
            return;
        }
        let database = &self.database;
        let maintenance = &self.target.maintenance;
        let dropped = self.runtime.block_on(async {
            let mut connection = PgConnection::connect(maintenance).await?;
            // Audited: the name is our own lowercase identifier, quoted.
            sqlx::raw_sql(AssertSqlSafe(format!(
                r#"DROP DATABASE IF EXISTS "{database}" WITH (FORCE)"#
            )))
            .execute(&mut connection)
            .await?;
            connection.close().await
        });
        if let Err(error) = dropped {
            note(format!("the database {database} was not dropped: {error}"));
        }
        if self.passed {
            let _ = fs::remove_dir_all(&self.work);
        } else {
            note(format!("kept for its logs: {}", self.work.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{answered, database_url};

    /// A role's URL is the superuser's, on the gate's database, as the role's login with the
    /// development password, its server and options kept; without a login it stays the
    /// superuser's, so `migrate` and the assertions reach the same database the roles use.
    #[test]
    fn a_role_reaches_the_gates_database_with_its_own_login() {
        let admin = "postgres://norbelys:secret@127.0.0.1:5433/norbelys_dev?sslmode=disable";
        assert_eq!(
            database_url(admin, Some("norbelys_app"), "norbelys_gates_sender_7").unwrap(),
            "postgres://norbelys_app:norbelys@127.0.0.1:5433/norbelys_gates_sender_7?sslmode=disable"
        );
        assert_eq!(
            database_url(admin, None, "norbelys_gates_sender_7").unwrap(),
            "postgres://norbelys:secret@127.0.0.1:5433/norbelys_gates_sender_7?sslmode=disable"
        );
        assert!(database_url("not a url", None, "x").is_err());
    }

    /// `curl`'s output splits into the body and the status it wrote on the last line, an empty
    /// body included; output without a status line is no answer.
    #[test]
    fn curl_output_splits_into_body_and_status() {
        assert_eq!(
            answered("{\"id\": \"imp_1\"}\n202"),
            Some((202, "{\"id\": \"imp_1\"}"))
        );
        assert_eq!(answered("\n204"), Some((204, "")));
        assert_eq!(answered("{\"a\":\n1}\n422"), Some((422, "{\"a\":\n1}")));
        assert_eq!(answered("no status"), None);
        assert_eq!(answered("body\nnot a status"), None);
    }
}
