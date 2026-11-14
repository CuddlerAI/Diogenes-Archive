import { randomUUID } from "node:crypto";

import { badRequest, conflict, notFound } from "./errors.js";
import type {
  CreateAnnotatorInput,
  CreateConsentInput,
  CreateDeviceInput,
  ReviewVerdictInput,
  UpdateAnnotatorInput,
  UpdateDeviceInput,
} from "./governance-contracts.js";
import type {
  CatalogRelease,
  CatalogVersion,
  GovernanceRepository,
  IdempotentReplay,
  PurgeOutcome,
  QualityOutcome,
  ReviewVerdict,
  SessionQualityOutcome,
  WrittenAnnotator,
  WrittenConsent,
  WrittenDevice,
} from "./governance-domain.js";
import type { SqlClient } from "./sql.js";

const idempotencyTtlHours = 24;

/**
 * 把一次设备写的结果投影进 mirror_credential_outbox。
 *
 * 拼进业务语句的 CTE 里，而不是跟在后面单发一条 INSERT：跨系统写不可能与 SQL
 * 事务原子提交，但 outbox 行与业务行**必须**同生同死。PostgreSQL 单语句即原子，
 * 因此可写 CTE 正好给出这个保证，也不需要给只有 `query` 的 SqlClient 加事务接口。
 *
 * 少了它，「Cloud SQL 里停用了、D1 里还活着」会静默存在，而且没有任何地方
 * 记得要重试。
 */
const DEVICE_OUTBOX_CTE = `
  outbox AS (
    INSERT INTO mirror_credential_outbox(
        device_id, credential_fingerprint, device_status, annotator_status, retire_others
    )
    SELECT written.id,
           written.credential_fingerprint,
           written.status,
           COALESCE(annotator.status::text, 'unbound'),
           $RETIRE$
    FROM written
    LEFT JOIN annotators annotator ON annotator.id = written.annotator_id
  )`;

function withDeviceOutbox(businessCte: string, retireOthers: boolean): string {
  return `WITH written AS (
    ${businessCte}
  ),${DEVICE_OUTBOX_CTE.replace("$RETIRE$", retireOthers ? "true" : "false")}
  SELECT id, client_device_id, annotator_id, status, credential_fingerprint FROM written`;
}

export class PostgresGovernanceRepository implements GovernanceRepository {
  readonly #pool: SqlClient;

  constructor(pool: SqlClient) {
    this.#pool = pool;
  }

