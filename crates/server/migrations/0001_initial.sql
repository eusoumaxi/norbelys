-- The Norbelys database schema, for PostgreSQL 18.
--
-- The authoritative fresh baseline, applied by external SQLx maintenance tooling.
-- Applied migrations are immutable. Subsequent schema changes are new numbered files.
-- Cluster roles come from ../provision.sql and are never created by a running application.
--
-- Conventions, so every table reads the same way:
-- * Tenant tables are keyed workspace first, PRIMARY KEY (workspace_id, id), and children reference
--   their parents through composite foreign keys that include workspace_id. A row can therefore
--   never point at another workspace's row, and the wider keys also prove membership in a chain
--   (a step of this campaign, a variant of this step, an identity of this connection).
-- * Ids are uuidv7: time-ordered, so the large fact tables are range-partitioned by period on them.
-- * Times are timestamptz; every day a ledger or a partition names is a UTC day.
-- * Enumerations are text with a CHECK that lists the vocabulary: readable in psql, and a value is
--   added by changing the CHECK. An enum type would not do: a value added by ALTER TYPE cannot be
--   used in the transaction that adds it, and a value can never be removed.
-- * A second kind of a thing is a `kind` column with a CHECK saying which columns that kind sets;
--   a nullable column means "legitimately absent", never "depends on the kind".
-- * Secrets are bytea sealed with the deployment key, and anything only compared is stored as a
--   hash, so a dump of the database is not a credential leak.

-- ───────────────────────────── 1. Functions and system tables ─────────────────────────────
-- Case folding the API promises: ASCII only, immutable, so it can back generated columns.
CREATE FUNCTION ascii_lower(value text) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN translate(value, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz');

-- Cold mail runs on a grid of 5-minute slots that start on the 5-minute marks of UTC. Every paced
-- sender has a phase, a fixed second inside each slot chosen at random when it is connected, so the
-- senders' cold sends spread across the slot instead of bunching at the mark; its phase instants
-- are the marks plus its phase. This is the first phase instant at or after `at`; timestamps have
-- microsecond resolution, so an instant on the grid maps to itself. Every writer of a pacing clock
-- (connections.next_send_at) computes its instants with this one function.
CREATE FUNCTION next_phase_at(at timestamptz, phase_seconds integer) RETURNS timestamptz
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN date_bin('5 minutes', at - phase_seconds * interval '1 second' - interval '1 microsecond', TIMESTAMPTZ '2001-01-01 00:00:00+00')
           + interval '5 minutes' + phase_seconds * interval '1 second';

CREATE FUNCTION set_updated_at() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at := now(); RETURN NEW; END $$;

-- The workspace of the current request, set by the api role per transaction:
--   SET LOCAL norbelys.workspace_id = '<uuid>'
-- Every RLS policy compares against it. NULL (unset) matches nothing.
CREATE FUNCTION current_workspace() RETURNS uuid
    LANGUAGE sql STABLE PARALLEL SAFE
    RETURN nullif(current_setting('norbelys.workspace_id', true), '')::uuid;

-- UUIDv7 carries unix milliseconds in its top 48 bits (RFC 9562,
-- https://www.rfc-editor.org/rfc/rfc9562), so a period boundary is a uuid: the smallest uuidv7
-- that can be generated at that instant. The fact tables are range-partitioned by period on their
-- uuidv7 keys with these boundaries, so dropping a period is one DDL statement, with no DELETE
-- and no VACUUM.
CREATE FUNCTION uuidv7_boundary(at timestamptz) RETURNS uuid
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN (regexp_replace(lpad(to_hex((extract(epoch FROM at) * 1000)::bigint), 12, '0') || '70008000000000000000',
                           '^(.{8})(.{4})(.{4})(.{4})(.{12})$', '\1-\2-\3-\4-\5'))::uuid;

-- Which tables are partitioned by period (on a uuidv7 key, or on a timestamptz column), how long
-- a period is, and how long partitions stay online. Periods are UTC. A deployment at the
-- throughput floor (200 messages a second) uses days, so a leaf stays small and retention drops
-- whole days; a small one uses months. Changing a period applies to partitions created later.
CREATE TABLE partition_policies (
    table_name text     PRIMARY KEY,
    key_kind   text     NOT NULL DEFAULT 'uuidv7' CHECK (key_kind IN ('uuidv7', 'timestamptz')),
    period     interval NOT NULL CHECK (period IN (interval '1 day', interval '1 month')),
    retention  interval NOT NULL,                -- the online window: a leaf whose whole period is older is archived, then dropped
    archive    boolean  NOT NULL DEFAULT true    -- false: drop without export (receipts, increments)
);
-- Archive pairs: a dependent table's leaves reference its parent's and are archived together with
-- them (attempts with messages, webhook_deliveries with outbox_events), so each pair keeps one
-- period and one retention; otherwise a webhook delivery could be reopened while the leaf holding
-- it is being archived. Checked at commit from either side, a missing partner tolerated, so a
-- change updates both rows in one transaction. The retention lock: every operation that relies on
-- a retention value serialises with a change of it on one advisory lock keyed on this table's OID,
-- pg_advisory_*lock*('partition_policies'::regclass::oid). Reopening a webhook delivery takes it
-- shared for its transaction, the archive takes it shared for a leaf's whole archive order, and a
-- change of a policy takes it exclusive: the statement trigger below takes it in the changing
-- transaction, so no change, whoever writes it, can skip the lock.
CREATE FUNCTION check_partition_policy_pairs() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $$
DECLARE pair text[]; a partition_policies; b partition_policies;
BEGIN
    FOREACH pair SLICE 1 IN ARRAY ARRAY[['messages', 'attempts'], ['outbox_events', 'webhook_deliveries']] LOOP
        CONTINUE WHEN NOT NEW.table_name = ANY (pair);
        SELECT * INTO a FROM partition_policies WHERE table_name = pair[1];
        SELECT * INTO b FROM partition_policies WHERE table_name = pair[2];
        IF a.table_name IS NOT NULL AND b.table_name IS NOT NULL
           AND (a.period, a.retention) IS DISTINCT FROM (b.period, b.retention) THEN
            RAISE EXCEPTION 'partition policies of % and % must share period and retention: their leaves are archived together', pair[1], pair[2]
                USING ERRCODE = 'check_violation';
        END IF;
    END LOOP;
    RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER partition_policy_pairs AFTER INSERT OR UPDATE ON partition_policies
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION check_partition_policy_pairs();
CREATE FUNCTION take_retention_lock() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $$
BEGIN
    PERFORM pg_advisory_xact_lock('partition_policies'::regclass::oid::bigint);
    RETURN NULL;
END $$;
CREATE TRIGGER partition_policies_retention_lock BEFORE INSERT OR UPDATE OR DELETE ON partition_policies
    FOR EACH STATEMENT EXECUTE FUNCTION take_retention_lock();
-- Every leaf ever created, with its bounds: the archive job and the period transition read
-- this instead of parsing relpartbound. A period change takes effect at the first boundary
-- after the latest existing leaf, so leaves never overlap.
CREATE TABLE partition_leaves (
    parent         text        NOT NULL,
    name           text        PRIMARY KEY,
    lower          timestamptz NOT NULL,
    upper          timestamptz NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    -- The archive's record of the leaf once it left the database: the Parquet object holding its
    -- rows (NULL for a table dropped without export), the rows exported and the object's SHA-256
    -- (hex), checked against the stored object before the drop, and when the leaf was dropped.
    -- Exports of archived ranges read the objects named here.
    archive_key    text,
    archived_rows  bigint,
    archive_sha256 text,
    dropped_at     timestamptz,
    CHECK (lower < upper),
    CHECK ((archive_key IS NULL) = (archived_rows IS NULL) AND (archive_key IS NULL) = (archive_sha256 IS NULL))
);
CREATE INDEX partition_leaves_by_parent ON partition_leaves (parent, upper);

-- Creates the leaf covering `at` for a partitioned table if none exists, with the settings every
-- leaf must have, and returns its name. Bounds are computed in UTC regardless of the session
-- time zone (day and month arithmetic on a timestamp without time zone). A new leaf starts at
-- the later of its period floor and the latest existing upper bound, so a period change (day ↔
-- month) never overlaps an existing leaf. SECURITY DEFINER so the maintenance job can run it as
-- norbelys_system without owning the parent (ownership stays with norbelys_owner).
CREATE FUNCTION ensure_partition(parent_rel regclass, at_instant timestamptz) RETURNS text
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    pol      partition_policies%ROWTYPE;
    at_utc   timestamp := at_instant AT TIME ZONE 'UTC';
    floor_u  timestamp;
    upper_u  timestamp;
    lower_u  timestamp;
    latest_u timestamp;                                                  -- upper bound of the nearest leaf ending at or before the instant
    name     text;
    lo       text; hi text;
BEGIN
    SELECT * INTO pol FROM partition_policies WHERE table_name = parent_rel::text;
    IF NOT FOUND THEN RAISE EXCEPTION 'no partition policy for %', parent_rel; END IF;
    SELECT l.name INTO name FROM partition_leaves l
     WHERE l.parent = parent_rel::text AND l.lower <= at_instant AND at_instant < l.upper;
    IF FOUND THEN RETURN name; END IF;                                   -- already covered
    -- The natural period of the instant, clipped to the neighbouring leaves so a period change
    -- (day ↔ month) never overlaps a leaf that already exists.
    floor_u := CASE WHEN pol.period = interval '1 month' THEN date_trunc('month', at_utc) ELSE date_trunc('day', at_utc) END;
    upper_u := floor_u + pol.period;
    SELECT max(upper AT TIME ZONE 'UTC') INTO latest_u FROM partition_leaves WHERE parent = parent_rel::text AND upper <= at_instant;
    lower_u := greatest(floor_u, coalesce(latest_u, floor_u));
    SELECT least(upper_u, min(lower AT TIME ZONE 'UTC')) INTO upper_u FROM partition_leaves WHERE parent = parent_rel::text AND lower > at_instant;
    name := format('%s_%s', parent_rel::text, to_char(lower_u, 'YYYYMMDD'));
    IF pol.key_kind = 'uuidv7' THEN
        lo := uuidv7_boundary(lower_u AT TIME ZONE 'UTC')::text; hi := uuidv7_boundary(upper_u AT TIME ZONE 'UTC')::text;
    ELSE
        lo := (lower_u AT TIME ZONE 'UTC')::text; hi := (upper_u AT TIME ZONE 'UTC')::text;
    END IF;
    EXECUTE format('CREATE TABLE %I PARTITION OF %s FOR VALUES FROM (%L) TO (%L)
                    WITH (autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01, fillfactor = 90)',
                   name, parent_rel, lo, hi);
    EXECUTE format('ALTER TABLE %I OWNER TO norbelys_owner', name);
    -- Leaves inherit the parent's policies for queries through the parent; direct leaf access
    -- (the archive job, admin) gets the same isolation policies and the parent's grants.
    IF EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = name AND column_name = 'workspace_id') THEN
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', name);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', name);
        EXECUTE format('CREATE POLICY workspace_isolation ON %I AS PERMISSIVE FOR ALL TO norbelys_app USING (workspace_id = current_workspace())', name);
        EXECUTE format('CREATE POLICY workspace_isolation_worker ON %I AS PERMISSIVE FOR ALL TO norbelys_worker USING (workspace_id = current_workspace())', name);
        -- When the archive detaches a leaf of a table that foreign keys point at, PostgreSQL first
        -- checks that no row elsewhere still references it, reading this leaf directly as the
        -- owner, to which forced row security otherwise shows nothing; this read-only policy lets
        -- that check see the leaf's rows, so the detach of a period still referenced is refused.
        -- The tables on the referencing side get the same policy at the end of this file.
        IF EXISTS (SELECT 1 FROM pg_constraint WHERE contype = 'f' AND confrelid = parent_rel) THEN
            EXECUTE format('CREATE POLICY detach_check ON %I AS PERMISSIVE FOR SELECT TO norbelys_owner USING (true)', name);
        END IF;
    END IF;
    -- Leaves are read directly only by the archive export; every write goes through the parent,
    -- so no role holds DML on a leaf (the owner's DDL drops it). This also keeps the system
    -- login unable to write stats_increments in leaves created later.
    EXECUTE format('GRANT SELECT ON %I TO norbelys_system', name);
    INSERT INTO partition_leaves (parent, name, lower, upper)
    VALUES (parent_rel::text, name, lower_u AT TIME ZONE 'UTC', upper_u AT TIME ZONE 'UTC');
    RETURN name;
END $$;

-- Ensures, for every partition policy, the leaf covering `at_instant`, the leaf after it whatever
-- the period's length, and every leaf up to the one covering `at_instant + ahead`, and returns
-- their names. The leaf after the current one is always ensured so a monthly table has next
-- month's leaf a whole month ahead: were it ensured only once `ahead` reached into the next month,
-- inserts would fail at midnight on the first of the month until the next daily run. Each next leaf
-- is ensured at the upper bound of the one before it, so the covered range has no gap even where a
-- period change or an older leaf clips a leaf short. The horizon is computed in UTC, so a session
-- in another time zone cannot move it across a daylight-saving change. The maintenance job calls
-- it with the default instant, now().
CREATE FUNCTION ensure_partitions_ahead(ahead interval, at_instant timestamptz DEFAULT now()) RETURNS SETOF text
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    pol        partition_policies%ROWTYPE;
    horizon    timestamptz := ((at_instant AT TIME ZONE 'UTC') + ahead) AT TIME ZONE 'UTC';
    first_leaf text;
    leaf       text;
    next_at    timestamptz;
BEGIN
    FOR pol IN SELECT * FROM partition_policies LOOP
        first_leaf := ensure_partition(pol.table_name::regclass, at_instant);
        RETURN NEXT first_leaf;
        leaf := first_leaf;
        LOOP
            SELECT upper INTO next_at FROM partition_leaves WHERE name = leaf;
            -- the leaf after the first is always ensured; after it, only up to the horizon
            EXIT WHEN leaf <> first_leaf AND next_at > horizon;
            leaf := ensure_partition(pol.table_name::regclass, next_at);
            RETURN NEXT leaf;
        END LOOP;
    END LOOP;
END $$;

-- Whether the row `row_id` of the partitioned table `parent_table` belongs to a period that left
-- the online database: its UUIDv7 instant is older than the table's online window, or the leaf
-- that covered it was dropped (which also covers a window lengthened after the drop). The api
-- asks when a read by id finds no row, to answer `404 archived` with a pointer to exports instead
-- of `not_found`. Only the id's instant and the record of periods are read, never a workspace's
-- rows, so a foreign id and an absent one still answer alike; a table without a policy keyed on
-- UUIDv7 ids is never archived. SECURITY DEFINER because the api's login may not read
-- partition_leaves: it learns this answer and nothing else.
CREATE FUNCTION period_archived(parent_table text, row_id uuid) RETURNS boolean
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    RETURN EXISTS (
        SELECT 1 FROM partition_policies p
         WHERE p.table_name = parent_table AND p.key_kind = 'uuidv7'
           AND (uuid_extract_timestamp(row_id) < now() - p.retention
                OR EXISTS (SELECT 1 FROM partition_leaves l
                            WHERE l.parent = p.table_name AND l.dropped_at IS NOT NULL
                              AND l.lower <= uuid_extract_timestamp(row_id) AND uuid_extract_timestamp(row_id) < l.upper)));

-- The signed-in user of the current request, set by the api role from the session before
-- any workspace is chosen: SET LOCAL norbelys.user_id = '<uuid>'. The user-scoped policies (a
-- user's own memberships and idempotency keys) and the invitation lookup let the bootstrap path
-- (list my workspaces, accept an invitation, create a workspace) run without a workspace principal.
CREATE FUNCTION current_user_id() RETURNS uuid
    LANGUAGE sql STABLE PARALLEL SAFE
    RETURN nullif(current_setting('norbelys.user_id', true), '')::uuid;

-- Per-workspace round-robin cursors for the sender and the inbox. System data.
-- Statement-level insert triggers register the distinct workspaces of each statement once
-- (a row-level trigger would serialise a 100,000-row insert on one hot row); an idle
-- workspace is removed by the worker under a work_version check.
CREATE TABLE dispatch_workspaces (
    workspace_id   uuid        PRIMARY KEY,
    work_version   bigint      NOT NULL DEFAULT 0,
    sender_turn_at timestamptz NOT NULL DEFAULT 'epoch',
    inbox_turn_at  timestamptz NOT NULL DEFAULT 'epoch'
);

CREATE FUNCTION register_dispatch_workspace() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO dispatch_workspaces (workspace_id)
    SELECT DISTINCT workspace_id FROM inserted
    ON CONFLICT (workspace_id) DO UPDATE SET work_version = dispatch_workspaces.work_version + 1;
    RETURN NULL;
END $$;

-- ───────────────────────────── 2. Identity (no workspace_id: keyed by person) ─────────────────────────────
CREATE TABLE users (
    id                uuid        PRIMARY KEY DEFAULT uuidv7(),
    email             text        NOT NULL,
    email_key         text        NOT NULL GENERATED ALWAYS AS (ascii_lower(email)) STORED UNIQUE,
    email_verified_at timestamptz,
    name              text,
    locale            text        NOT NULL DEFAULT 'en',
    status            text        NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    last_seen_at      timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- Passwordless sign-in and verification codes, delivered by our own transactional mail.
-- Only the hash is stored; single use; attempts bounded; rate limited by (email_key, purpose).
CREATE TABLE login_codes (
    id           uuid        PRIMARY KEY DEFAULT uuidv7(),
    email_key    text        NOT NULL,
    purpose      text        NOT NULL CHECK (purpose IN ('sign_in', 'verify_email', 'invitation')),
    code_hash    bytea       NOT NULL,                                   -- HMAC of the 6-digit code under the deployment key (low entropy)
    link_token_hash bytea    NOT NULL UNIQUE,                            -- an independent 32-byte token for the magic link; confirmed by POST
    ceremony_id  uuid,                                                   -- the ceremony of the browser that asked: the 6-digit code is accepted only from that browser, the link from any
    attempts     smallint    NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 5),
    ip_hash      bytea,
    expires_at   timestamptz NOT NULL,
    consumed_at  timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX login_codes_open ON login_codes (email_key, purpose, created_at) WHERE consumed_at IS NULL;

-- Passkeys (webauthn-rs). Challenges live in a table so two api replicas can finish one ceremony.
CREATE TABLE passkeys (
    id               uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id          uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    credential_id    bytea       NOT NULL UNIQUE,                          -- extracted for lookup
    credential       jsonb       NOT NULL,                                 -- webauthn-rs `Passkey`, serialised whole: counter, backup flags, attestation, extensions; updated after every authentication
    name             text        NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    last_used_at     timestamptz
);
-- Owner recovery codes: single-use codes a person registers ahead of need (ten at a time, a new
-- set replacing the old one), kept as keyed hashes. An operator's break-glass for an owner of a
-- workspace that enforces single sign-on consumes one registered before the enforcement began:
-- the person's own proof, set up while they could still sign in normally.
CREATE TABLE recovery_codes (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash  bytea       NOT NULL,                                     -- the deployment key's MAC of the code's digits
    created_at timestamptz NOT NULL DEFAULT now(),
    used_at    timestamptz
);
CREATE INDEX recovery_codes_unused ON recovery_codes (user_id) WHERE used_at IS NULL;

-- Server-side ceremony state for passkeys (webauthn-rs registration/authentication state)
-- and OIDC sign-ins (state, nonce, PKCE verifier, return path): nothing of this lives in a
-- cookie, so two api replicas can finish one ceremony and a cookie cannot be replayed.
CREATE TABLE auth_ceremonies (
    id           uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id      uuid        REFERENCES users (id) ON DELETE CASCADE,  -- NULL for discoverable or first sign-in
    kind         text        NOT NULL CHECK (kind IN ('email_code', 'passkey_registration', 'passkey_authentication', 'oidc', 'sso',
                                                      'identity_link', 'mailbox_oauth')),  -- the purpose: every redirect ceremony returns to one callback, which dispatches on it
    browser_hash bytea       NOT NULL,                                  -- SHA-256 of the __Host-nb_ceremony cookie set at start: the finish must come from the same browser, so a callback URL carried elsewhere completes nothing
    state        bytea       NOT NULL,                                  -- sealed
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz,
    CHECK (kind NOT IN ('passkey_registration', 'identity_link', 'mailbox_oauth') OR user_id IS NOT NULL)   -- started by a signed-in user
);
ALTER TABLE login_codes ADD CONSTRAINT login_codes_ceremony_fk FOREIGN KEY (ceremony_id) REFERENCES auth_ceremonies (id) ON DELETE SET NULL;

-- OIDC sign-in through any provider (Google, Microsoft, a workspace's SSO connection).
-- The pair (issuer, subject) identifies a person; the same Google account reached through
-- the generic Google button or a workspace SSO connection is one link.
CREATE TABLE identity_links (
    issuer       text        NOT NULL,
    subject      text        NOT NULL,
    id           uuid        NOT NULL DEFAULT uuidv7() UNIQUE,     -- the API's idn_ id; the pair (issuer, subject) stays the key
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    email        text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    PRIMARY KEY (issuer, subject)
);
CREATE INDEX identity_links_by_user ON identity_links (user_id);

-- Browser sessions, our own: 32 random bytes in an HttpOnly cookie, only the hash stored.
-- The active workspace is a property of the session, switched by a membership-checked call.
CREATE TABLE sessions (
    id                   uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id              uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash           bytea       NOT NULL UNIQUE,
    auth_method          text        NOT NULL CHECK (auth_method IN ('email_code', 'passkey', 'oidc', 'sso', 'break_glass', 'impersonation')),  -- impersonation: an operator's 10-minute support session
    sso_connection_id    uuid,                                           -- the workspace SSO connection that authenticated this session, if any; for break_glass, the enforcing connection it stands in for
    sso_policy_version   integer,                                        -- sso_connections.policy_version at the proof; a changed policy invalidates the proof
    authenticated_at     timestamptz NOT NULL DEFAULT now(),             -- for SSO: the IdP's verified auth_time (max_age requested), not our clock; enforcement compares this
    active_workspace_id  uuid,                                           -- FK added after workspaces
    created_at           timestamptz NOT NULL DEFAULT now(),
    last_seen_at         timestamptz NOT NULL DEFAULT now(),
    expires_at           timestamptz NOT NULL,                           -- absolute (90 d)
    idle_expires_at      timestamptz NOT NULL,                           -- sliding (30 d)
    revoked_at           timestamptz,
    revoked_reason       text,
    ip_hash              bytea,
    user_agent           text
);
CREATE INDEX sessions_by_user ON sessions (user_id) WHERE revoked_at IS NULL;

-- Keys that sign our JWT access tokens (EdDSA). Public parts are served at the JWKS URL.
CREATE TABLE signing_keys (
    kid         text        PRIMARY KEY,
    algorithm   text        NOT NULL CHECK (algorithm IN ('EdDSA')),
    private_key bytea       NOT NULL,                                    -- sealed
    public_jwk  jsonb       NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    retired_at  timestamptz
);

-- ───────────────────────────── 3. Workspaces, memberships, keys, SSO, OAuth ─────────────────────────────
CREATE TABLE workspaces (
    id          uuid        PRIMARY KEY DEFAULT uuidv7(),
    slug        text        NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
    name        text        NOT NULL,
    mode        text        NOT NULL DEFAULT 'live' CHECK (mode IN ('live', 'test')),   -- test: fake transport, no provider calls
    timezone    text        NOT NULL DEFAULT 'UTC',
    settings    jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(settings) = 'object'),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz
);
ALTER TABLE sessions ADD CONSTRAINT sessions_active_workspace_fk
    FOREIGN KEY (active_workspace_id) REFERENCES workspaces (id) ON DELETE SET NULL;

CREATE TABLE memberships (
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    id           uuid        NOT NULL DEFAULT uuidv7() UNIQUE,            -- the API's mem_ id
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role         text        NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'viewer')),
    source       text        NOT NULL DEFAULT 'invitation' CHECK (source IN ('creator', 'invitation', 'sso_jit')),
    status       text        NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended', 'removed')),
    status_changed_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),                     -- a member is updated (role, status): its version
    PRIMARY KEY (workspace_id, user_id)
);
-- A removed or suspended membership stays as a tombstone: JIT provisioning creates a
-- membership only when no row exists, so a removed person is not re-admitted by signing in
-- again; an invitation accepted later revives the row explicitly.
CREATE INDEX memberships_by_user ON memberships (user_id) WHERE status = 'active';
-- "At least one owner" is enforced by the application transaction that changes roles,
-- under FOR UPDATE of the workspace row.

