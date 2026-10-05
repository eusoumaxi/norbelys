//! Tests of tracking against real PostgreSQL and the routers in process: the drain's once-only
//! storage and its rollups (store), the tracking role's routes over a real spool (protocol), and
//! the api's unsubscribe and image routes (API).

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use sqlx::AssertSqlSafe;
use tower::ServiceExt as _;
use uuid::{NoContext, Uuid};

use super::drain;
use super::http::{Tracker, tracker};
use super::spool::{Event, Limits, Spool};
use super::token::Token;
use super::unsubscribe::{self, Outcome};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::scope::{Scope, ScopeSet};
use crate::domain::tracking::{ActorClass, EventKind};
use crate::process::Shutdown;
use crate::testing::{TestDb, TestWorkspace, keys};

// ───────────────────────────── fixtures ─────────────────────────────

/// What [`fixture`] made in one workspace.
struct Fixture {
    workspace: TestWorkspace,
    /// A campaign message to `ada@example.com`, two minutes old.
    campaign: Id<Message>,
    /// A direct message to `bob@example.com`, two minutes old: mail outside a campaign.
    direct: Id<Message>,
    /// The campaign's, its step's and its variant's ids, for the counters.
    ancestry: (Uuid, Uuid, Uuid),
}

/// A message id created `seconds` ago.
fn message_id(seconds: u64) -> Uuid {
    let now = jiff::Timestamp::now().as_second();
    let at = u64::try_from(now).unwrap() - seconds;
    Uuid::new_v7(uuid::Timestamp::from_unix(NoContext, at, 0))
}

/// A workspace with a mailbox, a person, a campaign of one step and variant with Ada enrolled,
/// and two messages two minutes old (old enough for a person to have read them): Ada's campaign
/// message and a direct one. Written through the system login, as an operator would.
async fn fixture(test: &TestDb, slug: &str) -> Fixture {
    let workspace = test.workspace(slug).await;
    let ws = workspace.id.uuid();
    let [
        connection,
        identity,
        person,
        campaign,
        step,
        variant,
        enrollment,
    ] = std::array::from_fn(|_| Uuid::now_v7());
    let (message, direct) = (message_id(120), message_id(120));
    for parent in ["messages", "message_engagement"] {
        sqlx::query("SELECT ensure_partition($1::text::regclass, now() - interval '2 minutes')")
            .bind(parent)
            .execute(test.system.pool())
            .await
            .unwrap();
    }
    let sql = format!(
        "INSERT INTO connections (workspace_id, id, provider, transport, account_email, smtp, status, daily_limit, send_interval_minutes)
           VALUES ('{ws}', '{connection}', 'smtp', 'smtp', 'max@{slug}.example', '{{}}', 'active', 100, 10);
         INSERT INTO sender_identities (workspace_id, id, connection_id, email) VALUES ('{ws}', '{identity}', '{connection}', 'max@{slug}.example');
         INSERT INTO people (workspace_id, id, email) VALUES ('{ws}', '{person}', 'ada@example.com');
         INSERT INTO campaigns (workspace_id, id, name) VALUES ('{ws}', '{campaign}', 'Launch');
         INSERT INTO steps (workspace_id, id, campaign_id, position, name) VALUES ('{ws}', '{step}', '{campaign}', 1, 'First');
         INSERT INTO step_revisions (workspace_id, step_id, revision, delay_seconds, ranking_objective, observation_window_seconds, minimum_sample)
           VALUES ('{ws}', '{step}', 1, 0, 'opens', 3600, 10);
         INSERT INTO variants (workspace_id, id, step_id, name) VALUES ('{ws}', '{variant}', '{step}', 'A');
         INSERT INTO variant_revisions (workspace_id, variant_id, version, subject, html) VALUES ('{ws}', '{variant}', 1, 'Hi', '<p>Hi</p>');
         INSERT INTO step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version) VALUES ('{ws}', '{step}', 1, '{variant}', 1);
         INSERT INTO enrollments (workspace_id, id, campaign_id, person_id) VALUES ('{ws}', '{enrollment}', '{campaign}', '{person}');
         INSERT INTO messages (workspace_id, id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, render_version,
                               rendered_at, internet_message_id, send_at, campaign_id, step_id, step_revision, variant_id, variant_version,
                               enrollment_id, person_id)
           VALUES ('{ws}', '{message}', 'campaign', '{identity}', '{connection}', 'max@{slug}.example', ARRAY['ada@example.com'], 'Hi', '1',
                   now(), '<{message}@{slug}.example>', now(), '{campaign}', '{step}', 1, '{variant}', 1, '{enrollment}', '{person}');
         INSERT INTO messages (workspace_id, id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, html,
                               render_version, rendered_at, internet_message_id, send_at)
           VALUES ('{ws}', '{direct}', 'direct', '{identity}', '{connection}', 'max@{slug}.example', ARRAY['bob@example.com'], 'Hi',
                   '<p>Hi</p>', '1', now(), '<{direct}@{slug}.example>', now());"
    );
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(test.system.pool())
        .await
        .unwrap();
    Fixture {
        workspace,
        campaign: Id::from_uuid(message),
        direct: Id::from_uuid(direct),
        ancestry: (campaign, step, variant),
    }
}

