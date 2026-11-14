-- dataminer-mirror · D1 schema 0001
--
-- 这个库保存的是「某台采集机上次报告说本地有什么」，不是任何权威事实。
-- 采集事实的唯一权威是客户端本地的 journal.sqlite3；摄取与治理事实的唯一权威是
-- Cloud SQL。本库中的任何一行都不得参与 trainability、purge 授权或上传许可判断。
-- 详见 docs/13-local-journal-cloud-mirror.md §0 与 §4。
--
-- 所有表统一 mirror_ 前缀：让「查错库」在写 SQL 的时候就长得不对劲。
--
-- 刻意不建外键：投影按 outbox seq 顺序到达，但一批可能只覆盖部分实体，
-- 且客户端可以在 outbox 清理后从任意 seq 续报。外键会把「事件先于父实体到达」
-- 变成整批失败，而这里没有任何需要引用完整性保护的权威数据。

-- 凭据镜像表。Worker 够不到 Cloud SQL，因此这里保存
-- devices.credential_fingerprint 的投影，由 console 在签发/吊销时写入。
-- 与本库其余部分一样是非权威副本：它不签发凭据，只判断某个 fingerprint
-- 当前是否还该被放行。表为空时整个服务 fail closed，而不是放行。
--
-- 口径必须与摄取面 S0 完全一致：opaque per-device 凭据，sha256(bearer)
-- 匹配 fingerprint，无共享密钥，令牌不自述 device id。
CREATE TABLE mirror_device_credentials (
    credential_fingerprint TEXT    PRIMARY KEY,
    device_id              TEXT    NOT NULL,
    -- console 侧 outbox 的单调序号。发布方可能并发或重试，乱序到达的旧行
    -- 必须被 upsert 守卫挡住，否则一次重投就能把「已停用」改回「活跃」。
    source_seq             INTEGER NOT NULL,
    -- 与 Cloud SQL devices.status 同枚举：pending/active/suspended/retired
    device_status          TEXT    NOT NULL
                                  CHECK (device_status IN ('pending', 'active', 'suspended', 'retired')),
    -- annotator 停用要一次带走它名下整个设备队列，因此活性投影到每一行；
    -- 未绑定 annotator 的设备（今天 staging 设备的样子）写 'unbound'。
    annotator_status       TEXT    NOT NULL DEFAULT 'unbound'
                                  CHECK (annotator_status IN ('unbound', 'active', 'suspended', 'tombstoned')),
    updated_at             INTEGER NOT NULL
) STRICT;

-- 一个 device 可以先后持有多份凭据（轮换），但一份凭据只属于一个 device——
-- 主键已经保证后者。这个索引服务的是吊销：按设备一次撤掉全部凭据。
CREATE INDEX mirror_device_credentials_device ON mirror_device_credentials (device_id);

CREATE TABLE mirror_devices (
    device_id       TEXT    PRIMARY KEY,
    first_seen_at   INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    -- 已接收的最大 seq。客户端丢失本地 cursor 时用它恢复。
    acked_seq       INTEGER NOT NULL DEFAULT 0,
    client_version  TEXT,
    -- 最近一次 heartbeat 的脱敏摘要
    disk_free_bytes INTEGER,
    recording_state TEXT
) STRICT;

-- append-only 事件日志。(device_id, seq) 是幂等键——重发同一批是 no-op。
CREATE TABLE mirror_events (
    device_id    TEXT    NOT NULL,
    seq          INTEGER NOT NULL,
    entity       TEXT    NOT NULL,
    entity_id    TEXT    NOT NULL,
    observed_at  INTEGER NOT NULL,
    received_at  INTEGER NOT NULL,
    payload_json TEXT    NOT NULL,
    PRIMARY KEY (device_id, seq)
) STRICT;

CREATE INDEX mirror_events_entity   ON mirror_events (entity, entity_id);
CREATE INDEX mirror_events_received ON mirror_events (received_at);

-- 以下两张是可从 mirror_events 完全重建的当前态投影。
-- source_seq 保证乱序或重放的旧事件不会把状态改回去。

CREATE TABLE mirror_sessions (
    device_id       TEXT    NOT NULL,
    local_session_id TEXT   NOT NULL,
    source_seq      INTEGER NOT NULL,
    state           TEXT    NOT NULL,
    game_slug       TEXT,
    started_at      INTEGER,
    ended_at        INTEGER,
    run_count       INTEGER NOT NULL DEFAULT 0,
    segment_count   INTEGER NOT NULL DEFAULT 0,
    ready_segment_count INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (device_id, local_session_id)
) STRICT;

CREATE INDEX mirror_sessions_state ON mirror_sessions (state);

CREATE TABLE mirror_segments (
    device_id        TEXT    NOT NULL,
    local_session_id TEXT    NOT NULL,
    local_run_id     TEXT    NOT NULL,
    segment_index    INTEGER NOT NULL,
    source_seq       INTEGER NOT NULL,
    -- 本地完整性观察值，取自客户端 journal：ready/corrupt/quarantined/deleted
    local_state      TEXT    NOT NULL,
    n_frames         INTEGER,
    total_bytes      INTEGER,
    -- 客户端是否已把这一段登记到 Cloud SQL。它只是客户端的说法，
    -- 「远端是否真的 verified」必须去 Cloud SQL 查，不能读这一列。
    client_upload_state TEXT NOT NULL DEFAULT 'unknown',
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (device_id, local_session_id, local_run_id, segment_index)
) STRICT;

CREATE INDEX mirror_segments_pending
    ON mirror_segments (device_id, local_state, client_upload_state);
