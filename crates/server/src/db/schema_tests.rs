//! Store tests of the schema itself: what the SQLx migrations make PostgreSQL enforce whatever the code
//! above it does, proven against a real database with the real role logins.
//!
//! They cover the constraints that keep rows coherent (a campaign message's ancestry proven link
//! by link, the refresh-token chain, the rules of connections, idempotency scopes, archive pairs),
//! row security and the grants of every role (what each login reaches, and the scheduler's narrow
//! view across workspaces), the lookups that run before a workspace is known, the secret
//! accessors, the partition helper with its period changes and its lookahead, the archive gate
//! and the sealed archive order, replay protection for provider events under concurrency, and the
//! agreement of the stored Rust enumerations with their `CHECK`s.
//!
//! Fixtures are written through the system login, which bypasses row security and holds every
//! table grant, so a refusal in a constraint test can only come from the constraint under test.
//! The role tests run each statement through the login of the role they describe, in a
//! transaction rolled back afterwards. Every refusal is asserted with its SQLSTATE, and refusals
//! sit beside an accepted statement of the same shape, so a mistake in a fixture cannot pass for
//! a refusal.
//!
//! The protocols that run on these tables (the delivery claim and its 5-minute grid, the start and
//! finish of a submission, the slot projection, inbox polls, the creation of a step's message) are
//! tested with the modules that implement them; the job runner's own use of the scheduler role and
//! of the maintenance lane is tested in `jobs`.

use std::borrow::Cow;
use std::time::Duration;

use sqlx::AssertSqlSafe;
use strum::IntoEnumIterator as _;
use uuid::Uuid;

use crate::db::{self, Database};
use crate::domain::ids::WorkspaceId;
use crate::domain::scope::MembershipRole;
use crate::identity::api_keys::KeyMode;
use crate::testing::TestDb;

/// SQLSTATE of a violated `CHECK`.
const CHECK: &str = "23514";
/// SQLSTATE of a violated unique constraint or index.
const UNIQUE: &str = "23505";
/// SQLSTATE of a violated foreign key.
const FOREIGN_KEY: &str = "23503";
/// SQLSTATE of a delete refused by `ON DELETE RESTRICT`.
const RESTRICT: &str = "23001";
/// SQLSTATE of a missing privilege, and of a new row refused by row security.
const DENIED: &str = "42501";

/// `acme`, the workspace most fixtures belong to.
const ACME: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_a000);
/// `acme`'s owner.
const ACME_OWNER: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_a001);
/// `acme`'s managed MTA connection, which holds a credential.
const ACME_MTA: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_a010);
/// `acme`'s webhook endpoint.
const ACME_ENDPOINT: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_a090);
/// `beta`, the other workspace, whose rows `acme`'s transactions must never reach.
const BETA: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_b000);
/// `beta`'s SMTP mailbox, which holds a credential.
const BETA_MAILBOX: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_b010);
/// `beta`'s webhook endpoint.
const BETA_ENDPOINT: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_b090);

/// The rows the tests share: two workspaces with their owners; in `acme` the managed MTA (rate
/// paced, with a credential), a Google mailbox (paced, on a half-open quota scope) and an SES relay
/// (on its account's scope); a campaign whose step's revision 1 offers variant `a` only, a second
/// campaign, and a person enrolled in both; a provider webhook, an invitation and an API key for
/// the lookups; OAuth grants with a root refresh token; an inbound message awaiting review;
/// idempotency keys of two users and of the workspace. In `beta`, a paced SMTP mailbox with a
/// credential and one not yet verified. Each workspace has a person or two, a webhook endpoint, a
/// job with its lane, and a direct message in its delivery queue.
const SEED: &str = r#"
INSERT INTO workspaces (id, slug, name) VALUES
  ('00000000-0000-7000-8000-00000000a000', 'acme', 'Acme'),
  ('00000000-0000-7000-8000-00000000b000', 'beta', 'Beta');
INSERT INTO users (id, email) VALUES
  ('00000000-0000-7000-8000-00000000a001', 'owner@acme.test'),
  ('00000000-0000-7000-8000-00000000b001', 'owner@beta.test');
INSERT INTO memberships (workspace_id, user_id, role, source) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a001', 'owner', 'creator'),
  ('00000000-0000-7000-8000-00000000b000', '00000000-0000-7000-8000-00000000b001', 'owner', 'creator');
INSERT INTO quota_scopes (workspace_id, id, provider, scope_key, consecutive_failures, paused_until) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a080', 'google', 'project-1', 1, now() - interval '1 minute');
INSERT INTO quota_scopes (workspace_id, id, provider, scope_key, recipients_per_day, window_limit, window_unit, window_seconds) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a081', 'ses', '111122223333:eu-west-1', 50000, 14, 'recipients', 1);
INSERT INTO connections (workspace_id, id, provider, transport, account_email, smtp, credential, status, daily_limit, send_interval_minutes, quota_scope_id) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a010', 'norbelys', 'smtp', 'mta@acme.test', '{}', '\xdeadbeef', 'active', 1000000, NULL, NULL),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a011', 'google', 'api', 'gmail@acme.test', NULL, NULL, 'active', 2000, 10, '00000000-0000-7000-8000-00000000a080'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a012', 'ses', 'smtp', 'relay@acme.test', '{"configuration_set": "acme-events"}', NULL, 'active', 100000, NULL, '00000000-0000-7000-8000-00000000a081'),
  ('00000000-0000-7000-8000-00000000b000', '00000000-0000-7000-8000-00000000b010', 'smtp', 'smtp', 'mailbox@beta.test', '{}', '\xcafe', 'active', 100, 10, NULL),
  ('00000000-0000-7000-8000-00000000b000', '00000000-0000-7000-8000-00000000b011', 'smtp', 'smtp', 'new@beta.test', '{}', NULL, 'unverified', 100, 10, NULL);
INSERT INTO sender_identities (workspace_id, id, connection_id, email) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a021', '00000000-0000-7000-8000-00000000a011', 'gmail@acme.test'),
  ('00000000-0000-7000-8000-00000000b000', '00000000-0000-7000-8000-00000000b020', '00000000-0000-7000-8000-00000000b010', 'mailbox@beta.test');
INSERT INTO people (workspace_id, id, email) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a030', 'p@example.com'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a031', 'q@example.com'),
  ('00000000-0000-7000-8000-00000000b000', '00000000-0000-7000-8000-00000000b030', 'r@example.com');
INSERT INTO campaigns (workspace_id, id, name) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a040', 'one'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a041', 'two');
INSERT INTO steps (workspace_id, id, campaign_id, position, name) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a050', '00000000-0000-7000-8000-00000000a040', 1, 'first');
INSERT INTO step_revisions (workspace_id, step_id, revision, delay_seconds, ranking_objective, observation_window_seconds, minimum_sample) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a050', 1, 0, 'opens', 3600, 10);
INSERT INTO variants (workspace_id, id, step_id, name) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a060', '00000000-0000-7000-8000-00000000a050', 'a'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a061', '00000000-0000-7000-8000-00000000a050', 'b');
INSERT INTO variant_revisions (workspace_id, variant_id, version, subject, html) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a060', 1, 's', '<p>a</p>'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a061', 1, 's', '<p>b</p>');
INSERT INTO step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a050', 1, '00000000-0000-7000-8000-00000000a060', 1);
INSERT INTO enrollments (workspace_id, id, campaign_id, person_id) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a070', '00000000-0000-7000-8000-00000000a041', '00000000-0000-7000-8000-00000000a030'),
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a071', '00000000-0000-7000-8000-00000000a040', '00000000-0000-7000-8000-00000000a030');
INSERT INTO provider_webhooks (workspace_id, id, connection_id, provider, name, signing_secret) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a0a0', '00000000-0000-7000-8000-00000000a010', 'norbelys', 'mta', '\x00');
INSERT INTO invitations (workspace_id, id, email, role, token_hash, invited_by, expires_at) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a0c0', 'invitee@example.com', 'member', '\x1c', '00000000-0000-7000-8000-00000000a001', now() + interval '7 days');
INSERT INTO api_keys (workspace_id, id, name, prefix, secret_hash, scopes, created_by) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a0d0', 'ci', 'nb_live_abcd', '\xa1', '{}', '00000000-0000-7000-8000-00000000a001');
INSERT INTO oauth_clients (client_id, kind, name, redirect_uris) VALUES
  ('https://client.example', 'cimd', 'Client', ARRAY['https://client.example/callback']);
INSERT INTO oauth_grants (id, client_id, user_id, workspace_id, resource, scopes, auth_method, authenticated_at, expires_at) VALUES
  ('00000000-0000-7000-8000-00000000a0f1', 'https://client.example', '00000000-0000-7000-8000-00000000a001', '00000000-0000-7000-8000-00000000a000', 'https://api.example', '{people:read}', 'passkey', now(), now() + interval '90 days'),
  ('00000000-0000-7000-8000-00000000a0f2', 'https://client.example', '00000000-0000-7000-8000-00000000a001', '00000000-0000-7000-8000-00000000a000', 'https://api.example', '{people:read}', 'passkey', now(), now() + interval '90 days');
INSERT INTO oauth_refresh_tokens (token_hash, grant_id, expires_at) VALUES
  ('\x01', '00000000-0000-7000-8000-00000000a0f1', now() + interval '1 day');
INSERT INTO webhook_endpoints (workspace_id, id, url, secret, event_types) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a090', 'https://acme.example/hooks', '\x5ec0', ARRAY['message.sent']),
  ('00000000-0000-7000-8000-00000000b000', '00000000-0000-7000-8000-00000000b090', 'https://beta.example/hooks', '\x5ec1', ARRAY['message.sent']);
INSERT INTO jobs (id, workspace_id, queue, kind, payload) VALUES
  ('00000000-0000-7000-8000-00000000a0b0', '00000000-0000-7000-8000-00000000a000', 'imports', 'people.import', '{"secret": "acme"}'),
  ('00000000-0000-7000-8000-00000000b0b0', '00000000-0000-7000-8000-00000000b000', 'imports', 'people.import', '{"secret": "beta"}');
INSERT INTO job_lanes (workspace_id, queue, max_running) VALUES
  ('00000000-0000-7000-8000-00000000a000', 'imports', 4),
  ('00000000-0000-7000-8000-00000000b000', 'imports', 4);
WITH m AS (
  INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, html, render_version, rendered_at, internet_message_id, send_at) VALUES
    ('00000000-0000-7000-8000-00000000a000', 'direct', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['p@example.com'], 's', '<p>x</p>', 'v1', now(), '<acme-1@acme.test>', now()),
    ('00000000-0000-7000-8000-00000000b000', 'direct', '00000000-0000-7000-8000-00000000b020', '00000000-0000-7000-8000-00000000b010', 'mailbox@beta.test', ARRAY['r@example.com'], 's', '<p>x</p>', 'v1', now(), '<beta-1@beta.test>', now())
  RETURNING workspace_id, id, connection_id, send_at)
INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at) SELECT workspace_id, id, connection_id, send_at FROM m;
INSERT INTO receive_bindings (workspace_id, id, connection_id) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a0e0', '00000000-0000-7000-8000-00000000a011');
INSERT INTO inbound_messages (workspace_id, id, receive_binding_id, connection_id, transport_identity, transport_key, received_at, classification, classification_source, evidence, review_requested_at, review_proposal) VALUES
  ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a0e1', '00000000-0000-7000-8000-00000000a0e0', '00000000-0000-7000-8000-00000000a011', '{"uid": 1}', '1', now(), 'address_change', 'rules', 'notice', now(),
   '{"action": "change_address", "email": "p@example.com", "new_email": "p2@example.com"}');
INSERT INTO idempotency_keys (workspace_id, user_id, key, fingerprint, expires_at) VALUES
  (NULL, '00000000-0000-7000-8000-00000000a001', 'own', '\x00', now() + interval '1 day'),
  (NULL, '00000000-0000-7000-8000-00000000b001', 'other', '\x00', now() + interval '1 day'),
  ('00000000-0000-7000-8000-00000000a000', NULL, 'workspace', '\x00', now() + interval '1 day');
"#;

/// Writes the shared rows through the system login.
async fn seed(test: &TestDb) {
    run(&test.system, SEED).await;
}

/// Runs statements of our own (fixed text, never input) through `db`'s pool, outside any
/// transaction of the test, and fails the test if they fail.
async fn run(db: &Database, sql: &str) {
    if let Err(error) = sqlx::raw_sql(AssertSqlSafe(sql)).execute(db.pool()).await {
        panic!("{sql}: {error}");
    }
}

/// Runs statements of our own on `executor` and answers how many rows they returned or touched,
/// or the SQLSTATE that refused them.
async fn outcome<'e>(executor: impl sqlx::PgExecutor<'e>, sql: &str) -> Result<u64, String> {
    match sqlx::raw_sql(AssertSqlSafe(sql)).execute(executor).await {
        Ok(done) => Ok(done.rows_affected()),
        Err(sqlx::Error::Database(error)) => {
            Err(error.code().map(Cow::into_owned).unwrap_or_default())
        }
        Err(error) => panic!("{sql}: {error}"),
    }
}

/// Who runs a statement: a login, and the context its transaction sets first.
#[derive(Debug, Clone, Copy)]
enum As {
    /// The api's login inside `acme`.
    App,
    /// The api's login with neither a workspace nor a user set.
    AppAlone,
    /// The api's login as `acme`'s owner, before a workspace is chosen.
    AppUser,
    /// The worker's login inside `acme`.
    Worker,
    /// The worker's login with no workspace set and no role switch.
    WorkerAlone,
    /// The worker's login switched to the scheduler role.
    Scheduler,
    /// The tracking drain's login.
    Tracking,
    /// The system login.
    System,
}

/// Runs `sql` as `who` in a transaction that is rolled back, so cases never see each other's
/// writes, and answers what [`outcome`] answers.
async fn attempt(test: &TestDb, who: As, sql: &str) -> Result<u64, String> {
    let db = match who {
        As::App | As::AppAlone | As::AppUser => &test.app,
        As::Worker | As::WorkerAlone | As::Scheduler => &test.worker,
        As::Tracking => &test.tracking,
        As::System => &test.system,
    };
    let mut tx = db.begin().await.unwrap();
    match who {
        As::App | As::Worker => db::set_workspace(&mut tx, WorkspaceId::trusted(ACME))
            .await
            .unwrap(),
        As::AppUser => db::set_user(&mut tx, ACME_OWNER).await.unwrap(),
        As::Scheduler => db::as_scheduler(&mut tx).await.unwrap(),
        As::AppAlone | As::WorkerAlone | As::Tracking | As::System => {}
    }
    let result = outcome(&mut *tx, sql).await;
    tx.rollback().await.unwrap();
    result
}

/// One expectation: what it shows, who runs the statement, the statement, and its outcome (the
/// rows returned or touched, or the SQLSTATE that refuses it).
type Case<'a> = (&'a str, As, &'a str, Result<u64, &'a str>);

/// Runs every case, each in its own rolled-back transaction, and names the first that differs.
async fn expect_all(test: &TestDb, cases: &[Case<'_>]) {
    for &(what, who, sql, expected) in cases {
        assert_eq!(
            attempt(test, who, sql).await,
            expected.map_err(str::to_owned),
            "{what}"
        );
    }
}

/// A count read through the system login.
async fn count(test: &TestDb, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(test.system.pool())
        .await
        .unwrap()
}

/// Row security covers every tenant table: each relation with a `workspace_id` column, parent or
/// leaf, has row security enabled and forced, with the api's and the worker's workspace policies,
/// except exactly the tables `rls_exceptions` lists. A query that forgets its workspace predicate
/// therefore still sees one workspace, and an exception is a reviewed row, never an omission.
/// Every table with `updated_at` has exactly one trigger maintaining it (no table forgets it, no
/// leaf gets it twice), and no partition grants INSERT, UPDATE or DELETE to anyone, so every write
/// goes through a parent and a detached leaf can be frozen. The owner's policies are read-only
/// and sit on exactly what the archive's detach checks read: every table on the referencing side
/// of a foreign key to a partitioned table, and every leaf of a referenced one; a foreign key
/// added later without its policy would let the archive drop a period still referenced.
#[tokio::test]
async fn row_security_and_triggers_cover_every_table_and_no_leaf_is_writable() {
    let test = TestDb::new().await;
    let unprotected: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
          WHERE c.relkind IN ('r', 'p')
            AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'workspace_id' AND NOT a.attisdropped)
            AND NOT (c.relrowsecurity AND c.relforcerowsecurity
                     AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid AND p.polname = 'workspace_isolation')
                     AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid AND p.polname = 'workspace_isolation_worker'))
          ORDER BY 1",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    let excepted: Vec<String> = sqlx::query_scalar(
        "SELECT e.table_name FROM rls_exceptions e
          WHERE EXISTS (SELECT 1 FROM information_schema.columns c
                         WHERE c.table_schema = 'public' AND c.table_name = e.table_name AND c.column_name = 'workspace_id')
          ORDER BY 1",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(unprotected, excepted);

    let mistriggered: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
          WHERE c.relkind IN ('r', 'p')
            AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'updated_at' AND NOT a.attisdropped)
            AND (SELECT count(*) FROM pg_trigger t WHERE t.tgrelid = c.oid AND t.tgfoid = 'set_updated_at'::regproc) <> 1",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(mistriggered, Vec::<String>::new());

    let writable_leaves: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text || ' ' || a.privilege_type
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public', aclexplode(c.relacl) a
          WHERE c.relispartition AND c.relkind = 'r' AND a.privilege_type IN ('INSERT', 'UPDATE', 'DELETE')
            AND a.grantee <> c.relowner",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(writable_leaves, Vec::<String>::new());

    let misplaced_owner_policies: Vec<String> = sqlx::query_scalar(
        "WITH referenced AS (
              SELECT DISTINCT c.conrelid AS referencing, c.confrelid AS parent
                FROM pg_constraint c JOIN pg_class p ON p.oid = c.confrelid
               WHERE c.contype = 'f' AND p.relkind = 'p' AND c.conparentid = 0),
         checked AS (
              SELECT referencing AS relid FROM referenced
              UNION SELECT i.inhrelid FROM pg_inherits i JOIN referenced r ON r.parent = i.inhparent),
         owner_policies AS (
              SELECT polrelid AS relid, polcmd FROM pg_policy WHERE 'norbelys_owner'::regrole = ANY (polroles))
         SELECT relid::regclass::text || ' lacks its read-only owner policy' FROM checked
          WHERE relid NOT IN (SELECT relid FROM owner_policies WHERE polcmd = 'r')
         UNION ALL
         SELECT relid::regclass::text || ' has an owner policy no detach check needs' FROM owner_policies
          WHERE polcmd <> 'r' OR relid NOT IN (SELECT relid FROM checked)",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(misplaced_owner_policies, Vec::<String>::new());
}

