BEGIN;

-- ── 收回 catalog_releases 的 certified 渠道词表（docs/100 §11.7）───────────
--
-- 这是 certified 车道退役的最后一段。0029 收的是**采集**词表并明确把这一条留给
-- 发布侧（"那是发布面的词表，不是采集准入"）；PR #830 收的是发布侧**应用层**；
-- 本迁移收 DB 侧仅剩的那一条词表。
--
-- 0029 当时给的推迟理由是"收它要连带重建 0015 的唯一索引与 release 指针机制"。
-- **那条理由今天已经不成立**（issue #829 §3，docs/100 §10.3 已更正）：迁移 0022 §5
-- 为"每游戏一份 bundle"断代时，`DROP COLUMN catalog_mode` 连带带走了 0015 那条
-- channel↔catalog_id↔catalog_mode 的"两支舰队"复合 CHECK，并把
-- `UNIQUE (channel, catalog_version)` 降到 `(channel, catalog_id, catalog_version)`、
-- `one_published_per_channel` 换成 `one_published_per_catalog ON (channel, catalog_id)`、
-- `publish_catalog_release()` 的作用域降到 `(channel, catalog_id)`。这四样今天都
-- **不含 channel 单值假设**，`catalog_releases_history_idx ON (channel, created_at DESC)`
-- 同理。所以本迁移**一个索引、一条唯一约束、一个函数都不动**——它只有一条
-- UPDATE、一条 INSERT、一次 CHECK 替换。改到索引就是走偏了。
--
-- ⚠ 与 0029 的一处**有意不同**：0029 遇到存量就 RAISE EXCEPTION 停住，因为那时
-- "有多少行、该怎么处置"还没人裁决。这一次裁决有了（issue #829 §5 选项 A，
-- owner 2026-08-20），所以本迁移**处置**存量而不是拒绝执行。点名仍然照做。

-- ① 存量点名（照 0022/0024/0029 的先例）。
--    不许只信一次人工确认——"有多少行、都是什么"是数据事实，要落在迁移的输出里
--    而不是落在一次对话里。下面 ② 的 WHERE 是按渠道扫全表，不是按某一行钉死的。
DO $$
DECLARE
    certified_releases bigint;
    r record;
BEGIN
    SELECT count(*) INTO certified_releases
      FROM catalog_releases
     WHERE channel = 'certified';
    RAISE NOTICE 'migration 0035: certified catalog releases to entomb: %', certified_releases;
    FOR r IN
        SELECT catalog_id, catalog_version, status, created_at
          FROM catalog_releases
         WHERE channel = 'certified'
         ORDER BY created_at
    LOOP
        RAISE NOTICE '  % % status=% created=%',
            r.catalog_id, r.catalog_version, r.status, r.created_at;
    END LOOP;
END $$;

-- ② revoke 成墓碑，逐字照 0026 的形状。
--
--    为什么是 revoke 而不是删行：这张表**根本不允许删**。
--    `reject_catalog_release_delete()` 是 `BEFORE DELETE ON catalog_releases` 的无条件
--    RAISE（0015:200-211），`catalog_release_entries` 与 `catalog_payloads` 各有
--    `BEFORE UPDATE OR DELETE` 的 reject_mutation，FK 还是 ON DELETE RESTRICT。
--    四道守卫，任何一道都够。
--
--    为什么不用绕任何守卫：`protect_catalog_release()`（0015:182-190）本来就允许
--    draft/published/superseded → revoked；只要不动 `published_by` / `published_at`
--    就不会触发"publication attribution is immutable"。这正是 0026 注释说的效果：
--    "Revocation keeps their signed bytes and publication attribution for audit purposes."
--
--    ⚠ 生产库那条 `dataminer-certified-cod-steam` v2026-07-31.2 的 `envelope_bytes`
--    是那本已删除的 COD authoring bundle（60fps / GOP 240 / 进程音频 /
--    `non_trainable: false`，全仓唯一）**唯一一份签过名的留存**（docs/100 §10.6）。
--    revoke 保住字节与发布归属；签名密钥退役后它的**可验证性**会失去，但要留的
--    本来就是字节，不是可验证性。
WITH revoked AS (
    UPDATE catalog_releases
       SET status = 'revoked',
           revoked_by = 'migration-0035',
           revoked_at = transaction_timestamp()
     WHERE channel = 'certified'
       AND status IN ('draft', 'published', 'superseded')
    RETURNING id, channel, catalog_id, catalog_version
)
INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0035',
    'catalog.release_revoked',
    'catalog_release',
    revoked.id::text,
    jsonb_build_object(
        'channel', revoked.channel,
        'catalogId', revoked.catalog_id,
        'catalogVersion', revoked.catalog_version,
        'reason', 'certified_channel_retired'
    )
FROM revoked;

-- ③ 收词表。
--
--    ⚠ 0015:80 那条 CHECK 是**内联无名**的，名字由 Postgres 自动生成。本地把整条
--    迁移链跑进 PGlite 查过 `pg_constraint`，实测叫 `catalog_releases_channel_check`
--    ——但不写死它：按**定义**定位（收敛之后全表只有这一条 CHECK 提到 certified），
--    并断言恰好命中一条。写死错名字会让迁移在生产上以一句 "constraint does not
--    exist" 失败，而那句话完全不指向真正的问题。
--
--    新 CHECK 留 `OR status = 'revoked'` 那一臂，正是为了让 ② 造出来的墓碑活下去。
--    新的 certified **发布**仍然进不来：INSERT 的 status 默认 'draft'，
--    `channel='certified'` 配 draft/published 都过不了这条 CHECK，而 revoked
--    是终态（`protect_catalog_release()` 没有任何出边）。
DO $$
DECLARE
    channel_check_name text;
    matches bigint;
BEGIN
    SELECT count(*), min(conname)
      INTO matches, channel_check_name
      FROM pg_constraint
     WHERE conrelid = 'catalog_releases'::regclass
       AND contype = 'c'
       AND pg_get_constraintdef(oid) LIKE '%certified%';
    IF matches <> 1 THEN
        RAISE EXCEPTION
            'migration 0035 expected exactly one CHECK on catalog_releases mentioning the certified channel, found %. The vocabulary is not where this migration assumes; inspect pg_constraint before re-running.',
            matches;
    END IF;
    EXECUTE format(
        'ALTER TABLE catalog_releases DROP CONSTRAINT %I', channel_check_name);
END $$;

ALTER TABLE catalog_releases
    ADD CONSTRAINT catalog_releases_channel_retired
        CHECK (channel = 'dev' OR status = 'revoked');

INSERT INTO schema_migrations(version, name)
VALUES (35, 'retire_certified_catalog_channel');

COMMIT;
