import { createHash } from "node:crypto";
import { zstdCompressSync } from "node:zlib";

import { describe, expect, it } from "vitest";

import { loadConfig } from "./config.js";
import { ReviewService, type ReviewStorage, type ReviewTreeObject } from "./review.js";
import type { SqlClient } from "./sql.js";

const sources = JSON.stringify([
  {
    id: "raw-prod",
    bucket: "diogenes-model-training",
    prefixes: ["raw/", "raw-staging/", "training/"],
    dbAuthority: "production",
  },
  {
    id: "training-dev",
    bucket: "diogenes-training-dev",
    prefixes: ["raw/", "method-validation/"],
  },
  {
    id: "depth-delivery",
    bucket: "diogenes-depth-delivery",
    prefixes: ["verified/", "viz/"],
  },
]);

const baseEnvironment = {
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "p".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
};

const reviewEnvironment = {
  ...baseEnvironment,
  CONSOLE_REVIEW_R2_ACCOUNT_ID: "account",
  CONSOLE_REVIEW_R2_ACCESS_KEY_ID: "review-read-key",
  CONSOLE_REVIEW_R2_SECRET_ACCESS_KEY: "review-read-secret",
  CONSOLE_REVIEW_SOURCES: sources,
};

class FakeStorage implements ReviewStorage {
  readonly objects = new Map<string, Buffer>();
  readonly signed: { bucket: string; key: string; ttlSeconds: number }[] = [];
  listResult: { objects: ReviewTreeObject[]; directories: string[]; nextCursor: string | null } = {
    objects: [],
    directories: [],
    nextCursor: null,
  };
  listCalls: { bucket: string; prefix: string; cursor?: string }[] = [];

  put(bucket: string, key: string, bytes: Buffer | string): void {
    this.objects.set(`${bucket}/${key}`, Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes));
  }

  async list(
    bucket: string,
    options: { prefix: string; cursor?: string },
  ): Promise<{ objects: ReviewTreeObject[]; directories: string[]; nextCursor: string | null }> {
    this.listCalls.push({ bucket, prefix: options.prefix, ...(options.cursor ? { cursor: options.cursor } : {}) });
    return this.listResult;
  }

  async head(
    bucket: string,
    key: string,
  ): Promise<{ sizeBytes: number; lastModified: string | null } | null> {
    const bytes = this.objects.get(`${bucket}/${key}`);
    return bytes ? { sizeBytes: bytes.length, lastModified: null } : null;
  }

  async getObject(bucket: string, key: string): Promise<Buffer> {
    const bytes = this.objects.get(`${bucket}/${key}`);
    if (!bytes) throw new Error(`missing object ${bucket}/${key}`);
    return bytes;
  }

  async signObjectRead(bucket: string, key: string, ttlSeconds: number): Promise<string> {
    this.signed.push({ bucket, key, ttlSeconds });
    return `https://example.r2.cloudflarestorage.com/${bucket}/${key}?signature=test`;
  }
}

class StubSql implements SqlClient {
  rows: unknown[] = [];
  queries: { text: string; params?: unknown[] }[] = [];

  async query<T>(text: string, params?: unknown[]): Promise<{ rows: T[] }> {
    this.queries.push({ text, ...(params ? { params } : {}) });
    return { rows: this.rows as T[] };
  }
}

function build(overrides: Partial<Record<string, string>> = {}) {
  const storage = new FakeStorage();
  const sql = new StubSql();
  const service = new ReviewService(
    sql,
    storage,
    loadConfig({ ...reviewEnvironment, ...overrides }),
  );
  return { storage, sql, service };
}

const rawBucket = "diogenes-model-training";
const segmentDir = "raw/v1/non-trainable/manual/HOST/app/non-trainable-sessions/s1/runs/r0000/segments/00000/";
// dataminer-api clip layout: content-addressed artifact names (docs/33 §12).
const clipDir = "raw/v1/non-trainable/dataminer-api-production/sessions/s1/runs/r1/clips/c1/";

