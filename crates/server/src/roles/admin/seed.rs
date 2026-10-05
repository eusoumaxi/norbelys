//! `norbelys-server admin seed`: a demonstrable workspace in a development database.
//!
//! It creates a test-mode workspace (a fake transport: no mail leaves the machine) with its
//! owner and a first API key, printed once, then drives the `/v1` API in process with that key,
//! exactly as a customer's program would: custom fields, groups, a dozen people, a segment, a
//! sender connection (an SMTP login, never contacted in test mode, whose identity is tagged
//! `outbound`) and a three-step campaign sending from that tag, with the first group enrolled,
//! started. Going through the API keeps the data as valid as a customer's: every rule, event and
//! job the operations write. A running worker then checks the sender and materialises the
//! campaign.
//!
//! Idempotent by slug: when a workspace with the slug exists, nothing is written and its id is
//! printed. A seed that failed half-way leaves its workspace behind; seed again under another
//! slug.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, header};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::config::{Common, IdentityArgs, MailArgs, RenderingArgs, StorageArgs};
use crate::db::Database;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Workspace};
use crate::http::{AppState, Settings, router};
use crate::identity::api_keys::KeyMode;
use crate::identity::workspaces;

/// The largest answer read back.
const ANSWER_LIMIT: usize = 1 << 20;

/// The settings the api in process is built from, as the api role reads them.
pub struct Environment<'a> {
    /// The database, the deployment key and the environment's name.
    pub common: &'a Common,
    /// The Sending area's settings.
    pub mail: &'a MailArgs,
    /// The object store.
    pub storage: &'a StorageArgs,
    /// Sign-in and the dashboard surface.
    pub identity: &'a IdentityArgs,
    /// The tracking host.
    pub rendering: &'a RenderingArgs,
}

/// The people seeded: given name, family name, company, tier, industry, and whether they are
/// founders (the rest are agencies).
const PEOPLE: [(&str, &str, &str, &str, &str, bool); 12] = [
    (
        "Ada",
        "Lovelace",
        "Analytical Engines",
        "gold",
        "Software",
        true,
    ),
    ("Grace", "Hopper", "Compilers Inc", "gold", "Software", true),
    ("Alan", "Turing", "Enigma Labs", "silver", "Security", true),
    (
        "Katherine",
        "Johnson",
        "Orbit Analytics",
        "gold",
        "Aerospace",
        true,
    ),
    (
        "Linus",
        "Pauling",
        "Bonds & Co",
        "silver",
        "Chemistry",
        true,
    ),
    ("Hedy", "Lamarr", "Spread Spectrum", "gold", "Telecom", true),
    (
        "Claude",
        "Shannon",
        "Information Works",
        "silver",
        "Telecom",
        true,
    ),
    (
        "Barbara",
        "Liskov",
        "Substitution Systems",
        "gold",
        "Software",
        true,
    ),
    (
        "Margaret",
        "Hamilton",
        "Apollo Agency",
        "gold",
        "Aerospace",
        false,
    ),
    (
        "Edsger",
        "Dijkstra",
        "Shortest Path Studio",
        "silver",
        "Software",
        false,
    ),
    (
        "Frances",
        "Allen",
        "Optimizing Partners",
        "silver",
        "Software",
        false,
    ),
    (
        "Donald",
        "Knuth",
        "Literate Agency",
        "gold",
        "Publishing",
        false,
    ),
];

/// The api in process, called with the seeded workspace's key.
struct Api {
    router: Router,
    key: String,
    slug: String,
    calls: usize,
}

impl Api {
    /// `method path` with `body` under an idempotency key of its own; answers the JSON of a
    /// `2xx`.
    ///
    /// # Errors
    ///
    /// Any other answer, with its problem document.
    async fn call(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        self.calls += 1;
        let mut request = Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.key))
            .header(
                "idempotency-key",
                format!("seed-{}-{}", self.slug, self.calls),
            );
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&body)?)
            }
            None => Body::empty(),
        };
        let response = self.router.clone().oneshot(request.body(body)?).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), ANSWER_LIMIT).await?;
        let answer: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        if !status.is_success() {
            bail!("{method} {path} answered {status}: {answer}");
        }
        Ok(answer)
    }

    /// `POST path` with `body`; answers the created object's id.
    ///
    /// # Errors
    ///
    /// As [`Api::call`], or an answer without an id.
    async fn create(&mut self, path: &str, body: Value) -> anyhow::Result<String> {
        let answer = self.call(Method::POST, path, Some(body)).await?;
        answer
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("POST {path} answered no id: {answer}"))
    }
}

