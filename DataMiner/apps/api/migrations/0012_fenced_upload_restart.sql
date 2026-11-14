-- 0012 · fenced restart-upload-attempt（docs/30 §5.6）
--
-- 背景：upload intent 的寿命是 24 小时。过期之后客户端**不得**自行清掉本地
-- durable intent id 再建一个——它无法证明 provider 侧的状态。旧的 multipart
-- 可能正在 CompleteMultipartUpload 的窗口里、可能已经形成 immutable object、
-- 也可能已被 maintenance purge 处理掉。客户端清 id 会同时绕过 completion fence
-- （0002）和 purge fence，最坏情况是把一个已经完整的对象重新覆盖掉。
--
-- 所以重启是**服务端显式动作**，且必须留证据：
--   1. 锁 clip；
--   2. 确认旧 intent 确实过期（没过期就不许重启——那是客户端在乱来）；
--   3. 确认旧 multipart 没有活的 completion 窗口（0002 的 completion_lease_id /
--      completion_started_at），有活窗口就等，不抢；
--   4. 到 provider 上 abort 旧 multipart，并对五个 object key 逐个 HEAD 对账；
--   5. 只有五个对象**都不存在**才允许开新 attempt。任何一个已经存在，说明旧
--      attempt 实际上完成了一部分，重启会破坏 immutability —— fail-closed，
--      交给正常的 complete/verify 路径。
--
-- 证据全部落 upload_restart_attempts.evidence_json，且是 append-only 的事实
-- 记录（哪个 multipart abort 了、哪个 key HEAD 的结果是什么），出事能复盘。
--
-- 本表刻意不复用 purge_jobs：purge 是"把数据删干净"，restart 是"确认什么都没
-- 留下之后重来一次"，两者的终态含义相反，混在一张表里迟早会有人写错查询。

BEGIN;

CREATE TABLE upload_restart_attempts (
    id uuid PRIMARY KEY,
    clip_id uuid NOT NULL REFERENCES clips(id) ON DELETE RESTRICT,
    -- 被替换掉的那个过期 intent。
    from_upload_intent_id uuid NOT NULL REFERENCES upload_intents(id) ON DELETE RESTRICT,
    -- 新建的 intent；只有 succeeded 才有值。
    to_upload_intent_id uuid REFERENCES upload_intents(id) ON DELETE RESTRICT,
    status job_status NOT NULL DEFAULT 'running',
    reason text NOT NULL CHECK (btrim(reason) <> ''),
    requested_by_device_id uuid NOT NULL REFERENCES devices(id) ON DELETE RESTRICT,
    -- append-only 的 fence 证据：{"findings":[{kind,...}]}。
    evidence_json jsonb NOT NULL DEFAULT '{"findings": []}'::jsonb
        CHECK (jsonb_typeof(evidence_json) = 'object'),
    locked_at timestamptz,
    locked_by text,
    last_error_code text,
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    -- 成功必须指向新 intent，且新旧不能是同一个。
    CHECK (status <> 'succeeded' OR to_upload_intent_id IS NOT NULL),
    CHECK (to_upload_intent_id IS NULL OR to_upload_intent_id <> from_upload_intent_id),
    CHECK (status NOT IN ('succeeded', 'failed', 'dead_letter') OR completed_at IS NOT NULL),
    CHECK (status <> 'running' OR (locked_at IS NOT NULL AND locked_by IS NOT NULL))
);

COMMENT ON TABLE upload_restart_attempts IS
    'Server-owned fence for restarting an expired upload intent. A client may never clear its durable intent id on its own: it cannot prove provider state, and doing so would bypass both the multipart completion fence and the purge fence.';

COMMENT ON COLUMN upload_restart_attempts.evidence_json IS
    'Append-only fence findings: which provider multiparts were aborted and what each of the five object HEADs returned. Kept so a restart that later looks wrong can be reconstructed.';

-- 一个 clip 同时只能有一个未终结的 restart。并发的第二个请求会撞这个索引，
-- 而不是并行地去 provider 上 abort 同一批 multipart。
CREATE UNIQUE INDEX upload_restart_attempts_one_active_per_clip_idx
    ON upload_restart_attempts(clip_id)
    WHERE status IN ('pending', 'running', 'retry_wait');

-- 同一个过期 intent 只允许被成功重启一次；重放不会产生第二个新 attempt。
CREATE UNIQUE INDEX upload_restart_attempts_one_success_per_source_idx
    ON upload_restart_attempts(from_upload_intent_id)
    WHERE status = 'succeeded';

CREATE INDEX upload_restart_attempts_clip_idx
    ON upload_restart_attempts(clip_id, created_at DESC);

CREATE TRIGGER upload_restart_attempts_set_updated_at
BEFORE UPDATE ON upload_restart_attempts
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

INSERT INTO schema_migrations(version, name)
VALUES (12, 'fenced_upload_restart');

COMMIT;
