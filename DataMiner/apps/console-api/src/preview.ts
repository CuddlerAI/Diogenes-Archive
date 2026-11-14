import { GetObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";

import type { ConsoleConfig } from "./config.js";
import { badRequest, conflict, notFound } from "./errors.js";
import type { SqlClient } from "./sql.js";

export type PreviewGrant = {
  url: string;
  objectKey: string;
  expiresInSeconds: number;
  expiresAt: string;
  sizeBytes: number;
};

export interface PreviewSigner {
  signObjectRead(objectKey: string, ttlSeconds: number): Promise<string>;
}

/**
 * Read-only presign for a single stored object.
 *
 * The credentials behind this are separate from the ingest write keys and can be
 * revoked on their own, so a compromised console cannot write or delete R2
 * objects. Even so, a presigned URL is a bearer token for those bytes: it is
 * scoped to one object, lives for minutes, and every issuance is audited.
 */
export class R2PreviewSigner implements PreviewSigner {
  readonly #client: S3Client;
  readonly #bucket: string;

  constructor(options: {
    accountId: string;
    bucket: string;
    accessKeyId: string;
    secretAccessKey: string;
  }) {
    this.#bucket = options.bucket;
    this.#client = new S3Client({
      region: "auto",
      endpoint: `https://${options.accountId}.r2.cloudflarestorage.com`,
      credentials: {
        accessKeyId: options.accessKeyId,
        secretAccessKey: options.secretAccessKey,
      },
    });
  }

  async signObjectRead(objectKey: string, ttlSeconds: number): Promise<string> {
    return getSignedUrl(
      this.#client,
      new GetObjectCommand({ Bucket: this.#bucket, Key: objectKey }),
      { expiresIn: ttlSeconds },
    );
  }
}

type PreviewRow = {
  object_key: string;
  bucket_name: string;
  expected_size_bytes: string | number;
  storage_state: string;
  retention_state: string;
};

export class PreviewService {
  readonly #pool: SqlClient;
  readonly #signer: PreviewSigner | null;
  readonly #config: ConsoleConfig;

  constructor(pool: SqlClient, signer: PreviewSigner | null, config: ConsoleConfig) {
    this.#pool = pool;
    this.#signer = signer;
    this.#config = config;
  }

  get available(): boolean {
    return this.#signer !== null && this.#config.preview !== null;
  }

  async signClipVideo(clipId: string): Promise<PreviewGrant> {
    const preview = this.#config.preview;
    if (!this.#signer || !preview) {
      throw conflict(
        "preview_not_configured",
        "this deployment has no read-only object credentials, so preview is unavailable",
      );
    }

    const result = await this.#pool.query<PreviewRow>(
      `SELECT ao.object_key, ao.bucket_name, ao.expected_size_bytes, ao.storage_state,
              c.retention_state
         FROM artifact_objects ao
         JOIN clips c ON c.id = ao.clip_id
        WHERE ao.clip_id = $1 AND ao.kind = 'video'`,
      [clipId],
    );
    const row = result.rows[0];
    if (!row) throw notFound("clip_video_not_found", "this clip has no video object");
    if (row.storage_state !== "stored") {
      throw conflict("object_not_stored", "the video object is not stored yet");
    }
    if (row.retention_state !== "retained") {
      throw conflict("object_purge_active", "this clip is being or has been purged");
    }
    if (row.bucket_name !== preview.bucket) {
      throw conflict("object_outside_bucket", "the object does not live in the configured bucket");
    }
    // Belt and braces on top of the row lookup: a key outside every prefix the
    // deployment admits is never signed, whatever the database says. The list
    // is per-lane (docs/67 §3.1) — a certified clip lives under a different
    // prefix than a non-trainable one, and pinning a single prefix here made
    // preview fail for exactly the data the certified lane produces.
    if (!preview.prefixes.some((prefix) => row.object_key.startsWith(`${prefix}/`))) {
      throw badRequest("object_outside_prefix", "the object key is outside the console prefixes");
    }

    const ttlSeconds = preview.ttlSeconds;
    const url = await this.#signer.signObjectRead(row.object_key, ttlSeconds);
    return {
      url,
      objectKey: row.object_key,
      expiresInSeconds: ttlSeconds,
      expiresAt: new Date(Date.now() + ttlSeconds * 1_000).toISOString(),
      sizeBytes: Number(row.expected_size_bytes),
    };
  }
}
