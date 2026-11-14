BEGIN;

-- ── certified 会话准入（docs/63 §5.1，裁决 A 之后）────────────────────────
--
-- 0022 §4 留过一句待办：「等 certified 录制真正接上 P5，再加家族与配对」。
-- 这一天到了——capture grant 链已经落地（POST /v1/capture-grants + 客户端
-- provider + 预检终态 `certified_pass`），certified 会话从「根本插不进库」
-- 变成一条真实车道。本迁移做三件事：
--
--   1. `sessions` 的 schema_version 词表加 `dm-certified-session-2/3`，并把
--      schema_version ↔ catalog_mode 收成真配对（dev 词表只配 dev mode，
--      certified 词表只配 certified mode）——0022 曾允许交叉，那是 certified
--      产不出自己证据时的过渡态。
--   2. `capture_runs_preflight_reference_check` 词表放宽到两条车道的通过值。
--   3. `enforce_capture_run_preflight_contract()` 恢复 mode↔family 真配对。
--      权威映射是客户端 `preflight::pass_result_for(mode)`：
--      `dev_non_trainable`↔`dev_non_trainable_pass`、`certified`↔`certified_pass`。
--      ⚠ 本函数体基于 **0023** 的版本改（0023 用 CREATE OR REPLACE 重定义过
--      它，并把 sessions 词表扩到了 v3），不是 0022 的。
--
-- 另外重建 `clip_trainability`：0023 的六件套判定按 schema_version =
-- 'dm-local-dev-session-3' 认音频代，certified 的音频代（dm-certified-session-3）
-- 必须走同一条判定，否则 certified 音频 clip 恒挂 artifact_set_incomplete。

-- 收真配对之前先点名存量（照 0022 的先例）：交叉配对的行若真存在，下面的
-- CHECK/语义会以一句语焉不详的约束冲突失败。宁可在这里显式停住，把「有多少
-- 行、该怎么处置」交回给人——那是数据决定，不该由一条迁移替谁做主。
DO $$
DECLARE
    cross_paired_sessions bigint;
    cross_paired_runs bigint;
BEGIN
    SELECT count(*) INTO cross_paired_sessions
      FROM sessions
     WHERE (schema_version IN ('dm-local-dev-session-2', 'dm-local-dev-session-3')
            AND catalog_mode IS DISTINCT FROM 'dev_non_trainable')
        OR (schema_version IN ('dm-certified-session-2', 'dm-certified-session-3')
            AND catalog_mode IS DISTINCT FROM 'certified');
    SELECT count(*) INTO cross_paired_runs
      FROM capture_runs cr
      JOIN sessions s ON s.id = cr.session_id
     WHERE (s.catalog_mode = 'certified' AND cr.preflight_result = 'dev_non_trainable_pass')
        OR (s.catalog_mode = 'dev_non_trainable' AND cr.preflight_result = 'certified_pass');
    IF cross_paired_sessions > 0 OR cross_paired_runs > 0 THEN
        RAISE EXCEPTION
            'migration 0024 refuses to enforce the mode-family pairing while cross-paired rows exist: % session(s), % capture run(s). These predate the certified lane (0022 §4 transitional state); decide explicitly whether they are archived, reclassified, or deleted before re-running.',
            cross_paired_sessions, cross_paired_runs;
    END IF;
END $$;

ALTER TABLE sessions
    DROP CONSTRAINT sessions_local_catalog_evidence_check;
