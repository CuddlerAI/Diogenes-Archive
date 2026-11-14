BEGIN;

-- docs/49 E7: the consented first-party lane. Migration 0004 pinned every v2
-- session to catalog_mode = 'dev_non_trainable' and every v2 run to
-- preflight_result = 'dev_non_trainable_pass', which was accurate while the dev
-- lane was the only admission. The first-party lane records sessions captured
-- under an operator-provisioned first_party_release catalog with an explicit
-- consent, so both pins widen to the exact two-value vocabulary — and the
-- trigger now enforces the *pairing*: a session's runs must carry the preflight
-- vocabulary of that session's own lane, not merely any admitted one.
--
-- Widening the admission vocabulary does not touch trainability:
-- `clip_trainability` still requires the full consent/grant/catalog/QA chain,
-- and `sessions.server_data_classification` still requires the bound identity
-- triple via its CHECK before it can say `production_candidate`.

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
            AND catalog_mode IN ('dev_non_trainable', 'first_party_release')
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
            AND preflight_result IN ('dev_non_trainable_pass', 'first_party_release_pass')
            AND preflight_evidence_sha256 IS NOT NULL
        )
    );

-- The API validates the preflight vocabulary against its admitted families, but
-- the run body carries no classification block, so only this trigger can pair
-- the run's preflight result with the parent session's lane. Keep the boundary
-- fail-closed for maintenance scripts and other direct SQL writers too.
CREATE OR REPLACE FUNCTION enforce_capture_run_preflight_contract()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    parent_schema_version text;
    parent_catalog_mode text;
    required_result text;
BEGIN
    SELECT schema_version, catalog_mode
      INTO parent_schema_version, parent_catalog_mode
      FROM sessions
     WHERE id = NEW.session_id;

    IF parent_schema_version = 'dm-local-dev-session-2' THEN
        required_result := CASE parent_catalog_mode
            WHEN 'dev_non_trainable' THEN 'dev_non_trainable_pass'
            WHEN 'first_party_release' THEN 'first_party_release_pass'
            ELSE NULL
        END;
        IF required_result IS NULL THEN
            RAISE EXCEPTION 'session v2 capture run has an unsupported parent catalog mode'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.preflight_schema_version IS DISTINCT FROM 'dm-preflight-evidence-1'
           OR NEW.preflight_id IS NULL
           OR btrim(NEW.preflight_id) = ''
           OR NEW.preflight_result IS DISTINCT FROM required_result
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

INSERT INTO schema_migrations(version, name)
VALUES (9, 'first_party_admission');

COMMIT;
