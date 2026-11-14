BEGIN;

ALTER TABLE multipart_uploads
    ALTER COLUMN provider_upload_id DROP NOT NULL,
    ADD CONSTRAINT multipart_provider_binding_check CHECK (
        status NOT IN ('uploading', 'completing', 'completed')
        OR provider_upload_id IS NOT NULL
    );

COMMENT ON COLUMN multipart_uploads.provider_upload_id IS
    'Server-only R2 multipart identifier. NULL is a durable pre-provider creation claim; it must be bound before signing or completion and is never returned to an untrusted client.';

INSERT INTO schema_migrations(version, name)
VALUES (3, 'durable_multipart_creation_claim');

COMMIT;