CREATE TABLE invitations (
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    id           uuid        NOT NULL DEFAULT uuidv7(),
    email        text        NOT NULL,
    email_key    text        NOT NULL GENERATED ALWAYS AS (ascii_lower(email)) STORED,
    role         text        NOT NULL CHECK (role IN ('admin', 'member', 'viewer')),
    token_hash   bytea       NOT NULL UNIQUE,
    invited_by   uuid        NOT NULL REFERENCES users (id),
    expires_at   timestamptz NOT NULL,
    accepted_at  timestamptz,
    accepted_by  uuid        REFERENCES users (id),
    revoked_at   timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);
CREATE UNIQUE INDEX invitations_pending_once ON invitations (workspace_id, email_key)
    WHERE accepted_at IS NULL AND revoked_at IS NULL;

-- Workspace API keys: `nb_live_` + base32 of 20 random bytes + checksum; SHA-256 stored.
CREATE TABLE api_keys (
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    id           uuid        NOT NULL DEFAULT uuidv7(),
    name         text        NOT NULL,
    prefix       text        NOT NULL,                                    -- first 12 chars, display only
    secret_hash  bytea       NOT NULL UNIQUE,
    scopes       text[]      NOT NULL,
    created_by   uuid        NOT NULL,                                   -- the delegating member: suspending or removing the membership (a tombstone) revokes the key in the same transaction; the cascade covers only a physical delete (workspace deletion)
    expires_at   timestamptz,
    last_used_at timestamptz,
    revoked_at   timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),                     -- a key is renamed: its version
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, created_by) REFERENCES memberships (workspace_id, user_id) ON DELETE CASCADE
);

-- A workspace's external identity provider (OIDC). SAML is not supported.
CREATE TABLE sso_connections (
    workspace_id        uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    id                  uuid        NOT NULL DEFAULT uuidv7(),
    kind                text        NOT NULL CHECK (kind IN ('oidc')),
    name                text        NOT NULL,
    issuer              text        NOT NULL,                             -- discovery: {issuer}/.well-known/openid-configuration
    client_id           text        NOT NULL,
    client_secret       bytea,                                            -- sealed; NULL for public clients
    metadata            jsonb       NOT NULL DEFAULT '{}',                -- cached discovery document
    metadata_fetched_at timestamptz,
    default_role        text        NOT NULL DEFAULT 'member' CHECK (default_role IN ('admin', 'member', 'viewer')),
    jit_provisioning    boolean     NOT NULL DEFAULT true,                -- create memberships on first sign-in
    enforced            boolean     NOT NULL DEFAULT false,               -- users of its domains must use it
    enforced_at         timestamptz,                                      -- when enforcement last turned on (NULL while off): an owner's recovery codes registered before it prove a break-glass
    policy_version      integer     NOT NULL DEFAULT 1,                   -- bumped by every change to issuer, client, domains, JIT, enforcement or role: proofs record it
    status              text        NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'active', 'disabled')),
    status_detail       text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);

-- Email domains routed to an SSO connection. A domain is claimed by one workspace once
-- verified (DNS TXT); until then it only routes nothing.
CREATE TABLE sso_email_domains (
    workspace_id       uuid        NOT NULL,
    sso_connection_id  uuid        NOT NULL,
    domain             text        NOT NULL CHECK (domain = ascii_lower(domain)),
    ownership_token    text        NOT NULL,
    verified_at        timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, domain),
    FOREIGN KEY (workspace_id, sso_connection_id) REFERENCES sso_connections (workspace_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX sso_email_domains_owner ON sso_email_domains (domain) WHERE verified_at IS NOT NULL;

-- Our own OAuth 2.1 authorization server, a minimal profile, for MCP clients and the CLI.
CREATE TABLE oauth_clients (
    client_id     text        PRIMARY KEY,                                -- an https URL for CIMD clients
    kind          text        NOT NULL CHECK (kind IN ('cimd', 'registered')),
    name          text        NOT NULL,
    redirect_uris text[]      NOT NULL,
    auth_method   text        NOT NULL DEFAULT 'none' CHECK (auth_method IN ('none', 'client_secret_basic')),
    secret_hash   bytea,                                                  -- confidential registered clients only
    metadata      jsonb       NOT NULL DEFAULT '{}',
    fetched_at    timestamptz,                                            -- CIMD cache timestamp
    created_at    timestamptz NOT NULL DEFAULT now(),
    CHECK ((auth_method = 'client_secret_basic') = (secret_hash IS NOT NULL)),
    CHECK (kind <> 'cimd' OR auth_method = 'none')
);
-- A grant records how the person authenticated when consenting (the workspace authentication
-- proof it inherits) and has an absolute lifetime; refresh tokens cannot outlive it, and SSO
-- enforcement compares authenticated_at and sso_connection_id like a session's.
CREATE TABLE oauth_grants (
    id                uuid        PRIMARY KEY DEFAULT uuidv7(),
    client_id         text        NOT NULL REFERENCES oauth_clients (client_id) ON DELETE CASCADE,
    user_id           uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id      uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    resource          text        NOT NULL,                               -- RFC 8707 resource: the MCP resource URL, or the API for the CLI's device grant
    scopes            text[]      NOT NULL,
    auth_method       text        NOT NULL CHECK (auth_method IN ('email_code', 'passkey', 'oidc', 'sso')),
    sso_connection_id uuid,                                               -- the connection that authenticated the consent, if any
    sso_policy_version integer,
    authenticated_at  timestamptz NOT NULL,                               -- the IdP's auth_time for SSO consents
    expires_at        timestamptz NOT NULL,                               -- absolute (90 d); refresh tokens expire with it
    created_at        timestamptz NOT NULL DEFAULT now(),
    last_used_at      timestamptz,
    revoked_at        timestamptz
);
CREATE INDEX oauth_grants_by_user ON oauth_grants (user_id) WHERE revoked_at IS NULL;
CREATE TABLE oauth_codes (
    code_hash      bytea       PRIMARY KEY,
    grant_id       uuid        NOT NULL REFERENCES oauth_grants (id) ON DELETE CASCADE,
    redirect_uri   text        NOT NULL,
    code_challenge text        NOT NULL,                                  -- PKCE S256, mandatory
    expires_at     timestamptz NOT NULL,
    consumed_at    timestamptz
);
-- Device authorization grant (RFC 8628) for the CLI: the device code is polled, the user code
-- is approved in the dashboard after sign-in and workspace choice, and approval creates the
-- grant the refresh chain hangs from.
CREATE TABLE oauth_device_codes (
    device_code_hash bytea       PRIMARY KEY,
    user_code        text        NOT NULL UNIQUE,                         -- 8 characters, shown to the person
    client_id        text        NOT NULL REFERENCES oauth_clients (client_id) ON DELETE CASCADE,
    resource         text        NOT NULL,
    scopes           text[]      NOT NULL,
    interval_seconds smallint    NOT NULL DEFAULT 5,
    expires_at       timestamptz NOT NULL,                                -- 10 minutes
    last_polled_at   timestamptz,
    approved_grant_id uuid       REFERENCES oauth_grants (id) ON DELETE CASCADE,
    denied_at        timestamptz,
    consumed_at      timestamptz,                                         -- the token response was issued once
    created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE oauth_refresh_tokens (
    token_hash   bytea       PRIMARY KEY,
    grant_id     uuid        NOT NULL REFERENCES oauth_grants (id) ON DELETE CASCADE,
    rotated_from bytea,                                                   -- the parent token of the same grant; reuse of a consumed parent revokes the chain
    expires_at   timestamptz NOT NULL,
    consumed_at  timestamptz,
    revoked_at   timestamptz,
    UNIQUE (grant_id, token_hash),
    UNIQUE (rotated_from),                                                -- one child per parent
    FOREIGN KEY (grant_id, rotated_from) REFERENCES oauth_refresh_tokens (grant_id, token_hash) ON DELETE CASCADE
);
CREATE UNIQUE INDEX oauth_refresh_tokens_one_root ON oauth_refresh_tokens (grant_id) WHERE rotated_from IS NULL;

CREATE TABLE audit_log (
    workspace_id uuid        NOT NULL,
    id           uuid        NOT NULL DEFAULT uuidv7(),
    actor_kind   text        NOT NULL CHECK (actor_kind IN ('user', 'api_key', 'oauth', 'system')),
    actor_id     text        NOT NULL,
    action       text        NOT NULL,                                    -- 'member.role_changed', 'api_key.created'
    target       text,
    details      jsonb       NOT NULL DEFAULT '{}',
    ip_hash      bytea,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);
-- What happened to a person's account outside any workspace: sign-ins, sign-outs and revoked
-- sessions, an operator's suspension. Keyed by the person: the api writes rows and reads only the
-- signed-in person's own (row security on norbelys.user_id); never changed once written; removed
-- by retention after 180 days, like audit_log.
CREATE TABLE user_audit_log (
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    id           uuid        NOT NULL DEFAULT uuidv7(),
    actor_kind   text        NOT NULL CHECK (actor_kind IN ('user', 'api_key', 'oauth', 'system')),
    actor_id     text        NOT NULL,
    action       text        NOT NULL,                                    -- 'session.created', 'user.suspended'
    target       text,
    details      jsonb       NOT NULL DEFAULT '{}',
    ip_hash      bytea,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, id)
);
CREATE INDEX user_audit_log_by_age ON user_audit_log (created_at);       -- retention

-- ───────────────────────────── 4. People ─────────────────────────────
CREATE TABLE people (
    workspace_id  uuid        NOT NULL REFERENCES workspaces (id),
    id            uuid        NOT NULL DEFAULT uuidv7(),
    email         text        NOT NULL,
    email_key     text        NOT NULL GENERATED ALWAYS AS (ascii_lower(email)) STORED,
    given_name    text,
    family_name   text,
    company       text,
    custom_fields jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(custom_fields) = 'object'),
    last_sent_at  timestamptz,
    replied_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, email_key)
);
CREATE INDEX people_by_created ON people (workspace_id, created_at, id);
CREATE INDEX people_by_updated ON people (workspace_id, updated_at, id);
CREATE INDEX people_by_company ON people (workspace_id, ascii_lower(company)) WHERE company IS NOT NULL;

