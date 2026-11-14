import { createHash } from "node:crypto";
import { zstdDecompressSync } from "node:zlib";

import {
  GetObjectCommand,
  HeadObjectCommand,
  ListObjectsV2Command,
  S3Client,
} from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";

import type { ConsoleConfig, ReviewSource } from "./config.js";
import { badRequest, conflict, notFound } from "./errors.js";
import type { SqlClient } from "./sql.js";

// The data-review surface (docs/33): browse the two R2 buckets, hand the
// reviewer component its bytes, and never let either step reach outside the
// configured prefix allowlist. Every byte exit here is audited by the caller;
// this module owns the guards, not the audit trail.

const MAX_INLINE_COMPRESSED_BYTES = 64 * 1024 * 1024;
const MAX_INLINE_DECOMPRESSED_BYTES = 256 * 1024 * 1024;
const MAX_META_BYTES = 1024 * 1024;
// A 720p f32 frame is 3.5 MiB and a depth-index.jsonl runs a few MiB for a
// whole capture. 32 MiB leaves headroom for 1080p frames (7.9 MiB) without
// letting the depth route become a general-purpose byte pump.
const MAX_DEPTH_OBJECT_BYTES = 32 * 1024 * 1024;
const MAX_TREE_PAGE = 1000;
// Viz artifact plane (docs/104 §3): skill-generated visualization groups live
// under this prefix; signing is limited to it so the viz route can never become
// a side door to `verified/` delivery objects or RAW segments.
const VIZ_PREFIX = "viz/";
const VIZ_SIGNABLE_EXTENSIONS = [".html", ".glb", ".ply", ".mp4", ".npz"];
// dataminer-api clips (apps/api `clipArtifactObjectKey`) store every artifact
// under a content-addressed name — `<kind>-<sha256hex>.<ext>` — and keep the
// logical five-piece names only inside the manifest. For those objects the
// embedded hash IS the integrity declaration: there is no sha256.json sibling,
// and the meta lives under `meta-<sha256>.json` (docs/33 §12).
const CONTENT_ADDRESSED_FRAMES = /^frames-([0-9a-f]{64})\.jsonl\.zst$/;
const CONTENT_ADDRESSED_EVENTS = /^events-([0-9a-f]{64})\.jsonl\.zst$/;
const CONTENT_ADDRESSED_META = /^meta-[0-9a-f]{64}\.json$/;

export type ReviewTreeObject = {
  key: string;
  sizeBytes: number;
  lastModified: string | null;
};

export type ReviewTreePage = {
  source: string;
  prefix: string;
  directories: string[];
  objects: ReviewTreeObject[];
  nextCursor: string | null;
};

export type ReviewObjectInfo = {
  source: string;
  key: string;
  sizeBytes: number;
  lastModified: string | null;
  dbAuthority: "production" | "none";
  /** true/false only when this console's database is the registration
   * authority for the bucket; null for sources whose registry lives in another
   * environment's database (docs/33 §3.2 — not an unregistered-object finding). */
  dbRegistered: boolean | null;
  artifact: { clipId: string | null; kind: string; storageState: string } | null;
};

export type ReviewVideoGrant = {
  url: string;
  source: string;
  objectKey: string;
  expiresInSeconds: number;
  expiresAt: string;
  sizeBytes: number;
};

export type ReviewFramesSchema = "dm1" | "raw-v1" | "raw-events-v1";

export type ReviewFramesResult = {
  source: string;
  key: string;
  schema: ReviewFramesSchema;
  rows: unknown[];
  /** Frames per second when the sibling meta declares it; null means the
   * reviewer must say "fps undeclared" rather than silently assume 30. */
  fps: number | null;
  meta: unknown | null;
  provenance: {
    objectKey: string;
    sizeBytes: number;
    sha256: string;
    sha256Verified: boolean;
    frameCountVerified: boolean;
  };
};

export type ReviewMetaResult = {
  source: string;
  key: string;
  sizeBytes: number;
  meta: unknown;
};

