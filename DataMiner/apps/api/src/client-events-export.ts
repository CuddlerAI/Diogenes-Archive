import { randomUUID } from "node:crypto";
import { gzipSync } from "node:zlib";

import { GoogleAuth } from "google-auth-library";

import type { Environment } from "./environment.js";

/**
 * 把 `client_event.recorded` outbox 排空到 GCS 的行为日志落地区（docs/23 §6）。
 *
 * 压缩是 gzip 而不是任务书初稿的 zstd：BigQuery 的 NDJSON 外部表/load 只认
 * gzip（实测 zstd 直接解析失败），而「BQ 直接可查」是 raw 区存在的全部意义。
 *
 * 形态照抄 mirror-publisher 的 drainMirrorCredentialOutbox：按 id 认领、整批
 * 发布、at-least-once、失败整批退避、sink 未配置显式报错。差异只有两处：
 * outbox_jobs 带 status/locked_by 租约（mirror_credential_outbox 没有），认领用
 * FOR UPDATE SKIP LOCKED；以及 payload 是指针不是事件——事件本体在发布时从
 * client_events 回读，所以 purge 过的行（retention_state='purged'）自然导不出去，
 * 这是删除级联在导出面的第一道闸（docs/23 §7.3）。
 *
 * 幂等语义：对象名由批内 outbox id 区间决定，重试同一批覆盖写同名对象；重试
 * 认领到不同子集时会产生内容重叠的新对象——GCS raw 区允许重复，去重发生在
 * BQ MERGE（docs/23 §7.1）。
 */

export const EXPORT_SCHEMA_VERSION = "dm-client-event-export-1";

const BACKOFF_SECONDS = [15, 60, 300, 900, 3_600];

export type ExportedClientEvent = {
  export_schema_version: typeof EXPORT_SCHEMA_VERSION;
  event_id: string;
  device_id: string;
  annotator_id: string | null;
  consent_id: string | null;
  session_id: string | null;
  occurred_at: string;
  received_at: string;
  kind: string;
  severity: string;
  client_version: string;
  environment: Environment;
  payload: Record<string, unknown>;
  retention_state: string;
};

export interface ClientEventSink {
  /** Writes one finished object. objectName is bucket-relative. */
  publish(objectName: string, body: Uint8Array): Promise<void>;
}

export type ExportDrainResult = {
  exported: number;
  skippedPurged: number;
  failed: number;
  objects: string[];
};

export type ExportBacklog = {
  pending: number;
  oldestCreatedAt: string | null;
};

type SqlClient = {
  query<T>(text: string, values?: unknown[]): Promise<{ rows: T[] }>;
};

const CLAIM_JOBS = `
UPDATE outbox_jobs
   SET status = 'running', locked_at = transaction_timestamp(), locked_by = $2,
       updated_at = transaction_timestamp()
 WHERE id IN (
   SELECT id FROM outbox_jobs
    WHERE topic = 'client_event.recorded'
      AND status IN ('pending', 'retry_wait')
      AND available_at <= transaction_timestamp()
    ORDER BY id
    LIMIT $1
    FOR UPDATE SKIP LOCKED
 )
RETURNING id, payload, attempt_count, max_attempts
`;

const READ_EVENTS = `
SELECT id, event_id, device_id, annotator_id, consent_id, session_id,
       to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS occurred_at,
       to_char(received_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS received_at,
       to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS dt,
       to_char(occurred_at AT TIME ZONE 'UTC', 'HH24') AS hour,
       kind, severity, client_version, details, retention_state
  FROM client_events
 WHERE id = ANY($1::bigint[])
`;

const MARK_SUCCEEDED = `
UPDATE outbox_jobs
   SET status = 'succeeded', published_at = transaction_timestamp(),
       locked_at = NULL, locked_by = NULL, last_error_code = NULL,
       updated_at = transaction_timestamp()
 WHERE id = ANY($1::bigint[])
`;

