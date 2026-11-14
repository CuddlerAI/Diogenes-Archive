import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { loadConfig, type ConsoleConfig } from "./config.js";
import { PreviewService, type PreviewSigner } from "./preview.js";
import type { SqlClient } from "./sql.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const prefix = "raw-staging/v1/non-trainable/dataminer-api-staging";
const bucket = "diogenes-model-training";

const previewConfig: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "p".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
  CONSOLE_R2_ACCOUNT_ID: "account",
  CONSOLE_R2_BUCKET: bucket,
  CONSOLE_R2_ACCESS_KEY_ID: "read-only-key",
  CONSOLE_R2_SECRET_ACCESS_KEY: "read-only-secret",
  CONSOLE_R2_PREFIX: prefix,
});

class RecordingSigner implements PreviewSigner {
  readonly signed: { objectKey: string; ttlSeconds: number }[] = [];

  async signObjectRead(objectKey: string, ttlSeconds: number): Promise<string> {
    this.signed.push({ objectKey, ttlSeconds });
    return `https://example.r2.cloudflarestorage.com/${objectKey}?signature=test`;
  }
}

let db: PGlite;
let signer: RecordingSigner;
let service: PreviewService;
const ids = {
  device: randomUUID(),
  deviceProfile: randomUUID(),
  recordingProfile: randomUUID(),
  mapping: randomUUID(),
  session: randomUUID(),
  run: randomUUID(),
};

