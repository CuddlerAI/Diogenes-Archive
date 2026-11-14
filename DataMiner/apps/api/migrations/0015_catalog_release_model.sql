-- 0015 · 服务端驱动多游戏目录 D1：exact payload 与 immutable release
--
-- trusted_catalog_versions.snapshot 继续服务于查询与语义治理，但它不是
-- 签名输入。只有注册时保存并逐字节承诺的 catalog_payloads.exact_bytes
-- 才能进入后续 D2 的 builder。迁移刻意不从既有 JSONB 行回填 payload；
-- 没有 payload 的旧行在 console 中显示为不可发布。

BEGIN;

ALTER TYPE catalog_kind ADD VALUE 'game_profile';

CREATE TYPE catalog_release_status AS ENUM (
    'draft',
    'published',
    'superseded',
    'revoked'
);

CREATE TABLE catalog_payloads (
    trusted_catalog_version_id uuid PRIMARY KEY
        REFERENCES trusted_catalog_versions(id) ON DELETE RESTRICT,
    exact_bytes bytea NOT NULL CHECK (octet_length(exact_bytes) > 0),
    byte_size bigint NOT NULL CHECK (
        byte_size > 0 AND byte_size = octet_length(exact_bytes)
    ),
    sha256 sha256_digest NOT NULL,
    registered_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

COMMENT ON TABLE catalog_payloads IS
    'Registration-time exact JSON bytes. The signing path must read exact_bytes and must never reserialize trusted_catalog_versions.snapshot.';

CREATE FUNCTION validate_catalog_payload()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    expected_hash sha256_digest;
    actual_hash sha256_digest;
BEGIN
    SELECT content_hash
      INTO expected_hash
      FROM trusted_catalog_versions
     WHERE id = NEW.trusted_catalog_version_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'catalog payload references an unknown trusted catalog version';
    END IF;
    IF NEW.sha256 IS DISTINCT FROM expected_hash THEN
        RAISE EXCEPTION 'catalog payload sha256 must equal the trusted catalog content_hash';
    END IF;
    actual_hash :=
        ('sha256:' || encode(sha256(NEW.exact_bytes), 'hex'))::sha256_digest;
    IF NEW.sha256 IS DISTINCT FROM actual_hash THEN
        RAISE EXCEPTION 'catalog payload exact bytes do not match the declared sha256';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION reject_catalog_payload_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'catalog payload exact bytes are immutable';
END;
$$;

CREATE TRIGGER catalog_payloads_validate
BEFORE INSERT ON catalog_payloads
FOR EACH ROW EXECUTE FUNCTION validate_catalog_payload();

CREATE TRIGGER catalog_payloads_reject_mutation
BEFORE UPDATE OR DELETE ON catalog_payloads
FOR EACH ROW EXECUTE FUNCTION reject_catalog_payload_mutation();

CREATE TABLE catalog_releases (
    id uuid PRIMARY KEY,
    channel text NOT NULL CHECK (channel IN ('dev', 'certified')),
    catalog_id text NOT NULL CHECK (btrim(catalog_id) <> ''),
    catalog_version text NOT NULL CHECK (btrim(catalog_version) <> ''),
    catalog_mode text NOT NULL CHECK (btrim(catalog_mode) <> ''),
    manifest_bytes bytea NOT NULL CHECK (octet_length(manifest_bytes) > 0),
    envelope_bytes bytea NOT NULL CHECK (octet_length(envelope_bytes) > 0),
    envelope_sha256 sha256_digest NOT NULL,
    signing_key_id text NOT NULL CHECK (btrim(signing_key_id) <> ''),
    status catalog_release_status NOT NULL DEFAULT 'draft',
    created_by text NOT NULL CHECK (btrim(created_by) <> ''),
    published_by text,
    published_at timestamptz,
    superseded_at timestamptz,
    revoked_by text,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (
        (
            channel = 'dev'
            AND catalog_id = 'dataminer-dev-fleet'
            AND catalog_mode = 'dev_non_trainable'
        )
        OR
        (
            channel = 'certified'
            AND catalog_id = 'dataminer-certified-fleet'
            AND catalog_mode = 'certified'
        )
    ),
    CHECK (
        envelope_sha256 =
            ('sha256:' || encode(sha256(envelope_bytes), 'hex'))::sha256_digest
    ),
    CHECK (
        (status = 'draft'
            AND published_by IS NULL AND published_at IS NULL
            AND superseded_at IS NULL AND revoked_by IS NULL AND revoked_at IS NULL)
        OR
        (status = 'published'
            AND published_by IS NOT NULL AND published_at IS NOT NULL
            AND superseded_at IS NULL AND revoked_by IS NULL AND revoked_at IS NULL)
        OR
        (status = 'superseded'
            AND published_by IS NOT NULL AND published_at IS NOT NULL
            AND superseded_at IS NOT NULL AND revoked_by IS NULL AND revoked_at IS NULL)
        OR
        (status = 'revoked'
            AND revoked_by IS NOT NULL AND revoked_at IS NOT NULL)
    ),
    UNIQUE (channel, catalog_version)
);

COMMENT ON TABLE catalog_releases IS
    'Immutable signed catalog envelopes. GET will read envelope_bytes only after a later D3 wires delivery.';

CREATE UNIQUE INDEX catalog_releases_one_published_per_channel
    ON catalog_releases(channel)
    WHERE status = 'published';

CREATE TABLE catalog_release_entries (
    release_id uuid NOT NULL REFERENCES catalog_releases(id) ON DELETE RESTRICT,
    trusted_catalog_version_id uuid NOT NULL,
    kind catalog_kind NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (release_id, kind, ordinal),
    UNIQUE (release_id, trusted_catalog_version_id),
    FOREIGN KEY (trusted_catalog_version_id, kind)
        REFERENCES trusted_catalog_versions(id, kind) ON DELETE RESTRICT
);

COMMENT ON TABLE catalog_release_entries IS
    'Frozen release roster. Ordinal is the position in the corresponding manifest array.';

CREATE FUNCTION protect_catalog_release()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.channel IS DISTINCT FROM OLD.channel
        OR NEW.catalog_id IS DISTINCT FROM OLD.catalog_id
        OR NEW.catalog_version IS DISTINCT FROM OLD.catalog_version
        OR NEW.catalog_mode IS DISTINCT FROM OLD.catalog_mode
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

CREATE TRIGGER catalog_releases_protect
BEFORE UPDATE ON catalog_releases
FOR EACH ROW EXECUTE FUNCTION protect_catalog_release();

CREATE FUNCTION reject_catalog_release_delete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'catalog releases are immutable and may only transition to revoked';
END;
$$;

CREATE TRIGGER catalog_releases_reject_delete
BEFORE DELETE ON catalog_releases
FOR EACH ROW EXECUTE FUNCTION reject_catalog_release_delete();

CREATE FUNCTION validate_catalog_release_entry()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release_state catalog_release_status;
    trusted_state catalog_status;
BEGIN
    SELECT status INTO release_state
      FROM catalog_releases
     WHERE id = NEW.release_id;
    IF release_state IS DISTINCT FROM 'draft'::catalog_release_status THEN
        RAISE EXCEPTION 'catalog release entries can only be added to a draft release';
    END IF;

    SELECT status INTO trusted_state
      FROM trusted_catalog_versions
     WHERE id = NEW.trusted_catalog_version_id;
    IF trusted_state = 'revoked'::catalog_status THEN
        RAISE EXCEPTION 'revoked catalog versions cannot enter a release';
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM catalog_payloads
         WHERE trusted_catalog_version_id = NEW.trusted_catalog_version_id
    ) THEN
        RAISE EXCEPTION 'catalog version has no registered exact payload and is not publishable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION reject_catalog_release_entry_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'catalog release entries are immutable';
