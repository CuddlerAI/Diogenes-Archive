-- 0018 · 游戏程序指纹改为云端事后把关（负责人裁决 2026-07-28）。
--
-- 背景：#278 取消了"authoring 必须钉 executable SHA-256 / 窗口类"的事前门。
-- 指纹证据本身没有消失——客户端每次绑定仍现场全文件哈希（window_tracker
-- `verify`，算的同时锁文件防偷换）并随 session 上报。本迁移把把关时机从
-- 「事前 authoring」挪到「事后治理」：
--
-- 1) `approved_game_executables` = 每游戏的已批准 build 名单。两个来源：
--    `authored`（catalog 里钉了指纹，注册时随治理审批入册）与 `observed`
--    （采集员真机送上门的新指纹，落 pending 等治理点头）；
-- 2) sessions 插入触发器自动登记 observed 指纹——任何写入路径都不会漏记，
--    治理侧因此总能看见"有个没见过的 build 正在被采集"；
-- 3) clip_trainability 追加 `executable_unapproved`：观测指纹不在名单上
--    （或已撤销）就拿不到训练资格。数据照常入库，只是不可训练——与
--    `view_scale_unverified`（0016）同形，fail-closed 由服务端派生。
--
-- 存量影响：迁移把已 approved 的 game_profile 里 authored 的指纹回填为
-- approved（治理已审过的 build 不因本迁移失效），已有 sessions 的观测指纹
-- 回填为 pending（可见但不放行）。当前无 certified/可训练产线（docs/40 全
-- HOLD、certified catalog 为空），因此本门不会打断任何既有可训练数据。
BEGIN;

CREATE TABLE approved_game_executables (
    game_id text NOT NULL REFERENCES games(id) ON DELETE RESTRICT,
    executable_sha256 sha256_digest NOT NULL,
    -- pending: 已观测未审批；approved: 可用于可训练判定；revoked: 显式撤销
    -- （撤销是治理行为，保留行以留审计痕迹，而不是删除）。
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'revoked')),
    source text NOT NULL CHECK (source IN ('authored', 'observed')),
    approved_by text CHECK (approved_by IS NULL OR btrim(approved_by) <> ''),
    approved_at timestamptz,
    note text,
    first_seen_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    PRIMARY KEY (game_id, executable_sha256),
    -- 审批必须可归属：approved 行必须带审批人与时刻。
    CONSTRAINT approved_rows_are_attributable CHECK (
        status <> 'approved' OR (approved_by IS NOT NULL AND approved_at IS NOT NULL)
    )
);

COMMENT ON TABLE approved_game_executables IS
    'Per-game approved build allowlist. Identity pins are optional at authoring time (#278); trainability is gated here instead, on the executable hash the client actually observed and hashed at bind time.';
COMMENT ON COLUMN approved_game_executables.source IS
    'authored = pinned in the registered game profile; observed = first seen in a real capture session and awaiting governance review.';

CREATE INDEX approved_game_executables_pending_idx
    ON approved_game_executables(game_id, first_seen_at DESC)
    WHERE status = 'pending';

CREATE TRIGGER approved_game_executables_set_updated_at
    BEFORE UPDATE ON approved_game_executables
    FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

-- 观测指纹自动登记。放在触发器而不是应用层：任何插入 sessions 的路径
-- （API、回填脚本、将来的迁移）都会登记，且天然幂等——已在名单上的
-- build（无论 pending/approved/revoked）不被本触发器改写，撤销不会被
-- 一次新采集悄悄复活。
--
-- SECURITY DEFINER（先例：0014 的 wipe_annotator_profile_on_tombstone）。
-- 触发器函数默认以**调用者**权限执行，而写入 sessions 的是运行时角色
-- `dataminer_api`；若按默认来，这张治理表就必须对运行时角色开 INSERT，
-- 否则每一次 `POST /v1/sessions` 都会 permission denied。两者都不可接受：
-- 前者让摄取路径能直接写审批名单，后者直接压垮核心路径。DEFINER 让登记
-- 以函数所有者（迁移角色）身份发生，运行时角色因此对本表零权限。
-- 函数体无动态 SQL，值只取自 NEW；`SET search_path` 关掉 search_path 注入面。
CREATE FUNCTION record_observed_game_executable()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
BEGIN
    INSERT INTO approved_game_executables(game_id, executable_sha256, status, source, note)
    VALUES (
        NEW.game_id,
        NEW.game_executable_sha256,
        'pending',
        'observed',
        'first observed in session ' || NEW.id::text
    )
    ON CONFLICT (game_id, executable_sha256) DO NOTHING;
    RETURN NEW;
END;
$$;

CREATE TRIGGER sessions_record_observed_executable
    AFTER INSERT ON sessions
    FOR EACH ROW EXECUTE FUNCTION record_observed_game_executable();

-- 回填 1：已 approved 的 game_profile 中 authored 的指纹 → approved。
-- 治理已经审过这些 build，本迁移不应让它们失效。
-- 同一 build 可能被多个 game_profile 版本钉住（改了 display_name/input_contract
-- 等其他字段）。ORDER BY 让"取哪一次审批"确定且可解释：最早的那次，缺
-- approved_at 的排最后。没有 ORDER BY 的 DISTINCT ON 由执行计划决定取哪行，
-- 会让回填出的 approved_by/approved_at 不可复现。
INSERT INTO approved_game_executables(
    game_id, executable_sha256, status, source, approved_by, approved_at, note
)
SELECT DISTINCT ON (tcv.game_id, tcv.snapshot ->> 'executable_sha256')
    tcv.game_id,
    (tcv.snapshot ->> 'executable_sha256')::sha256_digest,
    'approved',
    'authored',
    COALESCE(tcv.approved_by, 'migration:0018'),
    COALESCE(tcv.approved_at, transaction_timestamp()),
    'backfilled from approved game_profile ' || tcv.catalog_id || '@' || tcv.version
FROM trusted_catalog_versions tcv
WHERE tcv.kind = 'game_profile'
  AND tcv.game_id IS NOT NULL
  AND tcv.status IN ('approved', 'retired')
  AND tcv.snapshot ->> 'executable_sha256' ~ '^sha256:[0-9a-f]{64}$'
ORDER BY tcv.game_id,
         tcv.snapshot ->> 'executable_sha256',
         tcv.approved_at ASC NULLS LAST,
         tcv.created_at ASC
ON CONFLICT (game_id, executable_sha256) DO NOTHING;

-- 回填 2：已有 sessions 的观测指纹 → pending（可见，不放行）。
INSERT INTO approved_game_executables(
    game_id, executable_sha256, status, source, first_seen_at, note
)
SELECT
    s.game_id,
    s.game_executable_sha256,
    'pending',
    'observed',
    min(s.created_at),
    'backfilled from existing sessions'
FROM sessions s
GROUP BY s.game_id, s.game_executable_sha256
ON CONFLICT (game_id, executable_sha256) DO NOTHING;

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
            -- #278 把身份钉从 authoring 硬门降为选填证据；把关改在这里：
            -- 客户端绑定时实测的指纹必须已被治理批准。
            CASE
                WHEN age.game_id IS NULL THEN 'executable_unapproved'
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
    'Read-only, fail-closed server derivation. Five verified artifacts, consent, grant, catalog, approved executable build, view-scale calibration, QA, and retention gates must all pass.';

INSERT INTO schema_migrations(version, name)
VALUES (18, 'executable_approval');

COMMIT;
