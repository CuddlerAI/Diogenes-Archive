-- 0010 · session-root artifact 上传（#57 任务3，docs/47）
--
-- clip 五件套（video/frames/events/meta/manifest）之外，每个 session 还有一批
-- session 根产物——7 份 snapshot（session/catalog/device/capture/game/settings/
-- mapping profile）+ markers.jsonl。它们**不进 clip 协议**（doc 14 §1 的硬边界），
-- 这里给独立、带版本的 session-artifact 契约：server-owned 预签名 PUT + 服务端
-- getObject 重算 SHA-256 校验。产物小（JSON/JSONL），单 PUT 即可，无需 multipart。
--
-- R2 布局：raw/v1/non-trainable/<deployment>/<sessionId>/session/<file>。

BEGIN;

CREATE TYPE session_artifact_kind AS ENUM (
    'session',
    'catalog',
    'device_profile',
    'capture_profile',
    'game_profile',
    'settings_profile',
    'mapping',
    'markers'
);

CREATE TABLE session_artifacts (
    id uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
    kind session_artifact_kind NOT NULL,
    object_key text NOT NULL CHECK (btrim(object_key) <> ''),
    content_type text NOT NULL CHECK (btrim(content_type) <> ''),
    expected_sha256 sha256_digest NOT NULL,
    expected_size_bytes bigint NOT NULL CHECK (expected_size_bytes > 0),
    ingest_state ingest_state NOT NULL DEFAULT 'pending',
    verification_state verification_state NOT NULL DEFAULT 'unverified',
    stored_at timestamptz,
    verified_at timestamptz,
    rejected_reason text,
    revision integer NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    -- Same fail-closed companion-timestamp invariants as clips.
    CHECK (ingest_state <> 'stored' OR stored_at IS NOT NULL),
    CHECK (verification_state <> 'verified' OR verified_at IS NOT NULL),
    CHECK (verification_state <> 'rejected' OR rejected_reason IS NOT NULL),
    -- One immutable row per (session, kind); a re-declare must match or bump revision.
    UNIQUE (session_id, kind),
    UNIQUE (object_key)
);

COMMENT ON TABLE session_artifacts IS
    'Session-root snapshots + markers.jsonl. Server-owned presigned PUT, server-verified SHA-256. Audit/reproducibility evidence; does not gate clip_trainability.';

CREATE INDEX session_artifacts_by_session ON session_artifacts (session_id);

CREATE TRIGGER session_artifacts_set_updated_at
BEFORE UPDATE ON session_artifacts
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

INSERT INTO schema_migrations(version, name)
VALUES (10, 'session_artifacts');

COMMIT;