describe("review configuration", () => {
  it("refuses a partial CONSOLE_REVIEW_* configuration", () => {
    const partial = { ...reviewEnvironment } as Record<string, string>;
    delete partial.CONSOLE_REVIEW_SOURCES;
    expect(() => loadConfig(partial)).toThrow(/every CONSOLE_REVIEW_\* value or none/);
  });

  it("refuses sources that are not valid JSON", () => {
    expect(() =>
      loadConfig({ ...reviewEnvironment, CONSOLE_REVIEW_SOURCES: "not-json" }),
    ).toThrow(/valid JSON/);
  });

  it("refuses a prefix that does not end with a slash", () => {
    expect(() =>
      loadConfig({
        ...reviewEnvironment,
        CONSOLE_REVIEW_SOURCES: JSON.stringify([
          { id: "bad", bucket: "b", prefixes: ["raw"] },
        ]),
      }),
    ).toThrow(/end with/);
  });

  it("refuses duplicate source ids", () => {
    expect(() =>
      loadConfig({
        ...reviewEnvironment,
        CONSOLE_REVIEW_SOURCES: JSON.stringify([
          { id: "dup", bucket: "a", prefixes: ["raw/"] },
          { id: "dup", bucket: "b", prefixes: ["raw/"] },
        ]),
      }),
    ).toThrow(/unique/);
  });

  it("reports itself unavailable without credentials", () => {
    const service = new ReviewService(new StubSql(), null, loadConfig(baseEnvironment));
    expect(service.available).toBe(false);
    void expect(service.tree("raw-prod", "")).rejects.toMatchObject({
      code: "review_not_configured",
    });
  });
});

describe("review tree", () => {
  it("answers the empty prefix from configuration alone", async () => {
    const { storage, service } = build();
    const page = await service.tree("raw-prod", "");
    expect(page.directories).toEqual(["raw/", "raw-staging/", "training/"]);
    expect(page.objects).toEqual([]);
    expect(storage.listCalls).toHaveLength(0);
  });

  it("lists inside an allowlisted prefix and maps the page through", async () => {
    const { storage, service } = build();
    storage.listResult = {
      objects: [{ key: "raw/v1/x/video.mp4", sizeBytes: 10, lastModified: null }],
      directories: ["raw/v1/x/segments/"],
      nextCursor: "token",
    };
    const page = await service.tree("raw-prod", "raw/v1/x/", "cursor-1");
    expect(page.directories).toEqual(["raw/v1/x/segments/"]);
    expect(page.objects[0]?.key).toBe("raw/v1/x/video.mp4");
    expect(page.nextCursor).toBe("token");
    expect(storage.listCalls[0]).toMatchObject({
      bucket: rawBucket,
      prefix: "raw/v1/x/",
      cursor: "cursor-1",
    });
  });

  it("refuses a prefix outside the allowlist", async () => {
    const { service } = build();
    await expect(service.tree("raw-prod", "secret/")).rejects.toMatchObject({
      code: "key_outside_allowlist",
    });
  });

  it("refuses traversal in a prefix", async () => {
    const { service } = build();
    await expect(service.tree("raw-prod", "raw/../secret/")).rejects.toMatchObject({
      code: "invalid_object_key",
    });
  });

  it("refuses an unknown source", async () => {
    const { service } = build();
    await expect(service.tree("nope", "")).rejects.toMatchObject({
      code: "review_source_not_found",
    });
  });
});