ALTER TABLE sessions
    ADD CONSTRAINT sessions_local_catalog_evidence_check CHECK (
        (
            schema_version = 'dm-local-dev-session-1'
            AND catalog_id IS NULL
            AND catalog_version IS NULL
            AND catalog_mode IS NULL
            AND catalog_manifest_sha256 IS NULL
            AND game_profile_sha256 IS NULL
        )
        OR (
            schema_version IN ('dm-local-dev-session-2', 'dm-local-dev-session-3')
            -- IS NOT NULL 不可省：catalog_mode 可空（0004），裸等号在 NULL 上算出
            -- NULL，而 CHECK 对 NULL 放行——历代约束（0004/0009/0022/0023）都带
            -- 这个连接词，这里收真配对不能反而放宽 NULL。
            AND catalog_mode IS NOT NULL
            AND catalog_mode = 'dev_non_trainable'
            AND catalog_id IS NOT NULL
            AND btrim(catalog_id) <> ''
            AND catalog_version IS NOT NULL
            AND btrim(catalog_version) <> ''
            AND catalog_manifest_sha256 IS NOT NULL
            AND game_profile_sha256 IS NOT NULL
        )
        OR (
            schema_version IN ('dm-certified-session-2', 'dm-certified-session-3')
            AND catalog_mode IS NOT NULL
            AND catalog_mode = 'certified'
            AND catalog_id IS NOT NULL
            AND btrim(catalog_id) <> ''
            AND catalog_version IS NOT NULL
            AND btrim(catalog_version) <> ''
            AND catalog_manifest_sha256 IS NOT NULL
            AND game_profile_sha256 IS NOT NULL
        )
    );

ALTER TABLE capture_runs
    DROP CONSTRAINT capture_runs_preflight_reference_check;
ALTER TABLE capture_runs
    ADD CONSTRAINT capture_runs_preflight_reference_check CHECK (
        (
            preflight_schema_version IS NULL
            AND preflight_id IS NULL
            AND preflight_result IS NULL
            AND preflight_evidence_sha256 IS NULL
        )
        OR (
            preflight_schema_version IS NOT NULL
            AND preflight_schema_version = 'dm-preflight-evidence-1'
            AND preflight_id IS NOT NULL
            AND btrim(preflight_id) <> ''
            AND preflight_result IS NOT NULL
            AND preflight_result IN ('dev_non_trainable_pass', 'certified_pass')
            AND preflight_evidence_sha256 IS NOT NULL
        )
    );

-- 基于 0023 的函数版本。变化只有一处：versioned 分支不再把 preflight_result
-- 钉死为 dev 的通过值，而是按父会话的 catalog_mode 要求**它自己车道**的通过值。
CREATE OR REPLACE FUNCTION enforce_capture_run_preflight_contract()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_schema_version text;
    parent_catalog_mode text;
    required_result text;
BEGIN
    SELECT schema_version, catalog_mode
      INTO parent_schema_version, parent_catalog_mode
      FROM sessions
     WHERE id = NEW.session_id;

    IF parent_schema_version IN ('dm-local-dev-session-2', 'dm-local-dev-session-3',
                                 'dm-certified-session-2', 'dm-certified-session-3') THEN
        -- 权威映射是客户端的 preflight::pass_result_for(mode)，逐字照抄。
        IF parent_catalog_mode = 'dev_non_trainable' THEN
            required_result := 'dev_non_trainable_pass';
        ELSIF parent_catalog_mode = 'certified' THEN
            required_result := 'certified_pass';
        ELSE
            RAISE EXCEPTION 'versioned capture run has an unsupported parent catalog mode'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.preflight_schema_version IS DISTINCT FROM 'dm-preflight-evidence-1'
           OR NEW.preflight_id IS NULL
           OR btrim(NEW.preflight_id) = ''
           OR NEW.preflight_result IS DISTINCT FROM required_result
           OR NEW.preflight_evidence_sha256 IS NULL THEN
            RAISE EXCEPTION 'versioned capture run requires the preflight evidence of its own lane'
                USING ERRCODE = '23514';
        END IF;
    ELSIF parent_schema_version = 'dm-local-dev-session-1' THEN
        IF NEW.preflight_schema_version IS NOT NULL
           OR NEW.preflight_id IS NOT NULL
           OR NEW.preflight_result IS NOT NULL
           OR NEW.preflight_evidence_sha256 IS NOT NULL THEN
            RAISE EXCEPTION 'legacy session v1 capture run cannot carry versioned preflight evidence'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported parent session schema version'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