export interface ReviewStorage {
  list(
    bucket: string,
    options: { prefix: string; cursor?: string; maxKeys?: number },
  ): Promise<{
    objects: ReviewTreeObject[];
    directories: string[];
    nextCursor: string | null;
  }>;
  head(
    bucket: string,
    key: string,
  ): Promise<{ sizeBytes: number; lastModified: string | null } | null>;
  getObject(bucket: string, key: string): Promise<Buffer>;
  signObjectRead(bucket: string, key: string, ttlSeconds: number): Promise<string>;
}

// Same construction as R2PreviewSigner, plus list/head/get. One client serves
// every source: the credential is a single token scoped to exactly the review
// buckets, so the per-source boundary is the bucket parameter, not the client.
export class R2ReviewStorage implements ReviewStorage {
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

  async list(
    bucket: string,
    options: { prefix: string; cursor?: string; maxKeys?: number },
  ): Promise<{ objects: ReviewTreeObject[]; directories: string[]; nextCursor: string | null }> {
    const response = await this.#client.send(
      new ListObjectsV2Command({
        Bucket: bucket,
        Prefix: options.prefix,
        Delimiter: "/",
        MaxKeys: options.maxKeys ?? MAX_TREE_PAGE,
        ...(options.cursor ? { ContinuationToken: options.cursor } : {}),
      }),
    );
    return {
      objects: (response.Contents ?? [])
        .filter((entry) => entry.Key && entry.Key !== options.prefix)
        .map((entry) => ({
          key: entry.Key!,
          sizeBytes: Number(entry.Size ?? 0),
          lastModified: entry.LastModified?.toISOString() ?? null,
        })),
      directories: (response.CommonPrefixes ?? [])
        .map((entry) => entry.Prefix)
        .filter((prefix): prefix is string => typeof prefix === "string"),
      nextCursor: response.IsTruncated ? (response.NextContinuationToken ?? null) : null,
    };
  }

  async head(
    bucket: string,
    key: string,
  ): Promise<{ sizeBytes: number; lastModified: string | null } | null> {
    try {
      const response = await this.#client.send(
        new HeadObjectCommand({ Bucket: bucket, Key: key }),
      );
      return {
        sizeBytes: Number(response.ContentLength ?? 0),
        lastModified: response.LastModified?.toISOString() ?? null,
      };
    } catch (error) {
      if (isMissingObjectError(error)) return null;
      throw error;
    }
  }

  async getObject(bucket: string, key: string): Promise<Buffer> {
    const response = await this.#client.send(new GetObjectCommand({ Bucket: bucket, Key: key }));
    if (!response.Body) throw new Error(`empty body for ${bucket}/${key}`);
    return Buffer.from(await response.Body.transformToByteArray());
  }

  async signObjectRead(bucket: string, key: string, ttlSeconds: number): Promise<string> {
    return getSignedUrl(this.#client, new GetObjectCommand({ Bucket: bucket, Key: key }), {
      expiresIn: ttlSeconds,
    });
  }
}

function isMissingObjectError(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const name = (error as { name?: string }).name;
  const status = (error as { $metadata?: { httpStatusCode?: number } }).$metadata?.httpStatusCode;
  return name === "NotFound" || name === "NoSuchKey" || status === 404;
}

type ArtifactRow = {
  clip_id: string | null;
  kind: string;
  storage_state: string;
};

export class ReviewService {
  readonly #pool: SqlClient;
  readonly #storage: ReviewStorage | null;
  readonly #config: ConsoleConfig;

  constructor(pool: SqlClient, storage: ReviewStorage | null, config: ConsoleConfig) {
    this.#pool = pool;
    this.#storage = storage;
    this.#config = config;
  }

  get available(): boolean {
    return this.#storage !== null && this.#config.review !== null;
  }

