-- 0022 · catalog schema v2 一次断代的服务端落点（docs/57 §3，批次二）。
--
-- 客户端与 authored bundle 已经断代：capture-profile + settings-profile 合成
-- 一本 recording-profile，引用去版本化，mode 由投递渠道决定而不再写在 manifest
-- 里。这一版把同一刀落到控制面上。
--
-- 未发版，直接改列改视图，不留兼容层（docs/57 §4）。
--
-- ── 存量影响：部署后必须重发 capture grant ──────────────────────────────
-- `capture_grants.capture_profile_version_id` 改名并改指 recording profile。
-- 存量 grant 的取值仍是它当初钉的那一行 capture_profile 的 UUID，而断代之后
-- 新会话钉的是 recording_profile 行——两者不可能相等，于是
-- `clip_trainability` 会对每一个新 clip 报 `capture_grant_scope_mismatch`。
-- 这是**故意的 fail-closed**：一份按旧口径签发的授权，不该自动覆盖新口径下
-- 的录制。部署后需要按新的 recording profile 版本重新签发 grant。
--
-- ── 三处与批次二任务书不同的取舍，需要 owner 复核 ──────────────────────
-- (1) `device_catalog_kind` / `mapping_catalog_kind` **保留**，只删 capture 与
--     settings 那两个。理由见下面 §2 的注释。
-- (2) certified 会话的 preflight 归属：见下面 §4 的注释。
-- (3) `catalog_releases` 的唯一性约束按「每游戏一份 bundle」重做，这一条任务书
--     里没有，但不改就发不出第二个游戏。见下面 §5 的注释。

BEGIN;

-- ── §1 catalog_kind 枚举：只加不删 ────────────────────────────────────
-- `capture_profile` 与 `settings_profile` 两个标签保留。registry 是不可变档案，
-- 已登记的 v1 条目仍然是 v1，改标签等于篡改它们当初的身份。
--
-- PG 15 允许在事务块里 ADD VALUE，代价是新值在本事务内不可使用。本迁移全程
-- 不使用 'recording_profile' 字面量（要动的 kind 列全部是删除，不是改默认值），
-- 所以这条限制不构成问题。
ALTER TYPE catalog_kind ADD VALUE IF NOT EXISTS 'recording_profile';

-- ── §2 sessions：两对目录列并一对 ─────────────────────────────────────
-- 视图依赖被删的列，先拆再建。
DROP VIEW clip_trainability;

-- trusted_catalog_versions 上补一条 (id, game_id) 唯一约束，好让下面的外键在
-- **不带 kind** 的前提下仍然钉住「这一行属于本会话这款游戏」。id 本身就是主键，
-- 所以这条约束不增加任何限制，只是给外键一个可引用的目标。
ALTER TABLE trusted_catalog_versions
    ADD CONSTRAINT trusted_catalog_versions_id_game_key UNIQUE (id, game_id);

ALTER TABLE sessions ADD COLUMN recording_catalog_version_id uuid;

-- 回填取 capture 那一列：grant 的 scope 对账历来比的就是它，取 settings 会让
-- 存量行的对账语义凭空变一次。存量行因此仍然指向一行 `capture_profile` kind
-- 的 registry 条目——这是如实记录，那些会话当初声明的确实是 v1 的两本册子。
UPDATE sessions SET recording_catalog_version_id = capture_catalog_version_id;

ALTER TABLE sessions
    ALTER COLUMN recording_catalog_version_id SET NOT NULL,
    ADD CONSTRAINT sessions_recording_catalog_version_fkey
        FOREIGN KEY (recording_catalog_version_id, game_id)
        REFERENCES trusted_catalog_versions(id, game_id) ON DELETE RESTRICT;

