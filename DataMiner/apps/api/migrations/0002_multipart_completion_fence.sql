BEGIN;

ALTER TABLE multipart_uploads
    ADD COLUMN completion_lease_id uuid,
    ADD COLUMN completion_started_at timestamptz,
    ADD CONSTRAINT multipart_completion_fence_check CHECK (
        status <> 'completing'
        OR (completion_lease_id IS NOT NULL AND completion_started_at IS NOT NULL)
    );

COMMENT ON COLUMN multipart_uploads.completion_lease_id IS
    'Fences the SQL finalizer from stale CompleteMultipartUpload calls after purge or takeover.';

COMMENT ON COLUMN multipart_uploads.completion_started_at IS
    'Start of the provider CompleteMultipartUpload ownership window. Purge waits for a live window and may take over only after it is stale.';

ALTER TABLE upload_parts
    ADD COLUMN signed_expires_at timestamptz,
    ADD CONSTRAINT upload_part_signed_expiry_check CHECK (
        signed_expires_at IS NULL OR signed_expires_at >= created_at
    );

COMMENT ON COLUMN upload_parts.signed_expires_at IS
    'Conservative upper bound for the latest presigned capability issued for this part; no URL is persisted.';

CREATE INDEX multipart_uploads_completion_lease_idx
    ON multipart_uploads(completion_started_at)
    WHERE status = 'completing';

INSERT INTO schema_migrations(version, name)
VALUES (2, 'multipart_completion_fence');

COMMIT;