describe("review object info", () => {
  it("cross-checks artifact_objects when the source is the production authority", async () => {
    const { storage, sql, service } = build();
    storage.put(rawBucket, "raw/v1/x/video.mp4", "bytes");
    sql.rows = [{ clip_id: "clip-1", kind: "video", storage_state: "stored" }];
    const info = await service.objectInfo("raw-prod", "raw/v1/x/video.mp4");
    expect(info.dbAuthority).toBe("production");
    expect(info.dbRegistered).toBe(true);
    expect(info.artifact).toEqual({ clipId: "clip-1", kind: "video", storageState: "stored" });
  });

  it("reports out-of-band objects as unregistered, not as an error", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, "raw/v1/x/video.mp4", "bytes");
    const info = await service.objectInfo("raw-prod", "raw/v1/x/video.mp4");
    expect(info.dbRegistered).toBe(false);
    expect(info.artifact).toBeNull();
  });

  it("skips the database entirely for dbAuthority=none sources", async () => {
    const { storage, sql, service } = build();
    storage.put("diogenes-training-dev", "raw/v1/e2e/video.mp4", "bytes");
    const info = await service.objectInfo("training-dev", "raw/v1/e2e/video.mp4");
    expect(info.dbAuthority).toBe("none");
    expect(info.dbRegistered).toBeNull();
    expect(sql.queries).toHaveLength(0);
  });
});

describe("review video signing", () => {
  it("signs an allowlisted .mp4 for the configured ttl", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${segmentDir}video.mp4`, "mp4-bytes");
    const grant = await service.signVideo("raw-prod", `${segmentDir}video.mp4`);
    expect(grant.expiresInSeconds).toBe(300);
    expect(grant.sizeBytes).toBe(9);
    expect(storage.signed[0]).toEqual({
      bucket: rawBucket,
      key: `${segmentDir}video.mp4`,
      ttlSeconds: 300,
    });
  });

  it("refuses to sign anything that is not an .mp4", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${segmentDir}frames.jsonl.zst`, "z");
    await expect(
      service.signVideo("raw-prod", `${segmentDir}frames.jsonl.zst`),
    ).rejects.toMatchObject({ code: "not_a_video_object" });
  });

  it("refuses a key outside the allowlist", async () => {
    const { service } = build();
    await expect(service.signVideo("raw-prod", "private/video.mp4")).rejects.toMatchObject({
      code: "key_outside_allowlist",
    });
  });

  it("refuses a missing object", async () => {
    const { service } = build();
    await expect(service.signVideo("raw-prod", `${segmentDir}video.mp4`)).rejects.toMatchObject({
      code: "object_not_found",
    });
  });
});

describe("review viz signing", () => {
  const vizBucket = "diogenes-depth-delivery";
  const vizGroup = "viz/2026_08_06_FO4Commonwealth_120s/";

  it("signs a viewer.html under viz/ for the configured ttl", async () => {
    const { storage, service } = build();
    storage.put(vizBucket, `${vizGroup}viewer.html`, "<!doctype html>");
    const grant = await service.signVizAsset("depth-delivery", `${vizGroup}viewer.html`);
    expect(grant.expiresInSeconds).toBe(300);
    expect(grant.sizeBytes).toBe(15);
    expect(storage.signed[0]).toEqual({
      bucket: vizBucket,
      key: `${vizGroup}viewer.html`,
      ttlSeconds: 300,
    });
  });

  it("signs mesh and cloud artifacts (.glb/.ply/.npz)", async () => {
    const { storage, service } = build();
    for (const name of ["mesh_01.glb", "mesh_01.ply", "cloud_01.npz"]) {
      storage.put(vizBucket, `${vizGroup}${name}`, "bytes");
      const grant = await service.signVizAsset("depth-delivery", `${vizGroup}${name}`);
      expect(grant.objectKey).toBe(`${vizGroup}${name}`);
    }
  });

  it("refuses extensions outside the viz whitelist (manifest stays meta-only)", async () => {
    const { storage, service } = build();
    storage.put(vizBucket, `${vizGroup}viz-manifest.json`, "{}");
    await expect(
      service.signVizAsset("depth-delivery", `${vizGroup}viz-manifest.json`),
    ).rejects.toMatchObject({ code: "unsupported_viz_asset" });
  });

  it("refuses keys outside viz/ even when inside the source allowlist", async () => {
    const { storage, service } = build();
    storage.put(vizBucket, "verified/pkg/video.mp4", "mp4");
    await expect(
      service.signVizAsset("depth-delivery", "verified/pkg/video.mp4"),
    ).rejects.toMatchObject({ code: "not_a_viz_asset" });
  });

  it("refuses a key outside the allowlist", async () => {
    const { service } = build();
    await expect(
      service.signVizAsset("depth-delivery", "private/viewer.html"),
    ).rejects.toMatchObject({ code: "key_outside_allowlist" });
  });

  it("refuses a missing object", async () => {
    const { service } = build();
    await expect(
      service.signVizAsset("depth-delivery", `${vizGroup}viewer.html`),
    ).rejects.toMatchObject({ code: "object_not_found" });
  });
});