/// Every login that names a day runs in UTC, so a stray `current_date` can never read another
/// time zone's day; the logins that write increments run under a transaction timeout (30 s for the
/// api and the tracker, 60 s for the worker), the bound that keeps every increment above the
/// rollup's 120-second watermark.
#[tokio::test]
async fn the_logins_run_in_utc_under_their_transaction_bounds() {
    let test = TestDb::new().await;
    for (login, db, bound) in [
        ("app", &test.app, Some("30s")),
        ("worker", &test.worker, Some("1min")),
        ("tracking", &test.tracking, Some("30s")),
        ("system", &test.system, None),
    ] {
        let (zone, timeout): (String, String) = sqlx::query_as(
            "SELECT current_setting('TimeZone'), current_setting('transaction_timeout')",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(zone, "UTC", "{login}");
        if let Some(bound) = bound {
            assert_eq!(timeout, bound, "{login}");
        }
    }
}

/// A transaction tightens its login's bound with `db::set_transaction_timeout`, and the tighter
/// bound holds although the worker's own minute is already running: PostgreSQL ignores a new
/// `transaction_timeout` while a timer runs, so a plain `SET LOCAL` would let a delivery Start hold
/// the locks every removal waits on for a minute instead of ten seconds. The next transaction of
/// the login is back under its own bound.
#[tokio::test]
async fn a_transaction_tightens_its_logins_bound() {
    let test = TestDb::new().await;
    let mut tx = test.worker.begin().await.unwrap();
    db::set_transaction_timeout(&mut tx, Duration::from_millis(200))
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let slept = sqlx::query("SELECT pg_sleep(5)").execute(&mut *tx).await;
    assert!(slept.is_err(), "the bound ended the transaction");
    assert!(started.elapsed() < Duration::from_secs(4));
    drop(tx);

    let timeout: String = sqlx::query_scalar("SELECT current_setting('transaction_timeout')")
        .fetch_one(test.worker.pool())
        .await
        .unwrap();
    assert_eq!(timeout, "1min");
}

/// The partition helper bounds every leaf in UTC whatever the session's time zone, and a period
/// change never makes two leaves overlap. Under a Bogota session a day leaf still runs from UTC
/// midnight to UTC midnight. After a change to monthly periods an instant already covered keeps its
/// day leaf, the next leaf starts where the day leaf ends and runs to the month's end, and a leaf
/// before the day leaf is clipped to it; after the change back, an instant inside the month leaf
/// keeps it and the first day after it gets a day leaf; a leaf earlier than every other is a plain
/// period. Across every table no two leaves overlap, and each leaf's real bounds are exactly the
/// uuidv7 boundaries of the instants it records, boundaries that carry the instant's unix
/// milliseconds; a timestamptz table's bounds are whole UTC months; and a table without a policy
/// is refused by name.
#[tokio::test]
async fn the_partition_helper_bounds_leaves_in_utc_across_period_changes() {
    async fn ensure(tx: &mut db::Tx, table: &str, at: &str) -> String {
        sqlx::query_scalar("SELECT ensure_partition($1::regclass, $2::timestamptz)")
            .bind(table)
            .bind(at)
            .fetch_one(&mut **tx)
            .await
            .unwrap()
    }
    async fn bounds(tx: &mut db::Tx, leaf: &str) -> (String, String) {
        sqlx::query_as(
            "SELECT to_char(lower AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'), to_char(upper AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI')
               FROM partition_leaves WHERE name = $1",
        )
        .bind(leaf)
        .fetch_one(&mut **tx)
        .await
        .unwrap()
    }
    let span = |lower: &str, upper: &str| (lower.to_owned(), upper.to_owned());
    let test = TestDb::new().await;
    let mut tx = test.system.begin().await.unwrap();
    outcome(&mut *tx, "SET LOCAL timezone = 'America/Bogota'")
        .await
        .unwrap();
    let day = "message_engagement_20311211";
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2031-12-10 23:30-05").await,
        day
    );
    assert_eq!(
        bounds(&mut tx, day).await,
        span("2031-12-11 00:00", "2031-12-12 00:00")
    );

    outcome(
        &mut *tx,
        "UPDATE partition_policies SET period = interval '1 month' WHERE table_name = 'message_engagement'",
    )
    .await
    .unwrap();
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2031-12-11 12:00+00").await,
        day
    );
    let month = "message_engagement_20311212";
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2031-12-25 00:00+00").await,
        month
    );
    assert_eq!(
        bounds(&mut tx, month).await,
        span("2031-12-12 00:00", "2032-01-01 00:00")
    );
    let before = "message_engagement_20311201";
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2031-12-05 00:00+00").await,
        before
    );
    assert_eq!(
        bounds(&mut tx, before).await,
        span("2031-12-01 00:00", "2031-12-11 00:00")
    );

    outcome(
        &mut *tx,
        "UPDATE partition_policies SET period = interval '1 day' WHERE table_name = 'message_engagement'",
    )
    .await
    .unwrap();
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2031-12-20 00:00+00").await,
        month
    );
    let after = "message_engagement_20320101";
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2032-01-01 05:00+00").await,
        after
    );
    assert_eq!(
        bounds(&mut tx, after).await,
        span("2032-01-01 00:00", "2032-01-02 00:00")
    );
    let earliest = "message_engagement_20260515";
    assert_eq!(
        ensure(&mut tx, "message_engagement", "2026-05-15 12:00+00").await,
        earliest
    );
    assert_eq!(
        bounds(&mut tx, earliest).await,
        span("2026-05-15 00:00", "2026-05-16 00:00")
    );

    let overlapping: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM partition_leaves a JOIN partition_leaves b
             ON a.parent = b.parent AND a.name < b.name AND a.lower < b.upper AND b.lower < a.upper",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(overlapping, 0);
    let misbounded: Vec<String> = sqlx::query_scalar(
        "SELECT l.name FROM partition_leaves l
           JOIN partition_policies p ON p.table_name = l.parent AND p.key_kind = 'uuidv7'
           JOIN pg_class c ON c.relname = l.name
          WHERE pg_get_expr(c.relpartbound, c.oid) <> format('FOR VALUES FROM (%L) TO (%L)', uuidv7_boundary(l.lower), uuidv7_boundary(l.upper))",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(misbounded, Vec::<String>::new());
    let (boundary, instant): (String, bool) = sqlx::query_as(
        "SELECT uuidv7_boundary('2031-12-11 00:00:00.123+00')::text,
                uuid_extract_timestamp(uuidv7_boundary('2031-12-11 00:00:00.123+00')) = '2031-12-11 00:00:00.123+00'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(instant, "{boundary}");
    assert!(boundary.ends_with("-7000-8000-000000000000"), "{boundary}");

    outcome(&mut *tx, "SET LOCAL timezone = 'UTC'")
        .await
        .unwrap();
    let tracking = "tracking_events_20311101";
    assert_eq!(
        ensure(&mut tx, "tracking_events", "2031-11-03 00:00+00").await,
        tracking
    );
    let partition: String = sqlx::query_scalar(
        "SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = $1",
    )
    .bind(tracking)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        partition,
        "FOR VALUES FROM ('2031-11-01 00:00:00+00') TO ('2031-12-01 00:00:00+00')"
    );
    assert_eq!(
        outcome(&mut *tx, "SELECT ensure_partition('people', now())").await,
        Err("P0001".to_owned())
    );
    tx.rollback().await.unwrap();
}

/// The daily maintenance run keeps the next period's partition ready whatever the period. At 12:00
/// UTC on 30 January, two days ahead, a monthly table (one set monthly here, and the tracking
/// events, monthly by default) gets January's and February's leaves, and a daily table exactly the
/// 30th, the 31st and the 1st; a tracking event at the first second of February is then accepted.
/// Stepping the two days a month at a time would have stopped at January, refusing every insert
/// from midnight until the next day's run.
#[tokio::test]
async fn partitions_ahead_include_next_month_in_a_months_last_days() {
    let test = TestDb::new().await;
    run(
        &test.system,
        "UPDATE partition_policies SET period = interval '1 month' WHERE table_name = 'delivery_events'",
    )
    .await;
    let ensured: Vec<String> = sqlx::query_scalar(
        "SELECT ensure_partitions_ahead(interval '2 days', '2031-01-30 12:00+00')",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    let leaves: Vec<String> = sqlx::query_scalar(
        "SELECT parent || ' ' || to_char(lower AT TIME ZONE 'UTC', 'YYYY-MM-DD') || ' ' || to_char(upper AT TIME ZONE 'UTC', 'YYYY-MM-DD')
           FROM partition_leaves
          WHERE name = ANY ($1) AND parent IN ('delivery_events', 'messages', 'tracking_events')
          ORDER BY 1",
    )
    .bind(&ensured)
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        leaves,
        [
            "delivery_events 2031-01-01 2031-02-01",
            "delivery_events 2031-02-01 2031-03-01",
            "messages 2031-01-30 2031-01-31",
            "messages 2031-01-31 2031-02-01",
            "messages 2031-02-01 2031-02-02",
            "tracking_events 2031-01-01 2031-02-01",
            "tracking_events 2031-02-01 2031-03-01",
        ]
    );
    assert_eq!(
        attempt(
            &test,
            As::Tracking,
            "INSERT INTO tracking_events (workspace_id, id, message_id, kind, actor_class, occurred_at)
             VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7(), uuidv7(), 'open', 'human', '2031-02-01 00:00:01+00')",
        )
        .await,
        Ok(1)
    );
}

/// A campaign message proves its whole ancestry, link by link, even against parents of its own
/// workspace: an enrollment of another campaign, a variant the step's revision does not offer and
/// a person who is not the enrolled one are refused by the composite foreign keys, as are a winner
/// and an assignment outside the revision's options and an identity of another connection; a
/// direct message carrying campaign columns is refused by its kind's CHECK, and a person still
/// enrolled cannot be deleted. The same message with its true parents is accepted.
#[tokio::test]
async fn a_campaign_message_proves_its_ancestry_link_by_link() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("a campaign message with its true parents", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, render_version, rendered_at, internet_message_id, send_at, campaign_id, step_id, step_revision, variant_id, variant_version, enrollment_id, person_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'campaign', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['p@example.com'], 's', 'v1', now(), '<c@acme.test>', now(),
                  '00000000-0000-7000-8000-00000000a040', '00000000-0000-7000-8000-00000000a050', 1, '00000000-0000-7000-8000-00000000a060', 1, '00000000-0000-7000-8000-00000000a071', '00000000-0000-7000-8000-00000000a030')",
         Ok(1)),
        ("an enrollment of another campaign", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, render_version, rendered_at, internet_message_id, send_at, campaign_id, step_id, step_revision, variant_id, variant_version, enrollment_id, person_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'campaign', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['p@example.com'], 's', 'v1', now(), '<c@acme.test>', now(),
                  '00000000-0000-7000-8000-00000000a040', '00000000-0000-7000-8000-00000000a050', 1, '00000000-0000-7000-8000-00000000a060', 1, '00000000-0000-7000-8000-00000000a070', '00000000-0000-7000-8000-00000000a030')",
         Err(FOREIGN_KEY)),
        ("a variant the revision does not offer", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, render_version, rendered_at, internet_message_id, send_at, campaign_id, step_id, step_revision, variant_id, variant_version, enrollment_id, person_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'campaign', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['p@example.com'], 's', 'v1', now(), '<c@acme.test>', now(),
                  '00000000-0000-7000-8000-00000000a040', '00000000-0000-7000-8000-00000000a050', 1, '00000000-0000-7000-8000-00000000a061', 1, '00000000-0000-7000-8000-00000000a071', '00000000-0000-7000-8000-00000000a030')",
         Err(FOREIGN_KEY)),
        ("a person who is not the enrolled one", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, render_version, rendered_at, internet_message_id, send_at, campaign_id, step_id, step_revision, variant_id, variant_version, enrollment_id, person_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'campaign', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['q@example.com'], 's', 'v1', now(), '<c@acme.test>', now(),
                  '00000000-0000-7000-8000-00000000a040', '00000000-0000-7000-8000-00000000a050', 1, '00000000-0000-7000-8000-00000000a060', 1, '00000000-0000-7000-8000-00000000a071', '00000000-0000-7000-8000-00000000a031')",
         Err(FOREIGN_KEY)),
        ("a direct message carrying a campaign", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, html, render_version, rendered_at, internet_message_id, send_at, campaign_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'direct', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['p@example.com'], 's', '<p>x</p>', 'v1', now(), '<d@acme.test>', now(),
                  '00000000-0000-7000-8000-00000000a040')",
         Err(CHECK)),
        ("an identity of another connection", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, html, render_version, rendered_at, internet_message_id, send_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'direct', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a011', 'mta@acme.test', ARRAY['p@example.com'], 's', '<p>x</p>', 'v1', now(), '<e@acme.test>', now())",
         Err(FOREIGN_KEY)),
        ("the revision's winner among its options", As::System,
         "UPDATE step_revisions SET winner_variant_id = '00000000-0000-7000-8000-00000000a060', winner_variant_version = 1, winner_selected_at = now()
           WHERE step_id = '00000000-0000-7000-8000-00000000a050'",
         Ok(1)),
        ("a winner outside the revision's options", As::System,
         "UPDATE step_revisions SET winner_variant_id = '00000000-0000-7000-8000-00000000a061', winner_variant_version = 1, winner_selected_at = now()
           WHERE step_id = '00000000-0000-7000-8000-00000000a050'",
         Err(FOREIGN_KEY)),
        ("an assignment to a variant outside the revision", As::System,
         "INSERT INTO step_assignments (workspace_id, step_id, step_revision, person_id, variant_id, variant_version)
          VALUES ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a050', 1, '00000000-0000-7000-8000-00000000a030', '00000000-0000-7000-8000-00000000a061', 1)",
         Err(FOREIGN_KEY)),
        ("deleting a person still enrolled", As::System,
         "DELETE FROM people WHERE id = '00000000-0000-7000-8000-00000000a030'",
         Err(RESTRICT)),
    ])
    .await;
}

