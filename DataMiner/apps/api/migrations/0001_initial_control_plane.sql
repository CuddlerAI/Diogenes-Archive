BEGIN;

-- Migration convention:
--   * Files are immutable and applied exactly once in ascending numeric order.
--   * Every file runs in one transaction and records its own version only after all DDL succeeds.
--   * Applied files are never edited; schema changes are appended as a new numbered migration.
-- Rerunning an applied file is intentionally an error instead of silently accepting schema drift.
CREATE TABLE schema_migrations (
    version bigint PRIMARY KEY CHECK (version > 0),
    name text NOT NULL UNIQUE CHECK (btrim(name) <> ''),
    applied_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

COMMENT ON TABLE schema_migrations IS
    'Append-only migration ledger. Apply numbered SQL files once, in order, and never edit an applied file.';

CREATE TYPE annotator_status AS ENUM ('active', 'suspended', 'tombstoned');
CREATE TYPE consent_status AS ENUM ('pending', 'active', 'expired', 'revoked');
CREATE TYPE grant_status AS ENUM ('active', 'revoked');
CREATE TYPE device_status AS ENUM ('pending', 'active', 'suspended', 'retired');
CREATE TYPE catalog_kind AS ENUM (
    'device_profile',
    'capture_profile',
    'settings_profile',
    'game_mapping'
);
CREATE TYPE catalog_status AS ENUM ('draft', 'approved', 'retired', 'revoked');
CREATE TYPE data_classification AS ENUM ('non_trainable', 'production_candidate');
CREATE TYPE session_status AS ENUM ('registered', 'capturing', 'sealed', 'completed', 'rejected');
CREATE TYPE capture_run_status AS ENUM ('registered', 'capturing', 'paused', 'completed', 'faulted');
CREATE TYPE artifact_kind AS ENUM ('video', 'frames', 'events', 'meta', 'manifest');
CREATE TYPE ingest_state AS ENUM ('pending', 'uploading', 'stored', 'retry_wait', 'failed');
CREATE TYPE storage_state AS ENUM ('pending', 'uploading', 'stored', 'failed', 'purging', 'purged');
CREATE TYPE verification_state AS ENUM ('unverified', 'queued', 'verifying', 'verified', 'rejected');
CREATE TYPE quality_state AS ENUM ('pending', 'passed', 'failed');
CREATE TYPE retention_state AS ENUM ('retained', 'purge_requested', 'purging', 'purged', 'purge_failed');
CREATE TYPE idempotency_state AS ENUM ('in_progress', 'completed', 'failed');
CREATE TYPE object_store_provider AS ENUM ('r2');
CREATE TYPE upload_intent_status AS ENUM ('pending', 'uploading', 'completing', 'completed', 'aborted', 'expired', 'failed');
CREATE TYPE multipart_status AS ENUM ('initiated', 'uploading', 'completing', 'completed', 'aborted', 'expired', 'failed');
CREATE TYPE upload_part_status AS ENUM ('pending', 'uploaded');
CREATE TYPE job_status AS ENUM ('pending', 'running', 'retry_wait', 'succeeded', 'failed', 'dead_letter');
CREATE TYPE audit_actor_type AS ENUM ('service', 'annotator', 'device', 'operator', 'worker');
CREATE TYPE purge_scope AS ENUM ('consent', 'session', 'clip');

CREATE DOMAIN sha256_digest AS text
    CHECK (VALUE ~ '^sha256:[0-9a-f]{64}$');

CREATE FUNCTION set_row_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at := transaction_timestamp();
    RETURN NEW;
END;
$$;

CREATE FUNCTION protect_catalog_version_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.kind IS DISTINCT FROM OLD.kind
        OR NEW.catalog_id IS DISTINCT FROM OLD.catalog_id
        OR NEW.version IS DISTINCT FROM OLD.version
        OR NEW.game_id IS DISTINCT FROM OLD.game_id
        OR NEW.content_hash IS DISTINCT FROM OLD.content_hash
        OR NEW.snapshot IS DISTINCT FROM OLD.snapshot THEN
        RAISE EXCEPTION 'catalog version identity and snapshot are immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION reject_governance_delete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% records are tombstoned, not deleted', TG_TABLE_NAME;
END;
$$;

CREATE FUNCTION reject_audit_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'audit_events is append-only';
END;
$$;

CREATE TABLE annotators (
    id uuid PRIMARY KEY,
    external_subject_hash text UNIQUE,
    alias text,
    status annotator_status NOT NULL DEFAULT 'active',
    tombstoned_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (external_subject_hash IS NULL OR external_subject_hash ~ '^[0-9a-f]{64}$'),
    CHECK (alias IS NULL OR btrim(alias) <> ''),
    CHECK (status <> 'tombstoned' OR tombstoned_at IS NOT NULL),
    UNIQUE (id, status)
);

CREATE TABLE consents (
    id uuid PRIMARY KEY,
    annotator_id uuid NOT NULL REFERENCES annotators(id) ON DELETE RESTRICT,
    policy_version text NOT NULL CHECK (btrim(policy_version) <> ''),
    purpose text NOT NULL CHECK (btrim(purpose) <> ''),
    terms_hash text NOT NULL CHECK (terms_hash ~ '^[0-9a-f]{64}$'),
    status consent_status NOT NULL DEFAULT 'pending',
    allows_trainable_use boolean NOT NULL DEFAULT false,
    accepted_at timestamptz,
    expires_at timestamptz,
    revoked_at timestamptz,
    revoke_reason text,
    retention_deadline timestamptz,
    retention_state retention_state NOT NULL DEFAULT 'retained',
    purge_requested_at timestamptz,
    purged_at timestamptz,
    tombstoned_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at IS NULL OR accepted_at IS NULL OR expires_at > accepted_at),
    CHECK (status <> 'active' OR (accepted_at IS NOT NULL AND revoked_at IS NULL)),
    CHECK (status <> 'revoked' OR revoked_at IS NOT NULL),
    CHECK (retention_state <> 'purge_requested' OR purge_requested_at IS NOT NULL),
    CHECK (retention_state <> 'purged' OR purged_at IS NOT NULL),
    CHECK (tombstoned_at IS NULL OR purged_at IS NOT NULL),
    UNIQUE (id, annotator_id)
);

