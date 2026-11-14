import { randomUUID } from "node:crypto";

import { Pool, type PoolClient } from "pg";

import { currentCatalogReleaseSql } from "./catalog-delivery.js";
import { stableJsonSha256 } from "./checksum.js";
import { artifactKinds, requiredArtifactKinds } from "./contracts.js";
import type {
  ArtifactKind,
  CreateSessionInput,
  PutClipInput,
  PutRunInput,
  PutSessionArtifactsInput,
  SealSessionInput,
  SessionArtifactKind,
  SignPartsInput,
} from "./contracts.js";
import {
  artifactContentTypes,
  artifactInput,
  type ArtifactRecord,
  type ClipRecord,
  type CurrentCatalogRelease,
  type IdempotencyResponse,
  type IngestionRepository,
  type MultipartCompletionRecord,
  type MultipartRecord,
  type PurgeJob,
  type PurgeProgress,
  type RunRecord,
  type SessionArtifactRecord,
  type SessionRecord,
  type UploadIntentRecord,
  type VerificationJob,
} from "./domain.js";
import {
  sessionArtifactObjectKey,
  type AdmissionFamily,
} from "./environment.js";
import { ApiError, badRequest, conflict, notFound, retryableConflict } from "./errors.js";
import { uploadFailureDisposition } from "./verification-faults.js";

type JsonRecord = Record<string, unknown>;

const PROCESS_LOOPBACK_AUDIO_PROFILE: Readonly<JsonRecord> = {
  mode: "process_loopback",
  sample_rate_hz: 48_000,
  channels: 2,
  bits_per_sample: 16,
  sample_format: "pcm_s16le",
  channel_mask: 3,
  compression: "zstd",
};

// Exported so `repository.test.ts` executes this exact statement against a real
// PostgreSQL engine. A copy in the test would only prove that the copy works.
export const authenticateDeviceSql = `SELECT d.id, d.annotator_id
         FROM devices d
         LEFT JOIN annotators a ON a.id = d.annotator_id
        WHERE d.credential_fingerprint = $1
          AND d.status IN ('pending', 'active')
          AND (d.annotator_id IS NULL OR a.status = 'active')`;

// The signed catalog says what the client can recognize. This query says what
// the ingestion service currently permits, so an approval/pause takes effect
// without rebuilding the catalog or desktop client.
export const gameAdmissionsSql = `SELECT id, allowed_for_capture
         FROM games
        ORDER BY id`;

/**
 * 心跳节流窗口：一台设备最多每这么久落一次 `last_seen_at`。
 *
 * 5 分钟是在两个方向之间取的：再短就是把读路径变成写路径（每个 `/v1/*` 请求都
 * UPDATE），再长「谁在线」就答不准。以 08-06 的实测节奏（单日 1,669 clip、每 clip
 * 约 18 次请求）算，节流前约 3 万次写/天，节流后每台设备上限 288 次/天。
 */
export const DEVICE_HEARTBEAT_THROTTLE_SECONDS = 300;

/**
 * 记一次设备心跳。**没有这条语句之前，`devices.last_seen_at` 全表恒为 NULL。**
 *
 * # 为什么它此前不存在
 *
 * 这一列从 0001 就有定义，还带着 `devices(annotator_id, status, last_seen_at DESC)`
 * 索引，`pipelineOverview` 也一直在用它算「24h / 7d 活跃设备」——但**全仓没有任何
 * 写入方**（`grep -rn last_seen_at apps/api/` 只命中建表与索引）。于是总览页那两个
 * 磁贴恒为 0，而运营分不清「这个人今天没干活」与「这个人的客户端每次都被拒」。
 * docs/101 §3.2 的 `wangharp` 就是这条洞的成本。
 *
 * # 为什么放在鉴权路径上，而不是让客户端单独上报
 *
 * 客户端已经有 `heartbeat.jsonl`，但那是**录制产物**、随 session 上传，答的是
 * 「那次录制期间画面在不在动」。「这台机器现在还活着吗」需要的是服务端**自己观察到**
 * 的事实——而每一次 `/v1/*` 都已经带着设备凭据打到这里了，这就是最诚实、也最省事的
 * 观察点：**不新增任何客户端上报通道，也就没有「客户端忘了报」这种失效模式**。
 *
 * # 三条刻意的选择
 *
 * 1. **节流谓词写在 WHERE 里，不在应用层判**。应用层判就要先读一次，多一次往返，
 *    而且两个实例并发时仍会各写一次。放进 WHERE 让数据库自己决定，通常匹配 0 行。
 * 2. **只认已鉴权设备的 id**，不接受任意 id：这条语句在鉴权成功之后才跑。
 * 3. **失败不影响请求**。心跳是可观测性，不是准入。调用方吞掉错误——
 *    让一次统计写失败去拒绝一次真实采集，是把主次弄反了。
 */
export const touchDeviceLastSeenSql = `UPDATE devices
          SET last_seen_at = transaction_timestamp()
        WHERE id = $1
          AND (last_seen_at IS NULL
               OR last_seen_at < transaction_timestamp() - make_interval(secs => $2::int))`;

// 查询本体搬到了 catalog-delivery：console 面的绑定前通道读的必须是同一张表、
// 同一条谓词。这里保持导出，repository.test.ts 仍然拿它去打真库。
export { currentCatalogReleaseSql };

// The identity triple is appended after `started_at` rather than placed next to
// `device_id`, so the parameter numbering stays sequential and a reader can
// check the column list against the values list by counting straight down.
//
// `started_at` gets $25 here. It previously had *no* placeholder at all: the
// column list carried 27 names against 26 expressions, so PostgreSQL rejected
// every insert with 42601 and `POST /v1/sessions` could not succeed against a
// real database. Nothing caught it because the repository had no SQL tests and
// the service tests mock this layer — see `repository.test.ts`.
/**
 * 无 consent 遥测的期限清扫（docs/11 §6.4）。
 *
 * 与其它 purge 一样是墓碑而不是删行：载荷消失，事件存在过这个事实留下。
 * `FOR UPDATE SKIP LOCKED` 让多个 worker 实例并发跑同一条也不会互相阻塞或重复计数。
 */
export const sweepUnconsentedTelemetrySql = `UPDATE client_events
      SET retention_state = 'purged',
          purged_at = transaction_timestamp(),
          details = '{}'::jsonb
    WHERE id IN (
      SELECT id
        FROM client_events
       WHERE consent_id IS NULL
         AND retention_state <> 'purged'
         AND received_at <= transaction_timestamp() - make_interval(days => $1)
       ORDER BY received_at
       FOR UPDATE SKIP LOCKED
       LIMIT $2
    )
    RETURNING id`;

export const createSessionSql = `INSERT INTO sessions(
           id, client_session_id, device_id, game_id, game_version, game_executable_sha256,
           catalog_id, catalog_version, catalog_mode, catalog_manifest_sha256, game_profile_sha256,
           device_catalog_version_id, recording_catalog_version_id,
           mapping_catalog_version_id, schema_version, manifest_json, manifest_sha256,
           server_data_classification, status, capture_engine, native_width, native_height,
           fps_num, fps_den, client_version, started_at,
           annotator_id, consent_id, capture_grant_id,
           view_scale_sha256, view_scale_verdict, view_scale_deviation_pct, view_scale_polling_hz,
           game_executable_sha256_missing_reason
         ) VALUES (
           $1, $2, $3, $4, $5, $6,
           $7, $8, $9, $10, $11,
           $12, $13, $14, $15, $16, $17,
           $28, 'registered', $18, $19, $20, $21, $22, $23, $24,
           $25, $26, $27,
           $29, $30, $31, $32,
           $33
         ) ON CONFLICT (device_id, client_session_id) DO NOTHING`;

/**
 * Fill-once for the run outcome columns (docs/67 §3.2).
 *
 * `COALESCE(existing, new)` is the whole rule: a NULL column accepts the first
 * value offered, a populated one keeps what it has. Exported so the test runs
 * this statement instead of a retyped copy — "never overwrites" is the property
 * that matters and it lives entirely in the SQL.
 */
export const runOutcomeFillOnceSql = `UPDATE capture_runs
        SET frame_count = COALESCE(frame_count, $3),
            stop_reason = COALESCE(stop_reason, $4),
            error_code  = COALESCE(error_code, $5)
      WHERE session_id = $1
        AND run_seq = $2
        AND (frame_count IS NULL OR stop_reason IS NULL OR error_code IS NULL)`;

/**
 * The consent that authorized this annotator's capture at the moment it started
 * (docs/108 §3.4).
 *
 * This replaces `activeGrantForSessionSql`, which looked up a per-annotator ×
 * device × game `capture_grants` row. That lane has had no writer since
 * docs/100 removed its issuing endpoint, so it always returned nothing — and
 * because it was the only source of `sessions.consent_id`, every session was
 * recorded with no consent at all. Removing the gate had also removed the
 * ledger.
 *
 * No game predicate, deliberately: the signed document is not written per game.
 * `annotator-2026-07-25-teaser.txt` does not name a single title; it authorizes
 * the data layer the contributor themselves produces. Which games may be
 * captured is answered elsewhere and by three independent facts — the catalog is
 * signed server-side, it is only served to enrolled devices, and a game enters it
 * only by an operator's explicit `--allow-capture` (docs/100 §2.2).
 *
 * Picking by time window rather than by `status = 'active'` is what makes this
 * deterministic rather than a guess. Renewal supersedes the previous row inside
 * one transaction (0031), so the windows never overlap and exactly one consent
 * can contain any given instant. That is also why the old comment here — that
 * deriving a consent without a grant would mean "picking one of an annotator's
 * consents arbitrarily" — no longer holds: there is nothing to pick between.
 *
 * Withdrawal is a veto and it is retroactive: the terms promise that withdrawing
 * purges data already captured, so a revoked consent authorizes nothing, not
 * even in hindsight. Being *superseded* is not withdrawal and is not a veto.
 *
 * Exported for the same reason as `authenticateDeviceSql`: every predicate is
 * load-bearing, and the test executes this statement rather than a retyped copy.
 */
export const activeConsentForSessionSql = `SELECT id, policy_version, terms_hash, scopes
       FROM consents
      WHERE annotator_id = $1
        AND accepted_at IS NOT NULL
        AND accepted_at <= $2
        AND (superseded_at IS NULL OR $2 < superseded_at)
        AND revoked_at IS NULL
        AND retention_state = 'retained'
      ORDER BY accepted_at DESC, id DESC
      LIMIT 1`;

// docs/30 §5.6. Exported for the same reason as `activeGrantForSessionSql`:
// every predicate here is load-bearing and silently getting one wrong would let
// a restart proceed over a live provider window. The test runs this statement,
// not a retyped copy.
//
// `$3` is the completion-lease window in seconds — the same ownership window
// purge respects (0002). A multipart that entered `completing` inside that
// window may still be finishing at the provider, so a restart must NOT take
// over; outside it the window is stale and takeover is allowed.
export const uploadRestartFenceSql = `SELECT
          c.id AS clip_id,
          ui.id AS intent_id,
          ui.status::text AS intent_status,
          ui.attempt_no,
          ui.expires_at <= transaction_timestamp() AS intent_expired,
          EXISTS (
            SELECT 1 FROM multipart_uploads mp
             WHERE mp.upload_intent_id = ui.id
               AND mp.status = 'completing'
               AND mp.completion_started_at
                     > transaction_timestamp() - make_interval(secs => $3)
          ) AS completion_window_live,
          EXISTS (
            SELECT 1 FROM artifact_objects ao
             WHERE ao.clip_id = c.id
               AND ao.storage_state IN ('stored', 'purging', 'purged')
          ) AS artifacts_beyond_upload,
          d.status IN ('pending','active') AS device_allowed,
          c.retention_state = 'retained' AS retention_allowed
     FROM clips c
     JOIN sessions s ON s.id = c.session_id
     JOIN devices d ON d.id = s.device_id
     JOIN upload_intents ui ON ui.clip_id = c.id
    WHERE c.id = $1 AND d.credential_fingerprint = $2
    ORDER BY ui.attempt_no DESC
    LIMIT 1`;

// `upload_intents_one_active_idx` (0001) already allows only one live intent
// per clip, and `multipart_uploads_one_active_idx` only one live multipart per
// artifact. That is the schema-level reason a client cannot quietly rebuild an
// intent: the second INSERT collides. A restart therefore has to *retire* the
// dead attempt first, and these two statements are that step. They run only
// after the provider-side abort has been confirmed, so the row state never
// claims an abort that did not happen.
export const abortIntentMultipartsSql = `UPDATE multipart_uploads
        SET status = 'aborted',
            aborted_at = transaction_timestamp(),
            completion_lease_id = NULL,
            completion_started_at = NULL,
            last_error_code = 'restarted_after_expiry'
      WHERE upload_intent_id = $1
        AND status IN ('initiated', 'uploading', 'completing')`;

export const terminateExpiredIntentSql = `UPDATE upload_intents
        SET status = 'expired',
            last_error_code = 'restarted_after_expiry'
      WHERE id = $1
        AND status IN ('pending', 'uploading', 'completing')
        AND expires_at <= transaction_timestamp()`;

// The reset that makes a new attempt legal. It is deliberately narrow: it only
// moves artifacts that are still `uploading` back to `pending` and clears the
// provider identity that the dead attempt wrote. An artifact that already
// reached `stored` is NOT touched — that case is refused upstream, because
// rewinding it would contradict the object's immutability.
export const resetArtifactsForRestartSql = `UPDATE artifact_objects
        SET storage_state = 'pending',
            provider_etag = NULL,
            provider_version_id = NULL,
            last_error_code = NULL
      WHERE clip_id = $1 AND storage_state = 'uploading'`;

// docs/66 裁-3（2026-08-03）：过期的 upload intent 不再是 purge 触发条件。此前维护清扫
// 会把「intent/multipart 已过期」的 clip 以 reason 'upload_expired' 入队删除——客户端只是
// 离线超过 24h，已传分片就被服务端删掉，而本地副本还在（docs/65 J-31：净丢失云端路径，
// 因为 clip 墓碑化后重传会被永久拒绝）。现在唯一的入队条件是 clip 自身的
// `retention_deadline`（建行时就带的兜底期限）；过期 attempt 的续传能力由 docs/30 §5.6
// 的 fence 在后续批次恢复。导出供 PGlite 测试直接运行本语句，不重打副本。
export const maintenancePurgeCandidatesSql = `SELECT c.id,
                'retention_expired' AS reason
           FROM clips c
          WHERE c.retention_state = 'retained'
            AND c.retention_deadline <= transaction_timestamp()
          ORDER BY c.retention_deadline, c.created_at
            FOR UPDATE OF c SKIP LOCKED
          LIMIT $1`;

/**
 * Verification must use the same deployment-owned classification allowlist as
 * upload admission and multipart completion. Keeping a literal
 * `non_trainable` here used to purge every valid `training_candidate` clip as
 * soon as its first stored artifact entered the worker queue.
 *
 * Exported so the regression test executes the production statements against a
 * real PostgreSQL engine rather than checking a retyped predicate.
 */
export const governanceBlockedVerificationJobsSql = `SELECT v.artifact_object_id, ao.clip_id,
                CASE
                  WHEN NOT (s.server_data_classification::text = ANY($2::text[]))
                    THEN 'classification_blocked'
                  ELSE 'retention_blocked'
                END AS rejection_code
           FROM verification_jobs v
           JOIN artifact_objects ao ON ao.id = v.artifact_object_id
           JOIN clips c ON c.id = ao.clip_id
           JOIN sessions s ON s.id = c.session_id
          WHERE v.status IN ('pending','retry_wait')
            AND v.available_at <= transaction_timestamp()
            AND (
              NOT (s.server_data_classification::text = ANY($2::text[]))
              OR c.retention_state <> 'retained'
              OR c.retention_deadline <= transaction_timestamp()
            )
          ORDER BY v.available_at, v.created_at
          FOR UPDATE OF v, c SKIP LOCKED
          LIMIT $1`;

export const verificationJobCandidatesSql = `SELECT v.id, v.artifact_object_id, v.attempt_count, ao.object_key,
                ao.expected_size_bytes, ao.expected_sha256, ao.kind, ao.clip_id,
                s.client_session_id, s.schema_version AS session_schema_version,
                r.client_run_id, c.segment_seq, c.session_frame_start,
                c.n_frames, c.qpc_start, c.qpc_end, c.event_count,
                r.width, r.height, r.fps_num, r.fps_den, c.health_json,
                s.manifest_json AS session_manifest_json,
                c.meta_json AS clip_meta_json
           FROM verification_jobs v
           JOIN artifact_objects ao ON ao.id = v.artifact_object_id
           JOIN clips c ON c.id = ao.clip_id
           JOIN capture_runs r ON r.id = c.capture_run_id
           JOIN sessions s ON s.id = c.session_id
          WHERE v.status IN ('pending','retry_wait') AND v.available_at <= transaction_timestamp()
            AND v.attempt_count < v.max_attempts
            AND c.verification_state <> 'rejected'
            AND s.server_data_classification::text = ANY($2::text[])
            AND c.retention_state = 'retained'
            AND c.retention_deadline > transaction_timestamp()
            AND (
              ao.kind <> 'manifest'
              OR NOT EXISTS (
                SELECT 1 FROM artifact_objects sibling
                 WHERE sibling.clip_id = ao.clip_id AND sibling.kind <> 'manifest'
                   AND sibling.verification_state <> 'verified'
              )
            )
          ORDER BY v.available_at, v.created_at
          FOR UPDATE OF v, c SKIP LOCKED
          LIMIT $1`;

// PostgreSQL cannot infer one untyped parameter when it is used both as an
// enum assignment and inside an equality expression (42P08). Keep the cast in
// the shared statement so production and the real-engine regression test use
// the exact same bind contract.
export const finishArtifactVerificationSql = `UPDATE artifact_objects
    SET verification_state = $2::verification_state,
        verified_at = CASE
          WHEN $2::verification_state = 'verified'::verification_state
            THEN transaction_timestamp()
          ELSE NULL
        END,
        last_error_code = $3
  WHERE id = $1`;

