-- 0028 · 自称「禁止训练」的游戏不得携带 approved_for_trainable。
--
-- 生产库里 `games` 只有一行 approved_for_trainable = true，而它恰恰是
-- `call-of-duty-steam-release-test`——标题写着「release 测试，禁止训练」。
--
-- 成因（按 content_hash 与 git blob 对齐还原）：2026-07-26 07:58:14Z 那次注册用的
-- 不是 dev 的 cod-steam-release-test bundle，而是已被删除的
-- `config/catalogs/release/cod-steam/`（catalog_id `dataminer-first-party-cod-steam`、
-- `catalog_mode: first_party_release`、`non_trainable: false`）。那本 release bundle 是
-- 照着 dev bundle 改出来的：`non_trainable` 翻成 false、mode 换成 first_party，但
-- **game_id 与 display_name 一个字没改**，仍是 dev 的
-- `call-of-duty-steam-release-test` /「release 测试，禁止训练」。
-- `games` 一个 game_id 只有一行，于是按 docs/31 §102 那条
-- `--approve-trainable --allow-capture --game-trainable` 注册时，可训练审批落到了
-- 一行自称禁止训练的记录上。
--
-- 随后 first-party release 那条线整体作废（PR #226 删除该 bundle、schema v2 连
-- `catalogs/release/` 目录一并去掉），真正可训练的 COD 另起了 `call-of-duty-steam`
-- 这个 id。但 register-catalog 是只升不降的（`approved_for_trainable OR $3`），
-- 这行陈旧的 true 没有任何注册路径能清掉——`games.updated_at = created_at`
-- 即为证据：自 2026-07-26 写入后从未被改动过。
--
-- 今天没有泄漏：7 条 session 全是 `non_trainable` / `dev_non_trainable`，
-- `source_non_trainable` 等 blocker 仍在挡，全库可训练 clip 为 0。但 `game_unapproved`
-- 这道游戏级闸门对它是关掉的，只差一次分类变更就会真正生效。
--
-- 本迁移做三件事：把这行降回 false；清掉同一次错误注册留下的 4 行 catalog 可训练
-- 审批（撤销走 fail-closed 方向，status 不动，无 session 引用它们）；并加一条 CHECK，
-- 让「标题自称禁止训练」与「approved_for_trainable」在任何写入路径上都不能共存。

BEGIN;

-- 1) 标题自称禁止训练的游戏一律降回不可训练。写成通用规则而非钉死 id，这样它与
--    下面那条 CHECK 是同一条口径，迁移自身即保证约束可验证通过。
WITH demoted AS (
    UPDATE games
       SET approved_for_trainable = false
     WHERE approved_for_trainable
       AND title ~* '禁止训练|non[-_ ]?trainable'
    RETURNING id, title
)
INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0028',
    'game.trainable_approval_revoked',
    'game',
    demoted.id,
    jsonb_build_object(
        'title', demoted.title,
        'reason', 'title_declares_non_trainable_contradicting_approval',
        'origin', 'first_party_release_bundle_reused_dev_game_identity_2026-07-26'
    )
FROM demoted;

-- 2) 同一次注册（2026-07-26 07:58:14Z）留下的 4 行 catalog 可训练审批。它们的
--    snapshot 自称 non_trainable=false（来自那本 release bundle），所以按 snapshot
--    判不出来；这里按身份逐行点名，属一次性清理，不引入新的通用不变量。
--    capture_profile / settings_profile 两个 kind 在 0022 合并进 recording_profile
--    后已不再被 clip_trainability 消费；device_profile v1 与 mapping 2026-07-24.2
--    仍是活 kind 的旧版本。四行均无 session 引用，撤销只会更收紧。
WITH revoked AS (
    UPDATE trusted_catalog_versions
       SET approved_for_trainable = false
     WHERE approved_for_trainable
       AND (kind, catalog_id, version) IN (
           ('device_profile'::catalog_kind, 'cod-steam-windows-nvidia-release-test-v1', '1'),
           ('capture_profile'::catalog_kind, 'cod-steam-release-test-1920x1080-30-v1', '1'),
           ('settings_profile'::catalog_kind, 'cod-steam-sdr-1920x1080-30-release-test-v1', '1'),
           ('game_mapping'::catalog_kind, 'game.call-of-duty.steam.pc', '2026-07-24.2')
       )
    RETURNING id, kind, catalog_id, version
)
INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0028',
    'catalog_version.trainable_approval_revoked',
    'trusted_catalog_version',
    revoked.id::text,
    jsonb_build_object(
        'kind', revoked.kind,
        'catalogId', revoked.catalog_id,
        'version', revoked.version,
        'reason', 'granted_by_the_same_erroneous_2026-07-26_registration'
    )
FROM revoked;

-- 3) 结构性闸门：标题自己声明禁止训练，就不能同时是可训练审批过的。
--    标题是自由文本，这条 CHECK 只认本仓库实际在用的两种写法（中文「禁止训练」、
--    英文 non-trainable / non_trainable），够覆盖现有 8 行的命名习惯；它是兜底，
--    不是唯一防线——真正的根因修复在 register-catalog.ts 的注册期校验。
ALTER TABLE games
    ADD CONSTRAINT games_non_trainable_title_guard
    CHECK (NOT approved_for_trainable OR title !~* '禁止训练|non[-_ ]?trainable');

COMMENT ON CONSTRAINT games_non_trainable_title_guard ON games IS
    'A game whose title declares it non-trainable (禁止训练 / non-trainable) cannot carry approved_for_trainable. Backstop for the 2026-07-26 incident where a first-party release bundle reused the dev release-test game identity; the primary guard is in register-catalog.ts.';

INSERT INTO schema_migrations(version, name)
VALUES (28, 'non_trainable_title_guard');

COMMIT;
