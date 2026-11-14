-- Depth capture profiles and camera bindings are optional per-game catalog
-- payloads. They use the same immutable exact-byte registry and release roster
-- as the four core catalog kinds.
BEGIN;

ALTER TYPE catalog_kind ADD VALUE IF NOT EXISTS 'depth_profile';
ALTER TYPE catalog_kind ADD VALUE IF NOT EXISTS 'camera_binding';

-- Publishing still requires the four core books, while either depth book is
-- optional. Using kind::text avoids consuming the new enum values before this
-- transaction commits (PostgreSQL's ADD VALUE restriction).
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
    required_kind_count integer;
    supported_kind_count integer;
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

    SELECT
        count(*),
        count(DISTINCT kind),
        count(DISTINCT kind) FILTER (
            WHERE kind::text IN (
                'device_profile', 'recording_profile', 'game_profile', 'game_mapping'
            )
        ),
        count(DISTINCT kind) FILTER (
            WHERE kind::text IN (
                'device_profile', 'recording_profile', 'game_profile', 'game_mapping',
                'depth_profile', 'camera_binding'
            )
        )
      INTO entry_count, kind_count, required_kind_count, supported_kind_count
      FROM catalog_release_entries
     WHERE release_id = p_release_id;
    IF entry_count < 4
       OR required_kind_count <> 4
       OR supported_kind_count <> kind_count THEN
        RAISE EXCEPTION 'catalog release must contain all four profile kinds and only supported optional depth kinds';
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

INSERT INTO schema_migrations(version, name)
VALUES (33, 'depth_catalog_payloads');

COMMIT;