/// Seeds the workspace `slug` owned by `owner` with the api built from `environment` (see the
/// module); answers what to print.
///
/// # Errors
///
/// The settings, the database, or the first operation the API refused.
pub async fn run(
    environment: &Environment<'_>,
    slug: &str,
    owner: &EmailAddress,
) -> anyhow::Result<Value> {
    let common = environment.common;
    let db = super::super::connect(
        common,
        super::super::background_pool("norbelys-seed", 4, Duration::from_secs(30)),
    )
    .await?;
    let state = AppState {
        db: db.clone(),
        keys: super::super::keys(common)?,
        authority: crate::identity::authority::Authority::new(),
        settings: Arc::new(Settings {
            public_api_url: url::Url::parse("http://127.0.0.1:3001")?,
            senders: crate::senders::Settings::from_args(environment.mail)?,
            public_tracking_url: environment.rendering.tracking_url.clone(),
        }),
        storage: crate::storage::Storage::from_args(environment.storage, &common.environment)?,
        resolver: crate::dns::Resolver::system()?,
        identity: crate::identity::Identity::from_args(environment.identity, environment.mail)?,
        limits: crate::http::ratelimit::Limits::new(false, 1, Vec::new()),
        ingress: crate::webhooks::ingress::Ingress::start(db.clone(), None),
    };
    seed(&db, router::product(state), slug, owner).await
}

