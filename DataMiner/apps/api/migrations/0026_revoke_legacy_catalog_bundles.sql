-- Catalog schema v2 replaced the aggregate five-entry bundle with one
-- four-entry bundle per game. A published v1 row left behind by migration 0022
-- is still returned by GET /v1/catalog and makes the client reject the entire
-- signed set because it has no recordingProfiles member.
--
-- Current registration already accepts only dm-catalog-bundle-2, so this is a
-- one-time cleanup of immutable historical releases. Revocation keeps their
-- signed bytes and publication attribution for audit purposes.

BEGIN;

WITH revoked AS (
    UPDATE catalog_releases
       SET status = 'revoked',
           revoked_by = 'migration-0026',
           revoked_at = transaction_timestamp()
     WHERE status = 'published'
       AND convert_from(manifest_bytes, 'UTF8')::jsonb ->> 'schema_version'
           = 'dm-catalog-bundle-1'
    RETURNING id, channel, catalog_id, catalog_version
)
INSERT INTO audit_events(
    event_id, actor_type, actor_id, action, entity_type, entity_id, details
)
SELECT
    gen_random_uuid(),
    'service',
    'migration-0026',
    'catalog.release_revoked',
    'catalog_release',
    revoked.id::text,
    jsonb_build_object(
        'channel', revoked.channel,
        'catalogId', revoked.catalog_id,
        'catalogVersion', revoked.catalog_version,
        'reason', 'legacy_bundle_schema_incompatible_with_recording_profiles'
    )
FROM revoked;

INSERT INTO schema_migrations(version, name)
VALUES (26, 'revoke_legacy_catalog_bundles');

COMMIT;