  async createAnnotator(input: CreateAnnotatorInput): Promise<WrittenAnnotator> {
    const result = await this.#pool.query<AnnotatorRow>(
      `INSERT INTO annotators(id, alias, external_subject_hash, status)
       VALUES ($1, $2, $3, 'active')
       RETURNING id, alias, status, created_at`,
      [randomUUID(), input.alias, input.externalSubjectHash ?? null],
    );
    return toAnnotator(required(result.rows[0], "annotator_not_created"));
  }

  async updateAnnotator(id: string, input: UpdateAnnotatorInput): Promise<WrittenAnnotator> {
    // 停用一个 annotator 必须一次带走它名下的整个设备队列——设备数不设上限，
    // 逐台去 updateDevice 早晚会漏。扇出到该 annotator 当前的每一台设备，
    // 让镜像的 annotator_status 与这里保持一致。alias-only 的改动也会发一遍，
    // 那是幂等的重投，比「判断哪些字段值得发」少一个出错的地方。
    const result = await this.#pool.query<AnnotatorRow>(
      `WITH written AS (
         UPDATE annotators
            SET alias = COALESCE($2, alias),
                status = COALESCE($3, status),
                -- 表上有 CHECK (status <> 'tombstoned' OR tombstoned_at IS NOT NULL)，
                -- 而这条语句原先从不设时间戳——也就是说注销一个 annotator 必然违反
                -- 约束、直接抛错。至今没被发现是因为 updateAnnotatorSchema 只放行
                -- active/suspended，这条路在 API 上根本走不到；但 SQL 本身是不自洽的，
                -- 而下面的兜底清除正要靠它。
                tombstoned_at = CASE
                  WHEN $3 = 'tombstoned' THEN COALESCE(tombstoned_at, transaction_timestamp())
                  ELSE tombstoned_at
                END
          WHERE id = $1 AND status <> 'tombstoned'
          RETURNING id, alias, status, created_at
       ), outbox AS (
         INSERT INTO mirror_credential_outbox(
             device_id, credential_fingerprint, device_status, annotator_status, retire_others
         )
         SELECT device.id, device.credential_fingerprint, device.status, written.status::text, false
         FROM written
         JOIN devices device ON device.annotator_id = written.id
       ), purged_telemetry AS (
         -- Tombstoning is the end of the relationship, so it takes the telemetry
         -- with it. This is also the only thing that reaches the events recorded
         -- *before* any consent existed — signup and enrolment — which a consent
         -- purge cannot see, because their consent_id is null (11 §6.4 records
         -- this gap explicitly). Everything else is left to the consent purge.
         UPDATE client_events
            SET retention_state = 'purged',
                purged_at = transaction_timestamp(),
                details = '{}'::jsonb
          WHERE annotator_id IN (SELECT id FROM written WHERE status = 'tombstoned')
            AND retention_state <> 'purged'
          RETURNING 1
       )
       SELECT id, alias, status, created_at FROM written`,
      [id, input.alias ?? null, input.status ?? null],
    );
    return toAnnotator(required(result.rows[0], "annotator_not_found"));
  }

  async createConsent(input: CreateConsentInput): Promise<WrittenConsent> {
    const result = await this.#pool.query<ConsentRow>(
      `INSERT INTO consents(id, annotator_id, policy_version, purpose, terms_hash, status,
                            allows_trainable_use, accepted_at, expires_at, retention_deadline)
       VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9)
       RETURNING id, annotator_id, policy_version, status, allows_trainable_use,
                 accepted_at, expires_at`,
      [
        randomUUID(),
        input.annotatorId,
        input.policyVersion,
        input.purpose,
        input.termsHash,
        input.allowsTrainableUse,
        input.acceptedAt,
        input.expiresAt ?? null,
        input.retentionDeadline ?? null,
      ],
    );
    return toConsent(required(result.rows[0], "consent_not_created"));
  }

  /**
   * Revoking consent also erases the telemetry recorded under it.
   *
   * 11 §6.4 is explicit that behaviour telemetry must be reachable by a consent
   * purge — it is the whole reason `client_events` is a separate table from
   * `audit_events`, which is deliberately *kept* after a purge. The schema was
   * built for it (a `retention_state`, a partial index for exactly this sweep)
   * and then nothing ever performed the erasure: revoking flipped a status and
   * every telemetry row stayed intact, indefinitely.
   *
   * One writable CTE rather than two statements, because a cross-statement
   * failure would leave consent revoked and its telemetry retained — the
   * silent half-state compliance work cannot afford. Both land in one
   * transaction or neither does.
   *
   * `details` is emptied rather than the row deleted, matching how clips are
   * purged: `retention_state = 'purged'` plus a tombstone that records the
   * event existed. What is erased is the payload — the part that carries
   * anything about the person.
   */
  async revokeConsent(id: string, reason: string): Promise<WrittenConsent> {
    const result = await this.#pool.query<ConsentRow>(
      `WITH revoked AS (
         UPDATE consents
            SET status = 'revoked',
                revoked_at = transaction_timestamp(),
                revoke_reason = $2
          WHERE id = $1 AND status <> 'revoked'
          RETURNING id, annotator_id, policy_version, status, allows_trainable_use,
                    accepted_at, expires_at
       ), purged_telemetry AS (
         UPDATE client_events
            SET retention_state = 'purged',
                purged_at = transaction_timestamp(),
                details = '{}'::jsonb
          WHERE consent_id IN (SELECT id FROM revoked)
            AND retention_state <> 'purged'
          RETURNING 1
       )
       SELECT * FROM revoked`,
      [id, reason],
    );
    return toConsent(required(result.rows[0], "consent_not_found"));
  }

  async createDevice(
    input: CreateDeviceInput,
    credentialFingerprint: string,
  ): Promise<WrittenDevice> {
    const result = await this.#pool.query<DeviceRow>(
      withDeviceOutbox(
        // Device registration owns identity and credentials only. The nullable
        // legacy profile column is retained for audit, not populated here.
        `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                             credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                             display_snapshot, input_devices_snapshot, client_version)
         VALUES ($1::uuid, $2, $3::uuid, NULL, $4, 'pending', $5, $6, $7, $8::jsonb, $9::jsonb, $10)
         RETURNING id, client_device_id, annotator_id, status, credential_fingerprint`,
        false,
      ),
      [
        randomUUID(),
        input.clientDeviceId,
        input.annotatorId ?? null,
        credentialFingerprint,
        input.windowsBuild,
        input.gpuPciId,
        input.gpuDriver,
        JSON.stringify(input.displaySnapshot),
        JSON.stringify(input.inputDevicesSnapshot),
        input.clientVersion,
      ],
    );
    return toDevice(required(result.rows[0], "device_not_created"));
  }

  async updateDevice(id: string, input: UpdateDeviceInput): Promise<WrittenDevice> {
    const annotatorId = input.annotatorId === "none" ? null : (input.annotatorId ?? undefined);
    const result = await this.#pool.query<DeviceRow>(
      withDeviceOutbox(
        `UPDATE devices
            SET status = COALESCE($2, status),
                annotator_id = CASE WHEN $4::boolean THEN $3 ELSE annotator_id END,
                retired_at = CASE WHEN $2 = 'retired' THEN transaction_timestamp() ELSE retired_at END
          WHERE id = $1
          RETURNING id, client_device_id, annotator_id, status, credential_fingerprint`,
        false,
      ),
      [id, input.status ?? null, annotatorId ?? null, input.annotatorId !== undefined],
    );
    return toDevice(required(result.rows[0], "device_not_found"));
  }

  async rotateDeviceCredential(id: string, credentialFingerprint: string): Promise<WrittenDevice> {
    const result = await this.#pool.query<DeviceRow>(
      // 轮换：retire_others 让 publisher 在 upsert 新凭据后立刻作废旧 fingerprint。
      withDeviceOutbox(
        `UPDATE devices SET credential_fingerprint = $2
          WHERE id = $1 AND status <> 'retired'
          RETURNING id, client_device_id, annotator_id, status, credential_fingerprint`,
        true,
      ),
      [id, credentialFingerprint],
    );
    return toDevice(required(result.rows[0], "device_not_found"));
  }

  async recordClipQuality(
    clipId: string,
    result: "passed" | "failed",
    reviewedBy: string,
  ): Promise<QualityOutcome> {
    // Only the quality column moves. Transport, verification, and retention are
    // owned by the ingest service and its worker, and the console must not be
    // able to nudge a clip toward eligibility from here.
    const updated = await this.#pool.query<QualityRow>(
      `UPDATE clips
          SET quality_state = $2,
              quality_reviewed_at = transaction_timestamp(),
              quality_reviewed_by = $3
        WHERE id = $1 AND verification_state = 'verified'
        RETURNING id, quality_state, quality_reviewed_at, quality_reviewed_by`,
      [clipId, result, reviewedBy],
    );
    const row = updated.rows[0];
    if (!row) {
      const exists = await this.#pool.query<{ verification_state: string }>(
        `SELECT verification_state FROM clips WHERE id = $1`,
        [clipId],
      );
      if (!exists.rows[0]) throw notFound("clip_not_found", "the clip was not found");
      throw conflict(
        "clip_not_verified",
        "a quality decision requires a clip whose transport verification has passed",
      );
    }
    return {
      clipId: row.id,
      qualityState: row.quality_state,
      reviewedBy: row.quality_reviewed_by,
      reviewedAt: row.quality_reviewed_at.toISOString(),
    };
  }

  async recordSessionQuality(
    sessionId: string,
    result: "passed" | "failed",
    reviewedBy: string,
  ): Promise<SessionQualityOutcome> {
    // 同一条 WHERE 条件，只是主语从一条 clip 换成一场录制：仍然只有通过传输验证
    // 的 clip 会被判定，未验证的照旧留 `pending`。逐 clip 入口（recordClipQuality）
    // 保留，用于"这场大体没问题但第 37 条废了"这类例外。
    //
    // 写成一条可写 CTE 而不是「先 UPDATE 再数一遍」：PostgreSQL 单语句即原子，
    // 一场录制的判定不能出现"改了一半"。子查询与 UPDATE 共享同一快照且互不可见，
    // 但这里数的是 `verification_state`——UPDATE 一列都不碰它——所以 skipped 的口径
    // 是干净的。
    const outcome = await this.#pool.query<SessionQualityRow>(
      `WITH decided AS (
         UPDATE clips
            SET quality_state = $2,
                quality_reviewed_at = transaction_timestamp(),
                quality_reviewed_by = $3
          WHERE session_id = $1 AND verification_state = 'verified'
          RETURNING id, quality_reviewed_at
       )
       SELECT (SELECT count(*)::int FROM decided) AS decided_count,
              (SELECT max(quality_reviewed_at) FROM decided) AS reviewed_at,
              (SELECT count(*)::int FROM clips
                WHERE session_id = $1 AND verification_state <> 'verified') AS skipped_count,
              EXISTS (SELECT 1 FROM sessions WHERE id = $1) AS session_exists`,
      [sessionId, result, reviewedBy],
    );
    const row = required(outcome.rows[0], "session_quality_not_evaluated");
    if (!row.session_exists) throw notFound("session_not_found", "the session was not found");
    if (row.decided_count === 0 || row.reviewed_at === null) {
      // 判定没有落到任何一条 clip 上。回一条明确的 409 而不是"成功判定了 0 条"：
      // 后者会让 operator 以为整场已经审完。
      throw conflict(
        "session_has_no_verified_clip",
        "a quality decision requires at least one clip in this session whose transport verification has passed",
      );
    }
    return {
      sessionId,
      qualityState: result,
      reviewedBy,
      reviewedAt: row.reviewed_at.toISOString(),
      decidedClipCount: row.decided_count,
      skippedClipCount: row.skipped_count,
    };
  }

  async requestClipPurge(
    clipId: string,
    reason: string,
    operatorId: string,
  ): Promise<PurgeOutcome> {
    const clip = await this.#pool.query<{ retention_state: string }>(
      `SELECT retention_state FROM clips WHERE id = $1`,
      [clipId],
    );
    const current = clip.rows[0];
    if (!current) throw notFound("clip_not_found", "the clip was not found");
    if (current.retention_state === "purged" || current.retention_state === "purging") {
      return { clipId, retentionState: current.retention_state, enqueued: false };
    }

    const transitioned = await this.#pool.query<{ retention_state: string }>(
      `UPDATE clips
          SET retention_state = 'purge_requested',
              purge_requested_at = COALESCE(purge_requested_at, transaction_timestamp())
        WHERE id = $1 AND retention_state = 'retained'
        RETURNING retention_state`,
      [clipId],
    );

    // The worker owns execution. The console only enqueues, and a job that is
    // already queued is not duplicated.
    const job = await this.#pool.query<{ id: string }>(
      `INSERT INTO purge_jobs(id, scope, clip_id, reason, requested_by_type, requested_by_id)
       SELECT $1, 'clip', $2, $3, 'operator', $4
        WHERE NOT EXISTS (
          SELECT 1 FROM purge_jobs
           WHERE scope = 'clip' AND clip_id = $2
             AND status IN ('pending', 'running', 'retry_wait')
        )
       RETURNING id`,
      [randomUUID(), clipId, reason, operatorId],
    );

    return {
      clipId,
      retentionState: transitioned.rows[0]?.retention_state ?? current.retention_state,
      enqueued: job.rows.length === 1,
    };
  }

  async recordReviewVerdict(
    input: ReviewVerdictInput,
    operatorId: string,
    requestId: string | null,
  ): Promise<ReviewVerdict> {
    // Append-only by design: a verdict is corrected by a newer row, never by
    // rewriting history (docs/33 §5). No UPDATE path exists on this table.
    const result = await this.#pool.query<ReviewVerdictRow>(
      `INSERT INTO review_verdicts(id, source, subject_kind, subject_key, result, note,
                                   operator_id, request_id)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
       RETURNING id, source, subject_kind, subject_key, result, note, operator_id,
                 request_id, created_at`,
      [
        randomUUID(),
        input.source,
        input.subjectKind,
        input.subjectKey,
        input.result,
        input.note ?? null,
        operatorId,
        requestId,
      ],
    );
    return toReviewVerdict(result.rows[0]!, null);
  }

  async listReviewVerdicts(
    source: string,
    subjectKey: string,
    limit: number,
  ): Promise<ReviewVerdict[]> {
    const result = await this.#pool.query<ReviewVerdictRow & { email: string | null }>(
      `SELECT v.id, v.source, v.subject_kind, v.subject_key, v.result, v.note,
              v.operator_id, v.request_id, v.created_at, o.email
         FROM review_verdicts v
         LEFT JOIN operators o ON o.id = v.operator_id
        WHERE v.source = $1 AND v.subject_key = $2
        ORDER BY v.created_at DESC, v.id DESC
        LIMIT $3`,
      [source, subjectKey, limit],
    );
    return result.rows.map((row) => toReviewVerdict(row, row.email));
  }

  async listCatalogVersions(kind?: string, gameId?: string): Promise<CatalogVersion[]> {
    const result = await this.#pool.query<CatalogRow>(
      // The governance timestamps are read alongside the state so the console
      // can answer "who approved this, when" instead of only "is it approved".
      // `snapshot` stays out: it is kilobytes of jsonb per row and the list
      // view has no use for it.
      `SELECT trusted.id, trusted.kind, trusted.catalog_id, trusted.version, trusted.game_id,
              trusted.status, trusted.content_hash, trusted.approved_for_trainable,
              trusted.approved_by, trusted.approved_at, trusted.retired_at, trusted.revoked_at,
              trusted.created_at, trusted.updated_at,
              payload.trusted_catalog_version_id IS NOT NULL AS exact_payload_available,
              payload.byte_size AS payload_byte_size
         FROM trusted_catalog_versions trusted
         LEFT JOIN catalog_payloads payload
           ON payload.trusted_catalog_version_id = trusted.id
        WHERE ($1::text IS NULL OR trusted.kind = $1::catalog_kind)
          AND ($2::text IS NULL OR trusted.game_id = $2)
        ORDER BY trusted.catalog_id, trusted.kind, trusted.version`,
      [kind ?? null, gameId ?? null],
    );
    return result.rows.map((row) => ({
      id: row.id,
      kind: row.kind,
      catalogId: row.catalog_id,
      version: row.version,
      gameId: row.game_id,
      status: row.status,
      contentHash: row.content_hash,
      exactPayloadAvailable: row.exact_payload_available,
      payloadByteSize: row.payload_byte_size === null ? null : Number(row.payload_byte_size),
      publishable: row.exact_payload_available && row.status !== "revoked",
      approvedForTrainable: row.approved_for_trainable,
      approvedBy: row.approved_by,
      approvedAt: row.approved_at?.toISOString() ?? null,
      retiredAt: row.retired_at?.toISOString() ?? null,
      revokedAt: row.revoked_at?.toISOString() ?? null,
      createdAt: row.created_at.toISOString(),
      updatedAt: row.updated_at.toISOString(),
    }));
  }

  async listCatalogReleases(channel?: string): Promise<CatalogRelease[]> {
    const result = await this.#pool.query<CatalogReleaseRow>(
      `SELECT release.id, release.channel, release.catalog_id, release.catalog_version,
              release.status, release.envelope_sha256,
              release.signing_key_id, release.created_by, release.published_by,
              release.published_at, release.superseded_at, release.revoked_by,
              release.revoked_at, release.created_at,
              count(entry.trusted_catalog_version_id)::integer AS entry_count,
              count(*) FILTER (WHERE entry.kind = 'game_profile')::integer AS game_count
         FROM catalog_releases release
         LEFT JOIN catalog_release_entries entry ON entry.release_id = release.id
        WHERE ($1::text IS NULL OR release.channel = $1)
        GROUP BY release.id
        ORDER BY release.channel, release.created_at DESC`,
      [channel ?? null],
    );
    return result.rows.map((row) => ({
      id: row.id,
      channel: row.channel,
      catalogId: row.catalog_id,
      catalogVersion: row.catalog_version,
      status: row.status,
      envelopeSha256: row.envelope_sha256,
      signingKeyId: row.signing_key_id,
      entryCount: row.entry_count,
      gameCount: row.game_count,
      createdBy: row.created_by,
      publishedBy: row.published_by,
      publishedAt: row.published_at?.toISOString() ?? null,
      supersededAt: row.superseded_at?.toISOString() ?? null,
      revokedBy: row.revoked_by,
      revokedAt: row.revoked_at?.toISOString() ?? null,
      createdAt: row.created_at.toISOString(),
    }));
  }

  async beginIdempotent(
    scope: string,
    key: string,
    requestHash: string,
  ): Promise<IdempotentReplay | null> {
    const inserted = await this.#pool.query<{ id: string }>(
      `INSERT INTO idempotency_requests(id, scope, key, request_hash, state, expires_at)
       VALUES ($1, $2, $3, $4, 'in_progress',
               transaction_timestamp() + make_interval(hours => $5))
       ON CONFLICT (scope, key) DO NOTHING
       RETURNING id`,
      [randomUUID(), scope, key, requestHash, idempotencyTtlHours],
    );
    if (inserted.rows.length === 1) return null;

    const existing = await this.#pool.query<{
      request_hash: string;
      state: string;
      response_body: unknown;
    }>(
      `SELECT request_hash, state, response_body FROM idempotency_requests
        WHERE scope = $1 AND key = $2`,
      [scope, key],
    );
    const row = existing.rows[0];
    if (!row) throw conflict("idempotency_race", "retry the request");
    if (row.request_hash !== requestHash) {
      throw conflict(
        "idempotency_key_reused",
        "this idempotency key was already used with a different request body",
      );
    }
    if (row.state === "in_progress") {
      throw conflict("request_in_progress", "an identical request is still being processed");
    }
    return { status: "replayed", body: row.response_body };
  }

  async finishIdempotent(scope: string, key: string, body: unknown): Promise<void> {
    await this.#pool.query(
      `UPDATE idempotency_requests
          SET state = 'completed',
              response_status = 200,
              response_body = $3::jsonb,
              completed_at = transaction_timestamp()
        WHERE scope = $1 AND key = $2 AND state = 'in_progress'`,
      [scope, key, JSON.stringify(body ?? null)],
    );
  }

  async releaseIdempotent(scope: string, key: string): Promise<void> {
    // A failed attempt frees the key so the operator can correct the request and
    // resubmit with the same one.
    await this.#pool.query(
      `DELETE FROM idempotency_requests
        WHERE scope = $1 AND key = $2 AND state = 'in_progress'`,
      [scope, key],
    );
  }
}

