//! Provisioning: how a change decided by the control API reaches Postfix, Dovecot and Rspamd.
//! The unprivileged service queues changes in `pending_changes`, in the same transaction as the
//! state they follow from; `norbelys-smtp provision-apply`, a short-lived helper that systemd
//! starts when the trigger file changes (a path unit, with a timer as a backstop), applies them
//! to docker-mailserver's configuration directory and exits. No idle process holds the
//! privilege.
//!
//! What the helper writes, each file published atomically ([`publish`]) and only when its
//! content changes:
//!
//! - `postfix-accounts.cf`: the logins and their Dovecot credentials (`login|{SCRAM-SHA-256}…`).
//!   Lines it does not manage are kept, and a create never overwrites a login it finds there
//!   with another credential: that change is refused and recorded.
//! - `domain-senders.pcre` (Postfix's `smtpd_sender_login_maps`: which logins may use which
//!   envelope senders, the relay logins' VERP return paths `bounce+<token>@<mail host>`
//!   included, [`crate::bounce`]), `sender-domains.map` (the grants Rspamd's sender policy
//!   checks against the MIME From) and `relay-logins.map` (the `whitelisted_user` set of
//!   Rspamd's `ratelimit` module: the logins of the `relay` rate class, the core's own
//!   credentials, carry no per-login bucket, so the product's own ledger is the only limit on
//!   them, while `customer` logins keep Rspamd's backstop).
//! - The managed block of `postfix-virtual.cf`, between `# BEGIN NORBELYS MANAGED` and
//!   `# END NORBELYS MANAGED`: catch-alls, the real mailboxes they must not swallow, and a
//!   digest of the three maps above. This file is written last on purpose: docker-mailserver's
//!   change detector polls it (and `postfix-accounts.cf`) every two seconds, regenerates its
//!   own maps and reloads Postfix and Dovecot, so a changed map reaches every `smtpd` process
//!   within seconds without the helper joining the `docker` group.
//! - `rspamd/dkim/rsa-2048-<selector>-<domain>.private.txt`: a new RSA-2048 key, or an existing
//!   one adopted ([`dkim`]); its public half goes to `domains.dkim_public`. The private key
//!   never enters the database.
//!
//! Order and failure: the helper takes an exclusive lock, applies pending changes in id order
//! in batches, renders the maps from the database's state read in the same transaction as
//! the batch (not from the change payloads, so any number of queued changes collapse into one
//! consistent render), publishes, and only then marks the batch processed in one transaction,
//! removing credentials from the payloads. A change it refuses (a login taken outside the
//! service, an unmanaged catch-all, an unreadable key) is marked processed with its `error` in
//! the payload and logged; the service counts refusals in a metric. An I/O failure leaves the
//! batch pending and exits non-zero; the next run repeats it, and every step is idempotent.
//!
//! Privilege: the helper runs as the service's own user, so host locks and configuration files keep one owner,
//! with `CAP_DAC_OVERRIDE` and `CAP_CHOWN`: it writes into a root-owned directory and gives
//! each file the owner of the file it replaces, or of its directory.

pub mod dkim;
pub mod publish;
pub mod render;

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Instant;

use crate::db::{Connection, OptionalExtension as _, Transaction, params};
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::config::ProvisionArgs;
use crate::crypto;
use crate::db;

/// Changes processed per transaction.
const BATCH: usize = 500;

/// A login's new credential.
#[derive(Debug, Serialize, Deserialize)]
pub struct Credential {
    /// The login.
    pub username: String,
    /// Its Dovecot credential ([`crate::crypto::scram_sha256`]).
    pub credential: String,
}

/// A login.
#[derive(Debug, Serialize, Deserialize)]
pub struct Login {
    /// The login.
    pub username: String,
}

/// A domain's DKIM key.
#[derive(Debug, Serialize, Deserialize)]
pub struct DkimKey {
    /// The domain.
    pub domain: String,
    /// Its selector.
    pub selector: String,
}

/// One queued change.
#[derive(Debug)]
pub enum Change {
    /// Render the maps again from the database.
    Maps,
    /// Add a login.
    AccountCreate(Credential),
    /// Replace (or restore) a login's credential.
    AccountPassword(Credential),
    /// Remove a login; its mail stays.
    AccountDisable(Login),
    /// Create or adopt a domain's DKIM key.
    DkimCreate(DkimKey),
}