async function insertClip(options: {
  storageState: string;
  retentionState: string;
  objectKey?: string;
  bucketName?: string;
}): Promise<string> {
  const clipId = randomUUID();
  const segment = Math.floor(Math.random() * 1_000_000);
  await db.query(
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
                       manifest_sha256, ingest_state, stored_at, retention_state, purge_requested_at,
                       purged_at, retention_deadline)
     VALUES ($1, $2, $3, 0, $4, 0, 120, 1, 2, 0, '{}'::jsonb, '{}'::jsonb, $5, 'stored',
             transaction_timestamp(), $6::retention_state,
             CASE WHEN $6::retention_state <> 'retained' THEN transaction_timestamp() END,
             CASE WHEN $6::retention_state = 'purged' THEN transaction_timestamp() END,
             transaction_timestamp() + interval '30 days')`,
    [clipId, ids.session, ids.run, segment, `sha256:${"e".repeat(64)}`, options.retentionState],
  );
  await db.query(
    `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                  expected_size_bytes, storage_state, stored_at)
     VALUES ($1, $2, 'video', $3, $4, $5, 4096, $6::storage_state,
             CASE WHEN $6::storage_state = 'stored' THEN transaction_timestamp() END)`,
    [
      randomUUID(),
      clipId,
      options.bucketName ?? bucket,
      options.objectKey ?? `${prefix}/${clipId}/video.mp4`,
      `sha256:${"f".repeat(64)}`,
      options.storageState,
    ],
  );
  return clipId;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  signer = new RecordingSigner();
  service = new PreviewService(db as unknown as SqlClient, signer, previewConfig);

  const digest = `sha256:${"a".repeat(64)}`;
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
     VALUES ('yuanshen', 'Genshin Impact', 'game', 'third_person', 'rendered', true)`,
  );
  await db.query(
    `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash, snapshot)
     VALUES ($1, 'device_profile', 'dm-device-1', '1.0.0', NULL, $4, '{}'::jsonb),
            ($2, 'recording_profile', 'dm-recording-1', '1.0.0', 'yuanshen', $4, '{}'::jsonb),
            ($3, 'game_mapping', 'dm-mapping-1', '1.0.0', 'yuanshen', $4, '{}'::jsonb)`,
    [ids.deviceProfile, ids.recordingProfile, ids.mapping, digest],
  );
  await db.query(
    `INSERT INTO devices(id, client_device_id, certified_profile_id, credential_fingerprint,
                         status, windows_build, gpu_pci_id, gpu_driver, display_snapshot,
                         input_devices_snapshot, client_version)
     VALUES ($1, 'dev-preview', $2, $3, 'active', '10.0.26100', 'PCI', '566.36',
             '{}'::jsonb, '[]'::jsonb, '0.1.0')`,
    [ids.device, "dm-device-1", "b".repeat(64)],
  );
  await db.query(
    `INSERT INTO sessions(id, client_session_id, device_id, game_id, game_version,
                          game_executable_sha256, device_catalog_version_id, recording_catalog_version_id,
                          mapping_catalog_version_id, schema_version,
                          manifest_json, manifest_sha256, status, capture_engine, native_width,
                          native_height, fps_num, fps_den, client_version, started_at)
     VALUES ($1, 'client-preview', $2, 'yuanshen', '6.7', $3, $4, $5, $6,
             'dm-local-dev-session-1', '{}'::jsonb, $3, 'completed', 'native_wgc_window_v1',
             1920, 1080, 30, 1, '0.1.0', transaction_timestamp())`,
    [
      ids.session,
      ids.device,
      `sha256:${"c".repeat(64)}`,
      ids.deviceProfile,
      ids.recordingProfile,
      ids.mapping,
    ],
  );
  await db.query(
    `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, capture_engine,
                              encoder_backend, width, height, fps_num, fps_den, session_frame_start)
     VALUES ($1, $2, 'run-preview', 0, 'native_wgc_window_v1', 'nvenc_h264', 1920, 1080, 30, 1, 0)`,
    [ids.run, ids.session],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("clip preview", () => {
  it("signs one stored video object for a short window", async () => {
    const clipId = await insertClip({ storageState: "stored", retentionState: "retained" });

    const grant = await service.signClipVideo(clipId);

    expect(grant.expiresInSeconds).toBe(300);
    expect(grant.objectKey.startsWith(`${prefix}/`)).toBe(true);
    expect(grant.sizeBytes).toBe(4096);
    expect(signer.signed.at(-1)).toEqual({ objectKey: grant.objectKey, ttlSeconds: 300 });
  });

  it("signs only the video object, never the other four artifacts", async () => {
    const clipId = await insertClip({ storageState: "stored", retentionState: "retained" });
    for (const kind of ["frames", "events", "meta", "manifest"]) {
      await db.query(
        `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                      expected_size_bytes, storage_state, stored_at)
         VALUES ($1, $2, $3, $4, $5, $6, 100, 'stored', transaction_timestamp())`,
        [
          randomUUID(),
          clipId,
          kind,
          bucket,
          `${prefix}/${clipId}/${kind}`,
          `sha256:${"1".repeat(64)}`,
        ],
      );
    }

    const grant = await service.signClipVideo(clipId);

    expect(grant.objectKey).toContain("video");
    expect(signer.signed.filter((entry) => !entry.objectKey.includes("video"))).toHaveLength(0);
  });

  it("refuses an object that is not stored yet", async () => {
    const clipId = await insertClip({ storageState: "pending", retentionState: "retained" });

    await expect(service.signClipVideo(clipId)).rejects.toMatchObject({
      code: "object_not_stored",
    });
  });

  it("refuses a clip that is being purged or already purged", async () => {
    const requested = await insertClip({
      storageState: "stored",
      retentionState: "purge_requested",
    });
    const purged = await insertClip({ storageState: "stored", retentionState: "purged" });

    await expect(service.signClipVideo(requested)).rejects.toMatchObject({
      code: "object_purge_active",
    });
    await expect(service.signClipVideo(purged)).rejects.toMatchObject({
      code: "object_purge_active",
    });
  });

  it("refuses an object key outside the deployment prefix", async () => {
    const clipId = await insertClip({
      storageState: "stored",
      retentionState: "retained",
      objectKey: "raw/production/other-deployment/video.mp4",
    });

    await expect(service.signClipVideo(clipId)).rejects.toMatchObject({
      code: "object_outside_prefix",
    });
  });

  // docs/67 §3.1：certified 车道写的是另一条前缀。前缀钉死一条时，点火后每个新
  // clip 的预览都会 400——坏的正好是新产生的那批数据。
  it("signs a certified-lane key when the deployment admits both lane prefixes", async () => {
    const certifiedPrefix = "raw-staging/v1/certified/dataminer-api-staging";
    const twoLaneService = new PreviewService(
      db as unknown as SqlClient,
      signer,
      loadConfig({
        CONSOLE_ENV: "development",
        DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
        CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
        CONSOLE_IP_HASH_SALT: "p".repeat(48),
        CONSOLE_MAIL_MODE: "capture",
        CONSOLE_R2_ACCOUNT_ID: "account",
        CONSOLE_R2_BUCKET: bucket,
        CONSOLE_R2_ACCESS_KEY_ID: "read-only-key",
        CONSOLE_R2_SECRET_ACCESS_KEY: "read-only-secret",
        CONSOLE_R2_PREFIX: `${prefix}, ${certifiedPrefix}/`,
      }),
    );
    const certifiedClip = await insertClip({
      storageState: "stored",
      retentionState: "retained",
      objectKey: `${certifiedPrefix}/session/run/clip/video.mp4`,
    });
    const nonTrainableClip = await insertClip({ storageState: "stored", retentionState: "retained" });

    await expect(twoLaneService.signClipVideo(certifiedClip)).resolves.toMatchObject({
      objectKey: `${certifiedPrefix}/session/run/clip/video.mp4`,
    });
    // The added lane does not widen anything else: the original prefix still
    // signs, and a key under neither is still refused.
    await expect(twoLaneService.signClipVideo(nonTrainableClip)).resolves.toBeTruthy();
    const foreignClip = await insertClip({
      storageState: "stored",
      retentionState: "retained",
      objectKey: "raw/v1/certified/some-other-deployment/video.mp4",
    });
    await expect(twoLaneService.signClipVideo(foreignClip)).rejects.toMatchObject({
      code: "object_outside_prefix",
    });
  });

  it("refuses an object in a different bucket", async () => {
    const clipId = await insertClip({
      storageState: "stored",
      retentionState: "retained",
      bucketName: "some-other-bucket",
    });

    await expect(service.signClipVideo(clipId)).rejects.toMatchObject({
      code: "object_outside_bucket",
    });
  });

  it("refuses a clip that has no video object", async () => {
    await expect(service.signClipVideo(randomUUID())).rejects.toMatchObject({
      code: "clip_video_not_found",
    });
  });

  it("reports itself unavailable when no read-only credentials are configured", async () => {
    const withoutCredentials = new PreviewService(
      db as unknown as SqlClient,
      null,
      loadConfig({
        CONSOLE_ENV: "development",
        DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
        CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
        CONSOLE_IP_HASH_SALT: "p".repeat(48),
        CONSOLE_MAIL_MODE: "capture",
      }),
    );
    const clipId = await insertClip({ storageState: "stored", retentionState: "retained" });

    expect(withoutCredentials.available).toBe(false);
    await expect(withoutCredentials.signClipVideo(clipId)).rejects.toMatchObject({
      code: "preview_not_configured",
    });
  });

  it("refuses a partial credential configuration instead of quietly disabling preview", () => {
    expect(() =>
      loadConfig({
        CONSOLE_ENV: "development",
        DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
        CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
        CONSOLE_IP_HASH_SALT: "p".repeat(48),
        CONSOLE_MAIL_MODE: "capture",
        CONSOLE_R2_ACCOUNT_ID: "account",
        CONSOLE_R2_BUCKET: bucket,
      }),
    ).toThrow(/every CONSOLE_R2_\* value or none/);
  });
});