/// An event of `kind` and `actor` on `message`, as the tracking routes record it.
fn event(
    workspace: WorkspaceId,
    message: Id<Message>,
    kind: EventKind,
    actor: ActorClass,
) -> Event {
    Event {
        id: Uuid::now_v7(),
        workspace,
        message,
        kind,
        link: (kind == EventKind::Click).then_some(0),
        url_hash: (kind == EventKind::Click).then(|| crate::crypto::sha256(b"https://example.com")),
        actor,
        ip_hash: Some(vec![1; 32]),
        user_agent: Some("Mozilla/5.0 (Macintosh)".to_owned()),
        occurred_at: crate::process::now(),
    }
}

/// A message's rollup: opens, human opens, clicks, human clicks, and whether its first open and
/// first click are set; `None` without a row.
async fn engagement(
    test: &TestDb,
    message: Id<Message>,
) -> Option<(i32, i32, i32, i32, bool, bool)> {
    sqlx::query!(
        "SELECT opens, human_opens, clicks, human_clicks, first_open_at IS NOT NULL AS \"opened!\",
                first_click_at IS NOT NULL AS \"clicked!\"
           FROM message_engagement WHERE message_id = $1",
        message.uuid()
    )
    .fetch_optional(test.system.pool())
    .await
    .unwrap()
    .map(|row| {
        (
            row.opens,
            row.human_opens,
            row.clicks,
            row.human_clicks,
            row.opened,
            row.clicked,
        )
    })
}

/// The increments of `metric` in the workspace, with their campaign, step and variant.
async fn increments(
    test: &TestDb,
    workspace: WorkspaceId,
    metric: &str,
) -> Vec<(Option<Uuid>, Option<Uuid>, Option<Uuid>)> {
    sqlx::query!(
        "SELECT campaign_id, step_id, variant_id FROM stats_increments
          WHERE workspace_id = $1 AND metric = $2 AND day = (now() AT TIME ZONE 'UTC')::date",
        workspace.uuid(),
        metric
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.campaign_id, row.step_id, row.variant_id))
    .collect()
}

/// The workspace's raw tracking events: their kind and actor class, oldest first.
async fn stored(test: &TestDb, workspace: WorkspaceId) -> Vec<(String, String)> {
    sqlx::query!(
        "SELECT kind, actor_class FROM tracking_events WHERE workspace_id = $1 ORDER BY occurred_at, id",
        workspace.uuid()
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.kind, row.actor_class))
    .collect()
}