// Provider-side CompleteMultipartUpload can fail after consuming the upload
// without creating the final object. The replacement keeps the same artifact
// row/key, advances only its opaque generation, and clears the old completion
// fence. Exported so the real-PostgreSQL regression test executes this exact SQL.
export const restartMultipartAttemptSql = `UPDATE multipart_uploads
    SET attempt_no = attempt_no + 1, provider_upload_id = $2, status = 'initiated',
        expires_at = transaction_timestamp() + interval '24 hours',
        completed_at = NULL, aborted_at = NULL,
        completion_lease_id = NULL, completion_started_at = NULL,
        last_error_code = 'provider_upload_restarted'
  WHERE id = $1 AND status = 'completing' AND completion_lease_id = $3
  RETURNING attempt_no`;

export const resetMultipartPartsForProviderRestartSql = `UPDATE upload_parts
    SET status = 'pending', etag = NULL, uploaded_at = NULL, signed_expires_at = NULL
  WHERE multipart_upload_id = $1`;

const artifactExtensions: Record<ArtifactKind, string> = {
  video: "mp4",
  frames: "jsonl.zst",
  events: "jsonl.zst",
  meta: "json",
  manifest: "json",
  audio: "pcm.zst",
  depth: "zip",
};

export function clipArtifactObjectKey(input: {
  objectPrefix: string;
  sessionId: string;
  runId: string;
  clipId: string;
  kind: ArtifactKind;
  sha256: string;
}): string {
  const hashHex = input.sha256.slice("sha256:".length);
  return `${input.objectPrefix}/sessions/${input.sessionId}/runs/${input.runId}/clips/${input.clipId}/${input.kind}-${hashHex}.${artifactExtensions[input.kind]}`;
}

function isValidClipArtifactSet(kinds: readonly ArtifactKind[]): boolean {
  const unique = new Set(kinds);
  return (
    unique.size === kinds.length &&
    kinds.every((kind) => artifactKinds.includes(kind)) &&
    requiredArtifactKinds.every((kind) => unique.has(kind)) &&
    unique.size <= artifactKinds.length
  );
}

export class PostgresRepository implements IngestionRepository {
  readonly #pool: Pool;
  readonly #retentionDays: number;
  readonly #admittedClassifications: readonly string[];

  constructor(
    databaseUrl: string,
    retentionDays: number,
    /**
     * Every classification this deployment's admitted families can produce.
     * The classification ceiling itself is per-family (`family.bestClassification`,
     * passed into createSession); this set only widens the upload gate so a
     * `training_candidate` session admitted by a candidate-capable lane can still
     * move its bytes. A deployment that stops admitting the lane stops moving
     * its bytes too — fail-closed, loudly, not under another lane's prefix.
     */
    admittedClassifications: readonly string[] = ["non_trainable"],
  ) {
    this.#pool = new Pool({ connectionString: databaseUrl, max: 10 });
    this.#retentionDays = retentionDays;
    this.#admittedClassifications = admittedClassifications;
  }

  async close(): Promise<void> {
    await this.#pool.end();
  }

  async ping(): Promise<void> {
    await this.#pool.query("SELECT 1");
  }