CREATE TABLE person_field_definitions (
    workspace_id uuid   NOT NULL REFERENCES workspaces (id),
    id           uuid   NOT NULL DEFAULT uuidv7(),                        -- the API's fld_ id
    key          text   NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{0,63}$'),
    label        text   NOT NULL,
    field_type   text   NOT NULL CHECK (field_type IN ('text', 'number', 'boolean', 'enum', 'date')),
    options      text[] NOT NULL DEFAULT '{}',
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),                     -- the definition's version (a label or options change moves it)
    deleted_at   timestamptz,
    PRIMARY KEY (workspace_id, key),
    UNIQUE (workspace_id, id),
    CHECK ((field_type = 'enum' AND cardinality(options) BETWEEN 1 AND 100) OR (field_type <> 'enum' AND cardinality(options) = 0))
);
-- custom_fields is validated against the workspace's definitions on every write (the API
-- validates first with garde; this is the second net). Definitions are serialised against
-- writers with a shared advisory lock per workspace.
CREATE FUNCTION person_field_value_valid(kind text, options text[], value jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE AS $$
    SELECT value IS NULL OR value = 'null'::jsonb OR CASE kind
      WHEN 'text'    THEN jsonb_typeof(value) = 'string'
      WHEN 'number'  THEN jsonb_typeof(value) = 'number'
      WHEN 'boolean' THEN jsonb_typeof(value) = 'boolean'
      WHEN 'enum'    THEN jsonb_typeof(value) = 'string' AND (value #>> '{}') = ANY (options)
      WHEN 'date'    THEN jsonb_typeof(value) = 'string' AND (value #>> '{}') ~ '^\d{4}-\d{2}-\d{2}$'
      ELSE false END $$;
-- Deleted keys remain reserved until their fenced cleanup job finishes. Reads and writes
-- suppress their old values immediately; a later definition can never inherit old values.
CREATE FUNCTION visible_person_fields(workspace uuid, fields jsonb) RETURNS jsonb
    LANGUAGE sql STABLE AS $$
    SELECT fields - ARRAY(SELECT key FROM person_field_definitions
                          WHERE workspace_id = workspace AND deleted_at IS NOT NULL) $$;
REVOKE EXECUTE ON FUNCTION visible_person_fields(uuid, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION visible_person_fields(uuid, jsonb)
    TO norbelys_app, norbelys_worker, norbelys_system, norbelys_tracking;
CREATE FUNCTION enforce_person_fields() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock_shared(hashtextextended('people-fields:' || NEW.workspace_id::text, 0));
    NEW.custom_fields := visible_person_fields(NEW.workspace_id, NEW.custom_fields);
    IF EXISTS (SELECT 1 FROM person_field_definitions f WHERE f.workspace_id = NEW.workspace_id
                  AND f.deleted_at IS NULL
                  AND NOT person_field_value_valid(f.field_type, f.options, NEW.custom_fields -> f.key)) THEN
        RAISE EXCEPTION 'custom field violates workspace definition' USING ERRCODE = '23514', CONSTRAINT = 'people_field_definition';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER people_field_definition BEFORE INSERT OR UPDATE OF custom_fields ON people
    FOR EACH ROW EXECUTE FUNCTION enforce_person_fields();

CREATE TABLE groups (
    workspace_id uuid NOT NULL REFERENCES workspaces (id),
    id           uuid NOT NULL DEFAULT uuidv7(),
    name         text NOT NULL,
    description  text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    deleted_at   timestamptz,
    PRIMARY KEY (workspace_id, id)
);
CREATE TABLE group_people (
    workspace_id uuid NOT NULL,
    group_id     uuid NOT NULL,
    person_id    uuid NOT NULL,
    PRIMARY KEY (workspace_id, group_id, person_id),
    FOREIGN KEY (workspace_id, group_id)  REFERENCES groups (workspace_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE RESTRICT
);
CREATE INDEX group_people_by_person ON group_people (workspace_id, person_id);

-- A shared row lock serializes new memberships with the deletion's exclusive lock.
CREATE FUNCTION enforce_live_group() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM groups WHERE workspace_id = NEW.workspace_id AND id = NEW.group_id
                 AND deleted_at IS NULL FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'group does not exist' USING ERRCODE = '23503';
    END IF;
    RETURN NEW;
END $$;
REVOKE EXECUTE ON FUNCTION enforce_live_group() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION enforce_live_group() TO norbelys_app, norbelys_worker, norbelys_system;
CREATE TRIGGER group_people_live BEFORE INSERT ON group_people
    FOR EACH ROW EXECUTE FUNCTION enforce_live_group();

CREATE TABLE segments (
    workspace_id uuid  NOT NULL REFERENCES workspaces (id),
    id           uuid  NOT NULL DEFAULT uuidv7(),
    name         text  NOT NULL,
    filter       jsonb NOT NULL CHECK (jsonb_typeof(filter) = 'object'),
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);

-- Workspace-wide exclusions. Irreversible signals only; temporary holds are separate.
CREATE TABLE suppressions (
    workspace_id uuid NOT NULL REFERENCES workspaces (id),
    id           uuid NOT NULL DEFAULT uuidv7(),
    email        text NOT NULL,
    email_key    text NOT NULL GENERATED ALWAYS AS (ascii_lower(email)) STORED,
    reason       text NOT NULL CHECK (reason IN ('unsubscribe', 'bounce', 'complaint', 'manual',
                                                 'address_changed', 'account_closed', 'no_mail_service')),
    source_event uuid,                                                    -- delivery_events.id, when any
    created_by   text,                                                    -- actor id or 'system'
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- Who or what suppressed the address: a person ('manual'), the recipient's unsubscribe, or
    -- the source of the delivery event that proved it, in that table's vocabulary.
    source       text NOT NULL DEFAULT 'manual' CHECK (source IN ('manual', 'unsubscribe', 'smtp', 'provider_api',
                                                                  'provider_webhook', 'dsn', 'arf', 'inbound_notice')),
    -- A summary of that evidence, kept here because the event itself is archived within days
    -- while the suppression lasts.
    evidence     jsonb CHECK (evidence IS NULL OR jsonb_typeof(evidence) = 'object'),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, email_key)
);

CREATE TABLE imports (
    workspace_id  uuid  NOT NULL REFERENCES workspaces (id),
    id            uuid  NOT NULL DEFAULT uuidv7(),
    source        jsonb NOT NULL,                                         -- kind + object key; never the bytes
    group_id      uuid,
    job_id        uuid,                                                   -- the jobs row doing the work
    status        text  NOT NULL CHECK (status IN ('queued', 'processing', 'completed', 'failed')),
    cursor        bigint NOT NULL DEFAULT 0,
    total         bigint NOT NULL DEFAULT 0,
    imported      bigint NOT NULL DEFAULT 0,
    skipped       bigint NOT NULL DEFAULT 0,
    invalid       bigint NOT NULL DEFAULT 0,
    errors        jsonb  NOT NULL DEFAULT '[]',                           -- bounded sample
    last_error    jsonb,                                                  -- {code, detail, at}: why this asynchronous resource last failed, as its API object shows it
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    completed_at  timestamptz,
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, group_id) REFERENCES groups (workspace_id, id) ON DELETE SET NULL (group_id)
);
-- A requested file of workspace data (people, messages, events), written by a job to
-- object storage and served through a signed URL until it expires.
CREATE TABLE exports (
    workspace_id uuid  NOT NULL REFERENCES workspaces (id),
    id           uuid  NOT NULL DEFAULT uuidv7(),
    kind         text  NOT NULL CHECK (kind IN ('people', 'messages', 'attempts', 'delivery_events', 'inbound_messages')),
    filter       jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(filter) = 'object'),
    format       text  NOT NULL DEFAULT 'csv' CHECK (format IN ('csv', 'jsonl')),
    status       text  NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'ready', 'failed', 'expired')),
    job_id       uuid,
    object_key   text,
    rows         bigint,
    last_error   jsonb,                                                   -- {code, detail, at}
    requested_by text  NOT NULL,
    expires_at   timestamptz NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);

CREATE TABLE import_people (
    workspace_id uuid NOT NULL,
    import_id    uuid NOT NULL,
    person_id    uuid NOT NULL,
    PRIMARY KEY (workspace_id, import_id, person_id),
    FOREIGN KEY (workspace_id, import_id) REFERENCES imports (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE CASCADE
);

-- ───────────────────────────── 5. Senders ─────────────────────────────
-- A connection: one credential, one provider account, one budget, one pacing clock.
CREATE TABLE connections (
    workspace_id             uuid NOT NULL REFERENCES workspaces (id),
    id                       uuid NOT NULL DEFAULT uuidv7(),
    provider                 text NOT NULL CHECK (provider IN ('smtp', 'google', 'microsoft', 'ses', 'sendgrid', 'mailgun', 'norbelys')),
    transport                text NOT NULL CHECK (transport IN ('smtp', 'api')),             -- api: the Gmail API or Microsoft Graph; the relays (ses, sendgrid, mailgun) and the managed MTA submit over SMTP, their HTTP APIs not being transports
    account_email            text NOT NULL,                               -- the authenticated account; a relay's is the From address it paces (a paced SES connection), or a name the customer gives the account when it is rate-paced
    account_email_key        text NOT NULL GENERATED ALWAYS AS (ascii_lower(account_email)) STORED,
    account_issuer           text,                                        -- OAuth: the ID token's issuer (Google, or the Microsoft tenant)
    account_subject          text,                                        -- OAuth: its immutable subject (Google `sub`, Microsoft `oid`); kept through archive and email changes, so reconnecting the same account finds this row
    smtp                     jsonb,                                       -- {host, port, security, username}; SES also {configuration_set}, sent as X-SES-CONFIGURATION-SET on every message so SES publishes its events
    imap                     jsonb,                                       -- {host, port, security}; NULL = no IMAP
    credential               bytea,                                       -- sealed password or OAuth token set
    credential_version       bigint NOT NULL DEFAULT 1,
    status                   text NOT NULL CHECK (status IN ('unverified', 'verifying', 'active',
                                                             'authorization_required', 'failed', 'disabled', 'archived')),
    status_detail            text,
    paused                   boolean NOT NULL DEFAULT false,              -- the person's pause: temporary, its conversations wait for it and nothing is moved to another sender
    checked_at               timestamptz,                                 -- the last daily check of the credential and the account (the connection.check job)
    consecutive_failures     integer NOT NULL DEFAULT 0,                  -- the breaker's count: failures scoped to the connection since its last success; the third opens the breaker
    paused_until             timestamptz,                                 -- the breaker is open until then
    breaker_opened_at        timestamptz,                                 -- only a submission started after this instant may close the breaker
    probe_message_id         uuid,                                        -- half-open (pause over, failures counted): the one delivery_queue row admitted as the probe ...
    probe_generation         bigint,                                      -- ... and its lease generation; the probe is live while that row is leased in that generation, so a lost probe frees the slot
    timezone                 text NOT NULL DEFAULT 'UTC',
    send_window              jsonb,                                       -- {"days":[1..7],"start":"09:00","end":"17:00"}
    daily_limit              integer NOT NULL CHECK (daily_limit > 0),                 -- the connection's own daily budget; the providers' caps are held by the application
    send_interval_minutes    integer CHECK (send_interval_minutes IS NULL OR send_interval_minutes BETWEEN 5 AND 1440),  -- a paced sender's interval between scheduled cold sends, rounded up to whole slots (17 becomes 20): every mailbox, and an SES connection set to pace one From address; NULL: a rate-paced relay or the MTA, which send as fast as their limits allow. No default: the API sets 10 for a mailbox
    send_phase_seconds       smallint NOT NULL DEFAULT floor(random() * 300) CHECK (send_phase_seconds BETWEEN 0 AND 299),  -- a paced sender's fixed second inside each 5-minute slot
    next_send_at             timestamptz NOT NULL DEFAULT 'epoch',        -- the pacing clock: a paced sender's next scheduled cold send, a phase instant once set (by creation, resume, reactivation, the start of a cold submission, a window opening, the claim's catch-up of an idle mailbox), only ever moved forward; 'epoch' is due at once; a rate-paced connection's is never moved
    next_claim_at            timestamptz,                                 -- the claim's own wait: a spent daily budget (the connection's, its provider cap's or its scope's) skips the connection until it can free, at its phase; cleared by a settings change or its own release; never the pacing clock
    warmup_stage             smallint,                                    -- NULL = not warming; otherwise the stage of the ramp of daily limits
    warmup_evaluated_on      date,                                        -- the UTC day the warm-up was last evaluated for: one stage step per day, whatever retries
    created_by               uuid REFERENCES users (id) ON DELETE SET NULL, -- the member who connected it: `connections:manage` for members applies to their own
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    CHECK ((account_issuer IS NULL) = (account_subject IS NULL)),
    CHECK (provider NOT IN ('google', 'microsoft', 'smtp') OR send_interval_minutes IS NOT NULL),   -- every mailbox is paced
    CONSTRAINT connections_ses_configuration_set CHECK (provider <> 'ses' OR coalesce(smtp ->> 'configuration_set', '') ~ '^[A-Za-z0-9_-]{1,64}$'),   -- a set name as SES allows one; SES publishes events only for a message sent in a set; coalesced, because a CHECK passes a NULL
    CHECK (provider NOT IN ('norbelys', 'sendgrid', 'mailgun') OR send_interval_minutes IS NULL),   -- the managed MTA paces by its own admission; SendGrid and Mailgun events are read once per account, so the several connections per account that pacing implies (one per From address) would read them twice; an SES connection may be paced; the claim tells paced from rate-paced by the interval alone
    CHECK ((transport = 'smtp') = (smtp IS NOT NULL)),
    CHECK (transport = 'smtp' OR provider IN ('google', 'microsoft'))
);
CREATE INDEX connections_due ON connections (workspace_id, next_send_at, id) WHERE status = 'active' AND NOT paused;   -- the claim's candidates, read a page at a time after each replica's cursor
-- One live connection per provider account; an archived row keeps its email without blocking a
-- different account that later holds the address. The provider's subject identifies the account
-- itself, across email changes and archive: reconnecting restores the row with the same subject.
CREATE UNIQUE INDEX connections_live_account ON connections (workspace_id, provider, account_email_key) WHERE status <> 'archived';
CREATE UNIQUE INDEX connections_subject ON connections (workspace_id, account_issuer, account_subject) WHERE account_subject IS NOT NULL;
-- Within a workspace an address is one live paced sender whatever the way in (a mailbox by OAuth or a
-- password, or an SES connection set to pace), so its pacing and its provider's per-mailbox limits are
-- admitted in one place there. Across workspaces nothing is compared: a password login to an arbitrary
-- SMTP server proves no address, so exclusivity would let a stranger block or probe someone else's
-- mailbox; the same mailbox in two workspaces is admitted per connection, the provider's throttles
-- being the backstop.
CREATE UNIQUE INDEX connections_live_paced_sender ON connections (workspace_id, account_email_key) WHERE status <> 'archived' AND send_interval_minutes IS NOT NULL;
-- An archived connection keeps its row for history and has no credential: reconnecting the same
-- account restores this row.
ALTER TABLE connections ADD CONSTRAINT connections_archived_without_credential CHECK (status <> 'archived' OR credential IS NULL);

-- A From identity. Many per connection; all share the connection's budget and clock.
CREATE TABLE sender_identities (
    workspace_id   uuid NOT NULL,
    id             uuid NOT NULL DEFAULT uuidv7(),
    connection_id  uuid NOT NULL,
    email          text NOT NULL,
    email_key      text NOT NULL GENERATED ALWAYS AS (ascii_lower(email)) STORED,
    name           text,
    reply_to       text,
    signature_html text,
    signature_text text,
    tags           text[] NOT NULL DEFAULT '{}',
    enabled        boolean NOT NULL DEFAULT true,
    verified_at    timestamptz,                                           -- provider send-as confirmed
    archived_at    timestamptz,                                           -- set when its connection is archived, cleared when the connection is restored; an archived identity sends nothing and holds no address
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, connection_id, id),                             -- lets messages prove identity ∈ connection
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id) ON DELETE RESTRICT
);
CREATE INDEX sender_identities_by_tag ON sender_identities USING gin (tags);
-- An address is a From identity of one live connection in a workspace, so a message's sender is
-- never ambiguous. Only live identities hold their address: an archived connection keeps its
-- identities for the history that points at them (messages, threads), but another account may
-- then send as the same address, the same way a different account may take an archived
-- connection's address. Restoring the archived connection while a live identity holds one of its
-- addresses is refused, naming the live connection; archiving that one first lets it come back.
CREATE UNIQUE INDEX sender_identities_live_address ON sender_identities (workspace_id, email_key) WHERE archived_at IS NULL;

-- A provider-side limit of an account the customer owns, shared by several of its connections: a
-- Microsoft 365 tenant's external-recipient allowance, an SES account and Region, a relay account,
-- a Google Cloud project of the customer's own. The limits of Norbelys's own Google project and
-- Microsoft app are the platform's configuration, not a scope, and a mailbox's own limits are held
-- by its adapter. A connection without a scope is its own scope. Two kinds of limit: daily units
-- (messages and recipients, accounted durably in quota_scope_usage over today's and yesterday's
-- UTC buckets, so a rolling limit of N never admits 2N across midnight) and a short-window rate
-- (window_limit units of window_unit per window_seconds, enforced in process, each replica holding
-- its share). A provider answer that names the shared limit pauses the scope durably
-- (paused_until), so every replica sees it.
CREATE TABLE quota_scopes (
    workspace_id        uuid NOT NULL REFERENCES workspaces (id),
    id                  uuid NOT NULL DEFAULT uuidv7(),
    provider            text NOT NULL CHECK (provider IN ('google', 'microsoft', 'ses', 'sendgrid', 'mailgun', 'smtp', 'norbelys')),
    scope_key           text NOT NULL,                                   -- project id, tenant id, account:region, relay host
    messages_per_day    integer CHECK (messages_per_day > 0),
    recipients_per_day  integer CHECK (recipients_per_day > 0),
    window_limit        integer CHECK (window_limit > 0),                -- SES: the account and Region's maximum send rate (GetSendQuota), in recipients per second
    window_unit         text CHECK (window_unit IN ('requests', 'recipients', 'units')),   -- what one submission is charged: one request, its recipients, or provider units
    window_seconds      integer CHECK (window_seconds BETWEEN 1 AND 3600),
    paused_until        timestamptz,
    paused_detail       text,
    consecutive_failures integer NOT NULL DEFAULT 0,                    -- the scope's breaker: failures scoped to the scope since its last success
    breaker_opened_at   timestamptz,
    probe_message_id    uuid,                                           -- one probe per scope across all its connections and replicas,
    probe_generation    bigint,                                         -- owned by a delivery_queue row and generation like the connection's
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, provider, scope_key),
    CHECK ((window_limit IS NULL) = (window_seconds IS NULL) AND (window_limit IS NULL) = (window_unit IS NULL))
);
CREATE TABLE quota_scope_usage (
    workspace_id        uuid    NOT NULL,
    scope_id            uuid    NOT NULL,
    day                 date    NOT NULL,
    messages_reserved   integer NOT NULL DEFAULT 0 CHECK (messages_reserved >= 0),     -- one unit per attempt
    messages_used       integer NOT NULL DEFAULT 0 CHECK (messages_used >= 0),
    recipients_reserved integer NOT NULL DEFAULT 0 CHECK (recipients_reserved >= 0),   -- one unit per envelope address
    recipients_used     integer NOT NULL DEFAULT 0 CHECK (recipients_used >= 0),
    PRIMARY KEY (workspace_id, scope_id, day),
    FOREIGN KEY (workspace_id, scope_id) REFERENCES quota_scopes (workspace_id, id) ON DELETE CASCADE
);
ALTER TABLE connections ADD COLUMN quota_scope_id uuid;
ALTER TABLE connections ADD CONSTRAINT connections_quota_scope_fk
    FOREIGN KEY (workspace_id, quota_scope_id) REFERENCES quota_scopes (workspace_id, id) ON DELETE SET NULL (quota_scope_id);
ALTER TABLE connections ADD CONSTRAINT connections_ses_scope CHECK (provider <> 'ses' OR quota_scope_id IS NOT NULL);   -- an SES account's send rate and daily quota live on its scope, so every SES connection names one; the SET NULL above makes deleting a scope that SES connections name fail

-- The inbox actually read. One per connection and folder; its cursor is provider-owned.
CREATE TABLE receive_bindings (
    workspace_id     uuid NOT NULL,
    id               uuid NOT NULL DEFAULT uuidv7(),
    connection_id    uuid NOT NULL,
    folder           text NOT NULL DEFAULT 'INBOX',
    enabled          boolean NOT NULL DEFAULT true,                      -- false when the person stops reading it or the connection is archived
    cursor           jsonb,                                               -- {uid_validity,last_uid} | {history_id} | {delta_link}
    next_poll_at     timestamptz NOT NULL DEFAULT now(),                 -- a poll sets it to its start plus the poll interval (5 minutes), to now when its page came back full, or to the failure backoff
    polled_at        timestamptz,
    lease_owner      text,
    lease_generation bigint NOT NULL DEFAULT 0,                          -- fence: cursor advances only WHERE lease_owner = $o AND lease_generation = $g
    lease_expires_at timestamptz,
    failures         integer NOT NULL DEFAULT 0,
    status_detail    text,
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, connection_id, folder),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX receive_bindings_due ON receive_bindings (next_poll_at) WHERE lease_owner IS NULL AND enabled;
CREATE INDEX receive_bindings_expired_lease ON receive_bindings (lease_expires_at) WHERE lease_expires_at IS NOT NULL;
CREATE TRIGGER receive_bindings_registered AFTER INSERT ON receive_bindings
    REFERENCING NEW TABLE AS inserted FOR EACH STATEMENT EXECUTE FUNCTION register_dispatch_workspace();

-- Daily budget ledger per connection (UTC day). `reserved` = claimed without an outcome;
-- `used` = accepted or uncertain. Settled exactly once per attempt, against the day it reserved on.
-- Pruned after 35 days only where no attempt references the row (NOT EXISTS).
CREATE TABLE connection_usage (
    workspace_id  uuid    NOT NULL,
    connection_id uuid    NOT NULL,
    day           date    NOT NULL,
    reserved      integer NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    used          integer NOT NULL DEFAULT 0 CHECK (used >= 0),
    PRIMARY KEY (workspace_id, connection_id, day),
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id) ON DELETE RESTRICT
);