CREATE TABLE games (
    id text PRIMARY KEY CHECK (btrim(id) <> ''),
    title text NOT NULL CHECK (btrim(title) <> ''),
    content_kind text NOT NULL CHECK (btrim(content_kind) <> ''),
    view_kind text NOT NULL CHECK (btrim(view_kind) <> ''),
    material_kind text NOT NULL CHECK (btrim(material_kind) <> ''),
    allowed_for_capture boolean NOT NULL DEFAULT false,
    approved_for_trainable boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

CREATE TABLE trusted_catalog_versions (
    id uuid PRIMARY KEY,
    kind catalog_kind NOT NULL,
    catalog_id text NOT NULL CHECK (btrim(catalog_id) <> ''),
    version text NOT NULL CHECK (btrim(version) <> ''),
    game_id text REFERENCES games(id) ON DELETE RESTRICT,
    content_hash sha256_digest NOT NULL,
    snapshot jsonb NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    status catalog_status NOT NULL DEFAULT 'draft',
    approved_for_trainable boolean NOT NULL DEFAULT false,
    approved_by text,
    approved_at timestamptz,
    retired_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (
        (kind = 'device_profile' AND game_id IS NULL)
        OR (kind <> 'device_profile' AND game_id IS NOT NULL)
    ),
    CHECK (status <> 'approved' OR (approved_at IS NOT NULL AND approved_by IS NOT NULL)),
    CHECK (status <> 'retired' OR retired_at IS NOT NULL),
    CHECK (status <> 'revoked' OR revoked_at IS NOT NULL),
    CHECK (NOT approved_for_trainable OR status IN ('approved', 'retired')),
    UNIQUE (kind, catalog_id, version),
    UNIQUE (id, kind),
    UNIQUE (id, kind, game_id)
);

COMMENT ON TABLE trusted_catalog_versions IS
    'Server-controlled immutable catalog snapshots. draft entries may support staging but never make data trainable.';

CREATE TABLE devices (
    id uuid PRIMARY KEY,
    client_device_id text NOT NULL UNIQUE CHECK (btrim(client_device_id) <> ''),
    annotator_id uuid REFERENCES annotators(id) ON DELETE RESTRICT,
    certified_profile_version_id uuid NOT NULL,
    certified_profile_kind catalog_kind NOT NULL DEFAULT 'device_profile',
    credential_fingerprint text NOT NULL UNIQUE CHECK (credential_fingerprint ~ '^[0-9a-f]{64}$'),
    status device_status NOT NULL DEFAULT 'pending',
    windows_build text NOT NULL CHECK (btrim(windows_build) <> ''),
    gpu_pci_id text NOT NULL CHECK (btrim(gpu_pci_id) <> ''),
    gpu_driver text NOT NULL CHECK (btrim(gpu_driver) <> ''),
    display_snapshot jsonb NOT NULL CHECK (jsonb_typeof(display_snapshot) = 'object'),
    input_devices_snapshot jsonb NOT NULL CHECK (jsonb_typeof(input_devices_snapshot) IN ('array', 'object')),
    client_version text NOT NULL CHECK (btrim(client_version) <> ''),
    last_seen_at timestamptz,
    retired_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (certified_profile_kind = 'device_profile'),
    CHECK (status <> 'retired' OR retired_at IS NOT NULL),
    FOREIGN KEY (certified_profile_version_id, certified_profile_kind)
        REFERENCES trusted_catalog_versions(id, kind) ON DELETE RESTRICT,
    UNIQUE (id, annotator_id)
);

CREATE TABLE capture_grants (
    id uuid PRIMARY KEY,
    annotator_id uuid NOT NULL,
    consent_id uuid NOT NULL,
    device_id uuid NOT NULL,
    game_id text NOT NULL REFERENCES games(id) ON DELETE RESTRICT,
    capture_profile_version_id uuid NOT NULL,
    capture_profile_kind catalog_kind NOT NULL DEFAULT 'capture_profile',
    status grant_status NOT NULL DEFAULT 'active',
    allows_trainable boolean NOT NULL DEFAULT false,
    valid_from timestamptz NOT NULL,
    valid_until timestamptz NOT NULL,
    issued_by text NOT NULL CHECK (btrim(issued_by) <> ''),
    issued_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    revoked_at timestamptz,
    revoke_reason text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (capture_profile_kind = 'capture_profile'),
    CHECK (valid_until > valid_from),
    CHECK (status <> 'revoked' OR revoked_at IS NOT NULL),
    FOREIGN KEY (consent_id, annotator_id)
        REFERENCES consents(id, annotator_id) ON DELETE RESTRICT,
    FOREIGN KEY (device_id, annotator_id)
        REFERENCES devices(id, annotator_id) ON DELETE RESTRICT,
    FOREIGN KEY (capture_profile_version_id, capture_profile_kind, game_id)
        REFERENCES trusted_catalog_versions(id, kind, game_id) ON DELETE RESTRICT,
    UNIQUE (id, annotator_id, consent_id, device_id)
);

CREATE TABLE sessions (
    id uuid PRIMARY KEY,
    client_session_id text NOT NULL CHECK (btrim(client_session_id) <> ''),
    device_id uuid NOT NULL REFERENCES devices(id) ON DELETE RESTRICT,
    annotator_id uuid,
    consent_id uuid,
    capture_grant_id uuid,
    game_id text NOT NULL REFERENCES games(id) ON DELETE RESTRICT,
    game_version text NOT NULL CHECK (btrim(game_version) <> ''),
    game_executable_sha256 sha256_digest NOT NULL,
    device_catalog_version_id uuid NOT NULL,
    device_catalog_kind catalog_kind NOT NULL DEFAULT 'device_profile',
    capture_catalog_version_id uuid NOT NULL,
    capture_catalog_kind catalog_kind NOT NULL DEFAULT 'capture_profile',
    settings_catalog_version_id uuid NOT NULL,
    settings_catalog_kind catalog_kind NOT NULL DEFAULT 'settings_profile',
    mapping_catalog_version_id uuid NOT NULL,
    mapping_catalog_kind catalog_kind NOT NULL DEFAULT 'game_mapping',
    schema_version text NOT NULL CHECK (btrim(schema_version) <> ''),
    manifest_json jsonb NOT NULL CHECK (jsonb_typeof(manifest_json) = 'object'),
    manifest_sha256 sha256_digest NOT NULL,
    server_data_classification data_classification NOT NULL DEFAULT 'non_trainable',
    status session_status NOT NULL DEFAULT 'registered',
    capture_engine text NOT NULL CHECK (capture_engine = 'native_wgc_window_v1'),
    native_width integer NOT NULL CHECK (native_width >= 1),
    native_height integer NOT NULL CHECK (native_height >= 1),
    fps_num integer NOT NULL CHECK (fps_num > 0),
    fps_den integer NOT NULL CHECK (fps_den > 0),
    client_version text NOT NULL CHECK (btrim(client_version) <> ''),
    started_at timestamptz NOT NULL,
    ended_at timestamptz,
    sealed_at timestamptz,
    stop_reason text,
    rejected_reason text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (device_catalog_kind = 'device_profile'),
    CHECK (capture_catalog_kind = 'capture_profile'),
    CHECK (settings_catalog_kind = 'settings_profile'),
    CHECK (mapping_catalog_kind = 'game_mapping'),
    CHECK (fps_num >= 1 * fps_den),
    CHECK (ended_at IS NULL OR ended_at >= started_at),
    CHECK (sealed_at IS NULL OR sealed_at >= started_at),
    CHECK (consent_id IS NULL OR annotator_id IS NOT NULL),
    CHECK (
        capture_grant_id IS NULL
        OR (annotator_id IS NOT NULL AND consent_id IS NOT NULL)
    ),
    CHECK (
        server_data_classification = 'non_trainable'
        OR (annotator_id IS NOT NULL AND consent_id IS NOT NULL AND capture_grant_id IS NOT NULL)
    ),
    CHECK (status <> 'rejected' OR rejected_reason IS NOT NULL),
    UNIQUE (device_id, client_session_id),
    FOREIGN KEY (device_id, annotator_id)
        REFERENCES devices(id, annotator_id) ON DELETE RESTRICT,
    FOREIGN KEY (consent_id, annotator_id)
        REFERENCES consents(id, annotator_id) ON DELETE RESTRICT,
    FOREIGN KEY (capture_grant_id, annotator_id, consent_id, device_id)
        REFERENCES capture_grants(id, annotator_id, consent_id, device_id) ON DELETE RESTRICT,
    FOREIGN KEY (device_catalog_version_id, device_catalog_kind)
        REFERENCES trusted_catalog_versions(id, kind) ON DELETE RESTRICT,
    FOREIGN KEY (capture_catalog_version_id, capture_catalog_kind, game_id)
        REFERENCES trusted_catalog_versions(id, kind, game_id) ON DELETE RESTRICT,
    FOREIGN KEY (settings_catalog_version_id, settings_catalog_kind, game_id)
        REFERENCES trusted_catalog_versions(id, kind, game_id) ON DELETE RESTRICT,
    FOREIGN KEY (mapping_catalog_version_id, mapping_catalog_kind, game_id)
        REFERENCES trusted_catalog_versions(id, kind, game_id) ON DELETE RESTRICT
);

COMMENT ON COLUMN sessions.server_data_classification IS
    'Fail-closed server decision. Client non_trainable/production_certified fields are evidence only and never write this column directly.';

CREATE TABLE capture_runs (
    id uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
    client_run_id text NOT NULL CHECK (btrim(client_run_id) <> ''),
    run_seq integer NOT NULL CHECK (run_seq >= 0),
    status capture_run_status NOT NULL DEFAULT 'registered',
    capture_engine text NOT NULL CHECK (capture_engine = 'native_wgc_window_v1'),
    encoder_backend text NOT NULL CHECK (btrim(encoder_backend) <> ''),
    width integer NOT NULL CHECK (width >= 1),
    height integer NOT NULL CHECK (height >= 1),
    fps_num integer NOT NULL CHECK (fps_num > 0),
    fps_den integer NOT NULL CHECK (fps_den > 0),
    qpc_frequency bigint CHECK (qpc_frequency > 0),
    anchor_qpc bigint,
    qpc_start bigint,
    qpc_end bigint,
    session_frame_start bigint NOT NULL CHECK (session_frame_start >= 0),
    session_frame_end bigint,
    frame_count bigint CHECK (frame_count >= 0),
    ready_clip_count integer CHECK (ready_clip_count >= 0),
    health_json jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(health_json) = 'object'),
    manifest_json jsonb CHECK (manifest_json IS NULL OR jsonb_typeof(manifest_json) = 'object'),
    manifest_sha256 sha256_digest,
    started_at timestamptz,
    ended_at timestamptz,
    stop_reason text,
    error_code text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (fps_num >= 1 * fps_den),
    CHECK (qpc_end IS NULL OR qpc_start IS NULL OR qpc_end >= qpc_start),
    CHECK (session_frame_end IS NULL OR session_frame_end >= session_frame_start),
    CHECK (ended_at IS NULL OR started_at IS NULL OR ended_at >= started_at),
    CHECK ((manifest_json IS NULL) = (manifest_sha256 IS NULL)),
    UNIQUE (session_id, run_seq),
    UNIQUE (session_id, client_run_id),
    UNIQUE (id, session_id, run_seq)
);