END;
$$;

CREATE TRIGGER catalog_release_entries_validate
BEFORE INSERT ON catalog_release_entries
FOR EACH ROW EXECUTE FUNCTION validate_catalog_release_entry();

CREATE TRIGGER catalog_release_entries_reject_mutation
BEFORE UPDATE OR DELETE ON catalog_release_entries
FOR EACH ROW EXECUTE FUNCTION reject_catalog_release_entry_mutation();

-- One database transaction owns the current pointer. Any validation, unique
-- index, trigger, or audit failure rolls the supersede + publish pair back.
CREATE FUNCTION publish_catalog_release(p_release_id uuid, p_actor text)
RETURNS uuid
LANGUAGE plpgsql
AS $$
DECLARE
    target_channel text;
    target_status catalog_release_status;
    previous_release_id uuid;
    entry_count integer;
    kind_count integer;
BEGIN
    IF p_actor IS NULL OR btrim(p_actor) = '' THEN
        RAISE EXCEPTION 'catalog release publisher is required';
    END IF;

    SELECT channel, status
      INTO target_channel, target_status
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
    IF entry_count < 5 OR kind_count <> 5 THEN
        RAISE EXCEPTION 'catalog release must contain all five profile kinds';
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
            'previous_release_id', previous_release_id,
            'entry_count', entry_count
        )
    );

    RETURN p_release_id;
END;
$$;

-- D1 exposes no runtime publisher. D2 will grant this function only to the
-- dedicated publish path after validator + signer wiring exists.
REVOKE EXECUTE ON FUNCTION publish_catalog_release(uuid, text) FROM PUBLIC;

CREATE INDEX catalog_release_entries_trusted_idx
    ON catalog_release_entries(trusted_catalog_version_id);
CREATE INDEX catalog_releases_history_idx
    ON catalog_releases(channel, created_at DESC);

INSERT INTO schema_migrations(version, name)
VALUES (15, 'catalog_release_model');

COMMIT;
