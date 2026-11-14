-- 0031 · consent 携带授权范围（scope）；「换版」与「撤回」分家。
--
-- 依据 docs/108 §3.1（模型）、§2 D2（为什么用 scope 集合）、§2 D5（为什么 supersede
-- 不是 revoke）。
--
-- 两件事：
--
-- 1) `consents.scopes` —— 这份文书授权了哪些事。今天系统里表达"授权范围"的手段只有
--    `allows_trainable_use` 一个布尔，以及 `apps/api` 里那个把音频钉死在
--    `2026-07-29-audio-public` 版本串上的相等比较（repository.ts:2576）。后者每加一版
--    条款就要改一次代码；前者根本表达不了"新条款比旧条款多授权了什么"，于是客户端也
--    无法告诉采集员补签能扩大什么范围。scope 集合把这件事变成差集运算。
--
-- 2) `superseded_at` / `superseded_by` —— 换版不是撤回。今天 `recordConsentRenewal`
--    把旧 consent 置成 `status='revoked'`，而 `clip_trainability` 按 `status <> 'active'`
--    判 `consent_inactive`：**采集员为了扩大授权而补签的那一刻，他此前所有数据都被判成
--    "授权已撤回"**。更糟的是语义混淆——撤回是采集员行使条款第 6/7 条的权利、必须触发
--    purge，换版是同一个人继续授权只是换了文书。两者记成同一个状态，"撤回即删除"就没有
--    可执行的判据。
--
-- 本迁移只改 `consents` 表。`clip_trainability` 视图**零触碰**——它读这两列的收敛在
-- 0032（docs/108 §3.5），拆开是为了让"加列"与"改判据"分别可回滚。0030 刚重建过该视图
-- （depth，docs/105），下一刀必须基于 0030 的形状改，不是 0024 的。
--
-- ⚠ scope 回填只按 **terms_hash（文书的逐字节身份）**，映射表见 docs/108 §3.2，逐条对到
-- 条款序号。这不违反"consent 永不回填"那条纪律——被回填的不是"这个人同意了什么"，而是
-- "他签的那份**字节完全确定**的文书说了什么"。真正被禁止的是"按**当前配置**回填历史行"，
-- 那才会改写一份 consent 被给出时的含义。哈希对不上的行留空集（fail-closed）。
--
-- ⚠ 2026-07-25-teaser 有两个哈希，这不是笔误：docs/105 §4 把该正文**就地修订**了
-- （加 clause 3 depth、把 audio 写进 clause 2），版本串没变而字节变了。所以：
--   1ed1fdd3… = 修订前签的那批人**实际读到的**文本，没有 audio、没有 depth；
--   10b14ab6… = 今天部署的文本。
-- 修订前的行按**他们读到的那份**回填，不按今天这份。docs/105 用"内部员工线下逐一告知"
-- 补上了这个差，本迁移不推翻那条裁决，也不替它盖章——它只是让差额变成机读的
-- `pendingScopes`，于是同一件事有了一条可自助、留痕的补签路径（docs/108 §0 目标 1）。
-- depth 的**准入**判据仍然只有 0030 建的 `depth_consent_policy_versions` 一处，本迁移
-- 不新增第二道 depth 闸；两者的收敛留给 0032（docs/108 §6）。

BEGIN;

-- ── §1 列 ────────────────────────────────────────────────────────────────
ALTER TABLE consents
    ADD COLUMN scopes text[] NOT NULL DEFAULT '{}'::text[],
    ADD COLUMN superseded_at timestamptz,
    ADD COLUMN superseded_by uuid REFERENCES consents(id) ON DELETE RESTRICT;

-- 受控词表。新增 scope 词必须同时在 docs/108 §3.2 的对账表里写明它对应文书的哪一条
-- ——翻译可能出错，但必须有地方对账。
ALTER TABLE consents
    ADD CONSTRAINT consents_scopes_vocabulary_check CHECK (
        scopes <@ ARRAY['input', 'video', 'audio', 'depth', 'public_release', 'trainable_research']::text[]
    );