-- Customer domains: ownership (TXT), optional tracking host (CNAME + TLS). Managed sending
-- (DKIM) will be a later capability on the same row.
CREATE TABLE sending_domains (
    workspace_id         uuid NOT NULL REFERENCES workspaces (id),
    id                   uuid NOT NULL DEFAULT uuidv7(),
    hostname             text NOT NULL CHECK (hostname = ascii_lower(hostname)),
    status               text NOT NULL CHECK (status IN ('pending_verification', 'verifying', 'verified',
                                                         'pending_certificate', 'active', 'suspended', 'deleting')),
    last_error           jsonb,                                           -- {code, detail, at}
    ownership_token      text NOT NULL,
    ownership_expires_at timestamptz NOT NULL,
    tracking_enabled     boolean NOT NULL DEFAULT false,
    dns_checks           jsonb NOT NULL DEFAULT '{}',                     -- last observed SPF/DKIM/DMARC/MX/CNAME
    verified_at          timestamptz,
    activated_at         timestamptz,
    checked_at           timestamptz,
    next_check_at        timestamptz NOT NULL DEFAULT now(),
    -- Consecutive checks that found the managed MTA not ready for the domain (not verified, no
    -- key yet, unreachable): the next check backs off with them, up to the daily recheck.
    mta_unready_checks   integer NOT NULL DEFAULT 0 CHECK (mta_unready_checks >= 0),
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, hostname)
);
CREATE UNIQUE INDEX sending_domains_owner ON sending_domains (hostname) WHERE status <> 'pending_verification';
CREATE INDEX sending_domains_due ON sending_domains (next_check_at);

-- Provider webhooks: one inbound evidence source per connection (Mailgun, SendGrid, SES/SNS,
-- the managed MTA). The route is /webhooks/{id}; the
-- workspace is resolved from the id by a SECURITY DEFINER lookup before RLS applies.
CREATE TABLE provider_webhooks (
    workspace_id   uuid NOT NULL REFERENCES workspaces (id),
    id             uuid NOT NULL DEFAULT uuidv7(),
    connection_id  uuid NOT NULL,
    provider       text NOT NULL CHECK (provider IN ('mailgun', 'sendgrid', 'ses', 'norbelys')),
    name           text NOT NULL,
    signing_secret bytea,                                                 -- sealed verification material: the secret Norbelys generates for the managed MTA; for a relay the provider's own key or topic, NULL until the customer pastes it (SendGrid shows its key only once our URL is configured), and callbacks are refused meanwhile
    status         text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    last_event_at  timestamptz,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (id),
    UNIQUE (workspace_id, connection_id),
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id) ON DELETE CASCADE
);
-- Resolves an invitation token to its row before any workspace context exists (the accept
-- flow has a user and a token, no workspace yet). Same ownership and policy model as the
-- webhook lookup.
CREATE FUNCTION invitation_by_token(hash bytea)
    RETURNS TABLE (workspace_id uuid, id uuid, email_key text, role text, expires_at timestamptz, accepted_at timestamptz, revoked_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    AS $$ SELECT i.workspace_id, i.id, i.email_key, i.role, i.expires_at, i.accepted_at, i.revoked_at FROM invitations i WHERE i.token_hash = hash $$;

-- Resolves an API key's hash to its row before any workspace context exists: a key names no
-- workspace, so authentication must find it first. Same ownership and policy model as the
-- invitation lookup.
CREATE FUNCTION api_key_by_hash(hash bytea)
    RETURNS TABLE (workspace_id uuid, id uuid, scopes text[], created_by uuid, expires_at timestamptz, revoked_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    AS $$ SELECT k.workspace_id, k.id, k.scopes, k.created_by, k.expires_at, k.revoked_at FROM api_keys k WHERE k.secret_hash = hash $$;

-- Routes an email domain to the SSO connection that verified it, before any workspace context
-- exists: a sign-in names only an email address. Only a verified domain of an active connection
-- routes, and a domain is verified for one workspace at most (sso_email_domains_owner). Same
-- ownership and policy model as the lookups above.
CREATE FUNCTION sso_route(email_domain text)
    RETURNS TABLE (workspace_id uuid, sso_connection_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    AS $$ SELECT d.workspace_id, d.sso_connection_id FROM sso_email_domains d
            JOIN sso_connections c ON c.workspace_id = d.workspace_id AND c.id = d.sso_connection_id
           WHERE d.domain = email_domain AND d.verified_at IS NOT NULL AND c.status = 'active' $$;

-- Resolves a webhook URL's workspace before any tenant context exists. Owned by norbelys_lookup,
-- which has one SELECT policy on provider_webhooks and nothing else; forced row security
-- applies to owners too, so a definer owned by norbelys_owner would see no rows here.
CREATE FUNCTION provider_webhook_workspace(webhook uuid) RETURNS uuid
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    RETURN (SELECT workspace_id FROM provider_webhooks WHERE id = webhook AND status = 'active');

-- ───────────────────────────── 6. Campaigns ─────────────────────────────
CREATE TABLE campaigns (
    workspace_id          uuid NOT NULL REFERENCES workspaces (id),
    id                    uuid NOT NULL DEFAULT uuidv7(),
    name                  text NOT NULL,
    status                text NOT NULL DEFAULT 'draft'
                          CHECK (status IN ('draft', 'materialising', 'active', 'paused', 'completed', 'archived')),
    last_error            jsonb,                                          -- {code, detail, at}: set when materialisation fails
    timezone              text NOT NULL DEFAULT 'UTC',
    send_window           jsonb,
    start_at              timestamptz,
    tracking_domain_id    uuid,
    track_opens           boolean NOT NULL DEFAULT false,
    track_clicks          boolean NOT NULL DEFAULT false,
    stop_on_reply         text NOT NULL DEFAULT 'all' CHECK (stop_on_reply IN ('all', 'campaign', 'none')),
    sender_tags           text[] NOT NULL DEFAULT '{}',                    -- the pool also takes every enabled identity with one of these tags, read when a sender is assigned
    on_sender_removed     text NOT NULL DEFAULT 'reassign' CHECK (on_sender_removed IN ('reassign', 'stop')),
    stop_company_on_reply boolean NOT NULL DEFAULT false,
    cooldown_hours        integer NOT NULL DEFAULT 72 CHECK (cooldown_hours >= 0),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, tracking_domain_id) REFERENCES sending_domains (workspace_id, id)
        ON DELETE SET NULL (tracking_domain_id)
);

-- The pool of From identities a campaign rotates over, named explicitly; the identities matching
-- campaigns.sender_tags join it at assignment time. Capacity is the connection's.
CREATE TABLE campaign_senders (
    workspace_id       uuid NOT NULL,
    campaign_id        uuid NOT NULL,
    sender_identity_id uuid NOT NULL,
    PRIMARY KEY (workspace_id, campaign_id, sender_identity_id),
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, sender_identity_id) REFERENCES sender_identities (workspace_id, id) ON DELETE CASCADE
);
-- Rotation state per campaign and identity, apart from how the identity entered the pool (named in
-- campaign_senders or carrying one of campaigns.sender_tags): a row exists once the identity was
-- assigned, and removing its tag removes it from the pool whatever this row says. Assignments of one
-- campaign are serialised by the campaign's row lock, so two concurrent assignments see each
-- other's rotation and pick different identities.
CREATE TABLE campaign_sender_rotation (
    workspace_id       uuid NOT NULL,
    campaign_id        uuid NOT NULL,
    sender_identity_id uuid NOT NULL,
    last_assigned_at   timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, campaign_id, sender_identity_id),
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, sender_identity_id) REFERENCES sender_identities (workspace_id, id) ON DELETE CASCADE
);
-- Sticky sender per conversation: follow-ups leave from the identity that started the thread.
CREATE TABLE campaign_sender_affinity (
    workspace_id       uuid NOT NULL,
    campaign_id        uuid NOT NULL,
    person_id          uuid NOT NULL,
    sender_identity_id uuid NOT NULL,
    assigned_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, campaign_id, person_id),
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, sender_identity_id) REFERENCES sender_identities (workspace_id, id) ON DELETE CASCADE
);

-- Steps belong to the campaign directly (no `sequences` table).
CREATE TABLE steps (
    workspace_id     uuid NOT NULL,
    id               uuid NOT NULL DEFAULT uuidv7(),
    campaign_id      uuid NOT NULL,
    position         integer NOT NULL CHECK (position > 0),
    name             text NOT NULL,
    current_revision integer CHECK (current_revision > 0),               -- NULL while drafting
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, campaign_id, position) DEFERRABLE INITIALLY DEFERRED,
    UNIQUE (workspace_id, campaign_id, id),
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id)
);

-- Published, immutable configuration of a step. Messages reference (step, revision).
CREATE TABLE step_revisions (
    workspace_id               uuid NOT NULL,
    step_id                    uuid NOT NULL,
    revision                   integer NOT NULL CHECK (revision > 0),
    delay_seconds              integer NOT NULL CHECK (delay_seconds BETWEEN 0 AND 31536000),
    same_thread                boolean NOT NULL DEFAULT true,
    ranking_objective          text NOT NULL CHECK (ranking_objective IN ('opens', 'clicks', 'replies')),
    observation_window_seconds integer NOT NULL CHECK (observation_window_seconds BETWEEN 1 AND 31536000),
    minimum_sample             integer NOT NULL CHECK (minimum_sample > 0),
    allocation                 text NOT NULL DEFAULT 'balanced' CHECK (allocation IN ('balanced', 'weighted', 'automatic')),
    winner_variant_id          uuid,
    winner_variant_version     integer,
    winner_selected_at         timestamptz,
    personalisation_prompt     text CHECK (char_length(personalisation_prompt) BETWEEN 1 AND 4000),   -- what the AI writes the snippets of each message from; NULL: the templates alone
    created_at                 timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, step_id, revision),
    FOREIGN KEY (workspace_id, step_id) REFERENCES steps (workspace_id, id),
    CHECK ((winner_variant_id IS NULL) = (winner_variant_version IS NULL)
       AND (winner_variant_id IS NULL) = (winner_selected_at IS NULL))
);
ALTER TABLE steps ADD CONSTRAINT steps_current_revision_fk
    FOREIGN KEY (workspace_id, id, current_revision) REFERENCES step_revisions (workspace_id, step_id, revision);

CREATE TABLE variants (
    workspace_id uuid NOT NULL,
    id           uuid NOT NULL DEFAULT uuidv7(),
    step_id      uuid NOT NULL,
    name         text NOT NULL,
    version      integer NOT NULL DEFAULT 1 CHECK (version > 0),         -- latest published version
    retired_at   timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, step_id, id),                                   -- lets messages prove variant ∈ step
    FOREIGN KEY (workspace_id, step_id) REFERENCES steps (workspace_id, id)
);
-- Every content version a variant ever had. Messages reference these, never copy bodies.
CREATE TABLE variant_revisions (
    workspace_id uuid NOT NULL,
    variant_id   uuid NOT NULL,
    version      integer NOT NULL CHECK (version > 0),
    subject      text NOT NULL,
    preheader    text,
    html         text NOT NULL,
    text         text,
    cc           text[] NOT NULL DEFAULT '{}',
    bcc          text[] NOT NULL DEFAULT '{}',
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, variant_id, version),
    FOREIGN KEY (workspace_id, variant_id) REFERENCES variants (workspace_id, id)
);
-- The variants a published revision offers, with their weights: one row per option, so
-- assignments, winners and messages prove membership in the revision by foreign key, which a
-- JSON array of options could not.
CREATE TABLE step_revision_variants (
    workspace_id    uuid NOT NULL,
    step_id         uuid NOT NULL,
    step_revision   integer NOT NULL,
    variant_id      uuid NOT NULL,
    variant_version integer NOT NULL,
    weight          integer NOT NULL DEFAULT 1 CHECK (weight BETWEEN 1 AND 100),
    PRIMARY KEY (workspace_id, step_id, step_revision, variant_id),
    UNIQUE (workspace_id, step_id, step_revision, variant_id, variant_version),
    FOREIGN KEY (workspace_id, step_id, step_revision) REFERENCES step_revisions (workspace_id, step_id, revision) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, step_id, variant_id) REFERENCES variants (workspace_id, step_id, id),
    FOREIGN KEY (workspace_id, variant_id, variant_version) REFERENCES variant_revisions (workspace_id, variant_id, version)
);
ALTER TABLE step_revisions ADD CONSTRAINT step_revisions_winner_fk
    FOREIGN KEY (workspace_id, step_id, revision, winner_variant_id, winner_variant_version)
    REFERENCES step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version);

-- One stable variant assignment per person, step and configuration revision.
CREATE TABLE step_assignments (
    workspace_id    uuid NOT NULL,
    step_id         uuid NOT NULL,
    step_revision   integer NOT NULL,
    person_id       uuid NOT NULL,
    variant_id      uuid NOT NULL,
    variant_version integer NOT NULL,
    assigned_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, step_id, step_revision, person_id),
    FOREIGN KEY (workspace_id, step_id, step_revision, variant_id, variant_version)
        REFERENCES step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version),
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE CASCADE
);
CREATE TABLE step_winner_selections (
    workspace_id    uuid NOT NULL,
    id              uuid NOT NULL DEFAULT uuidv7(),
    step_id         uuid NOT NULL,
    step_revision   integer NOT NULL,
    variant_id      uuid NOT NULL,
    variant_version integer NOT NULL,
    objective       text NOT NULL CHECK (objective IN ('opens', 'clicks', 'replies')),
    selected_by     text NOT NULL,                                        -- 'automatic' or a user id
    evidence        jsonb NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, step_id, step_revision, variant_id, variant_version)
        REFERENCES step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version)
);

CREATE TABLE enrollments (
    workspace_id           uuid NOT NULL,
    id                     uuid NOT NULL DEFAULT uuidv7(),
    campaign_id            uuid NOT NULL,
    person_id              uuid NOT NULL,
    status                 text NOT NULL DEFAULT 'active'
                           CHECK (status IN ('active', 'paused', 'completed', 'replied', 'stopped', 'failed')),
    current_position       integer NOT NULL DEFAULT 1 CHECK (current_position > 0),
    next_run_at            timestamptz,                                   -- NULL while a message of this step is in flight
    paused_until           timestamptz,
    message_id             uuid,                                          -- the in-flight message for the current step
    thread_root_message_id uuid,                                          -- step-1 message: follow-ups reply in its thread
    attempts               smallint NOT NULL DEFAULT 0,                   -- unstarted failures of the current step
    status_detail          text,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, campaign_id, id),                               -- lets messages prove enrollment ∈ campaign
    UNIQUE (workspace_id, campaign_id, id, person_id),                    -- and that the message's person is the enrolled one
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX enrollments_active_once ON enrollments (workspace_id, campaign_id, person_id)
    WHERE status IN ('active', 'paused');
CREATE INDEX enrollments_due ON enrollments (workspace_id, next_run_at)
    WHERE status = 'active' AND next_run_at IS NOT NULL;
CREATE INDEX enrollments_by_person ON enrollments (workspace_id, person_id);
CREATE INDEX enrollments_by_campaign ON enrollments (workspace_id, campaign_id, status);
-- The enrollments whose current message has not been settled: enrollment.advance reads them on every
-- pass to move each one on once its message ends, so a pass reads the conversations in flight, not
-- every enrollment of the workspace (settling clears message_id).
CREATE INDEX enrollments_in_flight ON enrollments (workspace_id, message_id) WHERE message_id IS NOT NULL;
CREATE TRIGGER enrollments_registered AFTER INSERT ON enrollments
    REFERENCING NEW TABLE AS inserted FOR EACH STATEMENT EXECUTE FUNCTION register_dispatch_workspace();

-- Images a workspace uploaded for its mail (POST /v1/images), served to anyone at their public URL
-- from object storage, where each lives at images/<workspace>/<id>.<extension>. The row is what the
-- API knows exists: a deletion finds its object through it, and a workspace's images are its rows.
-- The public route reads the object alone, so an image in sent mail keeps loading while the
-- database is unavailable.
CREATE TABLE images (
    workspace_id uuid        NOT NULL REFERENCES workspaces (id),
    id           uuid        NOT NULL DEFAULT uuidv7(),
    content_type text        NOT NULL CHECK (content_type IN ('image/png', 'image/jpeg', 'image/gif', 'image/webp')),
    size_bytes   integer     NOT NULL CHECK (size_bytes BETWEEN 1 AND 16777216),   -- the upload limit, 16 MiB
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);