CREATE TABLE clips (
    id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    capture_run_id uuid NOT NULL,
    run_seq integer NOT NULL CHECK (run_seq >= 0),
    segment_seq integer NOT NULL CHECK (segment_seq >= 0),
    session_frame_start bigint NOT NULL CHECK (session_frame_start >= 0),
    n_frames bigint NOT NULL CHECK (n_frames > 0),
    qpc_start bigint NOT NULL,
    qpc_end bigint NOT NULL,
    event_count bigint NOT NULL CHECK (event_count >= 0),
    health_json jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(health_json) = 'object'),
    meta_json jsonb NOT NULL CHECK (jsonb_typeof(meta_json) = 'object'),
    manifest_json jsonb NOT NULL CHECK (jsonb_typeof(manifest_json) = 'object'),
    manifest_sha256 sha256_digest NOT NULL,
    revision integer NOT NULL DEFAULT 1 CHECK (revision > 0),
    ingest_state ingest_state NOT NULL DEFAULT 'pending',
    verification_state verification_state NOT NULL DEFAULT 'unverified',
    quality_state quality_state NOT NULL DEFAULT 'pending',
    retention_state retention_state NOT NULL DEFAULT 'retained',
    stored_at timestamptz,
    verified_at timestamptz,
    quality_reviewed_at timestamptz,
    quality_reviewed_by text,
    retention_deadline timestamptz NOT NULL,
    purge_requested_at timestamptz,
    purged_at timestamptz,
    rejected_reason text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (qpc_end >= qpc_start),
    CHECK (retention_deadline > created_at),
    CHECK (ingest_state <> 'stored' OR stored_at IS NOT NULL),
    CHECK (verification_state <> 'verified' OR verified_at IS NOT NULL),
    CHECK (quality_state = 'pending' OR quality_reviewed_at IS NOT NULL),
    CHECK (retention_state <> 'purge_requested' OR purge_requested_at IS NOT NULL),
    CHECK (retention_state <> 'purged' OR purged_at IS NOT NULL),
    CHECK (verification_state <> 'rejected' OR rejected_reason IS NOT NULL),
    UNIQUE (session_id, run_seq, segment_seq),
    UNIQUE (id, revision),
    FOREIGN KEY (capture_run_id, session_id, run_seq)
        REFERENCES capture_runs(id, session_id, run_seq) ON DELETE RESTRICT
);