-- `scopes <@ ...` 对含 NULL 元素的数组算出 NULL，而 CHECK 对 NULL 放行——词表约束会被
-- 一个 NULL 元素整体绕过。这条堵住它。
ALTER TABLE consents
    ADD CONSTRAINT consents_scopes_no_null_element_check CHECK (
        array_position(scopes, NULL) IS NULL
    );

-- 单向配对：可以只知道"被换掉了"而不知道被谁换掉（历史行可能查不到后继），但不能反过来
-- 指名后继却说不出何时被换。
ALTER TABLE consents
    ADD CONSTRAINT consents_superseded_by_needs_time_check CHECK (
        superseded_by IS NULL OR superseded_at IS NOT NULL
    );

-- 按时点取有效 consent 的查询走这个索引（0032 与 apps/api 的 activeConsentForSession）。
-- 既有的 consents_annotator_status_idx 是按 status 建的，对"时点落在哪个窗口"无用。
CREATE INDEX consents_annotator_window_idx
    ON consents(annotator_id, accepted_at DESC)
    WHERE accepted_at IS NOT NULL;

-- ── §2 scope 回填（只认逐字节确定的三份文书）─────────────────────────────
--
-- 映射依据 docs/108 §3.2：
--   5061fef6 2026-07-24-staging       第 2 条采集+研究；**无**公开发布/音频/depth
--   1ed1fdd3 2026-07-25-teaser 修订前  第 2 条 input/video + 研究、第 3 条公开发布；
--                                      **无**音频、**无** depth（正文里根本没有这两个词）
--   10b14ab6 2026-07-25-teaser 修订后  第 2 条含音频、第 3 条 depth、第 4 条研究、第 5 条公开发布
--   e048368c 2026-07-29-audio-public  第 2 条含音频、第 3 条研究、第 4 条公开发布；**无** depth
WITH stamped AS (
    UPDATE consents
       SET scopes = CASE terms_hash
               WHEN '5061fef6b6a4595d963061efcb544cca8e5f243990ce6d9398dc41e7713cb3d0'
                   THEN ARRAY['input', 'trainable_research', 'video']::text[]
               WHEN '1ed1fdd3809a6fc713a340e5080be05c9f6721bd6fca5d65441fbf9d4e73b389'
                   THEN ARRAY['input', 'public_release', 'trainable_research', 'video']::text[]
               WHEN '10b14ab69e36ba9c9bcdd605e9eaa4447b660f961c94dce47ac5448b7229856a'
                   THEN ARRAY['audio', 'depth', 'input', 'public_release', 'trainable_research', 'video']::text[]
               WHEN 'e048368cfad32fe8641a5562f213ec1de7f680efa7e46efca7ab830c8758e50b'
                   THEN ARRAY['audio', 'input', 'public_release', 'trainable_research', 'video']::text[]
           END
     WHERE terms_hash IN (
               '5061fef6b6a4595d963061efcb544cca8e5f243990ce6d9398dc41e7713cb3d0',
               '1ed1fdd3809a6fc713a340e5080be05c9f6721bd6fca5d65441fbf9d4e73b389',
               '10b14ab69e36ba9c9bcdd605e9eaa4447b660f961c94dce47ac5448b7229856a',
               'e048368cfad32fe8641a5562f213ec1de7f680efa7e46efca7ab830c8758e50b'
           )
    RETURNING id, annotator_id, policy_version, scopes
)
INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, consent_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0031',
    'consent.scopes_stamped_from_terms_document',
    'consent',
    stamped.id::text,
    stamped.id,
    jsonb_build_object(
        'policyVersion', stamped.policy_version,
        'scopes', to_jsonb(stamped.scopes),
        'basis', 'terms_hash_exact_match_docs_106_section_3_2'
    )
FROM stamped;