-- 旧的两对列连同它们的复合外键与 kind CHECK 一起走。DROP COLUMN 会自动带走
-- 涉及该列的约束，所以这里不点名那些名字——它们是 PostgreSQL 按 63 字符上限
-- 截出来的，猜不得（先例：0021）。
--
-- 为什么这里必须删 kind 列，而不是改成 `recording_catalog_kind`：
-- 那一列的存在形式是 `NOT NULL DEFAULT '<字面量>'` 加一条 `= '<字面量>'` 的
-- CHECK。合并之后**存量行指向 capture_profile、新行指向 recording_profile**，
-- 任何单一常量的 CHECK 都会在其中一边失败。要么删列，要么改写历史谎称存量
-- 会话当初声明的是 recording profile。选前者。
--
-- 为什么 device / mapping 的 kind 列**留着**（与任务书不同，需 owner 复核）：
-- 那两列的 kind 不随本次断代变化，常量 CHECK 依然成立，复合外键仍在数据库层
-- 挡住「会话的设备档案指向了一行 mapping」这类错配。0021 删 kind 列的前提是
-- 「那一列只为一条其保证已冗余的外键而存在」——对 device / mapping 不成立，
-- 删了就是净损失一条真实的完整性保证，换不到任何东西。
ALTER TABLE sessions
    DROP COLUMN capture_catalog_version_id,
    DROP COLUMN capture_catalog_kind,
    DROP COLUMN settings_catalog_version_id,
    DROP COLUMN settings_catalog_kind;

COMMENT ON COLUMN sessions.recording_catalog_version_id IS
    'The recording profile version this session declared (docs/57 §3.1: capture + settings merged into one book). Rows created before migration 0022 point at the legacy capture_profile entry they actually declared; the kind is deliberately not pinned here because the column legitimately spans both eras.';

-- ── §3 capture_grants：授权改指 recording profile ─────────────────────
ALTER TABLE capture_grants ADD COLUMN recording_profile_version_id uuid;

UPDATE capture_grants SET recording_profile_version_id = capture_profile_version_id;

ALTER TABLE capture_grants
    ALTER COLUMN recording_profile_version_id SET NOT NULL,
    ADD CONSTRAINT capture_grants_recording_profile_version_fkey
        FOREIGN KEY (recording_profile_version_id, game_id)
        REFERENCES trusted_catalog_versions(id, game_id) ON DELETE RESTRICT;

ALTER TABLE capture_grants
    DROP COLUMN capture_profile_version_id,
    DROP COLUMN capture_profile_kind;

COMMENT ON COLUMN capture_grants.recording_profile_version_id IS
    'Which recording profile version this grant authorises. Grants issued before migration 0022 still carry their old capture_profile pin and therefore will not match sessions recorded under a v2 catalog — that mismatch is intentional and requires re-issuing the grant, it is not a bug to work around.';

-- ── §4 准入词汇表：first_party_release 退役 ───────────────────────────
-- docs/57 §3.3 整条退役了 first-party 磁盘通道，客户端的 CatalogMode 收成
-- 两值（certified / dev_non_trainable），journal v15 的 preflight 状态也只剩
-- 一个 `dev_non_trainable_pass`。
--
-- ⚠ 需 owner 复核：certified 会话归到哪个 preflight family。
-- `preflight_result` 命名的是**准入家族**，不是 catalog mode——今天只存在
-- 「非可训练准入」这一个家族，certified 车道的录制仍然 fail-closed 卡在 P5
-- grant 门上、session.json 的 non_trainable 恒为 true。所以这里让两个 mode
-- 都要求 `dev_non_trainable_pass`，而不是给 certified 造一个它今天根本产不出
-- 的 `certified_pass`。等 certified 录制真正接上 P5，再加家族与配对。
-- 另一种读法是「certified 会话现在就该被服务端拒收」，那会让 CHECK 收成
-- 单值 `= 'dev_non_trainable'`。两种都自洽，取哪种是产品决定。
--
-- 收窄词汇表之前先点名存量：如果库里真有 first-party 行，下面的 CHECK 会以
-- 一句语焉不详的约束冲突失败。宁可在这里显式停住，把「有多少行、该怎么处置」
-- 交回给人——那是数据决定，不该由一条迁移替谁做主。
DO $$
DECLARE
    stale_sessions bigint;
    stale_runs bigint;