  // The `/v1/*` gate. A bearer token is only valid if it hashes to a live
  // device credential, so revoking a device is a status change rather than a
  // secret rotation across every caller.
  //
  // The annotator join is the enforcement point for 11 §12.5: devices carry no
  // upper bound per annotator, so suspending an annotator has to invalidate its
  // whole fleet at once. Doing it here rather than with a batch UPDATE also
  // covers devices enrolled *after* the suspend, while a session token that was
  // issued earlier is still alive.
  async authenticateDevice(credentialFingerprint: string) {
    const result = await this.#pool.query<{
      id: string;
      annotator_id: string | null;
    }>(authenticateDeviceSql, [credentialFingerprint]);
    const row = result.rows[0];
    if (!row) return null;
    // 心跳与鉴权分成两条语句是有意的：鉴权按 fingerprint 查，心跳按已确认的 id 写。
    // 合并成一条 `UPDATE … RETURNING` 会让每一次未授权探测也进入写路径。
    await this.touchDeviceLastSeen(row.id);
    return { id: row.id, annotatorId: row.annotator_id, credentialFingerprint };
  }

  /**
   * 落一次设备心跳（节流见 [`DEVICE_HEARTBEAT_THROTTLE_SECONDS`]）。
   *
   * **吞掉失败**：这是可观测性写入，不是准入判定。让一次统计写失败去拒绝一次
   * 真实采集，是把主次弄反了——而采集是不可重来的。
   */
  async touchDeviceLastSeen(deviceId: string): Promise<void> {
    try {
      await this.#pool.query(touchDeviceLastSeenSql, [
        deviceId,
        DEVICE_HEARTBEAT_THROTTLE_SECONDS,
      ]);
    } catch {
      // 有意静默：调用方是鉴权路径，心跳失败不该改变任何请求的结果。
    }
  }

  async currentCatalogRelease(
    channel: "dev" | "certified",
  ): Promise<CurrentCatalogRelease[]> {
    const result = await this.#pool.query<{
      catalog_id: string;
      catalog_version: string;
      envelope_bytes: Buffer;
      envelope_sha256: string;
    }>(currentCatalogReleaseSql, [channel]);
    // 每游戏一份 bundle（docs/57 §3.3）：这条查询现在天然返回多行，下发面把整
    // 个集合发出去。断代前取 rows[0] 是对的——那时一个渠道就只有一份大目录。
    return result.rows.map((row) => ({
      catalogId: row.catalog_id,
      catalogVersion: row.catalog_version,
      envelopeBytes: Buffer.from(row.envelope_bytes),
      envelopeSha256: row.envelope_sha256,
    }));
  }

  async listGameAdmissions() {
    const result = await this.#pool.query<{
      id: string;
      allowed_for_capture: boolean;
    }>(gameAdmissionsSql);
    return result.rows.map((row) => ({
      gameId: row.id,
      admission: row.allowed_for_capture ? ("admitted" as const) : ("onboarding" as const),
      reason: row.allowed_for_capture ? null : ("capture_not_allowed" as const),
    }));
  }

  async getIdempotency(scope: string, key: string) {
    const result = await this.#pool.query<{
      request_hash: string;
      state: string;
      response_status: number | null;
      response_headers: Record<string, string>;
      response_body: unknown;
    }>(
      `SELECT request_hash, state, response_status, response_headers, response_body
         FROM idempotency_requests
        WHERE scope = $1 AND key = $2 AND expires_at > transaction_timestamp()`,
      [scope, key],
    );
    const row = result.rows[0];
    if (!row) return null;
    return {
      requestHash: row.request_hash,
      ...(row.state === "completed" && row.response_status !== null
        ? {
            response: {
              statusCode: row.response_status,
              body: row.response_body,
              ...(Object.keys(row.response_headers).length > 0 ? { headers: row.response_headers } : {}),
            },
          }
        : {}),
    };
  }

  async reserveIdempotency(scope: string, key: string, requestHash: string) {
    const result = await this.#pool.query(
      `INSERT INTO idempotency_requests(
         id, scope, key, request_hash, expires_at
       ) VALUES ($1, $2, $3, $4, transaction_timestamp() + interval '24 hours')
       ON CONFLICT (scope, key) DO UPDATE
         SET id = EXCLUDED.id,
             request_hash = EXCLUDED.request_hash,
             state = 'in_progress',
             response_status = NULL,
             response_headers = '{}'::jsonb,
             response_body = NULL,
             locked_at = transaction_timestamp(),
             completed_at = NULL,
             expires_at = EXCLUDED.expires_at
       WHERE idempotency_requests.expires_at <= transaction_timestamp()
       RETURNING id`,
      [randomUUID(), scope, key, requestHash],
    );
    return result.rowCount === 1 ? ("reserved" as const) : ("exists" as const);
  }

  async finishIdempotency(scope: string, key: string, response: IdempotencyResponse): Promise<void> {
    const result = await this.#pool.query(
      `UPDATE idempotency_requests
          SET state = 'completed', response_status = $3, response_headers = $4, response_body = $5,
              completed_at = transaction_timestamp()
        WHERE scope = $1 AND key = $2 AND state = 'in_progress'`,
      [scope, key, response.statusCode, response.headers ?? {}, response.body],
    );
    if (result.rowCount !== 1) {
      throw conflict("idempotency_not_reserved", "idempotency reservation was lost");
    }
  }

  async releaseIdempotency(scope: string, key: string, requestHash: string): Promise<void> {
    await this.#pool.query(
      `DELETE FROM idempotency_requests
        WHERE scope = $1 AND key = $2 AND request_hash = $3 AND state = 'in_progress'`,
      [scope, key, requestHash],
    );
  }

  async createSession(
    deviceId: string,
    input: CreateSessionInput,
    family: AdmissionFamily,
  ): Promise<SessionRecord> {
    return this.#transaction(async (client) => {
      const { device, deviceCatalog, gameCatalog, recordingCatalog, mappingCatalog } =
        await resolveSessionCatalogs(client, deviceId, input);
      assertRecordingProfileFacts(input.recordingProfile, recordingCatalog.snapshot);
      // legacy 车道由 session schema 世代声明有无音频；中性车道没有世代可声明
      // （issue #855 §B），所以这里传 null——录制档案配没配音频照旧由服务端快照
      // 回答，clip 侧的配对在 `putClip`。
      const declaredAudioGeneration =
        family.audioDeclaration === "session_schema"
          ? input.sessionSchemaVersion === family.audio?.sessionSchemaVersion
          : null;
      const profileConfiguresAudio = assertRecordingProfileAudioFacts(
        declaredAudioGeneration,
        recordingCatalog.snapshot,
      );
      // The identity pair comes entirely from server-side facts: the device row
      // names its annotator, and the annotator's own consent — the one whose
      // validity window contains `startedAt` — is the authorization. Nothing
      // here is read from the request body, so a caller cannot claim to be
      // someone else's annotator (09 §5.3 resource ownership).
      const consent = device.annotator_id
        ? await activeConsentForSession(client, device.annotator_id, input.startedAt)
        : null;
      // The client also says which consent it believed it was recording under.
      // The server's derivation wins — always — and a disagreement is recorded
      // rather than refused (docs/108 D4).
      //
      // Refusing was the old behaviour on the grant lane, and it is the wrong
      // trade here: a session registers after the bytes exist, so a 409 costs a
      // capture that cannot be re-run, and the most likely cause of a mismatch
      // is the benign one — somebody re-signed to widen their authorization
      // while the client still held the previous state. That is a diagnostic
      // signal about client staleness, and wiring a diagnostic signal to an
      // admission gate is exactly the failure direction docs/100 §2.1 rejected.
      if (
        input.declaredConsentId !== undefined &&
        consent !== null &&
        input.declaredConsentId !== consent.id
      ) {
        await audit(
          client,
          device.id,
          "session.consent_declaration_mismatch",
          "session",
          input.localSessionId,
          {
            declaredConsentId: input.declaredConsentId,
            boundConsentId: consent.id,
            startedAt: input.startedAt,
          },
        );
      }
      // A capture whose consent does not cover every modality it produces is
      // recorded, not refused (docs/108 D3). `clip_trainability` reports the
      // same shortfall as `consent_scope_insufficient` and keeps the clip out of
      // any release; writing it here as well is what lets an operator see the
      // shortfall at registration time instead of discovering it later from a
      // blocker list.
      const scopeShortfall = consent
        ? consentScopeShortfall(
            consent.scopes ?? [],
            requiredConsentScopes({
              carriesAudio: declaredAudioGeneration ?? profileConfiguresAudio,
            }),
          )
        : [];
      if (scopeShortfall.length > 0) {
        await audit(
          client,
          device.id,
          "session.consent_scope_shortfall",
          "session",
          input.localSessionId,
          {
            consentId: consent!.id,
            policyVersion: consent!.policy_version,
            missingScopes: scopeShortfall,
          },
        );
      }
      // Fail-closed derivation: the admitted family sets the ceiling, the
      // capture-time facts decide whether the session reaches it (issue #855 §B).
      const derived = await deriveSourceClassification(client, family, {
        gameId: input.game.id,
        annotatorId: device.annotator_id,
        consentId: consent?.id ?? null,
        catalogVersionIds: [
          deviceCatalog.id,
          gameCatalog.id,
          recordingCatalog.id,
          mappingCatalog.id,
        ],
      });
      const classification = derived.classification;
      const catalogMode = input.catalog.mode ?? null;
      const manifestHash = stableJsonSha256(input);
      const id = randomUUID();
      await client.query(
        createSessionSql,
        [
          id,
          input.localSessionId,
          device.id,
          input.game.id,
          input.game.version,
          input.game.executableSha256 ?? null,
          input.catalog.id,
          input.catalog.version,
          catalogMode,
          input.catalog.contentHash,
          input.game.contentHash,
          deviceCatalog.id,
          recordingCatalog.id,
          mappingCatalog.id,
          input.sessionSchemaVersion,
          input,
          manifestHash,
          input.recordingProfile.captureEngine,
          input.recordingProfile.width,
          input.recordingProfile.height,
          input.recordingProfile.fpsNum,
          input.recordingProfile.fpsDen,
          input.clientVersion,
          input.startedAt,
          device.annotator_id,
          consent?.id ?? null,
          // The per-game grant lane has no writer and is no longer consulted
          // (docs/108 D1). The column stays for the existing history it points at.
          null,
          classification,
          input.viewScaleCalibration?.sha256 ?? null,
          input.viewScaleCalibration?.verdict ?? null,
          input.viewScaleCalibration?.deviationPct ?? null,
          input.viewScaleCalibration?.pollingHzP50 ?? null,
          input.game.executableSha256MissingReason ?? null,
        ],
      );
      const result = await client.query<{
        id: string;
        client_session_id: string;
        status: string;
        manifest_sha256: string;
        created_at: Date;
        updated_at: Date;
      }>(
        `SELECT id, client_session_id, status, manifest_sha256, created_at, updated_at
           FROM sessions WHERE device_id = $1 AND client_session_id = $2`,
        [device.id, input.localSessionId],
      );
      const row = requiredRow(result.rows[0], "session_not_found", "session could not be read after creation");
      if (row.manifest_sha256 !== manifestHash) {
        throw conflict("session_identity_conflict", "localSessionId is already bound to different metadata");
      }
      await audit(client, deviceId, "session.registered", "session", row.id, {
        classification,
        // 为什么没到 candidate，逐条写进审计：一次「怎么又是 non_trainable」的排查
        // 不该退化成重跑一遍派生（docs/101 的同一条教训）。上限就是 non_trainable
        // 的车道只会得到 `lane_non_trainable` 一条。
        classificationWithheldBecause: derived.withheldBecause,
        sessionSchemaVersion: input.sessionSchemaVersion,
        catalogMode,
        // How the session was attributed, so console can tell "no annotator on
        // this device" apart from "annotator known but no consent covering the
        // moment capture started" without re-deriving it (11 §9).
        annotatorId: device.annotator_id,
        consentId: consent?.id ?? null,
        consentPolicyVersion: consent?.policy_version ?? null,
      });
      return {
        id: row.id,
        deviceId,
        localSessionId: row.client_session_id,
        state: sessionState(row.status),
        sessionSchemaVersion: input.sessionSchemaVersion,
        createdAt: iso(row.created_at),
        updatedAt: iso(row.updated_at),
      };
    });
  }

  async getSession(deviceId: string, sessionId: string): Promise<SessionRecord | null> {
    return this.#transaction(async (client) => {
      try {
        const session = await ownedSession(client, deviceId, sessionId, false);
        return {
          id: session.id,
          deviceId,
          localSessionId: session.client_session_id,
          state: sessionState(session.status),
          sessionSchemaVersion: session.schema_version,
          createdAt: iso(session.created_at),
          updatedAt: iso(session.updated_at),
        };
      } catch (error) {
        if (error instanceof ApiError && error.code === "session_not_found") return null;
        throw error;
      }
    });
  }

  async putRun(
    deviceId: string,
    sessionId: string,
    localRunId: string,
    input: PutRunInput,
  ): Promise<RunRecord> {
    return this.#transaction(async (client) => {
      const session = await ownedSession(client, deviceId, sessionId, true);
      if (Date.parse(input.endedAt) < Date.parse(input.startedAt)) {
        throw badRequest("invalid_run_time_range", "endedAt must be greater than or equal to startedAt");
      }
      if (
        session.capture_engine !== input.capture.captureEngine ||
        session.native_width !== input.capture.width ||
        session.native_height !== input.capture.height ||
        session.fps_num !== input.capture.fpsNum ||
        session.fps_den !== input.capture.fpsDen
      ) {
        throw conflict("run_recording_profile_mismatch", "run capture facts do not match the session catalog");
      }
      const id = randomUUID();
      const status = input.localState === "faulted" ? "faulted" : "completed";
      await client.query(
        `INSERT INTO capture_runs(
           id, session_id, client_run_id, run_seq, status, capture_engine, encoder_backend,
           width, height, fps_num, fps_den, qpc_frequency,
           preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256,
           session_frame_start, started_at, ended_at, frame_count, stop_reason, error_code
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)
          ON CONFLICT DO NOTHING`,
        [
          id,
          session.id,
          localRunId,
          input.sequence,
          status,
          input.capture.captureEngine,
          input.capture.encoderBackend,
          input.capture.width,
          input.capture.height,
          input.capture.fpsNum,
          input.capture.fpsDen,
          input.qpcFrequency,
          input.preflight.schemaVersion,
          input.preflight.id,
          input.preflight.result,
          input.preflight.evidenceHash,
          input.sessionFrameStart,
          input.startedAt,
          input.endedAt,
          input.frameCount ?? null,
          input.stopReason ?? null,
          input.errorCode ?? null,
        ],
      );
      // Fill-once (docs/67 §3.2). A run registered by an older client, or by a
      // retry that predates the outcome being known, still has NULL outcome
      // columns; a later PUT may fill them. It may never rewrite them — an
      // already-recorded stop reason is the run's history, and a client that
      // could restate it could launder a faulted run into a clean one.
      await client.query(runOutcomeFillOnceSql, [
        session.id,
        input.sequence,
        input.frameCount ?? null,
        input.stopReason ?? null,
        input.errorCode ?? null,
      ]);
      const result = await client.query<{
        id: string;
        session_id: string;
        client_run_id: string;
        run_seq: number;
        status: string;
        width: number;
        height: number;
        fps_num: number;
        fps_den: number;
        qpc_frequency: string;
        preflight_schema_version: string;
        preflight_id: string;
        preflight_result: string;
        preflight_evidence_sha256: string;
        session_frame_start: string;
        capture_engine: string;
        encoder_backend: string;
        started_at: Date;
        ended_at: Date;
        created_at: Date;
        updated_at: Date;
      }>(
        `SELECT id, session_id, client_run_id, run_seq, status, width, height, fps_num, fps_den,
                qpc_frequency, preflight_schema_version, preflight_id, preflight_result,
                preflight_evidence_sha256, session_frame_start, capture_engine, encoder_backend,
                started_at, ended_at, created_at, updated_at
           FROM capture_runs
          WHERE session_id = $1 AND (client_run_id = $2 OR run_seq = $3)
          ORDER BY (client_run_id = $2) DESC
          LIMIT 1`,
        [session.id, localRunId, input.sequence],
      );
      const row = requiredRow(result.rows[0], "run_not_found", "run could not be read after creation");
      if (
        row.client_run_id !== localRunId ||
        row.run_seq !== input.sequence ||
        row.status !== status ||
        row.width !== input.capture.width ||
        row.height !== input.capture.height ||
        row.fps_num !== input.capture.fpsNum ||
        row.fps_den !== input.capture.fpsDen ||
        Number(row.qpc_frequency) !== input.qpcFrequency ||
        row.preflight_schema_version !== input.preflight.schemaVersion ||
        row.preflight_id !== input.preflight.id ||
        row.preflight_result !== input.preflight.result ||
        row.preflight_evidence_sha256 !== input.preflight.evidenceHash ||
        Number(row.session_frame_start) !== input.sessionFrameStart ||
        row.capture_engine !== input.capture.captureEngine ||
        row.encoder_backend !== input.capture.encoderBackend ||
        iso(row.started_at) !== iso(input.startedAt) ||
        iso(row.ended_at) !== iso(input.endedAt)
      ) {
        throw conflict("run_identity_conflict", "localRunId is already bound to different metadata");
      }
      await audit(client, deviceId, "run.registered", "capture_run", row.id, {
        localRunId,
        preflightId: input.preflight.id,
      });
      return {
        id: row.id,
        sessionId: row.session_id,
        localRunId: row.client_run_id,
        sequence: row.run_seq,
        state: row.status === "completed" ? "closed" : "rejected",
        createdAt: iso(row.created_at),
        updatedAt: iso(row.updated_at),
      };
    });
  }

  async putClip(
    deviceId: string,
    sessionId: string,
    localRunId: string,
    segmentIndex: number,
    input: PutClipInput,
    objectPrefix: string,
    bucket: string,
    family: AdmissionFamily,
  ): Promise<ClipRecord> {
    const clipId = await this.#transaction(async (client) => {
      const session = await ownedSession(client, deviceId, sessionId, true);
      // The clip's declared lane (which chose objectPrefix upstream) must be the
      // lane that owns its session's persisted `schema_version`; otherwise a v2
      // clip could land under the raw/v1 prefix or vice versa. Routing reads the
      // stored column, never the request's catalog identity (issue #855 §B).
      const laneSessionSchemas = [
        family.sessionSchemaVersion,
        family.audio?.sessionSchemaVersion,
      ].filter((version): version is string => version !== undefined);
      if (!laneSessionSchemas.includes(session.schema_version)) {
        throw conflict(
          "classification_lane_mismatch",
          "clip classification does not match the session's admitted lane",
        );
      }
      // 中性车道没有音频世代，所以"这条 clip 该不该带音频"由服务端的 recording
      // profile 快照回答：档案配了音频就必须申报音频元数据（`state` 才回答这一段
      // 到底有没有采到），没配就不许申报。强度与 legacy 的世代配对相同，只是把
      // 判据从 schema 世代换成了服务端事实。
      if (
        family.audioDeclaration === "per_clip" &&
        session.recording_profile_has_audio !== (input.audio !== undefined)
      ) {
        throw conflict(
          "clip_audio_profile_mismatch",
          "clip audio metadata must be present exactly when the session recording profile configures audio",
        );
      }
      // 世代配对是家族数据，不是词表硬编码：音频代的 artifact schema 只能挂在
      // 音频代的 session 下，基础代同理——dev 与 certified 两条车道同一条规则。
      const requiredSessionSchema =
        family.audio !== undefined &&
        input.classification.artifactSchemaVersion === family.audio.artifactSchemaVersion
          ? family.audio.sessionSchemaVersion
          : family.sessionSchemaVersion;
      if (session.schema_version !== requiredSessionSchema) {
        throw conflict(
          "artifact_schema_mismatch",
          "clip artifact schema generation does not match its parent session",
        );
      }
      const runResult = await client.query<{ id: string; run_seq: number; session_frame_start: string }>(
        `SELECT r.id, r.run_seq, r.session_frame_start
           FROM capture_runs r
           JOIN sessions s ON s.id = r.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE r.session_id = $1 AND r.client_run_id = $2 AND d.credential_fingerprint = $3
          FOR UPDATE OF r`,
        [sessionId, localRunId, deviceId],
      );
      const run = requiredRow(runResult.rows[0], "run_not_found", "capture run was not found");
      if (input.sessionFrameStart < Number(run.session_frame_start)) {
        throw conflict("clip_frame_range_invalid", "clip starts before its capture run");
      }
      const id = randomUUID();
      await client.query(
        `INSERT INTO clips(
           id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start, n_frames,
           qpc_start, qpc_end, event_count, health_json, meta_json, manifest_json, manifest_sha256,
           retention_deadline
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,
                   transaction_timestamp() + make_interval(days => $15))
         ON CONFLICT (session_id, run_seq, segment_seq) DO NOTHING`,
        [
          id,
          sessionId,
          run.id,
          run.run_seq,
          segmentIndex,
          input.sessionFrameStart,
          input.nFrames,
          input.qpcStart,
          input.qpcEnd,
          input.eventCount,
          input.health,
          input,
          input.artifacts,
          input.artifacts.manifest.sha256,
          this.#retentionDays,
        ],
      );
      const clipResult = await client.query<{ id: string; meta_json: PutClipInput }>(
        `SELECT id, meta_json FROM clips WHERE session_id = $1 AND run_seq = $2 AND segment_seq = $3`,
        [sessionId, run.run_seq, segmentIndex],
      );
      const clip = requiredRow(clipResult.rows[0], "clip_not_found", "clip could not be read after creation");
      if (stableJsonSha256(clip.meta_json) !== stableJsonSha256(input)) {
        throw conflict("clip_identity_conflict", "segmentIndex is already bound to different metadata");
      }
      for (const kind of artifactKinds) {
        const artifact = artifactInput(input, kind);
        if (artifact === undefined) continue;
        const key = clipArtifactObjectKey({
          objectPrefix,
          sessionId,
          runId: run.id,
          clipId: clip.id,
          kind,
          sha256: artifact.sha256,
        });
        await client.query(
          `INSERT INTO artifact_objects(
             id, clip_id, kind, bucket_name, object_key, expected_sha256, expected_size_bytes
           ) VALUES ($1,$2,$3,$4,$5,$6,$7)
           ON CONFLICT (clip_id, kind) DO NOTHING`,
          [randomUUID(), clip.id, kind, bucket, key, artifact.sha256, artifact.byteSize],
        );
      }
      const count = await client.query<{ count: string }>(
        `SELECT count(*)::text AS count FROM artifact_objects WHERE clip_id = $1`,
        [clip.id],
      );
      const expectedArtifactCount = Object.keys(input.artifacts).length;
      if (Number(count.rows[0]?.count ?? 0) !== expectedArtifactCount) {
        throw conflict("artifact_set_incomplete", "clip does not contain its declared artifact set");
      }
      await audit(client, deviceId, "clip.declared", "clip", clip.id, { segmentIndex });
      return clip.id;
    });
    const clip = await this.getClip(deviceId, clipId);
    return requiredRow(clip, "clip_not_found", "clip could not be read after creation");
  }

  async getClip(deviceId: string, clipId: string): Promise<ClipRecord | null> {
    const result = await this.#pool.query<ClipJoinRow>(clipSelectSql, [clipId, deviceId]);
    if (result.rows.length === 0) return null;
    return mapClip(result.rows);
  }

  async requestClipPurge(deviceId: string, clipId: string): Promise<void> {
    await this.#transaction(async (client) => {
      const owned = await client.query(
        `SELECT c.id
           FROM clips c
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE c.id = $1 AND d.credential_fingerprint = $2
            AND d.status IN ('pending','active')
          FOR UPDATE OF c`,
        [clipId, deviceId],
      );
      if (owned.rowCount !== 1) {
        throw notFound("clip_not_found", "clip was not found");
      }
      await enqueueClipPurge(
        client,
        clipId,
        "device_principal_delete",
        "device",
        deviceId,
      );
    });
  }

  async sealSession(
    deviceId: string,
    sessionId: string,
    input: SealSessionInput,
  ): Promise<SessionRecord> {
    return this.#transaction(async (client) => {
      const session = await ownedSession(client, deviceId, sessionId, false);
      if (Date.parse(input.endedAt) < session.started_at.getTime()) {
        throw badRequest("invalid_session_end", "endedAt must be greater than or equal to session startedAt");
      }
      const rejected = input.localFinalState !== "completed";
      const status = rejected ? "rejected" : "completed";
      const reason = rejected ? (input.stopReason ?? `local_${input.localFinalState}`) : input.stopReason;
      if (isSessionSealed(session.status)) {
        if (
          session.status !== status ||
          session.ended_at === null ||
          iso(session.ended_at) !== iso(input.endedAt)
        ) {
          throw conflict("session_already_sealed", "session was sealed with a different terminal identity");
        }
        return {
          id: session.id,
          deviceId,
          localSessionId: session.client_session_id,
          state: sessionState(session.status),
          sessionSchemaVersion: session.schema_version,
          createdAt: iso(session.created_at),
          updatedAt: iso(session.updated_at),
        };
      }
      const result = await client.query<{
        id: string;
        client_session_id: string;
        created_at: Date;
        updated_at: Date;
      }>(
        `UPDATE sessions
            SET status = $2, ended_at = $3, sealed_at = transaction_timestamp(),
                stop_reason = $4, rejected_reason = $5
          WHERE id = $1
          RETURNING id, client_session_id, created_at, updated_at`,
        [session.id, status, input.endedAt, input.stopReason ?? null, rejected ? reason : null],
      );
      const row = requiredRow(result.rows[0], "session_not_found", "session could not be sealed");
      await audit(client, deviceId, "session.sealed", "session", row.id, { localFinalState: input.localFinalState });
      return {
        id: row.id,
        deviceId,
        localSessionId: row.client_session_id,
        state: sessionState(status),
        sessionSchemaVersion: session.schema_version,
        createdAt: iso(row.created_at),
        updatedAt: iso(row.updated_at),
      };
    });
  }

  async declareSessionArtifacts(
    deviceId: string,
    sessionId: string,
    input: PutSessionArtifactsInput,
    objectPrefix: string,
    keyStyle: AdmissionFamily["sessionArtifactKeyStyle"],
  ): Promise<SessionArtifactRecord[]> {
    return this.#transaction(async (client) => {
      const session = await ownedSession(client, deviceId, sessionId, false);
      if (session.status !== "completed") {
        throw conflict(
          "session_artifacts_require_completed_session",
          "session artifacts may be declared only after a successful session seal",
        );
      }
      // Declaration is immutable per (session, kind): a re-declare with the same
      // bytes is idempotent; the returned rows are always the stored truth.
      for (const [kind, declaration] of Object.entries(input.artifacts)) {
        if (!declaration) continue;
        const artifactKind = kind as SessionArtifactKind;
        await client.query(
          `INSERT INTO session_artifacts(
             id, session_id, kind, object_key, content_type, expected_sha256, expected_size_bytes
           ) VALUES ($1, $2, $3::session_artifact_kind, $4, $5, $6, $7)
           ON CONFLICT (session_id, kind) DO NOTHING`,
          [
            randomUUID(),
            sessionId,
            artifactKind,
            sessionArtifactObjectKey(
              objectPrefix,
              sessionId,
              SESSION_ARTIFACT_FILES[artifactKind],
              // 车道自己的 key 形状（issue #855 §B）：legacy 是历史的
              // `<prefix>/<session-id>/session/<file>`，raw/v2 是
              // `<prefix>/sessions/<session-id>/session/<file>`。显式传进来而不是
              // 靠默认值——一旦有第二种形状，"没写"就不再等于"我知道该用哪种"。
              keyStyle,
            ),
            sessionArtifactContentType(artifactKind),
            declaration.sha256,
            declaration.byteSize,
          ],
        );
      }
      const rows = await sessionArtifactRows(client, sessionId);
      const declarations = Object.entries(input.artifacts).filter((entry) => entry[1] !== undefined);
      if (rows.length !== declarations.length) {
        throw conflict(
          "session_artifact_identity_conflict",
          "session artifact set was already declared with different members",
        );
      }
      for (const [kind, declaration] of declarations) {
        const stored = rows.find((row) => row.kind === kind);
        if (
          !stored ||
          stored.expectedBytes !== declaration!.byteSize ||
          stored.expectedSha256 !== declaration!.sha256
        ) {
          throw conflict(
            "session_artifact_identity_conflict",
            `${kind} was already declared with different bytes`,
          );
        }
      }
      return rows;
    });
  }

  async getSessionArtifacts(deviceId: string, sessionId: string): Promise<SessionArtifactRecord[]> {
    return this.#transaction(async (client) => {
      await ownedSession(client, deviceId, sessionId, false);
      return sessionArtifactRows(client, sessionId);
    });
  }

  async getSessionArtifact(
    deviceId: string,
    sessionId: string,
    kind: SessionArtifactKind,
  ): Promise<SessionArtifactRecord | null> {
    return this.#transaction(async (client) => {
      await ownedSession(client, deviceId, sessionId, false);
      const rows = await sessionArtifactRows(client, sessionId, kind);
      return rows[0] ?? null;
    });
  }

  async finalizeSessionArtifact(
    deviceId: string,
    sessionId: string,
    kind: SessionArtifactKind,
    outcome:
      | { verified: true; actualBytes: number; actualSha256: string }
      | { verified: false; rejectionCode: string },
  ): Promise<SessionArtifactRecord> {
    return this.#transaction(async (client) => {
      await ownedSession(client, deviceId, sessionId, false);
      const result = outcome.verified
        ? await client.query(
            `UPDATE session_artifacts
                SET ingest_state = 'stored', stored_at = transaction_timestamp(),
                    verification_state = 'verified', verified_at = transaction_timestamp(),
                    rejected_reason = NULL
              WHERE session_id = $1 AND kind = $2::session_artifact_kind`,
            [sessionId, kind],
          )
        : await client.query(
            `UPDATE session_artifacts
                SET ingest_state = 'failed', verification_state = 'rejected', rejected_reason = $3
              WHERE session_id = $1 AND kind = $2::session_artifact_kind`,
            [sessionId, kind, outcome.rejectionCode],
          );
      if (result.rowCount !== 1) {
        throw notFound("session_artifact_not_found", "session artifact was not declared");
      }
      const row = (await sessionArtifactRows(client, sessionId, kind))[0];
      return requiredRow(row, "session_artifact_not_found", "session artifact was not declared");
    });
  }

  async assertUploadAllowed(deviceId: string, clipId: string): Promise<void> {
    const result = await this.#pool.query<UploadGateRow>(
      `SELECT d.status IN ('pending','active') AS device_allowed,
              s.server_data_classification::text AS session_classification,
              c.retention_state = 'retained' AS retention_allowed,
              c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
              true AS intent_not_expired,
              true AS multipart_not_expired
         FROM clips c
         JOIN sessions s ON s.id = c.session_id
         JOIN devices d ON d.id = s.device_id
        WHERE c.id = $1 AND d.credential_fingerprint = $2`,
      [clipId, deviceId],
    );
    assertUploadGate(requiredRow(result.rows[0], "clip_not_found", "clip was not found"), this.#admittedClassifications);
  }

  async beginUploadIntent(
    deviceId: string,
    clipId: string,
    uploads: ReadonlyArray<{
      artifactId: string;
      kind: ArtifactKind;
      partSizeBytes: number;
    }>,
  ): Promise<UploadIntentRecord> {
    const intentId = await this.#transaction(async (client) => {
      const clipResult = await client.query<{ id: string; revision: number } & UploadGateRow>(
        `SELECT c.id, c.revision,
                d.status IN ('pending','active') AS device_allowed,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
                true AS intent_not_expired,
                true AS multipart_not_expired
           FROM clips c
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE c.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF c`,
        [clipId, deviceId],
      );
      const clip = requiredRow(clipResult.rows[0], "clip_not_found", "clip was not found");
      assertUploadGate(clip, this.#admittedClassifications);
      const artifactResult = await client.query<{ id: string; kind: ArtifactKind; storage_state: string }>(
        `SELECT id, kind, storage_state FROM artifact_objects WHERE clip_id = $1 FOR UPDATE`,
        [clip.id],
      );
      const artifactSetValid = isValidClipArtifactSet(
        artifactResult.rows.map((artifact) => artifact.kind),
      );
      if (!artifactSetValid || artifactResult.rows.some((artifact) => artifact.storage_state !== "pending")) {
        throw conflict("clip_upload_started", "all declared clip artifacts must be pending");
      }
      const supplied = new Map(uploads.map((upload) => [upload.kind, upload]));
      if (uploads.length !== artifactResult.rows.length || supplied.size !== artifactResult.rows.length) {
        throw conflict("upload_plan_incomplete", "upload intent must contain every declared artifact");
      }
      const attemptResult = await client.query<{ attempt_no: number }>(
        `SELECT COALESCE(max(attempt_no), 0)::int + 1 AS attempt_no FROM upload_intents WHERE clip_id = $1`,
        [clip.id],
      );
      const attempt = attemptResult.rows[0]?.attempt_no ?? 1;
      const id = randomUUID();
      await client.query(
        `INSERT INTO upload_intents(id, clip_id, clip_revision, attempt_no, status, expires_at)
         VALUES ($1,$2,$3,$4,'uploading',transaction_timestamp() + interval '24 hours')`,
        [id, clip.id, clip.revision, attempt],
      );
      for (const artifact of artifactResult.rows) {
        const upload = supplied.get(artifact.kind);
        if (!upload || upload.artifactId !== artifact.id) {
          throw conflict("upload_plan_mismatch", `upload plan does not match ${artifact.kind} artifact`);
        }
        await client.query(
          `INSERT INTO multipart_uploads(
             id, upload_intent_id, artifact_object_id, attempt_no,
             part_size_bytes, status, expires_at
           ) VALUES ($1,$2,$3,$4,$5,'initiated',transaction_timestamp() + interval '24 hours')`,
          [randomUUID(), id, artifact.id, attempt, upload.partSizeBytes],
        );
      }
      await client.query(
        `UPDATE artifact_objects SET storage_state = 'uploading' WHERE clip_id = $1`,
        [clip.id],
      );
      await client.query(`UPDATE clips SET ingest_state = 'uploading' WHERE id = $1`, [clip.id]);
      await audit(client, deviceId, "upload_intent.created", "upload_intent", id, { clipId });
      return id;
    });
    const intent = await this.getUploadIntent(deviceId, intentId);
    return requiredRow(intent, "upload_intent_not_found", "upload intent could not be read after creation");
  }

  async bindMultipartProviderUpload(
    deviceId: string,
    multipartId: string,
    providerUploadId: string,
  ): Promise<void> {
    await this.#transaction(async (client) => {
      const result = await client.query<{
        provider_upload_id: string | null;
        multipart_status: string;
      } & UploadGateRow>(
        `SELECT mp.provider_upload_id, mp.status AS multipart_status,
                d.status IN ('pending','active') AS device_allowed,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
                ui.expires_at > transaction_timestamp() AS intent_not_expired,
                mp.expires_at > transaction_timestamp() AS multipart_not_expired
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE mp.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF mp, ui, c`,
        [multipartId, deviceId],
      );
      const row = requiredRow(result.rows[0], "upload_not_found", "multipart creation claim was not found");
      assertUploadGate(row, this.#admittedClassifications);
      if (row.multipart_status !== "initiated") {
        throw conflict("upload_intent_not_active", "multipart creation claim is no longer active");
      }
      if (row.provider_upload_id !== null && row.provider_upload_id !== providerUploadId) {
        throw conflict("provider_upload_conflict", "multipart creation claim is already bound");
      }
      if (row.provider_upload_id === null) {
        await client.query(
          `UPDATE multipart_uploads SET provider_upload_id = $2 WHERE id = $1`,
          [multipartId, providerUploadId],
        );
        await audit(client, deviceId, "multipart.provider_bound", "multipart_upload", multipartId, {});
      }
    });
  }

  async getUploadIntent(deviceId: string, uploadIntentId: string): Promise<UploadIntentRecord | null> {
    const result = await this.#pool.query<IntentJoinRow>(
      `SELECT ui.id AS intent_id, ui.clip_id, ui.status AS intent_status, ui.created_at AS intent_created_at,
              mp.id AS multipart_id, mp.artifact_object_id, mp.provider_upload_id,
              mp.status AS multipart_status, mp.attempt_no AS multipart_attempt_no,
              mp.part_size_bytes, mp.created_at AS multipart_created_at,
              ao.kind, ao.object_key, ao.expected_size_bytes,
              s.schema_version AS session_schema_version,
              d.status IN ('pending','active') AS device_allowed,
              s.server_data_classification::text AS session_classification,
              c.retention_state = 'retained' AS retention_allowed,
              c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
              ui.expires_at > transaction_timestamp() AS intent_not_expired,
              mp.expires_at > transaction_timestamp() AS multipart_not_expired
         FROM upload_intents ui
         JOIN clips c ON c.id = ui.clip_id
         JOIN sessions s ON s.id = c.session_id
         JOIN devices d ON d.id = s.device_id
         JOIN multipart_uploads mp ON mp.upload_intent_id = ui.id
         JOIN artifact_objects ao ON ao.id = mp.artifact_object_id
        WHERE ui.id = $1 AND d.credential_fingerprint = $2
        ORDER BY ao.kind`,
      [uploadIntentId, deviceId],
    );
    if (result.rows.length === 0) return null;
    return mapIntent(result.rows, this.#admittedClassifications);
  }

  async findUploadIntentForClip(deviceId: string, clipId: string): Promise<UploadIntentRecord | null> {
    const result = await this.#pool.query<{ id: string }>(
      `SELECT ui.id
         FROM upload_intents ui
         JOIN clips c ON c.id = ui.clip_id
         JOIN sessions s ON s.id = c.session_id
         JOIN devices d ON d.id = s.device_id
        WHERE ui.clip_id = $1 AND d.credential_fingerprint = $2
          AND ui.status IN ('pending','uploading','completing','completed')
          AND ui.expires_at > transaction_timestamp()
          AND c.retention_state = 'retained'
          AND c.retention_deadline > transaction_timestamp()
          AND s.server_data_classification::text = ANY($3::text[])
          AND d.status IN ('pending','active')
        ORDER BY ui.attempt_no DESC
        LIMIT 1`,
      [clipId, deviceId, this.#admittedClassifications],
    );
    const id = result.rows[0]?.id;
    return id ? this.getUploadIntent(deviceId, id) : null;
  }

  async recordSignedParts(
    deviceId: string,
    uploadIntentId: string,
    parts: SignPartsInput["parts"],
    minimumValiditySeconds: number,
  ): Promise<void> {
    await this.#transaction(async (client) => {
      const result = await client.query<{
        intent_status: string;
        multipart_id: string;
        kind: ArtifactKind;
      } & UploadGateRow>(
        `SELECT ui.status AS intent_status, mp.id AS multipart_id, ao.kind,
                d.status IN ('pending','active') AS device_allowed,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() + make_interval(secs => $3) AS before_retention_deadline,
                ui.expires_at > transaction_timestamp() + make_interval(secs => $3) AS intent_not_expired,
                mp.expires_at > transaction_timestamp() + make_interval(secs => $3) AS multipart_not_expired
           FROM upload_intents ui
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
           JOIN multipart_uploads mp ON mp.upload_intent_id = ui.id
           JOIN artifact_objects ao ON ao.id = mp.artifact_object_id
          WHERE ui.id = $1 AND d.credential_fingerprint = $2
            AND mp.provider_upload_id IS NOT NULL
          FOR UPDATE OF ui, mp, c`,
        [uploadIntentId, deviceId, minimumValiditySeconds],
      );
      if (!isValidClipArtifactSet(result.rows.map((row) => row.kind))) {
        throw notFound("upload_intent_not_found", "upload intent was not found");
      }
      result.rows.forEach((row) => assertUploadGate(row, this.#admittedClassifications));
      if (!result.rows.every((row) => row.intent_status === "uploading" || row.intent_status === "pending")) {
        throw conflict("upload_intent_not_active", "upload intent is no longer active");
      }
      const uploads = new Map(result.rows.map((row) => [row.kind, row.multipart_id]));
      for (const part of parts) {
        const multipartId = uploads.get(part.artifactType);
        if (!multipartId) {
          throw conflict("upload_plan_mismatch", `upload intent has no ${part.artifactType} upload`);
        }
        await client.query(
          `INSERT INTO upload_parts(
             multipart_upload_id, part_number, size_bytes, content_md5, signed_expires_at, status
           ) VALUES (
             $1,$2,$3,$4,
             transaction_timestamp() + make_interval(secs => $5),
             'pending'
           )
           ON CONFLICT (multipart_upload_id, part_number) DO UPDATE
             SET signed_expires_at = GREATEST(
                   upload_parts.signed_expires_at,
                   EXCLUDED.signed_expires_at
                 )
           WHERE upload_parts.size_bytes = EXCLUDED.size_bytes
             AND upload_parts.content_md5 = EXCLUDED.content_md5`,
          [multipartId, part.partNumber, part.byteSize, part.contentMd5, minimumValiditySeconds],
        );
        const persisted = await client.query<{ size_bytes: string; content_md5: string }>(
          `SELECT size_bytes, content_md5 FROM upload_parts
            WHERE multipart_upload_id = $1 AND part_number = $2`,
          [multipartId, part.partNumber],
        );
        const row = requiredRow(persisted.rows[0], "upload_part_not_found", "signed part was not persisted");
        if (Number(row.size_bytes) !== part.byteSize || row.content_md5 !== part.contentMd5) {
          throw conflict("upload_part_identity_conflict", "part number is already bound to different bytes or MD5");
        }
        await client.query(
          `UPDATE multipart_uploads SET status = 'uploading'
            WHERE id = $1 AND status = 'initiated'`,
          [multipartId],
        );
      }
      await audit(client, deviceId, "upload_parts.signed", "upload_intent", uploadIntentId, {
        partCount: parts.length,
      });
    });
  }

  async beginMultipartCompletion(
    deviceId: string,
    multipartId: string,
    completionLeaseSeconds: number,
  ): Promise<MultipartCompletionRecord> {
    return this.#transaction(async (client) => {
      const result = await client.query<IntentJoinRow & {
        completion_lease_id: string | null;
        completion_started_at: Date | null;
        completion_lease_fresh: boolean;
      }>(
        `SELECT ui.id AS intent_id, ui.clip_id, ui.status AS intent_status,
                ui.created_at AS intent_created_at,
                mp.id AS multipart_id, mp.artifact_object_id, mp.provider_upload_id,
                mp.status AS multipart_status, mp.attempt_no AS multipart_attempt_no,
                mp.part_size_bytes,
                mp.created_at AS multipart_created_at, mp.completion_lease_id,
                mp.completion_started_at,
                mp.completion_started_at >
                  transaction_timestamp() - make_interval(secs => $3) AS completion_lease_fresh,
                ao.kind, ao.object_key, ao.expected_size_bytes,
                d.status IN ('pending','active') AS device_allowed,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
                ui.expires_at > transaction_timestamp() AS intent_not_expired,
                mp.expires_at > transaction_timestamp() AS multipart_not_expired
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
           JOIN artifact_objects ao ON ao.id = mp.artifact_object_id
          WHERE mp.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF mp, ui, c`,
        [multipartId, deviceId, completionLeaseSeconds],
      );
      const row = requiredRow(result.rows[0], "upload_not_found", "multipart upload was not found");
      assertUploadGate(row, this.#admittedClassifications);
      if (row.provider_upload_id === null) {
        throw conflict("upload_intent_not_ready", "multipart provider creation is not bound yet");
      }
      if (row.multipart_status === "completed") {
        throw conflict("multipart_already_completed", "multipart upload is already completed");
      }
      if (!["initiated", "uploading", "completing"].includes(row.multipart_status)) {
        throw conflict("upload_intent_not_active", "multipart upload is no longer active");
      }
      if (
        row.multipart_status === "completing" &&
        row.completion_started_at !== null &&
        row.completion_lease_fresh
      ) {
        throw retryableConflict(
          "multipart_completion_in_progress",
          "multipart completion is already owned by another live request",
        );
      }
      const completionLeaseId = randomUUID();
      await client.query(
        `UPDATE multipart_uploads
            SET status = 'completing', completion_lease_id = $2,
                completion_started_at = transaction_timestamp(), last_error_code = NULL
          WHERE id = $1`,
        [multipartId, completionLeaseId],
      );
      await client.query(
        `UPDATE upload_intents SET status = 'completing'
          WHERE id = $1 AND status IN ('pending','uploading','completing')`,
        [row.intent_id],
      );
      await audit(client, deviceId, "multipart.completion_started", "multipart_upload", multipartId, {
        takeover: row.multipart_status === "completing",
      });
      return {
        id: row.multipart_id,
        uploadIntentId: row.intent_id,
        artifactId: row.artifact_object_id,
        clipId: row.clip_id,
        kind: row.kind,
        objectKey: row.object_key,
        contentType: artifactContentTypes[row.kind],
        providerUploadId: row.provider_upload_id,
        state: "initiated",
        attemptNo: row.multipart_attempt_no,
        partSizeBytes: Number(row.part_size_bytes),
        expectedBytes: Number(row.expected_size_bytes),
        createdAt: iso(row.multipart_created_at),
        completionLeaseId,
      };
    });
  }

  async completeMultipart(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    actualBytes: number,
    parts: ReadonlyArray<{ partNumber: number; etag: string; bytes: number }>,
  ): Promise<MultipartRecord> {
    const uploadIntentId = await this.#transaction(async (client) => {
      const result = await client.query<IntentJoinRow & { completion_lease_id: string | null }>(
        `SELECT ui.id AS intent_id, ui.clip_id, ui.status AS intent_status, ui.created_at AS intent_created_at,
                mp.id AS multipart_id, mp.artifact_object_id, mp.provider_upload_id,
                mp.status AS multipart_status, mp.attempt_no AS multipart_attempt_no,
                mp.part_size_bytes, mp.created_at AS multipart_created_at,
                mp.completion_lease_id,
                ao.kind, ao.object_key, ao.expected_size_bytes,
                d.status IN ('pending','active') AS device_allowed,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
                ui.expires_at > transaction_timestamp() AS intent_not_expired,
                mp.expires_at > transaction_timestamp() AS multipart_not_expired
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
           JOIN artifact_objects ao ON ao.id = mp.artifact_object_id
          WHERE mp.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF mp, ui, ao, c`,
        [multipartId, deviceId],
      );
      const row = requiredRow(result.rows[0], "upload_not_found", "multipart upload was not found");
      assertUploadGate(row, this.#admittedClassifications);
      if (
        row.intent_status !== "completing" ||
        row.multipart_status !== "completing" ||
        row.completion_lease_id !== completionLeaseId
      ) {
        throw conflict("multipart_completion_lease_lost", "multipart completion ownership changed");
      }
      for (const part of parts) {
          const updated = await client.query(
            `UPDATE upload_parts
                SET size_bytes = $3, etag = $4, status = 'uploaded', uploaded_at = transaction_timestamp()
              WHERE multipart_upload_id = $1 AND part_number = $2
                AND size_bytes = $3 AND content_md5 IS NOT NULL
              RETURNING id`,
            [multipartId, part.partNumber, part.bytes, part.etag],
          );
          if (updated.rowCount !== 1) {
            throw conflict("upload_part_not_signed", `part ${part.partNumber} was not signed with matching bytes`);
          }
      }
      await client.query(
          `UPDATE multipart_uploads
              SET status = 'completed', completed_at = transaction_timestamp(),
                  completion_lease_id = NULL, completion_started_at = NULL,
                  last_error_code = NULL
            WHERE id = $1 AND completion_lease_id = $2`,
          [multipartId, completionLeaseId],
        );
        await client.query(
          `UPDATE artifact_objects
              SET storage_state = 'stored', stored_at = transaction_timestamp(),
                  verification_state = 'queued',
                  last_error_code = CASE WHEN expected_size_bytes = $2 THEN NULL ELSE 'head_size_mismatch' END
            WHERE id = $1`,
          [row.artifact_object_id, actualBytes],
        );
        await client.query(
          `INSERT INTO verification_jobs(id, artifact_object_id, status)
           SELECT $1, $2, 'pending'
            WHERE NOT EXISTS (
              SELECT 1 FROM verification_jobs
               WHERE artifact_object_id = $2 AND status IN ('pending','running','retry_wait')
            )`,
          [randomUUID(), row.artifact_object_id],
        );
      await client.query(
        `UPDATE clips c
            SET ingest_state = CASE
                  WHEN NOT EXISTS (
                    SELECT 1 FROM artifact_objects ao
                     WHERE ao.clip_id = c.id AND ao.storage_state <> 'stored'
                  ) THEN 'stored'::ingest_state
                  ELSE 'uploading'::ingest_state
                END,
                stored_at = CASE
                  WHEN NOT EXISTS (
                    SELECT 1 FROM artifact_objects ao
                     WHERE ao.clip_id = c.id AND ao.storage_state <> 'stored'
                  ) THEN COALESCE(stored_at, transaction_timestamp())
                  ELSE stored_at
                END,
                verification_state = 'queued'
          WHERE c.id = $1`,
        [row.clip_id],
      );
      await client.query(
        `UPDATE upload_intents ui
            SET status = CASE
                  WHEN NOT EXISTS (
                    SELECT 1 FROM multipart_uploads mp
                     WHERE mp.upload_intent_id = ui.id AND mp.status <> 'completed'
                  ) THEN 'completed'::upload_intent_status
                  ELSE 'uploading'::upload_intent_status
                END,
                completed_at = CASE
                  WHEN NOT EXISTS (
                    SELECT 1 FROM multipart_uploads mp
                     WHERE mp.upload_intent_id = ui.id AND mp.status <> 'completed'
                  ) THEN COALESCE(completed_at, transaction_timestamp())
                  ELSE completed_at
                END,
                last_error_code = NULL
          WHERE ui.id = $1`,
        [row.intent_id],
      );
      await audit(client, deviceId, "artifact.stored", "artifact_object", row.artifact_object_id, {
        actualBytes,
      });
      return row.intent_id;
    });
    const intent = await this.getUploadIntent(deviceId, uploadIntentId);
    const upload = intent?.uploads.find((candidate) => candidate.id === multipartId);
    return requiredRow(upload, "upload_not_found", "multipart upload could not be read after completion");
  }

  async releaseMultipartCompletion(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    errorCode: string,
  ): Promise<boolean> {
    return this.#transaction(async (client) => {
      const result = await client.query<{
        intent_id: string;
        multipart_status: string;
        completion_lease_id: string | null;
      }>(
        `SELECT ui.id AS intent_id, mp.status AS multipart_status, mp.completion_lease_id
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE mp.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF mp, ui`,
        [multipartId, deviceId],
      );
      const row = requiredRow(result.rows[0], "upload_not_found", "multipart upload was not found");
      if (
        row.multipart_status !== "completing" ||
        row.completion_lease_id !== completionLeaseId
      ) {
        return false;
      }
      await client.query(
        `UPDATE multipart_uploads
            SET status = 'uploading', last_error_code = $2,
                completion_lease_id = NULL, completion_started_at = NULL
          WHERE id = $1 AND completion_lease_id = $3`,
        [multipartId, errorCode, completionLeaseId],
      );
      await client.query(
        `UPDATE upload_intents ui
            SET status = CASE
                  WHEN EXISTS (
                    SELECT 1 FROM multipart_uploads sibling
                     WHERE sibling.upload_intent_id = ui.id AND sibling.status = 'completing'
                  ) THEN 'completing'::upload_intent_status
                  ELSE 'uploading'::upload_intent_status
                END,
                last_error_code = $2
          WHERE ui.id = $1 AND ui.status IN ('pending','uploading','completing')`,
        [row.intent_id, errorCode],
      );
      await audit(client, deviceId, "multipart.completion_released", "multipart_upload", multipartId, {
        errorCode,
      });
      return true;
    });
  }

  async restartMultipartCompletion(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    providerUploadId: string,
  ): Promise<MultipartRecord> {
    const uploadIntentId = await this.#transaction(async (client) => {
      const result = await client.query<{
        intent_id: string;
        artifact_object_id: string;
        multipart_status: string;
        completion_lease_id: string | null;
        attempt_no: number;
      } & UploadGateRow>(
        `SELECT ui.id AS intent_id, mp.artifact_object_id,
                mp.status AS multipart_status, mp.completion_lease_id, mp.attempt_no,
                d.status IN ('pending','active') AS device_allowed,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() AS before_retention_deadline,
                ui.expires_at > transaction_timestamp() AS intent_not_expired,
                mp.expires_at > transaction_timestamp() AS multipart_not_expired
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE mp.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF mp, ui, c`,
        [multipartId, deviceId],
      );
      const row = requiredRow(result.rows[0], "upload_not_found", "multipart upload was not found");
      assertUploadGate(row, this.#admittedClassifications);
      if (
        row.multipart_status !== "completing" ||
        row.completion_lease_id !== completionLeaseId
      ) {
        throw conflict("multipart_completion_lease_lost", "multipart completion ownership changed");
      }
      const restarted = await client.query<{ attempt_no: number }>(restartMultipartAttemptSql, [
        multipartId,
        providerUploadId,
        completionLeaseId,
      ]);
      const nextAttemptNo = requiredRow(
        restarted.rows[0],
        "multipart_completion_lease_lost",
        "multipart completion ownership changed",
      ).attempt_no;
      await client.query(resetMultipartPartsForProviderRestartSql, [multipartId]);
      await client.query(
        `UPDATE artifact_objects
            SET storage_state = 'uploading', verification_state = 'unverified', last_error_code = NULL
          WHERE id = $1 AND storage_state = 'uploading'`,
        [row.artifact_object_id],
      );
      await client.query(
        `UPDATE upload_intents ui
            SET status = CASE
                  WHEN EXISTS (
                    SELECT 1 FROM multipart_uploads sibling
                     WHERE sibling.upload_intent_id = ui.id AND sibling.status = 'completing'
                  ) THEN 'completing'::upload_intent_status
                  ELSE 'uploading'::upload_intent_status
                END,
                last_error_code = 'provider_upload_restarted'
          WHERE ui.id = $1 AND ui.status IN ('pending','uploading','completing')`,
        [row.intent_id],
      );
      await audit(client, deviceId, "multipart.provider_restarted", "multipart_upload", multipartId, {
        previousAttemptNo: row.attempt_no,
        attemptNo: nextAttemptNo,
        reason: "provider_upload_missing_after_completion",
      });
      return row.intent_id;
    });
    const intent = await this.getUploadIntent(deviceId, uploadIntentId);
    const upload = intent?.uploads.find((candidate) => candidate.id === multipartId);
    return requiredRow(upload, "upload_not_found", "multipart upload could not be read after restart");
  }

  async failMultipart(
    deviceId: string,
    multipartId: string,
    completionLeaseId: string,
    errorCode: string,
  ): Promise<boolean> {
    return this.#transaction(async (client) => {
      const result = await client.query<{
        artifact_object_id: string;
        intent_id: string;
        clip_id: string;
        multipart_status: string;
        completion_lease_id: string | null;
      }>(
        `SELECT mp.artifact_object_id, ui.id AS intent_id, ui.clip_id,
                mp.status AS multipart_status, mp.completion_lease_id
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
           JOIN clips c ON c.id = ui.clip_id
           JOIN sessions s ON s.id = c.session_id
           JOIN devices d ON d.id = s.device_id
          WHERE mp.id = $1 AND d.credential_fingerprint = $2
          FOR UPDATE OF mp, ui, c`,
        [multipartId, deviceId],
      );
      const row = requiredRow(result.rows[0], "upload_not_found", "multipart upload was not found");
      if (
        row.multipart_status !== "completing" ||
        row.completion_lease_id !== completionLeaseId
      ) {
        return false;
      }
      await client.query(
        `UPDATE multipart_uploads
            SET status = 'failed', last_error_code = $2,
                completion_lease_id = NULL, completion_started_at = NULL
          WHERE id = $1 AND status NOT IN ('completed','aborted')`,
        [multipartId, errorCode],
      );
      await client.query(
        `UPDATE artifact_objects
            SET storage_state = 'failed', verification_state = 'rejected', last_error_code = $2
          WHERE id = $1 AND storage_state NOT IN ('purging','purged')`,
        [row.artifact_object_id, errorCode],
      );
      await client.query(
        `UPDATE clips
            SET ingest_state = 'failed', verification_state = 'rejected', rejected_reason = $2
          WHERE id = $1 AND retention_state = 'retained'`,
        [row.clip_id, errorCode],
      );
      await client.query(
        `UPDATE upload_intents SET status = 'failed', last_error_code = $2
          WHERE id = $1 AND status IN ('pending','uploading','completing')`,
        [row.intent_id, errorCode],
      );
      const disposition = uploadFailureDisposition(errorCode);
      if (disposition.retainsData) {
        // `recognized: false` 跟着审计事件落库而不是只打日志——本函数要修的正是
        // 「证据只活在会过期的日志里」这件事（#701），未登记的新码不该也这样。
        await audit(client, deviceId, "clip.infrastructure_hold", "clip", row.clip_id, {
          errorCode,
          retentionState: "retained",
          recognizedErrorCode: disposition.recognized,
        });
      } else {
        await enqueueClipPurge(client, row.clip_id, errorCode);
      }
      await audit(client, deviceId, "artifact.storage_failed", "artifact_object", row.artifact_object_id, {
        errorCode,
      });
      return true;
    });
  }

  async leaseVerificationJobs(limit: number, leaseSeconds: number): Promise<VerificationJob[]> {
    return this.#transaction(async (client) => {
      const exhausted = await client.query<{ artifact_object_id: string; clip_id: string }>(
        `UPDATE verification_jobs v
            SET status = 'dead_letter', completed_at = transaction_timestamp(),
                locked_at = NULL, locked_by = NULL, last_error_code = 'verification_lease_exhausted'
           FROM artifact_objects ao
          WHERE v.artifact_object_id = ao.id
            AND v.status = 'running'
            AND v.attempt_count >= v.max_attempts
            AND v.locked_at < transaction_timestamp() - make_interval(secs => $1)
          RETURNING v.artifact_object_id, ao.clip_id`,
        [leaseSeconds],
      );
      for (const row of exhausted.rows) {
        await client.query(
          `UPDATE artifact_objects
              SET verification_state = 'rejected', last_error_code = 'verification_lease_exhausted'
            WHERE id = $1`,
          [row.artifact_object_id],
        );
        await client.query(
          `UPDATE clips
              SET verification_state = 'rejected', rejected_reason = 'verification_lease_exhausted'
            WHERE id = $1`,
          [row.clip_id],
        );
        await client.query(
          `UPDATE verification_jobs
              SET status = 'failed', last_error_code = 'package_rejected',
                  completed_at = transaction_timestamp()
            WHERE artifact_object_id IN (SELECT id FROM artifact_objects WHERE clip_id = $1)
              AND status IN ('pending','retry_wait')`,
          [row.clip_id],
        );
        await audit(
          client,
          "verification-worker",
          "clip.infrastructure_hold",
          "clip",
          row.clip_id,
          { errorCode: "verification_lease_exhausted", retentionState: "retained" },
          "worker",
        );
      }
      const governanceBlocked = await client.query<{
        artifact_object_id: string;
        clip_id: string;
        rejection_code: string;
      }>(
        governanceBlockedVerificationJobsSql,
        [limit, this.#admittedClassifications],
      );
      const blockedClips = new Set<string>();
      for (const row of governanceBlocked.rows) {
        if (blockedClips.has(row.clip_id)) continue;
        blockedClips.add(row.clip_id);
        await client.query(
          `UPDATE verification_jobs
              SET status = 'failed', last_error_code = $2, completed_at = transaction_timestamp(),
                  locked_at = NULL, locked_by = NULL
            WHERE artifact_object_id IN (SELECT id FROM artifact_objects WHERE clip_id = $1)
              AND status IN ('pending','retry_wait')`,
          [row.clip_id, row.rejection_code],
        );
        await client.query(
          `UPDATE artifact_objects
              SET verification_state = 'rejected', last_error_code = $2
            WHERE id = $1 AND verification_state <> 'verified'`,
          [row.artifact_object_id, row.rejection_code],
        );
        await client.query(
          `UPDATE clips SET verification_state = 'rejected', rejected_reason = $2 WHERE id = $1`,
          [row.clip_id, row.rejection_code],
        );
        await enqueueClipPurge(
          client,
          row.clip_id,
          row.rejection_code,
          "worker",
          "verification-worker",
        );
        await audit(
          client,
          "verification-worker",
          "clip.governance_blocked",
          "clip",
          row.clip_id,
          { rejectionCode: row.rejection_code },
          "worker",
        );
      }
      await client.query(
        `UPDATE verification_jobs
            SET status = 'retry_wait', available_at = transaction_timestamp(),
                locked_at = NULL, locked_by = NULL, last_error_code = 'lease_expired'
          WHERE status = 'running'
            AND attempt_count < max_attempts
            AND locked_at < transaction_timestamp() - make_interval(secs => $1)`,
        [leaseSeconds],
      );
      const result = await client.query<{
        id: string;
        artifact_object_id: string;
        attempt_count: number;
        object_key: string;
        expected_size_bytes: string;
        expected_sha256: string;
        kind: ArtifactKind;
        clip_id: string;
        client_session_id: string;
        session_schema_version: string;
        client_run_id: string;
        segment_seq: number;
        session_frame_start: string;
        n_frames: string;
        qpc_start: string;
        qpc_end: string;
        event_count: string;
        width: number;
        height: number;
        fps_num: number;
        fps_den: number;
        health_json: Record<string, number>;
        session_manifest_json: JsonRecord;
        clip_meta_json: PutClipInput;
      }>(verificationJobCandidatesSql, [limit, this.#admittedClassifications]);
      const jobs: VerificationJob[] = [];
      for (const row of result.rows) {
        const attempt = row.attempt_count + 1;
        await client.query(
          `UPDATE verification_jobs
              SET status = 'running', attempt_count = $2, locked_at = transaction_timestamp(),
                  locked_by = $3
            WHERE id = $1`,
          [row.id, attempt, `dataminer-verifier:${process.pid}`],
        );
        await client.query(
          `UPDATE artifact_objects SET verification_state = 'verifying' WHERE id = $1`,
          [row.artifact_object_id],
        );
        await client.query(
          `UPDATE clips SET verification_state = 'verifying'
            WHERE id = (SELECT clip_id FROM artifact_objects WHERE id = $1)
              AND verification_state <> 'rejected'`,
          [row.artifact_object_id],
        );
        const semantic =
          row.kind === "manifest" || row.kind === "meta"
            ? await semanticExpectation(client, row)
            : undefined;
        jobs.push({
          id: row.id,
          artifactId: row.artifact_object_id,
          clipId: row.clip_id,
          objectKey: row.object_key,
          expectedBytes: Number(row.expected_size_bytes),
          expectedSha256: row.expected_sha256,
          attempt,
          kind: row.kind,
          ...(semantic ? { semantic } : {}),
        });
      }
      return jobs;
    });
  }

  async finishVerification(
    job: VerificationJob,
    result:
      | { outcome: "verified"; actualBytes: number; actualSha256: string }
      | { outcome: "rejected"; actualBytes: number; actualSha256: string; rejectionCode: string },
  ): Promise<void> {
    await this.#transaction(async (client) => {
      const owned = await client.query<{
        artifact_object_id: string;
        attempt_count: number;
        clip_id: string;
        session_classification: string;
        retention_allowed: boolean;
        before_retention_deadline: boolean;
      }>(
        `SELECT v.artifact_object_id, v.attempt_count, ao.clip_id,
                s.server_data_classification::text AS session_classification,
                c.retention_state = 'retained' AS retention_allowed,
                c.retention_deadline > transaction_timestamp() AS before_retention_deadline
           FROM verification_jobs v
           JOIN artifact_objects ao ON ao.id = v.artifact_object_id
           JOIN clips c ON c.id = ao.clip_id
           JOIN sessions s ON s.id = c.session_id
          WHERE v.id = $1 AND v.status = 'running'
          FOR UPDATE OF v, c`,
        [job.id],
      );
      const row = requiredRow(owned.rows[0], "verification_lease_lost", "verification job is no longer leased");
      if (row.artifact_object_id !== job.artifactId || row.attempt_count !== job.attempt) {
        throw conflict("verification_lease_lost", "verification lease identity changed");
      }
      const governanceRejection = !this.#admittedClassifications.includes(row.session_classification)
        ? "classification_blocked"
        : !row.retention_allowed || !row.before_retention_deadline
          ? "retention_blocked"
          : undefined;
      const rejectionCode = result.outcome === "rejected" ? result.rejectionCode : governanceRejection;
      await client.query(
        `UPDATE verification_jobs
            SET status = 'succeeded', result_sha256 = $2, result_size_bytes = $3,
                last_error_code = $4, completed_at = transaction_timestamp(),
                locked_at = NULL, locked_by = NULL
          WHERE id = $1`,
        [job.id, result.actualSha256, result.actualBytes, rejectionCode ?? null],
      );
      await client.query(finishArtifactVerificationSql, [
        job.artifactId,
        rejectionCode ? "rejected" : "verified",
        rejectionCode ?? null,
      ]);
      const clipId = row.clip_id;
      if (rejectionCode) {
        await client.query(
          `UPDATE clips SET verification_state = 'rejected', rejected_reason = $2 WHERE id = $1`,
          [clipId, rejectionCode],
        );
        await client.query(
          `UPDATE verification_jobs
              SET status = 'failed', last_error_code = 'package_rejected',
                  completed_at = transaction_timestamp()
            WHERE artifact_object_id IN (SELECT id FROM artifact_objects WHERE clip_id = $1)
              AND status IN ('pending','retry_wait')`,
          [clipId],
        );
        await enqueueClipPurge(
          client,
          clipId,
          rejectionCode,
          "worker",
          "verification-worker",
        );
      } else {
        await client.query(
          `UPDATE clips c
              SET verification_state = CASE
                    WHEN NOT EXISTS (
                      SELECT 1 FROM artifact_objects ao
                       WHERE ao.clip_id = c.id AND ao.verification_state <> 'verified'
                    ) THEN 'verified'::verification_state
                    ELSE 'verifying'::verification_state
                  END,
                  verified_at = CASE
                    WHEN NOT EXISTS (
                      SELECT 1 FROM artifact_objects ao
                       WHERE ao.clip_id = c.id AND ao.verification_state <> 'verified'
                    ) THEN COALESCE(verified_at, transaction_timestamp())
                    ELSE verified_at
                  END
            WHERE c.id = $1 AND c.verification_state <> 'rejected'`,
          [clipId],
        );
      }
      await audit(client, "verification-worker", rejectionCode ? "artifact.rejected" : "artifact.verified", "artifact_object", job.artifactId, {
        actualBytes: result.actualBytes,
        hashMatched: result.actualSha256 === job.expectedSha256,
        ...(rejectionCode ? { rejectionCode } : {}),
      }, "worker");
    });
  }

  async retryVerification(
    job: VerificationJob,
    errorCode: string,
  ): Promise<"retry_scheduled" | "dead_letter" | "stale"> {
    return this.#transaction(async (client) => {
      const result = await client.query<{
        artifact_object_id: string;
        attempt_count: number;
        max_attempts: number;
        clip_id: string;
      }>(
        `SELECT v.artifact_object_id, v.attempt_count, v.max_attempts, ao.clip_id
           FROM verification_jobs v
           JOIN artifact_objects ao ON ao.id = v.artifact_object_id
          WHERE v.id = $1 AND v.status = 'running' FOR UPDATE OF v`,
        [job.id],
      );
      const row = result.rows[0];
      if (!row || row.artifact_object_id !== job.artifactId || row.attempt_count !== job.attempt) {
        return "stale";
      }
      if (row.attempt_count >= row.max_attempts) {
        await client.query(
          `UPDATE verification_jobs
              SET status = 'dead_letter', last_error_code = $2, completed_at = transaction_timestamp(),
                  locked_at = NULL, locked_by = NULL
            WHERE id = $1`,
          [job.id, errorCode],
        );
        await client.query(
          `UPDATE artifact_objects SET verification_state = 'rejected', last_error_code = $2
            WHERE id = $1`,
          [job.artifactId, errorCode],
        );
        await client.query(
          `UPDATE verification_jobs
              SET status = 'failed', last_error_code = 'package_held',
                  completed_at = transaction_timestamp()
            WHERE artifact_object_id IN (SELECT id FROM artifact_objects WHERE clip_id = $1)
              AND status IN ('pending','retry_wait')`,
          [row.clip_id],
        );
        await client.query(
          `UPDATE clips SET verification_state = 'rejected', rejected_reason = $2
            WHERE id = (SELECT clip_id FROM artifact_objects WHERE id = $1)`,
          [job.artifactId, errorCode],
        );
        await audit(
          client,
          "verification-worker",
          "clip.infrastructure_hold",
          "clip",
          row.clip_id,
          { errorCode, retentionState: "retained" },
          "worker",
        );
        return "dead_letter";
      }
      const delaySeconds = Math.min(300, 2 ** row.attempt_count);
      await client.query(
        `UPDATE verification_jobs
            SET status = 'retry_wait', available_at = transaction_timestamp() + make_interval(secs => $2),
                last_error_code = $3, locked_at = NULL, locked_by = NULL
          WHERE id = $1`,
        [job.id, delaySeconds, errorCode],
      );
      await client.query(
        `UPDATE artifact_objects SET verification_state = 'queued', last_error_code = $2 WHERE id = $1`,
        [job.artifactId, errorCode],
      );
      return "retry_scheduled";
    });
  }

  /**
   * Erases telemetry that no consent purge can ever reach.
   *
   * `client_events` rows written during signup and enrolment predate the
   * consent row, so their `consent_id` is null and the consent-level purge in
   * `revokeConsent` cannot see them. Tombstoning the annotator sweeps them up,
   * but an account that is simply never used again is never tombstoned — so
   * without a deadline of their own these rows are kept indefinitely, which is
   * precisely what splitting `client_events` out of `audit_events` was meant to
   * prevent (11 §6.4).
   *
   * Rows are tombstoned, not deleted, matching every other purge here: the
   * payload goes, the fact that an event existed stays.
   */
  async sweepUnconsentedTelemetry(ttlDays: number, limit: number): Promise<number> {
    const result = await this.#pool.query<{ id: string }>(sweepUnconsentedTelemetrySql, [
      ttlDays,
      limit,
    ]);
    return result.rows.length;
  }

  async enqueueMaintenancePurges(limit: number): Promise<number> {
    return this.#transaction(async (client) => {
      const candidates = await client.query<{ id: string; reason: string }>(
        maintenancePurgeCandidatesSql,
        [limit],
      );
      let enqueued = 0;
      for (const row of candidates.rows) {
        if (
          await enqueueClipPurge(
            client,
            row.id,
            row.reason,
            "worker",
            "dataminer-maintenance-sweeper",
          )
        ) {
          enqueued += 1;
        }
      }
      return enqueued;
    });
  }

  async leasePurgeJobs(
    limit: number,
    leaseSeconds: number,
    completionLeaseSeconds: number,
  ): Promise<PurgeJob[]> {
    return this.#transaction(async (client) => {
      const exhausted = await client.query<{ id: string; clip_id: string }>(
        `UPDATE purge_jobs p
            SET status = 'dead_letter', completed_at = transaction_timestamp(),
                locked_at = NULL, locked_by = NULL, last_error_code = 'purge_lease_exhausted'
          WHERE p.scope = 'clip'
            AND p.status = 'running'
            AND p.attempt_count >= p.max_attempts
            AND p.locked_at < transaction_timestamp() - make_interval(secs => $1)
          RETURNING p.id, p.clip_id`,
        [leaseSeconds],
      );
      for (const row of exhausted.rows) {
        await client.query(
          `UPDATE clips SET retention_state = 'purge_failed'
            WHERE id = $1 AND retention_state <> 'purged'`,
          [row.clip_id],
        );
        await audit(
          client,
          "dataminer-purge-worker",
          "clip.purge_dead_letter",
          "clip",
          row.clip_id,
          { purgeJobId: row.id },
          "worker",
        );
      }
      await client.query(
        `UPDATE purge_jobs
            SET status = 'retry_wait', available_at = transaction_timestamp(),
                locked_at = NULL, locked_by = NULL, last_error_code = 'lease_expired'
          WHERE scope = 'clip' AND status = 'running'
            AND attempt_count < max_attempts
            AND locked_at < transaction_timestamp() - make_interval(secs => $1)`,
        [leaseSeconds],
      );

      const result = await client.query<{
        id: string;
        clip_id: string;
        attempt_count: number;
        progress_json: JsonRecord;
      }>(
        `SELECT p.id, p.clip_id, p.attempt_count, p.progress_json
           FROM purge_jobs p
           JOIN clips c ON c.id = p.clip_id
          WHERE p.scope = 'clip'
            AND p.status IN ('pending','retry_wait')
            AND p.available_at <= transaction_timestamp()
            AND p.attempt_count < p.max_attempts
            AND c.retention_state IN ('purge_requested','purging','purge_failed')
            AND NOT EXISTS (
              SELECT 1
                FROM upload_intents ui
                JOIN multipart_uploads mp ON mp.upload_intent_id = ui.id
               WHERE ui.clip_id = p.clip_id
                 AND mp.status = 'completing'
                 AND mp.completion_started_at >
                     transaction_timestamp() - make_interval(secs => $2)
            )
          ORDER BY p.available_at, p.created_at
          FOR UPDATE OF p, c SKIP LOCKED
          LIMIT $1`,
        [limit, completionLeaseSeconds],
      );

      const jobs: PurgeJob[] = [];
      const leaseOwner = `dataminer-purge:${process.pid}:${randomUUID()}`;
      for (const row of result.rows) {
        const attempt = row.attempt_count + 1;
        await client.query(
          `UPDATE purge_jobs
              SET status = 'running', attempt_count = $2, locked_at = transaction_timestamp(),
                  locked_by = $3, started_at = COALESCE(started_at, transaction_timestamp())
            WHERE id = $1`,
          [row.id, attempt, leaseOwner],
        );
        await client.query(
          `UPDATE clips SET retention_state = 'purging'
            WHERE id = $1 AND retention_state <> 'purged'`,
          [row.clip_id],
        );
        await client.query(
          `UPDATE artifact_objects
              SET storage_state = 'purging',
                  purge_started_at = COALESCE(purge_started_at, transaction_timestamp())
            WHERE clip_id = $1 AND storage_state <> 'purged'`,
          [row.clip_id],
        );
        await client.query(
          `UPDATE verification_jobs
              SET status = 'failed', completed_at = transaction_timestamp(),
                  last_error_code = 'purge_started', locked_at = NULL, locked_by = NULL
            WHERE artifact_object_id IN (SELECT id FROM artifact_objects WHERE clip_id = $1)
              AND status IN ('pending','running','retry_wait')`,
          [row.clip_id],
        );

        const multiparts = await client.query<{
          id: string;
          object_key: string;
          provider_upload_id: string;
        }>(
          `SELECT mp.id, ao.object_key, mp.provider_upload_id
             FROM multipart_uploads mp
             JOIN upload_intents ui ON ui.id = mp.upload_intent_id
             JOIN artifact_objects ao ON ao.id = mp.artifact_object_id
            WHERE ui.clip_id = $1 AND mp.provider_upload_id IS NOT NULL
              AND mp.status NOT IN ('completed','aborted')
            ORDER BY mp.created_at, mp.id`,
          [row.clip_id],
        );
        const artifacts = await client.query<{ id: string; object_key: string }>(
          `SELECT id, object_key FROM artifact_objects
            WHERE clip_id = $1 ORDER BY kind`,
          [row.clip_id],
        );
        const abortedIds = progressIds(row.progress_json, "abortedMultipartIds");
        const absentIds = progressIds(row.progress_json, "absentArtifactIds");
        jobs.push({
          id: row.id,
          clipId: row.clip_id,
          attempt,
          leaseOwner,
          multiparts: multiparts.rows.map((multipart) => ({
            id: multipart.id,
            objectKey: multipart.object_key,
            providerUploadId: multipart.provider_upload_id,
            aborted: abortedIds.has(multipart.id),
          })),
          artifacts: artifacts.rows.map((artifact) => ({
            id: artifact.id,
            objectKey: artifact.object_key,
            confirmedAbsent: absentIds.has(artifact.id),
          })),
        });
      }
      return jobs;
    });
  }

  async renewPurgeLease(job: PurgeJob): Promise<void> {
    const renewed = await this.#pool.query(
      `UPDATE purge_jobs SET locked_at = transaction_timestamp()
        WHERE id = $1 AND scope = 'clip' AND clip_id = $2 AND status = 'running'
          AND attempt_count = $3 AND locked_by = $4`,
      [job.id, job.clipId, job.attempt, job.leaseOwner],
    );
    if (renewed.rowCount !== 1) {
      throw conflict("purge_lease_lost", "purge job is no longer leased");
    }
  }

  async recordPurgeProgress(job: PurgeJob, progress: PurgeProgress): Promise<void> {
    await this.#transaction(async (client) => {
      await assertPurgeLease(client, job);
      const targetId = progress.kind === "multipart_aborted" ? progress.multipartId : progress.artifactId;
      const target =
        progress.kind === "multipart_aborted"
          ? await client.query(
              `SELECT 1
                 FROM multipart_uploads mp
                 JOIN upload_intents ui ON ui.id = mp.upload_intent_id
                WHERE mp.id = $1 AND ui.clip_id = $2`,
              [targetId, job.clipId],
            )
          : await client.query(
              `SELECT 1 FROM artifact_objects WHERE id = $1 AND clip_id = $2`,
              [targetId, job.clipId],
            );
      if (target.rowCount !== 1) {
        throw conflict("purge_target_mismatch", "purge progress target is outside the leased clip");
      }
      const key = progress.kind === "multipart_aborted" ? "abortedMultipartIds" : "absentArtifactIds";
      const updated = await client.query(
        `UPDATE purge_jobs
            SET locked_at = transaction_timestamp(),
                progress_json = jsonb_set(
                  progress_json,
                  ARRAY[$4::text],
                  COALESCE(progress_json -> $4, '[]'::jsonb) || jsonb_build_array($5::text),
                  true
                )
          WHERE id = $1 AND status = 'running' AND attempt_count = $2 AND locked_by = $3
            AND NOT (COALESCE(progress_json -> $4, '[]'::jsonb) @> jsonb_build_array($5::text))`,
        [job.id, job.attempt, job.leaseOwner, key, targetId],
      );
      if (updated.rowCount === 1) {
        await audit(
          client,
          job.leaseOwner,
          progress.kind === "multipart_aborted"
            ? "multipart.purge_aborted"
            : "artifact.purge_absent",
          progress.kind === "multipart_aborted" ? "multipart_upload" : "artifact_object",
          targetId,
          { purgeJobId: job.id },
          "worker",
        );
      }
    });
  }

  async finishPurge(job: PurgeJob): Promise<void> {
    await this.#transaction(async (client) => {
      const lease = await assertPurgeLease(client, job);
      const unfinishedMultiparts = await client.query(
        `SELECT 1
           FROM multipart_uploads mp
           JOIN upload_intents ui ON ui.id = mp.upload_intent_id
          WHERE ui.clip_id = $1 AND mp.status NOT IN ('completed','aborted')
            AND mp.provider_upload_id IS NOT NULL
            AND NOT (COALESCE($2::jsonb -> 'abortedMultipartIds', '[]'::jsonb)
                     @> jsonb_build_array(mp.id::text))
          LIMIT 1`,
        [job.clipId, lease.progress_json],
      );
      const presentArtifacts = await client.query(
        `SELECT 1 FROM artifact_objects ao
          WHERE ao.clip_id = $1
            AND NOT (COALESCE($2::jsonb -> 'absentArtifactIds', '[]'::jsonb)
                     @> jsonb_build_array(ao.id::text))
          LIMIT 1`,
        [job.clipId, lease.progress_json],
      );
      if (unfinishedMultiparts.rowCount !== 0 || presentArtifacts.rowCount !== 0) {
        throw conflict("purge_incomplete", "all multipart and object absence checks must finish first");
      }
      await client.query(
        `UPDATE multipart_uploads mp
            SET status = 'aborted', aborted_at = COALESCE(mp.aborted_at, transaction_timestamp()),
                completion_lease_id = NULL, completion_started_at = NULL,
                last_error_code = NULL
           FROM upload_intents ui
          WHERE mp.upload_intent_id = ui.id AND ui.clip_id = $1
            AND mp.status NOT IN ('completed','aborted')`,
        [job.clipId],
      );
      await client.query(
        `UPDATE upload_intents
            SET status = 'aborted', aborted_at = COALESCE(aborted_at, transaction_timestamp()),
                last_error_code = NULL
          WHERE clip_id = $1 AND status <> 'completed'`,
        [job.clipId],
      );
      await client.query(
        `UPDATE artifact_objects
            SET storage_state = 'purged', purged_at = COALESCE(purged_at, transaction_timestamp()),
                last_error_code = NULL
          WHERE clip_id = $1`,
        [job.clipId],
      );
      await client.query(
        `UPDATE clips
            SET retention_state = 'purged', purged_at = COALESCE(purged_at, transaction_timestamp())
          WHERE id = $1`,
        [job.clipId],
      );
      await client.query(
        `UPDATE purge_jobs
            SET status = 'succeeded', completed_at = transaction_timestamp(),
                locked_at = NULL, locked_by = NULL, last_error_code = NULL
          WHERE id = $1 AND status = 'running' AND attempt_count = $2 AND locked_by = $3`,
        [job.id, job.attempt, job.leaseOwner],
      );
      await audit(
        client,
        job.leaseOwner,
        "clip.purged",
        "clip",
        job.clipId,
        { purgeJobId: job.id },
        "worker",
      );
    });
  }

  async retryPurge(job: PurgeJob, errorCode: string): Promise<void> {
    await this.#transaction(async (client) => {
      const lease = await assertPurgeLease(client, job);
      if (lease.attempt_count >= lease.max_attempts) {
        await client.query(
          `UPDATE purge_jobs
              SET status = 'dead_letter', completed_at = transaction_timestamp(),
                  last_error_code = $4, locked_at = NULL, locked_by = NULL
            WHERE id = $1 AND attempt_count = $2 AND locked_by = $3`,
          [job.id, job.attempt, job.leaseOwner, errorCode],
        );
        await client.query(
          `UPDATE clips SET retention_state = 'purge_failed'
            WHERE id = $1 AND retention_state <> 'purged'`,
          [job.clipId],
        );
        await audit(
          client,
          job.leaseOwner,
          "clip.purge_dead_letter",
          "clip",
          job.clipId,
          { purgeJobId: job.id, errorCode },
          "worker",
        );
        return;
      }
      const delaySeconds = Math.min(900, 2 ** lease.attempt_count);
      await client.query(
        `UPDATE purge_jobs
            SET status = 'retry_wait',
                available_at = transaction_timestamp() + make_interval(secs => $4),
                last_error_code = $5, locked_at = NULL, locked_by = NULL
          WHERE id = $1 AND attempt_count = $2 AND locked_by = $3`,
        [job.id, job.attempt, job.leaseOwner, delaySeconds, errorCode],
      );
    });
  }

  async isKnownActiveMultipart(providerUploadId: string, objectKey: string): Promise<boolean> {
    const result = await this.#pool.query(
      `SELECT 1
         FROM multipart_uploads mp
         JOIN upload_intents ui ON ui.id = mp.upload_intent_id
         JOIN artifact_objects ao ON ao.id = mp.artifact_object_id
         JOIN clips c ON c.id = ui.clip_id
        WHERE mp.provider = 'r2' AND ao.object_key = $2
          AND mp.status IN ('initiated','uploading','completing')
          AND (
            mp.provider_upload_id = $1
            OR (
              mp.provider_upload_id IS NULL
              AND ui.expires_at > transaction_timestamp()
              AND mp.expires_at > transaction_timestamp()
              AND c.retention_state = 'retained'
            )
          )`,
      [providerUploadId, objectKey],
    );
    return result.rowCount === 1;
  }

  async recordOrphanMultipartAborted(objectKey: string): Promise<void> {
    await this.#transaction(async (client) => {
      await audit(
        client,
        "dataminer-orphan-sweeper",
        "multipart.orphan_aborted",
        "r2_object_key_fingerprint",
        stableJsonSha256({ objectKey }),
        {},
        "worker",
      );
    });
  }

  async #transaction<T>(operation: (client: PoolClient) => Promise<T>): Promise<T> {
    const client = await this.#pool.connect();
    try {
      await client.query("BEGIN");
      const value = await operation(client);
      await client.query("COMMIT");
      return value;
    } catch (error) {
      await client.query("ROLLBACK").catch(() => undefined);
      throw error;
    } finally {
      client.release();
    }
  }
}