CREATE TABLE artifact_objects (
    id uuid PRIMARY KEY,
    clip_id uuid NOT NULL REFERENCES clips(id) ON DELETE RESTRICT,
    kind artifact_kind NOT NULL,
    bucket_name text NOT NULL CHECK (btrim(bucket_name) <> ''),
    object_key text NOT NULL CHECK (btrim(object_key) <> ''),
    expected_sha256 sha256_digest NOT NULL,
    expected_size_bytes bigint NOT NULL CHECK (expected_size_bytes > 0),
    storage_state storage_state NOT NULL DEFAULT 'pending',
    verification_state verification_state NOT NULL DEFAULT 'unverified',
    provider_etag text,
    provider_version_id text,
    stored_at timestamptz,
    verified_at timestamptz,
    purge_started_at timestamptz,
    purged_at timestamptz,
    last_error_code text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (storage_state <> 'stored' OR stored_at IS NOT NULL),
    CHECK (storage_state <> 'purging' OR purge_started_at IS NOT NULL),
    CHECK (storage_state <> 'purged' OR purged_at IS NOT NULL),
    CHECK (verification_state <> 'verified' OR verified_at IS NOT NULL),
    CHECK (
        verification_state NOT IN ('queued', 'verifying', 'verified')
        OR storage_state IN ('stored', 'purging', 'purged')
    ),
    UNIQUE (clip_id, kind),
    UNIQUE (bucket_name, object_key)
);

