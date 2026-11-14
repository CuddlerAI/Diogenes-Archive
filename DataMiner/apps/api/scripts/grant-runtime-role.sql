-- Least-privilege grants for the ingestion API's runtime role (`dataminer_api`).
--
-- Run as the instance root user (`postgres`) — which must already be a member
-- of the migration role, `GRANT dataminer_migrator TO postgres`, so it inherits
-- the table ownership the GRANTs below require. Root rather than the migration
-- role because the first two statements alter role attributes, which the schema
-- owner is not permitted to do.
--
-- Run AFTER `pnpm migrate`, and again after any migration that adds a table —
-- grants are per-object and new tables arrive with none. Deliberately not a
-- migration itself: migrations are
-- the shared whole-database schema (console reads what this API writes), while
-- role grants are per-deployment and name roles that need not exist everywhere.
--
-- The three tiers below are derived from the SQL actually issued by
-- apps/api/src, not from a guess at what an ingestion API might want:
--
--   1. read-write  — the ingestion tables this API owns
--   2. governance — full-table reads plus explicitly narrow column writes
--   3. no grant    — everything else, by omission
--
-- Tier 2 is the SQL half of the provisioning boundary in the README: the API
-- does not manufacture grants, catalogs, games, devices, or consents. That is
-- stated in prose there and enforced here, so a code path that tried would fail
-- on a permission error rather than quietly create governance facts.
--
-- Tier 3 matters most and is invisible: `operators`, `operator_sessions`,
-- `operator_email_challenges`, the `annotator_*` credential tables,
-- `invite_codes`, and `console_rate_limits` are the console's authentication
-- surface. This role gets nothing on them — not even SELECT — so an ingestion
-- compromise cannot read or mint console identity (docs/11 §4.2).

BEGIN;

-- Cloud SQL undoes least privilege unless you undo Cloud SQL first.
--
-- `gcloud sql users create` does not create a plain LOGIN role: it also grants
-- the new role membership in `cloudsqlsuperuser` and sets CREATEDB/CREATEROLE
-- as direct role attributes. `cloudsqlsuperuser` holds `UC` on schema `public`,
-- so every grant below is decoration until the membership is gone — the role
-- can CREATE TABLE, CREATE ROLE, and CREATE DATABASE regardless. Verified on
-- this instance: before these two statements, `dataminer_api` successfully ran
-- `CREATE TABLE probe_x(i int)` while correctly being denied `SELECT` on
-- `operators`. The denial made the setup look sound; it was not.
--
-- Membership is not enough on its own, because CREATEDB/CREATEROLE are set
-- directly on the role rather than inherited. Both statements are required.
--
-- This applies to every role made this way, including the console's
-- `dataminer_console` and `dataminer_app` (docs/11 §4.2). Guard the Cloud SQL
-- role so this same authority can be applied and asserted by CI on stock
-- PostgreSQL; production always takes the REVOKE branch.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataminer_api') THEN
    RAISE EXCEPTION 'role dataminer_api is missing — create it first (gcloud sql users create)';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'cloudsqlsuperuser') THEN
    REVOKE cloudsqlsuperuser FROM dataminer_api;
  END IF;
END $$;
ALTER ROLE dataminer_api NOCREATEDB NOCREATEROLE;

-- Start from zero so re-running this file after a privilege experiment
-- converges on the same state rather than accumulating grants.
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM dataminer_api;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM dataminer_api;

DO $$
BEGIN
  EXECUTE format(
    'GRANT CONNECT ON DATABASE %I TO dataminer_api',
    current_database());
END $$;
GRANT USAGE ON SCHEMA public TO dataminer_api;

-- 1. Read-write: sessions/clips and the upload, verification, and purge
-- machinery. `idempotency_requests` is the only table this role deletes from
-- (expired replay records); the rest are insert/update.
GRANT SELECT, INSERT, UPDATE, DELETE ON
    sessions,
    capture_runs,
    clips,
    artifact_objects,
    upload_intents,
    upload_parts,
    multipart_uploads,
    verification_jobs,
    purge_jobs,
    idempotency_requests
TO dataminer_api;

