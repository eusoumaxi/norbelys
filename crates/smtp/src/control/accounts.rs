//! Accounts: the MTA's logins, used for SMTP submission and IMAP.
//!
//! A login is an address on a verified domain. Its password is generated here, answered once in
//! the creation (or reset) response, and kept nowhere: the queued change carries only its
//! Dovecot credential until the provisioning helper writes it into `postfix-accounts.cf`, and
//! the credential is removed from the change once applied.
//!
//! A grant lets a login send as any address of its own domain (the sender policy also checks
//! that the MIME From matches the envelope). A catch-all receives mail for the domain's unknown
//! addresses; a domain has one at most. The rate class decides the limits: `customer` logins
//! keep Rspamd's per-login backstop (a small burst per minute and a daily cap, set above any
//! normal day so the product's own ledger is what users meet); `relay` logins, the core's own
//! sending credentials, carry no per-login bucket, so the product's ledger is their only limit.
//! Disabling a login removes it from Dovecot (its mail stays) and clears its grant and
//! catch-all; a password reset enables it again.

use crate::db::{Connection, OptionalExtension as _, Row, Transaction, params};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State as Extract};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::{ApiError, List, State, parse, split_address, trigger};
use crate::crypto;
use crate::db;
use crate::provision::{self, Change, Credential, Login};

/// What a login is for.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A mailbox a person reads.
    Mailbox,
    /// A service mailbox (a shared domain's catch-all, replies and DSNs).
    Service,
    /// A sending credential of the core.
    Relay,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mailbox => "mailbox",
            Self::Service => "service",
            Self::Relay => "relay",
        }
    }
}

/// Which limits a login carries.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateClass {
    /// Rspamd's per-login backstop applies.
    Customer,
    /// No per-login bucket: the product's ledger is the limit.
    Relay,
}

impl RateClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Customer => "customer",
            Self::Relay => "relay",
        }
    }
}

/// `POST /v1/accounts`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAccount {
    /// The login, a lowercase address on a verified domain.
    pub username: String,
    /// What the login is for.
    pub kind: Kind,
    /// Which limits it carries.
    pub rate_class: RateClass,
}

/// `PATCH /v1/accounts/{username}`: either field may be omitted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAccount {
    /// Send as any address of the login's own domain.
    pub grant: Option<bool>,
    /// Receive the domain's unknown addresses.
    pub catch_all: Option<bool>,
}

/// An account as stored.
#[derive(Debug, Serialize)]
pub struct Account {
    /// The login.
    pub username: String,
    /// Its domain.
    pub domain: String,
    /// `mailbox`, `service` or `relay`.
    pub kind: String,
    /// `customer` or `relay`.
    pub rate_class: String,
    /// The domain it may send as, when granted.
    pub grant_domain: Option<String>,
    /// Whether it is its domain's catch-all.
    pub catch_all: bool,
    /// The route its evidence is posted to.
    pub route: Option<String>,
    /// When it was created.
    pub created_at: String,
    /// When it was disabled.
    pub disabled_at: Option<String>,
}

/// An account with its new password, answered once.
#[derive(Debug, Serialize)]
pub struct Issued {
    /// The account.
    #[serde(flatten)]
    pub account: Account,
    /// The password; the service keeps no copy.
    pub password: String,
    /// Submission settings.
    pub smtp: Endpoint,
    /// Mailbox settings.
    pub imap: Endpoint,
}

/// How a client reaches the MTA.
#[derive(Debug, Serialize)]
pub struct Endpoint {
    /// The MTA's public host name.
    pub host: String,
    /// The port.
    pub port: u16,
    /// `starttls` or `tls`.
    pub security: &'static str,
    /// The login.
    pub username: String,
}

const SELECT: &str =
    "SELECT a.username, a.domain, a.kind, a.rate_class, a.grant_domain, a.catch_all, a.created_at,
                             a.disabled_at, r.provider_webhook_id
                        FROM accounts a LEFT JOIN account_routes r ON r.username = a.username";

fn from_row(row: &Row) -> db::Result<Account> {
    Ok(Account {
        username: row.get(0)?,
        domain: row.get(1)?,
        kind: row.get(2)?,
        rate_class: row.get(3)?,
        grant_domain: row.get(4)?,
        catch_all: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        disabled_at: row.get(7)?,
        route: row.get(8)?,
    })
}

fn load(conn: &Connection, username: &str) -> db::Result<Option<Account>> {
    conn.query_row(
        &format!("{SELECT} WHERE a.username = ?1"),
        [username],
        from_row,
    )
    .optional()
}