  sources(): Array<Pick<ReviewSource, "id" | "bucket" | "prefixes" | "dbAuthority">> {
    return (this.#config.review?.sources ?? []).map((source) => ({
      id: source.id,
      bucket: source.bucket,
      prefixes: source.prefixes,
      dbAuthority: source.dbAuthority,
    }));
  }

  async tree(sourceId: string, prefix: string, cursor?: string): Promise<ReviewTreePage> {
    const { storage, source } = this.#require(sourceId);
    // The empty prefix is answered from configuration alone: the allowlisted
    // prefixes ARE the roots, and listing the bucket root would show keys the
    // allowlist exists to keep out of reach.
    if (prefix === "") {
      return {
        source: source.id,
        prefix: "",
        directories: [...source.prefixes],
        objects: [],
        nextCursor: null,
      };
    }
    assertSaneKey(prefix);
    if (!prefix.endsWith("/")) {
      throw badRequest("prefix_not_a_directory", "tree prefixes must end with '/'");
    }
    this.#requireInsideAllowlist(source, prefix);
    const page = await storage.list(source.bucket, { prefix, ...(cursor ? { cursor } : {}) });
    return {
      source: source.id,
      prefix,
      directories: page.directories,
      objects: page.objects,
      nextCursor: page.nextCursor,
    };
  }

  async objectInfo(sourceId: string, key: string): Promise<ReviewObjectInfo> {
    const { storage, source } = this.#require(sourceId);
    this.#requireObjectKey(source, key);
    const head = await storage.head(source.bucket, key);
    if (!head) throw notFound("object_not_found", "the object does not exist in the bucket");

    let dbRegistered: boolean | null = null;
    let artifact: ReviewObjectInfo["artifact"] = null;
    if (source.dbAuthority === "production") {
      const result = await this.#pool.query<ArtifactRow>(
        `SELECT clip_id, kind, storage_state
           FROM artifact_objects
          WHERE bucket_name = $1 AND object_key = $2
          LIMIT 1`,
        [source.bucket, key],
      );
      const row = result.rows[0];
      dbRegistered = row !== undefined;
      artifact = row
        ? { clipId: row.clip_id, kind: row.kind, storageState: row.storage_state }
        : null;
    }
    return {
      source: source.id,
      key,
      sizeBytes: head.sizeBytes,
      lastModified: head.lastModified,
      dbAuthority: source.dbAuthority,
      dbRegistered,
      artifact,
    };
  }

  async signVideo(sourceId: string, key: string): Promise<ReviewVideoGrant> {
    const { storage, source } = this.#require(sourceId);
    this.#requireObjectKey(source, key);
    if (!key.endsWith(".mp4")) {
      throw badRequest("not_a_video_object", "only .mp4 objects can be signed for playback");
    }
    const head = await storage.head(source.bucket, key);
    if (!head) throw notFound("object_not_found", "the object does not exist in the bucket");

    const ttlSeconds = this.#config.review!.ttlSeconds;
    const url = await storage.signObjectRead(source.bucket, key, ttlSeconds);
    return {
      url,
      source: source.id,
      objectKey: key,
      expiresInSeconds: ttlSeconds,
      expiresAt: new Date(Date.now() + ttlSeconds * 1_000).toISOString(),
      sizeBytes: head.sizeBytes,
    };
  }

  /** Presign a viz artifact (docs/104 §3): the self-contained viewer HTML, the
   * composite MP4, mesh GLB/PLY or fused-cloud NPZ of a visualization group.
   * The browser talks to R2 directly; this service never proxies the bytes.
   * `viz/` prefix + extension whitelist keep this from widening the older
   * `.mp4`-only video route. */
  async signVizAsset(sourceId: string, key: string): Promise<ReviewVideoGrant> {
    const { storage, source } = this.#require(sourceId);
    this.#requireObjectKey(source, key);
    if (!key.startsWith(VIZ_PREFIX)) {
      throw badRequest("not_a_viz_asset", "viz signing is limited to the viz/ prefix");
    }
    if (!VIZ_SIGNABLE_EXTENSIONS.some((extension) => key.endsWith(extension))) {
      throw badRequest(
        "unsupported_viz_asset",
        `viz signing is limited to ${VIZ_SIGNABLE_EXTENSIONS.join(", ")} objects`,
      );
    }
    const head = await storage.head(source.bucket, key);
    if (!head) throw notFound("object_not_found", "the object does not exist in the bucket");

    const ttlSeconds = this.#config.review!.ttlSeconds;
    const url = await storage.signObjectRead(source.bucket, key, ttlSeconds);
    return {
      url,
      source: source.id,
      objectKey: key,
      expiresInSeconds: ttlSeconds,
      expiresAt: new Date(Date.now() + ttlSeconds * 1_000).toISOString(),
      sizeBytes: head.sizeBytes,
    };
  }

  async frames(sourceId: string, key: string): Promise<ReviewFramesResult> {
    const { storage, source } = this.#require(sourceId);
    this.#requireObjectKey(source, key);

    const basename = key.slice(key.lastIndexOf("/") + 1);
    const caFrames = CONTENT_ADDRESSED_FRAMES.exec(basename);
    const caEvents = CONTENT_ADDRESSED_EVENTS.exec(basename);
    const embeddedSha256 = caFrames?.[1] ?? caEvents?.[1] ?? null;
    let schema: ReviewFramesSchema;
    if (basename === "p0.jsonl") schema = "dm1";
    else if (basename.endsWith("frames.jsonl.zst") || caFrames) schema = "raw-v1";
    else if (basename.endsWith("events.jsonl.zst") || caEvents) schema = "raw-events-v1";
    else {
      throw badRequest(
        "unsupported_review_object",
        "frame data is served for p0.jsonl, frames[-<sha256>].jsonl.zst and events[-<sha256>].jsonl.zst only",
      );
    }

    const head = await storage.head(source.bucket, key);
    if (!head) throw notFound("object_not_found", "the object does not exist in the bucket");
    if (head.sizeBytes > MAX_INLINE_COMPRESSED_BYTES) {
      throw badRequest("object_too_large", "the object is too large to serve inline");
    }

    const bytes = await storage.getObject(source.bucket, key);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    const directory = key.slice(0, key.lastIndexOf("/") + 1);

    // Integrity first (docs/33 §3.4): the RAW five-piece always carries a
    // sha256.json, so if it parses and does not contain the hash of the bytes
    // we just fetched, something IS wrong — refuse rather than render. A
    // content-addressed clip declares the hash in its own name instead.
    let sha256Verified = false;
    if (embeddedSha256 !== null) {
      if (sha256 !== embeddedSha256) {
        throw conflict(
          "integrity_mismatch",
          "the object bytes do not match the sha256 embedded in its content-addressed name",
        );
      }
      sha256Verified = true;
    } else if (schema !== "dm1") {
      const declared = await this.#tryReadJson(storage, source.bucket, `${directory}sha256.json`);
      if (declared !== null) {
        if (!JSON.stringify(declared).toLowerCase().includes(sha256)) {
          throw conflict(
            "integrity_mismatch",
            "the object bytes do not match the sha256.json declaration next to them",
          );
        }
        sha256Verified = true;
      }
    }

    const text =
      schema === "dm1"
        ? bytes.toString("utf8")
        : zstdDecompressSync(bytes, { maxOutputLength: MAX_INLINE_DECOMPRESSED_BYTES }).toString(
            "utf8",
          );
    let rows: unknown[];
    try {
      rows = text
        .split("\n")
        .filter((line) => line.trim().length > 0)
        .map((line) => JSON.parse(line) as unknown);
    } catch {
      throw conflict("malformed_jsonl", "the object is not parseable line-delimited JSON");
    }

    const meta =
      embeddedSha256 === null
        ? await this.#tryReadJson(storage, source.bucket, `${directory}meta.json`)
        : await this.#tryReadContentAddressedMeta(storage, source.bucket, directory);
    let frameCountVerified = false;
    if (schema !== "raw-events-v1" && meta !== null) {
      const declared = declaredFrameCount(meta);
      if (typeof declared === "number") {
        if (rows.length !== declared) {
          throw conflict(
            "frame_count_mismatch",
            `the object holds ${rows.length} rows but meta.json declares ${declared} frames`,
          );
        }
        frameCountVerified = true;
      }
    }

    if (schema === "raw-v1") rows = rows.map(normalizeRawFrameRow);

    return {
      source: source.id,
      key,
      schema,
      rows,
      fps: declaredFps(meta),
      meta,
      provenance: {
        objectKey: key,
        sizeBytes: head.sizeBytes,
        sha256,
        sha256Verified,
        frameCountVerified,
      },
    };
  }

  async meta(sourceId: string, key: string): Promise<ReviewMetaResult> {
    const { storage, source } = this.#require(sourceId);
    this.#requireObjectKey(source, key);
    if (!key.endsWith(".json")) {
      throw badRequest("not_a_json_object", "only .json objects are served by this endpoint");
    }
    const head = await storage.head(source.bucket, key);
    if (!head) throw notFound("object_not_found", "the object does not exist in the bucket");
    if (head.sizeBytes > MAX_META_BYTES) {
      throw badRequest("object_too_large", "the object is too large to serve inline");
    }
    const bytes = await storage.getObject(source.bucket, key);
    let meta: unknown;
    try {
      meta = JSON.parse(bytes.toString("utf8"));
    } catch {
      throw conflict("malformed_json", "the object is not parseable JSON");
    }
    return { source: source.id, key, sizeBytes: head.sizeBytes, meta };
  }

  /**
   * Raw bytes for the depth surface (docs/77 §5.5): `raw/<present_seq>.f32`
   * frames and the `depth-index.jsonl` beside them.
   *
   * Bytes are proxied rather than presigned on purpose. The viewer decodes the
   * frame with `fetch` + `arrayBuffer`, which is a CORS request — a presigned
   * URL would work only for buckets carrying a CORS policy naming the console
   * origin, so every future review bucket would need one before its depth data
   * rendered. Proxying also keeps the audit trail on the bytes themselves
   * rather than on the grant that may or may not have been used.
   */
  async depthObject(
    sourceId: string,
    key: string,
  ): Promise<{ source: string; key: string; contentType: string; bytes: Buffer }> {
    const { storage, source } = this.#require(sourceId);
    this.#requireObjectKey(source, key);
    const basename = key.slice(key.lastIndexOf("/") + 1);
    // .exr is the current format (docs/85); .f32 is the v1 captures already in
    // the bucket. Both are served — the index's storage.encoding is what tells
    // the viewer which decoder to use, and a bucket can hold both at once.
    const isFrame = key.endsWith(".exr") || key.endsWith(".f32");
    if (!isFrame && basename !== "depth-index.jsonl") {
      throw badRequest(
        "unsupported_depth_object",
        "this endpoint serves .exr/.f32 depth frames and depth-index.jsonl only",
      );
    }
    const head = await storage.head(source.bucket, key);
    if (!head) throw notFound("object_not_found", "the object does not exist in the bucket");
    if (head.sizeBytes > MAX_DEPTH_OBJECT_BYTES) {
      throw badRequest("object_too_large", "the object is too large to serve inline");
    }
    return {
      source: source.id,
      key,
      // 720p f32 is 3.5 MiB, the fp16 EXR about 100 KiB; declaring either as a
      // stream keeps intermediaries from trying to sniff or transcode it.
      contentType: isFrame ? "application/octet-stream" : "application/x-ndjson",
      bytes: await storage.getObject(source.bucket, key),
    };
  }

  #require(sourceId: string): { storage: ReviewStorage; source: ReviewSource } {
    const review = this.#config.review;
    if (!this.#storage || !review) {
      throw conflict(
        "review_not_configured",
        "this deployment has no read-only review credentials, so the review surface is unavailable",
      );
    }
    const source = review.sources.find((entry) => entry.id === sourceId);
    if (!source) throw notFound("review_source_not_found", "no such review source is configured");
    return { storage: this.#storage, source };
  }

  #requireInsideAllowlist(source: ReviewSource, path: string): void {
    if (!source.prefixes.some((prefix) => path.startsWith(prefix))) {
      throw badRequest(
        "key_outside_allowlist",
        "the requested path is outside this source's allowlisted prefixes",
      );
    }
  }

  #requireObjectKey(source: ReviewSource, key: string): void {
    assertSaneKey(key);
    if (key.endsWith("/")) {
      throw badRequest("not_an_object_key", "object keys must not end with '/'");
    }
    this.#requireInsideAllowlist(source, key);
  }

  /** A content-addressed clip has no fixed `meta.json`; the meta sits under
   * `meta-<sha256>.json`, findable only by listing. The `meta-` prefix keeps
   * the listing to that one artifact instead of the whole directory. */
  async #tryReadContentAddressedMeta(
    storage: ReviewStorage,
    bucket: string,
    directory: string,
  ): Promise<unknown | null> {
    const page = await storage.list(bucket, { prefix: `${directory}meta-`, maxKeys: 16 });
    const key = page.objects.find((object) =>
      CONTENT_ADDRESSED_META.test(object.key.slice(object.key.lastIndexOf("/") + 1)),
    )?.key;
    return key === undefined ? null : this.#tryReadJson(storage, bucket, key);
  }

  async #tryReadJson(storage: ReviewStorage, bucket: string, key: string): Promise<unknown | null> {
    const head = await storage.head(bucket, key);
    if (!head || head.sizeBytes > MAX_META_BYTES) return null;
    try {
      return JSON.parse((await storage.getObject(bucket, key)).toString("utf8")) as unknown;
    } catch {
      return null;
    }
  }
}