type ClipJoinRow = {
  clip_id: string;
  session_id: string;
  capture_run_id: string;
  segment_seq: number;
  revision: number;
  ingest_state: string;
  clip_verification_state: string;
  retention_state: ClipRecord["retentionState"];
  retention_deadline: Date;
  clip_rejected_reason: string | null;
  clip_created_at: Date;
  clip_updated_at: Date;
  artifact_id: string;
  kind: ArtifactKind;
  object_key: string;
  expected_size_bytes: string;
  expected_sha256: string;
  storage_state: string;
  artifact_verification_state: string;
  last_error_code: string | null;
  result_size_bytes: string | null;
  result_sha256: string | null;
  is_trainable: boolean;
  blockers: string[];
};

const clipSelectSql = `
  SELECT c.id AS clip_id, c.session_id, c.capture_run_id, c.segment_seq, c.revision,
         c.ingest_state, c.verification_state AS clip_verification_state,
         c.retention_state, c.retention_deadline,
         c.rejected_reason AS clip_rejected_reason, c.created_at AS clip_created_at,
         c.updated_at AS clip_updated_at,
         ao.id AS artifact_id, ao.kind, ao.object_key, ao.expected_size_bytes, ao.expected_sha256,
         ao.storage_state, ao.verification_state AS artifact_verification_state, ao.last_error_code,
         v.result_size_bytes, v.result_sha256, ct.is_trainable, ct.blockers
    FROM clips c
    JOIN sessions s ON s.id = c.session_id
    JOIN devices d ON d.id = s.device_id
    JOIN clip_trainability ct ON ct.clip_id = c.id
    JOIN artifact_objects ao ON ao.clip_id = c.id
    LEFT JOIN LATERAL (
      SELECT result_size_bytes, result_sha256
        FROM verification_jobs
       WHERE artifact_object_id = ao.id AND status = 'succeeded'
       ORDER BY completed_at DESC
       LIMIT 1
    ) v ON true
   WHERE c.id = $1 AND d.credential_fingerprint = $2
     AND d.status IN ('pending','active')
   ORDER BY ao.kind`;