fn existing(tx: &Transaction<'_>, username: &str) -> Result<Account, ApiError> {
    load(tx, username)?.ok_or_else(|| ApiError::NotFound(format!("no account {username}")))
}

fn issued(state: &State, account: Account, password: String) -> Issued {
    let host = state.settings.mail_host.clone();
    Issued {
        smtp: Endpoint {
            host: host.clone(),
            port: 587,
            security: "starttls",
            username: account.username.clone(),
        },
        imap: Endpoint {
            host,
            port: 993,
            security: "tls",
            username: account.username.clone(),
        },
        account,
        password,
    }
}

/// A fresh password and its credential, computed on the blocking pool by the caller.
fn new_password() -> Result<(String, String), ApiError> {
    let password = crypto::random_token(32)?;
    let credential = crypto::scram_sha256(&password)?;
    Ok((password, credential))
}

/// `POST /v1/accounts`: creates a login on a verified domain; `201` with its password.
///
/// # Errors
///
/// The username is invalid (`422`), its domain is not registered or not verified, or the login
/// exists (`409`).
pub async fn create(
    Extract(state): Extract<State>,
    body: Bytes,
) -> Result<(StatusCode, Json<Issued>), ApiError> {
    let input: CreateAccount = parse(&body)?;
    let Some((_, domain)) = split_address(&input.username) else {
        return Err(ApiError::Invalid(
            "username must be a lowercase address".to_owned(),
        ));
    };
    let domain = domain.to_owned();
    let (account, password) = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let verified: Option<Option<String>> = tx
                .query_row("SELECT verified_at FROM domains WHERE name = ?1", [&domain], |row| row.get(0))
                .optional()?;
            match verified {
                None => return Err(ApiError::Conflict(format!("the domain {domain} is not registered"))),
                Some(None) => return Err(ApiError::Conflict(format!("the domain {domain} is not verified yet"))),
                Some(Some(_)) => {}
            }
            if load(&tx, &input.username)?.is_some() {
                return Err(ApiError::Conflict(format!("the account {} exists", input.username)));
            }
            let (password, credential) = new_password()?;
            tx.execute(
                "INSERT INTO accounts (username, domain, kind, rate_class, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![input.username, domain, input.kind.as_str(), input.rate_class.as_str(), db::now_text()],
            )?;
            provision::enqueue(
                &tx,
                &Change::AccountCreate(Credential {
                    username: input.username.clone(),
                    credential,
                }),
            )?;
            provision::enqueue(&tx, &Change::Maps)?;
            let account = existing(&tx, &input.username)?;
            tx.commit()?;
            Ok((account, password))
        })
        .await?;
    trigger(&state);
    Ok((StatusCode::CREATED, Json(issued(&state, account, password))))
}

/// `GET /v1/accounts`.
///
/// # Errors
///
/// The database fails.
pub async fn list(Extract(state): Extract<State>) -> Result<Json<List<Account>>, ApiError> {
    let data = state
        .db
        .call(|conn| {
            let stmt = conn.prepare(&format!("{SELECT} ORDER BY a.username"))?;
            let rows = stmt.query_map([], from_row)?;
            rows.collect::<db::Result<Vec<_>>>().map_err(ApiError::from)
        })
        .await?;
    Ok(Json(List { data }))
}

/// `GET /v1/accounts/{username}`.
///
/// # Errors
///
/// No such account.
pub async fn retrieve(
    Extract(state): Extract<State>,
    Path(username): Path<String>,
) -> Result<Json<Account>, ApiError> {
    let account = state
        .db
        .call(move |conn| {
            load(conn, &username)?
                .ok_or_else(|| ApiError::NotFound(format!("no account {username}")))
        })
        .await?;
    Ok(Json(account))
}

