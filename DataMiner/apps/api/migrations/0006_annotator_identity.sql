BEGIN;

-- Annotator identity (docs/11). Operators (0005) and annotators are two
-- separate identity systems on purpose: an operator signs credentials, revokes
-- grants, and triggers purges; an annotator may only upload data its own device
-- produced. Sharing one table would make a mistaken role column a privilege
-- escalation, so the separation is physical (11 §4.1).
--
-- Numbering stays global: only one package applies migrations.

CREATE TYPE invite_status AS ENUM ('active', 'revoked');
CREATE TYPE annotator_credential_status AS ENUM ('pending_verification', 'active', 'suspended');
CREATE TYPE annotator_challenge_purpose AS ENUM ('signup_verify', 'login_otp', 'password_reset');

-- Invite codes are stored as normalized plaintext, which is the one place in
-- this database where a secret-looking value is not hashed. The reasoning is in
-- 11 §12.2 and it is deliberate, not an oversight: an invite code is a
-- registration gate, not a credential. Holding one still requires a reachable
-- mailbox, a password, and accepting the terms; the actual credentials (the
-- password and the device token) remain hashed. Operations needs to read codes
-- back to answer "who did we send this to, and did they use it?", which hashing
-- makes impossible.
--
-- Compensating controls, all of which must stay in place:
--   * single use (max_uses defaults to 1),
--   * 60 bits of entropy from a de-ambiguated alphabet,
--   * mandatory email verification before the account activates,
--   * per-IP and per-email rate limits on the signup endpoint,
--   * showing a full code in the console writes an audit event.
CREATE TABLE invite_codes (
    -- Normalized form: `DM` + 12 characters. The display form `DM-XXXX-XXXX-XXXX`
    -- is a rendering concern. Crockford-ish alphabet with no 0/O/1/I/L, because
    -- a human retypes this from a message into a desktop app.
    code text PRIMARY KEY CHECK (code ~ '^DM[23456789ABCDEFGHJKMNPQRSTUVWXYZ]{12}$'),
    note text CHECK (note IS NULL OR btrim(note) <> ''),
    max_uses integer NOT NULL DEFAULT 1 CHECK (max_uses >= 1),
    used_count integer NOT NULL DEFAULT 0 CHECK (used_count >= 0),
    status invite_status NOT NULL DEFAULT 'active',
    expires_at timestamptz,
    -- Denormalized first-redemption columns, written with COALESCE so they only
    -- ever record the first use. The authoritative relation is
    -- annotator_invite_uses; these exist so the console list view does not need
    -- a join per row.
    used_at timestamptz,
    used_by_annotator_id uuid REFERENCES annotators(id) ON DELETE RESTRICT,
    created_by uuid NOT NULL REFERENCES operators(id) ON DELETE RESTRICT,
    revoked_by uuid REFERENCES operators(id) ON DELETE RESTRICT,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (used_count <= max_uses),
    CHECK (status <> 'revoked' OR revoked_at IS NOT NULL),
    CHECK ((used_at IS NULL) = (used_by_annotator_id IS NULL)),
    CHECK ((used_at IS NULL) = (used_count = 0))
);

COMMENT ON TABLE invite_codes IS
    'Registration gate, not a credential: normalized plaintext is the primary key so operations can match a code to the person it was issued to. See docs/11 §12.2.';
COMMENT ON COLUMN invite_codes.used_by_annotator_id IS
    'First redemption only. annotator_invite_uses is the authoritative code-to-annotator relation.';

CREATE INDEX invite_codes_status_idx
    ON invite_codes(status, created_at DESC);
CREATE INDEX invite_codes_open_idx
    ON invite_codes(created_at DESC)
    WHERE status = 'active' AND used_count = 0;

-- One row per redemption. Built now rather than added later even though every
-- code is single-use today: this is the "invite code -> user" relation the
-- console has to show, it costs one row per code, and adding it after the fact
-- would mean a backfill whose predicates have to mirror the redemption path
-- exactly to avoid a used_count with no matching row (11 §5.3).
CREATE TABLE annotator_invite_uses (
    id uuid PRIMARY KEY,
    code text NOT NULL REFERENCES invite_codes(code) ON DELETE RESTRICT,
    annotator_id uuid NOT NULL REFERENCES annotators(id) ON DELETE RESTRICT,
    used_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    ip_hash text CHECK (ip_hash IS NULL OR ip_hash ~ '^[0-9a-f]{64}$'),
    user_agent text CHECK (user_agent IS NULL OR length(user_agent) <= 256),
    UNIQUE (code, annotator_id)
);