function mapClip(rows: ClipJoinRow[]): ClipRecord {
  const first = rows[0];
  if (!first) throw new Error("cannot map an empty clip row set");
  const artifacts: ArtifactRecord[] = rows.map((row) => ({
    id: row.artifact_id,
    clipId: row.clip_id,
    kind: row.kind,
    objectKey: row.object_key,
    expectedBytes: Number(row.expected_size_bytes),
    expectedSha256: row.expected_sha256,
    contentType: artifactContentTypes[row.kind],
    state: artifactState(row.storage_state, row.artifact_verification_state),
    ...(row.result_size_bytes !== null ? { actualBytes: Number(row.result_size_bytes) } : {}),
    ...(row.result_sha256 !== null ? { actualSha256: row.result_sha256 } : {}),
    ...(row.last_error_code !== null ? { rejectionCode: row.last_error_code } : {}),
  }));
  return {
    id: first.clip_id,
    sessionId: first.session_id,
    runId: first.capture_run_id,
    segmentIndex: first.segment_seq,
    revision: first.revision,
    state: clipState(first.ingest_state, first.clip_verification_state),
    storageState: first.ingest_state as ClipRecord["storageState"],
    verificationState: first.clip_verification_state as ClipRecord["verificationState"],
    retentionState: first.retention_state,
    retentionDeadline: iso(first.retention_deadline),
    artifacts,
    isTrainable: first.is_trainable,
    trainabilityBlockers: first.blockers,
    createdAt: iso(first.clip_created_at),
    updatedAt: iso(first.clip_updated_at),
  };
}