/// Identity rows keep their invariants. A refresh chain is one line per grant: one root, one child
/// per parent, every parent of the same grant, so the reuse of a consumed token is traced to its
/// chain, which is revoked whole. A confidential client has a secret, and a client known by its
/// metadata document never has one. A ceremony that binds something to a user (registering a
/// passkey) names that user, while one anyone may start (an email code) needs none.
#[tokio::test]
async fn identity_rows_keep_their_chains_and_kinds() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("the root's one child", As::System,
         "INSERT INTO oauth_refresh_tokens (token_hash, grant_id, rotated_from, expires_at)
          VALUES ('\\x02', '00000000-0000-7000-8000-00000000a0f1', '\\x01', now() + interval '1 day')",
         Ok(1)),
        ("a second child of one parent", As::System,
         "INSERT INTO oauth_refresh_tokens (token_hash, grant_id, rotated_from, expires_at)
          VALUES ('\\x02', '00000000-0000-7000-8000-00000000a0f1', '\\x01', now() + interval '1 day'),
                 ('\\x03', '00000000-0000-7000-8000-00000000a0f1', '\\x01', now() + interval '1 day')",
         Err(UNIQUE)),
        ("a second root for one grant", As::System,
         "INSERT INTO oauth_refresh_tokens (token_hash, grant_id, expires_at)
          VALUES ('\\x04', '00000000-0000-7000-8000-00000000a0f1', now() + interval '1 day')",
         Err(UNIQUE)),
        ("a parent of another grant", As::System,
         "INSERT INTO oauth_refresh_tokens (token_hash, grant_id, rotated_from, expires_at)
          VALUES ('\\x05', '00000000-0000-7000-8000-00000000a0f2', '\\x01', now() + interval '1 day')",
         Err(FOREIGN_KEY)),
        ("a confidential client with its secret", As::System,
         "INSERT INTO oauth_clients (client_id, kind, name, redirect_uris, auth_method, secret_hash)
          VALUES ('registered-1', 'registered', 'Registered', ARRAY['https://registered.example/callback'], 'client_secret_basic', '\\x01')",
         Ok(1)),
        ("a confidential client without a secret", As::System,
         "INSERT INTO oauth_clients (client_id, kind, name, redirect_uris, auth_method)
          VALUES ('registered-1', 'registered', 'Registered', ARRAY['https://registered.example/callback'], 'client_secret_basic')",
         Err(CHECK)),
        ("a metadata-document client with a secret", As::System,
         "INSERT INTO oauth_clients (client_id, kind, name, redirect_uris, auth_method, secret_hash)
          VALUES ('https://other.example', 'cimd', 'Other', ARRAY['https://other.example/callback'], 'client_secret_basic', '\\x01')",
         Err(CHECK)),
        ("an email-code ceremony started by anyone", As::System,
         "INSERT INTO auth_ceremonies (kind, browser_hash, state, expires_at) VALUES ('email_code', '\\x01', '\\x01', now() + interval '10 minutes')",
         Ok(1)),
        ("a passkey registration without its user", As::System,
         "INSERT INTO auth_ceremonies (kind, browser_hash, state, expires_at) VALUES ('passkey_registration', '\\x01', '\\x01', now() + interval '10 minutes')",
         Err(CHECK)),
    ])
    .await;
}

/// The rules of connections hold in the database. Every mailbox is paced, at an interval of at
/// least five minutes that nothing defaults; managed delivery supports exact minutes, SendGrid refuses pacing, while
/// SES may be, one connection per From address. An SES connection names its account's quota scope
/// and a valid configuration set, so the scope cannot be deleted under it. Within a workspace an
/// address is one live paced sender whatever the way in, and an account one live connection, while
/// another workspace may connect the same mailbox and a different account may take an archived
/// connection's address. An OAuth subject comes with its issuer and is unique, and an archived
/// connection holds no credential.
#[tokio::test]
async fn connections_keep_their_pacing_scope_and_account_rules() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("a mailbox interval below five minutes", As::System,
         "UPDATE connections SET send_interval_minutes = 4 WHERE id = '00000000-0000-7000-8000-00000000a011'",
         Err(CHECK)),
        ("a mailbox without an interval", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit, send_interval_minutes)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'google', 'api', 'nointerval@acme.test', 'active', 100, NULL)",
         Err(CHECK)),
        ("a mailbox whose interval is left to a default", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'microsoft', 'api', 'omitted@acme.test', 'active', 100)",
         Err(CHECK)),
        ("the managed MTA with an exact interval", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, send_interval_minutes)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'norbelys', 'smtp', 'mta2@acme.test', '{}', 'active', 100000, 3)",
         Ok(1)),
        ("a SendGrid connection with an interval", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, send_interval_minutes)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'sendgrid', 'smtp', 'carol@acme.test', '{}', 'active', 40, 10)",
         Err(CHECK)),
        ("two paced SES senders on one SES account, one per From address", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, send_interval_minutes, quota_scope_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'ses', 'smtp', 'anna@acme.test', '{\"configuration_set\": \"acme-events\"}', 'active', 40, 10, '00000000-0000-7000-8000-00000000a081'),
                 ('00000000-0000-7000-8000-00000000a000', 'ses', 'smtp', 'bob@acme.test', '{\"configuration_set\": \"acme-events\"}', 'active', 40, 10, '00000000-0000-7000-8000-00000000a081')",
         Ok(2)),
        ("an SES connection without a quota scope", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'ses', 'smtp', 'noscope@acme.test', '{\"configuration_set\": \"acme-events\"}', 'active', 100)",
         Err(CHECK)),
        ("an SES connection without a configuration set", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, quota_scope_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'ses', 'smtp', 'noset@acme.test', '{}', 'active', 100, '00000000-0000-7000-8000-00000000a081')",
         Err(CHECK)),
        ("an SES connection whose configuration set is null", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, quota_scope_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'ses', 'smtp', 'nullset@acme.test', '{\"configuration_set\": null}', 'active', 100, '00000000-0000-7000-8000-00000000a081')",
         Err(CHECK)),
        ("deleting the quota scope an SES connection names", As::System,
         "DELETE FROM quota_scopes WHERE id = '00000000-0000-7000-8000-00000000a081'",
         Err(CHECK)),
        ("a mailbox for the address of a paced SES sender", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, send_interval_minutes, quota_scope_id)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'ses', 'smtp', 'anna@acme.test', '{\"configuration_set\": \"acme-events\"}', 'active', 40, 10, '00000000-0000-7000-8000-00000000a081');
          INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit, send_interval_minutes)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'google', 'api', 'Anna@acme.test', 'active', 100, 10)",
         Err(UNIQUE)),
        ("a mailbox's address again by a password", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit, send_interval_minutes)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'smtp', 'smtp', 'Gmail@acme.test', '{}', 'unverified', 100, 10)",
         Err(UNIQUE)),
        ("the same mailbox in another workspace", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit, send_interval_minutes)
          VALUES ('00000000-0000-7000-8000-00000000b000', 'google', 'api', 'gmail@acme.test', 'active', 100, 10)",
         Ok(1)),
        ("a second live connection for one account", As::System,
         "INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'norbelys', 'smtp', 'MTA@acme.test', '{}', 'active', 100)",
         Err(UNIQUE)),
        ("an issuer without a subject", As::System,
         "UPDATE connections SET account_issuer = 'https://accounts.google.com' WHERE id = '00000000-0000-7000-8000-00000000a011'",
         Err(CHECK)),
        ("one provider subject on two connections", As::System,
         "UPDATE connections SET account_issuer = 'https://accounts.google.com', account_subject = '1001' WHERE id = '00000000-0000-7000-8000-00000000a011';
          INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit, send_interval_minutes, account_issuer, account_subject)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'google', 'api', 'other@acme.test', 'active', 100, 10, 'https://accounts.google.com', '1001')",
         Err(UNIQUE)),
        ("another account taking an archived connection's address", As::System,
         "UPDATE connections SET status = 'archived' WHERE id = '00000000-0000-7000-8000-00000000a011';
          INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit, send_interval_minutes, account_issuer, account_subject)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'google', 'api', 'gmail@acme.test', 'active', 100, 10, 'https://accounts.google.com', '2002')",
         Ok(2)),
        ("archiving a connection that still holds its credential", As::System,
         "UPDATE connections SET status = 'archived' WHERE id = '00000000-0000-7000-8000-00000000a010'",
         Err(CHECK)),
        ("archiving it with its credential erased", As::System,
         "UPDATE connections SET status = 'archived', credential = NULL WHERE id = '00000000-0000-7000-8000-00000000a010'",
         Ok(1)),
    ])
    .await;
}

/// Rows of delivery and the inbox keep their shapes: a message that is not campaign mail carries
/// its own body; a Message-ID directory row belongs to a thread of its workspace, so a reply that
/// names the id resolves to a real conversation; and a review decision has its time and a review
/// proposal its request.
#[tokio::test]
async fn messages_directory_rows_and_reviews_keep_their_shapes() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("a direct message without a body", As::System,
         "INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, render_version, rendered_at, internet_message_id, send_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'direct', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['p@example.com'], 's', 'v1', now(), '<f@acme.test>', now())",
         Err(CHECK)),
        ("a directory row for a thread of its workspace", As::System,
         "INSERT INTO threads (workspace_id, id, sender_identity_id) VALUES ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a0f3', '00000000-0000-7000-8000-00000000a020');
          INSERT INTO message_id_directory (workspace_id, lookup_key, message_id, thread_id, recipients)
          VALUES ('00000000-0000-7000-8000-00000000a000', '<x@y.test>', uuidv7(), '00000000-0000-7000-8000-00000000a0f3', ARRAY['p@example.com'])",
         Ok(2)),
        ("a directory row for a thread of nowhere", As::System,
         "INSERT INTO message_id_directory (workspace_id, lookup_key, message_id, thread_id, recipients)
          VALUES ('00000000-0000-7000-8000-00000000a000', '<x@y.test>', uuidv7(), uuidv7(), ARRAY['p@example.com'])",
         Err(FOREIGN_KEY)),
        ("a review decision without its time", As::System,
         "UPDATE inbound_messages SET review_decision = 'confirmed' WHERE id = '00000000-0000-7000-8000-00000000a0e1'",
         Err(CHECK)),
        ("a proposal nobody asked to review", As::System,
         "UPDATE inbound_messages SET review_requested_at = NULL WHERE id = '00000000-0000-7000-8000-00000000a0e1'",
         Err(CHECK)),
        ("a confirmed review with its time", As::System,
         "UPDATE inbound_messages SET review_decision = 'confirmed', reviewed_at = now() WHERE id = '00000000-0000-7000-8000-00000000a0e1'",
         Ok(1)),
    ])
    .await;
}