COMMENT ON COLUMN annotator_invite_uses.ip_hash IS
    'sha256 over the client address and a rotating salt. Raw addresses are never stored.';

CREATE INDEX annotator_invite_uses_annotator_idx
    ON annotator_invite_uses(annotator_id, used_at DESC);

-- Credentials live beside annotators rather than inside them.
-- `annotators.external_subject_hash` is reserved for a future external identity
-- source (10 §13 open question 4); putting a self-hosted password's
-- sha256(email) there would bind two unrelated things to one column and force a
-- data migration the day Steam or Google sign-in arrives (11 §6.2).
CREATE TABLE annotator_credentials (
    annotator_id uuid PRIMARY KEY REFERENCES annotators(id) ON DELETE RESTRICT,
    email text NOT NULL CHECK (
        email = lower(btrim(email))
        AND position('@' IN email) > 1
        AND position('@' IN email) < length(email)
        AND length(email) <= 254
    ),
    password_hash text NOT NULL CHECK (password_hash LIKE '$argon2id$%'),
    status annotator_credential_status NOT NULL DEFAULT 'pending_verification',
    email_verified_at timestamptz,
    failed_login_count integer NOT NULL DEFAULT 0 CHECK (failed_login_count >= 0),
    locked_until timestamptz,
    password_updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    last_login_at timestamptz,
    suspended_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    -- D5 and D6 expressed at the table level: an account cannot be active
    -- without a verified address, and verification is the only gate — no
    -- separate operator approval step, because issuing the invite was already
    -- the authorization decision (11 §12.3).
    CHECK (status <> 'active' OR email_verified_at IS NOT NULL),
    CHECK (status <> 'suspended' OR suspended_at IS NOT NULL)
);

CREATE UNIQUE INDEX annotator_credentials_email_key
    ON annotator_credentials(email);

CREATE TABLE annotator_email_challenges (
    id uuid PRIMARY KEY,
    annotator_id uuid NOT NULL REFERENCES annotators(id) ON DELETE RESTRICT,
    purpose annotator_challenge_purpose NOT NULL,
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

COMMENT ON TABLE annotator_email_challenges IS
    'Single-use email challenges, same discipline as operator_email_challenges: short TTL, single use, attempt caps. A six digit code is not protected by hash strength alone.';

CREATE INDEX annotator_email_challenges_open_idx
    ON annotator_email_challenges(annotator_id, purpose)
    WHERE consumed_at IS NULL AND superseded_at IS NULL;
CREATE INDEX annotator_email_challenges_expiry_idx
    ON annotator_email_challenges(expires_at)
    WHERE consumed_at IS NULL;

-- Server-side sessions for the same reason operators have them (10 §12.7):
-- suspending a user, changing a password, or unbinding a device has to take
-- effect on the next request, which a stateless token cannot do. No sudo_until
-- column — end users have no high-risk operations to elevate into.
CREATE TABLE annotator_sessions (
    id uuid PRIMARY KEY,
    annotator_id uuid NOT NULL REFERENCES annotators(id) ON DELETE RESTRICT,
    token_hash text NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    expires_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    revoked_at timestamptz,
    revoke_reason text,
    ip_hash text CHECK (ip_hash IS NULL OR ip_hash ~ '^[0-9a-f]{64}$'),
    user_agent text CHECK (user_agent IS NULL OR length(user_agent) <= 256),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at > created_at)
);

CREATE INDEX annotator_sessions_annotator_idx
    ON annotator_sessions(annotator_id, created_at DESC);
CREATE INDEX annotator_sessions_active_idx
    ON annotator_sessions(expires_at)
    WHERE revoked_at IS NULL;

