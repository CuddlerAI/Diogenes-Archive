BEGIN;

-- Annotator claim links + contact profile (docs/34).
--
-- Two decisions land here, both ratified 2026-07-27 (docs/34 decision record):
--
--   * `account_claim` — an operator-issued, single-use set-password link. It
--     powers operator-initiated registration ("代登记") and operator-triggered
--     password resets. The challenge row stores the sha256 of a 43-character
--     opaque URL token, exactly like the operator invite/reset links in
--     `operator_email_challenges`; which flow a claim belongs to is decided at
--     confirmation time by the credential status (pending_verification →
--     activate, active → reset), never by a client-supplied field.
--
--   * `login_otp` is removed. It was defined in 0006 but never used by any
--     flow — annotator logins deliberately carry no per-login code (11 §7.3) —
--     and docs/34 decision ⑥ retires it instead of leaving a value that looks
--     like a capability. Postgres cannot drop a single enum value, so the type
--     is rebuilt; there are no stored `login_otp` rows for the USING cast to
--     trip over, and the migration test pins that assumption.
--
-- Numbering stays global: only one package applies migrations.

CREATE TYPE annotator_challenge_purpose_v2 AS ENUM (
    'signup_verify',
    'password_reset',
    'account_claim'
);

ALTER TABLE annotator_email_challenges
    ALTER COLUMN purpose TYPE annotator_challenge_purpose_v2
    USING purpose::text::annotator_challenge_purpose_v2;

DROP TYPE annotator_challenge_purpose;
ALTER TYPE annotator_challenge_purpose_v2 RENAME TO annotator_challenge_purpose;

-- Contact profile (docs/34 §5.4). Operational contact facts, not identity and
-- not a settlement record: no government id, no street address, no payout
-- fields — that boundary is the ratified verdict of docs/34 §7.5 and it is why
-- this is a separate table rather than columns on `annotators`. Every field is
-- optional; a missing row is a legal state, so existing annotators need no
-- backfill.
CREATE TABLE annotator_profiles (
    annotator_id uuid PRIMARY KEY REFERENCES annotators(id) ON DELETE RESTRICT,
    display_name text,
    phone text,
    im_kind text,
    im_handle text,
    region text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (display_name IS NULL OR (btrim(display_name) <> '' AND length(display_name) <= 80)),
    CHECK (phone IS NULL OR (btrim(phone) <> '' AND length(phone) <= 32)),
    CHECK (im_kind IS NULL OR im_kind IN ('wechat', 'qq', 'telegram', 'discord', 'other')),
    CHECK (im_handle IS NULL OR (btrim(im_handle) <> '' AND length(im_handle) <= 128)),
    CHECK (region IS NULL OR (btrim(region) <> '' AND length(region) <= 64)),
    -- An IM handle without a kind is unroutable; a kind without a handle is
    -- noise. They travel together or not at all.
    CHECK ((im_kind IS NULL) = (im_handle IS NULL))
);

COMMENT ON TABLE annotator_profiles IS
    'Optional contact facts for capture-operations coordination (docs/34 §5.4). Maintained by the annotator and by operators; wiped by trigger when the annotator is tombstoned. Never identity, never settlement data.';

CREATE TRIGGER annotator_profiles_set_updated_at
BEFORE UPDATE ON annotator_profiles
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER annotator_profiles_reject_delete
BEFORE DELETE ON annotator_profiles
FOR EACH ROW EXECUTE FUNCTION reject_governance_delete();

-- Tombstoning an annotator wipes the profile payload (row stays, fields go —
-- the same tombstone-not-delete shape client_events uses). It lives in a
-- trigger rather than application code so the invariant holds no matter which
-- surface performs the tombstone, and SECURITY DEFINER lets it fire from the
-- governance role without granting that role UPDATE on the profile table —
-- the grants file deliberately keeps dataminer_console read-only there.
CREATE FUNCTION wipe_annotator_profile_on_tombstone()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
BEGIN
    UPDATE annotator_profiles
       SET display_name = NULL,
           phone = NULL,
           im_kind = NULL,
           im_handle = NULL,
           region = NULL
     WHERE annotator_id = NEW.id;
    RETURN NEW;
END;
$$;

CREATE TRIGGER annotators_tombstone_wipes_profile
AFTER UPDATE OF status ON annotators
FOR EACH ROW
WHEN (NEW.status = 'tombstoned' AND OLD.status IS DISTINCT FROM 'tombstoned')
EXECUTE FUNCTION wipe_annotator_profile_on_tombstone();

INSERT INTO schema_migrations(version, name)
VALUES (14, 'annotator_claim_and_profile');

COMMIT;