/// A spool in a directory of its own, removed with it.
struct TestSpool {
    spool: Spool,
    dir: std::path::PathBuf,
}

impl TestSpool {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("norbelys-tracking-{}", Uuid::now_v7().simple()));
        Self {
            spool: Spool::open(&dir, Limits::default()).unwrap(),
            dir,
        }
    }

    /// The tracking role's router over this spool, trusting `X-Forwarded-For`.
    fn router(&self) -> Router {
        tracker(Tracker {
            keys: keys(),
            spool: self.spool.clone(),
            trust_forwarded_for: true,
            trusted_proxy_ips: vec!["127.0.0.1".parse().unwrap()],
        })
    }

    /// Every event waiting in the spool.
    async fn events(&self) -> Vec<Event> {
        self.spool
            .take(1_000)
            .await
            .unwrap()
            .records
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }
}

impl Drop for TestSpool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One request through `router`: the status, the headers and the body.
async fn hit(
    router: &Router,
    method: Method,
    path: &str,
    user_agent: Option<&str>,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("x-forwarded-for", "203.0.113.9")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
    if let Some(user_agent) = user_agent {
        request = request.header("user-agent", user_agent);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, body)
}

/// A browser's `User-Agent`.
const BROWSER: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)";

/// `token` with one character changed, so its signature no longer matches.
fn tampered(token: &str) -> String {
    let mut chars: Vec<char> = token.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
    chars.into_iter().collect()
}

// ───────────────────────────── store: the drain ─────────────────────────────

/// A batch stored twice, as after a crash between the commit and the spool's acknowledgement,
/// inserts its events once and rolls them up once: the second pass inserts nothing, adds nothing
/// to the per-message rollup and writes no increment. The first pass rolls up every class (a
/// scanner's open counts as an open, never as a human one) and counts the campaign message's
/// first human open and click once each, under its campaign, step and variant, while the direct
/// message contributes one increment with no campaign dimensions for general message metrics.
#[tokio::test]
async fn a_batch_stored_twice_counts_once() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let ws = fixture.workspace.id;
    let batch = vec![
        event(ws, fixture.campaign, EventKind::Open, ActorClass::Human),
        event(ws, fixture.campaign, EventKind::Open, ActorClass::Human),
        event(ws, fixture.campaign, EventKind::Open, ActorClass::Scanner),
        event(ws, fixture.campaign, EventKind::Click, ActorClass::Human),
        event(ws, fixture.direct, EventKind::Open, ActorClass::Human),
    ];
    let first = drain::store(&test.tracking, &batch).await.unwrap();
    assert_eq!((first.inserted, first.increments), (5, 3));
    let again = drain::store(&test.tracking, &batch).await.unwrap();
    assert_eq!((again.inserted, again.increments), (0, 0));

    assert_eq!(
        engagement(&test, fixture.campaign).await,
        Some((3, 2, 1, 1, true, true))
    );
    assert_eq!(
        engagement(&test, fixture.direct).await,
        Some((1, 1, 0, 0, true, false))
    );
    let (campaign, step, variant) = fixture.ancestry;
    let counted = vec![(Some(campaign), Some(step), Some(variant))];
    let mut opened = increments(&test, ws, "opened").await;
    opened.sort_unstable();
    assert_eq!(
        opened,
        vec![
            (None, None, None),
            (Some(campaign), Some(step), Some(variant))
        ]
    );
    assert_eq!(increments(&test, ws, "clicked").await, counted);
    assert_eq!(stored(&test, ws).await.len(), 5);
}