-- ───────────────────────────── 7. Delivery ─────────────────────────────
-- One logical email with an immutable Message-ID. Campaign mail references a variant
-- revision and a render context; direct, reply and transactional mail own their content.
CREATE TABLE messages (
    workspace_id          uuid NOT NULL,
    id                    uuid NOT NULL DEFAULT uuidv7(),
    kind                  text NOT NULL CHECK (kind IN ('campaign', 'direct', 'reply', 'transactional')),
    campaign_id           uuid,
    step_id               uuid,
    step_revision         integer,
    variant_id            uuid,
    variant_version       integer,
    enrollment_id         uuid,
    sender_identity_id    uuid NOT NULL,
    connection_id         uuid NOT NULL,                                  -- frozen at acceptance
    from_email            text NOT NULL,                                  -- frozen From identity
    from_name             text,
    person_id             uuid,
    to_addresses          text[] NOT NULL CHECK (cardinality(to_addresses) BETWEEN 1 AND 50),
    cc                    text[] NOT NULL DEFAULT '{}',
    bcc                   text[] NOT NULL DEFAULT '{}',
    reply_to              text,
    recipient_count       integer GENERATED ALWAYS AS (cardinality(to_addresses) + cardinality(cc) + cardinality(bcc)) STORED,
    primary_recipient_key text NOT NULL GENERATED ALWAYS AS (ascii_lower(to_addresses[1])) STORED,
    subject               text NOT NULL,
    html                  text,                                           -- not for campaign mail
    text_body             text,
    render_context        jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(render_context) = 'object'),
    snippets_fallback     text CHECK (snippets_fallback IN ('off', 'unavailable', 'unusable', 'over_budget', 'paused',
                                                            'deadline', 'refused', 'truncated', 'invalid', 'provider')),   -- a step asking for personalisation snippets got none, and why: its template's defaults were used; NULL otherwise
    render_version        text NOT NULL,
    rendered_at           timestamptz NOT NULL,
    internet_message_id   text NOT NULL,
    in_reply_to           text,
    thread_id             uuid,
    tracking              jsonb NOT NULL DEFAULT '{}',                    -- {opens,clicks,hostname} frozen at acceptance from the campaign (hostname null: the platform's tracking host); {} for mail that tracks nothing
    send_at               timestamptz NOT NULL,
    idempotency_key       text,
    state                 text NOT NULL DEFAULT 'queued'
                          CHECK (state IN ('queued', 'claimed', 'in_flight', 'sent', 'failed',
                                           'cancelled', 'uncertain', 'suppressed')),   -- no state for generation: AI-generated content exists before the row is inserted
    status_detail         text,
    attempt_number        integer NOT NULL DEFAULT 0 CHECK (attempt_number >= 0),
    trace_parent          text,                                           -- W3C traceparent of the accepting request
    sent_at               timestamptz,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    deleted_at            timestamptz,
    PRIMARY KEY (workspace_id, id),
    -- The Message-ID is derived from the row: '<' || id || '.' || thread_id || '.' || tag || '@' || domain || '>'
    -- (tag = truncated HMAC of id||thread_id), so its uniqueness follows from the key, a reply's
    -- In-Reply-To yields the thread without reading this table (threads outlive archived months),
    -- and no unique index is needed, which a partitioned table could only have with the key in it.
    -- Leases live on delivery_queue (the active set); this row is history plus current state.
    CHECK (state <> 'sent' OR sent_at IS NOT NULL),
    CHECK ((kind = 'campaign') = (campaign_id IS NOT NULL AND step_id IS NOT NULL AND step_revision IS NOT NULL
                                  AND variant_id IS NOT NULL AND variant_version IS NOT NULL AND enrollment_id IS NOT NULL
                                  AND person_id IS NOT NULL)),
    CHECK (kind = 'campaign' OR (campaign_id IS NULL AND step_id IS NULL AND step_revision IS NULL
                                 AND variant_id IS NULL AND variant_version IS NULL AND enrollment_id IS NULL)),
    CHECK (kind = 'campaign' OR (html IS NOT NULL OR text_body IS NOT NULL)),
    CHECK (kind <> 'campaign' OR html IS NULL),
    CHECK (kind = 'campaign' OR snippets_fallback IS NULL),
    -- The whole envelope is bounded here (attempts carry the same bound).
    CHECK (cardinality(to_addresses) + cardinality(cc) + cardinality(bcc) BETWEEN 1 AND 150),
    -- The 7-day scheduling cap (a message is accepted only if due within 7 days) is the API's
    -- rule and the accept operation's check, not a CHECK here: what keeps a live message out of an
    -- archived period is the archive job's gate (no queue row and no unsettled attempt in the
    -- period), backed by the delivery queue's foreign key, which makes PostgreSQL refuse to detach
    -- a period holding a queued message (see delivery_queue). Campaigns create a step's message
    -- when it is due.
    -- Ancestry: each reference proves membership in the previous one, not only existence.
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id),
    FOREIGN KEY (workspace_id, campaign_id, step_id) REFERENCES steps (workspace_id, campaign_id, id),
    FOREIGN KEY (workspace_id, step_id, step_revision, variant_id, variant_version)
        REFERENCES step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version),
    FOREIGN KEY (workspace_id, campaign_id, enrollment_id, person_id) REFERENCES enrollments (workspace_id, campaign_id, id, person_id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, connection_id, sender_identity_id) REFERENCES sender_identities (workspace_id, connection_id, id),
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id),
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE RESTRICT
) PARTITION BY RANGE (id);
-- Idempotency is guaranteed by idempotency_keys (the stored response); the column here is for display and has no index.
-- The connection, identity, person, ancestry, content and send_at of a message are immutable
-- after insert: the serving roles hold UPDATE on the state columns only (the grants at the end of
-- this file), so the copies of connection_id on delivery_queue and attempts, written from the row
-- in the same transaction, cannot drift from it, without a foreign key or trigger to compare them.
CREATE INDEX messages_by_connection ON messages (workspace_id, connection_id, id);
CREATE INDEX messages_by_campaign ON messages (workspace_id, campaign_id, id) WHERE campaign_id IS NOT NULL;
CREATE INDEX messages_by_person ON messages (workspace_id, person_id, id) WHERE person_id IS NOT NULL;
CREATE INDEX messages_by_recipient ON messages (workspace_id, primary_recipient_key, id);
CREATE INDEX messages_by_thread ON messages (workspace_id, thread_id) WHERE thread_id IS NOT NULL;
CREATE INDEX messages_by_internet_id ON messages (workspace_id, internet_message_id) WHERE state IN ('in_flight', 'sent', 'uncertain');
-- A connection's uncertain messages, which its connection.check reads again by Message-ID in the
-- mailbox's Sent folder until none is left unvisited: few rows (uncertain is rare), each read once a check.
CREATE INDEX messages_uncertain ON messages (workspace_id, connection_id, id) WHERE state = 'uncertain';
ALTER TABLE enrollments ADD CONSTRAINT enrollments_message_fk
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id);

-- The active delivery set: one small unpartitioned row per unresolved message, carrying its
-- lease. Claims scan this table, never message history. Its foreign key to messages is the
-- archive's backstop: PostgreSQL refuses to detach a period that still holds an unresolved
-- message. That check reads this table and the detached leaf as the owner, which runs the
-- archive's DDL, so the owner may read both (the read-only detach policies at the end of this
-- file); under forced row security it would otherwise see no row and let the detach through.
-- The row is deleted when the message reaches a terminal state.
CREATE TABLE delivery_queue (
    workspace_id       uuid NOT NULL,
    message_id         uuid NOT NULL,
    connection_id      uuid NOT NULL,
    run_at             timestamptz NOT NULL,                              -- the message's send_at, or the retry time
    state              text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'claimed', 'in_flight')),
    paced              boolean NOT NULL DEFAULT true,                     -- cold campaign mail: on a paced sender it follows the clock, one claimed at a time; false: mail created through the API, claimed when due, outside the clock and the send windows; on a rate-paced connection both alike
    lease_owner        text,
    lease_generation   bigint NOT NULL DEFAULT 0,                         -- fence with lease_owner for every later write
    lease_expires_at   timestamptz,
    submission_started_at timestamptz,                                    -- set per message immediately before its own submission
    first_submitted_at timestamptz,                                       -- set once, by the start of the first submission; never changed
    expires_at         timestamptz,                                       -- the message's own usefulness (a sign-in code, an invitation); NULL for campaign and direct mail
    deadline_at        timestamptz,                                       -- least(expires_at, first_submitted_at + the retry window, 24 hours by default): nothing is submitted after it; NULL until either exists
    reserved_day       date,
    PRIMARY KEY (workspace_id, message_id),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK ((state = 'queued') = (lease_owner IS NULL)),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id),
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id)
) WITH (autovacuum_vacuum_scale_factor = 0.02, autovacuum_vacuum_threshold = 1000, fillfactor = 80);
CREATE INDEX delivery_due ON delivery_queue (workspace_id, connection_id, run_at, message_id) WHERE state = 'queued';
CREATE INDEX delivery_leased ON delivery_queue (lease_expires_at) WHERE state <> 'queued';
CREATE INDEX delivery_unpaced ON delivery_queue (workspace_id, connection_id, run_at) WHERE state = 'queued' AND NOT paced;   -- mail created through the API, found by the claim's second scan whatever the pacing clock says
CREATE INDEX delivery_in_progress ON delivery_queue (workspace_id, connection_id) WHERE state <> 'queued';   -- a paced sender has at most one message claimed or in flight, so its provider's per-mailbox concurrency is shared by construction
CREATE INDEX delivery_expiring ON delivery_queue (deadline_at) WHERE state = 'queued' AND deadline_at IS NOT NULL;   -- delivery.expire

-- The slot projection: an estimate of the known sends of the next 12 slots (one hour), zero-filled.
-- paced: a paced sender's cold rows due within the hour, queued or claimed, in run_at order: the first at its clock (an
--        overdue clock in the current slot) and each next one ceil(interval / 5) slots after the previous, never before
--        the slot of its own run_at; the k-th send's slot is (k - 1) * every + the running maximum of d_j - (j - 1) * every,
--        d_j being row j's slot after the first (at least 0).
-- due:   queued or claimed rows of rate-paced connections, and mail created through the API, in the slot of their run_at
--        (overdue in the current slot).
-- Windows, budgets, breakers, one message in progress at a time and work created later are not modelled. The metric
-- reads it as the scheduler (every workspace); the app may read it too, its workspace only (RLS), though no API
-- operation serves it yet.
CREATE VIEW slot_projection WITH (security_invoker = true) AS
WITH slots AS (
    SELECT o AS slot_offset, date_bin('5 minutes', now(), TIMESTAMPTZ '2001-01-01 00:00:00+00') + o * interval '5 minutes' AS slot
      FROM generate_series(0, 11) o),