type UploadGateRow = {
  device_allowed: boolean;
  session_classification: string;
  retention_allowed: boolean;
  before_retention_deadline: boolean;
  intent_not_expired: boolean;
  multipart_not_expired: boolean;
};

type IntentJoinRow = UploadGateRow & {
  intent_id: string;
  clip_id: string;
  intent_status: string;
  intent_created_at: Date;
  /** Present on getUploadIntent rows; other gate queries do not need it. */
  session_schema_version?: string;
  multipart_id: string;
  artifact_object_id: string;
  provider_upload_id: string | null;
  multipart_status: string;
  multipart_attempt_no: number;
  part_size_bytes: string;
  multipart_created_at: Date;
  kind: ArtifactKind;
  object_key: string;
  expected_size_bytes: string;
};

/**
 * 上传意图所属 session 的契约版本——存储路由的唯一键（issue #855 §B）。
 *
 * 读不出来就抛，而不是回退到某条车道：`getUploadIntent` 的查询本来就 join 了
 * sessions，所以缺列只可能是有人改了那条查询忘了带上它。静默回退正是让新数据写进
 * 旧键空间的那条路。
 */
function requiredIntentSessionSchema(row: IntentJoinRow): string {
  if (row.session_schema_version === undefined) {
    throw new Error("upload intent row was selected without its session schema version");
  }
  return row.session_schema_version;
}