/*
 * 整批退避，按批内已重试最多的那行定档（同 mirror-publisher 的理由）。攒满
 * max_attempts 转 dead_letter：留在表里可见，而不是无限重试打穿 GCS。
 */
const MARK_FAILED = `
UPDATE outbox_jobs
   SET status = CASE WHEN attempt_count + 1 >= max_attempts THEN 'dead_letter'::job_status
                     ELSE 'retry_wait'::job_status END,
       attempt_count = attempt_count + 1,
       last_error_code = $2,
       available_at = transaction_timestamp() + make_interval(secs => $3::double precision),
       locked_at = NULL, locked_by = NULL,
       updated_at = transaction_timestamp()
 WHERE id = ANY($1::bigint[])
`;

const RELEASE_CLAIM = `
UPDATE outbox_jobs
   SET status = 'pending', locked_at = NULL, locked_by = NULL,
       updated_at = transaction_timestamp()
 WHERE id = ANY($1::bigint[])
`;

const BACKLOG = `
SELECT count(*)::text AS pending,
       to_char(min(created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS oldest
  FROM outbox_jobs
 WHERE topic = 'client_event.recorded'
   AND status IN ('pending', 'retry_wait')
`;

type ClaimedJob = {
  id: string | number;
  payload: { clientEventId?: unknown };
  attempt_count: number;
  max_attempts: number;
};

type EventRow = {
  id: string | number;
  event_id: string;
  device_id: string;
  annotator_id: string | null;
  consent_id: string | null;
  session_id: string | null;
  occurred_at: string;
  received_at: string;
  dt: string;
  hour: string;
  kind: string;
  severity: string;
  client_version: string;
  details: Record<string, unknown>;
  retention_state: string;
};

/** 积压深度：worker 每轮 log 一行，是「导出是否活着」的第一观测点。 */
export async function clientEventExportBacklog(pool: SqlClient): Promise<ExportBacklog> {
  const result = await pool.query<{ pending: string; oldest: string | null }>(BACKLOG);
  return {
    pending: Number(result.rows[0]?.pending ?? 0),
    oldestCreatedAt: result.rows[0]?.oldest ?? null,
  };
}

