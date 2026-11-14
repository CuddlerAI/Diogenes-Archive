BEGIN;

-- ── issue #855 PR 3：clip_trainability 认得中性摄取车道的音频 ─────────────────
--
-- 本迁移只改**一处**：期望 artifact 件数里的音频那一项。
--
-- 现状：件数 = 5 + （session schema 是 `dm-local-dev-session-3` 且这条 clip 的
-- audio state 不是 absent 时 +1） + （depth 存在时 +1）。音频那一项按 **session
-- schema 世代**认定，因为 legacy v1 用两代 schema（`-2` / `-3`）表达"这一场有没有
-- 音频"。
--
-- 中性摄取契约只有 `dm-session-ingest-2` 一个 session schema（issue #855 §B 明确它
-- 是唯一的路由键），所以"有没有音频"在 v2 上是 **clip 的 artifact 事实**。不改这
-- 一项的话，一条带音频的 v2 clip 会有 6 个对象而视图期望 5 个，于是永远挂在
-- `artifact_set_incomplete` 上——一个只会向"更不可训练"方向错的 bug，但它会把 30 款
-- 游戏里配了音频的那 25 款永久钉死，而且症状指不到真正的原因。
--
-- 判据形状逐字照抄同一张表里的 depth（0030）：`meta_json -> 'audio'` 存在且 state
-- 不是 absent。对 legacy 两代**逐值等价**：`-2` 不在新词表里，`-3` 必带 audio 元数据
-- 所以新增的 IS NOT NULL 恒真。也就是说这次重贴对存量行的判定是**零变化**。
--
-- 为什么这次是重贴而不是断言（与 0036 的取法相反）：0036 改的是枚举 label，视图的
-- 存储解析树按 OID 引用，传播是自动的，重贴只会把视图钉回那一版；这次改的是判定
-- 本身，除了改写没有别的办法。重贴的风险——"把视图钉回本迁移写下的这一版"——在
-- 这里正是想要的：本迁移就是最新的那一版。
--
-- 明确**不做**：不 UPDATE 任何分类、不动 `approved_for_trainable`、不改任何 blocker
-- 的判定、不放宽任何闸。`training_candidate` 在审批清零前照旧被这张视图挡住。

