-- 0007 · 设备凭据 → D1 观测镜像的发布 outbox
--
-- dataminer-mirror（Cloudflare Worker + D1，见 docs/13）按 sha256(bearer) 认凭据，
-- 但 Worker 够不到 Cloud SQL。因此 D1 里保存一张 mirror_device_credentials 投影，
-- 由 console 在签发/轮换/停用/退役设备、以及停用 annotator 时投递过去。
--
-- 为什么是 outbox 而不是在 console 里直接调 D1 HTTP API：
-- 跨系统写不可能与 SQL 事务原子提交。直接调用意味着「Cloud SQL 写成功、D1 调用失败」
-- 会静默留下一台停用了却仍能写镜像的设备，而且没有任何地方记得要重试。
-- outbox 行与业务行在**同一条语句**里写入（可写 CTE，PostgreSQL 单语句即原子），
-- 之后由 publisher 排空；失败就是「还没发」，不是「丢了」。
--
-- 版本号跳过 0006：那个号被在飞的标注用户身份 migration 占用（PR #40）。
-- migration runner 按 version 排序执行，留空号不影响。

BEGIN;

CREATE TABLE mirror_credential_outbox (
    -- 单调序号，同时是 D1 侧 upsert 的 source_seq 守卫值：
    -- 发布方会并发、会重试，乱序到达的旧行必须被挡住，否则一次重投
    -- 就能把「已停用」改回「活跃」。
    seq                    bigserial PRIMARY KEY,
    device_id              uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    credential_fingerprint text NOT NULL CHECK (credential_fingerprint ~ '^[0-9a-f]{64}$'),
    device_status          device_status NOT NULL,
    -- 未绑定 annotator 的设备写 'unbound'，与 D1 侧默认值一致。
    annotator_status       text NOT NULL CHECK (annotator_status IN ('unbound', 'active', 'suspended', 'tombstoned')),
    -- 轮换时置真：publisher 在 upsert 新凭据后，把该设备其余凭据 retire。
    retire_others          boolean NOT NULL DEFAULT false,
    enqueued_at            timestamptz NOT NULL DEFAULT transaction_timestamp(),
    published_at           timestamptz,
    attempt_count          integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    available_at           timestamptz NOT NULL DEFAULT transaction_timestamp(),
    last_error             text,
    CHECK (published_at IS NULL OR last_error IS NULL)
);

COMMENT ON TABLE mirror_credential_outbox IS
    'Durable publish queue for the non-authoritative D1 credential mirror. Rows are written in the same statement as the device or annotator change; a publisher drains them. Nothing here gates ingestion — apps/api reads devices.credential_fingerprint directly.';

-- publisher 只关心「还没发、且已到可重试时间」的行，按 seq 顺序排空。
CREATE INDEX mirror_credential_outbox_pending
    ON mirror_credential_outbox (available_at, seq)
    WHERE published_at IS NULL;

INSERT INTO schema_migrations(version, name)
VALUES (7, 'mirror_credential_outbox');

COMMIT;