/// An idempotency key belongs to a workspace or to a user, never both nor neither, and is unique
/// within its scope only; and a stored response is never one the client should retry (429 or any
/// 5xx), so a retry after a transient failure runs again instead of replaying the failure.
#[tokio::test]
async fn idempotency_keys_keep_one_scope_and_no_retryable_response() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("a key of both a workspace and a user", As::System,
         "INSERT INTO idempotency_keys (workspace_id, user_id, key, fingerprint, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a001', 'k', '\\x00', now() + interval '1 day')",
         Err(CHECK)),
        ("a key of neither", As::System,
         "INSERT INTO idempotency_keys (key, fingerprint, expires_at) VALUES ('k', '\\x00', now() + interval '1 day')",
         Err(CHECK)),
        ("a stored 409", As::System,
         "INSERT INTO idempotency_keys (workspace_id, key, fingerprint, response_status, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'k', '\\x00', 409, now() + interval '1 day')",
         Ok(1)),
        ("a stored 503", As::System,
         "INSERT INTO idempotency_keys (workspace_id, key, fingerprint, response_status, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'k', '\\x00', 503, now() + interval '1 day')",
         Err(CHECK)),
        ("a stored 429", As::System,
         "INSERT INTO idempotency_keys (workspace_id, key, fingerprint, response_status, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'k', '\\x00', 429, now() + interval '1 day')",
         Err(CHECK)),
        ("one key twice in a workspace", As::System,
         "INSERT INTO idempotency_keys (workspace_id, key, fingerprint, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', 'k', '\\x00', now() + interval '1 day'),
                 ('00000000-0000-7000-8000-00000000a000', 'k', '\\x00', now() + interval '1 day')",
         Err(UNIQUE)),
        ("one key twice for a user", As::System,
         "INSERT INTO idempotency_keys (user_id, key, fingerprint, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a001', 'k', '\\x00', now() + interval '1 day'),
                 ('00000000-0000-7000-8000-00000000a001', 'k', '\\x00', now() + interval '1 day')",
         Err(UNIQUE)),
        ("one key in two workspaces and for a user", As::System,
         "INSERT INTO idempotency_keys (workspace_id, user_id, key, fingerprint, expires_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', NULL, 'k', '\\x00', now() + interval '1 day'),
                 ('00000000-0000-7000-8000-00000000b000', NULL, 'k', '\\x00', now() + interval '1 day'),
                 (NULL, '00000000-0000-7000-8000-00000000a001', 'k', '\\x00', now() + interval '1 day')",
         Ok(3)),
    ])
    .await;
}

/// A provider event is stored once, however often and however concurrently it arrives: when two
/// sessions insert the same key at once, the second waits for the first to commit and then inserts
/// nothing, and a later replay inserts nothing either, so copies of one callback can never both
/// become receipts. The key is global because the table is hash-partitioned on it.
#[tokio::test]
async fn a_provider_event_is_stored_once_even_when_two_copies_race() {
    const INSERT: &str = "INSERT INTO provider_event_keys (provider_webhook_id, event_id, body_hash)
                          VALUES ('00000000-0000-7000-8000-00000000a0a0', 'evt-1', '\\x01') ON CONFLICT DO NOTHING";
    let test = TestDb::new().await;
    let mut first = test.app.begin().await.unwrap();
    assert_eq!(outcome(&mut *first, INSERT).await, Ok(1));
    let racing = test.app.clone();
    let second = tokio::spawn(async move { outcome(racing.pool(), INSERT).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "the second copy waits for the first to commit"
    );
    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap(), Ok(0));
    assert_eq!(outcome(test.app.pool(), INSERT).await, Ok(0));
    assert_eq!(
        count(&test, "SELECT count(*) FROM provider_event_keys").await,
        1
    );
}

/// Evidence always has a home: delivery events are partitioned on their own id, the time they are
/// recorded, not on the message they name, so an event about a message whose period has no
/// partition any more (archived long ago) lands in the leaf of now.
#[tokio::test]
async fn late_evidence_lands_in_the_current_partition() {
    let test = TestDb::new().await;
    let home: bool = sqlx::query_scalar(
        "WITH e AS (INSERT INTO delivery_events (workspace_id, message_id, source, source_event_id, kind, category, confidence, observed_at, recipient_ref)
                    VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-06-15'), 'dsn', 'late-1', 'bounced', 'invalid_recipient', 'inferred', now(), 'unknown')
                    RETURNING tableoid)
         SELECT l.lower <= now() AND now() < l.upper FROM e JOIN partition_leaves l ON l.name = e.tableoid::regclass::text",
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert!(home);
}

/// The three lookups that run before a workspace is known (a provider webhook's workspace, an
/// invitation by its token, an API key by its hash) resolve their row for the api's login with no
/// workspace set, while the tables behind them stay dark to that same login; and no other login
/// may run them. They are owned by a role whose only grants are those lookups' columns, so a
/// lookup cannot become a way around row security.
#[tokio::test]
async fn lookups_resolve_before_any_workspace_for_the_api_alone() {
    let test = TestDb::new().await;
    seed(&test).await;
    let webhook = "SELECT 1 WHERE provider_webhook_workspace('00000000-0000-7000-8000-00000000a0a0') = '00000000-0000-7000-8000-00000000a000'";
    let invitation = "SELECT 1 FROM invitation_by_token('\\x1c') WHERE workspace_id = '00000000-0000-7000-8000-00000000a000'";
    let api_key = "SELECT 1 FROM api_key_by_hash('\\xa1') WHERE workspace_id = '00000000-0000-7000-8000-00000000a000'";
    expect_all(
        &test,
        &[
            ("the webhook's workspace", As::AppAlone, webhook, Ok(1)),
            (
                "the invitation by its token",
                As::AppAlone,
                invitation,
                Ok(1),
            ),
            ("the API key by its hash", As::AppAlone, api_key, Ok(1)),
            (
                "the webhooks themselves",
                As::AppAlone,
                "SELECT id FROM provider_webhooks",
                Ok(0),
            ),
            (
                "the invitations themselves",
                As::AppAlone,
                "SELECT id FROM invitations",
                Ok(0),
            ),
            (
                "the API keys themselves",
                As::AppAlone,
                "SELECT id FROM api_keys",
                Ok(0),
            ),
            (
                "the webhook lookup as the tracker",
                As::Tracking,
                webhook,
                Err(DENIED),
            ),
            (
                "the invitation lookup as the tracker",
                As::Tracking,
                invitation,
                Err(DENIED),
            ),
            (
                "the API key lookup as the tracker",
                As::Tracking,
                api_key,
                Err(DENIED),
            ),
            (
                "the webhook lookup as the worker",
                As::WorkerAlone,
                webhook,
                Err(DENIED),
            ),
            (
                "the invitation lookup as the worker",
                As::WorkerAlone,
                invitation,
                Err(DENIED),
            ),
            (
                "the API key lookup as the worker",
                As::WorkerAlone,
                api_key,
                Err(DENIED),
            ),
        ],
    )
    .await;
}

/// Before a workspace is chosen, a signed-in user sees exactly their own memberships and their own
/// user-scoped idempotency keys, and nothing of any workspace: listing my workspaces and creating
/// one work from the session alone, while every tenant table stays dark.
#[tokio::test]
async fn a_user_without_a_workspace_sees_only_their_own_rows() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(
        &test,
        &[
            (
                "their memberships",
                As::AppUser,
                "SELECT workspace_id FROM memberships",
                Ok(1),
            ),
            (
                "their idempotency keys, not another user's nor a workspace's",
                As::AppUser,
                "SELECT key FROM idempotency_keys",
                Ok(1),
            ),
            (
                "the people of any workspace",
                As::AppUser,
                "SELECT id FROM people",
                Ok(0),
            ),
        ],
    )
    .await;
}

/// Background roles reach a sealed secret only through accessors that answer inside the current
/// workspace. Inside `acme` the worker reads `acme`'s connection credential and endpoint secret,
/// and replacing the credential bumps its version; asked for `beta`'s, each accessor answers NULL
/// and changes nothing. A temporary table named `connections` cannot shadow the real one inside
/// the accessor, whose search path puts `pg_temp` last. Outside any workspace the accessors answer
/// NULL and the sealed column itself cannot even be selected.
#[tokio::test]
async fn secret_accessors_answer_only_inside_the_current_workspace() {
    async fn credential(tx: &mut db::Tx, workspace: Uuid, connection: Uuid) -> Option<Vec<u8>> {
        sqlx::query_scalar("SELECT connection_credential($1, $2)")
            .bind(workspace)
            .bind(connection)
            .fetch_one(&mut **tx)
            .await
            .unwrap()
    }
    async fn replace(tx: &mut db::Tx, workspace: Uuid, connection: Uuid) -> Option<i64> {
        sqlx::query_scalar("SELECT set_connection_credential($1, $2, '\\xfeed')")
            .bind(workspace)
            .bind(connection)
            .fetch_one(&mut **tx)
            .await
            .unwrap()
    }
    async fn secret(tx: &mut db::Tx, workspace: Uuid, endpoint: Uuid) -> Option<Vec<u8>> {
        sqlx::query_scalar("SELECT webhook_endpoint_secret($1, $2)")
            .bind(workspace)
            .bind(endpoint)
            .fetch_one(&mut **tx)
            .await
            .unwrap()
    }
    let test = TestDb::new().await;
    seed(&test).await;
    let mut tx = test
        .worker
        .begin_in(WorkspaceId::trusted(ACME))
        .await
        .unwrap();
    assert_eq!(
        credential(&mut tx, ACME, ACME_MTA).await,
        Some(vec![0xde, 0xad, 0xbe, 0xef])
    );
    assert_eq!(credential(&mut tx, BETA, BETA_MAILBOX).await, None);
    assert_eq!(replace(&mut tx, ACME, ACME_MTA).await, Some(2));
    assert_eq!(replace(&mut tx, BETA, BETA_MAILBOX).await, None);
    assert_eq!(
        secret(&mut tx, ACME, ACME_ENDPOINT).await,
        Some(vec![0x5e, 0xc0])
    );
    assert_eq!(secret(&mut tx, BETA, BETA_ENDPOINT).await, None);
    outcome(
        &mut *tx,
        "CREATE TEMP TABLE connections (workspace_id uuid, id uuid, credential bytea) ON COMMIT DROP;
         INSERT INTO pg_temp.connections VALUES ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a010', '\\x0bad')",
    )
    .await
    .unwrap();
    assert_eq!(
        credential(&mut tx, ACME, ACME_MTA).await,
        Some(vec![0xfe, 0xed])
    );
    tx.commit().await.unwrap();
    assert_eq!(
        count(
            &test,
            "SELECT count(*) FROM connections WHERE id = '00000000-0000-7000-8000-00000000b010' AND credential = '\\xcafe' AND credential_version = 1"
        )
        .await,
        1
    );

    let mut alone = test.worker.begin().await.unwrap();
    assert_eq!(credential(&mut alone, ACME, ACME_MTA).await, None);
    alone.rollback().await.unwrap();
    assert_eq!(
        attempt(&test, As::WorkerAlone, "SELECT credential FROM connections").await,
        Err(DENIED.to_owned())
    );
}