-- ── §3 换版行改判：revoked(superseded_by_renewal) → expired + superseded_at ──
--
-- 后继按同一采集员、accepted_at 严格晚于本行的最早那一条取。取不到后继也照常改判，
-- 只是 superseded_by 留空——"被换掉了"这件事本身由 revoke_reason 证明，不依赖能否
-- 指名后继（§1 的配对约束是单向的，正是为此）。
WITH renewed AS (
    SELECT
        old.id,
        old.annotator_id,
        old.revoked_at,
        (
            SELECT next.id
              FROM consents next
             WHERE next.annotator_id = old.annotator_id
               AND next.accepted_at IS NOT NULL
               AND old.accepted_at IS NOT NULL
               AND next.accepted_at > old.accepted_at
             ORDER BY next.accepted_at ASC, next.id ASC
             LIMIT 1
        ) AS successor_id
      FROM consents old
     WHERE old.status = 'revoked'
       AND old.revoke_reason = 'superseded_by_renewal'
), converted AS (
    UPDATE consents c
       SET status = 'expired',
           superseded_at = renewed.revoked_at,
           superseded_by = renewed.successor_id,
           revoked_at = NULL,
           revoke_reason = NULL
      FROM renewed
     WHERE c.id = renewed.id
    RETURNING c.id, c.annotator_id, c.superseded_at, c.superseded_by
)
INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, consent_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0031',
    'consent.renewal_reclassified_from_revoked_to_superseded',
    'consent',
    converted.id::text,
    converted.id,
    jsonb_build_object(
        'supersededAt', converted.superseded_at,
        'supersededBy', converted.superseded_by,
        'reason', 'renewal_is_not_withdrawal_docs_106_decision_d5'
    )
FROM converted;

-- 改判之后不该再有任何以"换版"为由的撤回行。先断言（错误消息说人话），再上约束
-- （挡住以后的写入）——只上约束的话，失败会是一句 "violates check constraint"，
-- 读它的人得自己反推发生了什么。
DO $$
DECLARE
    leftover bigint;
BEGIN
    SELECT count(*) INTO leftover
      FROM consents
     WHERE revoke_reason = 'superseded_by_renewal';
    IF leftover > 0 THEN
        RAISE EXCEPTION
            'migration 0031 left % consent row(s) still recorded as revoked for renewal; renewal must not be stored as withdrawal (docs/108 D5)',
            leftover;
    END IF;
END $$;

-- 换版不得再被记成撤回（docs/108 D5）。
--
-- 注意这条**不是**"superseded 与 revoked 互斥"。两者同时成立是合法且必须支持的状态：
-- 一份 consent 可以先在 7/29 被换版、再在 7/30 被本人撤回——条款第 6/7 条承诺的
-- purge 必须够得着历史 consent，而历史 consent 按定义都是被换版过的。把两列写成互斥
-- 会让"撤回"在最需要它的那批行上直接失败。真正要禁的只有一件事：把换版**记成**撤回。
ALTER TABLE consents
    ADD CONSTRAINT consents_renewal_is_not_withdrawal_check CHECK (
        revoke_reason IS DISTINCT FROM 'superseded_by_renewal'
    );

-- ── §4 注释 ──────────────────────────────────────────────────────────────
COMMENT ON COLUMN consents.scopes IS
    'What the signed document authorizes, stamped from deployment config at signing time and never backfilled from current config. Vocabulary and the scope-to-clause mapping live in docs/108 §3.2. Compare with set operators (@>, <@), not array equality: ordering and duplicates carry no meaning.';

COMMENT ON COLUMN consents.superseded_at IS
    'When a later consent replaced this one. Superseding is not withdrawal (docs/108 D5): a superseded consent still authorizes the data captured inside its own window, and purge must not treat it as a revocation.';

COMMENT ON COLUMN consents.superseded_by IS
    'The consent that replaced this one, when it can be identified. Provenance only — the validity window is decided by superseded_at.';

INSERT INTO schema_migrations(version, name)
VALUES (31, 'annotator_scoped_consent');

COMMIT;