function mapIntent(rows: IntentJoinRow[], admitted: readonly string[]): UploadIntentRecord {
  const first = rows[0];
  if (!first) throw new Error("cannot map an empty upload intent row set");
  const gateOpen = uploadGateOpen(first, admitted) && rows.every((row) => row.multipart_not_expired);
  return {
    id: first.intent_id,
    clipId: first.clip_id,
    state:
      first.intent_status === "completed" && gateOpen
        ? "completed"
        : gateOpen && ["pending", "uploading", "completing"].includes(first.intent_status)
          ? "initiated"
          : "aborted",
    // `schema_version` 自 0001 起 NOT NULL，所以这里没有"旧行没有这一列"的分支：
    // 每一行 session 都说得出自己是哪一版契约创建的。读不出来的那种行不存在。
    sessionSchemaVersion: requiredIntentSessionSchema(first),
    createdAt: iso(first.intent_created_at),
    uploads: rows.map((row) => ({
      id: row.multipart_id,
      uploadIntentId: row.intent_id,
      artifactId: row.artifact_object_id,
      clipId: row.clip_id,
      kind: row.kind,
      objectKey: row.object_key,
      contentType: artifactContentTypes[row.kind],
      ...(row.provider_upload_id !== null ? { providerUploadId: row.provider_upload_id } : {}),
      state:
        row.multipart_status === "completed" && uploadGateOpen(row, admitted)
          ? "completed"
          : uploadGateOpen(row, admitted) && ["initiated", "uploading", "completing"].includes(row.multipart_status)
            ? "initiated"
            : "aborted",
      attemptNo: row.multipart_attempt_no,
      partSizeBytes: Number(row.part_size_bytes),
      expectedBytes: Number(row.expected_size_bytes),
      createdAt: iso(row.multipart_created_at),
    })),
  };
}

async function deviceForPrincipal(client: PoolClient, credentialFingerprint: string) {
  const result = await client.query<{
    id: string;
    annotator_id: string | null;
  }>(
    `SELECT id, annotator_id FROM devices
      WHERE credential_fingerprint = $1 AND status = 'active'`,
    [credentialFingerprint],
  );
  return requiredRow(result.rows[0], "device_not_registered", "device is not registered");
}

// No consent is not an error here. The CHECK constraints allow a partial
// identity, and unattached devices are still the normal case (11 §1.3). The
// session is simply recorded with the identity that could actually be proven —
// and on a candidate-capable lane that missing identity is exactly what
// `deriveSourceClassification` turns into a permanent `non_trainable`
// (issue #855 §B): refusing the capture would cost bytes that cannot be
// re-recorded, recording it as non-trainable costs nothing.
async function activeConsentForSession(
  client: PoolClient,
  annotatorId: string,
  startedAt: string,
) {
  const result = await client.query<ActiveConsentForSession>(activeConsentForSessionSql, [
    annotatorId,
    startedAt,
  ]);
  return result.rows[0] ?? null;
}

type ActiveConsentForSession = {
  id: string;
  policy_version: string;
  terms_hash: string;
  scopes: string[];
};

/**
 * Which capture scopes a session of this shape needs its consent to cover
 * (docs/108 §3.2).
 *
 * Video and input are always required: they are what every capture produces.
 * Audio is added only for the session contract that can carry it.
 *
 * Depth is deliberately absent even though it is a scope word. Its admission
 * gate is `depth_consent_policy_versions` (0030, docs/105), decided per policy
 * version, and this cut does not add a second one that could disagree with it —
 * see the comment in migration 0032 and docs/108 §3.2.
 */
export function requiredConsentScopes(input: {
  carriesAudio: boolean;
}): string[] {
  return ["input", "video", ...(input.carriesAudio ? ["audio"] : [])].sort();
}

/**
 * Whether a consent's recorded scopes cover what a capture needs.
 *
 * Returns the shortfall rather than throwing. The old `assertAudioConsentPolicy`
 * threw a 409, and reviving that behaviour here would be a regression in the
 * direction docs/100 §2.1 already ruled on: a session registers after the bytes
 * exist, so refusing it destroys a capture that cannot be re-run, in exchange
 * for a fact the server can simply write down. What a scope shortfall must do is
 * keep the data out of a release — and that is `clip_trainability`'s job, where
 * it is a reversible verdict rather than an unrecoverable rejection.
 *
 * Comparison is by set membership, never by policy-version string equality. The
 * version-string form this replaces had to be edited and redeployed for every
 * new terms document, and it could not express partial coverage at all.
 */
export function consentScopeShortfall(
  consentScopes: readonly string[],
  required: readonly string[],
): string[] {
  return required.filter((scope) => !consentScopes.includes(scope));
}

const SESSION_ARTIFACT_COLUMNS =
  "sa.id, sa.session_id, s.schema_version, sa.kind, sa.object_key, sa.content_type, sa.expected_sha256, sa.expected_size_bytes, sa.ingest_state, sa.verification_state, sa.revision";

type SessionArtifactRow = {
  id: string;
  session_id: string;
  schema_version: string;
  kind: SessionArtifactKind;
  object_key: string;
  content_type: string;
  expected_sha256: string;
  expected_size_bytes: string;
  ingest_state: SessionArtifactRecord["ingestState"];
  verification_state: SessionArtifactRecord["verificationState"];
  revision: number;
};

function toSessionArtifact(row: SessionArtifactRow): SessionArtifactRecord {
  return {
    id: row.id,
    sessionId: row.session_id,
    sessionSchemaVersion: row.schema_version,
    kind: row.kind,
    objectKey: row.object_key,
    contentType: row.content_type,
    expectedBytes: Number(row.expected_size_bytes),
    expectedSha256: row.expected_sha256,
    ingestState: row.ingest_state,
    verificationState: row.verification_state,
    revision: row.revision,
  };
}

async function sessionArtifactRows(
  client: PoolClient,
  sessionId: string,
  kind?: SessionArtifactKind,
): Promise<SessionArtifactRecord[]> {
  const result = kind
    ? await client.query<SessionArtifactRow>(
        `SELECT ${SESSION_ARTIFACT_COLUMNS}
           FROM session_artifacts sa JOIN sessions s ON s.id = sa.session_id
          WHERE sa.session_id = $1 AND sa.kind = $2::session_artifact_kind`,
        [sessionId, kind],
      )
    : await client.query<SessionArtifactRow>(
        `SELECT ${SESSION_ARTIFACT_COLUMNS}
           FROM session_artifacts sa JOIN sessions s ON s.id = sa.session_id
          WHERE sa.session_id = $1 ORDER BY sa.kind`,
        [sessionId],
      );
  return result.rows.map(toSessionArtifact);
}

const SESSION_ARTIFACT_FILES: Record<SessionArtifactKind, string> = {
  session: "session.json",
  catalog: "catalog.json",
  device_profile: "device-profile.json",
  recording_profile: "recording-profile.json",
  game_profile: "game-profile.json",
  mapping: "mapping.json",
  markers: "markers.jsonl",
  input_calibration: "input-calibration.json",
};

function sessionArtifactContentType(kind: SessionArtifactKind): string {
  return kind === "markers" ? "application/x-ndjson" : "application/json";
}

async function ownedSession(
  client: PoolClient,
  deviceId: string,
  sessionId: string,
  requireOpen: boolean,
) {
  const result = await client.query<{
    id: string;
    status: string;
    client_session_id: string;
    ended_at: Date | null;
    started_at: Date;
    created_at: Date;
    updated_at: Date;
    capture_engine: string;
    native_width: number;
    native_height: number;
    fps_num: number;
    fps_den: number;
    catalog_mode: string | null;
    schema_version: string;
    recording_profile_has_audio: boolean;
  }>(
    `SELECT s.id, s.status, s.client_session_id, s.ended_at, s.started_at, s.created_at, s.updated_at,
            s.capture_engine, s.native_width, s.native_height, s.fps_num, s.fps_den, s.catalog_mode,
            s.schema_version,
            (rp.snapshot -> 'audio') IS NOT NULL AS recording_profile_has_audio
       FROM sessions s
       JOIN devices d ON d.id = s.device_id
       JOIN trusted_catalog_versions rp ON rp.id = s.recording_catalog_version_id
      WHERE s.id = $1 AND d.credential_fingerprint = $2
        AND d.status IN ('pending','active')
      FOR UPDATE OF s`,
    [sessionId, deviceId],
  );
  const session = requiredRow(result.rows[0], "session_not_found", "session was not found");
  if (requireOpen && isSessionSealed(session.status)) {
    throw conflict("session_sealed", "session is immutable after sealing");
  }
  return session;
}

/**
 * 认这台 active 设备，并解析它声明的精确 non-revoked device profile。
 *
 * 设备登记不再授予 profile 资格。具体 profile 由已验签 game catalog 选择，硬件
 * 兼容性由录制前 preflight fail-closed 判断；服务端只校验这次声明命中精确 non-revoked
 * 行，并由 `resolveSessionCatalogs` 约束它必须是 trusted game_profile 引用的谱系。
 *
 * 导出是为了让测试打到真实代码而不是重打一遍这段逻辑（同本模块导出 SQL 的理由）。
 */
export async function resolveDeviceProfile(
  client: PoolClient,
  credentialFingerprint: string,
  declared: { id: string; version: string; contentHash: string },
) {
  const device = await deviceForPrincipal(client, credentialFingerprint);
  const deviceCatalog = await catalogVersion(
    client,
    "device_profile",
    declared.id,
    declared.contentHash,
    declared.version,
    null,
  );
  return { device, deviceCatalog };
}

/**
 * Resolve the complete server-trusted catalog tuple for session admission.
 * The request may name exact versions, but it cannot compose arbitrary approved
 * rows: the trusted game_profile is the authoritative cross-reference.
 */
export async function resolveSessionCatalogs(
  client: PoolClient,
  credentialFingerprint: string,
  input: Pick<CreateSessionInput, "game" | "deviceProfile" | "recordingProfile" | "mapping">,
) {
  const { device, deviceCatalog } = await resolveDeviceProfile(
    client,
    credentialFingerprint,
    input.deviceProfile,
  );
  await requireGame(client, input.game.id);
  const gameCatalog = await catalogVersion(
    client,
    "game_profile",
    input.game.id,
    input.game.contentHash,
    input.game.version,
    input.game.id,
  );
  const recordingCatalog = await catalogVersion(
    client,
    "recording_profile",
    input.recordingProfile.id,
    input.recordingProfile.contentHash,
    input.recordingProfile.version,
    input.game.id,
  );
  const mappingCatalog = await catalogVersion(
    client,
    "game_mapping",
    input.mapping.id,
    input.mapping.contentHash,
    input.mapping.version,
    input.game.id,
  );
  assertGameProfileReferences(input, gameCatalog.snapshot);
  return { device, deviceCatalog, gameCatalog, recordingCatalog, mappingCatalog };
}

async function catalogVersion(
  client: PoolClient,
  kind: string,
  catalogId: string,
  contentHash: string,
  version: string | null,
  gameId: string | null,
): Promise<{ id: string; snapshot: JsonRecord }> {
  const result = await client.query<{ id: string; snapshot: JsonRecord }>(
    `SELECT id, snapshot FROM trusted_catalog_versions
      WHERE kind = $1 AND catalog_id = $2 AND content_hash = $3 AND status <> 'revoked'
        AND ($4::text IS NULL OR version = $4)
        AND (($5::text IS NULL AND game_id IS NULL) OR game_id = $5)
      ORDER BY created_at DESC
      LIMIT 2`,
    [kind, catalogId, contentHash, version, gameId],
  );
  if (result.rows.length === 0) {
    throw notFound(
      "catalog_version_not_found",
      await describeCatalogMiss(client, kind, catalogId, contentHash, version, gameId),
    );
  }
  if (result.rows.length > 1) {
    throw conflict("catalog_version_ambiguous", `${kind} catalog identity resolves to multiple versions`);
  }
  return result.rows[0]!;
}

/**
 * 把「没找到」说成「差在哪一项」。
 *
 * 旧文案是一句 `${kind} catalog version was not found`——四本册子逐本查，任何一本
 * 对不上都得到同一句话，而**被拒绝的请求在数据库里不留任何行**（docs/101 §3），
 * 所以排障时既不知道是哪一本，也不知道差的是版本号还是字节。2026-08-05/06 那
 * 31 次拒绝就是这样，服务端日志答不出任何一个问题（docs/101 §3.3）。
 *
 * 这里补一次**只读的**诊断查询，逐级放宽条件定位差异点。安全上是干净的：
 * 调用方是已认证设备，而它拿到的每一项（catalog_id、版本、content_hash）本来就是
 * 它自己通过 `GET /v1/catalog` 能取到的目录内容，不构成新的信息泄露。
 */
export async function describeCatalogMiss(
  client: PoolClient,
  kind: string,
  catalogId: string,
  contentHash: string,
  version: string | null,
  gameId: string | null,
): Promise<string> {
  const asked = `${kind} ${catalogId}${version === null ? "" : `@${version}`} (${contentHash})`;
  const candidates = await client.query<{
    version: string | null;
    content_hash: string;
    status: string;
    game_id: string | null;
  }>(
    `SELECT version, content_hash, status, game_id FROM trusted_catalog_versions
      WHERE kind = $1 AND catalog_id = $2
      ORDER BY created_at DESC
      LIMIT 5`,
    [kind, catalogId],
  );
  if (candidates.rows.length === 0) {
    return `${asked} was not found: this deployment has no ${kind} catalog with that id at all`;
  }
  const hashMatch = candidates.rows.find((row) => row.content_hash === contentHash);
  if (hashMatch) {
    // 字节对得上，那么落差只可能来自另外三个条件之一。
    if (hashMatch.status === "revoked") {
      return `${asked} was not found: those exact bytes exist but the version is revoked`;
    }
    if (version !== null && hashMatch.version !== version) {
      return `${asked} was not found: those exact bytes are registered as version ${hashMatch.version ?? "(null)"}, not ${version}`;
    }
    return `${asked} was not found: those exact bytes are registered against game ${hashMatch.game_id ?? "(null)"}, not ${gameId ?? "(null)"}`;
  }
  // 最常见的一种：目录在、版本可能也在，但客户端手上的字节不是服务端信任的那份。
  const known = candidates.rows
    .filter((row) => row.status !== "revoked")
    .map((row) => `${row.version ?? "(null)"}=${row.content_hash}`)
    .join(", ");
  return `${asked} was not found: content hash does not match any trusted version of this catalog${
    known === "" ? " (all known versions are revoked)" : `; this deployment trusts ${known}`
  }`;
}

