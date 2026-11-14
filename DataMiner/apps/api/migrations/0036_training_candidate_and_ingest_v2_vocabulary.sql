BEGIN;

-- ── issue #855 PR 1：词表迁移（training_candidate + dm-session-ingest-2）────────
--
-- 本迁移只做**词表**，不做数据搬迁：
--   §1 `production_candidate` → `training_candidate`（纯改名，历史零行）
--   §2 `sessions.schema_version` 词表新增 `dm-session-ingest-2`
--   §3 `capture_runs.preflight_result` 词表新增 `preflight_pass`
--   §4 preflight 配对触发器认识新一代父会话
--
-- 明确**不做**的事，逐条对应 issue #855「存量数据边界」：
-- - 不 UPDATE 任何 `server_data_classification`；
-- - 不改任何 `approved_for_trainable`；
-- - 不动 `raw/v1/non-trainable` 存量与既有 session；
-- - 不新增写入方。§2/§3 只是把 PR 3 要落的行**变得可存**，本 PR 里没有任何代码
--   会产生 `dm-session-ingest-2` 的会话（`loadConfig` 仍只准入 primary family）。

-- ── §1 production_candidate → training_candidate ────────────────────────────
--
-- production 是部署环境，candidate 是训练资格状态，两者不该串台（issue #855 §B）。
--
-- 先点名存量（照 0022/0024/0029/0035 的先例）。这不是形式：如果真有行，"纯改名"
-- 这个前提就不成立，得有人先裁决那些行怎么处置。RENAME VALUE 不会改变任何行的
-- 取值——同一个枚举值换了个 label——所以即使有行，本迁移也**不会**重分类它们；
-- 点名是为了让"有没有行"这件事落在迁移的输出里，而不是落在一次对话里。
DO $$
DECLARE
    candidate_sessions bigint;
BEGIN
    SELECT count(*) INTO candidate_sessions
      FROM sessions
     WHERE server_data_classification = 'production_candidate';
    RAISE NOTICE 'migration 0036: % session(s) currently classified production_candidate (rename is value-preserving)',
        candidate_sessions;
END $$;

ALTER TYPE data_classification RENAME VALUE 'production_candidate' TO 'training_candidate';

-- `clip_trainability` 里那句 `s.server_data_classification <> 'production_candidate'`
-- **不需要重写**：视图的存储解析树按枚举 OID 引用该值，RENAME VALUE 之后
-- `pg_get_viewdef` 直接输出新 label。重贴一遍 160 行的视图定义反而危险——它会把
-- 视图钉回本迁移写下的那一版，静默回滚任何在此之后、由更晚的迁移做出的改动。
--
-- 所以这里做的是**断言传播已经发生**，而不是重建。断言比重建强：重建只能保证
-- 「我贴的这份是对的」，断言能保证「库里现在这份是对的」。
DO $$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_viewdef('clip_trainability'::regclass) INTO definition;
    IF definition LIKE '%production_candidate%' THEN
        RAISE EXCEPTION
            'migration 0036: clip_trainability still references production_candidate after the enum rename';
    END IF;
    IF definition NOT LIKE '%training_candidate%' THEN
        RAISE EXCEPTION
            'migration 0036: clip_trainability does not reference training_candidate; the source_non_trainable blocker is not what this migration was written against';
    END IF;
END $$;

COMMENT ON TYPE data_classification IS
    'Source classification of a capture (issue #855 §B). non_trainable = a local test/diagnostic capture, or one missing a capture-time fact that cannot be forged afterwards; it never enters the promotion path. training_candidate = the provenance chain is complete but no final judgement has been made about the game, the configuration, the executable build or the actual data quality — it is still NOT trainable. Final trainability is derived by clip_trainability alone. Renamed from production_candidate: production names a deployment environment, candidate names a training-eligibility state.';

-- ── §2 sessions.schema_version 词表 ─────────────────────────────────────────
--
-- 逐字拷贝 0029 的形状，只加一臂。新一臂与 v2/v3 那臂有一处**有意的不同**：
-- `catalog_mode IS NULL`。`dev_non_trainable` 正是 issue #855 §A.4 要删的那个词，
-- 新契约不携带它；写成 NULL 而不是"随便什么值"，是为了让"新车道不得复活旧词"
-- 成为数据库层的事实，而不是应用层的自觉。
--
-- catalog 身份三件套（id / version / manifest sha / game profile sha）照旧必填：
-- 去环境化去的是车道词，不是 provenance。录制前仍必须登记并发布精确 Catalog
-- bytes（issue #855「前置边界」）。
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
        OR (
            schema_version = 'dm-session-ingest-2'
            AND catalog_mode IS NULL
            AND catalog_id IS NOT NULL
            AND btrim(catalog_id) <> ''
            AND catalog_version IS NOT NULL
            AND btrim(catalog_version) <> ''
            AND catalog_manifest_sha256 IS NOT NULL
            AND game_profile_sha256 IS NOT NULL
        )
    );

COMMENT ON COLUMN sessions.schema_version IS
    'The session contract this row was created under, and the ONLY storage routing key during the v1/v2 coexistence (issue #855 §B). Written once at session creation and immutable afterwards: runs, clips, multipart completions and session artifacts all read this column to choose prefix and object class, and never re-derive the lane from catalog identity or from a later request. dm-local-dev-session-2/3 stay on raw/v1/non-trainable with the legacy object class and are permanently non_trainable; dm-session-ingest-2 goes to raw/v2/<deployment>/sessions/ with object class dataminer-raw. Unknown versions are refused rather than defaulted.';

-- ── §3 capture_runs.preflight_result 词表 ───────────────────────────────────
--
-- 旧值 `dev_non_trainable_pass` 把车道名写进了结果里。中性值 `preflight_pass` 只
-- 回答"preflight 过了没有"。两个值同时合法，**配对**由下面的触发器强制：一个
-- v2 会话的 run 不得带 legacy 结果串，反之亦然。CHECK 只管词表，触发器管配对——
-- 这是 0009 起就有的分工，不改。
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
            AND preflight_result IN ('dev_non_trainable_pass', 'preflight_pass')
            AND preflight_evidence_sha256 IS NOT NULL
        )
    );

-- ── §4 preflight 配对触发器 ─────────────────────────────────────────────────
--
-- 基于 0029 的版本，只新增 `dm-session-ingest-2` 那一支。legacy 支一个字节没动，
-- 包括它对父会话 catalog_mode 的核对。新支反过来要求父会话**没有** catalog_mode：
-- 一个带着 `dev_non_trainable` 的 v2 父会话是损坏，不是"另一条车道"。
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
    ELSIF parent_schema_version = 'dm-session-ingest-2' THEN
        IF parent_catalog_mode IS NOT NULL THEN
            RAISE EXCEPTION 'neutral ingest session must not carry a retired catalog mode'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.preflight_schema_version IS DISTINCT FROM 'dm-preflight-evidence-1'
           OR NEW.preflight_id IS NULL
           OR btrim(NEW.preflight_id) = ''
           OR NEW.preflight_result IS DISTINCT FROM 'preflight_pass'
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

INSERT INTO schema_migrations(version, name)
VALUES (36, 'training_candidate_and_ingest_v2_vocabulary');

COMMIT;
