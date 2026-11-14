-- 0016 · 视角标定进入可训练判定（docs/45 §6.3、docs/46 §5.2 / C6）。
--
-- 1) session artifact 词表加第 9 种（可选）`input_calibration`；
-- 2) sessions 增标定声明列。语义是 evidence-only：客户端只交证据，
--    分级仍由服务端行派生（0001 的既有纪律）。本列参与的是"存在性与
--    自洽门"——声明缺失/自报 fail 必拦；谎报 pass 由光流 QA 与
--    quality_gate_not_passed 捕获（docs/46 §5.2 分层语义）；
-- 3) clip_trainability 自 0001 以来首次修改：追加 `view_scale_unverified`。
--    只有 settings snapshot 显式声明 `view_scale.applicable = true` 的
--    目录才受此门约束——存量目录与不适用（指针型）目录零漂移。
BEGIN;

ALTER TYPE session_artifact_kind ADD VALUE IF NOT EXISTS 'input_calibration';

ALTER TABLE sessions
    ADD COLUMN view_scale_sha256 TEXT,
    ADD COLUMN view_scale_verdict TEXT
        CHECK (view_scale_verdict IN ('pass', 'fail') OR view_scale_verdict IS NULL),
    ADD COLUMN view_scale_deviation_pct DOUBLE PRECISION,
    ADD COLUMN view_scale_polling_hz DOUBLE PRECISION;

COMMENT ON COLUMN sessions.view_scale_sha256 IS
    'Client-declared view-scale calibration evidence digest (dm-input-calibration-1). Evidence only: gates key on presence and self-consistency, never on client honesty alone.';

CREATE OR REPLACE VIEW clip_trainability AS
WITH artifact_rollup AS (
    SELECT
        clip_id,
        count(*) AS object_count,
        bool_and(storage_state = 'stored') AS all_stored,
        bool_and(verification_state = 'verified') AS all_verified
    FROM artifact_objects
    GROUP BY clip_id
),
facts AS (
    SELECT
        c.id AS clip_id,
        s.id AS session_id,
        ARRAY_REMOVE(ARRAY[
            CASE WHEN s.server_data_classification <> 'production_candidate' THEN 'source_non_trainable' END,
            CASE WHEN s.annotator_id IS NULL THEN 'missing_annotator' END,
            CASE WHEN a.id IS NULL OR a.status <> 'active' THEN 'annotator_inactive' END,
            CASE WHEN con.id IS NULL THEN 'missing_consent' END,
            CASE WHEN con.id IS NOT NULL AND con.status <> 'active' THEN 'consent_inactive' END,
            CASE WHEN con.id IS NOT NULL AND NOT con.allows_trainable_use THEN 'consent_disallows_trainable' END,
            CASE WHEN con.expires_at IS NOT NULL AND con.expires_at <= now() THEN 'consent_expired' END,
            CASE WHEN con.id IS NOT NULL AND con.retention_state <> 'retained' THEN 'consent_purge_active' END,
            CASE WHEN g.id IS NULL THEN 'missing_capture_grant' END,
            CASE WHEN g.id IS NOT NULL AND g.status <> 'active' THEN 'capture_grant_inactive' END,
            CASE WHEN g.id IS NOT NULL AND NOT g.allows_trainable THEN 'capture_grant_disallows_trainable' END,
            CASE
                WHEN g.id IS NOT NULL
                    AND NOT (s.started_at >= g.valid_from AND s.started_at < g.valid_until)
                THEN 'capture_outside_grant_window'
            END,
            CASE
                WHEN g.id IS NOT NULL
                    AND (g.game_id <> s.game_id OR g.capture_profile_version_id <> s.capture_catalog_version_id)
                THEN 'capture_grant_scope_mismatch'
            END,
            CASE WHEN d.status <> 'active' THEN 'device_inactive' END,
            CASE WHEN NOT game.allowed_for_capture OR NOT game.approved_for_trainable THEN 'game_unapproved' END,
            CASE
                WHEN dv.status NOT IN ('approved', 'retired') OR NOT dv.approved_for_trainable
                THEN 'device_profile_unapproved'
            END,
            CASE
                WHEN cp.status NOT IN ('approved', 'retired') OR NOT cp.approved_for_trainable
                THEN 'capture_profile_unapproved'
            END,
            CASE
                WHEN sp.status NOT IN ('approved', 'retired') OR NOT sp.approved_for_trainable
                THEN 'settings_profile_unapproved'
            END,
            CASE
                WHEN mp.status NOT IN ('approved', 'retired') OR NOT mp.approved_for_trainable
                THEN 'mapping_unapproved'
            END,
            CASE
                WHEN COALESCE((sp.snapshot -> 'view_scale' ->> 'applicable')::boolean, false)
                    AND (s.view_scale_verdict IS DISTINCT FROM 'pass' OR s.view_scale_sha256 IS NULL)
                THEN 'view_scale_unverified'
            END,
            CASE WHEN c.ingest_state <> 'stored' THEN 'clip_not_stored' END,
            CASE WHEN c.verification_state <> 'verified' THEN 'clip_not_verified' END,
            CASE WHEN c.quality_state <> 'passed' THEN 'quality_gate_not_passed' END,
            CASE WHEN c.retention_state <> 'retained' THEN 'clip_purge_active' END,
            CASE WHEN c.retention_deadline <= now() THEN 'clip_retention_expired' END,
            CASE WHEN COALESCE(ar.object_count, 0) <> 5 THEN 'artifact_set_incomplete' END,
            CASE WHEN NOT COALESCE(ar.all_stored, false) THEN 'artifact_not_stored' END,
            CASE WHEN NOT COALESCE(ar.all_verified, false) THEN 'artifact_not_verified' END
        ], NULL)::text[] AS blockers
    FROM clips c
    JOIN sessions s ON s.id = c.session_id
    JOIN devices d ON d.id = s.device_id
    JOIN games game ON game.id = s.game_id
    JOIN trusted_catalog_versions dv ON dv.id = s.device_catalog_version_id
    JOIN trusted_catalog_versions cp ON cp.id = s.capture_catalog_version_id
    JOIN trusted_catalog_versions sp ON sp.id = s.settings_catalog_version_id
    JOIN trusted_catalog_versions mp ON mp.id = s.mapping_catalog_version_id
    LEFT JOIN annotators a ON a.id = s.annotator_id
    LEFT JOIN consents con ON con.id = s.consent_id
    LEFT JOIN capture_grants g ON g.id = s.capture_grant_id
    LEFT JOIN artifact_rollup ar ON ar.clip_id = c.id
)
SELECT
    clip_id,
    session_id,
    cardinality(blockers) = 0 AS is_trainable,
    blockers
FROM facts;

COMMENT ON VIEW clip_trainability IS
    'Read-only, fail-closed server derivation. Five verified artifacts, consent, grant, catalog, view-scale calibration, QA, and retention gates must all pass.';

INSERT INTO schema_migrations(version, name)
VALUES (16, 'view_scale_trainability');

COMMIT;