cold AS (
    SELECT c.workspace_id, c.id,
           date_bin('5 minutes', greatest(c.next_send_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00') AS first_slot,
           (c.send_interval_minutes + 4) / 5 AS every,
           row_number() OVER (PARTITION BY c.workspace_id, c.id ORDER BY q.run_at, q.message_id) AS k,
           greatest(0, (extract(epoch FROM date_bin('5 minutes', greatest(q.run_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00')
                                         - date_bin('5 minutes', greatest(c.next_send_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00')) / 300)::int) AS d
      FROM connections c
      JOIN delivery_queue q ON (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
     WHERE c.status = 'active' AND NOT c.paused AND c.send_interval_minutes IS NOT NULL
       AND q.state IN ('queued', 'claimed') AND q.paced AND q.run_at < now() + interval '1 hour'),
paced AS (
    SELECT first_slot + ((k - 1) * every + max(d - (k - 1) * every) OVER (PARTITION BY workspace_id, id ORDER BY k)) * interval '5 minutes' AS slot
      FROM cold),
due AS (
    SELECT date_bin('5 minutes', greatest(q.run_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00') AS slot
      FROM delivery_queue q JOIN connections c ON (c.workspace_id, c.id) = (q.workspace_id, q.connection_id)
     WHERE q.state IN ('queued', 'claimed') AND q.run_at < now() + interval '1 hour' AND c.status = 'active' AND NOT c.paused
       AND (NOT q.paced OR c.send_interval_minutes IS NULL))
SELECT s.slot_offset, s.slot,
       (SELECT count(*) FROM paced p WHERE p.slot = s.slot) AS paced,
       (SELECT count(*) FROM due d WHERE d.slot = s.slot) AS due
  FROM slots s;
CREATE TRIGGER delivery_queue_registered AFTER INSERT ON delivery_queue
    REFERENCING NEW TABLE AS inserted FOR EACH STATEMENT EXECUTE FUNCTION register_dispatch_workspace();

-- One fenced submission effort. Typed outcome, never a display string. Reserves budget at
-- claim and settles it exactly once at finish (quota_state leaves 'reserved' once).
CREATE TABLE attempts (
    workspace_id        uuid NOT NULL,
    message_id          uuid NOT NULL,
    attempt_number      integer NOT NULL CHECK (attempt_number > 0),
    id                  uuid NOT NULL DEFAULT uuidv7(),
    connection_id       uuid NOT NULL,
    reserved_day        date NOT NULL,
    quota_scope_id      uuid,                                             -- the scope charged at claim, frozen: a later scope change on the connection cannot move the settlement
    recipient_count     integer NOT NULL CHECK (recipient_count BETWEEN 1 AND 150),
    quota_state         text NOT NULL DEFAULT 'reserved' CHECK (quota_state IN ('reserved', 'consumed', 'released')),
    lease_owner         text NOT NULL,
    claimed_at          timestamptz NOT NULL DEFAULT now(),
    smtp_started_at     timestamptz,                                      -- after it, a lost sender means 'uncertain'
    finished_at         timestamptz,
    outcome             text CHECK (outcome IN ('accepted', 'transient', 'permanent', 'uncertain',
                                                'released', 'suppressed', 'skipped')),
    phase               text CHECK (phase IN ('connect', 'auth', 'mail_from', 'rcpt_to', 'data', 'api')),
    smtp_code           smallint,                                         -- 250, 421, 550
    enhanced_status     text,                                             -- '5.1.1'
    category            text,                                             -- domain::EvidenceCategory as text
    diagnostic          text,                                             -- bounded provider text, redacted
    provider_message_id text,                                             -- Gmail/Graph id when the API returns one
    PRIMARY KEY (workspace_id, message_id, attempt_number),
    CHECK ((finished_at IS NULL) = (outcome IS NULL)),
    CHECK ((finished_at IS NULL) = (quota_state = 'reserved')),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, connection_id, reserved_day) REFERENCES connection_usage (workspace_id, connection_id, day),
    FOREIGN KEY (workspace_id, quota_scope_id, reserved_day) REFERENCES quota_scope_usage (workspace_id, scope_id, day)
) PARTITION BY RANGE (message_id);
CREATE UNIQUE INDEX attempts_one_active ON attempts (workspace_id, message_id) WHERE finished_at IS NULL;
CREATE INDEX attempts_by_connection_day ON attempts (workspace_id, connection_id, reserved_day);

-- Every observation about a message's fate after (or during) submission, from a named
-- source, with a confidence. One row per (source event, recipient): a DSN with three
-- recipient blocks is three rows sharing source_event_id.
CREATE TABLE delivery_events (
    workspace_id        uuid NOT NULL,
    id                  uuid NOT NULL DEFAULT uuidv7(),                  -- partition key: ingestion time, so late evidence always has a home
    message_id          uuid,                                             -- parsed from our Message-ID; no FK: the message's partition may be archived; NULL = unmatched
    thread_id           uuid,                                             -- parsed from our Message-ID, which carries it; no FK for the same reason
    attempt_number      integer,                                          -- when the source is an attempt
    recipient_email     text,                                             -- NULL = envelope-level, recipient unknown
    recipient_email_key text GENERATED ALWAYS AS (ascii_lower(recipient_email)) STORED,
    recipient_ref       text NOT NULL CHECK (recipient_ref IN ('named', 'single_envelope', 'unknown')),  -- why the recipient is known: the evidence names it, or the envelope had one recipient; or that it is not
    source              text NOT NULL CHECK (source IN ('smtp', 'provider_api', 'provider_webhook', 'dsn', 'arf',
                                                        'inbound_notice', 'preflight', 'unsubscribe', 'manual', 'sent_folder')),   -- sent_folder: our own session found the message in the mailbox's Sent folder, reconciling an uncertain submission
    source_event_id     text NOT NULL,                                    -- 'attempt:<id>', DSN Message-ID, webhook event id
    received_via        uuid,                                             -- receive_bindings.id for dsn/arf/inbound_notice
    kind                text NOT NULL CHECK (kind IN ('accepted', 'deferred', 'delivered', 'bounced', 'rejected',
                                                      'complaint', 'unsubscribed', 'address_changed', 'reported')),
    action              text CHECK (action IN ('failed', 'delayed', 'delivered', 'relayed', 'expanded')),  -- RFC 3464
    phase               text CHECK (phase IN ('connect', 'auth', 'mail_from', 'rcpt_to', 'data', 'api')),  -- for smtp and provider_api sources
    enhanced_status     text,
    category            text NOT NULL,
    diagnostic          text,
    confidence          text NOT NULL CHECK (confidence IN ('authenticated', 'corroborated', 'inferred', 'human_text')),
    receipt_id          uuid,                                             -- the webhook receipt this was normalised from (no FK: receipts are dropped after 1 day)
    observed_at         timestamptz NOT NULL,                             -- when the source observed it
    created_at          timestamptz NOT NULL DEFAULT now(),               -- when we recorded it (processing time)
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, received_via) REFERENCES receive_bindings (workspace_id, id) ON DELETE SET NULL (received_via),
    CHECK (source NOT IN ('dsn', 'arf', 'inbound_notice') OR received_via IS NOT NULL OR confidence = 'inferred'),
    CHECK ((recipient_email IS NULL) = (recipient_ref = 'unknown'))
) PARTITION BY RANGE (id);
-- Replays are refused by the source's own identity, not by a unique index here: provider events
-- by provider_event_keys (one row per provider event id, kept longer than any provider retry
-- window), SMTP outcomes by the attempt row (one per attempt), mail-borne evidence by the
-- inbound message's transport key.
CREATE INDEX delivery_events_by_message ON delivery_events (workspace_id, message_id, observed_at) WHERE message_id IS NOT NULL;
CREATE INDEX delivery_events_unmatched ON delivery_events (workspace_id, observed_at) WHERE message_id IS NULL;
-- suppressions.source_event cites a delivery event by id without a foreign key: the event's
-- partition may be archived before the suppression expires (suppressions never expire).

-- Temporary, recipient-specific pauses (mailbox full). Resolved by delivery of the same message.
CREATE TABLE recipient_holds (
    workspace_id uuid NOT NULL,
    message_id   uuid NOT NULL,
    email        text NOT NULL,
    email_key    text NOT NULL GENERATED ALWAYS AS (ascii_lower(email)) STORED,
    reason       text NOT NULL CHECK (reason IN ('mailbox_full', 'greylisted', 'no_route', 'invalid_recipient')),   -- invalid_recipient: a corroborated 5.1.x, held while a person reviews it
    observed_at  timestamptz NOT NULL,
    review_after timestamptz NOT NULL,
    resolved_at  timestamptz,
    resolution   text CHECK (resolution IN ('delivered', 'expired', 'suppressed', 'manual')),
    PRIMARY KEY (workspace_id, message_id, email_key),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX recipient_holds_active ON recipient_holds (workspace_id, email_key) WHERE resolved_at IS NULL;

-- Preflight cache: what DNS answered about the routing of each address's domain, kept until expires_at
-- (a day); a failed lookup ('unknown') and a syntax verdict are never stored; never a mailbox probe.
CREATE TABLE recipient_validations (
    workspace_id uuid NOT NULL,
    email_key    text NOT NULL,
    status       text NOT NULL CHECK (status IN ('routable', 'invalid', 'unknown', 'risky')),
    reason       text NOT NULL,
    checked_at   timestamptz NOT NULL,
    expires_at   timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, email_key)
);
CREATE INDEX recipient_validations_expiry ON recipient_validations (expires_at);   -- retention.prune deletes expired verdicts

-- Generic idempotency for every write with effects: key + request fingerprint + stored response
-- (status, body and the headers the client needs again: Location, RateLimit). Two scopes: the
-- workspace of the credential, or the user of a session before a workspace exists
-- (POST /workspaces, accepting an invitation). Retryable failures (429, 5xx) are never stored.
CREATE TABLE idempotency_keys (
    id               uuid NOT NULL DEFAULT uuidv7() PRIMARY KEY,
    workspace_id     uuid,
    user_id          uuid REFERENCES users (id) ON DELETE CASCADE,
    key              text NOT NULL,
    fingerprint      bytea NOT NULL,
    locked_at        timestamptz,
    response_status  smallint,
    response_headers jsonb,
    response_body    jsonb,
    created_at       timestamptz NOT NULL DEFAULT now(),
    expires_at       timestamptz NOT NULL,
    CHECK ((workspace_id IS NULL) <> (user_id IS NULL)),
    CHECK (response_status IS NULL OR response_status < 500 AND response_status <> 429)
);
CREATE UNIQUE INDEX idempotency_keys_workspace ON idempotency_keys (workspace_id, key) WHERE workspace_id IS NOT NULL;
CREATE UNIQUE INDEX idempotency_keys_user ON idempotency_keys (user_id, key) WHERE user_id IS NOT NULL;
CREATE INDEX idempotency_keys_expiry ON idempotency_keys (expires_at);


-- ───────────────────────────── 8. Inbox ─────────────────────────────
CREATE TABLE threads (
    workspace_id       uuid NOT NULL,
    id                 uuid NOT NULL DEFAULT uuidv7(),
    person_id          uuid,
    campaign_id        uuid,
    sender_identity_id uuid NOT NULL,
    root_message_id    uuid,                                              -- our first outbound message, if any (no FK: messages are archived, threads live on)
    root_internet_message_id text,                                        -- for References on follow-ups
    last_message_id    uuid,                                              -- the latest outbound message of the thread
    last_internet_message_id text,                                        -- for In-Reply-To on the next follow-up
    subject            text,
    status             text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'archived', 'snoozed')),
    -- When a snoozed thread comes back: from then on it reads and filters as open, with no job
    -- to wake it; a person's answer arriving earlier opens it at once.
    snoozed_until      timestamptz,
    last_activity_at   timestamptz NOT NULL DEFAULT now(),
    unread             boolean NOT NULL DEFAULT false,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    CHECK ((status = 'snoozed') = (snoozed_until IS NOT NULL)),
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE SET NULL (person_id),
    FOREIGN KEY (workspace_id, campaign_id) REFERENCES campaigns (workspace_id, id) ON DELETE SET NULL (campaign_id),
    FOREIGN KEY (workspace_id, sender_identity_id) REFERENCES sender_identities (workspace_id, id)
);
-- `GET /threads?status=…&sort=last_activity_at`: keyset pages of one stored status by activity,
-- ending with the id like the cursor, read in either direction (`open` reads `open` and
-- `snoozed`, a snooze that ended reading as open).
CREATE INDEX threads_inbox ON threads (workspace_id, status, last_activity_at, id);
-- `GET /threads?sort=last_activity_at`: keyset pages by activity, whatever the filters, ending with the id
-- so a page boundary between threads with the same instant is exact.
CREATE INDEX threads_by_activity ON threads (workspace_id, last_activity_at, id);
-- A campaign's follow-up in the same thread finds the thread its enrollment's first message opened
-- (enrollments.thread_root_message_id), also after that message's row was archived.
CREATE INDEX threads_by_root ON threads (workspace_id, root_message_id) WHERE root_message_id IS NOT NULL;
-- Every outbound Message-ID that does not carry its thread: mail whose provider replaces our
-- Message-ID, such as Amazon SES (written by the finish of the submission from the id the provider
-- returns). A reply naming any of these ids in In-Reply-To or References resolves to its thread and
-- its original envelope even after the message row is archived. One row per message, not per
-- thread, so an intermediate id (B of A, B, C) and ids older than the thread's latest outbound
-- message all still resolve. Kept for the reply window, its partition policy's retention.
CREATE TABLE message_id_directory (
    workspace_id        uuid   NOT NULL,
    id                  uuid   NOT NULL DEFAULT uuidv7(),                -- partition key: when the row was written
    lookup_key          text   NOT NULL,                                 -- the provider's token: a reply's id is matched whole or by its local part
    message_id          uuid   NOT NULL,                                 -- no FK: the message may be archived
    thread_id           uuid   NOT NULL,
    recipients          text[] NOT NULL CHECK (cardinality(recipients) BETWEEN 1 AND 150),   -- the original envelope, for the sender check
    PRIMARY KEY (workspace_id, id),
    FOREIGN KEY (workspace_id, thread_id) REFERENCES threads (workspace_id, id) ON DELETE CASCADE
) PARTITION BY RANGE (id);
CREATE INDEX message_id_directory_lookup ON message_id_directory (workspace_id, lookup_key);
ALTER TABLE messages ADD CONSTRAINT messages_thread_fk
    FOREIGN KEY (workspace_id, thread_id) REFERENCES threads (workspace_id, id);

CREATE TABLE inbound_messages (
    workspace_id          uuid NOT NULL,
    id                    uuid NOT NULL DEFAULT uuidv7(),
    receive_binding_id    uuid NOT NULL,
    connection_id         uuid NOT NULL,
    transport_identity    jsonb NOT NULL,                                 -- {uid_validity,uid} | {provider_message_id}
    transport_key         text NOT NULL,                                  -- canonical string of the above
    internet_message_id   text,
    in_reply_to           text,
    references_ids        text[] NOT NULL DEFAULT '{}',
    thread_id             uuid,
    message_id            uuid,                                           -- the outbound message this answers, if correlated
    person_id             uuid,
    from_email            text,
    from_name             text,
    subject               text,
    received_at           timestamptz NOT NULL,
    classification        text NOT NULL CHECK (classification IN ('human_reply', 'auto_reply', 'out_of_office',
                                                                   'bounce', 'complaint', 'address_change', 'unsubscribe', 'unknown')),
    classification_source text NOT NULL CHECK (classification_source IN ('rules', 'manual', 'ai')),
    revision              integer NOT NULL DEFAULT 1,                     -- bumped by every reclassification; an AI result applies only `WHERE revision = $seen AND classification_source <> 'manual'`
    review_requested_at   timestamptz,                                    -- set when a rule or the AI asks a person to decide
    review_proposal       jsonb,                                          -- what confirming applies: {"action": "suppress" | "change_address", "email", "new_email"?}; mail-borne evidence that cannot be trusted alone waits for a person
    reviewed_at           timestamptz,
    review_decision       text CHECK (review_decision IN ('confirmed', 'dismissed')),
    sentiment             text CHECK (sentiment IN ('positive', 'neutral', 'negative')),
    evidence              text NOT NULL,                                  -- which header/field decided
    body_text             text,                                           -- bounded excerpt
    body_object_key       text,                                           -- full MIME in object storage when kept
    size_bytes            integer,
    truncated             boolean NOT NULL DEFAULT false,
    content_hash          bytea,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    deleted_at            timestamptz,
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, receive_binding_id, transport_key),
    CHECK ((review_decision IS NULL) = (reviewed_at IS NULL)),
    CHECK (review_proposal IS NULL OR review_requested_at IS NOT NULL),
    FOREIGN KEY (workspace_id, receive_binding_id) REFERENCES receive_bindings (workspace_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, connection_id) REFERENCES connections (workspace_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (workspace_id, thread_id) REFERENCES threads (workspace_id, id),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id) ON DELETE SET NULL (message_id),
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id) ON DELETE SET NULL (person_id)
);
CREATE INDEX inbound_by_content ON inbound_messages (workspace_id, connection_id, content_hash) WHERE content_hash IS NOT NULL;  -- evidence of a possible duplicate, never a key: two distinct messages can share a body
CREATE INDEX inbound_by_thread ON inbound_messages (workspace_id, thread_id, received_at) WHERE thread_id IS NOT NULL;
CREATE INDEX inbound_by_message ON inbound_messages (workspace_id, message_id) WHERE message_id IS NOT NULL;
CREATE INDEX inbound_recent ON inbound_messages (workspace_id, received_at DESC, id) WHERE deleted_at IS NULL;
-- The review queue's figures across workspaces (reviews asked in the last day, time to review of
-- those decided in the last week), read every minute: only the few rows that ever asked.
CREATE INDEX inbound_review_requested ON inbound_messages (review_requested_at) WHERE review_requested_at IS NOT NULL;
CREATE INDEX inbound_reviewed ON inbound_messages (reviewed_at) WHERE reviewed_at IS NOT NULL;

-- ───────────────────────────── 9. Work: jobs, schedules, outbox, webhooks ─────────────────────────────
-- The concurrency guard of the generic job runner: one row per workspace and queue, so one
-- workspace's jobs cannot take every worker. A claim locks the lane, claims at most its free
-- slots and increments `running` in the same transaction; finish, yield, cancel and recovery
-- decrement it. Lock order: lane → job.
CREATE TABLE job_lanes (
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    queue        text NOT NULL,
    running      integer NOT NULL DEFAULT 0 CHECK (running >= 0),
    max_running  integer NOT NULL CHECK (max_running > 0),
    turn_at      timestamptz NOT NULL DEFAULT 'epoch',                   -- fairness: oldest turn first
    PRIMARY KEY (workspace_id, queue),
    CHECK (running <= max_running)
);

-- One durable job table for everything that is not a delivery attempt or an inbox poll.
-- Priority is a queue, not a column. A long job yields by rescheduling itself, so it never holds
-- its lane slot longer than one bounded run.
-- System work belongs to the `system` workspace row created at install, never to NULL.
CREATE TABLE jobs (
    id                  uuid NOT NULL DEFAULT uuidv7() PRIMARY KEY,
    workspace_id        uuid NOT NULL REFERENCES workspaces (id),
    queue               text NOT NULL,                                    -- 'imports' | 'enrollment' | 'webhooks' | 'exports' | 'maintenance' | 'ai' | 'transactional' | 'receipts'
    kind                text NOT NULL,                                    -- 'import.process', 'campaign.enroll', 'webhook.deliver'
    payload             jsonb NOT NULL DEFAULT '{}',
    unique_key          text,                                             -- at most one live job per (kind, unique_key)
    state               text NOT NULL DEFAULT 'available'
                        CHECK (state IN ('available', 'running', 'completed', 'failed', 'cancelled', 'needs_review')),
    run_at              timestamptz NOT NULL DEFAULT now(),
    claims              integer  NOT NULL DEFAULT 0,                      -- every claim, including yields: the fence
    attempts            smallint NOT NULL DEFAULT 0,                      -- failed runs only: retry exhaustion
    max_attempts        smallint NOT NULL DEFAULT 10,
    lease_owner         text,
    lease_expires_at    timestamptz,
    effect_started_at   timestamptz,                                      -- set by external_ambiguous kinds before the effect
    cancel_requested_at timestamptz,
    last_error          text,
    progress            jsonb,                                            -- bounded, for status endpoints
    result              jsonb CHECK (pg_column_size(result) <= 65536),    -- the job's bounded result (a draft, suggestions, counts); larger results are object keys
    trace_parent        text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz,
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK ((state = 'running') = (lease_owner IS NOT NULL))
);
CREATE INDEX jobs_available ON jobs (workspace_id, queue, run_at, id) WHERE state = 'available';   -- claims are per lane
CREATE INDEX jobs_running ON jobs (lease_expires_at) WHERE state = 'running';
CREATE UNIQUE INDEX jobs_unique_live ON jobs (workspace_id, kind, unique_key) WHERE unique_key IS NOT NULL AND state IN ('available', 'running');
CREATE INDEX jobs_by_workspace ON jobs (workspace_id, created_at);
CREATE INDEX jobs_finished ON jobs (finished_at) WHERE state IN ('completed', 'failed', 'cancelled');

CREATE TABLE job_schedules (
    name         text PRIMARY KEY,                                        -- the kind it enqueues: 'retention.prune', 'connections.warmup'
    kind         text NOT NULL,
    payload      jsonb NOT NULL DEFAULT '{}',
    cron         text NOT NULL,
    next_run_at  timestamptz NOT NULL,
    last_run_at  timestamptz,
    enabled      boolean NOT NULL DEFAULT true
);

-- Transactional outbox: written in the same transaction as the business change.
CREATE TABLE outbox_events (
    id           uuid NOT NULL DEFAULT uuidv7() PRIMARY KEY,
    workspace_id uuid NOT NULL,
    type         text NOT NULL,                                           -- 'message.sent', 'inbound_message.received'
    subject_type text NOT NULL,
    subject_id   uuid NOT NULL,
    payload      jsonb NOT NULL,
    trace_parent text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    published_at timestamptz,
    UNIQUE (workspace_id, id)
) PARTITION BY RANGE (id);
CREATE INDEX outbox_unpublished ON outbox_events (id) WHERE published_at IS NULL;

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- A searchable projection of retained content, populated by acceptance and inbox polling.
-- Bodies are excluded from ordinary message lists. Prepared content is the last composition,
-- including signatures and tracking links; inbound truncation is explicitly recorded.
CREATE TABLE message_contents (
    workspace_id uuid NOT NULL REFERENCES workspaces(id),
    id uuid NOT NULL,
    direction text NOT NULL CHECK (direction IN ('outbound', 'inbound')),
    thread_id uuid,
    connection_id uuid NOT NULL,
    subject text NOT NULL DEFAULT '',
    from_email text NOT NULL DEFAULT '',
    to_addresses text[] NOT NULL DEFAULT '{}',
    cc text[] NOT NULL DEFAULT '{}',
    bcc text[] NOT NULL DEFAULT '{}',
    html text,
    text_body text,
    headers jsonb NOT NULL DEFAULT '[]',
    truncated boolean NOT NULL DEFAULT false,
    prepared_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    recipient_text text NOT NULL DEFAULT '',
    search_text text GENERATED ALWAYS AS (recipient_text || E'\n' || subject || E'\n' || from_email || E'\n' || coalesce(text_body, '') || E'\n' || coalesce(html, '')) STORED,
    PRIMARY KEY(workspace_id,id)
);
CREATE INDEX message_contents_thread ON message_contents(workspace_id,thread_id,id);
CREATE INDEX message_contents_connection ON message_contents(workspace_id,connection_id,id);
CREATE INDEX message_contents_search ON message_contents USING gin(search_text gin_trgm_ops);

CREATE TABLE attachments (
    workspace_id uuid NOT NULL REFERENCES workspaces(id),
    id uuid NOT NULL DEFAULT uuidv7(),
    filename text NOT NULL,
    content_type text NOT NULL,
    content_id text,
    size_bytes integer NOT NULL CHECK(size_bytes >= 0 AND size_bytes <= 1048576),
    object_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(workspace_id,id)
);
CREATE TABLE message_attachments (
    workspace_id uuid NOT NULL,
    message_id uuid NOT NULL,
    attachment_id uuid NOT NULL,
    PRIMARY KEY(workspace_id,message_id,attachment_id),
    FOREIGN KEY(workspace_id,message_id) REFERENCES message_contents(workspace_id,id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id,attachment_id) REFERENCES attachments(workspace_id,id)
);

CREATE INDEX message_attachments_file ON message_attachments(workspace_id,attachment_id);

CREATE TABLE webhook_endpoints (
    workspace_id    uuid NOT NULL REFERENCES workspaces (id),
    id              uuid NOT NULL DEFAULT uuidv7(),
    url             text NOT NULL,
    secret          bytea NOT NULL,                                       -- sealed; shown once as whsec_
    event_types     text[] NOT NULL,
    filters         jsonb NOT NULL DEFAULT '{}',
    header_names    text[] NOT NULL DEFAULT '{}',
    enabled         boolean NOT NULL DEFAULT true,
    disabled_reason text,                                                 -- 'gone' (the consumer answered 410), 'failing' (5 days without a success), 'manual'
    failing_since   timestamptz,                                          -- first failed attempt since the last success
    failure_notified_at timestamptz,                                      -- when the workspace's admins were first emailed about the failures
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id)
);
CREATE TABLE webhook_deliveries (
    workspace_id     uuid NOT NULL,
    id               uuid NOT NULL DEFAULT uuidv7(),
    endpoint_id      uuid NOT NULL,
    event_id         uuid NOT NULL,
    attempt          smallint NOT NULL DEFAULT 0,
    state            text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'delivered', 'failed', 'disabled')),
    next_attempt_at  timestamptz NOT NULL DEFAULT now(),
    response_status  smallint,                                            -- the latest attempt's answer (null: no answer, a timeout or a refused connection)
    response_excerpt text,
    last_attempt_at  timestamptz,                                         -- when the latest attempt started, failures included
    last_duration_ms integer CHECK (last_duration_ms >= 0),
    delivered_at     timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, event_id, id),
    UNIQUE (workspace_id, event_id, endpoint_id),
    FOREIGN KEY (workspace_id, endpoint_id) REFERENCES webhook_endpoints (workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, event_id) REFERENCES outbox_events (workspace_id, id) ON DELETE CASCADE
) PARTITION BY RANGE (event_id);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (next_attempt_at) WHERE state = 'pending';

-- Replay protection for inbound provider webhooks: one row per provider event id,
-- hash-partitioned so the primary key is enforced across the whole table (a range-partitioned
-- receipt table cannot be unique on anything but its partition key). The ingress inserts the
-- key and the receipt in one transaction; a key that already exists is a replay: the provider
-- gets its success code and no receipt is stored (a replay whose body hash differs is stored
-- as a quarantined receipt for review), so two concurrent copies, a same-day copy and a
-- cross-midnight copy are all refused once.
-- Rows older than 3 days (longer than SendGrid's 24 h, Mailgun's ~8 h, SNS's ≤ 1 h and the
-- managed MTA's 24 h retry windows) are deleted in batches by retention.prune.
CREATE TABLE provider_event_keys (
    provider_webhook_id uuid NOT NULL,
    event_id            text NOT NULL,
    body_hash           bytea NOT NULL,                                   -- a replay with a different body is quarantined, never applied
    received_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_webhook_id, event_id)
) PARTITION BY HASH (provider_webhook_id, event_id);
DO $$ BEGIN FOR i IN 0..7 LOOP
    EXECUTE format('CREATE TABLE provider_event_keys_%s PARTITION OF provider_event_keys FOR VALUES WITH (MODULUS 8, REMAINDER %s)', i, i);
END LOOP; END $$;
CREATE INDEX provider_event_keys_by_age ON provider_event_keys (received_at);

-- The verified body of an accepted provider event until the receipts.normalize job turns it
-- into delivery events (then raw is NULL and state = 'normalized'); quarantined when the body
-- cannot be understood. Partitioned by day on its own id and dropped after 1 day (its key lives
-- 3 days in provider_event_keys); a leaf is dropped only when no row of it is still 'received'.
CREATE TABLE webhook_receipts (
    workspace_id        uuid NOT NULL,
    id                  uuid NOT NULL DEFAULT uuidv7(),
    provider_webhook_id uuid NOT NULL,
    event_id            text NOT NULL,
    body_hash           bytea NOT NULL,
    raw                 bytea,                                            -- the verified body (≤ 1 MiB) until normalised, then NULL
    state               text NOT NULL DEFAULT 'received' CHECK (state IN ('received', 'normalized', 'quarantined')),
    received_at         timestamptz NOT NULL DEFAULT now(),
    processed_at        timestamptz,
    PRIMARY KEY (workspace_id, id)
) PARTITION BY RANGE (id);
CREATE INDEX webhook_receipts_pending ON webhook_receipts (id) WHERE state = 'received';

-- ───────────────────────────── 10. Tracking and analytics ─────────────────────────────
-- Raw observations, partitioned by month on occurred_at (a timestamptz policy row); drained by
-- the tracking role from its SQLite spool. No default partition: an unprovisioned month fails
-- the drain visibly and the spool keeps the rows until partitions.create runs.
CREATE TABLE tracking_events (
    workspace_id uuid NOT NULL,
    id           uuid NOT NULL,                                           -- generated at the tracker: the dedup key
    message_id   uuid NOT NULL,
    kind         text NOT NULL CHECK (kind IN ('open', 'click', 'unsubscribe_view')),
    link_index   smallint,
    url_hash     bytea,
    actor_class  text NOT NULL CHECK (actor_class IN ('human', 'scanner', 'proxy', 'unknown')),
    ip_hash      bytea,
    user_agent   text,
    occurred_at  timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, occurred_at, id)
) PARTITION BY RANGE (occurred_at);
CREATE INDEX tracking_events_by_message ON tracking_events (workspace_id, message_id, occurred_at);