impl Change {
    fn kind(&self) -> &'static str {
        match self {
            Self::Maps => "maps",
            Self::AccountCreate(_) => "account.create",
            Self::AccountPassword(_) => "account.password",
            Self::AccountDisable(_) => "account.disable",
            Self::DkimCreate(_) => "dkim.create",
        }
    }

    fn payload(&self) -> serde_json::Result<String> {
        match self {
            Self::Maps => Ok("{}".to_owned()),
            Self::AccountCreate(c) | Self::AccountPassword(c) => serde_json::to_string(c),
            Self::AccountDisable(l) => serde_json::to_string(l),
            Self::DkimCreate(d) => serde_json::to_string(d),
        }
    }

    fn parse(kind: &str, payload: &str) -> Result<Self, String> {
        let invalid = |error: serde_json::Error| format!("unreadable {kind} payload: {error}");
        match kind {
            "maps" => Ok(Self::Maps),
            "account.create" => serde_json::from_str(payload)
                .map(Self::AccountCreate)
                .map_err(invalid),
            "account.password" => serde_json::from_str(payload)
                .map(Self::AccountPassword)
                .map_err(invalid),
            "account.disable" => serde_json::from_str(payload)
                .map(Self::AccountDisable)
                .map_err(invalid),
            "dkim.create" => serde_json::from_str(payload)
                .map(Self::DkimCreate)
                .map_err(invalid),
            other => Err(format!("unknown change kind {other}")),
        }
    }
}

