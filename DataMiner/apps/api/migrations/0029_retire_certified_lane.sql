BEGIN;

-- ── 废除 certified 车道：词表收回（docs/100 §4.3）─────────────────────────
--
-- 0024 为 certified 会话放宽了三处词表：`sessions` 的 schema_version、
-- `capture_runs_preflight_reference_check` 的 preflight_result，以及
-- `enforce_capture_run_preflight_contract()` 的 mode↔family 配对。车道整条废除
-- （docs/100），这三处一起收回——留着的后果不是"以后好恢复"，而是一条服务端仍然
-- 收得进、却再没有任何客户端能产出、也没有任何发布路径认得的幽灵车道。
--
-- 本迁移**只动词表**。三件事明确不做：
--   1. `capture_grants` 表不删——`clip_trainability` 视图引用它，删表必改视图，
--      而那个视图在 0001/0016/0018/0022/0023/0024 反复重定义，改它即越界
--      （docs/100 §3 推论 1）。表变成"无写入方的空表"：签发接口已随 PR2 删除，
--      grant 相关的阻断条件因此恒不满足 → 维持"零自动可训练"现状，与今天一致。
--   2. `clip_trainability` 视图零触碰。它仍会提到 `dm-certified-session-3`——
--      一个永远匹配不上的常量，无害；表与视图的收敛归下一刀（可训练性开通）。
--   3. `catalog_releases.channel` 的 `IN ('dev','certified')` 不动。那是发布面的
--      词表，不是采集准入；收它要连带重建 0015 的唯一索引与 release 指针机制，
--      是一次独立的、发布侧的决定。
--
-- ⚠ 基版纪律（0023 的教训）：`enforce_capture_run_preflight_contract()` 下面这份
-- 函数体基于 **0024** 的版本改（0028 没有重定义过它，所以 0024 就是"0028 之后的
-- 函数现状"）。`CREATE OR REPLACE` 链上基错版本会静默丢语义。
-- 本次相对 0024 的变化只有一处：versioned 分支不再按父会话的 catalog_mode 分派
-- 通过值，而是只认唯一那条车道的通过值。

-- 收词表之前先点名存量（照 0022/0024 的先例）。这不是形式：下面的 CHECK 会以一句
-- 语焉不详的约束冲突失败，而"有多少行、该怎么处置"是**数据决定**，不该由一条迁移
-- 替谁做主。§0.2 的人工实测（生产 certified 零数据）不豁免这一步——只信一次人工
-- 确认，等于把"这条迁移在哪些库上安全"写进了一次对话而不是代码。
DO $$
DECLARE
    certified_sessions bigint;
    certified_runs bigint;
    certified_grants bigint;
BEGIN
    SELECT count(*) INTO certified_sessions
      FROM sessions
     WHERE catalog_mode = 'certified'
        OR schema_version IN ('dm-certified-session-2', 'dm-certified-session-3');
    SELECT count(*) INTO certified_runs
      FROM capture_runs
     WHERE preflight_result = 'certified_pass';
    SELECT count(*) INTO certified_grants FROM capture_grants;
    IF certified_sessions > 0 OR certified_runs > 0 OR certified_grants > 0 THEN
        RAISE EXCEPTION
            'migration 0029 refuses to retire the certified vocabulary while its rows exist: % session(s), % capture run(s), % capture grant(s). The lane was removed by docs/100 on the premise that production carries none of these; decide explicitly whether they are archived, reclassified, or deleted before re-running.',
            certified_sessions, certified_runs, certified_grants;
    END IF;
END $$;

-- 1) sessions 的 schema_version 词表退回 dm-local-dev-session-1/2/3。
--    逐字拷贝 0024 的形状，只删 certified 那一臂——v2/v3 那臂的 `catalog_mode
--    IS NOT NULL AND catalog_mode = 'dev_non_trainable'` 连接词照旧（0004 起
--    catalog_mode 可空，裸等号在 NULL 上算出 NULL 而 CHECK 对 NULL 放行；
--    历代约束都带它，收词表不能反而放宽 NULL）。
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
            AND catalog_mode IS NOT NULL
            AND catalog_mode = 'dev_non_trainable'
            AND catalog_id IS NOT NULL
            AND btrim(catalog_id) <> ''
            AND catalog_version IS NOT NULL
            AND btrim(catalog_version) <> ''
            AND catalog_manifest_sha256 IS NOT NULL
            AND game_profile_sha256 IS NOT NULL
        )
    );

-- 2) preflight_result 词表退回单值。客户端侧的权威取值是
--    `preflight::pass_result_for(mode)`，它在 docs/100 之后只剩
--    `dev_non_trainable_pass` 一个分支；journal v27 的 CHECK 钉着同一个值。
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
            AND preflight_result = 'dev_non_trainable_pass'
            AND preflight_evidence_sha256 IS NOT NULL
        )
    );

-- 3) 触发器函数：基于 0024 的版本，收回 mode↔family 分派。
--    父会话的 catalog_mode 仍然要核（一个 mode 对不上的父会话是损坏，不是
--    "另一条车道"），只是它现在只有一个合法值。
CREATE OR REPLACE FUNCTION enforce_capture_run_preflight_contract()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_schema_version text;
    parent_catalog_mode text;
BEGIN
    SELECT schema_version, catalog_mode
      INTO parent_schema_version, parent_catalog_mode
      FROM sessions
     WHERE id = NEW.session_id;

    IF parent_schema_version IN ('dm-local-dev-session-2', 'dm-local-dev-session-3') THEN
        IF parent_catalog_mode IS DISTINCT FROM 'dev_non_trainable' THEN
            RAISE EXCEPTION 'versioned capture run has an unsupported parent catalog mode'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.preflight_schema_version IS DISTINCT FROM 'dm-preflight-evidence-1'
           OR NEW.preflight_id IS NULL
           OR btrim(NEW.preflight_id) = ''
           OR NEW.preflight_result IS DISTINCT FROM 'dev_non_trainable_pass'
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

COMMENT ON TABLE capture_grants IS
    'Retired write path (docs/100): the issuing endpoint POST /v1/capture-grants was removed and nothing writes this table any more. It is kept only because clip_trainability still joins it — with no rows, every grant-related blocker stays unsatisfied, which preserves today''s "no automatic trainable clip" behaviour. Table and view converge in the trainability-enablement cut.';

INSERT INTO schema_migrations(version, name)
VALUES (29, 'retire_certified_lane');

COMMIT;