describe("review depth objects", () => {
  const depthDir = "raw/depth-formal-20260804/";

  it("serves an .f32 frame as opaque bytes", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${depthDir}raw/000000000002.f32`, " ");
    const result = await service.depthObject("raw-prod", `${depthDir}raw/000000000002.f32`);
    expect(result.contentType).toBe("application/octet-stream");
    expect(result.bytes.byteLength).toBe(4);
  });

  it("serves the index as ndjson", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${depthDir}depth-index.jsonl`, '{"present_seq":2}\n');
    const result = await service.depthObject("raw-prod", `${depthDir}depth-index.jsonl`);
    expect(result.contentType).toBe("application/x-ndjson");
  });

  // The route must not become a way to pull arbitrary objects out of the
  // bucket without the integrity and schema checks the other routes apply.
  it("refuses anything that is neither a frame nor the index", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${depthDir}meta.json`, "{}");
    await expect(service.depthObject("raw-prod", `${depthDir}meta.json`)).rejects.toMatchObject({
      code: "unsupported_depth_object",
    });
  });

  it("refuses a key outside the allowlist", async () => {
    const { service } = build();
    await expect(service.depthObject("raw-prod", "private/x.f32")).rejects.toMatchObject({
      code: "key_outside_allowlist",
    });
  });

  it("refuses a missing object", async () => {
    const { service } = build();
    await expect(
      service.depthObject("raw-prod", `${depthDir}raw/000000000002.f32`),
    ).rejects.toMatchObject({ code: "object_not_found" });
  });
});

const rawRows = [
  {
    frame: 0,
    session_frame: 7205,
    run_id: "r0001",
    video_t_us: 0,
    duplicate: false,
    keys: [87],
    pointerDx: 120,
    pointerDy: -14,
    pointerBins: [8, -2],
    events: [{ type: "mouse_down", vk: 1 }],
    src: "rec",
  },
  {
    frame: 1,
    session_frame: 7206,
    run_id: "r0001",
    video_t_us: 33333,
    duplicate: true,
    keys: [],
    pointerDx: 0,
    pointerDy: 0,
    pointerBins: [0, 0],
    events: [],
    src: "rec",
  },
];

function jsonl(rows: unknown[]): string {
  return rows.map((row) => JSON.stringify(row)).join("\n") + "\n";
}

describe("review frames", () => {
  it("serves a dm-1 p0.jsonl untouched", async () => {
    const { storage, service } = build();
    const rows = [
      { frame: 0, gframe: 720, video_t_ms: 24000, keys: [], pointerDx: 0, pointerDy: 0, pointerBins: [0, 0], events: [], src: "empty" },
    ];
    storage.put(rawBucket, "training/demo/s1/c0006/p0.jsonl", jsonl(rows));
    storage.put(
      rawBucket,
      "training/demo/s1/c0006/meta.json",
      JSON.stringify({ video: { fps: 30, frames: 1 } }),
    );
    const result = await service.frames("raw-prod", "training/demo/s1/c0006/p0.jsonl");
    expect(result.schema).toBe("dm1");
    expect(result.rows).toEqual(rows);
    expect(result.fps).toBe(30);
    expect(result.provenance.frameCountVerified).toBe(true);
    // dm-1 chunks carry no sha256.json; absence is reported, not invented.
    expect(result.provenance.sha256Verified).toBe(false);
  });

  it("decompresses RAW frames, converts µs to ms, and touches nothing else", async () => {
    const { storage, service } = build();
    const compressed = zstdCompressSync(Buffer.from(jsonl(rawRows)));
    const sha256 = createHash("sha256").update(compressed).digest("hex");
    storage.put(rawBucket, `${segmentDir}frames.jsonl.zst`, compressed);
    storage.put(rawBucket, `${segmentDir}sha256.json`, JSON.stringify({ frames: sha256 }));
    storage.put(
      rawBucket,
      `${segmentDir}meta.json`,
      JSON.stringify({ n_frames: 2, target: { fps_num: 30, fps_den: 1 } }),
    );

    const result = await service.frames("raw-prod", `${segmentDir}frames.jsonl.zst`);

    expect(result.schema).toBe("raw-v1");
    expect(result.fps).toBe(30);
    expect(result.provenance.sha256Verified).toBe(true);
    expect(result.provenance.frameCountVerified).toBe(true);
    const first = result.rows[0] as Record<string, unknown>;
    expect(first.gframe).toBeNull();
    expect(first.video_t_ms).toBe(0);
    const second = result.rows[1] as Record<string, unknown>;
    expect(second.video_t_ms).toBe(33);
    // The projected facts pass through byte-identical (docs/33 §8.4).
    expect(first.keys).toEqual([87]);
    expect(first.pointerDx).toBe(120);
    expect(first.pointerBins).toEqual([8, -2]);
    expect(first.events).toEqual([{ type: "mouse_down", vk: 1 }]);
    expect(second.duplicate).toBe(true);
  });

  it("refuses bytes that contradict the sha256.json next to them", async () => {
    const { storage, service } = build();
    const compressed = zstdCompressSync(Buffer.from(jsonl(rawRows)));
    storage.put(rawBucket, `${segmentDir}frames.jsonl.zst`, compressed);
    storage.put(rawBucket, `${segmentDir}sha256.json`, JSON.stringify({ frames: "0".repeat(64) }));
    await expect(
      service.frames("raw-prod", `${segmentDir}frames.jsonl.zst`),
    ).rejects.toMatchObject({ code: "integrity_mismatch" });
  });

  it("refuses a row count that contradicts meta.json", async () => {
    const { storage, service } = build();
    const compressed = zstdCompressSync(Buffer.from(jsonl(rawRows)));
    const sha256 = createHash("sha256").update(compressed).digest("hex");
    storage.put(rawBucket, `${segmentDir}frames.jsonl.zst`, compressed);
    storage.put(rawBucket, `${segmentDir}sha256.json`, JSON.stringify({ frames: sha256 }));
    storage.put(rawBucket, `${segmentDir}meta.json`, JSON.stringify({ n_frames: 300 }));
    await expect(
      service.frames("raw-prod", `${segmentDir}frames.jsonl.zst`),
    ).rejects.toMatchObject({ code: "frame_count_mismatch" });
  });

  it("serves RAW events without a frame-count check", async () => {
    const { storage, service } = build();
    const events = [{ seq: 1, qpc: 918273645512, type: "key_down", vk: 87 }];
    const compressed = zstdCompressSync(Buffer.from(jsonl(events)));
    const sha256 = createHash("sha256").update(compressed).digest("hex");
    storage.put(rawBucket, `${segmentDir}events.jsonl.zst`, compressed);
    storage.put(rawBucket, `${segmentDir}sha256.json`, JSON.stringify({ events: sha256 }));
    storage.put(rawBucket, `${segmentDir}meta.json`, JSON.stringify({ n_frames: 300 }));
    const result = await service.frames("raw-prod", `${segmentDir}events.jsonl.zst`);
    expect(result.schema).toBe("raw-events-v1");
    expect(result.rows).toEqual(events);
  });

  it("serves a content-addressed dataminer-api clip, verifying bytes against the name hash", async () => {
    const { storage, service } = build();
    const compressed = zstdCompressSync(Buffer.from(jsonl(rawRows)));
    const sha256 = createHash("sha256").update(compressed).digest("hex");
    const framesKey = `${clipDir}frames-${sha256}.jsonl.zst`;
    storage.put(rawBucket, framesKey, compressed);
    const metaBytes = JSON.stringify({ n_frames: 2, fps_num: 30, fps_den: 1 });
    const metaKey = `${clipDir}meta-${createHash("sha256").update(metaBytes).digest("hex")}.json`;
    storage.put(rawBucket, metaKey, metaBytes);
    storage.listResult = {
      objects: [{ key: metaKey, sizeBytes: metaBytes.length, lastModified: null }],
      directories: [],
      nextCursor: null,
    };

    const result = await service.frames("raw-prod", framesKey);

    expect(result.schema).toBe("raw-v1");
    expect(result.fps).toBe(30);
    expect(result.provenance.sha256Verified).toBe(true);
    expect(result.provenance.frameCountVerified).toBe(true);
    expect((result.rows[0] as Record<string, unknown>).video_t_ms).toBe(0);
    // The meta sibling is discovered through the narrow `meta-` prefix, not a
    // directory-wide list.
    expect(storage.listCalls[0]).toMatchObject({ bucket: rawBucket, prefix: `${clipDir}meta-` });
  });

  it("refuses bytes that contradict the content-addressed name", async () => {
    const { storage, service } = build();
    const compressed = zstdCompressSync(Buffer.from(jsonl(rawRows)));
    const framesKey = `${clipDir}frames-${"0".repeat(64)}.jsonl.zst`;
    storage.put(rawBucket, framesKey, compressed);
    await expect(service.frames("raw-prod", framesKey)).rejects.toMatchObject({
      code: "integrity_mismatch",
    });
  });

  it("serves content-addressed events without a frame-count check", async () => {
    const { storage, service } = build();
    const events = [{ seq: 1, qpc: 918273645512, type: "key_down", vk: 87 }];
    const compressed = zstdCompressSync(Buffer.from(jsonl(events)));
    const sha256 = createHash("sha256").update(compressed).digest("hex");
    const key = `${clipDir}events-${sha256}.jsonl.zst`;
    storage.put(rawBucket, key, compressed);
    const result = await service.frames("raw-prod", key);
    expect(result.schema).toBe("raw-events-v1");
    expect(result.provenance.sha256Verified).toBe(true);
    expect(result.rows).toEqual(events);
  });

  it("refuses unsupported objects", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${segmentDir}journal.sqlite3`, "db");
    await expect(
      service.frames("raw-prod", `${segmentDir}journal.sqlite3`),
    ).rejects.toMatchObject({ code: "unsupported_review_object" });
  });

  it("refuses an object larger than the inline cap", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${segmentDir}frames.jsonl.zst`, Buffer.alloc(64 * 1024 * 1024 + 1));
    await expect(
      service.frames("raw-prod", `${segmentDir}frames.jsonl.zst`),
    ).rejects.toMatchObject({ code: "object_too_large" });
  });
});

describe("review meta", () => {
  it("serves a small json object", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${segmentDir}meta.json`, JSON.stringify({ n_frames: 300 }));
    const result = await service.meta("raw-prod", `${segmentDir}meta.json`);
    expect(result.meta).toEqual({ n_frames: 300 });
  });

  it("refuses non-json keys", async () => {
    const { storage, service } = build();
    storage.put(rawBucket, `${segmentDir}video.mp4`, "bytes");
    await expect(service.meta("raw-prod", `${segmentDir}video.mp4`)).rejects.toMatchObject({
      code: "not_a_json_object",
    });
  });
});