COMMENT ON COLUMN artifact_objects.provider_etag IS
    'Provider transport identity only. Multipart ETags are never accepted as the full-object SHA-256.';

CREATE TABLE idempotency_requests (
    id uuid PRIMARY KEY,
    scope text NOT NULL CHECK (btrim(scope) <> ''),
    key text NOT NULL CHECK (btrim(key) <> ''),
    request_hash sha256_digest NOT NULL,
    state idempotency_state NOT NULL DEFAULT 'in_progress',
    response_status integer CHECK (response_status BETWEEN 100 AND 599),
    response_headers jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(response_headers) = 'object'),
    response_body jsonb,
    locked_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    completed_at timestamptz,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at > created_at),
    CHECK (state = 'in_progress' OR response_status IS NOT NULL),
    CHECK (state = 'in_progress' OR completed_at IS NOT NULL),
    UNIQUE (scope, key)
);

COMMENT ON TABLE idempotency_requests IS
    'Same scope/key with a different request_hash is a conflict; completed responses are replayed verbatim by the API.';

CREATE TABLE upload_intents (
    id uuid PRIMARY KEY,
    clip_id uuid NOT NULL,
    clip_revision integer NOT NULL CHECK (clip_revision > 0),
    attempt_no integer NOT NULL CHECK (attempt_no > 0),
    status upload_intent_status NOT NULL DEFAULT 'pending',
    expires_at timestamptz NOT NULL,
    completed_at timestamptz,
    aborted_at timestamptz,
    last_error_code text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at > created_at),
    CHECK (status <> 'completed' OR completed_at IS NOT NULL),
    CHECK (status <> 'aborted' OR aborted_at IS NOT NULL),
    UNIQUE (clip_id, attempt_no),
    FOREIGN KEY (clip_id, clip_revision)
        REFERENCES clips(id, revision) ON DELETE RESTRICT
);

CREATE TABLE multipart_uploads (
    id uuid PRIMARY KEY,
    upload_intent_id uuid NOT NULL REFERENCES upload_intents(id) ON DELETE RESTRICT,
    artifact_object_id uuid NOT NULL REFERENCES artifact_objects(id) ON DELETE RESTRICT,
    attempt_no integer NOT NULL CHECK (attempt_no > 0),
    provider object_store_provider NOT NULL DEFAULT 'r2',
    provider_upload_id text NOT NULL CHECK (btrim(provider_upload_id) <> ''),
    part_size_bytes bigint NOT NULL CHECK (
        part_size_bytes >= 5242880 AND part_size_bytes <= 5368709120
    ),
    status multipart_status NOT NULL DEFAULT 'initiated',
    expires_at timestamptz NOT NULL,
    completed_at timestamptz,
    aborted_at timestamptz,
    last_error_code text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (expires_at > created_at),
    CHECK (status <> 'completed' OR completed_at IS NOT NULL),
    CHECK (status <> 'aborted' OR aborted_at IS NOT NULL),
    UNIQUE (upload_intent_id, artifact_object_id),
    UNIQUE (artifact_object_id, attempt_no),
    UNIQUE (provider, provider_upload_id)
);

COMMENT ON COLUMN multipart_uploads.provider_upload_id IS
    'Server-only R2 multipart identifier. It is never accepted from or returned to an untrusted client.';

CREATE TABLE upload_parts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    multipart_upload_id uuid NOT NULL REFERENCES multipart_uploads(id) ON DELETE RESTRICT,
    part_number integer NOT NULL CHECK (part_number BETWEEN 1 AND 10000),
    size_bytes bigint CHECK (size_bytes > 0),
    content_md5 text CHECK (content_md5 IS NULL OR content_md5 ~ '^[A-Za-z0-9+/]{22}==$'),
    etag text,
    status upload_part_status NOT NULL DEFAULT 'pending',
    uploaded_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (
        status <> 'uploaded'
        OR (
            size_bytes IS NOT NULL
            AND content_md5 IS NOT NULL
            AND etag IS NOT NULL
            AND uploaded_at IS NOT NULL
        )
    ),
    UNIQUE (multipart_upload_id, part_number)
);

COMMENT ON TABLE upload_parts IS
    'Persists signed Content-Length/Content-MD5 and provider reconciliation. Presigned URLs and credentials must never be stored.';

CREATE TABLE verification_jobs (
    id uuid PRIMARY KEY,
    artifact_object_id uuid NOT NULL REFERENCES artifact_objects(id) ON DELETE RESTRICT,
    status job_status NOT NULL DEFAULT 'pending',
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    max_attempts integer NOT NULL DEFAULT 8 CHECK (max_attempts > 0),
    available_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    locked_at timestamptz,
    locked_by text,
    result_sha256 sha256_digest,
    result_size_bytes bigint CHECK (result_size_bytes IS NULL OR result_size_bytes >= 0),
    last_error_code text,
    last_error_summary text,
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (attempt_count <= max_attempts),
    CHECK (status <> 'running' OR (locked_at IS NOT NULL AND locked_by IS NOT NULL)),
    CHECK (
        status <> 'succeeded'
        OR (result_sha256 IS NOT NULL AND result_size_bytes IS NOT NULL AND completed_at IS NOT NULL)
    )
);