// Belt and braces under the allowlist check: even a key that somehow matched a
// prefix must still be a plain relative object path.
function assertSaneKey(value: string): void {
  const hasControlChars = [...value].some((ch) => {
    const code = ch.charCodeAt(0);
    return code < 0x20 || code === 0x7f;
  });
  if (
    value.startsWith("/") ||
    value.includes("\\") ||
    value.includes("..") ||
    value.includes("//") ||
    hasControlChars
  ) {
    throw badRequest("invalid_object_key", "the object key is not a plain relative path");
  }
}

// Unit conversion and a placeholder only. Every projected field — keys,
// pointer deltas, bins, events, buttons, actions, duplicate — passes through
// untouched: this adapter must never recompute what the client projected
// (docs/33 §3.4, debate §8.4).
function normalizeRawFrameRow(row: unknown): unknown {
  if (typeof row !== "object" || row === null) return row;
  const record = row as Record<string, unknown>;
  const out: Record<string, unknown> = { gframe: null, ...record };
  if (typeof record.video_t_us === "number" && record.video_t_ms === undefined) {
    out.video_t_ms = Math.round(record.video_t_us / 1_000);
  }
  return out;
}

function declaredFrameCount(meta: unknown): number | null {
  if (typeof meta !== "object" || meta === null) return null;
  const record = meta as Record<string, unknown>;
  if (typeof record.n_frames === "number") return record.n_frames;
  const video = record.video;
  if (typeof video === "object" && video !== null) {
    const frames = (video as Record<string, unknown>).frames;
    if (typeof frames === "number") return frames;
  }
  return null;
}

function declaredFps(meta: unknown): number | null {
  if (typeof meta !== "object" || meta === null) return null;
  const record = meta as Record<string, unknown>;
  const candidates: unknown[] = [record.video, record.target, record.probe, record];
  for (const candidate of candidates) {
    if (typeof candidate !== "object" || candidate === null) continue;
    const nested = candidate as Record<string, unknown>;
    if (typeof nested.fps_num === "number" && typeof nested.fps_den === "number" && nested.fps_den > 0) {
      return nested.fps_num / nested.fps_den;
    }
    if (typeof nested.fps === "number" && nested.fps > 0) return nested.fps;
  }
  return null;
}
