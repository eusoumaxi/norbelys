-- Cluster roles, provisioned explicitly before database migrations.
-- Run as the cluster administrator, outside application hosts. No passwords are stored here.
-- Callers wrap this script in a transaction on the postgres maintenance database.
SELECT pg_advisory_xact_lock(734286292);

-- ───────────────────────────── 0. Roles (cluster level, idempotent) ─────────────────────────────
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_owner') THEN
    CREATE ROLE norbelys_owner NOLOGIN;                       -- owns every object; migrations only
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_app') THEN
    CREATE ROLE norbelys_app LOGIN NOBYPASSRLS;               -- api: RLS enforced
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_system') THEN
    CREATE ROLE norbelys_system LOGIN BYPASSRLS;              -- admin, analytics, archive (SET ROLE norbelys_owner)
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_tracking') THEN
    CREATE ROLE norbelys_tracking LOGIN NOBYPASSRLS;          -- tracking drain on the public host
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_lookup') THEN
    CREATE ROLE norbelys_lookup NOLOGIN NOBYPASSRLS;          -- owns the few SECURITY DEFINER lookups; narrow policies
  END IF;
  -- The background roles run under row security like the api, so a predicate forgotten in their
  -- code cannot reach another workspace: each unit of work selects its workspace with SET LOCAL.
  -- What they must see across workspaces (routing and lease columns, to find due work) is the
  -- scheduler role's narrow view, entered with SET ROLE.
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_worker') THEN
    CREATE ROLE norbelys_worker LOGIN NOBYPASSRLS;            -- sender, inbox, worker: per-workspace transactions
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_scheduler') THEN
    CREATE ROLE norbelys_scheduler NOLOGIN NOBYPASSRLS;       -- SET ROLE target of norbelys_worker: routing rows only
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'norbelys_metrics') THEN
    CREATE ROLE norbelys_metrics LOGIN NOBYPASSRLS;           -- the PostgreSQL metrics scraper: pg_monitor, no table grants
  END IF;
END $$;
GRANT pg_monitor TO norbelys_metrics;
-- Role memberships are SET-only (no inheritance): the worker reaches the scheduler's global
-- routing view only inside `SET LOCAL ROLE norbelys_scheduler`. With inheritance, the scheduler's
-- USING (true) policies and its grants would combine with the worker's own (permissive policies
-- are OR-ed), and the worker would see every workspace's rows in the tables both can read.
-- The system login reaches the owner's DDL (an archived partition's drop) through `SET ROLE
-- norbelys_owner`; migrations run as the bootstrap login that creates these roles.
GRANT norbelys_scheduler TO norbelys_worker WITH INHERIT FALSE, SET TRUE;
GRANT norbelys_owner TO norbelys_system WITH INHERIT FALSE, SET TRUE;
-- A hard bound on the age of a transaction, for every role that writes stats_increments. The
-- rollup consumes only increments older than 120 s, twice the longest of these timeouts, so no
-- increment can commit below a watermark the rollup already passed. transaction_timeout
-- (PostgreSQL 17+) bounds the whole transaction; statement_timeout and
-- idle_in_transaction_session_timeout do not bound a transaction of many short statements.
ALTER ROLE norbelys_app      SET transaction_timeout = '30s';
ALTER ROLE norbelys_worker   SET transaction_timeout = '60s';
ALTER ROLE norbelys_tracking SET transaction_timeout = '30s';
ALTER ROLE norbelys_app      SET idle_in_transaction_session_timeout = '15s';
ALTER ROLE norbelys_worker   SET idle_in_transaction_session_timeout = '30s';
ALTER ROLE norbelys_tracking SET idle_in_transaction_session_timeout = '15s';
-- Every day the ledgers and partitions name is a UTC day. The statements compute it explicitly,
-- (now() AT TIME ZONE 'UTC')::date, and every login role also runs in UTC, so a stray current_date
-- cannot read another zone's day.
ALTER ROLE norbelys_app      SET timezone = 'UTC';
ALTER ROLE norbelys_worker   SET timezone = 'UTC';
ALTER ROLE norbelys_tracking SET timezone = 'UTC';
ALTER ROLE norbelys_system   SET timezone = 'UTC';