BEGIN
    SELECT count(*) INTO stale_sessions
      FROM sessions WHERE catalog_mode = 'first_party_release';
    SELECT count(*) INTO stale_runs
      FROM capture_runs WHERE preflight_result = 'first_party_release_pass';
    IF stale_sessions > 0 OR stale_runs > 0 THEN
        RAISE EXCEPTION
            'migration 0022 refuses to narrow the admission vocabulary while first-party rows exist: % session(s), % capture run(s). The first-party lane retired with docs/57 §3.3; decide explicitly whether those rows are archived, reclassified, or deleted before re-running.',
            stale_sessions, stale_runs;
    END IF;
END $$;

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
            schema_version = 'dm-local-dev-session-2'
            AND catalog_id IS NOT NULL
            AND btrim(catalog_id) <> ''
            AND catalog_version IS NOT NULL
            AND btrim(catalog_version) <> ''
            AND catalog_mode IS NOT NULL
            AND catalog_mode IN ('dev_non_trainable', 'certified')
            AND catalog_manifest_sha256 IS NOT NULL
            AND game_profile_sha256 IS NOT NULL
        )
    );

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

    IF parent_schema_version = 'dm-local-dev-session-2' THEN
        -- 家族只剩一个，配对退化成「mode 必须是已知的两值之一」加「结果必须是
        -- 那个家族」。仍然 fail-closed：未知 mode 直接抛，不放行。
        IF parent_catalog_mode NOT IN ('dev_non_trainable', 'certified') THEN
            RAISE EXCEPTION 'session v2 capture run has an unsupported parent catalog mode'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.preflight_schema_version IS DISTINCT FROM 'dm-preflight-evidence-1'
           OR NEW.preflight_id IS NULL
           OR btrim(NEW.preflight_id) = ''
           OR NEW.preflight_result IS DISTINCT FROM 'dev_non_trainable_pass'
           OR NEW.preflight_evidence_sha256 IS NULL THEN
            RAISE EXCEPTION 'session v2 capture run requires the preflight evidence of its own lane'
                USING ERRCODE = '23514';
        END IF;
    ELSIF parent_schema_version = 'dm-local-dev-session-1' THEN
        IF NEW.preflight_schema_version IS NOT NULL
           OR NEW.preflight_id IS NOT NULL
           OR NEW.preflight_result IS NOT NULL
           OR NEW.preflight_evidence_sha256 IS NOT NULL THEN
            RAISE EXCEPTION 'legacy session v1 capture run cannot carry v2 preflight evidence'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported parent session schema version'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

-- ── §5 catalog_releases：manifest 不再自称 mode，且每游戏一份 bundle ──
-- (a) `catalog_mode` 列走人。mode 由投递渠道决定（编译内嵌 = dev、签名渠道 =
--     certified），manifest 自称的那一份是冗余的第二事实源（docs/57 §3.3）。
--     DROP COLUMN 会自动带走那条把 channel / catalog_id / catalog_mode 三者
--     绑死成「两支舰队」的 CHECK。
--
-- (b) ⚠ 任务书之外，但不改就发不出第二个游戏：
--     原来的模型是「每个渠道一份大目录」（catalog_id 恒为
--     dataminer-dev-fleet / dataminer-certified-fleet），所以
--     `UNIQUE (channel, catalog_version)` 与「每渠道只能有一份 published」
--     都成立。断代后是**每游戏一份 bundle**，14 份 bundle 共用同一个
--     catalog_version（2026-07-29.0），发第二个游戏就会撞唯一约束；
--     one-published-per-channel 更是只允许全渠道有一份在线。
--     两条约束的粒度都下沉到 (channel, catalog_id)。
ALTER TABLE catalog_releases DROP COLUMN catalog_mode;

ALTER TABLE catalog_releases
    DROP CONSTRAINT catalog_releases_channel_catalog_version_key;
ALTER TABLE catalog_releases
    ADD CONSTRAINT catalog_releases_channel_catalog_version_key
        UNIQUE (channel, catalog_id, catalog_version);

DROP INDEX catalog_releases_one_published_per_channel;
CREATE UNIQUE INDEX catalog_releases_one_published_per_catalog
    ON catalog_releases(channel, catalog_id)
    WHERE status = 'published';

