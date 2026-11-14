BEGIN;

-- T14 moved new local sessions to dm-local-dev-session-2. The six session-root
-- catalog files remain local-only in M2a, so Cloud SQL records their exact-byte
-- identity as declared evidence without pretending the cloud has verified or
-- certified those files.
ALTER TABLE sessions
    ADD COLUMN catalog_id text,
    ADD COLUMN catalog_version text,
    ADD COLUMN catalog_mode text,
    ADD COLUMN catalog_manifest_sha256 sha256_digest,
    ADD COLUMN game_profile_sha256 sha256_digest,
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
            AND catalog_mode = 'dev_non_trainable'
            AND catalog_manifest_sha256 IS NOT NULL
            AND game_profile_sha256 IS NOT NULL
        )
    );

COMMENT ON COLUMN sessions.catalog_manifest_sha256 IS
    'Client-declared SHA-256 of the exact session-root catalog.json bytes. M2a does not upload or independently verify the snapshot.';
COMMENT ON COLUMN sessions.game_profile_sha256 IS
    'Client-declared SHA-256 of the exact session-root game-profile.json bytes. It never grants certified or trainable status.';

-- Every new v2 run is tied to the local journal v5 preflight pass that created
-- it. Only the stable reference is collected: the full checks remain local and
-- therefore the hash is audit evidence, not a server verification result.
ALTER TABLE capture_runs
    ADD COLUMN preflight_schema_version text,
    ADD COLUMN preflight_id text,
    ADD COLUMN preflight_result text,
    ADD COLUMN preflight_evidence_sha256 sha256_digest,
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

CREATE UNIQUE INDEX capture_runs_session_preflight_idx
    ON capture_runs(session_id, preflight_id)
    WHERE preflight_id IS NOT NULL;

-- The API already requires preflight evidence for every new v2 run. Keep the
-- same boundary fail-closed for maintenance scripts and other direct SQL
-- writers, while preserving legacy v1 rows with no preflight reference.
CREATE FUNCTION enforce_capture_run_preflight_contract()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_schema_version text;
BEGIN
    SELECT schema_version
      INTO parent_schema_version
      FROM sessions
     WHERE id = NEW.session_id;

    IF parent_schema_version = 'dm-local-dev-session-2' THEN
        IF NEW.preflight_schema_version IS DISTINCT FROM 'dm-preflight-evidence-1'
           OR NEW.preflight_id IS NULL
           OR btrim(NEW.preflight_id) = ''
           OR NEW.preflight_result IS DISTINCT FROM 'dev_non_trainable_pass'
           OR NEW.preflight_evidence_sha256 IS NULL THEN
            RAISE EXCEPTION 'session v2 capture run requires dev non-trainable preflight evidence'
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

CREATE TRIGGER capture_runs_enforce_preflight_contract
BEFORE INSERT OR UPDATE OF session_id, preflight_schema_version, preflight_id,
    preflight_result, preflight_evidence_sha256
ON capture_runs
FOR EACH ROW EXECUTE FUNCTION enforce_capture_run_preflight_contract();

INSERT INTO schema_migrations(version, name)
VALUES (4, 'session_v2_preflight_evidence');

COMMIT;
