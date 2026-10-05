//! Routes: where each login's evidence is posted. The core creates one provider webhook per
//! connection and registers it here, under the webhook's id, with the Standard Webhooks secret
//! the core generated; the outbox ([`crate::events`]) signs every batch for the route with it.
//!
//! A route posts only to `https://<an allowed evidence host>/webhooks/<its id>` on the default
//! port, without credentials, query or fragment: the URL is checked against the configured
//! host list, so a compromised or mistaken caller cannot point evidence elsewhere. A login
//! reports to one route at most, and moving it to another route is refused until the first
//! route drops it, so evidence never changes tenant by accident. The tail pins each event to
//! the login's route when it reads the line, so a later change of route never redirects older
//! evidence; deleting a route turns its pending events into dead letters.

use std::collections::{BTreeMap, HashSet};

use crate::db::{Connection, OptionalExtension as _, params};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State as Extract};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{ApiError, List, State, parse, split_address};
use crate::crypto;
use crate::db;

/// The most logins one route carries.
const MAX_USERNAMES: usize = 1000;

/// `PUT /v1/routes/{provider_webhook_id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutRoute {
    /// `https://<evidence host>/webhooks/<provider_webhook_id>`.
    pub url: String,
    /// The provider webhook's Standard Webhooks secret (`whsec_…`).
    pub secret: String,
    /// The logins whose evidence this route receives: exactly these from now on.
    pub usernames: Vec<String>,
}

/// A route, without its secret.
#[derive(Debug, Serialize)]
pub struct Route {
    /// The core's provider webhook id.
    pub id: String,
    /// Where batches are posted.
    pub url: String,
    /// The logins it receives evidence for.
    pub usernames: Vec<String>,
    /// When it was first registered.
    pub created_at: String,
}

fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn evidence_url(raw: &str, id: &str, hosts: &[String]) -> Result<Url, ApiError> {
    let invalid = || {
        ApiError::Invalid(format!(
            "url must be https://<an evidence host>/webhooks/{id}"
        ))
    };
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let allowed = url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| hosts.iter().any(|allowed| allowed == host))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == format!("/webhooks/{id}");
    if allowed { Ok(url) } else { Err(invalid()) }
}

fn load(conn: &Connection, id: &str) -> db::Result<Option<Route>> {
    let route = conn
        .query_row(
            "SELECT url, created_at FROM routes WHERE provider_webhook_id = ?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((url, created_at)) = route else {
        return Ok(None);
    };
    let stmt = conn.prepare(
        "SELECT username FROM account_routes WHERE provider_webhook_id = ?1 ORDER BY username",
    )?;
    let usernames = stmt
        .query_map([id], |row| row.get(0))?
        .collect::<db::Result<_>>()?;
    Ok(Some(Route {
        id: id.to_owned(),
        url,
        usernames,
        created_at,
    }))
}

/// `PUT /v1/routes/{id}`: registers or replaces a route and the logins it receives.
///
/// # Errors
///
/// The id, URL, secret or usernames are invalid, a login is unknown (`422`), or a login is
/// disabled or reports to another route (`409`).
pub async fn upsert(
    Extract(state): Extract<State>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Route>, ApiError> {
    if !valid_id(&id) {
        return Err(ApiError::Invalid(
            "a route id is 1 to 128 letters, digits, `_` or `-`".to_owned(),
        ));
    }
    let input: PutRoute = parse(&body)?;
    let url = evidence_url(&input.url, &id, &state.settings.evidence_hosts)?;
    let secret = crypto::decode_secret(&input.secret).map_err(|_| {
        ApiError::Invalid("secret must be `whsec_` followed by base64 of 24 to 64 bytes".to_owned())
    })?;
    let unique: HashSet<&str> = input.usernames.iter().map(String::as_str).collect();
    if !(1..=MAX_USERNAMES).contains(&input.usernames.len())
        || unique.len() != input.usernames.len()
        || input.usernames.iter().any(|u| split_address(u).is_none())
    {
        return Err(ApiError::Invalid(format!(
            "usernames must be 1 to {MAX_USERNAMES} distinct lowercase addresses"
        )));
    }
    let sealed = state.keys.seal(&secret, id.as_bytes())?;
    let route = state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            for username in &input.usernames {
                let disabled: Option<Option<String>> = tx
                    .query_row("SELECT disabled_at FROM accounts WHERE username = ?1", [username], |row| row.get(0))
                    .optional()?;
                match disabled {
                    None => return Err(ApiError::Invalid(format!("no account {username}"))),
                    Some(Some(_)) => return Err(ApiError::Conflict(format!("the account {username} is disabled"))),
                    Some(None) => {}
                }
                let other: Option<String> = tx
                    .query_row(
                        "SELECT provider_webhook_id FROM account_routes WHERE username = ?1 AND provider_webhook_id <> ?2",
                        params![username, id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(other) = other {
                    return Err(ApiError::Conflict(format!("{username} reports to the route {other}; remove it there first")));
                }
            }
            tx.execute(
                "INSERT INTO routes (provider_webhook_id, url, secret, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (provider_webhook_id) DO UPDATE SET url = excluded.url, secret = excluded.secret",
                params![id, url.as_str(), sealed, db::now_text()],
            )?;
            tx.execute("DELETE FROM account_routes WHERE provider_webhook_id = ?1", [&id])?;
            for username in &input.usernames {
                tx.execute(
                    "INSERT INTO account_routes (username, provider_webhook_id) VALUES (?1, ?2)",
                    params![username, id],
                )?;
            }
            let route = load(&tx, &id)?.ok_or_else(|| ApiError::Internal("a registered route vanished".to_owned()))?;
            tx.commit()?;
            Ok(route)
        })
        .await?;
    Ok(Json(route))
}

/// `GET /v1/routes`.
///
/// # Errors
///
/// The database fails.
pub async fn list(Extract(state): Extract<State>) -> Result<Json<List<Route>>, ApiError> {
    let data = state
        .db
        .call(|conn| {
            let mut usernames: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let stmt = conn.prepare("SELECT provider_webhook_id, username FROM account_routes ORDER BY username")?;
            for row in stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
                let (id, username) = row?;
                usernames.entry(id).or_default().push(username);
            }
            let stmt = conn.prepare("SELECT provider_webhook_id, url, created_at FROM routes ORDER BY provider_webhook_id")?;
            let routes = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?)))?
                .map(|row| {
                    row.map(|(id, url, created_at)| Route {
                        usernames: usernames.remove(&id).unwrap_or_default(),
                        id,
                        url,
                        created_at,
                    })
                })
                .collect::<db::Result<Vec<_>>>()?;
            Ok::<_, ApiError>(routes)
        })
        .await?;
    Ok(Json(List { data }))
}