-- Per-message rollup, updated by the drain in the same transaction as the raw insert,
-- only for rows the insert actually created (ON CONFLICT DO NOTHING RETURNING).
CREATE TABLE message_engagement (
    workspace_id   uuid NOT NULL,
    message_id     uuid NOT NULL,                                         -- partition key; no FK (the message may be archived)
    first_open_at  timestamptz,
    opens          integer NOT NULL DEFAULT 0,
    first_click_at timestamptz,
    clicks         integer NOT NULL DEFAULT 0,
    human_opens    integer NOT NULL DEFAULT 0,
    human_clicks   integer NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, message_id)
) PARTITION BY RANGE (message_id);

-- Append-only facts for the rollup, written in the same transaction as the change they count
-- (an attempt closed, an event recorded, a reply classified). Ids are generated by the
-- database (DEFAULT, one clock). The rollup consumes rows with id < uuidv7_boundary(now() - lag)
-- where lag (120 s) is twice the longest transaction_timeout of any role that writes here
-- (set on the roles at the top of this file), so no row can commit with an id below a watermark
-- already consumed; a nightly recount of the previous UTC day from this table is the
-- reconciliation that proves it. Partitioned by day; a leaf is dropped only when the watermark is
-- past its upper bound and its day was recounted.
CREATE TABLE stats_increments (
    workspace_id    uuid     NOT NULL,
    connection_id   uuid,
    message_kind    text,
    id              uuid     NOT NULL DEFAULT uuidv7(),
    campaign_id     uuid,
    step_id         uuid,
    step_revision   integer,
    variant_id      uuid,
    variant_version integer,
    day             date     NOT NULL,
    metric          text     NOT NULL CHECK (metric IN ('sent', 'delivered', 'bounced', 'opened', 'clicked', 'replied', 'unsubscribed', 'complained')),
    delta           smallint NOT NULL CHECK (delta BETWEEN -1 AND 1),
    PRIMARY KEY (workspace_id, id)
) PARTITION BY RANGE (id);
CREATE TABLE message_daily_stats (
    workspace_id uuid NOT NULL,
    day date NOT NULL,
    connection_id uuid,
    message_kind text,
    campaign_id uuid,
    metric text NOT NULL,
    value bigint NOT NULL DEFAULT 0,
    UNIQUE NULLS NOT DISTINCT (workspace_id, day, connection_id, message_kind, campaign_id, metric)
);
CREATE TABLE rollup_watermarks (
    name          text PRIMARY KEY,                                       -- 'campaign_daily_stats'
    processed_to  uuid NOT NULL,                                          -- every increment with id < processed_to is counted
    updated_at    timestamptz NOT NULL DEFAULT now()
);

-- Per campaign/step/variant/day counters: the source of dashboards and winner selection.
CREATE TABLE campaign_daily_stats (
    workspace_id    uuid NOT NULL,
    campaign_id     uuid NOT NULL,
    step_id         uuid NOT NULL,
    step_revision   integer NOT NULL,
    variant_id      uuid NOT NULL,
    variant_version integer NOT NULL,
    day             date NOT NULL,
    sent            integer NOT NULL DEFAULT 0,
    delivered       integer NOT NULL DEFAULT 0,
    bounced         integer NOT NULL DEFAULT 0,
    opened          integer NOT NULL DEFAULT 0,                           -- unique messages with a human open
    clicked         integer NOT NULL DEFAULT 0,
    replied         integer NOT NULL DEFAULT 0,
    unsubscribed    integer NOT NULL DEFAULT 0,
    complained      integer NOT NULL DEFAULT 0,
    verification    text NOT NULL DEFAULT 'pending' CHECK (verification IN ('pending', 'verified', 'unverified')),  -- set by the nightly analytics.recount: verified once its day was recounted from the increments, unverified when they were gone first
    verified_at     timestamptz,
    PRIMARY KEY (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day)
);

CREATE TABLE ai_usage (
    workspace_id         uuid NOT NULL REFERENCES workspaces (id),
    month                date NOT NULL,                                   -- first day of month, UTC
    calls                integer NOT NULL DEFAULT 0,
    cost_micros          bigint NOT NULL DEFAULT 0 CHECK (cost_micros >= 0),
    reserved_cost_micros bigint NOT NULL DEFAULT 0 CHECK (reserved_cost_micros >= 0),
    budget_micros        bigint NOT NULL CHECK (budget_micros >= 0),
    warned_at            timestamptz,                                     -- when `ai.budget_warning` (settled spend at 80 % of the budget) was recorded: once per month and budget; a budget change clears it
    exceeded_at          timestamptz,                                     -- when `ai.budget_exceeded` (a call refused for lack of budget, or settled spend at 100 %) was recorded: once per month and budget; a budget change clears it
    PRIMARY KEY (workspace_id, month)
);

-- One row per AI call: the reservation and its settlement are durable and fenced, so a crashed
-- job settles once on recovery and a billed refusal or truncation is still charged. The month is
-- the reservation's month even if settlement crosses midnight.
CREATE TABLE ai_calls (
    workspace_id    uuid NOT NULL REFERENCES workspaces (id),
    id              uuid NOT NULL DEFAULT uuidv7(),
    job_id          uuid NOT NULL,
    use_case        text NOT NULL,
    provider        text NOT NULL,
    model           text NOT NULL,
    prompt_id       text NOT NULL,
    canary          boolean NOT NULL DEFAULT false,                   -- served by the use case's canary prompt as a canary (its 5 % share), so the canary's guard counts it; false for the current prompt and once the canary is promoted
    month           date NOT NULL,
    reserved_micros bigint NOT NULL CHECK (reserved_micros >= 0),
    price_input_micros_per_mtok  bigint NOT NULL,                        -- pricing snapshot at reservation
    price_output_micros_per_mtok bigint NOT NULL,
    state           text NOT NULL DEFAULT 'reserved' CHECK (state IN ('reserved', 'settled', 'released', 'interrupted')),
    input_tokens    integer,
    output_tokens   integer,
    settled_micros  bigint CHECK (settled_micros >= 0),
    outcome         text CHECK (outcome IN ('completed', 'refused', 'truncated', 'invalid_output', 'provider_error', 'timeout', 'interrupted')),
    review_requested boolean,                                         -- a classification's verdict: whether it fell below the workspace's confidence threshold (a sampled review is not counted); NULL for a call without a verdict. The review rate a canary prompt is guarded by
    started_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    PRIMARY KEY (workspace_id, id),
    CHECK ((state = 'reserved') = (finished_at IS NULL)),
    CHECK (review_requested IS NULL OR outcome = 'completed')
);
CREATE INDEX ai_calls_open ON ai_calls (workspace_id, job_id) WHERE state = 'reserved';
CREATE INDEX ai_calls_verdicts ON ai_calls (use_case, prompt_id, started_at) WHERE review_requested IS NOT NULL;   -- the canary guard's review rates, read across workspaces

CREATE TABLE workspace_deletions (
    workspace_id         uuid PRIMARY KEY,
    requested_by         text NOT NULL,
    requested_at         timestamptz NOT NULL DEFAULT now(),
    job_id               uuid,
    tombstone_object_key text,
    completed_at         timestamptz
);

-- The restore drills an operator completed (`norbelys-server admin restore-drill`): the latest
-- backup restored onto an isolated host, the schema checked and the row counts compared, in
-- `minutes` from the start of the restore. A drill is the only proof that backups restore: the
-- worker reports the newest as `norbelys_restore_drill_last_success_timestamp_seconds`, and the
-- `restore-drill` alert fires when none completed for over a month. Only the operator's system
-- login writes it.
CREATE TABLE restore_drills (
    completed_at timestamptz PRIMARY KEY DEFAULT now(),
    minutes      integer     NOT NULL CHECK (minutes > 0)
);

-- ───────────────────────────── 10b. Partition policies and initial partitions ─────────────────────────────
-- Periods and online windows for the throughput floor (200 messages a second). The archive job
-- takes each leaf whose whole period is older than its retention through a sealed order:
--   1. a gate per table, so nothing live is archived: for messages no queue row and no unsettled
--      attempt (the foreign keys pointing at the leaf make PostgreSQL refuse its detach anyway,
--      see delivery_queue); for
--      receipts no row still 'received'; for increments the rollup past the leaf and its day
--      recounted; for the outbox no unpublished event; for deliveries nothing 'pending'. A leaf
--      failing its gate stays as retention debt with an alert, never a forced drop;
--   2. dependents first (the attempts leaf before its messages leaf, the deliveries leaf before
--      its outbox leaf), since PostgreSQL refuses to detach a leaf that rows still reference.
--      Each leaf is detached CONCURRENTLY, then sealed: its own top-level outgoing
--      foreign keys are dropped, so no delete elsewhere can cascade into it or be blocked by it,
--      and no role holds DML on any leaf, so a sealed leaf is frozen;
--   3. the frozen leaf is exported to Parquet, verified (row count, checksum) and dropped, the
--      drop being the owner's DDL, reached by SET ROLE norbelys_owner.
INSERT INTO partition_policies (table_name, key_kind, period, retention, archive) VALUES
  ('messages',           'uuidv7',      interval '1 day',   interval '7 days',  true),
  ('attempts',           'uuidv7',      interval '1 day',   interval '7 days',  true),
  ('delivery_events',    'uuidv7',      interval '1 day',   interval '3 days',  true),
  ('message_engagement', 'uuidv7',      interval '1 day',   interval '30 days', true),
  ('webhook_receipts',   'uuidv7',      interval '1 day',   interval '1 day',   false),
  ('outbox_events',      'uuidv7',      interval '1 day',   interval '7 days',  false),
  ('webhook_deliveries', 'uuidv7',      interval '1 day',   interval '7 days',  false),
  ('stats_increments',   'uuidv7',      interval '1 day',   interval '2 days',  false),
  ('message_id_directory', 'uuidv7',    interval '1 day',   interval '30 days', false),
  ('tracking_events',    'timestamptz', interval '1 month', interval '30 days', true);
-- These windows keep the floor's online data on a 1 TB volume; older periods live in the Parquet
-- archive. A small deployment sets monthly periods and longer retentions with one UPDATE per row
-- (both rows of an archive pair in one transaction).

-- The system workspace: owner of maintenance jobs and system connections (transactional mail).
INSERT INTO workspaces (id, slug, name) VALUES ('00000000-0000-7000-8000-000000000000', 'system', 'System');

-- The command-line client: a public client (it holds no secret) and the only one allowed the device
-- authorization grant, for the API as its resource; it has no redirect URIs because it never uses
-- the browser redirect of the authorization code grant.
INSERT INTO oauth_clients (client_id, kind, name, redirect_uris, auth_method)
  VALUES ('norbelys-cli', 'registered', 'Norbelys CLI', '{}', 'none');

-- ───────────────────────────── 11. updated_at triggers ─────────────────────────────
-- On a partitioned parent the trigger is cloned to every partition, present and future; the
-- loop therefore skips partitions (relispartition) so no leaf gets it twice.
DO $$ DECLARE t text; BEGIN
  FOR t IN SELECT DISTINCT c.table_name FROM information_schema.columns c
           JOIN pg_class r ON r.relname = c.table_name JOIN pg_namespace n ON n.oid = r.relnamespace AND n.nspname = 'public'
           WHERE c.table_schema = 'public' AND c.column_name = 'updated_at' AND NOT r.relispartition LOOP
    EXECUTE format('CREATE TRIGGER %I_updated_at BEFORE UPDATE ON %I FOR EACH ROW EXECUTE FUNCTION set_updated_at()', t, t);
  END LOOP;
END $$;

-- Flush the paired partition-policy seed checks before changing ownership/grants.
-- Executing one complete transactional script must not leave deferred trigger events on DDL targets.
SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

-- ───────────────────────────── 12. Ownership, grants and row security ─────────────────────────────
-- Everything is owned by norbelys_owner (migrations run as it). FORCE applies policies even
-- to the owner, so a mistaken query run as the owner cannot bypass them either: the owner's only
-- policies are the read-only ones its detach checks need (at the end of this file), and it has
-- none for writing a tenant row.
ALTER SCHEMA public OWNER TO norbelys_owner;                           -- the owner creates partitions through ensure_partition
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations' LOOP
    EXECUTE format('ALTER TABLE %I OWNER TO norbelys_owner', r.tablename);
  END LOOP;
  FOR r IN SELECT p.proname, pg_get_function_identity_arguments(p.oid) AS args
           FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'public' LOOP
    EXECUTE format('ALTER FUNCTION %I(%s) OWNER TO norbelys_owner', r.proname, r.args);
  END LOOP;
  ALTER FUNCTION provider_webhook_workspace(uuid) OWNER TO norbelys_lookup;
  ALTER FUNCTION invitation_by_token(bytea) OWNER TO norbelys_lookup;
  ALTER FUNCTION api_key_by_hash(bytea) OWNER TO norbelys_lookup;
  ALTER FUNCTION sso_route(text) OWNER TO norbelys_lookup;

END $$;

-- Tenant tables: every table with a workspace_id column, minus the documented exceptions.
-- The exceptions are keyed by something else (a grant, a job) and are reached by the
-- identity module or the runner before a workspace is known.
CREATE TABLE rls_exceptions (table_name text PRIMARY KEY, reason text NOT NULL);
INSERT INTO rls_exceptions VALUES
  ('oauth_grants',        'identity: the grant determines the workspace; read at the token endpoint'),
  ('sessions',            'identity: active_workspace_id is a session property, not a tenant row'),
  ('dispatch_workspaces', 'system: round-robin cursors'),
  ('workspace_deletions', 'system: operator queue'),
  ('rls_exceptions',      'system: this inventory'),
  ('partition_policies',  'system: no workspace column'),
  ('rollup_watermarks',   'system: no workspace column'),
  ('partition_leaves',    'system: no workspace column'),
  ('provider_event_keys', 'ingress: keyed by provider webhook; written by the api before the workspace is known');

DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT DISTINCT c.table_name FROM information_schema.columns c
           JOIN pg_class pc ON pc.relname = c.table_name JOIN pg_namespace n ON n.oid = pc.relnamespace AND n.nspname = 'public'
           WHERE c.table_schema = 'public' AND c.column_name = 'workspace_id' AND NOT pc.relispartition
             AND c.table_name NOT IN (SELECT table_name FROM rls_exceptions) LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', r.table_name);
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', r.table_name);
    EXECUTE format('CREATE POLICY workspace_isolation ON %I AS PERMISSIVE FOR ALL TO norbelys_app USING (workspace_id = current_workspace())', r.table_name);
  END LOOP;
END $$;
-- The policy has no WITH CHECK clause: it defaults to USING, so a row written for another
-- workspace is refused as well.

-- Grants: enumerated, by role. A column grant never subtracts a table grant, so every role
-- starts from nothing and receives exactly what it uses.
GRANT USAGE ON SCHEMA public TO norbelys_app, norbelys_system, norbelys_tracking, norbelys_worker, norbelys_scheduler, norbelys_lookup;
-- system: migrate (as owner), admin, analytics, archive. Everything, with BYPASSRLS; its only net is the application predicate.
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO norbelys_system;
-- The increments stream has exactly three producers, all under the transaction_timeout set on
-- their roles at the top of this file: the api, the worker and the tracker. The system role,
-- which has no such bound, reads it (rollup, recount) and never writes it, so no increment can
-- commit below the rollup's watermark; counters for old periods are rebuilt by
-- `admin analytics rebuild` from the facts, not from increments.
REVOKE INSERT, UPDATE, DELETE ON stats_increments FROM norbelys_system;
-- api: business and identity rows under the workspace policy; system tables excluded below.
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO norbelys_app;
REVOKE ALL ON signing_keys, rls_exceptions, dispatch_workspaces, workspace_deletions, partition_policies, partition_leaves, rollup_watermarks FROM norbelys_app;
REVOKE ALL ON restore_drills FROM norbelys_app;                          -- the operator's record; the worker reads it as the system login
-- No leaf of any partitioned table grants DML to any role: writes go through the parents, the
-- archive export reads leaves, the owner's DDL drops them (the hash partitions of
-- provider_event_keys exist before the blanket grants above and are revoked here; the range
-- leaves are created later by ensure_partition with SELECT only).
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = 'public' AND c.relispartition AND c.relkind = 'r' LOOP
    EXECUTE format('REVOKE INSERT, UPDATE, DELETE ON %I FROM norbelys_app, norbelys_system', r.relname);
  END LOOP;