-- (c) `publish_catalog_release` 有同一个「每渠道一份大目录」的假设，而且这一处
--     更凶：它按 **channel 单独** 找上一份 published 去下架。每游戏一份 bundle
--     之后，发布 A 游戏会把 B 游戏的在线目录顺手改成 superseded——机器还在按
--     它录，目录却在库里被判了下架。作用域跟着降到 (channel, catalog_id)。
--     顺带把册子数从五改四。函数体其余部分逐字保留。
CREATE OR REPLACE FUNCTION publish_catalog_release(p_release_id uuid, p_actor text)
RETURNS uuid
LANGUAGE plpgsql
AS $$
DECLARE
    target_channel text;
    target_catalog_id text;
    target_status catalog_release_status;
    previous_release_id uuid;
    entry_count integer;
    kind_count integer;
BEGIN
    IF p_actor IS NULL OR btrim(p_actor) = '' THEN
        RAISE EXCEPTION 'catalog release publisher is required';
    END IF;

    SELECT channel, catalog_id, status
      INTO target_channel, target_catalog_id, target_status
      FROM catalog_releases
     WHERE id = p_release_id
     FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'catalog release does not exist';
    END IF;
    IF target_status <> 'draft'::catalog_release_status THEN
        RAISE EXCEPTION 'only a draft catalog release can be published';
    END IF;

    SELECT count(*), count(DISTINCT kind)
      INTO entry_count, kind_count
      FROM catalog_release_entries
     WHERE release_id = p_release_id;
    IF entry_count < 4 OR kind_count <> 4 THEN
        RAISE EXCEPTION 'catalog release must contain all four profile kinds';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM catalog_release_entries entry
          JOIN trusted_catalog_versions trusted
            ON trusted.id = entry.trusted_catalog_version_id
          LEFT JOIN catalog_payloads payload
            ON payload.trusted_catalog_version_id = trusted.id
         WHERE entry.release_id = p_release_id
           AND (trusted.status = 'revoked' OR payload.trusted_catalog_version_id IS NULL)
    ) THEN
        RAISE EXCEPTION 'catalog release contains a revoked or non-publishable profile';
    END IF;

    SELECT id
      INTO previous_release_id
      FROM catalog_releases
     WHERE channel = target_channel
       AND catalog_id = target_catalog_id
       AND status = 'published'
     FOR UPDATE;

    IF previous_release_id IS NOT NULL THEN
        UPDATE catalog_releases
           SET status = 'superseded',
               superseded_at = transaction_timestamp()
         WHERE id = previous_release_id;
    END IF;

    UPDATE catalog_releases
       SET status = 'published',
           published_by = p_actor,
           published_at = transaction_timestamp()
     WHERE id = p_release_id;

    INSERT INTO audit_events(
        event_id, actor_type, actor_id, action, entity_type, entity_id, details
    ) VALUES (
        gen_random_uuid(), 'operator', p_actor, 'catalog.release_published',
        'catalog_release', p_release_id::text,
        jsonb_build_object(
            'channel', target_channel,
            'catalog_id', target_catalog_id,
            'previous_release_id', previous_release_id,
            'entry_count', entry_count
        )
    );

    RETURN p_release_id;
END;
$$;