-- Session-root evidence is declared after a session completes. The serving
-- path inserts the immutable declaration, updates upload state as each object
-- lands, and reads it for reconciliation. It never deletes these audit and
-- reproducibility records, so keep DELETE withheld. Migration 0010 introduced
-- this table; omitting this grant caused PUT /v1/sessions/{id}/artifacts to
-- return PostgreSQL 42501 for every production call (issue #825).
GRANT SELECT, INSERT, UPDATE ON session_artifacts TO dataminer_api;

-- `audit_events` is append-only. Withholding UPDATE and DELETE is the point:
-- an audit trail a compromised runtime role can rewrite is not an audit trail.
GRANT SELECT, INSERT ON audit_events TO dataminer_api;

-- Maintenance-only state. The worker tombstones expired client telemetry and
-- advances its export outbox; it never creates or deletes either class of row.
-- Keeping these out of the broad read-write tier avoids accidentally granting
-- DELETE while still matching the exact SELECT/UPDATE statements in
-- repository.ts and client-events-export.ts.
GRANT SELECT, UPDATE ON
    client_events,
    outbox_jobs
TO dataminer_api;

-- 2. Governance: provisioned by operators through the console and read here to
-- authenticate a device and to resolve the governance facts a session declares.
GRANT SELECT ON
    devices,
    device_profile_bindings,
    annotators,
    consents,
    capture_grants,
    games,
    trusted_catalog_versions,
    catalog_payloads,
    catalog_releases,
    catalog_release_entries,
    clip_trainability
TO dataminer_api;

-- Authentication observes liveness and throttles this single column update in
-- repository.ts. Keep the rest of the operator-owned device row immutable to
-- the ingestion role; a table-wide UPDATE grant would dissolve that boundary.
GRANT UPDATE (last_seen_at) ON devices TO dataminer_api;

-- The lone sequence in the schema; USAGE only, and only because an INSERT above
-- draws from it.
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO dataminer_api;

-- No DDL, ever: the runtime role must not be able to alter the schema it runs
-- against, and `schema_migrations` stays readable only to the migration role so
-- the ledger cannot be edited from the serving path.
REVOKE CREATE ON SCHEMA public FROM dataminer_api;

-- Every ordinary table must either be reachable by the ingestion role above or
-- be reviewed here as belonging to another trust boundary. This turns the next
-- migration that adds an unclassified table into a failed grants job and a red
-- PostgreSQL CI build instead of another production-only 42501.
DO $$
DECLARE
  reviewed_ungranted CONSTANT text[] := ARRAY[
    -- Console/operator and annotator identity surfaces.
    'operators',
    'operator_sessions',
    'operator_email_challenges',
    'console_rate_limits',
    'invite_codes',
    'annotator_invite_uses',
    'annotator_credentials',
    'annotator_email_challenges',
    'annotator_sessions',
    'annotator_profiles',
    'mirror_credential_outbox',
    -- Console review/Hugging Face reconciliation surfaces.
    'review_verdicts',
    'hf_release_checks',
    'hf_release_items',
    'hf_manual_marks',
    -- Migration/operations-only state. The serving role must not reach these.
    'schema_migrations',
    'upload_restart_attempts',
    'approved_game_executables',
    'depth_consent_policy_versions'
  ];
  bare text;
BEGIN
  SELECT string_agg(c.relname, ', ' ORDER BY c.relname) INTO bare
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'public'
     AND c.relkind = 'r'
     AND NOT (c.relname = ANY (reviewed_ungranted))
     AND NOT has_table_privilege('dataminer_api', c.oid, 'SELECT')
     AND NOT has_table_privilege('dataminer_api', c.oid, 'INSERT')
     AND NOT has_table_privilege('dataminer_api', c.oid, 'UPDATE')
     AND NOT has_table_privilege('dataminer_api', c.oid, 'DELETE');
  IF bare IS NOT NULL THEN
    RAISE EXCEPTION
      'these public tables have no dataminer_api privilege and are not in reviewed_ungranted: %. '
      'A migration added a table and the runtime grants were not classified — grant it above, '
      'or add it to reviewed_ungranted with a reason.', bare;
  END IF;
END $$;

-- Route-level assertion for the exact capability behind issue #825. A generic
-- "some privilege exists" check would let SELECT-only pass while INSERT still
-- 500s, so pin all three required operations and the intentional DELETE denial.
DO $$
BEGIN
  IF NOT has_table_privilege('dataminer_api', 'session_artifacts', 'SELECT')
     OR NOT has_table_privilege('dataminer_api', 'session_artifacts', 'INSERT')
     OR NOT has_table_privilege('dataminer_api', 'session_artifacts', 'UPDATE') THEN
    RAISE EXCEPTION
      'dataminer_api needs SELECT, INSERT and UPDATE on session_artifacts for '
      'PUT /v1/sessions/{id}/artifacts';
  END IF;
  IF has_table_privilege('dataminer_api', 'session_artifacts', 'DELETE') THEN
    RAISE EXCEPTION 'dataminer_api must not delete session artifact evidence';
  END IF;
  IF NOT has_column_privilege('dataminer_api', 'devices', 'last_seen_at', 'UPDATE') THEN
    RAISE EXCEPTION
      'dataminer_api needs UPDATE(last_seen_at) on devices for the throttled authentication heartbeat';
  END IF;
  IF has_table_privilege('dataminer_api', 'devices', 'UPDATE') THEN
    RAISE EXCEPTION
      'dataminer_api must not hold table-wide UPDATE on operator-owned device rows';
  END IF;
END $$;

COMMIT;