-- D4: a user may bind any number of devices. That is already the shape of
-- devices.annotator_id (nullable, no unique constraint), so no new table and
-- deliberately no upper bound, no counter, and no trigger. The risk an
-- unbounded fleet creates — a stolen account enrolling devices without limit —
-- is answered in the ingest gate, which joins annotators and refuses the whole
-- fleet the moment the annotator is suspended (11 §12.5).
--
-- The console device list and the client's own "my devices" view both order by
-- recency within one annotator, which the ingest indexes (status-leading) do
-- not serve.
CREATE INDEX devices_annotator_created_idx
    ON devices(annotator_id, created_at DESC, id DESC)
    WHERE annotator_id IS NOT NULL;

-- Client behaviour telemetry. Deliberately NOT audit_events: that table is
-- documented as `retained after object purge`, which is correct for a
-- governance trail ("who deleted what" must survive the deletion) and wrong for
-- telemetry, which has to disappear when the user withdraws consent. The two
-- retention semantics are incompatible, so they cannot share a table (11 §6.4).
--
-- Purge coverage does not need a new purge_scope: consent-scoped purge already
-- has the consent id, and these rows carry it. The draft design proposed
-- extending the enum, which would have required rewriting the purge_jobs CHECK
-- that binds each scope to its own mutually exclusive foreign key — a careful
-- invariant, changed for no benefit.
--
-- Known gap, recorded rather than hidden: events emitted during signup and
-- enrollment predate any consent, so consent_id is NULL and consent purge
-- cannot reach them. Those rows fall back to annotator_id (cleared when the
-- annotator is tombstoned) plus a TTL fixed in S6.
CREATE TABLE client_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Client-generated, so a retry after a timeout is deduplicated by the
    -- unique constraint rather than by the client guessing whether it landed.
    event_id uuid NOT NULL UNIQUE,
    device_id uuid NOT NULL REFERENCES devices(id) ON DELETE RESTRICT,
    annotator_id uuid REFERENCES annotators(id) ON DELETE RESTRICT,
    consent_id uuid REFERENCES consents(id) ON DELETE RESTRICT,
    session_id uuid REFERENCES sessions(id) ON DELETE RESTRICT,
    occurred_at timestamptz NOT NULL,
    received_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    kind text NOT NULL CHECK (btrim(kind) <> ''),
    severity text NOT NULL CHECK (severity IN ('debug', 'info', 'warn', 'error')),
    client_version text NOT NULL CHECK (btrim(client_version) <> ''),
    details jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(details) = 'object'),
    retention_state retention_state NOT NULL DEFAULT 'retained',
    purged_at timestamptz,
    CHECK (retention_state <> 'purged' OR purged_at IS NOT NULL),
    -- A consent always implies the annotator it belongs to, mirroring the same
    -- staged constraint on sessions.
    CHECK (consent_id IS NULL OR annotator_id IS NOT NULL)
);

COMMENT ON TABLE client_events IS
    'Client behaviour telemetry. Purgeable by design, unlike audit_events which survives purge. See docs/11 §6.4 and §8.';
COMMENT ON COLUMN client_events.occurred_at IS
    'Client clock, unverified. received_at is the server fact; compare the two rather than trusting occurred_at.';

CREATE INDEX client_events_device_idx
    ON client_events(device_id, occurred_at DESC, id DESC);
CREATE INDEX client_events_annotator_idx
    ON client_events(annotator_id, occurred_at DESC, id DESC)
    WHERE annotator_id IS NOT NULL;
CREATE INDEX client_events_kind_idx
    ON client_events(kind, occurred_at DESC, id DESC);
-- Consent-scoped purge sweeps by consent and skips rows it already cleared.
CREATE INDEX client_events_consent_purge_idx
    ON client_events(consent_id)
    WHERE consent_id IS NOT NULL AND retention_state <> 'purged';

CREATE TRIGGER invite_codes_set_updated_at
BEFORE UPDATE ON invite_codes
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER annotator_credentials_set_updated_at
BEFORE UPDATE ON annotator_credentials
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

-- Same tombstone-only discipline the governance tables already have.
CREATE TRIGGER invite_codes_reject_delete
BEFORE DELETE ON invite_codes
FOR EACH ROW EXECUTE FUNCTION reject_governance_delete();

CREATE TRIGGER annotator_credentials_reject_delete
BEFORE DELETE ON annotator_credentials
FOR EACH ROW EXECUTE FUNCTION reject_governance_delete();

INSERT INTO schema_migrations(version, name)
VALUES (6, 'annotator_identity');

COMMIT;
