BEGIN;

-- The console is a separate service with a separate credential set, but it reads
-- and writes the same database, so operator identity lives in the same migration
-- ledger. Numbering stays global; only one package applies migrations.

CREATE TYPE operator_role AS ENUM ('viewer', 'operator', 'admin');
CREATE TYPE operator_status AS ENUM ('invited', 'active', 'suspended', 'tombstoned');
CREATE TYPE operator_challenge_purpose AS ENUM ('login_otp', 'invite', 'password_reset');

CREATE TABLE operators (
    id uuid PRIMARY KEY,
    email text NOT NULL CHECK (
        email = lower(btrim(email))
        AND position('@' IN email) > 1
        AND position('@' IN email) < length(email)
        AND length(email) <= 254
    ),
    password_hash text CHECK (password_hash IS NULL OR password_hash LIKE '$argon2id$%'),
    role operator_role NOT NULL DEFAULT 'viewer',
    status operator_status NOT NULL DEFAULT 'invited',
    failed_login_count integer NOT NULL DEFAULT 0 CHECK (failed_login_count >= 0),
    locked_until timestamptz,
    password_updated_at timestamptz,
    last_login_at timestamptz,
    suspended_at timestamptz,
    tombstoned_at timestamptz,
    created_by uuid REFERENCES operators(id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (status <> 'active' OR password_hash IS NOT NULL),
    CHECK (status <> 'suspended' OR suspended_at IS NOT NULL),
    CHECK (status <> 'tombstoned' OR tombstoned_at IS NOT NULL),
    CHECK (password_hash IS NULL OR password_updated_at IS NOT NULL)
);

COMMENT ON TABLE operators IS
    'Console administrators. Invite-only: there is no self-service registration endpoint.';
COMMENT ON COLUMN operators.password_hash IS
    'argon2id PHC string. NULL while the invite has not been completed.';

CREATE UNIQUE INDEX operators_email_key
    ON operators(email);
CREATE INDEX operators_status_idx
    ON operators(status, created_at DESC);

CREATE TABLE operator_email_challenges (
    id uuid PRIMARY KEY,
    operator_id uuid NOT NULL REFERENCES operators(id) ON DELETE RESTRICT,
    purpose operator_challenge_purpose NOT NULL,
    code_salt bytea NOT NULL CHECK (octet_length(code_salt) = 32),
    code_hash text NOT NULL CHECK (code_hash ~ '^[0-9a-f]{64}$'),
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    superseded_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at > created_at),
    CHECK (consumed_at IS NULL OR consumed_at >= created_at)
);

COMMENT ON TABLE operator_email_challenges IS
    'Single-use email challenges. Security comes from short TTL, single use, and attempt caps; a six digit code is not protected by hash strength alone.';

CREATE INDEX operator_email_challenges_operator_idx
    ON operator_email_challenges(operator_id, purpose, created_at DESC);
CREATE INDEX operator_email_challenges_open_idx
    ON operator_email_challenges(operator_id, purpose)
    WHERE consumed_at IS NULL AND superseded_at IS NULL;
CREATE INDEX operator_email_challenges_expiry_idx
    ON operator_email_challenges(expires_at)
    WHERE consumed_at IS NULL;

CREATE TABLE operator_sessions (
    id uuid PRIMARY KEY,
    operator_id uuid NOT NULL REFERENCES operators(id) ON DELETE RESTRICT,
    token_hash text NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    sudo_until timestamptz,
    expires_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    revoked_at timestamptz,
    revoke_reason text,
    ip_hash text CHECK (ip_hash IS NULL OR ip_hash ~ '^[0-9a-f]{64}$'),
    user_agent text CHECK (user_agent IS NULL OR length(user_agent) <= 256),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at > created_at)
);

COMMENT ON TABLE operator_sessions IS
    'Server-side sessions. The console needs immediate revocation on suspend, role change, and password change, which a stateless token cannot provide.';
COMMENT ON COLUMN operator_sessions.ip_hash IS
    'sha256 over the client address and a rotating salt. Raw addresses are never stored.';

CREATE INDEX operator_sessions_operator_idx
    ON operator_sessions(operator_id, created_at DESC);
CREATE INDEX operator_sessions_active_idx
    ON operator_sessions(expires_at)
    WHERE revoked_at IS NULL;

CREATE TABLE console_rate_limits (
    bucket text PRIMARY KEY CHECK (btrim(bucket) <> ''),
    window_started_at timestamptz NOT NULL,
    counter integer NOT NULL CHECK (counter >= 0),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

COMMENT ON TABLE console_rate_limits IS
    'Fixed-window counters shared by every Cloud Run instance. Buckets hold hashed subjects, never raw emails or addresses.';

CREATE INDEX console_rate_limits_sweep_idx
    ON console_rate_limits(window_started_at);

CREATE TRIGGER operators_set_updated_at
BEFORE UPDATE ON operators
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER operators_reject_delete
BEFORE DELETE ON operators
FOR EACH ROW EXECUTE FUNCTION reject_governance_delete();

-- Console list and filter paths are ordered by created_at DESC with a keyset
-- cursor. The ingest indexes lead with status columns, so the by-game, by-device,
-- and by-annotator browsers need their own composite indexes.
CREATE INDEX sessions_game_created_idx
    ON sessions(game_id, created_at DESC, id DESC);
CREATE INDEX sessions_device_created_idx
    ON sessions(device_id, created_at DESC, id DESC);
CREATE INDEX sessions_annotator_created_idx
    ON sessions(annotator_id, created_at DESC, id DESC)
    WHERE annotator_id IS NOT NULL;
CREATE INDEX sessions_created_idx
    ON sessions(created_at DESC, id DESC);
CREATE INDEX clips_created_idx
    ON clips(created_at DESC, id DESC);
CREATE INDEX clips_session_created_idx
    ON clips(session_id, created_at DESC, id DESC);
CREATE INDEX audit_events_actor_idx
    ON audit_events(actor_type, actor_id, occurred_at DESC)
    WHERE actor_id IS NOT NULL;
CREATE INDEX audit_events_occurred_idx
    ON audit_events(occurred_at DESC, id DESC);

INSERT INTO schema_migrations(version, name)
VALUES (5, 'console_operators');

COMMIT;