/// Queues `change` inside the caller's transaction.
///
/// # Errors
///
/// The insert fails.
pub fn enqueue(tx: &Transaction<'_>, change: &Change) -> db::Result<()> {
    let payload = change
        .payload()
        .map_err(|error| db::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.execute(
        "INSERT INTO pending_changes (kind, payload, created) VALUES (?1, ?2, ?3)",
        params![change.kind(), payload, db::now()],
    )?;
    Ok(())
}

/// True when a DKIM key for `domain` is queued and not yet processed.
///
/// # Errors
///
/// The query fails.
pub fn dkim_pending(tx: &Transaction<'_>, domain: &str) -> db::Result<bool> {
    tx.query_row(
        "SELECT 1 FROM pending_changes
          WHERE kind = 'dkim.create' AND applied IS NULL AND json_extract(payload, '$.domain') = ?1",
        [domain],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

/// Touches the trigger file, which starts `provision-apply`.
///
/// # Errors
///
/// The file cannot be written.
pub fn trigger(path: &Path) -> io::Result<()> {
    std::fs::write(path, db::now_text())
}

/// What one batch did: each change's outcome, and the DKIM public keys to record by domain.
struct Applied {
    processed: Vec<Processed>,
    keys: Vec<(String, String)>,
}

/// The outcome of one change.
struct Processed {
    id: i64,
    payload: serde_json::Value,
    refused: Option<String>,
}

/// `provision-apply`: applies every pending change, then exits.
///
/// # Errors
///
/// The lock, the database or a file cannot be used; the pending changes stay pending.
pub fn apply(args: &ProvisionArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        crate::config::is_label(&args.common.node),
        "NORBELYS_SMTP_NODE must be a lowercase DNS label"
    );
    let conn = Connection::remote(&args.common.database, &args.common.node, None)
        .context("cannot open the remote database")?;
    apply_connection(args, conn)
}

/// Applies one node's canonical state to the MTA's files under its exclusive host lock.
fn apply_connection(args: &ProvisionArgs, mut conn: Connection) -> anyhow::Result<()> {
    let dms = &args.dms_config_dir;
    anyhow::ensure!(
        dms.is_dir(),
        "{} is not docker-mailserver's configuration directory",
        dms.display()
    );
    let mail_host = args.mail_host.as_deref();
    match mail_host {
        Some(host) => anyhow::ensure!(
            crate::control::is_domain(host),
            "NORBELYS_SMTP_MAIL_HOST must be a lowercase fully qualified host name"
        ),
        None => tracing::warn!(
            "NORBELYS_SMTP_MAIL_HOST is unset: relay logins cannot use VERP return paths, so Postfix refuses those envelope senders"
        ),
    }
    db::create_private_dir(&args.common.state_dir)?;
    let lock = File::create(args.common.state_dir.join("provision.lock"))?;
    lock.lock()
        .context("cannot lock the provisioning lock file")?;
    loop {
        let started = Instant::now();
        // One read transaction: the changes and the state they produced, as of the same commit,
        // so the maps never show a change whose own change is not in this batch yet.
        let (pending, state) = {
            let tx = conn.transaction()?;
            let pending = load_pending(&tx)?;
            let state = render::State::load(&tx)?;
            tx.commit()?;
            (pending, state)
        };
        if pending.is_empty() {
            return Ok(());
        }
        let count = pending.len();
        let Applied { processed, keys } = apply_batch(dms, mail_host, pending, &state)?;
        let refused = processed.iter().filter(|p| p.refused.is_some()).count();
        record(&mut conn, &processed, &keys)?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if refused == 0 {
            crate::telemetry::unit(crate::telemetry::Event::Provision);
            tracing::info!(
                event = "mta.provision",
                changes = count,
                refused,
                duration_ms,
                "mta.provision"
            );
        } else {
            crate::telemetry::unit(crate::telemetry::Event::Provision);
            tracing::error!(
                event = "mta.provision",
                changes = count,
                refused,
                duration_ms,
                "mta.provision"
            );
        }
        if count < BATCH {
            return Ok(());
        }
    }
}

fn load_pending(conn: &Connection) -> db::Result<Vec<(i64, String, String)>> {
    let stmt = conn.prepare(
        "SELECT id, kind, payload FROM pending_changes WHERE applied IS NULL ORDER BY id LIMIT ?1",
    )?;
    let rows = stmt.query_map([i64::try_from(BATCH).unwrap_or(i64::MAX)], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    rows.collect()
}

/// Applies one batch to the files; returns each change's outcome and the DKIM keys recorded.
fn apply_batch(
    dms: &Path,
    mail_host: Option<&str>,
    pending: Vec<(i64, String, String)>,
    state: &render::State,
) -> anyhow::Result<Applied> {
    let accounts_path = dms.join("postfix-accounts.cf");
    let original = read_or_empty(&accounts_path)?;
    let mut accounts = original.clone();
    let mut processed = Vec::with_capacity(pending.len());
    let mut maps = Vec::new();
    let mut keys = Vec::new();

    for (id, kind, payload) in pending {
        let mut outcome = Processed {
            id,
            payload: scrub(&payload),
            refused: None,
        };
        match Change::parse(&kind, &payload) {
            Err(reason) => outcome.refused = Some(reason),
            Ok(Change::Maps) => {
                maps.push(processed.len());
            }
            Ok(Change::AccountCreate(c)) => {
                match render::create_account(&accounts, &c.username, &c.credential) {
                    Ok(next) => accounts = next,
                    Err(reason) => outcome.refused = Some(reason),
                }
            }
            Ok(Change::AccountPassword(c)) => {
                accounts = render::set_account(&accounts, &c.username, &c.credential)
            }
            Ok(Change::AccountDisable(l)) => {
                accounts = render::remove_account(&accounts, &l.username)
            }
            Ok(Change::DkimCreate(d)) => {
                match dkim::ensure(&dms.join("rspamd").join("dkim"), &d.domain, &d.selector) {
                    Ok(public) => keys.push((d.domain, public)),
                    Err(dkim::DkimError::Io(error)) => {
                        return Err(
                            anyhow::Error::from(error).context(format!("DKIM key of {}", d.domain))
                        );
                    }
                    Err(dkim::DkimError::Key(reason)) => outcome.refused = Some(reason),
                }
            }
        }
        processed.push(outcome);
    }

    if accounts != original {
        publish::publish(&accounts_path, accounts.as_bytes(), 0o600)
            .context("postfix-accounts.cf")?;
    }

    // The maps follow the state read with the batch, whatever kinds of change it held.
    let senders = render::domain_senders(&state.grants, mail_host, &state.relays);
    let grants = render::sender_domains(&state.grants);
    let relays = render::relay_logins(&state.relays);
    let digest = crypto::sha256_hex_of(&[senders.as_bytes(), grants.as_bytes(), relays.as_bytes()]);
    let virtual_path = dms.join("postfix-virtual.cf");
    let current = read_or_empty(&virtual_path)?;
    match render::virtual_with_block(
        &current,
        &state.catch_alls,
        &render::logins(&accounts),
        &digest,
    ) {
        Ok(next) => {
            publish::publish(&dms.join("domain-senders.pcre"), senders.as_bytes(), 0o644)
                .context("domain-senders.pcre")?;
            publish::publish(&dms.join("sender-domains.map"), grants.as_bytes(), 0o644)
                .context("sender-domains.map")?;
            publish::publish(&dms.join("relay-logins.map"), relays.as_bytes(), 0o644)
                .context("relay-logins.map")?;
            // Last: its change makes docker-mailserver reload Postfix with the maps above.
            publish::publish(&virtual_path, next.as_bytes(), 0o644)
                .context("postfix-virtual.cf")?;
        }
        Err(reason) => {
            tracing::error!(reason = %reason, "maps not rendered");
            for index in maps {
                if let Some(outcome) = processed.get_mut(index) {
                    outcome.refused = Some(reason.clone());
                }
            }
        }
    }
    Ok(Applied { processed, keys })
}

/// Marks a batch processed and records the DKIM keys, in one transaction.
fn record(
    conn: &mut Connection,
    processed: &[Processed],
    keys: &[(String, String)],
) -> db::Result<()> {
    let tx = conn.transaction()?;
    let now = db::now();
    for outcome in processed {
        let mut payload = outcome.payload.clone();
        if let (Some(reason), Some(object)) = (&outcome.refused, payload.as_object_mut()) {
            tracing::error!(change = outcome.id, reason = %reason, "change refused");
            object.insert(
                "error".to_owned(),
                serde_json::Value::String(reason.clone()),
            );
        }
        tx.execute(
            "UPDATE pending_changes SET applied = ?1, payload = ?2 WHERE id = ?3",
            params![now, payload.to_string(), outcome.id],
        )?;
    }
    for (domain, public) in keys {
        tx.execute(
            "UPDATE domains SET dkim_public = ?1 WHERE name = ?2",
            params![public, domain],
        )?;
    }
    tx.commit()
}

/// The payload without its credential, as it is kept once processed.
fn scrub(payload: &str) -> serde_json::Value {
    let mut value = serde_json::from_str(payload).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = value.as_object_mut() {
        object.remove("credential");
    }
    value
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::{Common, Telemetry};
    use crate::testing::TempDir;

    fn args(dir: &TempDir) -> ProvisionArgs {
        ProvisionArgs {
            common: Common {
                state_dir: dir.join("state"),
                node: "mail-test".to_owned(),
                database: crate::config::DatabaseArgs {
                    database_url: "http://127.0.0.1:1".to_owned(),
                    database_token: secrecy::SecretString::from(String::new()),
                },
                telemetry: Telemetry {
                    trace_sample_percent: 100,
                    otlp_endpoint: None,
                    environment: "test".to_owned(),
                },
            },
            dms_config_dir: dir.join("dms"),
            mail_host: Some("mail.example.com".to_owned()),
        }
    }

    /// A verified domain with a relay login granted its domain and holding its catch-all, and
    /// the changes the control API queued for it.
    fn seed(args: &ProvisionArgs, credential: &str) -> Connection {
        fs::create_dir_all(args.dms_config_dir.join("rspamd/dkim")).unwrap();
        db::create_private_dir(&args.common.state_dir).unwrap();
        let mut conn = db::open(&args.common.database()).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(
            "INSERT INTO domains (name, ownership_token, verified_at, dkim_selector, created_at)
               VALUES ('example.com', 't', 'now', 'norbelys', 'now');
             INSERT INTO accounts (username, domain, kind, rate_class, grant_domain, catch_all, created_at)
               VALUES ('relay@example.com', 'example.com', 'relay', 'relay', 'example.com', 1, 'now');",
        )
        .unwrap();
        let login = "relay@example.com".to_owned();
        for change in [
            Change::AccountCreate(Credential {
                username: login,
                credential: credential.to_owned(),
            }),
            Change::DkimCreate(DkimKey {
                domain: "example.com".to_owned(),
                selector: "norbelys".to_owned(),
            }),
            Change::Maps,
        ] {
            enqueue(&tx, &change).unwrap();
        }
        tx.commit().unwrap();
        conn
    }

    /// The helper applies the queue to docker-mailserver's files: the login and its credential,
    /// the grant and relay maps, the catch-all block (keeping unmanaged lines), the DKIM key
    /// recorded in the database; credentials leave the queue; a second run changes nothing.
    #[test]
    fn applies_the_queue_to_the_mail_server_files() {
        let dir = TempDir::new();
        let args = args(&dir);
        let mut conn = seed(&args, "{SCRAM-SHA-256}c");
        let dms = &args.dms_config_dir;
        fs::write(
            dms.join("postfix-accounts.cf"),
            "owner@example.com|{SHA512-CRYPT}$6$x\n",
        )
        .unwrap();
        fs::write(
            dms.join("postfix-virtual.cf"),
            "support@example.com owner@example.com\n",
        )
        .unwrap();

        apply_connection(&args, db::open(&args.common.database()).unwrap()).unwrap();

        let read = |name: &str| fs::read_to_string(dms.join(name)).unwrap();
        assert_eq!(
            read("postfix-accounts.cf"),
            "owner@example.com|{SHA512-CRYPT}$6$x\nrelay@example.com|{SCRAM-SHA-256}c\n"
        );
        assert_eq!(
            read("sender-domains.map"),
            "relay@example.com example.com\n"
        );
        assert_eq!(read("relay-logins.map"), "relay@example.com\n");
        assert_eq!(
            read("domain-senders.pcre"),
            "/^([^@]+)@example\\.com$/ relay@example.com,${1}@example.com\n\
             /^bounce\\+[0-9a-z]{16,56}@mail\\.example\\.com$/ relay@example.com\n"
        );
        let virtual_map = read("postfix-virtual.cf");
        assert!(
            virtual_map
                .starts_with("support@example.com owner@example.com\n# BEGIN NORBELYS MANAGED\n")
        );
        assert!(
            virtual_map
                .contains("owner@example.com owner@example.com\nrelay@example.com relay@example.com\n@example.com relay@example.com\n")
        );

        let payloads: Vec<String> = conn
            .prepare("SELECT payload FROM pending_changes WHERE applied IS NOT NULL ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<db::Result<_>>()
            .unwrap();
        assert_eq!(payloads.len(), 3);
        assert!(
            payloads
                .iter()
                .all(|p| !p.contains("credential") && !p.contains("error"))
        );
        let public: Option<String> = conn
            .query_row("SELECT dkim_public FROM domains", [], |r| r.get(0))
            .unwrap();
        assert!(public.is_some());

        // Rendering the same state again writes nothing, so docker-mailserver reloads nothing.
        let before = fs::metadata(dms.join("postfix-virtual.cf"))
            .unwrap()
            .modified()
            .unwrap();
        let tx = conn.transaction().unwrap();
        enqueue(&tx, &Change::Maps).unwrap();
        tx.commit().unwrap();
        apply_connection(&args, db::open(&args.common.database()).unwrap()).unwrap();
        assert_eq!(
            fs::metadata(dms.join("postfix-virtual.cf"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        let pending: i64 = conn
            .query_row(
                "SELECT count(*) FROM pending_changes WHERE applied IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
    }

    /// A login already in `postfix-accounts.cf` with another credential is never overwritten:
    /// the change is marked processed with its reason, for an operator to resolve.
    #[test]
    fn refuses_a_login_it_did_not_create() {
        let dir = TempDir::new();
        let args = args(&dir);
        let conn = seed(&args, "{SCRAM-SHA-256}new");
        let accounts = args.dms_config_dir.join("postfix-accounts.cf");
        fs::write(&accounts, "relay@example.com|{SHA512-CRYPT}$6$old\n").unwrap();

        apply_connection(&args, db::open(&args.common.database()).unwrap()).unwrap();

        assert_eq!(
            fs::read_to_string(&accounts).unwrap(),
            "relay@example.com|{SHA512-CRYPT}$6$old\n"
        );
        let refused: String = conn
            .query_row(
                "SELECT json_extract(payload, '$.error') FROM pending_changes WHERE kind = 'account.create'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(refused.contains("another credential"));
    }
}