/// The api's login, inside a workspace, reads and writes only that workspace's rows: it reads its
/// two people and may move a message's state, and a row it writes for another workspace is refused
/// (a policy without WITH CHECK applies its USING clause to new rows). A message's send time,
/// person and the rest of what was accepted are immutable to it (UPDATE is granted on the state
/// columns alone), the campaign counters have one writer, the rollup, and the system tables are
/// out of its reach.
#[tokio::test]
async fn the_api_login_writes_its_workspace_and_only_mutable_columns() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(
        &test,
        &[
            ("its own people", As::App, "SELECT id FROM people", Ok(2)),
            (
                "a message's state",
                As::App,
                "UPDATE messages SET state = 'cancelled'",
                Ok(1),
            ),
            (
                "a row for another workspace",
                As::App,
                "INSERT INTO people (workspace_id, email) VALUES ('00000000-0000-7000-8000-00000000b000', 'z@example.com')",
                Err(DENIED),
            ),
            (
                "a message's send time",
                As::App,
                "UPDATE messages SET send_at = now()",
                Err(DENIED),
            ),
            (
                "a message's person",
                As::App,
                "UPDATE messages SET person_id = NULL",
                Err(DENIED),
            ),
            (
                "the campaign counters",
                As::App,
                "UPDATE campaign_daily_stats SET sent = 0",
                Err(DENIED),
            ),
            (
                "the partition inventory",
                As::App,
                "SELECT name FROM partition_leaves",
                Err(DENIED),
            ),
        ],
    )
    .await;
}

/// The worker's login works one workspace at a time. Inside `acme` it sees `acme`'s people and
/// queue row only and records health transitions (it opens a connection's breaker and closes a
/// quota scope's). With no workspace set and no role switch it sees no queue row, job, lane,
/// connection or endpoint at all. Whatever it sets, it never reads a sealed credential, a user's
/// folded email or the signing keys, never rewrites a message's connection and never writes the
/// campaign counters. Its dispatch registration runs with its own rights: a queue row it inserts
/// bumps its workspace's dispatch row exactly once, so the sender's round robin learns of the work.
#[tokio::test]
async fn the_worker_login_works_one_workspace_and_never_reads_a_secret() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("its workspace's people", As::Worker, "SELECT id FROM people", Ok(2)),
        ("its workspace's queue row", As::Worker, "SELECT message_id FROM delivery_queue", Ok(1)),
        ("opening a connection's breaker", As::Worker,
         "UPDATE connections SET consecutive_failures = consecutive_failures + 1, paused_until = now() + interval '60 seconds', breaker_opened_at = now()
           WHERE id = '00000000-0000-7000-8000-00000000a011'",
         Ok(1)),
        ("closing a quota scope's breaker", As::Worker,
         "UPDATE quota_scopes SET consecutive_failures = 0, paused_until = NULL, breaker_opened_at = NULL, probe_message_id = NULL, probe_generation = NULL
           WHERE id = '00000000-0000-7000-8000-00000000a080'",
         Ok(1)),
        ("queue rows without a workspace", As::WorkerAlone, "SELECT message_id FROM delivery_queue", Ok(0)),
        ("jobs without a workspace", As::WorkerAlone, "SELECT id FROM jobs", Ok(0)),
        ("lanes without a workspace", As::WorkerAlone, "SELECT queue FROM job_lanes", Ok(0)),
        ("connections without a workspace", As::WorkerAlone, "SELECT id FROM connections", Ok(0)),
        ("endpoints without a workspace", As::WorkerAlone, "SELECT id FROM webhook_endpoints", Ok(0)),
        ("a sealed credential", As::Worker, "SELECT credential FROM connections", Err(DENIED)),
        ("a user's folded email", As::Worker, "SELECT email_key FROM users", Err(DENIED)),
        ("a message's connection", As::Worker,
         "UPDATE messages SET connection_id = '00000000-0000-7000-8000-00000000a011'",
         Err(DENIED)),
        ("the signing keys", As::Worker, "SELECT kid FROM signing_keys", Err(DENIED)),
        ("the campaign counters", As::Worker,
         "INSERT INTO campaign_daily_stats (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day)
          VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7(), uuidv7(), 1, uuidv7(), 1, current_date)",
         Err(DENIED)),
    ])
    .await;

    let mut tx = test
        .worker
        .begin_in(WorkspaceId::trusted(ACME))
        .await
        .unwrap();
    let version = "SELECT work_version FROM dispatch_workspaces WHERE workspace_id = '00000000-0000-7000-8000-00000000a000'";
    let before: i64 = sqlx::query_scalar(version)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    outcome(
        &mut *tx,
        "WITH m AS (
           INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, html, render_version, rendered_at, internet_message_id, send_at)
           VALUES ('00000000-0000-7000-8000-00000000a000', 'direct', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test', ARRAY['q@example.com'], 's', '<p>x</p>', 'v1', now(), '<acme-2@acme.test>', now())
           RETURNING workspace_id, id, connection_id, send_at)
         INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at) SELECT workspace_id, id, connection_id, send_at FROM m",
    )
    .await
    .unwrap();
    let after: i64 = sqlx::query_scalar(version)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(after - before, 1);
}

/// The scheduler role, entered from the worker's login, is the one view across workspaces, and a
/// narrow one: it sees both workspaces' queue rows with their deadlines and every active connection
/// (not one still unverified), lists the active provider webhooks, may record a probe on a
/// connection and on a quota scope, and may lock a scope `FOR UPDATE` to admit one. It is refused
/// every payload, body and secret (job and outbox payloads, credentials, endpoints), the people,
/// the usage ledgers, and clearing a breaker, which only the finish of a submission does.
#[tokio::test]
async fn the_scheduler_view_reads_routing_columns_across_workspaces_and_nothing_else() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("both workspaces' queue rows", As::Scheduler, "SELECT message_id FROM delivery_queue", Ok(2)),
        ("their deadlines, for the expiry sweep", As::Scheduler, "SELECT deadline_at FROM delivery_queue", Ok(2)),
        ("the active connections", As::Scheduler, "SELECT id FROM connections", Ok(4)),
        ("the invoker-security send projection", As::Scheduler, "SELECT slot_offset FROM slot_projection", Ok(12)),
        ("the active provider webhooks", As::Scheduler, "SELECT id FROM provider_webhooks", Ok(1)),
        ("a connection's probe", As::Scheduler,
         "UPDATE connections SET probe_message_id = uuidv7(), probe_generation = 1 WHERE id = '00000000-0000-7000-8000-00000000a011'",
         Ok(1)),
        ("a half-open scope locked for its probe", As::Scheduler,
         "SELECT id FROM quota_scopes WHERE id = '00000000-0000-7000-8000-00000000a080' FOR UPDATE SKIP LOCKED",
         Ok(1)),
        ("a scope's probe", As::Scheduler,
         "UPDATE quota_scopes SET probe_message_id = uuidv7(), probe_generation = 1 WHERE id = '00000000-0000-7000-8000-00000000a080'",
         Ok(1)),
        ("clearing a connection's breaker", As::Scheduler, "UPDATE connections SET consecutive_failures = 0", Err(DENIED)),
        ("clearing a scope's breaker", As::Scheduler,
         "UPDATE quota_scopes SET consecutive_failures = 0, breaker_opened_at = NULL", Err(DENIED)),
        ("a job's payload", As::Scheduler, "SELECT payload FROM jobs", Err(DENIED)),
        ("an outbox payload", As::Scheduler, "SELECT payload FROM outbox_events", Err(DENIED)),
        ("a sealed credential", As::Scheduler, "SELECT credential FROM connections", Err(DENIED)),
        ("the webhook endpoints", As::Scheduler, "SELECT workspace_id FROM webhook_endpoints", Err(DENIED)),
        ("the people", As::Scheduler, "SELECT id FROM people", Err(DENIED)),
        ("the usage ledgers", As::Scheduler, "SELECT day FROM connection_usage", Err(DENIED)),
    ])
    .await;
}

/// The tracking drain's login, which runs on the public host, writes raw events for any workspace
/// (reading back what it inserted), the per-message rollup and increments, and reads only the
/// campaign ancestry of messages; it is refused the campaign counters, the people and message
/// bodies, so the public host can leak nothing more.
#[tokio::test]
async fn the_tracking_login_writes_events_and_reads_nothing_else() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("an event, read back", As::Tracking,
         "INSERT INTO tracking_events (workspace_id, id, message_id, kind, actor_class, occurred_at)
          VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7(), uuidv7(), 'open', 'human', now()) ON CONFLICT DO NOTHING RETURNING id",
         Ok(1)),
        ("the per-message rollup", As::Tracking,
         "INSERT INTO message_engagement (workspace_id, message_id, opens) VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7(), 1)",
         Ok(1)),
        ("an increment", As::Tracking,
         "INSERT INTO stats_increments (workspace_id, day, metric, delta) VALUES ('00000000-0000-7000-8000-00000000a000', current_date, 'opened', 1)",
         Ok(1)),
        ("the campaign ancestry of every message", As::Tracking, "SELECT campaign_id FROM messages", Ok(2)),
        ("the campaign counters", As::Tracking,
         "INSERT INTO campaign_daily_stats (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day)
          VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7(), uuidv7(), 1, uuidv7(), 1, current_date)",
         Err(DENIED)),
        ("the people", As::Tracking, "SELECT id FROM people", Err(DENIED)),
        ("a message's body", As::Tracking, "SELECT html FROM messages", Err(DENIED)),
    ])
    .await;
}

/// The system login bypasses row security, yet never writes increments, neither through the parent
/// nor into a leaf: it runs without a transaction timeout, so an increment of its own could commit
/// below the rollup's watermark and never be counted. It writes no leaf directly, so a detached
/// leaf stays frozen, and it reads a leaf directly, as the archive export does.
#[tokio::test]
async fn the_system_login_never_writes_increments_or_leaves() {
    let test = TestDb::new().await;
    seed(&test).await;
    let messages: String = sqlx::query_scalar(
        "SELECT tableoid::regclass::text FROM messages WHERE internet_message_id = '<acme-1@acme.test>'",
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let increments: String = sqlx::query_scalar(
        "SELECT name FROM partition_leaves WHERE parent = 'stats_increments' AND lower <= now() AND now() < upper",
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let increment = "(workspace_id, day, metric, delta) VALUES ('00000000-0000-7000-8000-00000000a000', current_date, 'sent', 1)";
    for (what, sql, expected) in [
        (
            "an increment through the parent",
            format!("INSERT INTO stats_increments {increment}"),
            Err(DENIED.to_owned()),
        ),
        (
            "an increment into its leaf",
            format!("INSERT INTO {increments} {increment}"),
            Err(DENIED.to_owned()),
        ),
        (
            "an update of a messages leaf",
            format!("UPDATE {messages} SET state = 'failed'"),
            Err(DENIED.to_owned()),
        ),
        (
            "a direct read of a messages leaf",
            format!("SELECT id FROM {messages} WHERE internet_message_id = '<acme-1@acme.test>'"),
            Ok(1),
        ),
    ] {
        assert_eq!(attempt(&test, As::System, &sql).await, expected, "{what}");
    }
}

/// A past period: its leaves of messages, attempts, engagement, the outbox and webhook
/// deliveries, and in them a message of `acme` still in the delivery queue, its engagement, a
/// published event and that event's delivery to `acme`'s endpoint.
const PAST_PERIOD: &str = "
SELECT ensure_partition(t, '2026-09-20 00:00+00')
  FROM unnest(ARRAY['messages', 'attempts', 'message_engagement', 'outbox_events', 'webhook_deliveries']::regclass[]) t;
INSERT INTO messages (workspace_id, id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject, html, render_version, rendered_at, internet_message_id, send_at, created_at)
VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-09-20 12:00+00'), 'direct', '00000000-0000-7000-8000-00000000a020', '00000000-0000-7000-8000-00000000a010', 'mta@acme.test',
        ARRAY['p@example.com'], 's', '<p>x</p>', 'v1', '2026-09-20', '<old@acme.test>', '2026-09-26', '2026-09-20');
INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at)
VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-09-20 12:00+00'), '00000000-0000-7000-8000-00000000a010', '2026-09-26');
INSERT INTO message_engagement (workspace_id, message_id, opens)
VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-09-20 12:00+00'), 1);
INSERT INTO outbox_events (workspace_id, id, type, subject_type, subject_id, payload, published_at)
VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-09-20 13:00+00'), 'message.sent', 'message', uuidv7_boundary('2026-09-20 12:00+00'), '{}', '2026-09-20 13:00+00');
INSERT INTO webhook_deliveries (workspace_id, endpoint_id, event_id, state, delivered_at)
VALUES ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a090', uuidv7_boundary('2026-09-20 13:00+00'), 'delivered', '2026-09-20 13:01+00');";