CREATE OR REPLACE VIEW clip_trainability AS
WITH artifact_rollup AS (
    SELECT
        clip_id,
        count(*) AS object_count,
        bool_or(kind = 'depth') AS has_depth_object,
        bool_or(kind = 'audio') AS has_audio_object,
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
            CASE WHEN s.server_data_classification <> 'training_candidate' THEN 'source_non_trainable' END,
            CASE WHEN s.annotator_id IS NULL THEN 'missing_annotator' END,
            CASE WHEN a.id IS NULL OR a.status <> 'active' THEN 'annotator_inactive' END,
            CASE WHEN con.id IS NULL THEN 'missing_consent' END,
            -- 原判据是 `status <> 'active'`，这里只挖掉**一种**离开 active 的方式：
            -- 换版（supersede）。那是同一个人继续授权、只是换了文书，由下面的时间窗判；
            -- 把它算成失效正是 docs/108 §1.3 缺陷 C 的成因——补签者的历史数据被判成
            -- 「授权已撤回」。其余任何非 active 状态照旧挡。
            --
            -- 撤回（revoked_at）则一票否决且**追溯**，连被换版过的历史行也否决：条款
            -- 承诺撤回会清除已采集的数据，所以撤回之前采的也不例外。
            CASE
                WHEN con.id IS NOT NULL
                    AND (
                        con.revoked_at IS NOT NULL
                        OR (con.status <> 'active' AND con.superseded_at IS NULL)
                    )
                THEN 'consent_inactive'
            END,
            CASE WHEN con.id IS NOT NULL AND NOT con.allows_trainable_use THEN 'consent_disallows_trainable' END,
            CASE WHEN con.expires_at IS NOT NULL AND con.expires_at <= now() THEN 'consent_expired' END,
            CASE WHEN con.id IS NOT NULL AND con.retention_state <> 'retained' THEN 'consent_purge_active' END,
            -- 采集时点必须落在这份 consent 自己的有效窗口内。
            --
            -- 这一条替掉的是 capture_grant_inactive + capture_outside_grant_window：
            -- 同样是「授权在采集发生时有没有生效」，只是主体从 grant 换成 consent。
            -- 上界用 superseded_at 而不是 status：一份被换版的 consent 对**它在任时**
            -- 采的数据依然有效，把它判成失效正是 docs/108 §1.3 缺陷 C 的成因。
            CASE
                WHEN con.id IS NOT NULL
                    AND (
                        con.accepted_at IS NULL
                        OR s.started_at < con.accepted_at
                        OR (con.superseded_at IS NOT NULL AND s.started_at >= con.superseded_at)
                    )
                THEN 'consent_window_mismatch'
            END,
            -- 这次采集用到的模态，必须都在这份 consent 声明的范围里。
            --
            -- 替掉 capture_grant_scope_mismatch。旧那条比的是「grant 绑的游戏/profile
            -- 是不是这一场」——一个逐游戏模型才需要问的问题；同意书从头到尾没有按游戏
            -- 写过（全文没有一个游戏名）。新这条问的是同意书真正回答得了的问题：
            -- 你采了音频/深度，那份文书写了吗。
            --
            -- 必需模态 input/video 一并要求：一份连采集本身都没写的文书（scopes 为空，
            -- 即 0031 回填时哈希对不上任何已知正文）不该让数据可训练。
            --
            -- 存在性看**真实 artifact 行**而不是 meta_json 声明：漏写声明就能绕过的闸
            -- 不是闸（同 0030 对 depth 的取法）。
            CASE
                WHEN con.id IS NOT NULL
                    AND NOT (
                        con.scopes @> (
                            ARRAY['input', 'video']
                            || CASE WHEN COALESCE(ar.has_audio_object, false) THEN ARRAY['audio'] ELSE ARRAY[]::text[] END
                            || CASE WHEN COALESCE(ar.has_depth_object, false) THEN ARRAY['depth'] ELSE ARRAY[]::text[] END
                        )::text[]
                    )
                THEN 'consent_scope_insufficient'
            END,
            -- depth 的准入判据仍然只有这一处（0030，docs/105）。上面的 scope 判据与它
            -- 口径不同——那张表按 policy_version 认定、因此把就地修订前签的行也算作覆盖
            -- depth，scopes 按 terms_hash 认定、因此不算。两条都留着是**刻意**的：任一
            -- 条不满足即挡，方向都是 fail-closed。收敛成一处是治理裁决，不是迁移能替谁做的
            -- 决定（docs/108 §3.2 末、§6）。
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
                            WHEN s.schema_version IN ('dm-local-dev-session-3', 'dm-session-ingest-2')
                                 AND (c.meta_json -> 'audio') IS NOT NULL
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
    'Read-only, fail-closed server derivation. Authorization is the annotator''s own consent, valid for the moment capture started and covering every modality the clip actually carries; the per-game capture_grants lane is no longer consulted (docs/108 D1). Expected artifact count is 5, plus one for a clip that actually carries audio (session v3 on the legacy lane, dm-session-ingest-2 on the neutral ingest lane), plus one for depth-present clips. Depth additionally requires a policy version listed in depth_consent_policy_versions. Catalog, executable, view-scale, QA, and retention gates are unchanged.';

-- 视图重贴之后再断言一次两代摄取车道都被认到了：断言保证的是"库里现在这份是对的"。
DO $$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_viewdef('clip_trainability'::regclass) INTO definition;
    IF definition LIKE '%production_candidate%' THEN
        RAISE EXCEPTION 'migration 0037: clip_trainability still references the pre-0036 classification label';
    END IF;
    IF definition NOT LIKE '%dm-session-ingest-2%' THEN
        RAISE EXCEPTION 'migration 0037: clip_trainability does not recognise the neutral ingest session contract';
    END IF;
    IF definition NOT LIKE '%dm-local-dev-session-3%' THEN
        RAISE EXCEPTION 'migration 0037: clip_trainability lost the legacy audio generation';
    END IF;
END $$;

INSERT INTO schema_migrations(version, name)
VALUES (37, 'ingest_v2_audio_artifact_count');

COMMIT;