type AnnotatorRow = { id: string; alias: string | null; status: string; created_at: Date };
type ConsentRow = {
  id: string;
  annotator_id: string;
  policy_version: string;
  status: string;
  allows_trainable_use: boolean;
  accepted_at: Date | null;
  expires_at: Date | null;
};
type DeviceRow = {
  id: string;
  client_device_id: string;
  annotator_id: string | null;
  status: string;
  credential_fingerprint: string;
};
type QualityRow = {
  id: string;
  quality_state: string;
  quality_reviewed_at: Date;
  quality_reviewed_by: string;
};
type SessionQualityRow = {
  decided_count: number;
  reviewed_at: Date | null;
  skipped_count: number;
  session_exists: boolean;
};
type ReviewVerdictRow = {
  id: string;
  source: string;
  subject_kind: string;
  subject_key: string;
  result: string;
  note: string | null;
  operator_id: string;
  request_id: string | null;
  created_at: Date;
};
type CatalogRow = {
  id: string;
  kind: string;
  catalog_id: string;
  version: string;
  game_id: string | null;
  status: string;
  content_hash: string;
  exact_payload_available: boolean;
  payload_byte_size: string | number | null;
  approved_for_trainable: boolean;
  approved_by: string | null;
  approved_at: Date | null;
  retired_at: Date | null;
  revoked_at: Date | null;
  created_at: Date;
  updated_at: Date;
};

