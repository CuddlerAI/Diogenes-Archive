import {
  RETIRE_OTHER_DEVICE_CREDENTIALS,
  UPSERT_DEVICE_CREDENTIAL,
} from "@dataminer/mirror/credentials";
import type { MirrorCredentialRow } from "@dataminer/mirror/credentials";

import type { SqlClient } from "./sql.js";

/**
 * 把 mirror_credential_outbox 排空到 dataminer-mirror 的 D1 凭据镜像表。
 *
 * 这条路是**单向、可丢、可重放**的：D1 镜像非权威（docs/13 §0），摄取面从来
 * 只读 Cloud SQL 的 devices.credential_fingerprint。发布失败的后果是「镜像还没
 * 收到这条更新」，不是「有人拿到了不该有的权限」——但停用/退役必须最终收敛，
 * 所以失败要重试而不是吞掉。
 *
 * 顺序保证来自 seq：D1 侧的 upsert 带 `WHERE excluded.source_seq > 现有` 守卫，
 * 因此即使重试导致乱序，旧行也改不动新状态。
 */

export type MirrorCredentialEvent = {
  seq: number;
  deviceId: string;
  credentialFingerprint: string;
  deviceStatus: MirrorCredentialRow["deviceStatus"];
  annotatorStatus: MirrorCredentialRow["annotatorStatus"];
  retireOthers: boolean;
};

export interface MirrorCredentialSink {
  publish(events: MirrorCredentialEvent[]): Promise<void>;
}

export type DrainResult = {
  published: number;
  failed: number;
};

const CLAIM_PENDING = `
SELECT seq, device_id, credential_fingerprint, device_status, annotator_status,
       retire_others, attempt_count
FROM mirror_credential_outbox
WHERE published_at IS NULL AND available_at <= transaction_timestamp()
ORDER BY seq
LIMIT $1
`;

const MARK_PUBLISHED = `
UPDATE mirror_credential_outbox
SET published_at = transaction_timestamp(), last_error = NULL
WHERE seq = ANY($1::bigint[]) AND published_at IS NULL
RETURNING seq
`;

/*
 * 退避上限 1 小时：镜像只是看板与普查输入，不值得为它把重试打成 DDoS；
 * 但也不能无限退，停用状态最终必须到达。
 */
const BACKOFF_SECONDS = [15, 60, 300, 900, 3_600];

const RECORD_FAILURE = `
UPDATE mirror_credential_outbox
SET attempt_count = attempt_count + 1,
    last_error = $2,
    available_at = transaction_timestamp()
                 + make_interval(secs => $3::double precision)
WHERE seq = ANY($1::bigint[]) AND published_at IS NULL
`;

type OutboxRow = {
  seq: string | number;
  device_id: string;
  credential_fingerprint: string;
  device_status: MirrorCredentialRow["deviceStatus"];
  annotator_status: MirrorCredentialRow["annotatorStatus"];
  retire_others: boolean;
  attempt_count: number;
};

export async function drainMirrorCredentialOutbox(
  pool: SqlClient,
  sink: MirrorCredentialSink,
  batchSize = 100,
): Promise<DrainResult> {
  const claimed = await pool.query<OutboxRow>(CLAIM_PENDING, [batchSize]);
  if (claimed.rows.length === 0) return { published: 0, failed: 0 };

  const events: MirrorCredentialEvent[] = claimed.rows.map((row) => ({
    // bigserial 经 pg 驱动是字符串；Number 在 2^53 前精确，这张表远到不了。
    seq: Number(row.seq),
    deviceId: row.device_id,
    credentialFingerprint: row.credential_fingerprint,
    deviceStatus: row.device_status,
    annotatorStatus: row.annotator_status,
    retireOthers: row.retire_others,
  }));
  const sequences = events.map((event) => event.seq);

  try {
    await sink.publish(events);
  } catch (error) {
    const message = error instanceof Error ? error.message : "unknown publish failure";
    // 整批一起退避，按批内已重试最多的那行定档：一批里混着新老行时，
    // 用最小值会让老行被反复快速重试。
    const attempt = Math.min(
      Math.max(...claimed.rows.map((row) => row.attempt_count)),
      BACKOFF_SECONDS.length - 1,
    );
    await pool.query(RECORD_FAILURE, [sequences, message.slice(0, 500), BACKOFF_SECONDS[attempt]]);
    return { published: 0, failed: events.length };
  }

  const marked = await pool.query<{ seq: string }>(MARK_PUBLISHED, [sequences]);
  return { published: marked.rows.length, failed: 0 };
}