CREATE TABLE outbox_jobs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id uuid NOT NULL UNIQUE,
    topic text NOT NULL CHECK (btrim(topic) <> ''),
    aggregate_type text NOT NULL CHECK (btrim(aggregate_type) <> ''),
    aggregate_id text NOT NULL CHECK (btrim(aggregate_id) <> ''),
    dedupe_key text NOT NULL UNIQUE CHECK (btrim(dedupe_key) <> ''),
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    status job_status NOT NULL DEFAULT 'pending',
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    max_attempts integer NOT NULL DEFAULT 12 CHECK (max_attempts > 0),
    available_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    locked_at timestamptz,
    locked_by text,
    last_error_code text,
    published_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (attempt_count <= max_attempts),
    CHECK (status <> 'running' OR (locked_at IS NOT NULL AND locked_by IS NOT NULL)),
    CHECK (status <> 'succeeded' OR published_at IS NOT NULL)
);

CREATE TABLE purge_jobs (
    id uuid PRIMARY KEY,
    scope purge_scope NOT NULL,
    consent_id uuid REFERENCES consents(id) ON DELETE RESTRICT,
    session_id uuid REFERENCES sessions(id) ON DELETE RESTRICT,
    clip_id uuid REFERENCES clips(id) ON DELETE RESTRICT,
    triggering_outbox_job_id bigint UNIQUE REFERENCES outbox_jobs(id) ON DELETE RESTRICT,
    reason text NOT NULL CHECK (btrim(reason) <> ''),
    requested_by_type audit_actor_type NOT NULL,
    requested_by_id text,
    status job_status NOT NULL DEFAULT 'pending',
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    max_attempts integer NOT NULL DEFAULT 20 CHECK (max_attempts > 0),
    available_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    locked_at timestamptz,
    locked_by text,
    progress_json jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(progress_json) = 'object'),
    last_error_code text,
    last_error_summary text,
    started_at timestamptz,
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    updated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (
        (scope = 'consent' AND consent_id IS NOT NULL AND session_id IS NULL AND clip_id IS NULL)
        OR (scope = 'session' AND consent_id IS NULL AND session_id IS NOT NULL AND clip_id IS NULL)
        OR (scope = 'clip' AND consent_id IS NULL AND session_id IS NULL AND clip_id IS NOT NULL)
    ),
    CHECK (attempt_count <= max_attempts),
    CHECK (status <> 'running' OR (locked_at IS NOT NULL AND locked_by IS NOT NULL)),
    CHECK (status <> 'succeeded' OR completed_at IS NOT NULL)
);

COMMENT ON TABLE purge_jobs IS
    'Durable consent/session/clip purge orchestration. Successful purge removes R2 bytes but retains SQL tombstones and audit history.';

CREATE TABLE audit_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id uuid NOT NULL UNIQUE,
    occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    request_id text,
    actor_type audit_actor_type NOT NULL,
    actor_id text,
    action text NOT NULL CHECK (btrim(action) <> ''),
    entity_type text NOT NULL CHECK (btrim(entity_type) <> ''),
    entity_id text NOT NULL CHECK (btrim(entity_id) <> ''),
    consent_id uuid REFERENCES consents(id) ON DELETE RESTRICT,
    details jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(details) = 'object')
);

COMMENT ON TABLE audit_events IS
    'Append-only security and governance audit trail. It is retained after object purge.';

CREATE TRIGGER annotators_set_updated_at
BEFORE UPDATE ON annotators
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER consents_set_updated_at
BEFORE UPDATE ON consents
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER games_set_updated_at
BEFORE UPDATE ON games
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER trusted_catalog_versions_set_updated_at
BEFORE UPDATE ON trusted_catalog_versions
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER trusted_catalog_versions_protect_identity
BEFORE UPDATE ON trusted_catalog_versions
FOR EACH ROW EXECUTE FUNCTION protect_catalog_version_identity();

CREATE TRIGGER devices_set_updated_at
BEFORE UPDATE ON devices
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER capture_grants_set_updated_at
BEFORE UPDATE ON capture_grants
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER sessions_set_updated_at
BEFORE UPDATE ON sessions
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER capture_runs_set_updated_at
BEFORE UPDATE ON capture_runs
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER clips_set_updated_at
BEFORE UPDATE ON clips
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER artifact_objects_set_updated_at
BEFORE UPDATE ON artifact_objects
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER idempotency_requests_set_updated_at
BEFORE UPDATE ON idempotency_requests
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER upload_intents_set_updated_at
BEFORE UPDATE ON upload_intents
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER multipart_uploads_set_updated_at
BEFORE UPDATE ON multipart_uploads
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER upload_parts_set_updated_at
BEFORE UPDATE ON upload_parts
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER verification_jobs_set_updated_at
BEFORE UPDATE ON verification_jobs
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER outbox_jobs_set_updated_at
BEFORE UPDATE ON outbox_jobs
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER purge_jobs_set_updated_at
BEFORE UPDATE ON purge_jobs
FOR EACH ROW EXECUTE FUNCTION set_row_updated_at();