/// `PATCH /v1/accounts/{username}`: sets `grant` and `catch_all`.
///
/// # Errors
///
/// No such account (`404`), the body changes nothing (`422`), the account is disabled or its
/// domain has another catch-all (`409`).
pub async fn update(
    Extract(state): Extract<State>,
    Path(username): Path<String>,
    body: Bytes,
) -> Result<Json<Account>, ApiError> {
    let input: UpdateAccount = parse(&body)?;
    if input.grant.is_none() && input.catch_all.is_none() {
        return Err(ApiError::Invalid("set grant, catch_all or both".to_owned()));
    }
    let account = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let account = existing(&tx, &username)?;
            if account.disabled_at.is_some() {
                return Err(ApiError::Conflict(format!("the account {username} is disabled")));
            }
            if let Some(grant) = input.grant {
                tx.execute(
                    "UPDATE accounts SET grant_domain = CASE WHEN ?2 THEN domain END WHERE username = ?1",
                    params![username, grant],
                )?;
            }
            if let Some(catch_all) = input.catch_all {
                if catch_all {
                    let other: Option<String> = tx
                        .query_row(
                            "SELECT username FROM accounts
                              WHERE domain = ?1 AND catch_all = 1 AND disabled_at IS NULL AND username <> ?2",
                            params![account.domain, username],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(other) = other {
                        return Err(ApiError::Conflict(format!("{other} is already the catch-all of {}", account.domain)));
                    }
                }
                tx.execute(
                    "UPDATE accounts SET catch_all = ?2 WHERE username = ?1",
                    params![username, i64::from(catch_all)],
                )?;
            }
            provision::enqueue(&tx, &Change::Maps)?;
            let account = existing(&tx, &username)?;
            tx.commit()?;
            Ok(account)
        })
        .await?;
    trigger(&state);
    Ok(Json(account))
}

/// `DELETE /v1/accounts/{username}`: disables the login, its grant and its catch-all; its mail
/// and its route stay. Disabling a disabled account changes nothing.
///
/// # Errors
///
/// No such account.
pub async fn disable(
    Extract(state): Extract<State>,
    Path(username): Path<String>,
) -> Result<Json<Account>, ApiError> {
    let (account, changed) = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            let account = existing(&tx, &username)?;
            if account.disabled_at.is_some() {
                return Ok((account, false));
            }
            tx.execute(
                "UPDATE accounts SET disabled_at = ?2, grant_domain = NULL, catch_all = 0 WHERE username = ?1",
                params![username, db::now_text()],
            )?;
            provision::enqueue(
                &tx,
                &Change::AccountDisable(Login {
                    username: username.clone(),
                }),
            )?;
            provision::enqueue(&tx, &Change::Maps)?;
            let account = existing(&tx, &username)?;
            tx.commit()?;
            Ok::<_, ApiError>((account, true))
        })
        .await?;
    if changed {
        trigger(&state);
    }
    Ok(Json(account))
}

