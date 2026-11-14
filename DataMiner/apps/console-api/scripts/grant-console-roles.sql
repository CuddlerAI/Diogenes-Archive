-- Least-privilege grants for the console's three runtime roles.
--
-- THIS FILE IS THE SINGLE AUTHORITY. Until 2026-07-28 a second, divergent copy
-- lived at tools/prod-console-db-grants.sql; each got edits the other missed
-- (#240 landed annotator_profiles + outbox_jobs only here, #241 landed the HF
-- tables only there). Production ran this one, so review_verdicts and the three
-- hf_* tables were never granted and two console routes 500'd on bare tables.
-- The other file has been deleted. Do not reintroduce a second copy: add tables
-- here, and the completeness assertion at the bottom will catch what you miss.
--
-- Run as the instance root user (`postgres`), which must be a member of the
-- migration role so it inherits the table ownership these GRANTs require:
--   GRANT dataminer_migrator TO postgres;
-- Root rather than the schema owner because the first statements alter role
-- attributes, which the owner is not permitted to do.
--
-- Run AFTER the schema exists (the ingestion API's `pnpm migrate` is the single
-- whole-database migration — see docs/21 §3 step 5) and again after any
-- migration that adds a table: grants are per-object, new tables arrive bare.
--
-- The two roles exist because docs/11 §4.2 makes the boundary physical rather
-- than conditional. console-api opens two pools in one process:
--   pool_console → dataminer_console → serves /console/v1/* (governance)
--   pool_app     → dataminer_app     → serves /app/v1/*     (annotator surface)
-- The point is that a routing mistake cannot become a privilege escalation: even
-- if an /app/v1 code path called governance SQL, the role cannot execute it.
--
-- Sets below are derived from the SQL actually issued by apps/console-api/src:
--   dataminer_console → repository.ts + browse-repository.ts + governance-repository.ts
--   dataminer_app     → app-repository.ts

BEGIN;

-- Preconditions. Without these the REVOKEs below fail partway with a bare
-- "role does not exist", which reads like a typo rather than missing
-- provisioning. `dataminer_catalog_publisher` in particular may exist only as a
-- NOLOGIN placeholder until #252 gives it credentials — that is fine here, the
-- grants attach to the role either way.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataminer_console') THEN
    RAISE EXCEPTION 'role dataminer_console is missing — create it first (gcloud sql users create)';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataminer_app') THEN
    RAISE EXCEPTION 'role dataminer_app is missing — create it first (gcloud sql users create)';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataminer_catalog_publisher') THEN
    RAISE EXCEPTION 'role dataminer_catalog_publisher is missing — create it first: '
      'CREATE ROLE dataminer_catalog_publisher NOLOGIN; (#252 gives it LOGIN + password)';
  END IF;
END $$;

-- Cloud SQL hands every `gcloud sql users create` role membership in
-- `cloudsqlsuperuser` (which holds UC on schema public) plus CREATEDB/CREATEROLE
-- as direct attributes. Without these four statements every grant below is
-- decoration: the roles could CREATE TABLE and CREATE ROLE regardless, and
-- `dataminer_app` would reach the operator tables through the back door that
-- §4.2 exists to close. Verified on the ingestion role, where the same setup
-- allowed CREATE TABLE while correctly denying SELECT on `operators`.
-- Guarded on the role existing: `cloudsqlsuperuser` is a Cloud SQL construct, so
-- an unguarded REVOKE makes this file unrunnable on stock PostgreSQL — which is
-- what CI uses to prove the file still applies. On Cloud SQL the branch always
-- fires; the security property is unchanged.
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'cloudsqlsuperuser') THEN
    REVOKE cloudsqlsuperuser FROM dataminer_console;
    REVOKE cloudsqlsuperuser FROM dataminer_app;
    REVOKE cloudsqlsuperuser FROM dataminer_catalog_publisher;
  END IF;
END $$;
ALTER ROLE dataminer_console NOCREATEDB NOCREATEROLE;
ALTER ROLE dataminer_app     NOCREATEDB NOCREATEROLE;
ALTER ROLE dataminer_catalog_publisher NOCREATEDB NOCREATEROLE;

REVOKE ALL ON ALL TABLES IN SCHEMA public FROM dataminer_console, dataminer_app, dataminer_catalog_publisher;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM dataminer_console, dataminer_app, dataminer_catalog_publisher;
REVOKE CREATE ON SCHEMA public FROM dataminer_console, dataminer_app, dataminer_catalog_publisher;

-- current_database() rather than a literal: this file hardcoded
-- `dataminer_production`, which meant it could not be applied to staging or to a
-- CI database at all. The database it is run against is the one that gets the
-- grant, which is what every caller already meant.
DO $$
BEGIN
  EXECUTE format(
    'GRANT CONNECT ON DATABASE %I TO dataminer_console, dataminer_app, dataminer_catalog_publisher',
    current_database());
END $$;
GRANT USAGE ON SCHEMA public TO dataminer_console, dataminer_app, dataminer_catalog_publisher;

-- ---------------------------------------------------------------------------
-- dataminer_console — the governance surface.
-- ---------------------------------------------------------------------------

-- Operator identity and the governance rows operators create. This role is the
-- only one that may touch the operator tables at all.
GRANT SELECT, INSERT, UPDATE, DELETE ON
    operators,
    operator_sessions,
    operator_email_challenges,
    console_rate_limits,
    annotators,
    consents,
    capture_grants,
    devices,
    clips,
    purge_jobs,
    idempotency_requests,
    mirror_credential_outbox
TO dataminer_console;

-- Profile qualification is revoked by tombstoning the binding, never DELETE.
-- Governance may add/revoke rows; the audit history remains queryable.
GRANT SELECT, INSERT, UPDATE ON device_profile_bindings TO dataminer_console;

GRANT SELECT, INSERT, UPDATE ON games, trusted_catalog_versions TO dataminer_console;

-- Registration stores exact bytes once. UPDATE/DELETE remain withheld and the
-- table trigger rejects mutation even for an owner.
GRANT INSERT ON catalog_payloads TO dataminer_console;

-- Browse surface: reads what the ingestion API writes. Read-only on purpose —
-- the console displays the capture record, it does not author it.
GRANT SELECT ON
    sessions,
    capture_runs,
    -- session detail 的「根快照」面板只读摄取侧写入的身份与标定证据。
    -- Console 不创建、不改写这些行；SELECT 是该路由唯一需要的权限。
    session_artifacts,
    artifact_objects,
    upload_intents,
    upload_parts,
    multipart_uploads,
    verification_jobs,
    games,
    trusted_catalog_versions,
    catalog_payloads,
    catalog_releases,
    catalog_release_entries,
    clip_trainability,
    annotator_credentials,
    annotator_invite_uses,
    -- Read-only on purpose (docs/34 §4.2): operator-side profile edits and the
    -- tombstone wipe both execute through other means — the edit goes through
    -- the app pool, the wipe through a SECURITY DEFINER trigger (0011) — so
    -- the governance role never needs write access to contact facts.
    annotator_profiles
TO dataminer_console;

-- Invite administration is a governance write. Issuing and self-issuing insert
-- rows; revocation updates an unused row; list/reveal paths select them. DELETE
-- remains withheld because invite history is tombstoned, never erased.
GRANT SELECT, INSERT, UPDATE ON invite_codes TO dataminer_console;

GRANT SELECT, INSERT ON audit_events TO dataminer_console;

-- Consent revocation and annotator tombstoning erase behaviour telemetry in the
-- same statement that writes the governance row (governance-repository.ts:112
-- and :179 — docs/11 §6.4 requires telemetry be reachable by a consent purge).
-- SELECT for the WHERE, UPDATE for the erase; never INSERT (the console does not
-- author telemetry) and never DELETE (erasure is UPDATE-to-purged, so the row
-- survives as auditable evidence that the purge happened).
-- Found by the completeness assertion below, not by an incident: without this
-- grant, revoking a consent in production fails on `permission denied for table
-- client_events` and the erase silently never happens.
GRANT SELECT, UPDATE ON client_events TO dataminer_console;

-- Data-review and HF reconciliation (docs/33 §5–§6). All append-only: a verdict
-- or a check is corrected by a newer row, never by rewriting history, so UPDATE
-- is withheld from every table here on purpose.
--
-- These four were the 2026-07-28 production incident: migrations 0011/0013
-- created them, this file was not updated, and `GET /console/v1/review/verdicts`
-- and `GET /console/v1/hf/status` returned 500 on a bare table while every
-- R2-only route kept working. The completeness assertion at the bottom of this
-- file exists so that the next table cannot fail the same way.
GRANT SELECT, INSERT ON review_verdicts   TO dataminer_console;
GRANT SELECT, INSERT ON hf_release_checks TO dataminer_console;
GRANT SELECT, INSERT ON hf_manual_marks   TO dataminer_console;
-- hf_release_items is a whole-repo snapshot cache refreshed by DELETE + INSERT
-- in one transaction (hf.ts:245-253) — the one deliberate DELETE for this role.
GRANT SELECT, INSERT, DELETE ON hf_release_items TO dataminer_console;

-- Dedicated D2 publisher. It cannot mutate trusted rows or payload bytes and
-- the only permitted release UPDATEs remain constrained by immutable triggers.
GRANT SELECT ON trusted_catalog_versions, catalog_payloads TO dataminer_catalog_publisher;
GRANT SELECT, INSERT, UPDATE ON catalog_releases TO dataminer_catalog_publisher;
GRANT SELECT, INSERT ON catalog_release_entries TO dataminer_catalog_publisher;
GRANT INSERT ON audit_events TO dataminer_catalog_publisher;
GRANT EXECUTE ON FUNCTION publish_catalog_release(uuid, text) TO dataminer_catalog_publisher;
-- SELECT 不足以给 roster 行加 FOR SHARE 锁（行锁子句要 UPDATE 级权限），而这个
-- 角色不该能改写 trusted_catalog_versions。加锁因此走迁移 0020 的 SECURITY
-- DEFINER 函数，这里只授它 EXECUTE。少了这一行，publish 会在事务里以
-- `permission denied for table trusted_catalog_versions` 失败。
GRANT EXECUTE ON FUNCTION lock_publishable_catalog_versions(uuid[], boolean) TO dataminer_catalog_publisher;

-- ---------------------------------------------------------------------------
-- dataminer_app — the annotator surface. The deny list is the design.
-- ---------------------------------------------------------------------------

-- No DELETE anywhere for this role (docs/11 §4.2): an end user's surface may
-- create and amend its own identity rows, never remove records. Withdrawal is a
-- purge *request* that the governance side executes, not a row deletion here.
GRANT SELECT, INSERT, UPDATE ON
    annotators,
    annotator_credentials,
    annotator_email_challenges,
    annotator_invite_uses,
    annotator_sessions,
    -- Contact profile (0011, docs/34 §5.4). The app role is the only writer:
    -- self-service edits arrive here directly, operator-side edits are routed
    -- through this same pool by the annotator-admin service.
    annotator_profiles,
    consents,
    devices,
    invite_codes,
    console_rate_limits
TO dataminer_app;

-- Enrollment may append an approved qualification, but the public annotator
-- role cannot reactivate or revoke one after governance has decided otherwise.
GRANT SELECT, INSERT ON device_profile_bindings TO dataminer_app;

GRANT SELECT, INSERT ON audit_events TO dataminer_app;
GRANT SELECT, INSERT ON client_events TO dataminer_app;
-- recordClientEvent writes the export outbox row in the same single-statement
-- CTE as the client_events INSERT (docs/25 §5), so without this grant the whole
-- statement — and POST /app/v1/client-events — fails `permission denied for
-- table outbox_jobs`. INSERT only, deliberately: the app surface *produces*
-- export jobs, it never claims or drains them; locking, retry and publish
-- belong to the ingestion API's worker. No SELECT is needed either — RETURNING
-- reads only client_events, and ON CONFLICT DO NOTHING checks no columns.
-- Both the failure and the INSERT-only sufficiency (including the replay path)
-- were probed on real PostgreSQL 15, 2026-07-28; PGlite cannot exercise grants
-- (docs/11 §11 S1b), so tests will never catch a regression here. Staging's
-- suffixed role got the same grant via tools/logs-warehouse/grants-app-role.sql.
GRANT INSERT ON outbox_jobs TO dataminer_app;
GRANT SELECT ON trusted_catalog_versions TO dataminer_app;
-- 绑定前 catalog 启动（docs/37 §Bootstrap，issue #323）。SELECT，且只有 SELECT：
-- 这条路把已发布的不可变信封原样读给一个已登录、尚未绑定的客户端，让它知道有哪些
-- 已批准的设备配置可以用来绑定。发布与改写仍然只属于 dataminer_catalog_publisher
-- ——那个角色有 INSERT/UPDATE，这个角色永远不该有。
GRANT SELECT ON catalog_releases TO dataminer_app;

-- Deliberately NOT granted to dataminer_app, and each omission is load-bearing:
--   * operators / operator_sessions / operator_email_challenges — not even
--     SELECT. This is the §4.2 headline: the annotator surface must not be able
--     to read, let alone mint, console identity. A `role` column written by the
--     wrong path is a privilege escalation, so the path is removed instead.
--   * clips / upload_intents / upload_parts / multipart_uploads /
--     verification_jobs / purge_jobs — the capture record belongs to the
--     ingestion API and the governance side.
--   * capture_grants — a grant is issued to an annotator, never by one.
--   * sessions / capture_runs / artifact_objects — ingestion-owned.

GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO dataminer_console, dataminer_app;

-- ---------------------------------------------------------------------------
-- Assertions. These run inside the transaction, so a violation rolls the whole
-- file back and nothing is half-applied.
-- ---------------------------------------------------------------------------

-- (1) Completeness. The 2026-07-28 incident was a table this file never
-- mentioned: nothing failed at grant time, and the gap only surfaced when a
-- human clicked the feature in production. A table is now either granted above
-- or listed here as deliberately ungranted — silence is no longer an option.
DO $$
DECLARE
  -- Reviewed and intentionally left with no console privilege:
  --   schema_migrations          — the migration ledger; only dataminer_migrator.
  --   upload_restart_attempts    — ingestion-owned fence evidence (0012); ditto.
  --   annotator_email_challenges — app-repository.ts only. Operator-initiated
  --     resend/reset deliberately routes through the app pool (docs/34 §4.2), so
  --     the governance role never needs to read or mint a challenge.
  --   annotator_sessions         — app-repository.ts only; same reason.
  --   outbox_jobs                — app INSERTs export jobs, the ingestion
  --     worker drains them. The console neither produces nor claims them.
  --   approved_game_executables  — approved-build allowlist (0018). No
  --     console-api SQL touches it: the console sees its effect through
  --     `clip_trainability` (the `executable_unapproved` blocker), and a view
  --     runs its joins with the VIEW OWNER's privileges, so SELECT on the view
  --     is sufficient — the 2026-07-28 incident shape (bare table → 500) does
  --     not apply here. Approval/revocation run through the CLI
  --     (`pnpm --filter @dataminer/api approve:executable`). Grant it here the
  --     day the console grows its own approval surface.
  reviewed_ungranted CONSTANT text[] := ARRAY[
    'schema_migrations',
    'upload_restart_attempts',
    'annotator_email_challenges',
    'annotator_sessions',
    'outbox_jobs',
    'approved_game_executables',
  --   * depth_consent_policy_versions —— console 不直接读它。它只被 clip_trainability
  --     视图消费，而 console 读的是视图；视图非 security_invoker，按视图属主的权限
  --     求值，所以给 console 表级 SELECT 不会多解锁任何东西，只会多一处要维护的授权面。
  --     哪一版同意书覆盖 depth 是治理动作（docs/105 §3），写入走迁移不走 console。
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
     AND NOT has_table_privilege('dataminer_console', c.oid, 'SELECT')
     AND NOT has_table_privilege('dataminer_console', c.oid, 'INSERT')
     AND NOT has_table_privilege('dataminer_console', c.oid, 'UPDATE')
     AND NOT has_table_privilege('dataminer_console', c.oid, 'DELETE');
  IF bare IS NOT NULL THEN
    RAISE EXCEPTION
      'these public tables have no dataminer_console privilege and are not in reviewed_ungranted: %. '
      'A migration added a table and this file was not updated — grant it above, or add it to '
      'reviewed_ungranted with a reason.', bare;
  END IF;
END $$;

-- (2) Session 根快照是摄取侧事实，Console 只能读取。这个路由在 2026-08-27
-- 上线时，browse-repository.ts 已直接 SELECT session_artifacts，但本文件仍把表列在
-- reviewed_ungranted，导致详情页稳定 500 / PostgreSQL 42501。钉住精确权限，既防漏
-- SELECT，也防为了修 500 顺手给写权限。
DO $$
BEGIN
  IF NOT has_table_privilege('dataminer_console', 'session_artifacts', 'SELECT') THEN
    RAISE EXCEPTION
      'dataminer_console needs SELECT ON session_artifacts for the session root snapshot panel';
  END IF;
  IF has_table_privilege('dataminer_console', 'session_artifacts', 'INSERT')
     OR has_table_privilege('dataminer_console', 'session_artifacts', 'UPDATE')
     OR has_table_privilege('dataminer_console', 'session_artifacts', 'DELETE') THEN
    RAISE EXCEPTION
      'dataminer_console must hold SELECT only on ingestion-owned session_artifacts';
  END IF;
END $$;

-- (3) Invite administration must remain writable by the governance role. The
-- generic completeness check above only proves that a table has *some*
-- privilege; SELECT alone passed it while POST /console/v1/invites failed 42501
-- in production on 2026-08-03. Assert the exact route-level capability here.
DO $$
BEGIN
  IF NOT has_table_privilege('dataminer_console', 'invite_codes', 'SELECT')
     OR NOT has_table_privilege('dataminer_console', 'invite_codes', 'INSERT')
     OR NOT has_table_privilege('dataminer_console', 'invite_codes', 'UPDATE') THEN
    RAISE EXCEPTION
      'dataminer_console needs SELECT, INSERT and UPDATE on invite_codes for list, issue and revoke';
  END IF;
  IF has_table_privilege('dataminer_console', 'invite_codes', 'DELETE') THEN
    RAISE EXCEPTION 'dataminer_console must not delete invite history';
  END IF;
END $$;

-- (4) The §4.2 headline: the annotator surface must not reach console identity,
-- and must not delete anything anywhere.
DO $$
DECLARE
  bad text;
BEGIN
  SELECT string_agg(c.relname, ', ' ORDER BY c.relname) INTO bad
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'public'
     AND c.relkind = 'r'
     AND c.relname IN ('operators', 'operator_sessions', 'operator_email_challenges')
     AND (has_table_privilege('dataminer_app', c.oid, 'SELECT')
       OR has_table_privilege('dataminer_app', c.oid, 'INSERT')
       OR has_table_privilege('dataminer_app', c.oid, 'UPDATE')
       OR has_table_privilege('dataminer_app', c.oid, 'DELETE'));
  IF bad IS NOT NULL THEN
    RAISE EXCEPTION 'dataminer_app must hold zero privilege on operator tables, found on: %', bad;
  END IF;

  SELECT string_agg(c.relname, ', ' ORDER BY c.relname) INTO bad
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'public'
     AND c.relkind = 'r'
     AND has_table_privilege('dataminer_app', c.oid, 'DELETE');
  IF bad IS NOT NULL THEN
    RAISE EXCEPTION 'dataminer_app must hold no DELETE anywhere, found on: %', bad;
  END IF;
END $$;

-- (5) 绑定前 catalog 启动的权限边界（issue #323）。这条通道让一个**还没有设备
-- 凭据**的调用方读到已发布的目录信封，所以它能读多少必须钉死：只读当前 release，
-- 不能写。发布仍然只属于 publisher 角色。
DO $$
BEGIN
  IF NOT has_table_privilege('dataminer_app', 'catalog_releases', 'SELECT') THEN
    RAISE EXCEPTION
      'dataminer_app needs SELECT ON catalog_releases, otherwise GET /app/v1/catalog/bootstrap '
      'fails and a fresh machine can never enroll (issue #323)';
  END IF;
  IF has_table_privilege('dataminer_app', 'catalog_releases', 'INSERT')
     OR has_table_privilege('dataminer_app', 'catalog_releases', 'UPDATE') THEN
    RAISE EXCEPTION
      'dataminer_app must not be able to publish or rewrite a catalog release; that belongs to '
      'dataminer_catalog_publisher alone';
  END IF;
END $$;

COMMIT;