export async function drainClientEventExport(
  pool: SqlClient,
  sink: ClientEventSink,
  options: {
    environment: Environment;
    batchSize?: number;
    workerId?: string;
    /** dry-run：认领后立即放回，只报数字，不碰 GCS（docs/23 §6 验收用）。 */
    dryRun?: boolean;
  },
): Promise<ExportDrainResult> {
  const batchSize = options.batchSize ?? 200;
  const workerId = options.workerId ?? `client-events-export:${randomUUID().slice(0, 8)}`;

  const claimed = await pool.query<ClaimedJob>(CLAIM_JOBS, [batchSize, workerId]);
  if (claimed.rows.length === 0) return { exported: 0, skippedPurged: 0, failed: 0, objects: [] };
  const jobIds = claimed.rows.map((row) => Number(row.id));

  if (options.dryRun) {
    await pool.query(RELEASE_CLAIM, [jobIds]);
    return { exported: 0, skippedPurged: 0, failed: 0, objects: [] };
  }

  const eventIds = claimed.rows
    .map((row) => row.payload?.clientEventId)
    .filter((id): id is number | string => typeof id === "number" || typeof id === "string")
    .map(Number);

  try {
    const events = await pool.query<EventRow>(READ_EVENTS, [eventIds]);

    // purge 过的行以及指针悬空的 job（事件行已被硬删）都算成功：它们的正确
    // 导出结果就是「不导出」。
    const exportable = events.rows.filter((row) => row.retention_state !== "purged");
    const skippedPurged = claimed.rows.length - exportable.length;

    const objects: string[] = [];
    const groups = new Map<string, EventRow[]>();
    for (const row of exportable) {
      const key = `${row.dt}/${row.hour}`;
      const group = groups.get(key) ?? [];
      group.push(row);
      groups.set(key, group);
    }

    for (const [key, rows] of groups) {
      const [dt, hour] = key.split("/");
      const first = String(Math.min(...rows.map((row) => Number(row.id)))).padStart(12, "0");
      const last = String(Math.max(...rows.map((row) => Number(row.id)))).padStart(12, "0");
      const objectName = `${options.environment}/client_events/dt=${dt}/hour=${hour}/${first}-${last}.ndjson.gz`;
      const ndjson = rows
        .map((row) =>
          JSON.stringify({
            export_schema_version: EXPORT_SCHEMA_VERSION,
            event_id: row.event_id,
            device_id: row.device_id,
            annotator_id: row.annotator_id,
            consent_id: row.consent_id,
            session_id: row.session_id,
            occurred_at: row.occurred_at,
            received_at: row.received_at,
            kind: row.kind,
            severity: row.severity,
            client_version: row.client_version,
            environment: options.environment,
            payload: row.details,
            retention_state: row.retention_state,
          } satisfies ExportedClientEvent),
        )
        .join("\n");
      await sink.publish(objectName, gzipSync(Buffer.from(`${ndjson}\n`, "utf8")));
      objects.push(objectName);
    }

    await pool.query(MARK_SUCCEEDED, [jobIds]);
    return { exported: exportable.length, skippedPurged, failed: 0, objects };
  } catch (error) {
    const code = error instanceof Error && error.name ? error.name.slice(0, 100) : "unknown_error";
    const attempt = Math.min(
      Math.max(...claimed.rows.map((row) => row.attempt_count)),
      BACKOFF_SECONDS.length - 1,
    );
    await pool.query(MARK_FAILED, [jobIds, code, BACKOFF_SECONDS[attempt]]);
    return { exported: 0, skippedPurged: 0, failed: claimed.rows.length, objects: [] };
  }
}

/**
 * GCS JSON API 实现。简单 media upload：一个对象一次 PUT 级别的请求，批对象
 * 都在几 MB 量级，不需要 resumable。认证走 ADC——Cloud Run 上是挂载的
 * dataminer-log-exporter@ SA（workload identity），本机是 impersonated ADC；
 * org policy 禁 SA key，所以这里没有任何密钥材料（docs/23 §6）。
 */
export class GcsClientEventSink implements ClientEventSink {
  readonly #bucket: string;
  readonly #auth: GoogleAuth;
  readonly #fetch: typeof fetch;

  constructor(input: { bucket: string; fetchImpl?: typeof fetch; auth?: GoogleAuth }) {
    this.#bucket = input.bucket;
    this.#auth =
      input.auth ?? new GoogleAuth({ scopes: "https://www.googleapis.com/auth/devstorage.read_write" });
    this.#fetch = input.fetchImpl ?? fetch;
  }

  async publish(objectName: string, body: Uint8Array): Promise<void> {
    const token = await this.#auth.getAccessToken();
    if (!token) throw new Error("gcs_publish_failed no_access_token");
    const url = `https://storage.googleapis.com/upload/storage/v1/b/${encodeURIComponent(this.#bucket)}/o?uploadType=media&name=${encodeURIComponent(objectName)}`;
    const response = await this.#fetch(url, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/gzip",
      },
      body,
    });
    if (!response.ok) {
      // 不回显响应体：错误里可能带对象名之外的请求细节，日志只需要状态码。
      throw new Error(`gcs_publish_failed status=${response.status}`);
    }
  }
}

/**
 * 未配置 sink 时的默认实现：明确失败，不假装成功（同 UnconfiguredCredentialSink
 * 的理由——积压看得见，静默丢弃看不见）。
 */
export class UnconfiguredClientEventSink implements ClientEventSink {
  async publish(): Promise<void> {
    throw new Error("client events export sink is not configured (set LOGS_GCS_BUCKET)");
  }
}