type CatalogReleaseRow = {
  id: string;
  channel: string;
  catalog_id: string;
  catalog_version: string;
  status: string;
  envelope_sha256: string;
  signing_key_id: string;
  entry_count: number;
  game_count: number;
  created_by: string;
  published_by: string | null;
  published_at: Date | null;
  superseded_at: Date | null;
  revoked_by: string | null;
  revoked_at: Date | null;
  created_at: Date;
};

function toAnnotator(row: AnnotatorRow): WrittenAnnotator {
  return {
    id: row.id,
    alias: row.alias,
    status: row.status,
    createdAt: row.created_at.toISOString(),
  };
}

function toConsent(row: ConsentRow): WrittenConsent {
  return {
    id: row.id,
    annotatorId: row.annotator_id,
    policyVersion: row.policy_version,
    status: row.status,
    allowsTrainableUse: row.allows_trainable_use,
    acceptedAt: row.accepted_at?.toISOString() ?? null,
    expiresAt: row.expires_at?.toISOString() ?? null,
  };
}

function toDevice(row: DeviceRow): WrittenDevice {
  return {
    id: row.id,
    clientDeviceId: row.client_device_id,
    annotatorId: row.annotator_id,
    status: row.status,
    credentialFingerprint: row.credential_fingerprint,
  };
}

function toReviewVerdict(row: ReviewVerdictRow, email: string | null): ReviewVerdict {
  return {
    id: row.id,
    source: row.source,
    subjectKind: row.subject_kind as ReviewVerdict["subjectKind"],
    subjectKey: row.subject_key,
    result: row.result as ReviewVerdict["result"],
    note: row.note,
    operatorId: row.operator_id,
    operatorEmail: email,
    createdAt: row.created_at.toISOString(),
  };
}


function required<T>(row: T | undefined, code: string): T {
  if (!row) throw notFound(code, "the target row was not found or is not in a writable state");
  return row;
}

export function assertWritable(condition: boolean, code: string, message: string): void {
  if (!condition) throw badRequest(code, message);
}
