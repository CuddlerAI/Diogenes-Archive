BEGIN;

-- ── 0038 · games.title 中性化（issue #855 PR 2）────────────────────────────────
--
-- 31 份活跃 bundle 的 `display_name` 在本 PR 里换成了中性标题（去掉「禁止训练」/
-- non-trainable / dev / release 测试，保留平台与采集范围描述）。**光改文件不够。**
--
-- `registerCatalogBundle()` 遇到已存在的 `games` 行时只做两件事：
--   `allowed_for_capture = allowed_for_capture OR $2`
--   `approved_for_trainable = approved_for_trainable OR $3`
-- 它**从不写 title**；而且外层那个 `escalate` 条件为假时（两个 flag 都已经是想要的
-- 值），连这条 UPDATE 都不会发。所以"重新注册新版本 bundle"这件事无论做多少次，
-- 库里那些写着「禁止训练」的标题都不会变（apps/api/src/register-catalog.ts）。
--
-- 而标题不是装饰：它是 issue #855 §4 三道守卫里的两道读的东西——
--   ② register-catalog 拒绝对标题命中 `禁止训练|non[-_ ]?trainable` 的游戏执行
--      `--game-trainable`（读 bundle 的 title）；
--   ③ 迁移 0028 的 `games_non_trainable_title_guard` CHECK（读 **DB 里的** title）。
-- 不迁这一步，PR 5 的"后批准"会在数据库层被 0028 挡住，而那时人会以为是审批流程的
-- 问题，不会想到是一行三个月前写下的展示名。
--
-- 本迁移**只改 title**，逐 id 点名：
-- - 不碰 `approved_for_trainable`（后批准是发布侧的人工动作，不是迁移的副作用）；
-- - 不碰 `allowed_for_capture`；
-- - 不做正则批量 UPDATE。库里可能还有别的标题命中那条正则（历史条目、其他环境的
--   夹具行），它们不在本 PR 的 31 份活跃 catalog 里，就不该被顺手改名——一次改名是
--   一次身份变更，得有出处。
-- - 0028 的 CHECK **保留、不放宽**：它继续阻止未来把明确写着「禁止训练」的标题误批
--   为可训练（issue #855 §A.6）。新标题都不命中该正则，所以本迁移只会让约束更容易
--   满足，不会与它冲突。
--
-- 幂等：只更新 title 确实不同的行；重跑是 0 行、0 条审计。

CREATE TEMP TABLE neutral_game_titles(id text PRIMARY KEY, title text NOT NULL) ON COMMIT DROP;

INSERT INTO neutral_game_titles(id, title)
VALUES
        ('a-plague-tale-innocence-steam-release-test', 'A Plague Tale: Innocence Steam'),
        ('age-of-empires-4-steam', 'Age of Empires IV: Anniversary Edition (Steam, RTS subset)'),
        ('atomic-heart-pc', 'Atomic Heart (PC, on-foot only)'),
        ('avatar-frontiers-of-pandora-pc', 'Avatar：潘多拉边境（PC 本地步战）'),
        ('bioshock-infinite-steam-e2e', 'BioShock Infinite Steam（本机生产 E2E）'),
        ('call-of-duty-steam-release-test', 'Call of Duty® Steam'),
        ('cyberpunk-2077-steam', 'Cyberpunk 2077 Steam（本机 RGB 预检）'),
        ('deathloop-pc', 'DEATHLOOP (PC)'),
        ('doom-eternal-pc', 'DOOM Eternal (PC)'),
        ('dying-light-pc', '消逝的光芒（PC 本地步战）'),
        ('elden-ring-nightreign-steam-release-test', 'ELDEN RING NIGHTREIGN Steam'),
        ('euro-truck-simulator-2-steam', 'Euro Truck Simulator 2 (Steam, in-cab driving)'),
        ('fallout4-steam-release-test', 'Fallout 4 Steam'),
        ('far-cry-5-pc', '孤岛惊魂 5（PC 本地步战）'),
        ('forza-horizon-5-pc', '极限竞速：地平线 5（Default WASD 驾驶，PC 本地）'),
        ('gta5-legacy-pc', '侠盗猎车手 V Legacy（单人徒步，PC 本地）'),
        ('hades-ii-steam-release-test', 'Hades II Steam'),
        ('halo-infinite-steam-release-test', 'Halo Infinite Steam'),
        ('halo-reach-mcc-s0', 'Halo: Reach (MCC) Steam S0'),
        ('indiana-jones-great-circle-pc', '夺宝奇兵：古老之圈（单人徒步，PC 本地）'),
        ('mount-and-blade-2-bannerlord-steam', 'Mount & Blade II: Bannerlord (Steam combat)'),
        ('nioh2-steam', 'Nioh 2 (Steam)'),
        ('prey-2017-pc', 'Prey（2017 主战役徒步，PC 本地）'),
        ('red-dead-redemption-2-pc', '荒野大镖客：救赎 2（单人徒步，截图键位待真机复核，PC 本地）'),
        ('resident-evil-village-pc', 'Resident Evil Village (PC, main campaign)'),
        ('starfield-pc', '星空（单人徒步，PC 本地）'),
        ('synthetic-win32-dev', 'DataMiner Synthetic Game'),
        ('titanfall-2-pc', '泰坦陨落 2（单人 Pilot，PC 本地）'),
        ('total-war-warhammer-iii-steam-release-test', 'Total War: WARHAMMER III Steam'),
        ('wolfenstein2-steam', 'Wolfenstein II: The New Colossus (Steam)'),
        ('wuchang-fallen-feathers-steam', 'WUCHANG: Fallen Feathers (Steam)')