CREATE TRIGGER annotators_reject_delete
BEFORE DELETE ON annotators
FOR EACH ROW EXECUTE FUNCTION reject_governance_delete();

CREATE TRIGGER consents_reject_delete
BEFORE DELETE ON consents
FOR EACH ROW EXECUTE FUNCTION reject_governance_delete();

CREATE TRIGGER audit_events_reject_mutation
BEFORE UPDATE OR DELETE ON audit_events
FOR EACH ROW EXECUTE FUNCTION reject_audit_mutation();

CREATE INDEX annotators_status_idx
    ON annotators(status, created_at);
CREATE INDEX consents_annotator_status_idx
    ON consents(annotator_id, status, created_at DESC);
CREATE INDEX consents_expiry_idx
    ON consents(expires_at)
    WHERE status = 'active' AND expires_at IS NOT NULL;
CREATE INDEX consents_retention_idx
    ON consents(retention_state, purge_requested_at)
    WHERE retention_state <> 'retained';
CREATE INDEX trusted_catalog_lookup_idx
    ON trusted_catalog_versions(kind, catalog_id, status, created_at DESC);
CREATE INDEX trusted_catalog_game_idx
    ON trusted_catalog_versions(game_id, kind, status)
    WHERE game_id IS NOT NULL;
CREATE INDEX devices_annotator_status_idx
    ON devices(annotator_id, status, last_seen_at DESC);
CREATE INDEX capture_grants_lookup_idx
    ON capture_grants(annotator_id, device_id, game_id, status, valid_until);
CREATE INDEX capture_grants_consent_idx
    ON capture_grants(consent_id, status);
CREATE INDEX sessions_consent_status_idx
    ON sessions(consent_id, status, created_at DESC)
    WHERE consent_id IS NOT NULL;
CREATE INDEX sessions_grant_idx
    ON sessions(capture_grant_id)
    WHERE capture_grant_id IS NOT NULL;
CREATE INDEX sessions_status_created_idx
    ON sessions(status, created_at);
CREATE INDEX capture_runs_session_idx
    ON capture_runs(session_id, run_seq);
CREATE INDEX clips_run_idx
    ON clips(capture_run_id, segment_seq);
CREATE INDEX clips_ingest_idx
    ON clips(ingest_state, verification_state, created_at);
CREATE INDEX clips_retention_idx
    ON clips(retention_state, purge_requested_at)
    WHERE retention_state <> 'retained';
CREATE INDEX clips_retention_deadline_idx
    ON clips(retention_deadline)
    WHERE retention_state = 'retained';
CREATE INDEX artifact_objects_state_idx
    ON artifact_objects(storage_state, verification_state, created_at);
CREATE INDEX artifact_objects_clip_state_idx
    ON artifact_objects(clip_id, storage_state, verification_state);
CREATE INDEX idempotency_requests_expiry_idx
    ON idempotency_requests(expires_at);
CREATE INDEX idempotency_requests_in_progress_idx
    ON idempotency_requests(locked_at)
    WHERE state = 'in_progress';
CREATE UNIQUE INDEX upload_intents_one_active_idx
    ON upload_intents(clip_id)
    WHERE status IN ('pending', 'uploading', 'completing');
CREATE INDEX upload_intents_expiry_idx
    ON upload_intents(status, expires_at)
    WHERE status IN ('pending', 'uploading', 'completing');
CREATE UNIQUE INDEX multipart_uploads_one_active_idx
    ON multipart_uploads(artifact_object_id)
    WHERE status IN ('initiated', 'uploading', 'completing');
CREATE INDEX multipart_uploads_expiry_idx
    ON multipart_uploads(status, expires_at)
    WHERE status IN ('initiated', 'uploading', 'completing');
CREATE INDEX upload_parts_reconcile_idx
    ON upload_parts(multipart_upload_id, status, part_number);
CREATE UNIQUE INDEX verification_jobs_one_active_idx
    ON verification_jobs(artifact_object_id)
    WHERE status IN ('pending', 'running', 'retry_wait');
CREATE INDEX verification_jobs_poll_idx
    ON verification_jobs(status, available_at)
    WHERE status IN ('pending', 'retry_wait');
CREATE INDEX outbox_jobs_poll_idx
    ON outbox_jobs(status, available_at)
    WHERE status IN ('pending', 'retry_wait');
CREATE UNIQUE INDEX purge_jobs_one_active_consent_idx
    ON purge_jobs(consent_id)
    WHERE scope = 'consent' AND status IN ('pending', 'running', 'retry_wait');
CREATE UNIQUE INDEX purge_jobs_one_active_session_idx
    ON purge_jobs(session_id)
    WHERE scope = 'session' AND status IN ('pending', 'running', 'retry_wait');
CREATE UNIQUE INDEX purge_jobs_one_active_clip_idx
    ON purge_jobs(clip_id)
    WHERE scope = 'clip' AND status IN ('pending', 'running', 'retry_wait');
CREATE INDEX purge_jobs_poll_idx
    ON purge_jobs(status, available_at)
    WHERE status IN ('pending', 'retry_wait');
CREATE INDEX audit_events_entity_idx
    ON audit_events(entity_type, entity_id, occurred_at DESC);
CREATE INDEX audit_events_consent_idx
    ON audit_events(consent_id, occurred_at DESC)
    WHERE consent_id IS NOT NULL;
