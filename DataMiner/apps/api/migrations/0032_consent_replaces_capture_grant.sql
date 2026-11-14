-- 0032 · 授权判据从「逐游戏 grant」收回到「采集员的 consent」。
--
-- 依据 docs/108 §2 D1（为什么收）、§3.5（判据怎么换）。
--
-- 现状：`clip_trainability` 有五条 grant 阻断，而 `capture_grants` 自 docs/100 起
-- **没有任何写入方**（签发接口已删，console 只剩只读历史表）。也就是说这五条恒亮，
-- 且更要命的是——`sessions.consent_id` 的唯一来源就是 grant（repository.ts:425 的注释
-- 把这条写死了），grant 没了，于是每一场采集连「在谁的什么授权下进行」都记不下来。
-- 拆掉的不只是一道闸，还有账本。
--
-- 本迁移把五条 grant 阻断换成三条 consent 阻断：
--
--   missing_capture_grant                → 由既有的 missing_consent 承担
--   capture_grant_disallows_trainable    → 由既有的 consent_disallows_trainable 承担
--   capture_grant_inactive               ┐
--   capture_outside_grant_window         ├→ consent_window_mismatch（按采集时点判）
--   capture_grant_scope_mismatch         ┘  + consent_scope_insufficient（按模态判）
--
-- ⚠ 基版：本视图**上次重建在 0030**（depth，docs/105），不是 0024。下面这份基于 0030
-- 逐字改，只动 consent/grant 相关的几行；depth 判据、件数期望、其余二十余条一字未动。
-- `CREATE OR REPLACE` 链上基错版本会静默丢语义——基于 0024 那份写会把 depth 判据抹掉，
-- 而 Postgres 不会有任何提示（0023 的教训）。
--
-- ⚠ 本迁移**不放宽**任何东西。两个存活 admission family 的 bestClassification 都是
-- non_trainable（environment.ts），`source_non_trainable` 因此恒亮，全库可训练 clip
-- 仍为 0。改的是「授权事实怎么记、怎么判」，不是「数据能不能训练」——后者仍然只能由
-- 发布侧人工放行开启（AGENTS.md 硬约束，docs/100 §3）。
--
-- `capture_grants` 表与 `sessions.capture_grant_id` 列都保留：既有 grant 行是历史，
-- 删表要连带改视图与外键（docs/100 §4.4 的同一条理由）。它们从判据里退出，不从库里消失。

BEGIN;

-- ── §1 sessions 的分类前置：不再要求 capture_grant_id ────────────────────
--
-- 0001 给 `sessions` 加了一条匿名 CHECK：
--     server_data_classification = 'non_trainable'
--     OR (annotator_id IS NOT NULL AND consent_id IS NOT NULL AND capture_grant_id IS NOT NULL)
-- 也就是说，任何非 non_trainable 的 session **在数据库层面**都必须挂一个 grant。而 grant
-- 自 docs/100 起没有写入方——这条约束因此不是"多一道保险"，是一堵**没有门的墙**：视图判据
-- 换掉之后它仍会让摄取面写不进任何可训练分类的行，且报错是一句 sessions_check5。
--
-- 收掉 grant 那一项，annotator + consent 两项照旧要求。约束**没有放宽**——它要求的仍然是
-- 「说得出是谁、在什么授权下采的」，只是授权的载体从逐游戏 grant 换成了本人的 consent。
--
-- 匿名约束按名字删是脆的（sessions_check5 是 Postgres 按顺序编的号，随建表语句变动而变）。
-- 这里按**定义**找它，找不到就明确失败，不静默跳过。
DO $$
DECLARE
    target text;
BEGIN
    SELECT conname INTO target
      FROM pg_constraint
     WHERE conrelid = 'sessions'::regclass
       AND contype = 'c'
       AND pg_get_constraintdef(oid) LIKE '%capture_grant_id IS NOT NULL%'
       AND pg_get_constraintdef(oid) LIKE '%server_data_classification%';
    IF target IS NULL THEN
        RAISE EXCEPTION
            'migration 0032 could not find the sessions classification CHECK that requires capture_grant_id; the schema is not what this migration was written against';
    END IF;
    EXECUTE format('ALTER TABLE sessions DROP CONSTRAINT %I', target);
END $$;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_trainable_classification_needs_consent_check CHECK (
        server_data_classification = 'non_trainable'
        OR (annotator_id IS NOT NULL AND consent_id IS NOT NULL)
    );

-- ── §2 clip_trainability ────────────────────────────────────────────────
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
            CASE WHEN s.server_data_classification <> 'production_candidate' THEN 'source_non_trainable' END,
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
    'Read-only, fail-closed server derivation. Authorization is the annotator''s own consent, valid for the moment capture started and covering every modality the clip actually carries; the per-game capture_grants lane is no longer consulted (docs/108 D1). Expected artifact count is 5, plus one for audio-present session v3, plus one for depth-present clips. Depth additionally requires a policy version listed in depth_consent_policy_versions. Catalog, executable, view-scale, QA, and retention gates are unchanged.';

COMMENT ON TABLE capture_grants IS
    'Historical per-game capture authorization. No writer since docs/100 removed POST /v1/capture-grants, and no reader since docs/108 moved trainability off it: clip_trainability now derives authorization from the session''s consent. Retained because existing rows are history and sessions.capture_grant_id still references them.';

INSERT INTO schema_migrations(version, name)
VALUES (32, 'consent_replaces_capture_grant');

COMMIT;