/// `DELETE /v1/routes/{id}`: the route and its logins' bindings; its pending events become dead
/// letters when they are next due.
///
/// # Errors
///
/// No such route.
pub async fn remove(
    Extract(state): Extract<State>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .db
        .call(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM account_routes WHERE provider_webhook_id = ?1",
                [&id],
            )?;
            if tx.execute("DELETE FROM routes WHERE provider_webhook_id = ?1", [&id])? == 0 {
                return Err(ApiError::NotFound(format!("no route {id}")));
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{SECRET, TempDir, call, router, signed, verify_domain};

    fn put(id: &str, usernames: &str) -> axum::http::Request<axum::body::Body> {
        let body = format!(
            r#"{{"url":"https://localhost/webhooks/{id}","secret":"{SECRET}","usernames":{usernames}}}"#
        );
        signed("PUT", &format!("/v1/routes/{id}"), &body)
    }

    async fn with_login(dir: &TempDir) -> (axum::Router, State) {
        let (app, state) = router(dir);
        call(
            &app,
            signed("POST", "/v1/domains", r#"{"name":"example.com"}"#),
        )
        .await;
        verify_domain(dir, "example.com");
        let account = r#"{"username":"a@example.com","kind":"relay","rate_class":"relay"}"#;
        call(&app, signed("POST", "/v1/accounts", account)).await;
        (app, state)
    }

    /// Evidence goes only to `https://<an allowed host>/webhooks/<the route's id>` on the
    /// default port, without credentials, query or fragment: a mistaken or compromised caller
    /// cannot point evidence anywhere else.
    #[test]
    fn evidence_urls_are_pinned() {
        let hosts = vec!["api.example.com".to_owned()];
        assert!(evidence_url("https://api.example.com/webhooks/pwh_1", "pwh_1", &hosts).is_ok());
        assert!(
            evidence_url(
                "https://api.example.com:443/webhooks/pwh_1",
                "pwh_1",
                &hosts
            )
            .is_ok()
        );
        for refused in [
            "http://api.example.com/webhooks/pwh_1",
            "https://other.example.com/webhooks/pwh_1",
            "https://api.example.com:8443/webhooks/pwh_1",
            "https://user:pass@api.example.com/webhooks/pwh_1",
            "https://api.example.com/webhooks/pwh_1?x=1",
            "https://api.example.com/webhooks/pwh_1#x",
            "https://api.example.com/webhooks/pwh_2",
            "not a url",
        ] {
            assert!(evidence_url(refused, "pwh_1", &hosts).is_err(), "{refused}");
        }
    }

    /// A login reports to one route: claiming it for a second route is refused until the first
    /// releases it, so evidence never moves to another tenant by accident.
    #[tokio::test]
    async fn a_login_reports_to_one_route() {
        let dir = TempDir::new();
        let (app, _) = with_login(&dir).await;
        let login = r#"["a@example.com"]"#;
        assert_eq!(call(&app, put("pwh_a", login)).await.0, StatusCode::OK);
        assert_eq!(
            call(&app, put("pwh_b", login)).await.0,
            StatusCode::CONFLICT
        );
        let (_, listed) = call(&app, signed("GET", "/v1/routes", "")).await;
        assert_eq!(
            listed["data"][0]["usernames"],
            serde_json::json!(["a@example.com"])
        );
        assert!(listed["data"][0].get("secret").is_none());
        assert_eq!(
            call(&app, signed("DELETE", "/v1/routes/pwh_a", "")).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(call(&app, put("pwh_b", login)).await.0, StatusCode::OK);
        assert_eq!(
            call(&app, put("pwh_c", r#"["nobody@example.com"]"#))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// The route's secret is stored sealed, bound to its id: the database alone, or a
    /// snapshot of it, cannot sign evidence.
    #[tokio::test]
    async fn route_secrets_are_sealed_at_rest() {
        let dir = TempDir::new();
        let (app, state) = with_login(&dir).await;
        call(&app, put("pwh_a", r#"["a@example.com"]"#)).await;
        let sealed: Vec<u8> = state
            .db
            .call(|conn| {
                conn.query_row(
                    "SELECT secret FROM routes WHERE provider_webhook_id = 'pwh_a'",
                    [],
                    |r| r.get(0),
                )
                .map_err(ApiError::from)
            })
            .await
            .unwrap();
        let raw = crypto::decode_secret(SECRET).unwrap();
        assert!(
            !sealed
                .windows(raw.len())
                .any(|window| window == raw.as_slice())
        );
        assert_eq!(state.keys.open(&sealed, b"pwh_a").unwrap(), raw);
    }
}