function assertGameProfileReferences(
  input: Pick<CreateSessionInput, "game" | "deviceProfile" | "recordingProfile" | "mapping">,
  snapshot: JsonRecord,
): void {
  if (
    snapshot.game_id !== input.game.id ||
    snapshot.device_profile_id !== input.deviceProfile.id ||
    snapshot.recording_profile_id !== input.recordingProfile.id ||
    snapshot.mapping_id !== input.mapping.id
  ) {
    throw conflict(
      "game_profile_reference_mismatch",
      "the trusted game profile does not reference the declared device, recording, and mapping profiles",
    );
  }
}

function assertRecordingProfileFacts(
  input: CreateSessionInput["recordingProfile"],
  snapshot: JsonRecord,
): void {
  const width = snapshot.width;
  const height = snapshot.height;
  const fpsNum = snapshot.fps_num ?? snapshot.fpsNum;
  const fpsDen = snapshot.fps_den ?? snapshot.fpsDen;
  const captureEngine = snapshot.capture_backend ?? snapshot.captureEngine;
  if (
    width !== input.width ||
    height !== input.height ||
    fpsNum !== input.fpsNum ||
    fpsDen !== input.fpsDen ||
    captureEngine !== input.captureEngine
  ) {
    throw conflict(
      "recording_profile_facts_mismatch",
      "capture facts do not match the server catalog snapshot",
    );
  }
}

/**
 * 录制档案里的音频事实，并在需要时与会话声明的音频世代配对。
 *
 * `audioRequested` 为 `null` 表示这条车道**不在会话层声明音频世代**（中性契约，
 * issue #855 §B）：那就没有可配对的东西，但"档案里若有 audio 块必须是规范那一份"
 * 这道闸照旧。返回值是服务端认定的事实——这份录制档案配没配音频——调用方拿它
 * 决定 consent scope 与 clip 侧的配对。
 */
export function assertRecordingProfileAudioFacts(
  audioRequested: boolean | null,
  snapshot: JsonRecord,
): boolean {
  const audio = snapshot.audio;
  const hasAudioProfile = audio !== undefined;
  const canonicalAudioProfile =
    isJsonRecord(audio) &&
    Object.keys(audio).length === Object.keys(PROCESS_LOOPBACK_AUDIO_PROFILE).length &&
    Object.entries(PROCESS_LOOPBACK_AUDIO_PROFILE).every(
      ([key, expected]) => audio[key] === expected,
    );
  if (
    (audioRequested !== null && audioRequested !== hasAudioProfile) ||
    (hasAudioProfile && !canonicalAudioProfile)
  ) {
    throw conflict(
      "recording_profile_audio_mismatch",
      "session audio generation does not match the server recording-profile snapshot",
    );
  }
  return hasAudioProfile;
}

/**
 * 采集时的**来源分类**派生（issue #855 §B / §C）。
 *
 * 契约版本是客户端在 `createSession` 那一刻声明的，所以"我自称走中性车道"必须
 * **不足以**换来 `training_candidate`。真正决定的是三类服务端事实，每一类都在采集
 * 发生时就已经存在、事后伪造不了：
 *
 * 1. **已登记 Catalog**——这次会话解析到的四本册子（device / game / recording /
 *    mapping）都必须是已登记并批准登记的精确版本行。`catalogVersion()` 的 404 只挡
 *    "根本不存在或已撤销"；`draft` 是**存在但尚未登记完成**，它能通过解析却不该换
 *    来 candidate。
 * 2. **capture admission**——`games.allowed_for_capture`。`requireGame()` 只问这行在
 *    不在，不问现在还让不让录。
 * 3. **身份与 consent**——设备绑定的标注员，以及覆盖采集起始时刻的那份同意书。这半
 *    还有 0032 的 DB CHECK 兜底（`non_trainable` 或 annotator+consent 齐全）；另外
 *    两半没有库级兜底，只有这里。
 *
 * 少任何一条都落 `non_trainable`——**永久**不可训练，不是"以后补上就好"。这是有意的：
 * 缺的正是那些事后补不回来的事实，而 `training_candidate` 的全部含义就是"来源链是
 * 完整的，只等裁决"。得到 candidate 也**不等于**通过：`clip_trainability` 的每一条
 * blocker 照旧要清零（`approved_for_trainable`、executable、QA……）。
 *
 * 导出以便测试直接打到派生本身，而不是只打到那两条提前返回的 404。
 */
export async function deriveSourceClassification(
  client: PoolClient,
  family: AdmissionFamily,
  facts: {
    gameId: string;
    annotatorId: string | null;
    consentId: string | null;
    /** 本次会话解析出的全部精确 catalog 版本行。 */
    catalogVersionIds: readonly string[];
  },
): Promise<{
  classification: "non_trainable" | "training_candidate";
  withheldBecause: string[];
}> {
  // 上限就是 non_trainable 的车道（legacy v1、staging）连查都不用查：它的数据在
  // 来源处就永久不可训练，任何治理事实都改不了这一点。
  if (family.bestClassification !== "training_candidate") {
    return { classification: "non_trainable", withheldBecause: ["lane_non_trainable"] };
  }
  const result = await client.query<{
    capture_allowed: boolean | null;
    registered_catalog_versions: number;
  }>(sourceClassificationFactsSql, [facts.gameId, facts.catalogVersionIds]);
  const row = result.rows[0];
  const withheldBecause = [
    row?.capture_allowed === true ? null : "capture_not_admitted",
    Number(row?.registered_catalog_versions ?? 0) === facts.catalogVersionIds.length
      ? null
      : "catalog_identity_not_registered",
    facts.annotatorId === null ? "missing_annotator" : null,
    facts.consentId === null ? "missing_consent" : null,
  ].filter((reason): reason is string => reason !== null);
  return {
    classification: withheldBecause.length === 0 ? "training_candidate" : "non_trainable",
    withheldBecause,
  };
}

/**
 * 派生用到的两条服务端事实。导出的理由同 `authenticateDeviceSql`：每个谓词都是
 * 承重的，测试执行这条语句本身而不是重打一遍。
 *
 * `status IN ('approved','retired')` 与 `clip_trainability` 对四本册子的取法逐字
 * 一致——`retired` 是"这一版不再下发"，不是"这一版从来不算数"，历史会话引用的正是
 * 这种行。
 */
export const sourceClassificationFactsSql = `SELECT
         (SELECT g.allowed_for_capture FROM games g WHERE g.id = $1) AS capture_allowed,
         (SELECT count(*)::int FROM trusted_catalog_versions tcv
           WHERE tcv.id = ANY($2::uuid[])
             AND tcv.status IN ('approved', 'retired')) AS registered_catalog_versions`;

async function requireGame(client: PoolClient, gameId: string): Promise<void> {
  const result = await client.query<{ id: string }>(`SELECT id FROM games WHERE id = $1`, [gameId]);
  if (result.rows.length !== 1) {
    throw notFound("game_not_registered", "game is not registered in the production catalog");
  }
}

async function semanticExpectation(
  client: PoolClient,
  row: {
    kind: ArtifactKind;
    clip_id: string;
    client_session_id: string;
    session_schema_version: string;
    client_run_id: string;
    segment_seq: number;
    session_frame_start: string;
    n_frames: string;
    qpc_start: string;
    qpc_end: string;
    event_count: string;
    width: number;
    height: number;
    fps_num: number;
    fps_den: number;
    health_json: Record<string, number>;
    session_manifest_json: JsonRecord;
    clip_meta_json: PutClipInput;
  },
): Promise<NonNullable<VerificationJob["semantic"]>> {
  const result = await client.query<{
    kind: ArtifactKind;
    expected_sha256: string;
    expected_size_bytes: string;
  }>(
    `SELECT kind, expected_sha256, expected_size_bytes
       FROM artifact_objects WHERE clip_id = $1`,
    [row.clip_id],
  );
  const artifacts = new Map(result.rows.map((artifact) => [artifact.kind, artifact]));
  if (!isValidClipArtifactSet([...artifacts.keys()])) {
    throw conflict("artifact_set_incomplete", "semantic verification requires a valid clip artifact set");
  }
  const declaredAudio = row.clip_meta_json.audio;
  const audioArtifact = artifacts.get("audio");
  const audioShouldExist = declaredAudio !== undefined && declaredAudio.state !== "absent";
  if (audioShouldExist !== (audioArtifact !== undefined)) {
    throw conflict("artifact_set_incomplete", "audio metadata and artifact set disagree");
  }
  const required = (kind: ArtifactKind) =>
    requiredRow(artifacts.get(kind), "artifact_not_found", `${kind} artifact was not found`);
  if (row.kind === "manifest") {
    const expectedEntries: Record<string, string> = {
      "video.mp4": required("video").expected_sha256,
      "frames.jsonl.zst": required("frames").expected_sha256,
      "events.jsonl.zst": required("events").expected_sha256,
      "meta.json": required("meta").expected_sha256,
    };
    if (audioArtifact) {
      expectedEntries["audio.pcm.zst"] = audioArtifact.expected_sha256;
    }
    return {
      kind: "manifest",
      expectedEntries,
    };
  }
  const mapping = row.session_manifest_json.mapping;
  if (!isJsonRecord(mapping)) {
    throw conflict("session_manifest_invalid", "session mapping identity is unavailable");
  }
  const generation = segmentMetaGenerationExpectation(row.session_schema_version);
  return {
    kind: "meta",
    expectedSubset: {
      schema_version: row.clip_meta_json.classification.artifactSchemaVersion,
      session_id: row.client_session_id,
      run_id: row.client_run_id,
      segment_index: row.segment_seq,
      session_frame_start: Number(row.session_frame_start),
      n_frames: Number(row.n_frames),
      state: generation.state,
      ...generation.trainingFields,
      input_status: "raw_input_v1",
      qpc_start: Number(row.qpc_start),
      qpc_end: Number(row.qpc_end),
      width: row.width,
      height: row.height,
      fps_num: row.fps_num,
      fps_den: row.fps_den,
      mapping: {
        id: mapping.id,
        version: mapping.version,
        content_hash: mapping.contentHash,
      },
      contract: { version: "dm-1" },
      health: {
        duplicate_frames: row.health_json.duplicateFrames,
        source_superseded: row.health_json.sourceSuperseded,
        queue_drops: row.health_json.queueDrops,
        input_overflow: row.health_json.inputOverflow,
      },
      video: {
        path: "video.mp4",
        bytes: Number(required("video").expected_size_bytes),
        sha256: required("video").expected_sha256,
      },
      frames: {
        path: "frames.jsonl.zst",
        lines: Number(row.n_frames),
        sha256: required("frames").expected_sha256,
      },
      events: {
        path: "events.jsonl.zst",
        lines: Number(row.event_count),
        sha256: required("events").expected_sha256,
      },
      ...(declaredAudio === undefined ? {} : { audio: declaredAudio }),
    },
    ...(generation.forbiddenTopLevelKeys
      ? { forbiddenTopLevelKeys: generation.forbiddenTopLevelKeys }
      : {}),
  };
}

/**
 * Server-owned clip meta vocabulary for each persisted session generation.
 *
 * The v2 upload envelope deliberately carries no training booleans. The stored
 * meta must match that same neutral contract: merely omitting the legacy fields
 * from `expectedSubset` would still allow a client to smuggle them in as extra
 * keys, so the verifier also requires them to be absent.
 */
export function segmentMetaGenerationExpectation(sessionSchemaVersion: string): {
  state: "ready" | "non_trainable_ready";
  trainingFields: Record<string, unknown>;
  forbiddenTopLevelKeys?: readonly string[];
} {
  switch (sessionSchemaVersion) {
    case "dm-session-ingest-2":
      return {
        state: "ready",
        trainingFields: {},
        forbiddenTopLevelKeys: ["non_trainable", "production_certified"],
      };
    case "dm-local-dev-session-1":
    case "dm-local-dev-session-2":
    case "dm-local-dev-session-3":
      return {
        state: "non_trainable_ready",
        trainingFields: { non_trainable: true, production_certified: false },
      };
    default:
      throw new Error(`unsupported persisted session schema ${sessionSchemaVersion}`);
  }
}

async function assertPurgeLease(
  client: PoolClient,
  job: PurgeJob,
): Promise<{
  attempt_count: number;
  max_attempts: number;
  progress_json: JsonRecord;
}> {
  const result = await client.query<{
    attempt_count: number;
    max_attempts: number;
    progress_json: JsonRecord;
  }>(
    `SELECT attempt_count, max_attempts, progress_json
       FROM purge_jobs
      WHERE id = $1 AND scope = 'clip' AND clip_id = $2 AND status = 'running'
        AND attempt_count = $3 AND locked_by = $4
      FOR UPDATE`,
    [job.id, job.clipId, job.attempt, job.leaseOwner],
  );
  return requiredRow(result.rows[0], "purge_lease_lost", "purge job is no longer leased");
}

function progressIds(progress: JsonRecord, key: string): Set<string> {
  const value = progress[key];
  return new Set(Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : []);
}

function isJsonRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function audit(
  client: PoolClient,
  actorId: string,
  action: string,
  entityType: string,
  entityId: string,
  details: JsonRecord,
  actorType: "device" | "worker" | "service" = "device",
): Promise<void> {
  await client.query(
    `INSERT INTO audit_events(event_id, actor_type, actor_id, action, entity_type, entity_id, details)
     VALUES ($1,$2,$3,$4,$5,$6,$7)`,
    [randomUUID(), actorType, actorId, action, entityType, entityId, details],
  );
}

async function enqueueClipPurge(
  client: PoolClient,
  clipId: string,
  reason: string,
  actorType: "device" | "service" | "worker" = "service",
  actorId = "dataminer-api",
): Promise<boolean> {
  const clip = await client.query<{ retention_state: string }>(
    `SELECT retention_state FROM clips WHERE id = $1 FOR UPDATE`,
    [clipId],
  );
  const row = requiredRow(clip.rows[0], "clip_not_found", "clip was not found for purge");
  if (["purged", "purge_failed"].includes(row.retention_state)) return false;
  const transitioned = await client.query(
    `UPDATE clips
        SET retention_state = 'purge_requested',
            purge_requested_at = COALESCE(purge_requested_at, transaction_timestamp())
      WHERE id = $1 AND retention_state = 'retained'`,
    [clipId],
  );
  const inserted = await client.query<{ id: string }>(
    `INSERT INTO purge_jobs(
       id, scope, clip_id, reason, requested_by_type, requested_by_id, status
     ) VALUES ($1, 'clip', $2, $3, $4, $5, 'pending')
     ON CONFLICT DO NOTHING
     RETURNING id`,
    [randomUUID(), clipId, reason, actorType, actorId],
  );
  if (transitioned.rowCount === 1 || inserted.rowCount === 1) {
    await audit(
      client,
      actorId,
      "clip.purge_requested",
      "clip",
      clipId,
      { reason, ...(inserted.rows[0] ? { purgeJobId: inserted.rows[0].id } : {}) },
      actorType,
    );
  }
  return inserted.rowCount === 1;
}

function artifactState(storage: string, verification: string): ArtifactRecord["state"] {
  if (storage === "purged") return "purged";
  if (storage === "purging") return "purging";
  if (verification === "rejected") return "rejected";
  if (verification === "verified") return "verified";
  if (verification === "verifying") return "verifying";
  if (storage === "stored") return "stored";
  if (storage === "uploading") return "uploading";
  return "declared";
}

function clipState(ingest: string, verification: string): ClipRecord["state"] {
  if (verification === "rejected") return "rejected";
  if (verification === "verified") return "verified";
  if (verification === "verifying") return "verifying";
  if (verification === "queued") return "queued";
  if (ingest === "stored") return "stored";
  if (ingest === "uploading") return "uploading";
  return "declared";
}

function isSessionSealed(status: string): boolean {
  return status === "sealed" || status === "completed" || status === "rejected";
}

function sessionState(status: string): SessionRecord["state"] {
  if (status === "rejected") return "rejected";
  return isSessionSealed(status) ? "sealed" : "open";
}

function uploadGateOpen(row: UploadGateRow, admitted: readonly string[]): boolean {
  return (
    row.device_allowed &&
    admitted.includes(row.session_classification) &&
    row.retention_allowed &&
    row.before_retention_deadline &&
    row.intent_not_expired &&
    row.multipart_not_expired
  );
}

function assertUploadGate(row: UploadGateRow, admitted: readonly string[]): void {
  if (!row.device_allowed) {
    throw notFound("device_not_registered", "device is not active");
  }
  // Widened from the historical `= 'non_trainable'` literal: a deployment that
  // admits the consented first-party lane also moves that lane's bytes. A
  // classification no admitted family can produce still fails closed here.
  if (!admitted.includes(row.session_classification)) {
    throw conflict("classification_blocked", "upload is outside this deployment's admitted RAW boundary");
  }
  if (!row.retention_allowed || !row.before_retention_deadline) {
    throw conflict("retention_blocked", "clip retention no longer permits upload");
  }
  if (!row.intent_not_expired || !row.multipart_not_expired) {
    throw conflict("upload_intent_expired", "upload intent has expired");
  }
}

function iso(value: Date | string): string {
  return value instanceof Date ? value.toISOString() : new Date(value).toISOString();
}

function requiredRow<T>(value: T | null | undefined, code: string, message: string): T {
  if (value === null || value === undefined) throw notFound(code, message);
  return value;
}