/// Two drains storing the same batch at once (a pass that timed out after its commit, raced by
/// its retry) meet on the events' keys: between them every event is inserted once, and the
/// message's first human open is counted once.
#[tokio::test]
async fn two_drains_of_one_batch_count_once() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let ws = fixture.workspace.id;
    let batch: Vec<Event> = (0..20)
        .map(|_| event(ws, fixture.campaign, EventKind::Open, ActorClass::Human))
        .collect();
    let (a, b) = tokio::join!(
        drain::store(&test.tracking, &batch),
        drain::store(&test.tracking, &batch)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.inserted + b.inserted, 20);
    assert_eq!(a.increments + b.increments, 1);
    assert_eq!(
        engagement(&test, fixture.campaign).await,
        Some((20, 20, 0, 0, true, false))
    );
    assert_eq!(increments(&test, ws, "opened").await.len(), 1);
}

/// Two drains with different events of one message at once meet on its rollup row: both are
/// rolled up, and only one of them counts the message's first human open; a later batch adds to
/// the rollup and counts only what is new (the first human click).
#[tokio::test]
async fn concurrent_batches_of_one_message_count_its_first_human_open_once() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let ws = fixture.workspace.id;
    let open = || {
        vec![event(
            ws,
            fixture.campaign,
            EventKind::Open,
            ActorClass::Human,
        )]
    };
    let (first, second) = (open(), open());
    let (a, b) = tokio::join!(
        drain::store(&test.tracking, &first),
        drain::store(&test.tracking, &second)
    );
    assert_eq!(a.unwrap().increments + b.unwrap().increments, 1);
    let later = drain::store(
        &test.tracking,
        &[
            event(ws, fixture.campaign, EventKind::Open, ActorClass::Human),
            event(ws, fixture.campaign, EventKind::Click, ActorClass::Human),
        ],
    )
    .await
    .unwrap();
    assert_eq!((later.inserted, later.increments), (2, 1));
    assert_eq!(
        engagement(&test, fixture.campaign).await,
        Some((3, 3, 1, 1, true, true))
    );
    assert_eq!(increments(&test, ws, "opened").await.len(), 1);
    assert_eq!(increments(&test, ws, "clicked").await.len(), 1);
}

/// An event of a message older than the record window (it reached the database late, after an
/// outage) is stored raw and not rolled up, since the partition of its rollup may already be
/// archived; it neither fails its batch nor counts.
#[tokio::test]
async fn a_message_older_than_the_window_is_stored_raw_only() {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let old = Id::<Message>::from_uuid(message_id(40 * 86_400));
    let stored_now = drain::store(
        &test.tracking,
        &[event(workspace.id, old, EventKind::Open, ActorClass::Human)],
    )
    .await
    .unwrap();
    assert_eq!((stored_now.inserted, stored_now.increments), (1, 0));
    assert_eq!(engagement(&test, old).await, None);
    assert_eq!(stored(&test, workspace.id).await.len(), 1);
}

// ───────────────────────────── store: unsubscribe ─────────────────────────────

/// Two unsubscribes of one address at once (a person's click and their provider's one-click
/// request) write one suppression, one delivery event and one `unsubscribed` increment: the
/// second waits for the first and finds the address suppressed.
#[tokio::test]
async fn concurrent_unsubscribes_write_once() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let ws = fixture.workspace.id;
    let ada = EmailAddress::parse("ada@example.com").unwrap();
    let (a, b) = tokio::join!(
        unsubscribe::record(&test.app, ws, fixture.campaign, &ada),
        unsubscribe::record(&test.app, ws, fixture.campaign, &ada)
    );
    let mut outcomes = [a.unwrap(), b.unwrap()];
    outcomes.sort_by_key(|outcome| *outcome == Outcome::Suppressed);
    assert_eq!(outcomes, [Outcome::AlreadySuppressed, Outcome::Suppressed]);
    let counts = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM suppressions WHERE workspace_id = $1) AS "suppressions!",
                  (SELECT count(*) FROM delivery_events WHERE workspace_id = $1 AND source = 'unsubscribe') AS "events!",
                  (SELECT count(*) FROM stats_increments WHERE workspace_id = $1 AND metric = 'unsubscribed') AS "increments!""#,
        ws.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        (counts.suppressions, counts.events, counts.increments),
        (1, 1, 1)
    );
}