-- 不可变性触发器里那一行 catalog_mode 引用跟着走。其余判据一字不改。
CREATE OR REPLACE FUNCTION protect_catalog_release()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.channel IS DISTINCT FROM OLD.channel
        OR NEW.catalog_id IS DISTINCT FROM OLD.catalog_id
        OR NEW.catalog_version IS DISTINCT FROM OLD.catalog_version
        OR NEW.manifest_bytes IS DISTINCT FROM OLD.manifest_bytes
        OR NEW.envelope_bytes IS DISTINCT FROM OLD.envelope_bytes
        OR NEW.envelope_sha256 IS DISTINCT FROM OLD.envelope_sha256
        OR NEW.signing_key_id IS DISTINCT FROM OLD.signing_key_id
        OR NEW.created_by IS DISTINCT FROM OLD.created_by
        OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'catalog release identity and signed bytes are immutable';
    END IF;
    IF OLD.published_by IS NOT NULL
        AND (
            NEW.published_by IS DISTINCT FROM OLD.published_by
            OR NEW.published_at IS DISTINCT FROM OLD.published_at
        ) THEN
        RAISE EXCEPTION 'catalog release publication attribution is immutable';
    END IF;
    IF OLD.superseded_at IS NOT NULL
        AND NEW.superseded_at IS DISTINCT FROM OLD.superseded_at THEN
        RAISE EXCEPTION 'catalog release superseded timestamp is immutable';
    END IF;

    IF OLD.status = 'draft' AND NEW.status IN ('published', 'revoked') THEN
        RETURN NEW;
    END IF;
    IF OLD.status = 'published' AND NEW.status IN ('superseded', 'revoked') THEN
        RETURN NEW;
    END IF;
    IF OLD.status = 'superseded' AND NEW.status = 'revoked' THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION 'invalid catalog release state transition: % -> %', OLD.status, NEW.status;
END;
$$;

-- ── §6 clip_trainability 重建 ────────────────────────────────────────
-- 与 0018 的版本相比只有四处变化，其余逐字保留：
--   * cp / sp 两个 join 合成一个 rp；
--   * `capture_profile_unapproved` + `settings_profile_unapproved` 两个 blocker
--     合成 `recording_profile_unapproved`；
--   * grant scope 对账改用改名后的两列；
--   * view_scale 的存在性判据从 sp.snapshot 改读 rp.snapshot——view_scale 块
--     随 settings profile 一起搬进了 recording profile（docs/57 §3.1）。
-- 注意 `artifact_set_incomplete` 那个 `<> 5` 不动：它数的是 clip 的五件产物
--（video/frames/events/meta/manifest，artifact_kind 枚举），与目录册子数无关。
CREATE VIEW clip_trainability AS
WITH artifact_rollup AS (
    SELECT
        clip_id,
        count(*) AS object_count,
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
            CASE WHEN con.id IS NOT NULL AND con.status <> 'active' THEN 'consent_inactive' END,
            CASE WHEN con.id IS NOT NULL AND NOT con.allows_trainable_use THEN 'consent_disallows_trainable' END,
            CASE WHEN con.expires_at IS NOT NULL AND con.expires_at <= now() THEN 'consent_expired' END,
            CASE WHEN con.id IS NOT NULL AND con.retention_state <> 'retained' THEN 'consent_purge_active' END,
            CASE WHEN g.id IS NULL THEN 'missing_capture_grant' END,
            CASE WHEN g.id IS NOT NULL AND g.status <> 'active' THEN 'capture_grant_inactive' END,
            CASE WHEN g.id IS NOT NULL AND NOT g.allows_trainable THEN 'capture_grant_disallows_trainable' END,
            CASE
                WHEN g.id IS NOT NULL
                    AND NOT (s.started_at >= g.valid_from AND s.started_at < g.valid_until)
                THEN 'capture_outside_grant_window'
            END,
            CASE
                WHEN g.id IS NOT NULL
                    AND (g.game_id <> s.game_id
                         OR g.recording_profile_version_id <> s.recording_catalog_version_id)
                THEN 'capture_grant_scope_mismatch'
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
            -- #278 把身份钉从 authoring 硬门降为选填证据；把关改在这里：
            -- 客户端绑定时实测的指纹必须已被治理批准。
            CASE
                WHEN age.game_id IS NULL THEN 'executable_unapproved'
            END,
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
            CASE WHEN COALESCE(ar.object_count, 0) <> 5 THEN 'artifact_set_incomplete' END,
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
    LEFT JOIN capture_grants g ON g.id = s.capture_grant_id
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
    'Read-only, fail-closed server derivation. Five verified artifacts, consent, grant, catalog (device + recording + mapping), approved executable build, view-scale calibration, QA, and retention gates must all pass.';

INSERT INTO schema_migrations(version, name)
VALUES (22, 'recording_profile_merge');

COMMIT;
