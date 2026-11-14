import { GetObjectCommand, S3Client } from "@aws-sdk/client-s3";

import { conflict } from "./errors.js";

// 会话验收报告的读取面（docs/136 §4）。验收管线（tools/session-acceptance/
// accept.py，GH Actions cron）把报告写回 session 前缀下的固定对象名；console
// 只读它、原样透传。报告是缓存不是判决（docs/136 §4 末条）：与 clip meta 冲突
// 时以 meta 为准，这里不做任何再计算。

/** schema `dm-session-acceptance/1` 的固定对象名（accept.py REPORT_REL）。 */
const REPORT_REL = "acceptance/dm-session-acceptance-1.json";

/**
 * 摄取车道的 session 前缀根（docs/67 §3.1、docs/136 §4）。顺序即优先级：
 * 同一 session 多条车道都可能有对象，以 non-trainable 车道为准源。
 *
 * `raw/v2` 是 issue #855 的中性摄取车道，2026-08-29 起是生产唯一的落桶车道；
 * 漏掉它，v2 那批 session 的「验收」栏会**恒空**——不是「未验收」而是根本没去看
 * （docs/167 §3.2 记的同一处写死）。部署侧的 CONSOLE_R2_PREFIX 早就带了它，
 * 这里补齐后两边才一致。
 */
const LANE_PREFIXES = [
  "raw/v1/non-trainable/dataminer-api-production/sessions",
  "raw/v2/dataminer-api-production/sessions",
  "raw/v1/attested-trainable/dataminer-api-production/sessions",
] as const;

/** 一份报告撑死几十 KiB；1 MiB 上限防的是拿错对象而不是正常报告。 */
const MAX_REPORT_BYTES = 1024 * 1024;

export interface AcceptanceStorage {
  /** 对象字节；不存在返回 null（这是业务态「未验收」，不是错误）。 */
  getObject(bucket: string, key: string): Promise<Buffer | null>;
}

// 与 R2ReviewStorage 同构（review.ts）：S3 兼容端点 + region auto。独立客户端
// 而不是复用 review 的，理由同 preview/review 的分离——各自的凭据各自可撤销。
export class R2AcceptanceStorage implements AcceptanceStorage {
  readonly #client: S3Client;

  constructor(options: { accountId: string; accessKeyId: string; secretAccessKey: string }) {
    this.#client = new S3Client({
      region: "auto",
      endpoint: `https://${options.accountId}.r2.cloudflarestorage.com`,
      credentials: {
        accessKeyId: options.accessKeyId,
        secretAccessKey: options.secretAccessKey,
      },
    });
  }

  async getObject(bucket: string, key: string): Promise<Buffer | null> {
    try {
      const response = await this.#client.send(new GetObjectCommand({ Bucket: bucket, Key: key }));
      if (!response.Body) return null;
      return Buffer.from(await response.Body.transformToByteArray());
    } catch (error) {
      if (isMissingObjectError(error)) return null;
      throw error;
    }
  }
}

function isMissingObjectError(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const name = (error as { name?: string }).name;
  const status = (error as { $metadata?: { httpStatusCode?: number } }).$metadata?.httpStatusCode;
  return name === "NotFound" || name === "NoSuchKey" || status === 404;
}

export class AcceptanceService {
  readonly #storage: AcceptanceStorage | null;
  readonly #bucket: string;

  constructor(storage: AcceptanceStorage | null, bucket: string) {
    this.#storage = storage;
    this.#bucket = bucket;
  }

  /** false = 部署没配 ACCEPTANCE_R2_*，路由答 503 而不是伪装 404。 */
  get available(): boolean {
    return this.#storage !== null;
  }

  /**
   * 按 docs/136 §4 读取报告并原样返回；两条车道都没有 → null（未验收）。
   * 只在这里 JSON.parse 一次——不是为了改写，而是保证透传出去的确实是 JSON：
   * 一个被截断/写坏的报告应当是 409，而不是让浏览器端去解析垃圾。
   */
  async report(sessionId: string): Promise<unknown | null> {
    if (!this.#storage) return null;
    for (const lane of LANE_PREFIXES) {
      const bytes = await this.#storage.getObject(
        this.#bucket,
        `${lane}/${sessionId}/${REPORT_REL}`,
      );
      if (bytes === null) continue;
      if (bytes.length > MAX_REPORT_BYTES) {
        throw conflict("malformed_acceptance_report", "the acceptance report object is too large");
      }
      try {
        return JSON.parse(bytes.toString("utf8")) as unknown;
      } catch {
        throw conflict("malformed_acceptance_report", "the acceptance report is not parseable JSON");
      }
    }
    return null;
  }
}