CREATE INDEX audit_events_request_idx
    ON audit_events(request_id)
    WHERE request_id IS NOT NULL;

-- This view is the sole schema-level trainability decision. It is deliberately
-- fail-closed and read-only: clients never submit or update is_trainable.
CREATE VIEW clip_trainability AS
WITH artifact_rollup AS (
    SELECT
        clip_id,
        count(*) AS object_count,
        bool_and(storage_state = 'stored') AS all_stored,
        bool_and(verification_state = 'verified') AS all_verified
    FROM artifact_objects
    GROUP BY clip_id
),
facts AS (
    SELECT
        c.id AS clip_id,
        s.id AS session_id,
        ARRAY_REMOVE(ARRAY[
            CASE WHEN s.server_data_classification <> 'production_candidate' THEN 'source_non_trainable' END,
            CASE WHEN s.annotator_id IS NULL THEN 'missing_annotator' END,
            CASE WHEN a.id IS NULL OR a.status <> 'active' THEN 'annotator_inactive' END,
            CASE WHEN con.id IS NULL THEN 'missing_consent' END,
            CASE WHEN con.id IS NOT NULL AND con.status <> 'active' THEN 'consent_inactive' END,
            CASE WHEN con.id IS NOT NULL AND NOT con.allows_trainable_use THEN 'consent_disallows_trainable' END,
            CASE WHEN con.expires_at IS NOT NULL AND con.expires_at <= now() THEN 'consent_expired' END,
            CASE WHEN con.id IS NOT NULL AND con.retention_state <> 'retained' THEN 'consent_purge_active' END,
            CASE WHEN g.id IS NULL THEN 'missing_capture_grant' END,
            CASE WHEN g.id IS NOT NULL AND g.status <> 'active' THEN 'capture_grant_inactive' END,
            CASE WHEN g.id IS NOT NULL AND NOT g.allows_trainable THEN 'capture_grant_disallows_trainable' END,
            CASE
                WHEN g.id IS NOT NULL
                    AND NOT (s.started_at >= g.valid_from AND s.started_at < g.valid_until)
                THEN 'capture_outside_grant_window'
            END,
            CASE
                WHEN g.id IS NOT NULL
                    AND (g.game_id <> s.game_id OR g.capture_profile_version_id <> s.capture_catalog_version_id)
                THEN 'capture_grant_scope_mismatch'
            END,
            CASE WHEN d.status <> 'active' THEN 'device_inactive' END,
            CASE WHEN NOT game.allowed_for_capture OR NOT game.approved_for_trainable THEN 'game_unapproved' END,
            CASE
                WHEN dv.status NOT IN ('approved', 'retired') OR NOT dv.approved_for_trainable
                THEN 'device_profile_unapproved'
            END,
            CASE
                WHEN cp.status NOT IN ('approved', 'retired') OR NOT cp.approved_for_trainable
                THEN 'capture_profile_unapproved'
            END,
            CASE
                WHEN sp.status NOT IN ('approved', 'retired') OR NOT sp.approved_for_trainable
                THEN 'settings_profile_unapproved'
            END,
            CASE
                WHEN mp.status NOT IN ('approved', 'retired') OR NOT mp.approved_for_trainable
                THEN 'mapping_unapproved'
            END,
            CASE WHEN c.ingest_state <> 'stored' THEN 'clip_not_stored' END,
            CASE WHEN c.verification_state <> 'verified' THEN 'clip_not_verified' END,
            CASE WHEN c.quality_state <> 'passed' THEN 'quality_gate_not_passed' END,
            CASE WHEN c.retention_state <> 'retained' THEN 'clip_purge_active' END,
            CASE WHEN c.retention_deadline <= now() THEN 'clip_retention_expired' END,
            CASE WHEN COALESCE(ar.object_count, 0) <> 5 THEN 'artifact_set_incomplete' END,
            CASE WHEN NOT COALESCE(ar.all_stored, false) THEN 'artifact_not_stored' END,
            CASE WHEN NOT COALESCE(ar.all_verified, false) THEN 'artifact_not_verified' END
        ], NULL)::text[] AS blockers
    FROM clips c
    JOIN sessions s ON s.id = c.session_id
    JOIN devices d ON d.id = s.device_id
    JOIN games game ON game.id = s.game_id
    JOIN trusted_catalog_versions dv ON dv.id = s.device_catalog_version_id
    JOIN trusted_catalog_versions cp ON cp.id = s.capture_catalog_version_id
    JOIN trusted_catalog_versions sp ON sp.id = s.settings_catalog_version_id
    JOIN trusted_catalog_versions mp ON mp.id = s.mapping_catalog_version_id
    LEFT JOIN annotators a ON a.id = s.annotator_id
    LEFT JOIN consents con ON con.id = s.consent_id
    LEFT JOIN capture_grants g ON g.id = s.capture_grant_id
    LEFT JOIN artifact_rollup ar ON ar.clip_id = c.id
)
SELECT
    clip_id,
    session_id,
    cardinality(blockers) = 0 AS is_trainable,
    blockers
FROM facts;

COMMENT ON VIEW clip_trainability IS
    'Read-only, fail-closed server derivation. Five verified artifacts, consent, grant, catalog, QA, and retention gates must all pass.';

INSERT INTO schema_migrations(version, name)
VALUES (1, 'initial_control_plane');

COMMIT;