-- 0023 的视图按 dm-local-dev-session-3 认音频代。certified 的音频代必须走同
-- 一条六件套判定；其余逐字不动。
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
                    AND (g.game_id <> s.game_id
                         OR g.recording_profile_version_id <> s.recording_catalog_version_id)
                THEN 'capture_grant_scope_mismatch'
            END,
            CASE WHEN d.status <> 'active' THEN 'device_inactive' END,
            CASE WHEN NOT game.allowed_for_capture OR NOT game.approved_for_trainable THEN 'game_unapproved' END,
            CASE
                WHEN dv.status NOT IN ('approved', 'retired') OR NOT dv.approved_for_trainable
                THEN 'device_profile_unapproved'
            END,
            CASE
                WHEN rp.status NOT IN ('approved', 'retired') OR NOT rp.approved_for_trainable
                THEN 'recording_profile_unapproved'
            END,
            CASE
                WHEN mp.status NOT IN ('approved', 'retired') OR NOT mp.approved_for_trainable
                THEN 'mapping_unapproved'
            END,
            CASE WHEN age.game_id IS NULL THEN 'executable_unapproved' END,
            CASE
                WHEN COALESCE((rp.snapshot -> 'view_scale' ->> 'applicable')::boolean, false)
                    AND (s.view_scale_verdict IS DISTINCT FROM 'pass' OR s.view_scale_sha256 IS NULL)
                THEN 'view_scale_unverified'
            END,
            CASE WHEN c.ingest_state <> 'stored' THEN 'clip_not_stored' END,
            CASE WHEN c.verification_state <> 'verified' THEN 'clip_not_verified' END,
            CASE WHEN c.quality_state <> 'passed' THEN 'quality_gate_not_passed' END,
            CASE WHEN c.retention_state <> 'retained' THEN 'clip_purge_active' END,
            CASE WHEN c.retention_deadline <= now() THEN 'clip_retention_expired' END,
            CASE
                WHEN COALESCE(ar.object_count, 0)
                    <> CASE
                        WHEN s.schema_version IN ('dm-local-dev-session-3', 'dm-certified-session-3')
                             AND (c.meta_json -> 'audio' ->> 'state') IS DISTINCT FROM 'absent'
                        THEN 6
                        ELSE 5
                    END
                THEN 'artifact_set_incomplete'
            END,
            CASE WHEN NOT COALESCE(ar.all_stored, false) THEN 'artifact_not_stored' END,
            CASE WHEN NOT COALESCE(ar.all_verified, false) THEN 'artifact_not_verified' END
        ], NULL)::text[] AS blockers
    FROM clips c
    JOIN sessions s ON s.id = c.session_id
    JOIN devices d ON d.id = s.device_id
    JOIN games game ON game.id = s.game_id
    JOIN trusted_catalog_versions dv ON dv.id = s.device_catalog_version_id
    JOIN trusted_catalog_versions rp ON rp.id = s.recording_catalog_version_id
    JOIN trusted_catalog_versions mp ON mp.id = s.mapping_catalog_version_id
    LEFT JOIN annotators a ON a.id = s.annotator_id
    LEFT JOIN consents con ON con.id = s.consent_id
    LEFT JOIN capture_grants g ON g.id = s.capture_grant_id
    LEFT JOIN artifact_rollup ar ON ar.clip_id = c.id
    LEFT JOIN approved_game_executables age
        ON age.game_id = s.game_id
       AND age.executable_sha256 = s.game_executable_sha256
       AND age.status = 'approved'
)
SELECT
    clip_id,
    session_id,
    cardinality(blockers) = 0 AS is_trainable,
    blockers
FROM facts;

COMMENT ON VIEW clip_trainability IS
    'Read-only, fail-closed server derivation. Audio-present session v3 (dev or certified generation) requires six verified artifacts; audio-absent v3 and historical sessions require five. Consent, grant, catalog, executable, view-scale, QA, and retention gates must also pass.';

INSERT INTO schema_migrations(version, name)
VALUES (24, 'certified_session_admission');

COMMIT;