/// `POST /v1/accounts/{username}/password`: a new password, answered once; a disabled login is
/// enabled again (without its former grant or catch-all).
///
/// # Errors
///
/// No such account.
pub async fn reset_password(
    Extract(state): Extract<State>,
    Path(username): Path<String>,
) -> Result<Json<Issued>, ApiError> {
    let (account, password) = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            existing(&tx, &username)?;
            let (password, credential) = new_password()?;
            tx.execute(
                "UPDATE accounts SET disabled_at = NULL WHERE username = ?1",
                [&username],
            )?;
            provision::enqueue(
                &tx,
                &Change::AccountPassword(Credential {
                    username: username.clone(),
                    credential,
                }),
            )?;
            provision::enqueue(&tx, &Change::Maps)?;
            let account = existing(&tx, &username)?;
            tx.commit()?;
            Ok::<_, ApiError>((account, password))
        })
        .await?;
    trigger(&state);
    Ok(Json(issued(&state, account, password)))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::*;
    use crate::testing::{TempDir, call, router, signed, verify_domain};

    const MAILBOX: &str =
        r#"{"username":"a@example.com","kind":"mailbox","rate_class":"customer"}"#;

    async fn verified(dir: &TempDir) -> (axum::Router, State) {
        let (app, state) = router(dir);
        call(
            &app,
            signed("POST", "/v1/domains", r#"{"name":"example.com"}"#),
        )
        .await;
        verify_domain(dir, "example.com");
        (app, state)
    }

    async fn changes(state: &State, kind: &'static str) -> Vec<String> {
        state
            .db
            .call(move |conn| {
                let stmt = conn
                    .prepare("SELECT payload FROM pending_changes WHERE kind = ?1 ORDER BY id")?;
                let rows = stmt.query_map([kind], |r| r.get(0))?;
                rows.collect::<db::Result<Vec<String>>>()
                    .map_err(ApiError::from)
            })
            .await
            .unwrap()
    }

    /// A login exists only on a registered, verified domain: the ownership check gates every
    /// account, so nobody sends as a domain they do not control.
    #[tokio::test]
    async fn creation_requires_a_verified_domain() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        assert_eq!(
            call(&app, signed("POST", "/v1/accounts", MAILBOX)).await.0,
            StatusCode::CONFLICT
        );
        call(
            &app,
            signed("POST", "/v1/domains", r#"{"name":"example.com"}"#),
        )
        .await;
        assert_eq!(
            call(&app, signed("POST", "/v1/accounts", MAILBOX)).await.0,
            StatusCode::CONFLICT
        );
    }

    /// The password is answered once and stored nowhere: the queued change carries only the
    /// Dovecot credential of that very password, and a repeated creation is refused rather than
    /// issuing a second password for the same login.
    #[tokio::test]
    async fn creation_answers_the_password_once_and_queues_its_credential() {
        let dir = TempDir::new();
        let (app, state) = verified(&dir).await;
        let (status, issued) = call(&app, signed("POST", "/v1/accounts", MAILBOX)).await;
        assert_eq!(status, StatusCode::CREATED);
        let password = issued["password"].as_str().unwrap().to_owned();
        assert_eq!(issued["smtp"]["port"], 587);

        let queued = changes(&state, "account.create").await;
        assert_eq!(queued.len(), 1);
        let change: Credential = serde_json::from_str(&queued[0]).unwrap();
        let salt = STANDARD
            .decode(change.credential.split(',').nth(1).unwrap())
            .unwrap();
        assert_eq!(
            change.credential,
            crypto::scram_sha256_salted(&password, &salt)
        );
        assert!(!queued[0].contains(&password));

        assert_eq!(
            call(&app, signed("POST", "/v1/accounts", MAILBOX)).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(changes(&state, "account.create").await.len(), 1);
    }

    /// A grant lets a login send as any address of its own domain, and is revoked the same way;
    /// an update that changes nothing is refused.
    #[tokio::test]
    async fn grants_follow_the_logins_own_domain() {
        let dir = TempDir::new();
        let (app, _) = verified(&dir).await;
        call(&app, signed("POST", "/v1/accounts", MAILBOX)).await;
        let path = "/v1/accounts/a@example.com";
        let (_, granted) = call(&app, signed("PATCH", path, r#"{"grant":true}"#)).await;
        let (_, revoked) = call(&app, signed("PATCH", path, r#"{"grant":false}"#)).await;
        assert_eq!(
            (
                granted["grant_domain"].as_str(),
                revoked["grant_domain"].as_str()
            ),
            (Some("example.com"), None)
        );
        assert_eq!(
            call(&app, signed("PATCH", path, "{}")).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// A domain has one catch-all: a second one is refused while the first holds it, so mail
    /// for unknown addresses has one destination.
    #[tokio::test]
    async fn one_catch_all_per_domain() {
        let dir = TempDir::new();
        let (app, _) = verified(&dir).await;
        call(&app, signed("POST", "/v1/accounts", MAILBOX)).await;
        call(
            &app,
            signed("POST", "/v1/accounts", &MAILBOX.replace("a@", "b@")),
        )
        .await;
        let catch_all = r#"{"catch_all":true}"#;
        assert_eq!(
            call(
                &app,
                signed("PATCH", "/v1/accounts/a@example.com", catch_all)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                signed("PATCH", "/v1/accounts/b@example.com", catch_all)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }

    /// Disabling queues the login's removal once and clears its grant and catch-all; a password
    /// reset enables it again with a new password, which is how the core recovers a credential
    /// it lost.
    #[tokio::test]
    async fn disable_then_reset_restores_the_login() {
        let dir = TempDir::new();
        let (app, state) = verified(&dir).await;
        let (_, issued) = call(&app, signed("POST", "/v1/accounts", MAILBOX)).await;
        let path = "/v1/accounts/a@example.com";
        call(
            &app,
            signed("PATCH", path, r#"{"grant":true,"catch_all":true}"#),
        )
        .await;
        for _ in 0..2 {
            let (status, disabled) = call(&app, signed("DELETE", path, "")).await;
            assert_eq!(status, StatusCode::OK);
            assert!(disabled["disabled_at"].is_string());
            assert_eq!(
                (
                    disabled["grant_domain"].is_null(),
                    disabled["catch_all"].as_bool()
                ),
                (true, Some(false))
            );
        }
        assert_eq!(changes(&state, "account.disable").await.len(), 1);
        assert_eq!(
            call(&app, signed("PATCH", path, r#"{"grant":true}"#))
                .await
                .0,
            StatusCode::CONFLICT
        );

        let (status, reset) = call(&app, signed("POST", &format!("{path}/password"), "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(reset["disabled_at"].is_null());
        assert_ne!(reset["password"], issued["password"]);
        assert_eq!(changes(&state, "account.password").await.len(), 1);
    }
}