;

-- 先点名存量（照 0022/0024/0029/0035/0036 的先例）：有多少行真的在库里、其中多少
-- 行标题还自称禁止训练。这不是形式——生产库里这 31 个 id 只有 8 个注册过
-- （shipped-games.json 的 admission=admitted），迁移在别的环境跑出来的行数不同是
-- 正常的，但"跑完还剩几行命中"必须是 0，见文件末尾的断言。
DO $$
DECLARE
    present bigint;
    still_declaring bigint;
BEGIN
    SELECT count(*) INTO present
      FROM games g JOIN neutral_game_titles n ON n.id = g.id;
    SELECT count(*) INTO still_declaring
      FROM games g JOIN neutral_game_titles n ON n.id = g.id
     WHERE g.title ~* '禁止训练|non[-_ ]?trainable';
    RAISE NOTICE 'migration 0038: % of 31 active catalog games are registered here; % still carry a non-trainable title',
        present, still_declaring;
END $$;

-- 先把"要改哪些行、原来写的是什么"物化下来。UPDATE ... RETURNING 拿不到旧值，而
-- 旧标题正是这条审计唯一不可重建的信息：改完之后库里再没有任何地方记得它。
CREATE TEMP TABLE game_title_rename ON COMMIT DROP AS
SELECT g.id, g.title AS old_title, n.title AS new_title, g.approved_for_trainable
  FROM games g
  JOIN neutral_game_titles n ON n.id = g.id
 WHERE g.title IS DISTINCT FROM n.title;

UPDATE games g
   SET title = r.new_title
  FROM game_title_rename r
 WHERE r.id = g.id;

INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0038',
    'game.title_neutralized',
    'game',
    r.id,
    jsonb_build_object(
        'oldTitle', r.old_title,
        'newTitle', r.new_title,
        'reason', 'catalog_display_name_neutralized_issue_855_pr2',
        -- 明确记下这一位没被动过：本迁移只改展示名，后批准是发布侧的人工动作。
        'approvedForTrainable', r.approved_for_trainable
    )
FROM game_title_rename r;

-- 迁移自身即保证目标状态可验证：跑完之后，31 个活跃 catalog 游戏里不得再有任何一行
-- 的标题命中 0028 的那条正则。断言比"我改对了"强——它验的是库里现在这份。
DO $$
DECLARE
    offenders text;
BEGIN
    SELECT string_agg(format('%s=%L', g.id, g.title), ', ')
      INTO offenders
      FROM games g JOIN neutral_game_titles n ON n.id = g.id
     WHERE g.title ~* '禁止训练|non[-_ ]?trainable';
    IF offenders IS NOT NULL THEN
        RAISE EXCEPTION
            'migration 0038: active catalog game(s) still declare themselves non-trainable after the rename: %',
            offenders;
    END IF;
END $$;

-- 0028 的 CHECK 必须还在。删它是另一件事，得单独裁决；本迁移只是把标题改成不再
-- 触发它的样子，不是把它拆掉（issue #855 §A.6）。
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'games_non_trainable_title_guard'
           AND conrelid = 'games'::regclass
    ) THEN
        RAISE EXCEPTION 'migration 0038: games_non_trainable_title_guard is gone; 0028 must stay in force';
    END IF;
END $$;

INSERT INTO schema_migrations(version, name)
VALUES (38, 'neutral_game_titles');

COMMIT;