/// The delivery queue's foreign key gates the archive, run as it really runs: as `norbelys_owner`,
/// reached from the system login, detaching the messages leaf of a period that still holds a
/// queued message is refused, and once nothing references the period the same detach goes
/// through. PostgreSQL checks for referencing rows as the current user, and forced row security
/// applies to the owner, so the check sees the queue row and the leaf's message only through the
/// owner's read-only detach policies; without them it would see nothing and let the period go,
/// leaving a queued message its parent no longer holds.
#[tokio::test]
async fn a_queued_message_gates_the_owners_detach_of_its_period() {
    let test = TestDb::new().await;
    seed(&test).await;
    run(&test.system, PAST_PERIOD).await;
    expect_all(&test, &[
        ("a period holding a queued message", As::System,
         "SET LOCAL ROLE norbelys_owner;
          ALTER TABLE messages DETACH PARTITION messages_20260920",
         Err(FOREIGN_KEY)),
        ("the same period once its queue row is gone", As::System,
         "DELETE FROM delivery_queue WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00');
          SET LOCAL ROLE norbelys_owner;
          ALTER TABLE messages DETACH PARTITION messages_20260920",
         Ok(1)),
    ])
    .await;
}

/// Every other foreign key that points at a partitioned table gates the owner's detach the same
/// way, each through the owner's read-only policy on its referencing table: with the queue row
/// gone, an unsettled attempt, an enrollment, a recipient hold or an inbound message that names
/// the period's message still makes its leaf's detach fail, and a webhook delivery of the period's
/// event makes the outbox leaf's fail until the delivery is gone. So the archive cannot drop a
/// period out from under anything that still points into it, and dependents must go first.
#[tokio::test]
async fn every_reference_into_a_period_gates_the_owners_detach() {
    let test = TestDb::new().await;
    seed(&test).await;
    run(&test.system, PAST_PERIOD).await;
    expect_all(&test, &[
        ("an unsettled attempt", As::System,
         "DELETE FROM delivery_queue WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00');
          INSERT INTO connection_usage (workspace_id, connection_id, day, reserved)
          VALUES ('00000000-0000-7000-8000-00000000a000', '00000000-0000-7000-8000-00000000a010', '2026-09-20', 1);
          INSERT INTO attempts (workspace_id, message_id, attempt_number, connection_id, reserved_day, recipient_count, lease_owner)
          VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-09-20 12:00+00'), 1, '00000000-0000-7000-8000-00000000a010', '2026-09-20', 1, 'worker-a');
          SET LOCAL ROLE norbelys_owner;
          ALTER TABLE messages DETACH PARTITION messages_20260920",
         Err(FOREIGN_KEY)),
        ("an enrollment pointing at the message", As::System,
         "DELETE FROM delivery_queue WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00');
          UPDATE enrollments SET message_id = uuidv7_boundary('2026-09-20 12:00+00') WHERE id = '00000000-0000-7000-8000-00000000a071';
          SET LOCAL ROLE norbelys_owner;
          ALTER TABLE messages DETACH PARTITION messages_20260920",
         Err(FOREIGN_KEY)),
        ("a recipient hold on the message", As::System,
         "DELETE FROM delivery_queue WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00');
          INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
          VALUES ('00000000-0000-7000-8000-00000000a000', uuidv7_boundary('2026-09-20 12:00+00'), 'p@example.com', 'mailbox_full', now(), now() + interval '1 day');
          SET LOCAL ROLE norbelys_owner;
          ALTER TABLE messages DETACH PARTITION messages_20260920",
         Err(FOREIGN_KEY)),
        ("an inbound message answering it", As::System,
         "DELETE FROM delivery_queue WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00');
          UPDATE inbound_messages SET message_id = uuidv7_boundary('2026-09-20 12:00+00') WHERE id = '00000000-0000-7000-8000-00000000a0e1';
          SET LOCAL ROLE norbelys_owner;
          ALTER TABLE messages DETACH PARTITION messages_20260920",
         Err(FOREIGN_KEY)),
        ("an outbox leaf whose event still has a delivery", As::System,
         "SET LOCAL ROLE norbelys_owner;
          ALTER TABLE outbox_events DETACH PARTITION outbox_events_20260920",
         Err(FOREIGN_KEY)),
        ("the outbox leaf once its delivery is gone", As::System,
         "DELETE FROM webhook_deliveries WHERE event_id = uuidv7_boundary('2026-09-20 13:00+00');
          SET LOCAL ROLE norbelys_owner;
          ALTER TABLE outbox_events DETACH PARTITION outbox_events_20260920",
         Ok(1)),
    ])
    .await;
}

/// The owner's read-only detach policies open no write: forced row security governs the owner as
/// every other role, so as the owner a tenant row cannot be inserted, and an update or a delete of
/// a table the owner may read reaches no row. The owner reads what its detach checks read (the
/// delivery queue) and nothing of the other tables (the people). A mistaken data-changing
/// statement run as the owner therefore still changes nothing.
#[tokio::test]
async fn the_owner_reads_only_what_its_detach_checks_need_and_writes_no_tenant_row() {
    let test = TestDb::new().await;
    seed(&test).await;
    expect_all(&test, &[
        ("the delivery queue, as its detach check reads it", As::System,
         "SET LOCAL ROLE norbelys_owner;
          SELECT message_id FROM delivery_queue",
         Ok(2)),
        ("a table no detach check reads", As::System,
         "SET LOCAL ROLE norbelys_owner;
          SELECT id FROM people",
         Ok(0)),
        ("a tenant row inserted", As::System,
         "SET LOCAL ROLE norbelys_owner;
          INSERT INTO people (workspace_id, email) VALUES ('00000000-0000-7000-8000-00000000a000', 'z@example.com')",
         Err(DENIED)),
        ("a row it may read, updated", As::System,
         "SET LOCAL ROLE norbelys_owner;
          UPDATE delivery_queue SET run_at = now()",
         Ok(0)),
        ("a row it may read, deleted", As::System,
         "SET LOCAL ROLE norbelys_owner;
          DELETE FROM delivery_queue",
         Ok(0)),
    ])
    .await;
}

/// The archive's sealed order freezes a leaf. A deliveries leaf, detached concurrently and sealed
/// (its own outgoing foreign keys dropped), keeps its row when its endpoint is deleted, which
/// would otherwise cascade into it, and refuses the system login's update. Once its queue row is
/// gone, the period's messages leaf detaches and seals with no outgoing foreign key left and no row
/// reachable through the parent, its row still readable for the export; and the period's
/// engagement leaf, on its own clock, stays attached with its row.
#[tokio::test]
async fn a_sealed_leaf_is_frozen_and_out_of_its_parents_reach() {
    let test = TestDb::new().await;
    seed(&test).await;
    run(&test.system, PAST_PERIOD).await;
    // The archive's DDL is the owner's, reached from the system login; a concurrent detach must run
    // outside any transaction, on a connection of its own.
    let mut owner = test.system.pool().acquire().await.unwrap();
    outcome(&mut *owner, "SET ROLE norbelys_owner")
        .await
        .unwrap();
    outcome(
        &mut *owner,
        "ALTER TABLE webhook_deliveries DETACH PARTITION webhook_deliveries_20260920 CONCURRENTLY",
    )
    .await
    .unwrap();
    outcome(
        &mut *owner,
        "DO $$ DECLARE c record; BEGIN
           FOR c IN SELECT conname FROM pg_constraint WHERE conrelid = 'webhook_deliveries_20260920'::regclass AND contype = 'f' AND conparentid = 0 LOOP
             EXECUTE format('ALTER TABLE webhook_deliveries_20260920 DROP CONSTRAINT %I', c.conname);
           END LOOP;
         END $$",
    )
    .await
    .unwrap();
    run(
        &test.system,
        "DELETE FROM webhook_endpoints WHERE id = '00000000-0000-7000-8000-00000000a090'",
    )
    .await;
    assert_eq!(
        count(&test, "SELECT count(*) FROM webhook_deliveries_20260920").await,
        1
    );
    assert_eq!(
        attempt(
            &test,
            As::System,
            "UPDATE webhook_deliveries_20260920 SET state = 'failed'"
        )
        .await,
        Err(DENIED.to_owned())
    );

    run(
        &test.system,
        "DELETE FROM delivery_queue WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00')",
    )
    .await;
    outcome(
        &mut *owner,
        "ALTER TABLE messages DETACH PARTITION messages_20260920 CONCURRENTLY",
    )
    .await
    .unwrap();
    outcome(
        &mut *owner,
        "DO $$ DECLARE c record; BEGIN
           FOR c IN SELECT conname FROM pg_constraint WHERE conrelid = 'messages_20260920'::regclass AND contype = 'f' AND conparentid = 0 LOOP
             EXECUTE format('ALTER TABLE messages_20260920 DROP CONSTRAINT %I', c.conname);
           END LOOP;
         END $$",
    )
    .await
    .unwrap();
    outcome(&mut *owner, "RESET ROLE").await.unwrap();
    drop(owner);
    assert_eq!(
        count(
            &test,
            "SELECT count(*) FROM pg_constraint WHERE conrelid = 'messages_20260920'::regclass AND contype = 'f'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &test,
            "SELECT count(*) FROM messages WHERE id >= uuidv7_boundary('2026-09-20') AND id < uuidv7_boundary('2026-09-21')"
        )
        .await,
        0
    );
    assert_eq!(
        count(&test, "SELECT count(*) FROM messages_20260920").await,
        1
    );
    assert_eq!(
        count(
            &test,
            "SELECT count(*) FROM message_engagement WHERE message_id = uuidv7_boundary('2026-09-20 12:00+00')"
        )
        .await,
        1
    );
}