END $$;
GRANT SELECT ON signing_keys TO norbelys_app;                            -- verify and sign; rotation is admin's
GRANT SELECT, INSERT, UPDATE ON dispatch_workspaces TO norbelys_app;     -- the statement-level registration trigger runs as the invoker
GRANT INSERT ON workspace_deletions TO norbelys_app;
GRANT SELECT ON partition_policies TO norbelys_app;
GRANT SELECT ON rollup_watermarks TO norbelys_app;                       -- the computed_at of a campaign's stats: how far the rollup has counted
-- campaign_daily_stats has one writer, the rollup (and the recount) as the system role.
REVOKE INSERT, UPDATE, DELETE ON campaign_daily_stats, message_daily_stats FROM norbelys_app;
-- messages: the serving roles may update state columns only; everything else is immutable after insert.
REVOKE UPDATE ON messages FROM norbelys_app;
GRANT UPDATE (state, status_detail, attempt_number, sent_at, tracking, thread_id, in_reply_to, internet_message_id, updated_at, deleted_at) ON messages TO norbelys_app;
-- worker: sender, inbox, worker. Business tables under the same workspace policy, set per claimed
-- row; never identity secrets or ceremonies; sealed columns reachable only through the accessors.
DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY[
    'people', 'groups', 'group_people', 'segments', 'suppressions', 'imports', 'import_people', 'exports',
    'sender_identities', 'receive_bindings', 'connection_usage', 'quota_scope_usage', 'sending_domains', 'provider_webhooks',
    'campaigns', 'campaign_senders', 'campaign_sender_rotation', 'campaign_sender_affinity', 'step_assignments', 'step_winner_selections', 'enrollments',
    'delivery_queue', 'attempts', 'delivery_events', 'recipient_holds', 'recipient_validations',
    'threads', 'inbound_messages', 'jobs', 'job_lanes', 'outbox_events', 'webhook_deliveries', 'webhook_receipts',
    'message_contents', 'attachments', 'message_attachments', 'stats_increments', 'ai_usage', 'ai_calls', 'audit_log', 'message_id_directory'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO norbelys_worker', t);
  END LOOP;
  FOREACH t IN ARRAY ARRAY['workspaces', 'memberships', 'person_field_definitions', 'steps', 'step_revisions', 'step_revision_variants',
                           'variants', 'variant_revisions', 'message_engagement', 'campaign_daily_stats', 'message_daily_stats', 'partition_policies', 'partition_leaves', 'job_schedules'] LOOP
    EXECUTE format('GRANT SELECT ON %I TO norbelys_worker', t);
  END LOOP;
END $$;
GRANT UPDATE ON job_schedules TO norbelys_worker;                        -- the schedule loop advances next_run_at
GRANT UPDATE (winner_variant_id, winner_variant_version, winner_selected_at) ON step_revisions TO norbelys_worker;   -- an automatic test's winner, named by enrollment.advance
GRANT SELECT, INSERT, UPDATE ON dispatch_workspaces TO norbelys_worker;  -- the registration trigger runs as the invoker (enrollments, bindings, queue rows)
GRANT SELECT, INSERT ON messages TO norbelys_worker;
GRANT UPDATE (state, status_detail, attempt_number, sent_at, tracking, thread_id, in_reply_to, internet_message_id, updated_at) ON messages TO norbelys_worker;
GRANT SELECT (id, email, name, locale, status) ON users TO norbelys_worker;                  -- transactional mail needs the address
GRANT SELECT (id, user_id, revoked_at), UPDATE (revoked_at, revoked_reason) ON sessions TO norbelys_worker;   -- sessions.revoke_user
GRANT SELECT (workspace_id, id, provider, transport, account_email, account_email_key, smtp, imap, credential_version, status,
              status_detail, paused, checked_at, account_issuer, account_subject, consecutive_failures, paused_until, breaker_opened_at,
              probe_message_id, probe_generation, timezone, send_window,
              daily_limit, send_interval_minutes, send_phase_seconds, next_send_at, next_claim_at, warmup_stage, warmup_evaluated_on, quota_scope_id, created_by, created_at, updated_at),
      UPDATE (status, status_detail, checked_at, consecutive_failures, paused_until, breaker_opened_at, probe_message_id, probe_generation, next_send_at, next_claim_at, warmup_stage, warmup_evaluated_on, updated_at) ON connections TO norbelys_worker;
GRANT SELECT (workspace_id, id, provider, scope_key, messages_per_day, recipients_per_day, window_limit, window_unit, window_seconds, paused_until, paused_detail,
              consecutive_failures, breaker_opened_at, probe_message_id, probe_generation),
      UPDATE (paused_until, paused_detail, consecutive_failures, breaker_opened_at, probe_message_id, probe_generation, updated_at) ON quota_scopes TO norbelys_worker;
GRANT SELECT (workspace_id, id, kind, name, issuer, client_id, metadata, metadata_fetched_at, status),
      UPDATE (metadata, metadata_fetched_at, status, status_detail, updated_at) ON sso_connections TO norbelys_worker;
GRANT SELECT (workspace_id, sso_connection_id, domain, ownership_token, verified_at),
      UPDATE (verified_at) ON sso_email_domains TO norbelys_worker;      -- the daily DNS proof of each SSO email domain
GRANT SELECT (workspace_id, id, url, event_types, filters, header_names, enabled, disabled_reason, failing_since, failure_notified_at, created_at, updated_at),
      UPDATE (enabled, disabled_reason, failing_since, failure_notified_at, updated_at) ON webhook_endpoints TO norbelys_worker;
GRANT SELECT, INSERT ON provider_event_keys TO norbelys_worker;   -- provider.reconcile stores what the relays' events APIs return through the ingress's path, each key once
-- Sealed secrets are reachable only through accessors that require the workspace context, so the
-- global routing view of the scheduler cannot read them. The accessors are owned by
-- the system role (BYPASSRLS) because forced row security would hide the row from a definer
-- owned by norbelys_owner; each refuses any workspace but the current one.
CREATE FUNCTION connection_credential(ws uuid, connection uuid) RETURNS bytea
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    AS $$ SELECT credential FROM connections WHERE workspace_id = ws AND id = connection AND ws = current_workspace() $$;
CREATE FUNCTION set_connection_credential(ws uuid, connection uuid, sealed bytea) RETURNS bigint
    LANGUAGE sql VOLATILE SECURITY DEFINER SET search_path = public, pg_temp
    AS $$ UPDATE connections SET credential = sealed, credential_version = credential_version + 1, updated_at = now()
           WHERE workspace_id = ws AND id = connection AND ws = current_workspace() RETURNING credential_version $$;
CREATE FUNCTION webhook_endpoint_secret(ws uuid, endpoint uuid) RETURNS bytea
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
    AS $$ SELECT secret FROM webhook_endpoints WHERE workspace_id = ws AND id = endpoint AND ws = current_workspace() $$;
ALTER FUNCTION connection_credential(uuid, uuid) OWNER TO norbelys_system;
ALTER FUNCTION set_connection_credential(uuid, uuid, bytea) OWNER TO norbelys_system;
ALTER FUNCTION webhook_endpoint_secret(uuid, uuid) OWNER TO norbelys_system;
REVOKE EXECUTE ON FUNCTION connection_credential(uuid, uuid), set_connection_credential(uuid, uuid, bytea), webhook_endpoint_secret(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION connection_credential(uuid, uuid), set_connection_credential(uuid, uuid, bytea), webhook_endpoint_secret(uuid, uuid) TO norbelys_worker, norbelys_app;
-- scheduler: the global routing and recovery view, reached only through SET LOCAL ROLE from the
-- worker (a SET-only membership, granted at the top of this file). Routing columns of connections
-- and quota scopes, lease columns of the three
-- queues, lanes and dispatch cursors; never a payload, a body or a secret.
GRANT SELECT, UPDATE ON dispatch_workspaces, job_lanes TO norbelys_scheduler;
GRANT SELECT (workspace_id, id, status, paused, next_send_at, next_claim_at, paused_until, timezone, send_window, quota_scope_id, send_interval_minutes, send_phase_seconds, daily_limit, warmup_stage,
              checked_at, created_at,
              consecutive_failures, probe_message_id, probe_generation),
      UPDATE (next_send_at, probe_message_id, probe_generation, updated_at) ON connections TO norbelys_scheduler;
GRANT SELECT (workspace_id, id, paused_until, consecutive_failures, probe_message_id, probe_generation),
      UPDATE (probe_message_id, probe_generation) ON quota_scopes TO norbelys_scheduler;   -- also what SELECT … FOR UPDATE needs
GRANT SELECT (workspace_id, message_id, connection_id, run_at, state, paced, lease_owner, lease_generation, lease_expires_at, submission_started_at, reserved_day, deadline_at),
      UPDATE (state, run_at, lease_owner, lease_generation, lease_expires_at, submission_started_at, reserved_day) ON delivery_queue TO norbelys_scheduler;
GRANT SELECT ON slot_projection TO norbelys_scheduler, norbelys_app;    -- the metric (every workspace); the app sees its own workspace only (row security)
GRANT SELECT (id, workspace_id, queue, kind, state, run_at, lease_owner, lease_expires_at, claims, attempts, max_attempts, effect_started_at, updated_at),
      UPDATE (state, run_at, lease_owner, lease_expires_at, claims, attempts, last_error, updated_at) ON jobs TO norbelys_scheduler;
GRANT SELECT (workspace_id, id, connection_id, enabled, next_poll_at, lease_owner, lease_generation, lease_expires_at),
      UPDATE (lease_owner, lease_generation, lease_expires_at, next_poll_at) ON receive_bindings TO norbelys_scheduler;
-- Directories of the jobs that fan out over workspaces, read as the scheduler: unpublished outbox rows
-- (the relay), due sending domains, active provider webhooks, SSO connections (their daily check);
-- dispatch_workspaces for enrollments.
GRANT SELECT (workspace_id, id, published_at) ON outbox_events TO norbelys_scheduler;
GRANT SELECT (workspace_id, id, status, next_check_at) ON sending_domains TO norbelys_scheduler;
GRANT SELECT (workspace_id, id, status, last_event_at) ON provider_webhooks TO norbelys_scheduler;
GRANT SELECT (workspace_id, id, status) ON sso_connections TO norbelys_scheduler;
-- Figures across workspaces, read as the scheduler and exported as metrics: when each review of
-- an inbound message was asked and decided (the review queue's arrivals and time to review), and
-- the verdicts of AI calls by prompt (the review rate a canary prompt is guarded by). Timestamps
-- and counts only: never a message, an answer or a payload.
GRANT SELECT (review_requested_at, reviewed_at) ON inbound_messages TO norbelys_scheduler;
GRANT SELECT (use_case, prompt_id, canary, review_requested, started_at) ON ai_calls TO norbelys_scheduler;
-- tracking: the drain on the public host. It writes raw events, the per-message rollup and
-- increments; the campaign counters are the rollup's alone, so opens and clicks are counted on
-- the same path as every other fact.
GRANT SELECT, INSERT ON tracking_events TO norbelys_tracking;             -- SELECT for INSERT … RETURNING
GRANT SELECT, INSERT, UPDATE ON message_engagement TO norbelys_tracking;
GRANT INSERT ON stats_increments TO norbelys_tracking;
GRANT SELECT (workspace_id, id, connection_id, kind, campaign_id, step_id, step_revision, variant_id, variant_version) ON messages TO norbelys_tracking;
-- lookup: owner of the pre-workspace lookups, with one SELECT policy per table they read.
GRANT SELECT (workspace_id, id, status) ON provider_webhooks TO norbelys_lookup;
CREATE POLICY lookup_by_id ON provider_webhooks FOR SELECT TO norbelys_lookup USING (true);
GRANT SELECT ON invitations TO norbelys_lookup;
CREATE POLICY lookup_by_token ON invitations FOR SELECT TO norbelys_lookup USING (true);
GRANT SELECT (workspace_id, id, scopes, created_by, expires_at, revoked_at, secret_hash) ON api_keys TO norbelys_lookup;
CREATE POLICY lookup_by_hash ON api_keys FOR SELECT TO norbelys_lookup USING (true);
GRANT SELECT (workspace_id, sso_connection_id, domain, verified_at) ON sso_email_domains TO norbelys_lookup;
CREATE POLICY lookup_by_domain ON sso_email_domains FOR SELECT TO norbelys_lookup USING (true);
GRANT SELECT (workspace_id, id, status) ON sso_connections TO norbelys_lookup;
CREATE POLICY lookup_by_route ON sso_connections FOR SELECT TO norbelys_lookup USING (true);
REVOKE EXECUTE ON FUNCTION provider_webhook_workspace(uuid), invitation_by_token(bytea), api_key_by_hash(bytea), sso_route(text), current_user_id(), current_workspace(),
                           uuidv7_boundary(timestamptz), ensure_partition(regclass, timestamptz), ensure_partitions_ahead(interval, timestamptz),
                           register_dispatch_workspace(), enforce_person_fields(), person_field_value_valid(text, text[], jsonb), set_updated_at() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION provider_webhook_workspace(uuid), invitation_by_token(bytea), api_key_by_hash(bytea), sso_route(text) TO norbelys_app;
GRANT EXECUTE ON FUNCTION current_user_id() TO norbelys_app, norbelys_system;
GRANT EXECUTE ON FUNCTION current_workspace() TO norbelys_app, norbelys_system, norbelys_tracking, norbelys_worker, norbelys_scheduler;
REVOKE EXECUTE ON FUNCTION ascii_lower(text), next_phase_at(timestamptz, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION next_phase_at(timestamptz, integer) TO norbelys_app, norbelys_worker, norbelys_scheduler;   -- connection creation and resume, the start of a cold submission, the claim's catch-up of an idle mailbox
GRANT EXECUTE ON FUNCTION ascii_lower(text) TO norbelys_app, norbelys_system, norbelys_tracking, norbelys_worker, norbelys_scheduler, norbelys_lookup;
GRANT EXECUTE ON FUNCTION uuidv7_boundary(timestamptz) TO norbelys_app, norbelys_system, norbelys_worker, norbelys_tracking;
GRANT EXECUTE ON FUNCTION ensure_partition(regclass, timestamptz), ensure_partitions_ahead(interval, timestamptz) TO norbelys_system;
REVOKE EXECUTE ON FUNCTION period_archived(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION period_archived(text, uuid) TO norbelys_app;   -- `404 archived` on a read of an id whose period left the database
GRANT EXECUTE ON FUNCTION register_dispatch_workspace(), set_updated_at(), enforce_person_fields(), person_field_value_valid(text, text[], jsonb)
    TO norbelys_app, norbelys_worker, norbelys_system, norbelys_tracking;       -- triggers run as the invoker
GRANT SELECT ON partition_policies TO norbelys_system;
-- The bootstrap path: a signed-in user sees their own memberships (and nothing else) before a
-- workspace is chosen; once chosen, the workspace policy applies as everywhere else.
CREATE POLICY own_memberships ON memberships FOR SELECT TO norbelys_app USING (user_id = current_user_id());
CREATE POLICY own_idempotency ON idempotency_keys FOR ALL TO norbelys_app USING (user_id = current_user_id());
-- A person's account log: the api records rows (a sign-in has no user context yet) and reads the
-- signed-in person's own; nobody changes a row once written but retention, as the system login.
ALTER TABLE user_audit_log ENABLE ROW LEVEL SECURITY;
ALTER TABLE user_audit_log FORCE ROW LEVEL SECURITY;
CREATE POLICY own_user_audit ON user_audit_log FOR SELECT TO norbelys_app USING (user_id = current_user_id());
CREATE POLICY record_user_audit ON user_audit_log FOR INSERT TO norbelys_app WITH CHECK (true);
REVOKE UPDATE, DELETE ON user_audit_log FROM norbelys_app;
-- Initial partitions: for every policy, the leaf of now and the next one (today and tomorrow for a
-- daily table, this month and the next for a monthly one); the daily partitions.create job then
-- keeps every table two days ahead and always one period beyond the current one.
SELECT ensure_partitions_ahead(interval '1 day');
-- Policies for the background roles: the workspace policy everywhere (as the api), and the
-- routing/leased views for the scheduler role across workspaces.
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT DISTINCT c.table_name FROM information_schema.columns c
           JOIN pg_class pc ON pc.relname = c.table_name JOIN pg_namespace n ON n.oid = pc.relnamespace AND n.nspname = 'public'
           WHERE c.table_schema = 'public' AND c.column_name = 'workspace_id' AND NOT pc.relispartition
             AND c.table_name NOT IN (SELECT table_name FROM rls_exceptions) LOOP
    EXECUTE format('CREATE POLICY workspace_isolation_worker ON %I AS PERMISSIVE FOR ALL TO norbelys_worker USING (workspace_id = current_workspace())', r.table_name);
  END LOOP;
END $$;
CREATE POLICY routing_connections ON connections FOR ALL TO norbelys_scheduler USING (status = 'active');
CREATE POLICY routing_scopes ON quota_scopes FOR ALL TO norbelys_scheduler USING (true);   -- FOR UPDATE applies UPDATE policies too (PostgreSQL 18, CREATE POLICY)
CREATE POLICY routing_queue ON delivery_queue FOR ALL TO norbelys_scheduler USING (true);
CREATE POLICY routing_bindings ON receive_bindings FOR ALL TO norbelys_scheduler USING (true);
CREATE POLICY routing_lanes ON job_lanes FOR ALL TO norbelys_scheduler USING (true);
CREATE POLICY routing_jobs ON jobs FOR ALL TO norbelys_scheduler USING (true);
CREATE POLICY routing_outbox ON outbox_events FOR SELECT TO norbelys_scheduler USING (published_at IS NULL);
CREATE POLICY routing_domains ON sending_domains FOR SELECT TO norbelys_scheduler USING (true);
CREATE POLICY routing_provider_webhooks ON provider_webhooks FOR SELECT TO norbelys_scheduler USING (status = 'active');
CREATE POLICY routing_sso ON sso_connections FOR SELECT TO norbelys_scheduler USING (true);
CREATE POLICY review_queue ON inbound_messages FOR SELECT TO norbelys_scheduler USING (review_requested_at IS NOT NULL);
CREATE POLICY canary_guard ON ai_calls FOR SELECT TO norbelys_scheduler USING (review_requested IS NOT NULL);
-- The worker reaches these policies only inside SET LOCAL ROLE norbelys_scheduler (a SET-only
-- membership, no inheritance): a claim locks the lane and leases the job rows as the scheduler, commits,
-- then reads each job's payload as the worker under the workspace policy; the sweeper releases
-- expired leases as the scheduler. dispatch_workspaces is an rls_exception (no policy).

-- The tracking role is itself subject to RLS on tracking_events and the counters, with a
-- policy that lets it write any workspace (its rows carry ids signed by the api).
CREATE POLICY tracking_drain ON tracking_events FOR INSERT TO norbelys_tracking WITH CHECK (true);
CREATE POLICY tracking_drain_returning ON tracking_events FOR SELECT TO norbelys_tracking USING (true);
CREATE POLICY tracking_drain ON message_engagement FOR ALL TO norbelys_tracking USING (true);
CREATE POLICY tracking_increments ON stats_increments FOR INSERT TO norbelys_tracking WITH CHECK (true);
CREATE POLICY tracking_lookup ON messages FOR SELECT TO norbelys_tracking USING (true);

-- The archive's detach checks. Detaching a leaf of a table that foreign keys point at (messages,
-- the outbox) makes PostgreSQL check that no row still references the leaf: it joins each
-- referencing table with the leaf, as the current user. The archive's DDL runs as the owner, to
-- which forced row security shows nothing without a policy, so that check would see no row and
-- let a period still referenced (a queued message, an unsettled attempt, a pending webhook
-- delivery) be detached, leaving the reference pointing at a row its parent no longer holds. So
-- the owner may read, and only read, every table on the referencing side of a foreign key to a
-- partitioned table (through the parent when that table is itself partitioned, which is how the
-- check reads it), and every leaf of a referenced table (ensure_partition adds the same policy to
-- each). Its writes still have no policy, so a mistaken data-changing statement run as the owner
-- is still refused or reaches no row; and only the system login may SET ROLE to the owner, a
-- login that bypasses row security already, so these reads open nothing to anyone else.
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT DISTINCT c.conrelid::regclass AS referencing
             FROM pg_constraint c JOIN pg_class p ON p.oid = c.confrelid
            WHERE c.contype = 'f' AND p.relkind = 'p' AND c.conparentid = 0 LOOP
    EXECUTE format('CREATE POLICY detach_check ON %s AS PERMISSIVE FOR SELECT TO norbelys_owner USING (true)', r.referencing);
  END LOOP;
END $$;

-- Migration history belongs to the external maintenance login. Runtime logins only read it.
REVOKE ALL ON _sqlx_migrations FROM PUBLIC, norbelys_app, norbelys_worker, norbelys_system, norbelys_tracking;
GRANT SELECT ON _sqlx_migrations TO norbelys_app, norbelys_worker, norbelys_system, norbelys_tracking;
GRANT UPDATE (deleted_at), DELETE ON person_field_definitions TO norbelys_worker;
