BEGIN;

-- depth 进摄取面（裁决见 docs/105）。
--
-- 这条迁移**推翻** docs/77 §7.1 的「独立桶 + 凭据级不可达」裁决。那条裁决给了四条
-- 理由，进摄取面之后它们的落点变了，逐条记在这里，因为迁移是最后一道能读到它们的地方：
--
--   ① 前缀隔离只防配置写错，防不住同凭据进程 List 整桶
--      → 不再适用：depth 现在**就是**摄取面的产物，共用凭据是设计而非事故。
--   ② 本数据要求凭据级不可达：注入来源、永久 non-trainable、现行同意书不覆盖
--      → 「同意书不覆盖」由 2026-07-25-teaser 的**就地修订**解决（clause 3 如实
--        披露注入行为，法务已通过）。「永久 non-trainable」**不再成立**——同意书授权之后，depth
--        与 audio 同级，由 clip_trainability 的既有闸统一判定，不再单独钉死。
--        ⚠ 但只有在**该 clip 的同意书版本确实覆盖 depth** 时才成立，见下面的 blocker。
--   ③ fail-closed 先例 → 保留：见 depth_consent_not_covered。
--   ④ 生命周期不同（实验数据该到期就删）→ depth 现在跟随 clip 的 retention_deadline，
--      与 video/audio 同寿命。这是**行为变化**，不是等价搬迁。
--
-- 第七件产物。历史 clip 与未开深度采集的 clip 仍是合法的五件/六件套；
-- 条件关系由 contracts/repository 与下面重建的视图共同约束。
ALTER TYPE artifact_kind ADD VALUE IF NOT EXISTS 'depth';

COMMIT;

BEGIN;

-- 哪些同意书版本覆盖了 depth 采集。做成表而不是视图里的字面量：新出一版同意书
-- INSERT 一行即可，不必改视图；也让「某一版到底覆不覆盖 depth」是一条**可查、
-- 可审计的数据**，而不是散在 SQL 里的字符串。
--
-- 空表 = 没有任何同意书覆盖 depth = 所有带 depth 的 clip 一律不可训练。
-- 这是刻意的初始态：先有同意书，才有可训练的 depth。
CREATE TABLE depth_consent_policy_versions (
    policy_version text PRIMARY KEY CHECK (btrim(policy_version) <> ''),
    note text NOT NULL CHECK (btrim(note) <> ''),
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

COMMENT ON TABLE depth_consent_policy_versions IS
    'Consent policy versions whose text authorizes depth capture and in-process instrumentation. clip_trainability treats depth bytes under any other policy version as depth_consent_not_covered. Adding a row is a governance act: it asserts that the named terms text actually discloses depth capture.';

-- 2026-07-25-teaser 的正文已就地修订（2026-08-07，法务通过），clause 3 明确披露了
-- 注入式深度采集。
--
-- ⚠ 就地修订而非新出一版，是一次**明确的裁决**（docs/105 §4）：采集员均为内部员工，
-- 由用户线下逐一告知。代价必须写在这里，因为这一行是它生效的地方——
-- **现存签署全部是这一版，因此它们会立刻被判为覆盖 depth，包括在正文尚未提及
-- depth 之前采集的数据**。签署行里存的 terms_hash 是旧正文的，不再能复现当前文本；
-- 「他们同意了什么」的凭据是线下告知，不是这些签署行。
INSERT INTO depth_consent_policy_versions(policy_version, note) VALUES
    ('2026-07-25-teaser',
     'amended in place 2026-08-07 (legal-approved) to disclose in-process depth instrumentation; annotators are internal staff notified out of band — see docs/105 §4');

-- 视图上次重建在 0024（0028/0029 都没碰它）。这里在 0024 的基础上加两件事：
--   1. 产物件数期望把 depth 算进去；
--   2. 新 blocker `depth_consent_not_covered`——带 depth 的 clip，其 session 绑定的
--      同意书必须是明确覆盖 depth 的版本，否则不可训练。
--
-- 为什么件数看 meta_json、blocker 看真实对象：
--   件数期望若也去数真实对象，那这条检查就恒真、等于没有；
--   而 blocker 若只看 meta_json 声明，漏写声明就能绕过——所以它看**字节是否真的存在**。
--   两个方向刻意不同，都是 fail-closed 的那一侧。
CREATE OR REPLACE VIEW clip_trainability AS
WITH artifact_rollup AS (
    SELECT
        clip_id,
        count(*) AS object_count,
        bool_or(kind = 'depth') AS has_depth_object,
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
            -- depth 的字节一旦存在，同意书就必须是覆盖 depth 的那一代。
            -- 判据放在 `depth_consent_policy_versions` 表里而不是写死字面量：新版同意书
            -- 出一版就 INSERT 一行，不必改视图。表为空 = 没有任何同意书覆盖 depth =
            -- 所有带 depth 的 clip 一律不可训练，这正是 fail-closed 的初始态。
            CASE
                WHEN COALESCE(ar.has_depth_object, false)
                    AND (
                        con.id IS NULL
                        OR NOT EXISTS (
                            SELECT 1 FROM depth_consent_policy_versions dcpv
                             WHERE dcpv.policy_version = con.policy_version
                        )
                    )
                THEN 'depth_consent_not_covered'
            END,
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
                    <> (
                        5
                        + CASE
                            WHEN s.schema_version IN ('dm-local-dev-session-3')
                                 AND (c.meta_json -> 'audio' ->> 'state') IS DISTINCT FROM 'absent'
                            THEN 1 ELSE 0
                        END
                        + CASE
                            WHEN (c.meta_json -> 'depth' ->> 'state') IS DISTINCT FROM 'absent'
                                 AND (c.meta_json -> 'depth') IS NOT NULL
                            THEN 1 ELSE 0
                        END
                    )
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
    'Read-only, fail-closed server derivation. Expected artifact count is 5, plus one for audio-present session v3, plus one for depth-present clips. A clip carrying depth bytes is trainable only under a consent policy version listed in depth_consent_policy_versions. Consent, grant, catalog, executable, view-scale, QA, and retention gates must also pass.';

INSERT INTO schema_migrations(version, name)
VALUES (30, 'depth_artifact');

COMMIT;