/// Archive pairs keep one period and one retention: changing `webhook_deliveries`' retention,
/// `attempts`' period or `outbox_events`' retention alone is refused, at once when the check is
/// made immediate and at commit by default (deferred, so a change can update both rows in one
/// transaction), and a refused change leaves the policy as it was; the pair changed together is
/// accepted. A dependent leaf is therefore never archived apart from the leaf it references.
#[tokio::test]
async fn archive_pairs_change_their_policies_together_or_not_at_all() {
    let test = TestDb::new().await;
    expect_all(&test, &[
        ("webhook_deliveries' retention alone", As::System,
         "SET CONSTRAINTS partition_policy_pairs IMMEDIATE;
          UPDATE partition_policies SET retention = interval '3 days' WHERE table_name = 'webhook_deliveries'",
         Err(CHECK)),
        ("attempts' period alone", As::System,
         "SET CONSTRAINTS partition_policy_pairs IMMEDIATE;
          UPDATE partition_policies SET period = interval '1 month' WHERE table_name = 'attempts'",
         Err(CHECK)),
        ("outbox_events' retention alone", As::System,
         "SET CONSTRAINTS partition_policy_pairs IMMEDIATE;
          UPDATE partition_policies SET retention = interval '9 days' WHERE table_name = 'outbox_events'",
         Err(CHECK)),
    ])
    .await;

    let retention = "SELECT retention::text FROM partition_policies WHERE table_name = $1";
    let mut alone = test.system.begin().await.unwrap();
    outcome(
        &mut *alone,
        "UPDATE partition_policies SET retention = interval '3 days' WHERE table_name = 'webhook_deliveries'",
    )
    .await
    .unwrap();
    let refused = alone.commit().await.err().and_then(|error| match error {
        sqlx::Error::Database(error) => error.code().map(Cow::into_owned),
        _ => None,
    });
    assert_eq!(refused.as_deref(), Some(CHECK));
    let kept: String = sqlx::query_scalar(retention)
        .bind("webhook_deliveries")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(kept, "7 days");

    let mut together = test.system.begin().await.unwrap();
    outcome(
        &mut *together,
        "UPDATE partition_policies SET retention = interval '8 days' WHERE table_name IN ('outbox_events', 'webhook_deliveries')",
    )
    .await
    .unwrap();
    together.commit().await.unwrap();
    for table in ["outbox_events", "webhook_deliveries"] {
        let changed: String = sqlx::query_scalar(retention)
            .bind(table)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
        assert_eq!(changed, "8 days", "{table}");
    }
}

/// The pacing grid's one formula maps an instant to the first phase instant at or after it: a
/// phase instant to itself, a microsecond after it to the next slot's, an instant before the phase
/// to the same slot's, and a window opening at 09:00 to 09:00 plus the phase; so every writer of a
/// pacing clock lands on the sender's phase.
#[tokio::test]
async fn next_phase_at_maps_an_instant_onto_the_phase_grid() {
    let test = TestDb::new().await;
    for (at, expected) in [
        ("2026-10-01 10:00:41+00", "2026-10-01 10:00:41+00"),
        ("2026-10-01 10:00:41.000001+00", "2026-10-01 10:05:41+00"),
        ("2026-10-01 10:00:12+00", "2026-10-01 10:00:41+00"),
        ("2026-10-02 09:00:00+00", "2026-10-02 09:00:41+00"),
    ] {
        let next: String = sqlx::query_scalar("SELECT next_phase_at($1::timestamptz, 41)::text")
            .bind(at)
            .fetch_one(test.app.pool())
            .await
            .unwrap();
        assert_eq!(next, expected, "{at}");
    }
}

/// The values a single-column `CHECK` of `table.column` admits, sorted.
async fn vocabulary(test: &TestDb, table: &str, column: &str) -> Vec<String> {
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c
          WHERE c.conrelid = $1::regclass AND c.contype = 'c'
            AND c.conkey = ARRAY[(SELECT a.attnum FROM pg_attribute a WHERE a.attrelid = $1::regclass AND a.attname = $2)]",
    )
    .bind(table)
    .bind(column)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    // `CHECK ((role = ANY (ARRAY['owner'::text, 'admin'::text])))`: the quoted parts are the values.
    let mut values: Vec<String> = definition
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    values.sort();
    values
}

/// Every Rust enumeration stored in a checked text column agrees with that column's `CHECK`, value
/// for value, so a value the code writes is never refused and a value the database holds is never
/// unreadable: membership roles with `memberships.role`; the roles an invitation or an SSO
/// connection grants with every role but owner, since ownership is never granted by an invitation
/// or a sign-in; key modes with `workspaces.mode`. The cases come from the enums, so a new variant
/// fails here until the schema knows it.
#[tokio::test]
async fn stored_rust_enums_agree_with_their_checks() {
    fn sorted(values: impl Iterator<Item = &'static str>) -> Vec<&'static str> {
        let mut values: Vec<&'static str> = values.collect();
        values.sort_unstable();
        values
    }
    let test = TestDb::new().await;
    assert_eq!(
        vocabulary(&test, "memberships", "role").await,
        sorted(MembershipRole::iter().map(MembershipRole::as_str))
    );
    let granted = sorted(
        MembershipRole::iter()
            .filter(|role| *role != MembershipRole::Owner)
            .map(MembershipRole::as_str),
    );
    assert_eq!(vocabulary(&test, "invitations", "role").await, granted);
    assert_eq!(
        vocabulary(&test, "sso_connections", "default_role").await,
        granted
    );
    assert_eq!(
        vocabulary(&test, "workspaces", "mode").await,
        sorted(KeyMode::iter().map(KeyMode::workspace_mode))
    );
}

/// Every enumeration the OpenAPI document publishes for a stored column agrees with that
/// column's `CHECK`, value for value, and the document names exactly those values. A response
/// field is read from its column as text and shown as it is, while the document types it with
/// the enumeration: a value the database admits and the document lacks would reach clients
/// outside the published type, and a value the document names and the database refuses would be
/// a promise nothing keeps. The cases come from the enumerations, so a new variant fails here
/// until the schema and the document know it.
#[tokio::test]
async fn published_enums_agree_with_their_checks() {
    use crate::ai::snippets::Fallback;
    use crate::delivery::evidence::Action;
    use crate::delivery::http::HoldResolution;
    use crate::domain::campaigns::{CampaignStatus, EnrollmentStatus};
    use crate::domain::inbox::{Classification, ClassificationSource, Sentiment};
    use crate::domain::messages::{Kind, State};
    use crate::domain::people::FieldType;
    use crate::domain::policy::delivery::{
        Confidence, EventKind, HoldReason, Outcome, Phase, RecipientRef, Source,
    };
    use crate::domain::senders::{Provider, Status, Transport};
    use crate::domain::suppressions::{Reason, Source as SuppressionSource};
    use crate::inbox::http::ThreadStatus;
    use crate::jobs::http::JobState;
    use crate::people::exports::{ExportStatus, Format as ExportFormat, Resource};
    use crate::people::imports::ImportStatus;
    use crate::senders::domains::SendingDomainStatus;
    use crate::senders::scopes::WindowUnit;
    use crate::webhooks::deliver::DeliveryState;

    /// The sorted wire names of every variant of `E`.
    fn names<E: strum::IntoEnumIterator + Into<&'static str>>() -> Vec<&'static str> {
        let mut values: Vec<&'static str> = E::iter().map(Into::into).collect();
        values.sort_unstable();
        values
    }

    let test = TestDb::new().await;
    let document = serde_json::to_value(crate::http::router::openapi()).unwrap();
    let mut images: Vec<&'static str> = crate::domain::images::Format::iter()
        .map(crate::domain::images::Format::media_type)
        .collect();
    images.sort_unstable();
    // The column, the schema the document publishes for it, and the enumeration's values.
    let cases: [(&str, &str, &str, Vec<&'static str>); 35] = [
        ("messages", "kind", "MessageKind", names::<Kind>()),
        ("messages", "state", "MessageState", names::<State>()),
        (
            "messages",
            "snippets_fallback",
            "SnippetsFallback",
            names::<Fallback>(),
        ),
        ("attempts", "outcome", "AttemptOutcome", names::<Outcome>()),
        ("attempts", "phase", "SubmissionPhase", names::<Phase>()),
        (
            "delivery_events",
            "kind",
            "DeliveryEventKind",
            names::<EventKind>(),
        ),
        (
            "delivery_events",
            "source",
            "EvidenceSource",
            names::<Source>(),
        ),
        (
            "delivery_events",
            "confidence",
            "EvidenceConfidence",
            names::<Confidence>(),
        ),
        (
            "delivery_events",
            "recipient_ref",
            "RecipientRef",
            names::<RecipientRef>(),
        ),
        ("delivery_events", "action", "DsnAction", names::<Action>()),
        (
            "delivery_events",
            "phase",
            "SubmissionPhase",
            names::<Phase>(),
        ),
        (
            "recipient_holds",
            "reason",
            "HoldReason",
            names::<HoldReason>(),
        ),
        (
            "recipient_holds",
            "resolution",
            "HoldResolution",
            names::<HoldResolution>(),
        ),
        ("connections", "provider", "Provider", names::<Provider>()),
        (
            "connections",
            "transport",
            "Transport",
            names::<Transport>(),
        ),
        (
            "connections",
            "status",
            "ConnectionStatus",
            names::<Status>(),
        ),
        ("quota_scopes", "provider", "Provider", names::<Provider>()),
        (
            "quota_scopes",
            "window_unit",
            "WindowUnit",
            names::<WindowUnit>(),
        ),
        (
            "sending_domains",
            "status",
            "SendingDomainStatus",
            names::<SendingDomainStatus>(),
        ),
        ("exports", "kind", "ExportResource", names::<Resource>()),
        ("exports", "format", "ExportFormat", names::<ExportFormat>()),
        ("exports", "status", "ExportStatus", names::<ExportStatus>()),
        ("imports", "status", "ImportStatus", names::<ImportStatus>()),
        ("jobs", "state", "JobState", names::<JobState>()),
        (
            "webhook_deliveries",
            "state",
            "WebhookDeliveryState",
            names::<DeliveryState>(),
        ),
        (
            "suppressions",
            "reason",
            "SuppressionReason",
            names::<Reason>(),
        ),
        (
            "suppressions",
            "source",
            "SuppressionSource",
            names::<SuppressionSource>(),
        ),
        (
            "inbound_messages",
            "classification",
            "InboundClassification",
            names::<Classification>(),
        ),
        (
            "inbound_messages",
            "classification_source",
            "ClassificationSource",
            names::<ClassificationSource>(),
        ),
        (
            "inbound_messages",
            "sentiment",
            "Sentiment",
            names::<Sentiment>(),
        ),
        ("threads", "status", "ThreadStatus", names::<ThreadStatus>()),
        (
            "campaigns",
            "status",
            "CampaignStatus",
            names::<CampaignStatus>(),
        ),
        (
            "enrollments",
            "status",
            "EnrollmentStatus",
            names::<EnrollmentStatus>(),
        ),
        (
            "person_field_definitions",
            "field_type",
            "FieldType",
            names::<FieldType>(),
        ),
        ("images", "content_type", "ImageContentType", images),
    ];
    for (table, column, schema, values) in cases {
        assert_eq!(
            vocabulary(&test, table, column).await,
            values,
            "{table}.{column}"
        );
        let mut documented: Vec<&str> = document["components"]["schemas"][schema]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("the document publishes no enum `{schema}`"))
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        documented.sort_unstable();
        assert_eq!(
            documented, values,
            "the schema `{schema}` of {table}.{column}"
        );
    }
}