// ───────────────────────────── protocol: the tracking role ─────────────────────────────

/// The open route answers its uncacheable pixel to every token, valid or not, so no mail shows a
/// broken image; it records only a valid open token of a message within the record window: not
/// a tampered token, not a click token presented as an open, not a message older than the window.
#[tokio::test]
async fn the_pixel_answers_every_token_and_records_only_valid_recent_ones() {
    let spool = TestSpool::new();
    let router = spool.router();
    let workspace = WorkspaceId::trusted(Uuid::now_v7());
    let message = Id::from_uuid(message_id(120));
    let open = Token::Open { workspace, message }.encode(&keys());
    let expired = Token::Open {
        workspace,
        message: Id::from_uuid(message_id(40 * 86_400)),
    }
    .encode(&keys());
    let click = Token::Click {
        workspace,
        message,
        link: 0,
        url: "https://example.com".to_owned(),
    }
    .encode(&keys());
    for token in [
        &open,
        &tampered(&open),
        &expired,
        &click,
        &"junk".to_owned(),
    ] {
        let (status, headers, body) = hit(
            &router,
            Method::GET,
            &format!("/t/o/{token}"),
            Some(BROWSER),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{token}");
        assert_eq!(headers["content-type"], "image/gif");
        assert!(
            headers["cache-control"]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        assert_eq!(&body[..6], b"GIF89a");
        assert_eq!(body.len(), 43);
    }
    let recorded = spool.events().await;
    assert_eq!(recorded.len(), 1);
    let only = &recorded[0];
    assert_eq!(
        (only.workspace, only.message, only.kind),
        (workspace, message, EventKind::Open)
    );
    assert_eq!(only.actor, ActorClass::Human);
    assert_eq!(only.ip_hash, Some(keys().hash_address("203.0.113.9")));
    assert_eq!(only.user_agent.as_deref(), Some(BROWSER));
}

/// A click redirects (`302`, uncacheable, passing on no `Referer`) only to the destination its
/// token signs, also for a message older than the record window, which is not recorded; a
/// tampered token, an open token presented as a click and junk answer `404` and record nothing.
/// A `HEAD` (a link checker) is answered and recorded as a scanner, and a click within the
/// message's first minute as one too.
#[tokio::test]
async fn a_click_redirects_only_where_its_token_signs() {
    let spool = TestSpool::new();
    let router = spool.router();
    let workspace = WorkspaceId::trusted(Uuid::now_v7());
    let destination = "https://example.com/pricing?plan=pro&ref=mail";
    let click = |message: Id<Message>| {
        Token::Click {
            workspace,
            message,
            link: 3,
            url: destination.to_owned(),
        }
        .encode(&keys())
    };
    let read = Id::from_uuid(message_id(120));
    let fresh = Id::from_uuid(message_id(0));
    let expired = Id::from_uuid(message_id(40 * 86_400));
    for (method, message) in [
        (Method::GET, read),
        (Method::HEAD, read),
        (Method::GET, fresh),
        (Method::GET, expired),
    ] {
        let (status, headers, _) = hit(
            &router,
            method,
            &format!("/t/c/{}", click(message)),
            Some(BROWSER),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(headers["location"], destination);
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(
            headers["cache-control"]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
    }
    let open = Token::Open {
        workspace,
        message: read,
    }
    .encode(&keys());
    for path in [
        format!("/t/c/{}", tampered(&click(read))),
        format!("/t/c/{open}"),
        "/t/c/junk".to_owned(),
    ] {
        let (status, headers, _) = hit(&router, Method::GET, &path, Some(BROWSER)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!headers.contains_key("location"));
    }
    let recorded: Vec<(Id<Message>, ActorClass, Option<i16>)> = spool
        .events()
        .await
        .into_iter()
        .map(|event| (event.message, event.actor, event.link))
        .collect();
    assert_eq!(
        recorded,
        [
            (read, ActorClass::Human, Some(3)),
            (read, ActorClass::Scanner, Some(3)),
            (fresh, ActorClass::Scanner, Some(3)),
        ]
    );
}

/// The tracking role serves the brand mark of the platform's own mail from the binary, to anyone,
/// with no database or object store: the exact PNG, typed as one, cacheable by everyone for a
/// year (a new mark gets a new path) and never sniffed as anything else.
#[tokio::test]
async fn the_tracking_role_serves_the_brand_mark() {
    let spool = TestSpool::new();
    let (status, headers, body) = hit(
        &spool.router(),
        Method::GET,
        "/brand/v1/email-mark.png",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(
        body.as_ref(),
        include_bytes!("../../assets/email-mark.png").as_slice()
    );
    assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
}

/// The role is live while it runs and ready while its spool takes events; a stopped spool makes
/// it unready, whatever the database does (the role answers without one).
#[tokio::test]
async fn readiness_follows_the_spool() {
    let spool = TestSpool::new();
    let router = spool.router();
    assert_eq!(
        hit(&router, Method::GET, "/health/live", None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hit(&router, Method::GET, "/health/ready", None).await.0,
        StatusCode::NO_CONTENT
    );
    spool.spool.close().await;
    assert_eq!(
        hit(&router, Method::GET, "/health/ready", None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        hit(&router, Method::GET, "/health/live", None).await.0,
        StatusCode::NO_CONTENT
    );
}

/// Opens and clicks answered by the tracking role reach the database through the spool and the
/// drain, classified by what each request showed: a browser and Gmail's image proxy as people,
/// Apple's bare prefetch as a proxy, a `HEAD` as a scanner; the message's rollup and its
/// campaign's increments follow, and a drain asked to stop empties the spool before it returns.
#[tokio::test]
async fn opens_and_clicks_reach_the_database_through_the_drain() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let ws = fixture.workspace.id;
    let spool = TestSpool::new();
    let router = spool.router();
    let open = Token::Open {
        workspace: ws,
        message: fixture.campaign,
    }
    .encode(&keys());
    let click = Token::Click {
        workspace: ws,
        message: fixture.campaign,
        link: 0,
        url: "https://example.com".to_owned(),
    }
    .encode(&keys());
    let gmail =
        "Mozilla/5.0 (Windows NT 5.1; rv:11.0) Gecko Firefox/11.0 (via ggpht.com GoogleImageProxy)";
    for (method, path, agent) in [
        (Method::GET, format!("/t/o/{open}"), BROWSER),
        (Method::GET, format!("/t/o/{open}"), gmail),
        (Method::GET, format!("/t/o/{open}"), "Mozilla/5.0"),
        (Method::GET, format!("/t/c/{click}"), BROWSER),
        (Method::HEAD, format!("/t/c/{click}"), BROWSER),
    ] {
        hit(&router, method, &path, Some(agent)).await;
    }
    let (stop, stopping) = Shutdown::manual();
    stopping.send(true).unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        drain::run(test.tracking.clone(), spool.spool.clone(), stop),
    )
    .await
    .unwrap();
    assert!(spool.events().await.is_empty());
    let classes: Vec<(String, String)> = stored(&test, ws).await;
    let expected = [
        ("open", "human"),
        ("open", "human"),
        ("open", "proxy"),
        ("click", "human"),
        ("click", "scanner"),
    ];
    assert_eq!(
        classes,
        expected.map(|(kind, class)| (kind.to_owned(), class.to_owned()))
    );
    assert_eq!(
        engagement(&test, fixture.campaign).await,
        Some((3, 2, 2, 1, true, true))
    );
    assert_eq!(increments(&test, ws, "opened").await.len(), 1);
    assert_eq!(increments(&test, ws, "clicked").await.len(), 1);
}

// ───────────────────────────── API: unsubscribe ─────────────────────────────

/// The unsubscribe page names the address and asks with a form that posts back to itself, and
/// changes nothing: scanners fetch every link they find. A link this deployment did not sign
/// answers `404`, for the page and for the request alike.
#[tokio::test]
async fn the_unsubscribe_page_asks_and_changes_nothing() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let app = test.app();
    let token = Token::Unsubscribe {
        workspace: fixture.workspace.id,
        message: fixture.campaign,
        email: "ada@example.com".to_owned(),
    }
    .encode(&keys());
    let page = app.get(&format!("/u/{token}")).send().await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(
        page.header("content-security-policy")
            .unwrap()
            .contains("form-action 'self'")
    );
    let html = String::from_utf8(page.body.to_vec()).unwrap();
    assert!(html.contains("ada@example.com"), "{html}");
    assert!(html.contains("<form method=\"post\">"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    let suppressions = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM suppressions WHERE workspace_id = $1"#,
        fixture.workspace.id.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(suppressions, 0);
    for reply in [
        app.get(&format!("/u/{}", tampered(&token))).send().await,
        app.post(&format!("/u/{}", tampered(&token))).send().await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }
}

/// The one-click request (RFC 8058's body, no credential) answers `200`, never a redirect, and
/// suppresses the address for the workspace with reason and source `unsubscribe` and the evidence's
/// summary, citing the delivery event it records; the campaign's `unsubscribed` counter and the
/// customer's `suppression.created` follow. Repeating it answers `200` and writes nothing more.
#[tokio::test]
async fn one_click_unsubscribes_once() {
    let test = TestDb::new().await;
    let fixture = fixture(&test, "acme").await;
    let ws = fixture.workspace.id;
    let app = test.app();
    let token = Token::Unsubscribe {
        workspace: ws,
        message: fixture.campaign,
        email: "Ada@Example.com".to_owned(),
    }
    .encode(&keys());
    for _ in 0..2 {
        let reply = app
            .post(&format!("/u/{token}"))
            .raw(
                "application/x-www-form-urlencoded",
                "List-Unsubscribe=One-Click",
            )
            .send()
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.header("location").is_none());
        assert!(String::from_utf8_lossy(&reply.body).contains("unsubscribed"));
    }
    let suppression = sqlx::query!(
        r#"SELECT s.email_key, s.reason, s.source, s.evidence AS "evidence!", s.source_event AS "source_event!",
                  (SELECT count(*) FROM suppressions WHERE workspace_id = $1) AS "suppressions!",
                  (SELECT count(*) FROM delivery_events e WHERE e.workspace_id = $1 AND e.id = s.source_event
                      AND e.source = 'unsubscribe' AND e.kind = 'unsubscribed' AND e.message_id = $2) AS "events!",
                  (SELECT count(*) FROM stats_increments WHERE workspace_id = $1 AND metric = 'unsubscribed') AS "increments!",
                  (SELECT count(*) FROM outbox_events WHERE workspace_id = $1 AND type = 'suppression.created') AS "told!"
             FROM suppressions s WHERE s.workspace_id = $1"#,
        ws.uuid(),
        fixture.campaign.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        (
            suppression.email_key.as_str(),
            suppression.reason.as_str(),
            suppression.source.as_str()
        ),
        ("ada@example.com", "unsubscribe", "unsubscribe")
    );
    assert_eq!(suppression.evidence["kind"], "unsubscribed");
    assert_eq!(
        (
            suppression.suppressions,
            suppression.events,
            suppression.increments,
            suppression.told
        ),
        (1, 1, 1, 1)
    );
}

// ───────────────────────────── API: images ─────────────────────────────

/// The first bytes of a PNG file, enough to prove its format.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

/// An image uploaded with its own type answers `201` with its public URL on the tracking host;
/// that URL serves the exact bytes to anyone, with the image's type, a year of immutable caching
/// and `nosniff`. Deleting it answers `204`, after which the URL answers `404` and a second
/// deletion `404`.
#[tokio::test]
async fn images_are_uploaded_served_and_deleted() {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let app = test.app();
    let created = app
        .post("/v1/images")
        .bearer(&workspace.key)
        .idempotency("logo")
        .raw("image/png", PNG)
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let id = created.json["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("img_"));
    assert_eq!(created.json["content_type"], "image/png");
    assert_eq!(created.json["size"], PNG.len());
    let url = created.json["url"].as_str().unwrap();
    let path = url.strip_prefix("https://t.norbelys.test").unwrap();
    assert_eq!(path, format!("/images/{}/{id}.png", workspace.id));

    let served = app.get(path).send().await;
    assert_eq!(served.status, StatusCode::OK);
    assert_eq!(served.body.as_ref(), PNG);
    assert_eq!(served.header("content-type"), Some("image/png"));
    assert_eq!(
        served.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(served.header("x-content-type-options"), Some("nosniff"));

    let deleted = app
        .delete(&format!("/v1/images/{id}"))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(app.get(path).send().await.status, StatusCode::NOT_FOUND);
    let again = app
        .delete(&format!("/v1/images/{id}"))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    assert_eq!(again.json["code"], "not_found");
}

/// An image belongs to its workspace: another workspace deleting it by its id answers `404`, as
/// for an id that does not exist, and the image keeps being served; a public path that names no
/// image (another extension, another resource's id, a stranger's path) answers `404`.
#[tokio::test]
async fn images_belong_to_their_workspace() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let app = test.app();
    let created = app
        .post("/v1/images")
        .bearer(&acme.key)
        .idempotency("logo")
        .raw("image/png", PNG)
        .send()
        .await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let path = format!("/images/{}/{id}.png", acme.id);
    let foreign = app
        .delete(&format!("/v1/images/{id}"))
        .bearer(&globex.key)
        .send()
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    assert_eq!(app.get(&path).send().await.status, StatusCode::OK);
    for other in [
        format!("/images/{}/{id}.jpg", acme.id),
        format!("/images/{}/{id}.png", globex.id),
        format!("/images/{}/{}.png", acme.id, Id::<Message>::new()),
        format!("/images/{}/{id}", acme.id),
    ] {
        assert_eq!(
            app.get(&other).send().await.status,
            StatusCode::NOT_FOUND,
            "{other}"
        );
    }
}

/// An upload must be an image of the type it declares: another type (SVG among them) is `415`,
/// content that is not the declared format or no content is `422`, a file over 16 MiB is `413`,
/// and a credential without `campaigns:write` is `403`; none of them stores anything.
#[tokio::test]
async fn uploads_must_be_images_of_their_declared_type() {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let reader = test
        .api_key(&workspace, ScopeSet::from_iter([Scope::CampaignsRead]))
        .await;
    let app = test.app();
    let upload = |key: &str, content_type: &str, body: Vec<u8>, attempt: &str| {
        app.post("/v1/images")
            .bearer(key)
            .idempotency(attempt)
            .raw(content_type, body)
    };
    let cases = [
        (
            &workspace.key,
            "image/svg+xml",
            b"<svg/>".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            &workspace.key,
            "text/plain",
            PNG.to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            &workspace.key,
            "image/jpeg",
            PNG.to_vec(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            &workspace.key,
            "image/png",
            Vec::new(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            &workspace.key,
            "image/png",
            vec![0; (16 << 20) + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (&reader, "image/png", PNG.to_vec(), StatusCode::FORBIDDEN),
    ];
    for (n, (key, content_type, body, status)) in cases.into_iter().enumerate() {
        let reply = upload(key, content_type, body, &format!("attempt-{n}"))
            .send()
            .await;
        assert_eq!(reply.status, status, "{content_type}: {}", reply.json);
    }
    let images = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM images WHERE workspace_id = $1"#,
        workspace.id.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(images, 0);
}