/**
 * Cloudflare D1 HTTP API 实现。
 *
 * 用 `/query` 批量提交语句而不是逐条发：一次网络往返写完一批，
 * 也让「这批要么都到、要么都没到」在语义上更接近 outbox 的批边界。
 */
export class CloudflareD1CredentialSink implements MirrorCredentialSink {
  readonly #endpoint: string;
  readonly #apiToken: string;
  readonly #fetch: typeof fetch;

  constructor(input: {
    accountId: string;
    databaseId: string;
    apiToken: string;
    fetchImpl?: typeof fetch;
  }) {
    this.#endpoint = `https://api.cloudflare.com/client/v4/accounts/${input.accountId}/d1/database/${input.databaseId}/query`;
    this.#apiToken = input.apiToken;
    this.#fetch = input.fetchImpl ?? fetch;
  }

  async publish(events: MirrorCredentialEvent[]): Promise<void> {
    const statements: { sql: string; params: unknown[] }[] = [];
    const updatedAt = Date.now();
    for (const event of events) {
      statements.push({
        sql: UPSERT_DEVICE_CREDENTIAL,
        params: [
          event.credentialFingerprint,
          event.deviceId,
          event.seq,
          event.deviceStatus,
          event.annotatorStatus,
          updatedAt,
        ],
      });
      if (event.retireOthers) {
        statements.push({
          sql: RETIRE_OTHER_DEVICE_CREDENTIALS,
          params: [event.deviceId, event.credentialFingerprint, event.seq, updatedAt],
        });
      }
    }
    if (statements.length > 0) await this.#run(statements);
  }

  async #run(statements: { sql: string; params: unknown[] }[]): Promise<void> {
    const response = await this.#fetch(this.#endpoint, {
      method: "POST",
      headers: {
        authorization: `Bearer ${this.#apiToken}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ batch: statements }),
    });
    if (!response.ok) {
      // 不回显响应体：D1 的错误里可能带回 SQL 与参数，参数里有 fingerprint。
      throw new Error(`d1_publish_failed status=${response.status}`);
    }

    let payload: unknown;
    try {
      payload = await response.json();
    } catch {
      throw new Error(`d1_publish_failed status=${response.status} result=invalid_json`);
    }
    const result =
      typeof payload === "object" && payload !== null && "result" in payload
        ? (payload as { result?: unknown }).result
        : undefined;
    const success =
      typeof payload === "object" && payload !== null && "success" in payload
        ? (payload as { success?: unknown }).success
        : undefined;
    if (
      success !== true ||
      !Array.isArray(result) ||
      result.length !== statements.length ||
      result.some(
        (entry) =>
          typeof entry !== "object" ||
          entry === null ||
          !("success" in entry) ||
          (entry as { success?: unknown }).success !== true,
      )
    ) {
      // 不回显 D1 errors/result：它们可能包含 SQL 与 fingerprint。
      throw new Error(`d1_publish_failed status=${response.status} result=unsuccessful`);
    }
  }
}

/**
 * 未配置 D1 时的默认实现：明确失败，不假装成功。
 *
 * 静默 no-op 会让 outbox 立刻标记 published，然后镜像永远是空的而没人发现。
 * 宁可让积压长出来——积压看得见，静默丢弃看不见。
 */
export class UnconfiguredCredentialSink implements MirrorCredentialSink {
  async publish(): Promise<void> {
    throw new Error("mirror credential sink is not configured");
  }
}