/// Seeds the workspace `slug` owned by `owner` through `router`, the product surface, creating
/// the workspace on `db`, the operator's login (see the module); answers what to print.
///
/// # Errors
///
/// The database, or the first operation the API refused.
pub(crate) async fn seed(
    db: &Database,
    router: Router,
    slug: &str,
    owner: &EmailAddress,
) -> anyhow::Result<Value> {
    let mut tx = db.begin().await?;
    let existing = sqlx::query_scalar!(
        r#"SELECT id AS "id: Id<Workspace>" FROM workspaces WHERE slug = $1"#,
        slug
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(workspace) = existing {
        tx.commit().await?;
        return Ok(json!({ "created": false, "slug": slug, "workspace": workspace }));
    }
    let created = workspaces::create_with_owner(&mut tx, slug, slug, owner, KeyMode::Test).await?;
    tx.commit().await?;
    let mut api = Api {
        router,
        key: created.api_key_secret.clone(),
        slug: slug.to_owned(),
        calls: 0,
    };

    api.create(
        "/v1/fields",
        json!({ "key": "tier", "label": "Tier", "type": "enum", "options": ["gold", "silver"] }),
    )
    .await?;
    api.create(
        "/v1/fields",
        json!({ "key": "industry", "label": "Industry", "type": "text" }),
    )
    .await?;
    let founders = api
        .create(
            "/v1/groups",
            json!({ "name": "Founders", "description": "Founders met at the 2026 summit" }),
        )
        .await?;
    let agencies = api
        .create(
            "/v1/groups",
            json!({ "name": "Agencies", "description": "Agencies that resell outreach" }),
        )
        .await?;
    for (given, family, company, tier, industry, founder) in PEOPLE {
        let group = if founder { &founders } else { &agencies };
        api.create(
            "/v1/people",
            json!({
                "email": format!("{}.{}@{}.example", given.to_ascii_lowercase(), family.to_ascii_lowercase(), slug),
                "given_name": given,
                "family_name": family,
                "company": company,
                "fields": { "tier": tier, "industry": industry },
                "group_ids": [group],
            }),
        )
        .await?;
    }
    let segment = api
        .create(
            "/v1/segments",
            json!({
                "name": "Gold accounts",
                "filter": { "match": "all", "conditions": [{ "field": "fields.tier", "operator": "equals", "value": "gold" }] },
            }),
        )
        .await?;
    let sender = format!("max@{slug}.example");
    let connection = api
        .create(
            "/v1/connections",
            json!({
                "provider": "smtp",
                "account_email": sender,
                "smtp": { "host": format!("smtp.{slug}.example"), "port": 587, "security": "starttls", "password": "demo-password" },
                "daily_limit": 50,
                "send_interval_minutes": 10,
                "identities": [{ "email": sender, "name": "Max Rivera", "tags": ["outbound"] }],
            }),
        )
        .await?;
    let campaign = api
        .create(
            "/v1/campaigns",
            json!({
                "name": "Founders outreach",
                "schedule": { "timezone": "UTC", "send_window": { "days": [1, 2, 3, 4, 5], "start": "09:00", "end": "17:00" } },
                "senders": { "tags": ["outbound"], "on_sender_removed": "reassign" },
                "steps": [
                    { "name": "Intro", "variants": [{
                        "subject": "Quick question, {{ person.given_name | default(\"there\") }}",
                        "html": "<p>Hi {{ person.given_name | default(\"there\") }},</p><p>Do you have ten minutes this week to talk about {{ person.company | default(\"your team\") }}?</p>",
                    }] },
                    { "name": "Follow-up", "delay_seconds": 172_800, "variants": [{
                        "subject": "Re: quick question",
                        "html": "<p>Any thoughts, {{ person.given_name | default(\"there\") }}?</p>",
                    }] },
                    { "name": "Last note", "delay_seconds": 345_600, "variants": [{
                        "subject": "Closing the loop",
                        "html": "<p>I will not write again; reply whenever the timing is better.</p>",
                    }] },
                ],
                "stop_rules": { "on_reply": "all" },
                "tracking": { "opens": false, "clicks": true },
            }),
        )
        .await?;
    let enrolled = api
        .call(
            Method::POST,
            "/v1/enrollments",
            Some(json!({ "campaign_id": campaign, "group_id": founders })),
        )
        .await?;
    api.call(
        Method::POST,
        &format!("/v1/campaigns/{campaign}/start"),
        None,
    )
    .await?;
    Ok(json!({
        "created": true,
        "slug": slug,
        "workspace": created.workspace,
        "owner": created.owner,
        "owner_email": owner.as_str(),
        "api_key": created.api_key,
        "api_key_secret": created.api_key_secret,
        "groups": { "founders": founders, "agencies": agencies },
        "people": PEOPLE.len(),
        "segment": segment,
        "connection": connection,
        "campaign": campaign,
        "enrollments": enrolled,
    }))
}

#[cfg(test)]
mod tests {
    use super::seed;
    use crate::domain::email::EmailAddress;
    use crate::testing::TestDb;

    /// The seed drives the real API to a demonstrable workspace, so every request it makes stays
    /// valid against the contract a customer's program meets; a second run with the same slug
    /// writes nothing and names the workspace.
    #[tokio::test]
    async fn the_seed_makes_a_demonstrable_workspace_once() {
        let test = TestDb::new().await;
        let owner = EmailAddress::parse("owner@demo.example").unwrap();
        let seeded = seed(&test.system, test.app().router(), "demo", &owner)
            .await
            .unwrap();
        assert_eq!(seeded["created"], true, "{seeded}");
        assert!(
            seeded["api_key_secret"]
                .as_str()
                .unwrap()
                .starts_with("nb_test_")
        );
        let made: (i64, i64, i64, i64, String) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM people p WHERE p.workspace_id = w.id),
                    (SELECT count(*) FROM groups g WHERE g.workspace_id = w.id),
                    (SELECT count(*) FROM enrollments e WHERE e.workspace_id = w.id),
                    (SELECT count(*) FROM steps s WHERE s.workspace_id = w.id),
                    (SELECT c.status FROM campaigns c WHERE c.workspace_id = w.id)
               FROM workspaces w WHERE w.slug = 'demo'",
        )
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(made, (12, 2, 8, 3, "materialising".to_owned()));
        let again = seed(&test.system, test.app().router(), "demo", &owner)
            .await
            .unwrap();
        assert_eq!(again["created"], false);
        assert_eq!(again["workspace"], seeded["workspace"]);
    }
}
