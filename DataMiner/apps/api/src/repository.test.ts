import { createHash, randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  abortIntentMultipartsSql,
  activeConsentForSessionSql,
  assertRecordingProfileAudioFacts,
  consentScopeShortfall,
  authenticateDeviceSql,
  clipArtifactObjectKey,
  currentCatalogReleaseSql,
  createSessionSql,
  finishArtifactVerificationSql,
  gameAdmissionsSql,
  governanceBlockedVerificationJobsSql,
  maintenancePurgeCandidatesSql,
  resetArtifactsForRestartSql,
  resetMultipartPartsForProviderRestartSql,
  restartMultipartAttemptSql,
  resolveDeviceProfile,
  requiredConsentScopes,
  resolveSessionCatalogs,
  runOutcomeFillOnceSql,
  segmentMetaGenerationExpectation,
  sweepUnconsentedTelemetrySql,
  terminateExpiredIntentSql,
  uploadRestartFenceSql,
  verificationJobCandidatesSql,
} from "./repository.js";

// The gate decides whether a caller may touch the ingest surface at all, so it
// runs against a real PostgreSQL engine (PGlite) instead of being argued from
// the SQL text. The statement under test is imported, not retyped.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

describe("clip artifact object keys", () => {
  it("keeps video and depth under the same clip prefix with a ZIP suffix for depth", () => {
    const common = {
      objectPrefix: "raw/v1/non-trainable/dataminer-api-prod",
      sessionId: "session-1",
      runId: "run-1",
      clipId: "clip-1",
      sha256: `sha256:${"a".repeat(64)}`,
    };
    const video = clipArtifactObjectKey({ ...common, kind: "video" });
    const depth = clipArtifactObjectKey({ ...common, kind: "depth" });

    expect(video).toMatch(/\/clips\/clip-1\/video-a{64}\.mp4$/);
    expect(depth).toMatch(/\/clips\/clip-1\/depth-a{64}\.zip$/);
    expect(depth.slice(0, depth.lastIndexOf("/"))).toBe(
      video.slice(0, video.lastIndexOf("/")),
    );
  });
});

let db: PGlite;
let deviceProfileVersionId: string;
// 设备钉的是 device profile 谱系（catalog_id 字符串），不是某一版本行的 UUID（57 §2.1）。
const DEV_DEVICE_PROFILE_ID = "dev-device-profile";

function fingerprintOf(token: string): string {
  return createHash("sha256").update(token).digest("hex");
}

async function insertAnnotator(status: "active" | "suspended" | "tombstoned" = "active") {
  const id = randomUUID();
  const tombstonedAt = status === "tombstoned" ? "transaction_timestamp()" : "NULL";
  await db.query(
    `INSERT INTO annotators(id, alias, status, tombstoned_at)
     VALUES ($1, $2, $3, ${tombstonedAt})`,
    [id, `annotator-${id.slice(0, 8)}`, status],
  );
  return id;
}

async function insertDevice(options: {
  credentialFingerprint: string;
  annotatorId?: string | null;
  status?: "pending" | "active" | "suspended" | "retired";
}) {
  const id = randomUUID();
  const status = options.status ?? "active";
  const retiredAt = status === "retired" ? "transaction_timestamp()" : "NULL";
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, annotator_id, certified_profile_id,
       credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
       display_snapshot, input_devices_snapshot, client_version, retired_at
     ) VALUES ($1, $2, $3, $4, $5, $6, '10.0.26100', '10DE:2684', '566.36',
               '{}'::jsonb, '[]'::jsonb, '0.1.0', ${retiredAt})`,
    [
      id,
      `client-${id.slice(0, 8)}`,
      options.annotatorId ?? null,
      DEV_DEVICE_PROFILE_ID,
      options.credentialFingerprint,
      status,
    ],
  );
  await db.query(
    `INSERT INTO device_profile_bindings(
       device_id, profile_id, status, validation_method, validation_evidence
     ) VALUES ($1, $2, 'active', 'test_fixture', '{}'::jsonb)`,
    [id, DEV_DEVICE_PROFILE_ID],
  );
  return id;
}

async function authenticate(token: string) {
  const result = await db.query<{ id: string; annotator_id: string | null }>(
    authenticateDeviceSql,
    [fingerprintOf(token)],
  );
  return result.rows[0] ?? null;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  // An approved row carries its approver evidence in the same statement: the
  // table's CHECK refuses `approved` without `approved_at` and `approved_by`.
  deviceProfileVersionId = randomUUID();
  await db.query(
    `INSERT INTO trusted_catalog_versions(
       id, kind, catalog_id, version, game_id, content_hash, snapshot,
       status, approved_at, approved_by
     ) VALUES ($1, 'device_profile', 'dev-device-profile', '1', NULL, $2, '{}'::jsonb,
               'approved', transaction_timestamp(), 'test')`,
    [deviceProfileVersionId, `sha256:${"a".repeat(64)}`],
  );
}, 120_000);

describe("verification completion SQL", () => {
  it("binds the verification enum parameter without PostgreSQL 42P08", async () => {
    await expect(
      db.query(finishArtifactVerificationSql, [randomUUID(), "verified", null]),
    ).resolves.toMatchObject({ affectedRows: 0 });
  });
});

describe("segment meta generation expectations", () => {
  it("keeps legacy verification vocabulary byte-compatible", () => {
    expect(segmentMetaGenerationExpectation("dm-local-dev-session-3")).toEqual({
      state: "non_trainable_ready",
      trainingFields: { non_trainable: true, production_certified: false },
    });
  });

  it("uses neutral ready vocabulary and forbids retired training claims for ingest v2", () => {
    expect(segmentMetaGenerationExpectation("dm-session-ingest-2")).toEqual({
      state: "ready",
      trainingFields: {},
      forbiddenTopLevelKeys: ["non_trainable", "production_certified"],
    });
  });
});

describe("game admission roster", () => {
  it("reads the live capture gate instead of inferring it from catalog presence", async () => {
    const suffix = randomUUID().slice(0, 8);
    const admittedId = `admission-ready-${suffix}`;
    const pausedId = `admission-paused-${suffix}`;
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
       VALUES ($1, $1, 'game', 'first_person', 'rendered', true),
              ($2, $2, 'game', 'first_person', 'rendered', false)`,
      [admittedId, pausedId],
    );

    const result = await db.query<{ id: string; allowed_for_capture: boolean }>(gameAdmissionsSql);
    expect(result.rows).toEqual(
      expect.arrayContaining([
        { id: admittedId, allowed_for_capture: true },
        { id: pausedId, allowed_for_capture: false },
      ]),
    );
  });
});

afterAll(async () => {
  await db?.close();
});

describe("consent scope coverage", () => {
  it("requires the modalities a session of that shape actually produces", () => {
    // Video and input are what every capture emits; audio only exists on the
    // contract that can carry it. Depth is intentionally not here — its gate is
    // depth_consent_policy_versions (0030), and a second, disagreeing gate is
    // worse than one (docs/108 §3.2).
    expect(requiredConsentScopes({ carriesAudio: false })).toEqual(["input", "video"]);
    expect(requiredConsentScopes({ carriesAudio: true })).toEqual(["audio", "input", "video"]);
  });

  it("names what is missing instead of answering yes or no", () => {
    // The predecessor compared a policy-version string for equality, so it could
    // only ever say "wrong document" — it could not say which capture the
    // document failed to cover, and it had to be edited and redeployed for every
    // new terms version.
    expect(consentScopeShortfall(["input", "video"], requiredConsentScopes({ carriesAudio: true })))
      .toEqual(["audio"]);
    expect(
      consentScopeShortfall(
        ["audio", "input", "public_release", "video"],
        requiredConsentScopes({ carriesAudio: true }),
      ),
    ).toEqual([]);
    // A consent whose document could not be identified at all (0031 leaves those
    // scopes empty) covers nothing, including the basics.
    expect(consentScopeShortfall([], requiredConsentScopes({ carriesAudio: false }))).toEqual([
      "input",
      "video",
    ]);
  });

  it("keeps the shipped terms bytes in step with the scope map in migration 0031", async () => {
    // docs/105 §4 amended annotator-2026-07-25-teaser.txt in place: same policy
    // version, different bytes. 0031 keys its scope backfill on terms_hash
    // precisely so the two texts stay distinguishable — which only works while
    // the hashes it lists are the hashes of the files in this repository.
    const termsDirectory = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "../../console-api/terms",
    );
    const migration = await readFile(
      path.resolve(
        path.dirname(fileURLToPath(import.meta.url)),
        "../migrations/0031_annotator_scoped_consent.sql",
      ),
      "utf8",
    );
    for (const file of await readdir(termsDirectory)) {
      if (!file.endsWith(".txt")) continue;
      const bytes = await readFile(path.join(termsDirectory, file));
      const digest = createHash("sha256").update(bytes).digest("hex");
      expect(migration, `${file} is not covered by the 0031 scope map`).toContain(digest);
    }
  });
});

describe("recording profile audio gate", () => {
  const videoSnapshot = {
    width: 1920,
    height: 1080,
    fps_num: 60,
    fps_den: 1,
    capture_backend: "native_wgc_window_v1",
  };
  const processAudio = {
    mode: "process_loopback",
    sample_rate_hz: 48_000,
    channels: 2,
    bits_per_sample: 16,
    sample_format: "pcm_s16le",
    channel_mask: 3,
    compression: "zstd",
  };

  it("requires the session generation to match the authoritative audio profile", () => {
    expect(() => assertRecordingProfileAudioFacts(false, videoSnapshot)).not.toThrow();
    expect(() =>
      assertRecordingProfileAudioFacts(true, { ...videoSnapshot, audio: processAudio }),
    ).not.toThrow();

    for (const [audioRequested, snapshot] of [
      [true, videoSnapshot],
      [false, { ...videoSnapshot, audio: processAudio }],
      [true, { ...videoSnapshot, audio: { ...processAudio, channels: 1 } }],
      [true, { ...videoSnapshot, audio: { ...processAudio, fallback: "device_loopback" } }],
    ] as const) {
      expect(() => assertRecordingProfileAudioFacts(audioRequested, snapshot)).toThrowError(
        expect.objectContaining({ code: "recording_profile_audio_mismatch", statusCode: 409 }),
      );
    }
  });

  it("accepts the shipped COD DEV process-audio profile", async () => {
    const profilePath = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "../../../config/catalogs/dev/cod-steam-release-test/recording-profile.json",
    );
    const snapshot = JSON.parse(await readFile(profilePath, "utf8")) as Record<string, unknown>;
    expect(() => assertRecordingProfileAudioFacts(true, snapshot)).not.toThrow();
  });
});

describe("device credential gate", () => {
  it("resolves a live credential to its device and annotator", async () => {
    const token = "token-active-device";
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf(token),
      annotatorId,
    });

    expect(await authenticate(token)).toEqual({ id: deviceId, annotator_id: annotatorId });
  });

  it("accepts a pending device so a freshly issued credential works before activation", async () => {
    const token = "token-pending-device";
    await insertDevice({ credentialFingerprint: fingerprintOf(token), status: "pending" });

    expect(await authenticate(token)).not.toBeNull();
  });

  it("accepts an unattached device, which is how staging devices exist today", async () => {
    const token = "token-unattached-device";
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf(token),
      annotatorId: null,
    });

    expect(await authenticate(token)).toEqual({ id: deviceId, annotator_id: null });
  });

  it("rejects a credential that was never issued", async () => {
    expect(await authenticate("token-never-issued")).toBeNull();
  });

  it("rejects suspended and retired devices", async () => {
    const suspended = "token-suspended-device";
    const retired = "token-retired-device";
    await insertDevice({ credentialFingerprint: fingerprintOf(suspended), status: "suspended" });
    await insertDevice({ credentialFingerprint: fingerprintOf(retired), status: "retired" });

    expect(await authenticate(suspended)).toBeNull();
    expect(await authenticate(retired)).toBeNull();
  });

  // 11 §12.5: devices are unbounded per annotator, so suspending the annotator
  // has to take the whole fleet down at once — including devices that were
  // enrolled after the suspend, which a batch UPDATE would miss.
  it("takes an annotator's entire fleet down when the annotator is suspended", async () => {
    const annotatorId = await insertAnnotator();
    const first = "token-fleet-first";
    const second = "token-fleet-second";
    await insertDevice({ credentialFingerprint: fingerprintOf(first), annotatorId });
    await insertDevice({ credentialFingerprint: fingerprintOf(second), annotatorId });

    expect(await authenticate(first)).not.toBeNull();
    expect(await authenticate(second)).not.toBeNull();

    await db.query(`UPDATE annotators SET status = 'suspended' WHERE id = $1`, [annotatorId]);

    expect(await authenticate(first)).toBeNull();
    expect(await authenticate(second)).toBeNull();

    // A device enrolled while the annotator is already suspended is refused too,
    // even though its own row is 'active'.
    const late = "token-fleet-enrolled-after-suspend";
    await insertDevice({ credentialFingerprint: fingerprintOf(late), annotatorId });
    expect(await authenticate(late)).toBeNull();
  });

  it("refuses two devices sharing one credential, so a fleet-wide secret cannot come back", async () => {
    const shared = fingerprintOf("token-shared-across-devices");
    await insertDevice({ credentialFingerprint: shared });

    await expect(insertDevice({ credentialFingerprint: shared })).rejects.toThrow();
  });
});

// Issue #453 removes enrollment-time profile qualification. These tests hit
// the production resolver directly: active device state and exact non-revoked
// catalog identity remain mandatory, while legacy pins/bindings are irrelevant.
describe("device profile admission", () => {
  // PGlite 冒充 pg 的 PoolClient：admission helpers 只用到 query。
  type PinClient = Parameters<typeof resolveDeviceProfile>[0];
  const client = {
    query: (text: string, values?: unknown[]) => db.query(text, values),
  } as unknown as PinClient;
  const declare = (version: string, hashFill: string, id = DEV_DEVICE_PROFILE_ID) => ({
    id,
    version,
    contentHash: `sha256:${hashFill.repeat(64)}`,
  });

  async function insertDeviceProfileVersion(catalogId: string, version: string, hashFill: string) {
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_at, approved_by
       ) VALUES ($1, 'device_profile', $2, $3, NULL, $4, '{}'::jsonb,
                 'approved', transaction_timestamp(), 'test')`,
      [randomUUID(), catalogId, version, `sha256:${hashFill.repeat(64)}`],
    );
  }

  it("accepts an exact approved profile without a legacy binding", async () => {
    const token = "token-pin-new-version";
    const deviceId = await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await insertDeviceProfileVersion(DEV_DEVICE_PROFILE_ID, "2", "9");
    await db.query(`DELETE FROM device_profile_bindings WHERE device_id = $1`, [deviceId]);

    const resolved = await resolveDeviceProfile(
      client,
      fingerprintOf(token),
      declare("2", "9"),
    );
    expect(resolved.device.id).toBe(deviceId);
    expect(resolved.deviceCatalog.id).toEqual(expect.any(String));
  });

  it("still accepts the version the device enrolled under", async () => {
    const token = "token-pin-enrolled-version";
    await insertDevice({ credentialFingerprint: fingerprintOf(token) });

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "a")),
    ).resolves.toBeDefined();
  });

  it("does not use legacy bindings as session authorization", async () => {
    const token = "token-pin-multi-profile";
    const deviceId = await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await insertDeviceProfileVersion("secondary-device-profile", "1", "8");
    await db.query(`DELETE FROM device_profile_bindings WHERE device_id = $1`, [deviceId]);

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "a")),
    ).resolves.toBeDefined();
    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "8", "secondary-device-profile")),
    ).resolves.toBeDefined();
  });

  it("ignores a revoked historical binding when the exact profile is approved", async () => {
    const token = "token-pin-revoked-profile";
    const deviceId = await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await insertDeviceProfileVersion("revoked-device-profile", "1", "6");
    await db.query(
      `INSERT INTO device_profile_bindings(
         device_id, profile_id, status, validation_method, validation_evidence, revoked_at
       ) VALUES ($1, $2, 'revoked', 'test_fixture', '{}'::jsonb, transaction_timestamp())`,
      [deviceId, "revoked-device-profile"],
    );

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "6", "revoked-device-profile")),
    ).resolves.toBeDefined();
    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "a")),
    ).resolves.toBeDefined();
  });

  it("accepts another exact approved lineage without enrollment authorization", async () => {
    const token = "token-pin-other-lineage";
    await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await insertDeviceProfileVersion("other-device-profile", "1", "8");

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "8", "other-device-profile")),
    ).resolves.toBeDefined();
  });

  it("refuses a version that was never registered", async () => {
    const token = "token-pin-unregistered";
    await insertDevice({ credentialFingerprint: fingerprintOf(token) });

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("7", "7")),
    ).rejects.toMatchObject({ code: "catalog_version_not_found" });
  });

  it("accepts exact draft and retired profiles while they remain non-revoked", async () => {
    const token = "token-pin-non-revoked";
    await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot, status
       ) VALUES ($1, 'device_profile', 'draft-device-profile', '1', NULL, $2, '{}'::jsonb, 'draft')`,
      [randomUUID(), `sha256:${"5".repeat(64)}`],
    );
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot, status, retired_at
       ) VALUES ($1, 'device_profile', 'retired-device-profile', '1', NULL, $2, '{}'::jsonb,
                 'retired', transaction_timestamp())`,
      [randomUUID(), `sha256:${"4".repeat(64)}`],
    );

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "5", "draft-device-profile")),
    ).resolves.toBeDefined();
    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "4", "retired-device-profile")),
    ).resolves.toBeDefined();
  });

  it("refuses an exact revoked profile", async () => {
    const token = "token-pin-revoked-catalog";
    await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot, status, revoked_at
       ) VALUES ($1, 'device_profile', 'revoked-catalog-profile', '1', NULL, $2, '{}'::jsonb,
                 'revoked', transaction_timestamp())`,
      [randomUUID(), `sha256:${"7".repeat(64)}`],
    );

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "7", "revoked-catalog-profile")),
    ).rejects.toMatchObject({ code: "catalog_version_not_found" });
  });

  it("refuses a device that is not active", async () => {
    const token = "token-pin-pending-device";
    await insertDevice({ credentialFingerprint: fingerprintOf(token), status: "pending" });

    await expect(
      resolveDeviceProfile(client, fingerprintOf(token), declare("1", "a")),
    ).rejects.toMatchObject({ code: "device_not_registered" });
  });
});

describe("session catalog cross-reference admission", () => {
  type AdmissionClient = Parameters<typeof resolveSessionCatalogs>[0];
  const client = {
    query: (text: string, values?: unknown[]) => db.query(text, values),
  } as unknown as AdmissionClient;

  async function fixture(gameId: string) {
    const token = `token-${gameId}`;
    const deviceId = await insertDevice({ credentialFingerprint: fingerprintOf(token) });
    await db.query(`DELETE FROM device_profile_bindings WHERE device_id = $1`, [deviceId]);
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
       VALUES ($1, $2, 'game', 'first_person', 'rendered', true)`,
      [gameId, gameId],
    );
    const refs = {
      game: { id: gameId, version: "1", contentHash: `sha256:${"1".repeat(64)}` },
      deviceProfile: { id: DEV_DEVICE_PROFILE_ID, version: "1", contentHash: `sha256:${"a".repeat(64)}` },
      recordingProfile: {
        id: `${gameId}-recording`,
        version: "1",
        contentHash: `sha256:${"2".repeat(64)}`,
        width: 1920,
        height: 1080,
        fpsNum: 60,
        fpsDen: 1,
        captureEngine: "native_wgc_window_v1" as const,
      },
      mapping: { id: `${gameId}-mapping`, version: "1", contentHash: `sha256:${"3".repeat(64)}` },
    };
    for (const [kind, catalogId, hash, snapshot] of [
      [
        "game_profile",
        refs.game.id,
        refs.game.contentHash,
        {
          game_id: gameId,
          device_profile_id: refs.deviceProfile.id,
          recording_profile_id: refs.recordingProfile.id,
          mapping_id: refs.mapping.id,
        },
      ],
      ["recording_profile", refs.recordingProfile.id, refs.recordingProfile.contentHash, {}],
      ["game_mapping", refs.mapping.id, refs.mapping.contentHash, {}],
    ] as const) {
      await db.query(
        `INSERT INTO trusted_catalog_versions(
           id, kind, catalog_id, version, game_id, content_hash, snapshot,
           status, approved_at, approved_by
         ) VALUES ($1, $2, $3, '1', $4, $5, $6::jsonb,
                   'approved', transaction_timestamp(), 'test')`,
        [randomUUID(), kind, catalogId, gameId, hash, JSON.stringify(snapshot)],
      );
    }
    return { token, deviceId, refs };
  }

  it("admits a legal session catalog tuple without any device binding", async () => {
    const { token, deviceId, refs } = await fixture(`admission-ok-${randomUUID().slice(0, 8)}`);

    const resolved = await resolveSessionCatalogs(client, fingerprintOf(token), refs);

    const sessionId = randomUUID();
    await db.query(createSessionSql, [
      sessionId,
      `local-${sessionId.slice(0, 8)}`,
      deviceId,
      refs.game.id,
      refs.game.version,
      `sha256:${"d".repeat(64)}`,
      "admission-test-catalog",
      "1",
      "dev_non_trainable",
      `sha256:${"e".repeat(64)}`,
      refs.game.contentHash,
      resolved.deviceCatalog.id,
      resolved.recordingCatalog.id,
      resolved.mappingCatalog.id,
      "dm-local-dev-session-2",
      {},
      `sha256:${"f".repeat(64)}`,
      "native_wgc_window_v1",
      1920,
      1080,
      60,
      1,
      "0.7.1",
      "2026-08-04T00:00:00Z",
      null,
      null,
      null,
      "non_trainable",
      null,
      null,
      null,
      null,
      null,
    ]);
    const stored = await db.query<{ id: string }>(`SELECT id FROM sessions WHERE id = $1`, [sessionId]);

    expect(resolved.device.id).toBe(deviceId);
    expect(resolved.gameCatalog.id).toEqual(expect.any(String));
    expect(stored.rows).toEqual([{ id: sessionId }]);
  });

  it("refuses a device profile not referenced by the trusted game profile", async () => {
    const { token, refs } = await fixture(`admission-cross-${randomUUID().slice(0, 8)}`);
    const otherId = `other-device-${randomUUID().slice(0, 8)}`;
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_at, approved_by
       ) VALUES ($1, 'device_profile', $2, '1', NULL, $3, '{}'::jsonb,
                 'approved', transaction_timestamp(), 'test')`,
      [randomUUID(), otherId, `sha256:${"4".repeat(64)}`],
    );

    await expect(
      resolveSessionCatalogs(client, fingerprintOf(token), {
        ...refs,
        deviceProfile: { id: otherId, version: "1", contentHash: `sha256:${"4".repeat(64)}` },
      }),
    ).rejects.toMatchObject({ code: "game_profile_reference_mismatch" });
  });

  it.each([
    ["game_profile", (refs: Awaited<ReturnType<typeof fixture>>["refs"]) => refs.game.id],
    [
      "recording_profile",
      (refs: Awaited<ReturnType<typeof fixture>>["refs"]) => refs.recordingProfile.id,
    ],
    ["game_mapping", (refs: Awaited<ReturnType<typeof fixture>>["refs"]) => refs.mapping.id],
  ] as const)("refuses an exact revoked %s identity", async (kind, catalogIdFor) => {
    const { token, refs } = await fixture(
      `admission-revoked-${kind}-${randomUUID().slice(0, 8)}`,
    );
    await db.query(
      `UPDATE trusted_catalog_versions
          SET status = 'revoked', revoked_at = transaction_timestamp()
        WHERE kind = $1 AND catalog_id = $2`,
      [kind, catalogIdFor(refs)],
    );

    await expect(
      resolveSessionCatalogs(client, fingerprintOf(token), refs),
    ).rejects.toMatchObject({ code: "catalog_version_not_found" });
  });
});

describe("immutable catalog delivery query", () => {
  it("returns only the published release's exact envelope bytes", async () => {
    const envelope = Buffer.from('{"schemaVersion":"dm-signed-catalog-bundle-1"}', "utf8");
    const envelopeHash = `sha256:${createHash("sha256").update(envelope).digest("hex")}`;
    await db.query(
      `INSERT INTO catalog_releases(
         id, channel, catalog_id, catalog_version,
         manifest_bytes, envelope_bytes, envelope_sha256, signing_key_id,
         status, created_by, published_by, published_at
       ) VALUES (
         $1, 'dev', 'dataminer-dev-probe-game', '2026-07-28.90',
         '{}'::bytea, $2::bytea, $3, 'test-key',
         'published', 'test', 'test', transaction_timestamp()
       )`,
      [randomUUID(), envelope, envelopeHash],
    );
    const draftEnvelope = Buffer.from('{"draft":true}', "utf8");
    await db.query(
      `INSERT INTO catalog_releases(
         id, channel, catalog_id, catalog_version,
         manifest_bytes, envelope_bytes, envelope_sha256, signing_key_id, created_by
       ) VALUES (
         $1, 'dev', 'dataminer-dev-probe-game', '2026-07-28.91',
         '{}'::bytea, $2::bytea, $3, 'test-key', 'test'
       )`,
      [
        randomUUID(),
        draftEnvelope,
        `sha256:${createHash("sha256").update(draftEnvelope).digest("hex")}`,
      ],
    );

    const current = await db.query<{
      catalog_id: string;
      catalog_version: string;
      envelope_bytes: Uint8Array;
      envelope_sha256: string;
    }>(currentCatalogReleaseSql, ["dev"]);
    expect(current.rows).toHaveLength(1);
    expect(Buffer.from(current.rows[0]!.envelope_bytes)).toEqual(envelope);
    expect(current.rows[0]).toMatchObject({
      catalog_id: "dataminer-dev-probe-game",
      catalog_version: "2026-07-28.90",
      envelope_sha256: envelopeHash,
    });
    expect((current.rows[0] as Record<string, unknown>).manifest_bytes).toBeUndefined();
  });
});

// 11 §6.5: `sessions.annotator_id / consent_id / capture_grant_id` were never
// written, so the console's "by user" dimension was structurally empty. These
// exercise the statements that fill them.
describe("session identity resolution", () => {
  const gameId = "dev-game";
  let recordingProfileVersionId: string;
  let mappingVersionId: string;

  async function insertGameCatalogVersion(
    kind: "recording_profile" | "game_mapping",
    hashFill: string,
  ) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_at, approved_by
       ) VALUES ($1, $2, $3, '1', $4, $5, '{}'::jsonb,
                 'approved', transaction_timestamp(), 'test')`,
      [id, kind, `dev-${kind}`, gameId, `sha256:${hashFill.repeat(64)}`],
    );
    return id;
  }

  beforeAll(async () => {
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
       VALUES ($1, 'Dev Game', 'game', 'first_person', 'rendered', true)`,
      [gameId],
    );
    recordingProfileVersionId = await insertGameCatalogVersion("recording_profile", "b");
    mappingVersionId = await insertGameCatalogVersion("game_mapping", "3");
  });

  async function insertConsent(
    annotatorId: string,
    options: {
      status?: "active" | "revoked" | "expired";
      acceptedAt?: string;
      supersededAt?: string;
      scopes?: string[];
    } = {},
  ) {
    const id = randomUUID();
    const status = options.status ?? "active";
    const revokedAt = status === "revoked" ? "2026-12-31T00:00:00Z" : null;
    await db.query(
      `INSERT INTO consents(
         id, annotator_id, policy_version, purpose, terms_hash, status,
         accepted_at, revoked_at, revoke_reason, superseded_at, scopes
       ) VALUES ($1, $2, 'v1', 'staging capture', $3, $4,
                 $5::timestamptz, $6::timestamptz, $7, $8::timestamptz, $9::text[])`,
      [
        id,
        annotatorId,
        "c".repeat(64),
        status,
        options.acceptedAt ?? "2026-01-01T00:00:00Z",
        revokedAt,
        status === "revoked" ? "test" : null,
        options.supersededAt ?? null,
        options.scopes ?? ["input", "video"],
      ],
    );
    return id;
  }

  async function insertGrant(options: {
    annotatorId: string;
    consentId: string;
    deviceId: string;
    status?: "active" | "revoked";
    validFrom?: string;
    validUntil?: string;
  }) {
    const id = randomUUID();
    const status = options.status ?? "active";
    const revokedAt = status === "revoked" ? "transaction_timestamp()" : "NULL";
    await db.query(
      `INSERT INTO capture_grants(
         id, annotator_id, consent_id, device_id, game_id, recording_profile_version_id,
         status, valid_from, valid_until, issued_by, revoked_at
       ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'test-operator', ${revokedAt})`,
      [
        id,
        options.annotatorId,
        options.consentId,
        options.deviceId,
        gameId,
        recordingProfileVersionId,
        status,
        options.validFrom ?? "2026-01-01T00:00:00Z",
        options.validUntil ?? "2027-01-01T00:00:00Z",
      ],
    );
    return id;
  }

  async function resolveConsent(annotatorId: string, startedAt: string) {
    const result = await db.query<{
      id: string;
      policy_version: string;
      terms_hash: string;
      scopes: string[];
    }>(activeConsentForSessionSql, [annotatorId, startedAt]);
    return result.rows[0] ?? null;
  }

  const startedAt = "2026-06-01T00:00:00Z";

  async function createSession(options: {
    deviceId: string;
    annotatorId: string | null;
    consentId: string | null;
    grantId: string | null;
    classification?: "non_trainable" | "training_candidate";
  }) {
    const id = randomUUID();
    await db.query(createSessionSql, [
      id,
      `local-${id.slice(0, 8)}`,
      options.deviceId,
      gameId,
      "1.0.0",
      `sha256:${"d".repeat(64)}`,
      // session-v2 shape: migration 0004 requires the catalog evidence quintet
      // to be present together, with catalog_mode pinned to dev_non_trainable.
      "dev-catalog",
      "1",
      "dev_non_trainable",
      `sha256:${"e".repeat(64)}`,
      `sha256:${"f".repeat(64)}`,
      deviceProfileVersionId,
      recordingProfileVersionId,
      mappingVersionId,
      "dm-local-dev-session-2",
      {},
      `sha256:${"1".repeat(64)}`,
      "native_wgc_window_v1",
      1920,
      1080,
      60,
      1,
      "0.1.0",
      startedAt,
      options.annotatorId,
      options.consentId,
      options.grantId,
      options.classification ?? "non_trainable",
      null,
      null,
      null,
      null,
      // 指纹在场，故 missing reason 必须为 null（0019 的互斥约束）。
      null,
    ]);
    const result = await db.query<{
      annotator_id: string | null;
      consent_id: string | null;
      capture_grant_id: string | null;
    }>(`SELECT annotator_id, consent_id, capture_grant_id FROM sessions WHERE id = $1`, [id]);
    return result.rows[0] ?? null;
  }

  // Production raises the ceiling; it does not hand out the classification. The
  // CHECK on `sessions` is the backstop, and these assert it actually fires.
  it("only reaches training_candidate when the identity triple is bound", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      annotatorId,
      credentialFingerprint: "1".repeat(64),
    });
    const consentId = await insertConsent(annotatorId);
    const grantId = await insertGrant({ annotatorId, consentId, deviceId });

    const attributed = await createSession({
      deviceId,
      annotatorId,
      consentId,
      grantId,
      classification: "training_candidate",
    });

    expect(attributed).toMatchObject({ capture_grant_id: grantId });
  });

  it("refuses training_candidate on a session with no grant", async () => {
    const deviceId = await insertDevice({
      annotatorId: null,
      credentialFingerprint: "2".repeat(64),
    });

    await expect(
      createSession({
        deviceId,
        annotatorId: null,
        consentId: null,
        grantId: null,
        classification: "training_candidate",
      }),
    ).rejects.toThrow();
  });

  it("selects the consent whose window contains the session start", async () => {
    const annotatorId = await insertAnnotator();
    const consentId = await insertConsent(annotatorId, {
      acceptedAt: "2026-01-01T00:00:00Z",
      scopes: ["input", "video"],
    });

    expect(await resolveConsent(annotatorId, "2026-06-01T00:00:00Z")).toEqual({
      id: consentId,
      policy_version: "v1",
      terms_hash: "c".repeat(64),
      scopes: ["input", "video"],
    });
  });

  it("does not reach back before the consent was accepted", async () => {
    // A consent authorizes what happens after it is given, not before.
    const annotatorId = await insertAnnotator();
    await insertConsent(annotatorId, { acceptedAt: "2026-03-01T00:00:00Z" });

    expect(await resolveConsent(annotatorId, "2026-02-28T23:59:59Z")).toBeNull();
    // The instant of acceptance itself is inside the window.
    expect(await resolveConsent(annotatorId, "2026-03-01T00:00:00Z")).not.toBeNull();
  });

  it("keeps a superseded consent authoritative for captures made while it was current", async () => {
    // The reason superseding is not withdrawal (docs/108 D5). Re-signing to
    // widen authorization must not retroactively strip the authorization from
    // everything captured under the previous document.
    const annotatorId = await insertAnnotator();
    const older = await insertConsent(annotatorId, {
      status: "expired",
      acceptedAt: "2026-01-01T00:00:00Z",
      supersededAt: "2026-05-01T00:00:00Z",
      scopes: ["input", "video"],
    });
    const newer = await insertConsent(annotatorId, {
      acceptedAt: "2026-05-01T00:00:00Z",
      scopes: ["audio", "input", "video"],
    });

    expect((await resolveConsent(annotatorId, "2026-04-01T00:00:00Z"))?.id).toBe(older);
    // The boundary belongs to the successor: superseded_at is exclusive below
    // and accepted_at is inclusive above, so the two windows meet without
    // overlapping and no instant can resolve to two consents.
    expect((await resolveConsent(annotatorId, "2026-05-01T00:00:00Z"))?.id).toBe(newer);
    expect((await resolveConsent(annotatorId, "2026-06-01T00:00:00Z"))?.id).toBe(newer);
  });

  it("treats withdrawal as retroactive and superseding as not", async () => {
    // Withdrawal is the annotator exercising the terms' withdrawal clause, which
    // promises that data already captured is purged — so it cannot keep
    // authorizing that data in hindsight.
    const annotatorId = await insertAnnotator();
    await insertConsent(annotatorId, {
      status: "revoked",
      acceptedAt: "2026-01-01T00:00:00Z",
    });

    expect(await resolveConsent(annotatorId, "2026-06-01T00:00:00Z")).toBeNull();
    expect(await resolveConsent(annotatorId, "2026-01-02T00:00:00Z")).toBeNull();
  });

  it("does not leak another annotator's consent", async () => {
    const owner = await insertAnnotator();
    const stranger = await insertAnnotator();
    await insertConsent(owner);

    expect(await resolveConsent(stranger, "2026-06-01T00:00:00Z")).toBeNull();
  });

  // Regression guard. Before this change the statement listed 27 columns
  // against 26 expressions — `started_at` had no placeholder — so every insert
  // was rejected with 42601 and `POST /v1/sessions` could not succeed against a
  // real database. The service tests mock the repository and the repository had
  // no SQL tests, so nothing noticed. Counting is cheap; regressing is not.
  it("supplies exactly one expression per target column", () => {
    const between = (open: string, close: string) => {
      const start = createSessionSql.indexOf(open);
      const end = createSessionSql.indexOf(close, start);
      expect(start).toBeGreaterThanOrEqual(0);
      expect(end).toBeGreaterThan(start);
      return createSessionSql.slice(start + open.length, end);
    };
    const count = (text: string) => text.split(",").filter((part) => part.trim() !== "").length;

    expect(count(between("INSERT INTO sessions(", ") VALUES"))).toBe(
      count(between(") VALUES (", ") ON CONFLICT")),
    );
  });

  it("writes the full identity triple onto the session row", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf("token-session-triple"),
      annotatorId,
    });
    const consentId = await insertConsent(annotatorId);
    const grantId = await insertGrant({ annotatorId, consentId, deviceId });

    expect(await createSession({ deviceId, annotatorId, consentId, grantId })).toEqual({
      annotator_id: annotatorId,
      consent_id: consentId,
      capture_grant_id: grantId,
    });
  });

  // The CHECK constraints stage the triple, so a device with no annotator and a
  // device with an annotator but no live grant are both legal — which is what
  // staging looks like today (11 §1.3).
  it("accepts a partial identity for an unattached device", async () => {
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf("token-session-unattached"),
      annotatorId: null,
    });

    expect(
      await createSession({ deviceId, annotatorId: null, consentId: null, grantId: null }),
    ).toEqual({ annotator_id: null, consent_id: null, capture_grant_id: null });
  });

  it("accepts an annotator without a grant, and refuses a grant without one", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf("token-session-partial"),
      annotatorId,
    });

    expect(
      await createSession({ deviceId, annotatorId, consentId: null, grantId: null }),
    ).toEqual({ annotator_id: annotatorId, consent_id: null, capture_grant_id: null });

    // A consent without its annotator is refused by the table, so a coding error
    // that dropped `annotator_id` could never land a half-attributed row.
    await expect(
      createSession({
        deviceId,
        annotatorId: null,
        consentId: await insertConsent(annotatorId),
        grantId: null,
      }),
    ).rejects.toThrow();
  });

  /**
   * docs/67 §3.2：run 的结果三列（帧数 / 停止原因 / 错误码）从 0001 起就可空，
   * 也从来没有写入者。现在客户端会带上它们，服务端**只填一次**：老客户端登记的
   * run 留着 NULL，后来的 PUT 可以补；补上之后任何重发都不能改写——一个能重述
   * 停止原因的客户端，就能把 faulted 洗成 completed。
   */
  it("fills the run outcome once and never rewrites it", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf("token-run-outcome"),
      annotatorId,
    });
    await createSession({ deviceId, annotatorId, consentId: null, grantId: null });
    const sessions = await db.query<{ id: string }>(
      `SELECT id FROM sessions WHERE device_id = $1`,
      [deviceId],
    );
    const sessionId = sessions.rows[0]!.id;
    await db.query(
      `INSERT INTO capture_runs(
         id, session_id, client_run_id, run_seq, status, capture_engine, encoder_backend,
         width, height, fps_num, fps_den, session_frame_start,
         preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
       ) VALUES ($1, $2, 'r0000', 0, 'faulted', 'native_wgc_window_v1',
                 'mf_nvidia_hardware_v1', 1920, 1080, 60, 1, 0,
                 'dm-preflight-evidence-1', $3, 'dev_non_trainable_pass', $4)`,
      [randomUUID(), sessionId, `pf-${randomUUID().slice(0, 8)}`, `sha256:${"8".repeat(64)}`],
    );
    const outcome = async () => {
      const result = await db.query<{
        frame_count: string | null;
        stop_reason: string | null;
        error_code: string | null;
      }>(`SELECT frame_count, stop_reason, error_code FROM capture_runs WHERE session_id = $1`, [
        sessionId,
      ]);
      const row = result.rows[0]!;
      return {
        frameCount: row.frame_count === null ? null : Number(row.frame_count),
        stopReason: row.stop_reason,
        errorCode: row.error_code,
      };
    };

    expect(await outcome()).toEqual({ frameCount: null, stopReason: null, errorCode: null });

    await db.query(runOutcomeFillOnceSql, [
      sessionId,
      0,
      20_400,
      "capture_fault",
      "CAPTURE_QUEUE_DROPPED",
    ]);
    expect(await outcome()).toEqual({
      frameCount: 20_400,
      stopReason: "capture_fault",
      errorCode: "CAPTURE_QUEUE_DROPPED",
    });

    // 第二次说什么都不算数，包括「其实没出错」。
    await db.query(runOutcomeFillOnceSql, [sessionId, 0, 1, "manual", null]);
    expect(await outcome()).toEqual({
      frameCount: 20_400,
      stopReason: "capture_fault",
      errorCode: "CAPTURE_QUEUE_DROPPED",
    });

    // 而没填过的列仍然接受第一次赋值：老客户端登记 + 新客户端重试的组合。
    await db.query(
      `UPDATE capture_runs SET error_code = NULL WHERE session_id = $1`,
      [sessionId],
    );
    await db.query(runOutcomeFillOnceSql, [sessionId, 0, 1, "manual", "LATE_CODE"]);
    expect(await outcome()).toEqual({
      frameCount: 20_400,
      stopReason: "capture_fault",
      errorCode: "LATE_CODE",
    });
  });
});

/**
 * 注册与 enroll 期间产生的遥测早于任何 consent，consent_id 为 NULL——
 * `revokeConsent` 的连带清除看不见它们，而一个再也没人用、因而永远不会被注销的账号
 * 也不会触发 tombstone 兜底。没有这条自己的期限，它们就是无限期保留，
 * 而那正是把 client_events 从 audit_events 里分出来要避免的事（docs/11 §6.4）。
 */
describe("unconsented telemetry deadline", () => {
  async function seedEvent(input: { deviceId: string; annotatorId: string; daysAgo: number }) {
    const eventId = randomUUID();
    await db.query(
      `INSERT INTO client_events(
         event_id, device_id, annotator_id, consent_id, occurred_at, received_at,
         kind, severity, client_version, details
       ) VALUES ($1, $2, $3, NULL, transaction_timestamp(),
                 transaction_timestamp() - make_interval(days => $4),
                 'app_started', 'info', '0.1.0', '{"secret":"payload"}'::jsonb)`,
      [eventId, input.deviceId, input.annotatorId, input.daysAgo],
    );
    return eventId;
  }

  async function stateOf(eventId: string) {
    const result = await db.query<{ retention_state: string; details: unknown }>(
      `SELECT retention_state, details FROM client_events WHERE event_id = $1`,
      [eventId],
    );
    return result.rows[0]!;
  }

  // 与生产同一条语句：从 repository 导出，避免测试里重打一遍 SQL。
  async function sweep(ttlDays: number, limit: number) {
    const result = await db.query<{ id: string }>(sweepUnconsentedTelemetrySql, [ttlDays, limit]);
    return result.rows.length;
  }

  it("erases only what is past the deadline, and leaves the tombstone", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      annotatorId,
      credentialFingerprint: fingerprintOf(`ttl-${randomUUID()}`),
    });
    const stale = await seedEvent({ deviceId, annotatorId, daysAgo: 40 });
    const fresh = await seedEvent({ deviceId, annotatorId, daysAgo: 1 });

    expect(await sweep(30, 100)).toBe(1);

    const staleRow = await stateOf(stale);
    expect(staleRow.retention_state).toBe("purged");
    expect(staleRow.details).toEqual({});
    expect((await stateOf(fresh)).retention_state).toBe("retained");
  });

  it("does not touch telemetry that a consent purge is responsible for", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      annotatorId,
      credentialFingerprint: fingerprintOf(`ttl-${randomUUID()}`),
    });
    const consentId = randomUUID();
    await db.query(
      `INSERT INTO consents(id, annotator_id, policy_version, purpose, terms_hash, status,
                            allows_trainable_use, accepted_at)
       VALUES ($1, $2, '2026-07-01', 'staging capture', $3, 'active', false, transaction_timestamp())`,
      [consentId, annotatorId, "b".repeat(64)],
    );
    const eventId = randomUUID();
    await db.query(
      `INSERT INTO client_events(
         event_id, device_id, annotator_id, consent_id, occurred_at, received_at,
         kind, severity, client_version, details
       ) VALUES ($1, $2, $3, $4, transaction_timestamp(),
                 transaction_timestamp() - make_interval(days => 400),
                 'app_started', 'info', '0.1.0', '{}'::jsonb)`,
      [eventId, deviceId, annotatorId, consentId],
    );

    // 有 consent 的行归 consent 级清除管，哪怕它比这里的期限老得多。
    expect(await sweep(30, 100)).toBe(0);
    expect((await stateOf(eventId)).retention_state).toBe("retained");
  });

  it("is idempotent and bounded", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice({
      annotatorId,
      credentialFingerprint: fingerprintOf(`ttl-${randomUUID()}`),
    });
    for (let index = 0; index < 3; index += 1) {
      await seedEvent({ deviceId, annotatorId, daysAgo: 90 });
    }

    expect(await sweep(30, 2)).toBe(2); // 批量上限生效
    expect(await sweep(30, 100)).toBe(1); // 余下的下一轮扫掉
    expect(await sweep(30, 100)).toBe(0); // 已清的不再重复计入
  });
});

// 治理链的端到端判定。这一组原来挂在 docs/49 E7 的 consented first-party 车道
// 上；那条车道随 docs/57 §3.3 退役，certified 那条随 docs/100 退役，准入词汇表
// 因此只剩 `dev_non_trainable` 一个值。
//
// 保留这组测试的理由是它们测的**不是**那条车道，而是 clip_trainability 的
// 派生：走完整条治理链之后，一个所有 blocker 都清掉的 clip 读出
// `is_trainable = true`，全程没有任何手写的分类。会话行直接用 SQL 铸出来，
// 因此不依赖任何准入家族——今天确实也没有家族能产出 training_candidate
// （first_party 退役后 productionDev 的 bestClassification 是 non_trainable），
// 但那是准入侧的现状，不该让视图侧的回归测试跟着一起消失。
describe("governance chain derives trainability (docs/57 §3.4)", () => {
  const gameId = "first-party-game";
  let recordingProfileVersionId: string;
  let mappingVersionId: string;
  let trainableDeviceProfileVersionId: string;

  async function insertTrainableCatalogVersion(
    kind: "device_profile" | "recording_profile" | "game_mapping",
    fill: string,
  ) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_for_trainable, approved_at, approved_by
       ) VALUES ($1, $2, $3, '1', $4, $5, '{}'::jsonb,
                 'approved', true, transaction_timestamp(), 'test-approver')`,
      [
        id,
        kind,
        `fp-${kind}`,
        kind === "device_profile" ? null : gameId,
        `sha256:${fill.repeat(64)}`,
      ],
    );
    return id;
  }

  beforeAll(async () => {
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ($1, 'First Party Game', 'game', 'first_person', 'rendered', true, true)`,
      [gameId],
    );
    trainableDeviceProfileVersionId = await insertTrainableCatalogVersion("device_profile", "4");
    recordingProfileVersionId = await insertTrainableCatalogVersion("recording_profile", "5");
    mappingVersionId = await insertTrainableCatalogVersion("game_mapping", "7");
  });

  // `grantedRecordingId` 默认取本块共用的那一份。断代之后 grant 的 scope 与
  // 会话的 recording profile 是**同一列**的对账（迁移 0022），所以任何用了
  // 别的 recording profile 的会话，必须让 grant 也指向那一份，否则
  // clip_trainability 会如实报 capture_grant_scope_mismatch——那正是这次
  // 断代要的 fail-closed，不是测试要绕开的噪声。
  async function insertTrainableIdentity(grantedRecordingId?: string) {
    const annotatorId = await insertAnnotator();
    const deviceId = randomUUID();
    await db.query(
      `INSERT INTO devices(
         id, client_device_id, annotator_id, certified_profile_id,
         credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
         display_snapshot, input_devices_snapshot, client_version
       ) VALUES ($1, $2, $3, $4, $5, 'active', '10.0.26100', '10DE:2684', '566.36',
                 '{}'::jsonb, '[]'::jsonb, '0.3.1')`,
      [
        deviceId,
        `fp-client-${deviceId.slice(0, 8)}`,
        annotatorId,
        "fp-device_profile",
        fingerprintOf(`fp-${deviceId}`),
      ],
    );
    const consentId = randomUUID();
    await db.query(
      `INSERT INTO consents(
         id, annotator_id, policy_version, purpose, terms_hash, status,
         allows_trainable_use, accepted_at, scopes
       ) VALUES ($1, $2, '2026-07-25-teaser', 'public first-party release', $3, 'active',
                 true, '2026-01-01T00:00:00Z', $4::text[])`,
      // Accepted well before the fixture sessions start (2026-06-01), and
      // covering the modalities they carry: without both, every clip below
      // would be held back by consent_window_mismatch or
      // consent_scope_insufficient rather than by whatever each case is testing.
      [consentId, annotatorId, "c".repeat(64), ["audio", "input", "video"]],
    );
    const grantId = randomUUID();
    await db.query(
      `INSERT INTO capture_grants(
         id, annotator_id, consent_id, device_id, game_id, recording_profile_version_id,
         status, allows_trainable, valid_from, valid_until, issued_by
       ) VALUES ($1, $2, $3, $4, $5, $6, 'active', true,
                 '2026-01-01T00:00:00Z', '2027-01-01T00:00:00Z', 'test-operator')`,
      [grantId, annotatorId, consentId, deviceId, gameId, grantedRecordingId ?? recordingProfileVersionId],
    );
    return { annotatorId, deviceId, consentId, grantId };
  }

  async function insertTrainableSession(identity: {
    annotatorId: string;
    deviceId: string;
    consentId: string;
    grantId: string;
  }) {
    const id = randomUUID();
    await db.query(createSessionSql, [
      id,
      `fp-local-${id.slice(0, 8)}`,
      identity.deviceId,
      gameId,
      "1.0.0",
      `sha256:${"d".repeat(64)}`,
      "fp-release-catalog",
      "1",
      "dev_non_trainable",
      `sha256:${"e".repeat(64)}`,
      `sha256:${"f".repeat(64)}`,
      trainableDeviceProfileVersionId,
      recordingProfileVersionId,
      mappingVersionId,
      // certified 词表随车道退役（迁移 0029），只剩 dev 一套。会话行是直接铸
      // 出来的，视图侧的判定与准入家族无关——这正是本组测试要保住的性质。
      "dm-local-dev-session-2",
      {},
      `sha256:${"2".repeat(64)}`,
      "native_wgc_window_v1",
      1920,
      1080,
      30,
      1,
      "0.3.1",
      "2026-06-01T00:00:00Z",
      identity.annotatorId,
      identity.consentId,
      identity.grantId,
      "training_candidate",
      null,
      null,
      null,
      null,
      null,
    ]);
    return id;
  }

  /**
   * 批准该会话实测到的 build（migration 0018）。
   *
   * 身份钉不再是 authoring 硬门（#278），把关改到了这里：插入 session 的
   * 触发器把观测指纹登记为 pending，治理点头后才可训练。可训练用例因此都要
   * 显式走一遍这步——正是真实流程。
   */
  async function approveSessionExecutable(sessionId: string) {
    await db.query(
      `UPDATE approved_game_executables age
          SET status = 'approved', approved_by = 'test-governance',
              approved_at = transaction_timestamp()
         FROM sessions s
        WHERE s.id = $1
          AND age.game_id = s.game_id
          AND age.executable_sha256 = s.game_executable_sha256`,
      [sessionId],
    );
  }

  async function insertRun(sessionId: string, preflightResult: string | null) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO capture_runs(
         id, session_id, client_run_id, run_seq, status, capture_engine,
         encoder_backend, width, height, fps_num, fps_den, session_frame_start,
         preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
       ) VALUES ($1, $2, $3, 0, 'completed', 'native_wgc_window_v1',
                 'mf_nvidia_hardware_v1', 1920, 1080, 30, 1, 0,
                 $4, $5, $6, $7)`,
      [
        id,
        sessionId,
        `r0000-${id.slice(0, 8)}`,
        preflightResult === null ? null : "dm-preflight-evidence-1",
        preflightResult === null ? null : `pf-${id.slice(0, 8)}`,
        preflightResult,
        preflightResult === null ? null : `sha256:${"8".repeat(64)}`,
      ],
    );
    return id;
  }

  /**
   * A clip with the full five-object set, every gate below governance already
   * satisfied. What remains between it and `is_trainable` is exactly the
   * consent chain, which is what the cases using this are about.
   */
  async function insertGovernedClip(sessionId: string, fill: string) {
    await approveSessionExecutable(sessionId);
    const runId = await insertRun(sessionId, "dev_non_trainable_pass");
    const clipId = randomUUID();
    await db.query(
      `INSERT INTO clips(
         id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
         n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
         manifest_sha256, ingest_state, verification_state, quality_state,
         stored_at, verified_at, quality_reviewed_at, quality_reviewed_by,
         retention_deadline
       ) VALUES ($1, $2, $3, 0, 0, 0,
                 300, 0, 100000, 42, '{"audio":{"state":"ready"}}'::jsonb, '{}'::jsonb,
                 $4, 'stored', 'verified', 'passed',
                 transaction_timestamp(), transaction_timestamp(), transaction_timestamp(), 'test-reviewer',
                 transaction_timestamp() + interval '30 days')`,
      [clipId, sessionId, runId, `sha256:${fill.repeat(64)}`],
    );
    await db.query(`UPDATE sessions SET schema_version = 'dm-local-dev-session-3' WHERE id = $1`, [
      sessionId,
    ]);
    for (const kind of ["video", "frames", "events", "meta", "manifest"]) {
      await db.query(
        `INSERT INTO artifact_objects(
           id, clip_id, kind, bucket_name, object_key, expected_sha256,
           expected_size_bytes, storage_state, verification_state, stored_at, verified_at
         ) VALUES ($1, $2, $3, 'bucket', $4, $5, 1024, 'stored', 'verified',
                   transaction_timestamp(), transaction_timestamp())`,
        [
          randomUUID(),
          clipId,
          kind,
          `raw/v1/first-party/test/${clipId}/${kind}`,
          `sha256:${"a".repeat(64)}`,
        ],
      );
    }
    return clipId;
  }

  it("pairs a session's runs to its own preflight family", async () => {
    const identity = await insertTrainableIdentity();
    const sessionId = await insertTrainableSession(identity);

    // 退役家族的词汇不再被接受：first_party_release_pass 由迁移 0022 收窄，
    // certified_pass 由 0029 收回（docs/100）。两者都必须是硬拒绝，而不是被
    // 当成"另一条车道"悄悄收下。
    await expect(insertRun(sessionId, "first_party_release_pass")).rejects.toThrow();
    await expect(insertRun(sessionId, "certified_pass")).rejects.toThrow();
    // 唯一那条车道的通过值通过。
    await expect(insertRun(sessionId, "dev_non_trainable_pass")).resolves.toBeDefined();
  });

  // 词表收回与 NULL-mode 堵漏由 0029 的 sessions CHECK 承担。API 合同层到不了
  // 这里（zod 恒给出 mode），CHECK 正是给直接写库的维护脚本留的边界，所以从
  // 数据库边界直接铸行验证。
  it("refuses retired-vocabulary and NULL-mode session rows at the database boundary", async () => {
    const identity = await insertTrainableIdentity();
    const forge = (schema: string, mode: string | null) =>
      db.query(createSessionSql, [
        randomUUID(),
        `fp-forge-${randomUUID().slice(0, 8)}`,
        identity.deviceId,
        gameId,
        "1.0.0",
        `sha256:${"d".repeat(64)}`,
        "fp-release-catalog",
        "1",
        mode,
        `sha256:${"e".repeat(64)}`,
        `sha256:${"f".repeat(64)}`,
        trainableDeviceProfileVersionId,
        recordingProfileVersionId,
        mappingVersionId,
        schema,
        {},
        `sha256:${"5".repeat(64)}`,
        "native_wgc_window_v1",
        1920,
        1080,
        30,
        1,
        "0.3.1",
        "2026-06-01T00:00:00Z",
        null,
        null,
        null,
        "non_trainable",
        null,
        null,
        null,
        null,
        null,
      ]);
    // 退役的 certified 词表，配任何 mode 都写不进去（0029）。
    await expect(forge("dm-certified-session-2", "dev_non_trainable")).rejects.toThrow();
    await expect(forge("dm-certified-session-2", "certified")).rejects.toThrow();
    // 退役的 certified mode，配 dev 词表同样写不进去。
    await expect(forge("dm-local-dev-session-2", "certified")).rejects.toThrow();
    // versioned 词表 + NULL mode：裸等号在 NULL 上算出 NULL 而 CHECK 放行 NULL，
    // IS NOT NULL 连接词把这个历代约束都堵着的洞继续堵着。
    await expect(forge("dm-local-dev-session-2", null)).rejects.toThrow();
    // 对照组：唯一合法的那一对确实能写进去，免得上面几条因为别的原因全绿。
    await expect(forge("dm-local-dev-session-2", "dev_non_trainable")).resolves.toBeDefined();
  });

  it("still refuses training_candidate without the bound identity triple", async () => {
    const identity = await insertTrainableIdentity();
    const id = randomUUID();
    await expect(
      db.query(createSessionSql, [
        id,
        `fp-broken-${id.slice(0, 8)}`,
        identity.deviceId,
        gameId,
        "1.0.0",
        `sha256:${"d".repeat(64)}`,
        "fp-release-catalog",
        "1",
        "dev_non_trainable",
        `sha256:${"e".repeat(64)}`,
        `sha256:${"f".repeat(64)}`,
        trainableDeviceProfileVersionId,
        recordingProfileVersionId,
        mappingVersionId,
        "dm-local-dev-session-2",
        {},
        `sha256:${"3".repeat(64)}`,
        "native_wgc_window_v1",
        1920,
        1080,
        30,
        1,
        "0.3.1",
        "2026-06-01T00:00:00Z",
        identity.annotatorId,
        null, // no consent bound
        null, // no grant bound
        "training_candidate",
        null,
        null,
        null,
        null,
        null,
      ]),
    ).rejects.toThrow();
  });

  it("derives is_trainable = true from the full governance chain, and lists what is missing otherwise", async () => {
    const identity = await insertTrainableIdentity();
    const sessionId = await insertTrainableSession(identity);
    await approveSessionExecutable(sessionId);
    const runId = await insertRun(sessionId, "dev_non_trainable_pass");

    const clipId = randomUUID();
    await db.query(
      `INSERT INTO clips(
         id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
         n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
         manifest_sha256, ingest_state, verification_state, quality_state,
         stored_at, verified_at, quality_reviewed_at, quality_reviewed_by,
         retention_deadline
       ) VALUES ($1, $2, $3, 0, 0, 0,
                 300, 0, 100000, 42, '{}'::jsonb, '{}'::jsonb,
                 $4, 'stored', 'verified', 'passed',
                 transaction_timestamp(), transaction_timestamp(), transaction_timestamp(), 'test-reviewer',
                 transaction_timestamp() + interval '30 days')`,
      [clipId, sessionId, runId, `sha256:${"9".repeat(64)}`],
    );
    for (const kind of ["video", "frames", "events", "meta", "manifest"]) {
      await db.query(
        `INSERT INTO artifact_objects(
           id, clip_id, kind, bucket_name, object_key, expected_sha256,
           expected_size_bytes, storage_state, verification_state, stored_at, verified_at
         ) VALUES ($1, $2, $3, 'bucket', $4, $5, 1024, 'stored', 'verified',
                   transaction_timestamp(), transaction_timestamp())`,
        [
          randomUUID(),
          clipId,
          kind,
          `raw/v1/first-party/test/${clipId}/${kind}`,
          `sha256:${"a".repeat(64)}`,
        ],
      );
    }

    const verdict = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(verdict.rows[0]).toEqual({ is_trainable: true, blockers: [] });

    // Session v3 can legitimately keep the five-object set when audio failed
    // explicitly. Once metadata declares present audio, the sixth object is
    // required and must be verified before the clip becomes trainable.
    await db.query(
      `UPDATE sessions SET schema_version = 'dm-local-dev-session-3' WHERE id = $1`,
      [sessionId],
    );
    await db.query(
      `UPDATE clips SET meta_json = '{"audio":{"state":"absent"}}'::jsonb WHERE id = $1`,
      [clipId],
    );
    const absentAudio = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(absentAudio.rows[0]).toEqual({ is_trainable: true, blockers: [] });
    await db.query(
      `UPDATE clips SET meta_json = '{"audio":{"state":"ready"}}'::jsonb WHERE id = $1`,
      [clipId],
    );
    const missingAudio = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(missingAudio.rows[0]).toEqual({
      is_trainable: false,
      blockers: ["artifact_set_incomplete"],
    });
    await db.query(
      `INSERT INTO artifact_objects(
         id, clip_id, kind, bucket_name, object_key, expected_sha256,
         expected_size_bytes, storage_state, verification_state, stored_at, verified_at
       ) VALUES ($1, $2, 'audio', 'bucket', $3, $4, 1024, 'stored', 'verified',
                 transaction_timestamp(), transaction_timestamp())`,
      [
        randomUUID(),
        clipId,
        `raw/v1/first-party/test/${clipId}/audio`,
        `sha256:${"b".repeat(64)}`,
      ],
    );
    const withAudio = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(withAudio.rows[0]).toEqual({ is_trainable: true, blockers: [] });

    // Take one fact away — quality — and the verdict names exactly that.
    await db.query(`UPDATE clips SET quality_state = 'pending', quality_reviewed_at = NULL WHERE id = $1`, [clipId]);
    const blocked = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(blocked.rows[0]!.is_trainable).toBe(false);
    expect(blocked.rows[0]!.blockers).toEqual(["quality_gate_not_passed"]);
  });

  // 0032: the five capture_grants blockers came out and consent took over the
  // question they were answering. These three pin the replacements, because a
  // gate that is silently absent looks exactly like a gate that passes.
  it("holds back a clip whose consent does not cover a modality it carries", async () => {
    const identity = await insertTrainableIdentity();
    const sessionId = await insertTrainableSession(identity);
    const clipId = await insertGovernedClip(sessionId, "7");
    await db.query(
      `INSERT INTO artifact_objects(
         id, clip_id, kind, bucket_name, object_key, expected_sha256,
         expected_size_bytes, storage_state, verification_state, stored_at, verified_at
       ) VALUES ($1, $2, 'audio', 'bucket', $3, $4, 1024, 'stored', 'verified',
                 transaction_timestamp(), transaction_timestamp())`,
      [randomUUID(), clipId, `raw/v1/first-party/test/${clipId}/audio`, `sha256:${"7".repeat(64)}`],
    );
    // Narrow the consent to the pre-audio reading of the document — exactly what
    // migration 0031 stamps onto anybody who signed the teaser text before it was
    // amended to mention audio.
    await db.query(`UPDATE consents SET scopes = ARRAY['input','video']::text[] WHERE id = $1`, [
      identity.consentId,
    ]);

    const verdict = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(verdict.rows[0]!.blockers).toContain("consent_scope_insufficient");

    // Re-signing the wider document clears it — the whole point of the re-sign
    // prompt is that this is a shortfall the annotator can actually fix.
    await db.query(
      `UPDATE consents SET scopes = ARRAY['audio','input','video']::text[] WHERE id = $1`,
      [identity.consentId],
    );
    const after = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(after.rows[0]).toEqual({ is_trainable: true, blockers: [] });
  });

  it("keeps a superseded consent valid for what was captured inside its window", async () => {
    // docs/108 §1.3 缺陷 C. Under the previous view this clip went from trainable
    // to consent_inactive the instant its owner re-signed — the good-faith act of
    // widening authorization retroactively invalidated their own earlier work.
    const identity = await insertTrainableIdentity();
    const sessionId = await insertTrainableSession(identity);
    const clipId = await insertGovernedClip(sessionId, "8");
    await db.query(
      `INSERT INTO artifact_objects(
         id, clip_id, kind, bucket_name, object_key, expected_sha256,
         expected_size_bytes, storage_state, verification_state, stored_at, verified_at
       ) VALUES ($1, $2, 'audio', 'bucket', $3, $4, 1024, 'stored', 'verified',
                 transaction_timestamp(), transaction_timestamp())`,
      [randomUUID(), clipId, `raw/v1/first-party/test/${clipId}/audio`, `sha256:${"8".repeat(64)}`],
    );
    expect(
      (
        await db.query<{ blockers: string[] }>(
          `SELECT blockers FROM clip_trainability WHERE clip_id = $1`,
          [clipId],
        )
      ).rows[0]!.blockers,
    ).toEqual([]);

    // The session starts 2026-06-01; supersede it afterwards.
    await db.query(
      `UPDATE consents
          SET status = 'expired', superseded_at = '2026-07-01T00:00:00Z'
        WHERE id = $1`,
      [identity.consentId],
    );
    expect(
      (
        await db.query<{ blockers: string[] }>(
          `SELECT blockers FROM clip_trainability WHERE clip_id = $1`,
          [clipId],
        )
      ).rows[0]!.blockers,
    ).toEqual([]);

    // Move the supersede before the capture and it is out of the window again.
    await db.query(`UPDATE consents SET superseded_at = '2026-05-01T00:00:00Z' WHERE id = $1`, [
      identity.consentId,
    ]);
    expect(
      (
        await db.query<{ blockers: string[] }>(
          `SELECT blockers FROM clip_trainability WHERE clip_id = $1`,
          [clipId],
        )
      ).rows[0]!.blockers,
    ).toContain("consent_window_mismatch");
  });

  it("no longer asks for a capture grant", async () => {
    // The grant lane has had no writer since docs/100. While the view still
    // joined it, `missing_capture_grant` was permanently lit on every clip — the
    // gate could not be satisfied, only worked around.
    const identity = await insertTrainableIdentity();
    const sessionId = await insertTrainableSession(identity);
    const clipId = await insertGovernedClip(sessionId, "9");
    await db.query(
      `INSERT INTO artifact_objects(
         id, clip_id, kind, bucket_name, object_key, expected_sha256,
         expected_size_bytes, storage_state, verification_state, stored_at, verified_at
       ) VALUES ($1, $2, 'audio', 'bucket', $3, $4, 1024, 'stored', 'verified',
                 transaction_timestamp(), transaction_timestamp())`,
      [randomUUID(), clipId, `raw/v1/first-party/test/${clipId}/audio`, `sha256:${"9".repeat(64)}`],
    );
    await db.query(`UPDATE sessions SET capture_grant_id = NULL WHERE id = $1`, [sessionId]);

    const verdict = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );
    expect(verdict.rows[0]).toEqual({ is_trainable: true, blockers: [] });
  });

  // 迁移 0015：clip_trainability 自 0001 以来的首次修改。上一个用例的
  // blockers = [] 全绿基线已经证明"未声明 view_scale 的存量目录零漂移"；
  // 目录快照受不可变触发器保护，所以本用例按真实纪律"新政策=新版本"
  // 另铸声明行，验证新门只对声明 applicable 的目录生效、按声明状态开合。
  it("gates view-scale only for catalogs that declare it applicable (docs/46 §5.2)", async () => {
    const insertRecordingWithViewScale = async (applicable: boolean, fill: string) => {
      const id = randomUUID();
      await db.query(
        `INSERT INTO trusted_catalog_versions(
           id, kind, catalog_id, version, game_id, content_hash, snapshot,
           status, approved_for_trainable, approved_at, approved_by
         ) VALUES ($1, 'recording_profile', $2, '1', $3, $4, $5::jsonb,
                   'approved', true, transaction_timestamp(), 'test-approver')`,
        [
          id,
          `fp-recording-vs-${id.slice(0, 8)}`,
          gameId,
          `sha256:${fill.repeat(64)}`,
          JSON.stringify({ view_scale: { applicable } }),
        ],
      );
      return id;
    };
    const insertSessionWithRecording = async (
      identity: Awaited<ReturnType<typeof insertTrainableIdentity>>,
      recordingVersionId: string,
    ) => {
      const id = randomUUID();
      await db.query(createSessionSql, [
        id,
        `fp-vs-${id.slice(0, 8)}`,
        identity.deviceId,
        gameId,
        "1.0.0",
        `sha256:${"d".repeat(64)}`,
        "fp-release-catalog",
        "1",
        "dev_non_trainable",
        `sha256:${"e".repeat(64)}`,
        `sha256:${"f".repeat(64)}`,
        trainableDeviceProfileVersionId,
        recordingVersionId,
        mappingVersionId,
        "dm-local-dev-session-2",
        {},
        `sha256:${"2".repeat(64)}`,
        "native_wgc_window_v1",
        1920,
        1080,
        30,
        1,
        "0.3.1",
        "2026-06-01T00:00:00Z",
        identity.annotatorId,
        identity.consentId,
        identity.grantId,
        "training_candidate",
        null,
        null,
        null,
        null,
        null,
      ]);
      return id;
    };
    const insertGovernedClip = async (sessionId: string, fill: string) => {
      const runId = await insertRun(sessionId, "dev_non_trainable_pass");
      const clipId = randomUUID();
      await db.query(
        `INSERT INTO clips(
           id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
           n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
           manifest_sha256, ingest_state, verification_state, quality_state,
           stored_at, verified_at, quality_reviewed_at, quality_reviewed_by,
           retention_deadline
         ) VALUES ($1, $2, $3, 0, 0, 0,
                   300, 0, 100000, 42, '{}'::jsonb, '{}'::jsonb,
                   $4, 'stored', 'verified', 'passed',
                   transaction_timestamp(), transaction_timestamp(), transaction_timestamp(), 'test-reviewer',
                   transaction_timestamp() + interval '30 days')`,
        [clipId, sessionId, runId, `sha256:${fill.repeat(64)}`],
      );
      for (const kind of ["video", "frames", "events", "meta", "manifest"]) {
        await db.query(
          `INSERT INTO artifact_objects(
             id, clip_id, kind, bucket_name, object_key, expected_sha256,
             expected_size_bytes, storage_state, verification_state, stored_at, verified_at
           ) VALUES ($1, $2, $3, 'bucket', $4, $5, 1024, 'stored', 'verified',
                     transaction_timestamp(), transaction_timestamp())`,
          [randomUUID(), clipId, kind, `raw/v1/first-party/vs/${clipId}/${kind}`, `sha256:${"b".repeat(64)}`],
        );
      }
      return clipId;
    };
    const blockersOf = async (clipId: string) =>
      (
        await db.query<{ blockers: string[] }>(
          `SELECT blockers FROM clip_trainability WHERE clip_id = $1`,
          [clipId],
        )
      ).rows[0]!.blockers;

    // applicable=true 的目录：会话无声明 → 恰好拦这一项；自报 fail 仍拦；
    // pass + 指纹齐 → 清空。
    const gatedRecordingId = await insertRecordingWithViewScale(true, "3");
    const gatedIdentity = await insertTrainableIdentity(gatedRecordingId);
    const gatedSessionId = await insertSessionWithRecording(gatedIdentity, gatedRecordingId);
    await approveSessionExecutable(gatedSessionId);
    const gatedClipId = await insertGovernedClip(gatedSessionId, "8");
    expect(await blockersOf(gatedClipId)).toEqual(["view_scale_unverified"]);
    await db.query(
      `UPDATE sessions SET view_scale_verdict = 'fail',
              view_scale_sha256 = 'sha256:' || repeat('c', 64) WHERE id = $1`,
      [gatedSessionId],
    );
    expect(await blockersOf(gatedClipId)).toEqual(["view_scale_unverified"]);
    await db.query(`UPDATE sessions SET view_scale_verdict = 'pass' WHERE id = $1`, [
      gatedSessionId,
    ]);
    expect(await blockersOf(gatedClipId)).toEqual([]);

    // applicable=false（指针型显式声明）：无任何会话声明也不拦。
    const pointerRecordingId = await insertRecordingWithViewScale(false, "5");
    const pointerIdentity = await insertTrainableIdentity(pointerRecordingId);
    const pointerSessionId = await insertSessionWithRecording(pointerIdentity, pointerRecordingId);
    await approveSessionExecutable(pointerSessionId);
    const pointerClipId = await insertGovernedClip(pointerSessionId, "9");
    expect(await blockersOf(pointerClipId)).toEqual([]);
  });
});

// docs/30 §5.6. An expired upload intent must not be restarted by the client
// quietly clearing its durable intent id: the client cannot prove provider
// state, and clearing it bypasses both the completion fence (0002) and the
// purge fence. These run the real fence statement against a real engine,
// because every predicate in it is what stops a restart from stomping an
// object that actually finished uploading.
describe("fenced restart-upload-attempt (docs/30 §5.6)", () => {
  const gameId = "restart-fence-game";
  const completionLeaseSeconds = 900;
  let deviceProfileId: string;
  let recordingProfileId: string;
  let mappingId: string;

  async function insertCatalogVersion(
    kind: "device_profile" | "recording_profile" | "game_mapping",
    fill: string,
  ) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_at, approved_by
       ) VALUES ($1, $2, $3, '1', $4, $5, '{}'::jsonb,
                 'approved', transaction_timestamp(), 'test-approver')`,
      [id, kind, `rf-${kind}`, kind === "device_profile" ? null : gameId, `sha256:${fill.repeat(64)}`],
    );
    return id;
  }

  beforeAll(async () => {
    await db.query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
       VALUES ($1, 'Restart Fence Game', 'game', 'first_person', 'rendered', true)`,
      [gameId],
    );
    deviceProfileId = await insertCatalogVersion("device_profile", "a");
    recordingProfileId = await insertCatalogVersion("recording_profile", "b");
    mappingId = await insertCatalogVersion("game_mapping", "d");
  });

  /**
   * A clip mid-upload: five artifacts in `uploading`, one intent with five
   * multiparts. `intentAgeHours` decides whether the intent has expired.
   */
  async function seedUploadingClip(options: {
    intentAgeHours: number;
    token?: string;
    classification?: "non_trainable" | "training_candidate";
  }) {
    const token = options.token ?? `rf-${randomUUID()}`;
    const annotatorId = await insertAnnotator();
    const classification = options.classification ?? "non_trainable";
    const consentId =
      classification === "training_candidate"
        ? await (async () => {
            const id = randomUUID();
            await db.query(
              `INSERT INTO consents(
                 id, annotator_id, policy_version, purpose, terms_hash, status,
                 accepted_at, scopes
               ) VALUES ($1, $2, 'candidate-verifier-v1', 'candidate verifier regression',
                         $3, 'active', '2026-01-01T00:00:00Z', ARRAY['input','video']::text[])`,
              [id, annotatorId, "c".repeat(64)],
            );
            return id;
          })()
        : null;
    const deviceId = await insertDevice({
      credentialFingerprint: fingerprintOf(token),
      annotatorId,
    });
    await db.query(`UPDATE devices SET certified_profile_id = $2 WHERE id = $1`, [
      deviceId,
      "rf-device_profile",
    ]);

    const sessionId = randomUUID();
    await db.query(createSessionSql, [
      sessionId,
      `rf-local-${sessionId.slice(0, 8)}`,
      deviceId,
      gameId,
      "1.0.0",
      `sha256:${"1".repeat(64)}`,
      "rf-catalog",
      "1",
      classification === "training_candidate" ? null : "dev_non_trainable",
      `sha256:${"2".repeat(64)}`,
      `sha256:${"3".repeat(64)}`,
      deviceProfileId,
      recordingProfileId,
      mappingId,
      classification === "training_candidate" ? "dm-session-ingest-2" : "dm-local-dev-session-2",
      {},
      `sha256:${"4".repeat(64)}`,
      "native_wgc_window_v1",
      1920,
      1080,
      30,
      1,
      "0.3.1",
      "2026-06-01T00:00:00Z",
      annotatorId,
      consentId,
      null,
      classification,
      null,
      null,
      null,
      null,
      null,
    ]);

    const runId = randomUUID();
    await db.query(
      `INSERT INTO capture_runs(
         id, session_id, client_run_id, run_seq, status, capture_engine,
         encoder_backend, width, height, fps_num, fps_den, session_frame_start,
         preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
       ) VALUES ($1, $2, $3, 0, 'completed', 'native_wgc_window_v1',
                 'mf_nvidia_hardware_v1', 1920, 1080, 30, 1, 0,
                 'dm-preflight-evidence-1', $4, $5, $6)`,
      [
        runId,
        sessionId,
        `r0000-${runId.slice(0, 8)}`,
        `pf-${runId.slice(0, 8)}`,
        classification === "training_candidate" ? "preflight_pass" : "dev_non_trainable_pass",
        `sha256:${"5".repeat(64)}`,
      ],
    );

    const clipId = randomUUID();
    await db.query(
      `INSERT INTO clips(
         id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
         n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
         manifest_sha256, ingest_state, retention_deadline
       ) VALUES ($1, $2, $3, 0, 0, 0, 300, 0, 100000, 42, '{}'::jsonb, '{}'::jsonb,
                 $4, 'uploading', transaction_timestamp() + interval '30 days')`,
      [clipId, sessionId, runId, `sha256:${"6".repeat(64)}`],
    );

    const artifactIds: Record<string, string> = {};
    for (const kind of ["video", "frames", "events", "meta", "manifest"] as const) {
      const artifactId = randomUUID();
      artifactIds[kind] = artifactId;
      await db.query(
        `INSERT INTO artifact_objects(
           id, clip_id, kind, bucket_name, object_key, expected_sha256,
           expected_size_bytes, storage_state
         ) VALUES ($1, $2, $3, 'bucket', $4, $5, 1024, 'uploading')`,
        [artifactId, clipId, kind, `raw/v1/non-trainable/${clipId}/${kind}`, `sha256:${"7".repeat(64)}`],
      );
    }

    const intentId = randomUUID();
    await db.query(
      `INSERT INTO upload_intents(id, clip_id, clip_revision, attempt_no, status, created_at, expires_at)
       VALUES ($1, $2, 1, 1, 'uploading',
               transaction_timestamp() - make_interval(hours => $3),
               transaction_timestamp() - make_interval(hours => $3) + interval '24 hours')`,
      [intentId, clipId, options.intentAgeHours],
    );

    const multipartIds: string[] = [];
    for (const kind of ["video", "frames", "events", "meta", "manifest"] as const) {
      const multipartId = randomUUID();
      multipartIds.push(multipartId);
      await db.query(
        `INSERT INTO multipart_uploads(
           id, upload_intent_id, artifact_object_id, attempt_no, provider_upload_id,
           part_size_bytes, status, expires_at
         ) VALUES ($1, $2, $3, 1, $4, 8388608, 'uploading',
                   transaction_timestamp() + interval '24 hours')`,
        [multipartId, intentId, artifactIds[kind], `provider-${multipartId}`],
      );
    }

    return { token, deviceId, clipId, intentId, artifactIds, multipartIds };
  }

  it("restarts one missing provider upload without changing sibling artifacts", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 1 });
    const multipartId = seed.multipartIds[2]!;
    const leaseId = randomUUID();
    await db.query(
      `UPDATE multipart_uploads
          SET status = 'completing', completion_lease_id = $2,
              completion_started_at = transaction_timestamp()
        WHERE id = $1`,
      [multipartId, leaseId],
    );
    await db.query(
      `INSERT INTO upload_parts(
         multipart_upload_id, part_number, size_bytes, content_md5,
         etag, status, uploaded_at, signed_expires_at
       ) VALUES ($1, 1, 1024, $2, 'old-etag', 'uploaded',
                 transaction_timestamp(), transaction_timestamp() + interval '5 minutes')`,
      [multipartId, "A".repeat(22) + "=="],
    );

    const restarted = await db.query<{ attempt_no: number }>(restartMultipartAttemptSql, [
      multipartId,
      "replacement-provider-upload",
      leaseId,
    ]);
    await db.query(resetMultipartPartsForProviderRestartSql, [multipartId]);

    expect(restarted.rows[0]?.attempt_no).toBe(2);
    const multipart = await db.query<{
      attempt_no: number;
      provider_upload_id: string;
      status: string;
      completion_lease_id: string | null;
    }>(
      `SELECT attempt_no, provider_upload_id, status, completion_lease_id
         FROM multipart_uploads WHERE id = $1`,
      [multipartId],
    );
    expect(multipart.rows[0]).toMatchObject({
      attempt_no: 2,
      provider_upload_id: "replacement-provider-upload",
      status: "initiated",
      completion_lease_id: null,
    });
    const part = await db.query<{ status: string; etag: string | null }>(
      `SELECT status, etag FROM upload_parts WHERE multipart_upload_id = $1`,
      [multipartId],
    );
    expect(part.rows[0]).toEqual({ status: "pending", etag: null });
    const siblingAttempts = await db.query<{ attempt_no: number }>(
      `SELECT attempt_no FROM multipart_uploads
        WHERE upload_intent_id = $1 AND id <> $2`,
      [seed.intentId, multipartId],
    );
    expect(siblingAttempts.rows.every((row) => row.attempt_no === 1)).toBe(true);
  });

  it("leases training_candidate verification jobs when the deployment admits them", async () => {
    const seed = await seedUploadingClip({
      intentAgeHours: 1,
      classification: "training_candidate",
    });
    await db.query(
      `UPDATE artifact_objects
          SET storage_state = 'stored', verification_state = 'queued',
              stored_at = transaction_timestamp()
        WHERE id = $1`,
      [seed.artifactIds.video],
    );
    await db.query(
      `INSERT INTO verification_jobs(id, artifact_object_id)
       VALUES ($1, $2)`,
      [randomUUID(), seed.artifactIds.video],
    );

    const admitted = ["non_trainable", "training_candidate"];
    const blocked = await db.query<{ clip_id: string }>(
      governanceBlockedVerificationJobsSql,
      [1000, admitted],
    );
    expect(blocked.rows.map((row) => row.clip_id)).not.toContain(seed.clipId);

    const candidates = await db.query<{ clip_id: string; session_schema_version: string }>(
      verificationJobCandidatesSql,
      [1000, admitted],
    );
    expect(candidates.rows).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          clip_id: seed.clipId,
          session_schema_version: "dm-session-ingest-2",
        }),
      ]),
    );

    const legacyOnly = await db.query<{ clip_id: string; rejection_code: string }>(
      governanceBlockedVerificationJobsSql,
      [1000, ["non_trainable"]],
    );
    expect(legacyOnly.rows).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          clip_id: seed.clipId,
          rejection_code: "classification_blocked",
        }),
      ]),
    );
  });

  async function fence(clipId: string, token: string) {
    const result = await db.query(uploadRestartFenceSql, [
      clipId,
      fingerprintOf(token),
      completionLeaseSeconds,
    ]);
    return result.rows[0] as
      | {
          clip_id: string;
          intent_id: string;
          intent_status: string;
          attempt_no: number;
          intent_expired: boolean;
          completion_window_live: boolean;
          artifacts_beyond_upload: boolean;
          device_allowed: boolean;
          retention_allowed: boolean;
        }
      | undefined;
  }

  it("refuses to restart an intent that has not expired yet", async () => {
    // 1 hour old, 24 hour lifetime: still live. A client asking to restart this
    // is a client that lost its own state, not a client the server should help
    // by throwing away a working upload.
    const seed = await seedUploadingClip({ intentAgeHours: 1 });
    const row = await fence(seed.clipId, seed.token);
    expect(row?.intent_id).toBe(seed.intentId);
    expect(row?.intent_expired).toBe(false);
  });

  it("marks an intent past its 24 hour lifetime as restartable", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    const row = await fence(seed.clipId, seed.token);
    expect(row?.intent_expired).toBe(true);
    expect(row?.completion_window_live).toBe(false);
    expect(row?.artifacts_beyond_upload).toBe(false);
  });

  it("refuses while a provider completion window is still live", async () => {
    // A multipart that entered `completing` inside the lease window may still
    // be finishing at R2. Aborting it now would race a CompleteMultipartUpload
    // that is already in flight.
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await db.query(
      `UPDATE multipart_uploads
          SET status = 'completing',
              completion_lease_id = $2,
              completion_started_at = transaction_timestamp() - interval '1 minute'
        WHERE id = $1`,
      [seed.multipartIds[0], randomUUID()],
    );
    const row = await fence(seed.clipId, seed.token);
    expect(row?.intent_expired).toBe(true);
    expect(row?.completion_window_live).toBe(true);
  });

  it("allows takeover once the completion window is stale", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await db.query(
      `UPDATE multipart_uploads
          SET status = 'completing',
              completion_lease_id = $2,
              completion_started_at = transaction_timestamp() - interval '2 hours'
        WHERE id = $1`,
      [seed.multipartIds[0], randomUUID()],
    );
    const row = await fence(seed.clipId, seed.token);
    expect(row?.completion_window_live).toBe(false);
  });

  it("refuses when any artifact already reached an immutable state", async () => {
    // The dead attempt actually finished one object. Restarting would either
    // overwrite an immutable object or orphan it; both contradict the storage
    // contract, so this must fail closed and go through complete/verify.
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await db.query(
      `UPDATE artifact_objects
          SET storage_state = 'stored', stored_at = transaction_timestamp()
        WHERE id = $1`,
      [seed.artifactIds.meta],
    );
    const row = await fence(seed.clipId, seed.token);
    expect(row?.intent_expired).toBe(true);
    expect(row?.artifacts_beyond_upload).toBe(true);
  });

  it("refuses a second live intent while the dead one is still active", async () => {
    // This is the schema-level reason the client may not quietly rebuild its
    // intent: `upload_intents_one_active_idx` allows one live intent per clip.
    // Restarting is not "insert another one", it is "retire the dead one under
    // a fence, then insert".
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await expect(
      db.query(
        `INSERT INTO upload_intents(id, clip_id, clip_revision, attempt_no, status, expires_at)
         VALUES ($1, $2, 1, 2, 'uploading', transaction_timestamp() + interval '24 hours')`,
        [randomUUID(), seed.clipId],
      ),
    ).rejects.toThrow();
  });

  it("retires the dead attempt and then reads the new one", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await db.query(abortIntentMultipartsSql, [seed.intentId]);
    const terminated = await db.query(terminateExpiredIntentSql, [seed.intentId]);
    expect(terminated.affectedRows).toBe(1);

    const secondIntent = randomUUID();
    await db.query(
      `INSERT INTO upload_intents(id, clip_id, clip_revision, attempt_no, status, expires_at)
       VALUES ($1, $2, 1, 2, 'uploading', transaction_timestamp() + interval '24 hours')`,
      [secondIntent, seed.clipId],
    );

    const row = await fence(seed.clipId, seed.token);
    expect(row?.intent_id).toBe(secondIntent);
    expect(row?.attempt_no).toBe(2);
    // The new attempt is live, so the clip is not restartable again.
    expect(row?.intent_expired).toBe(false);
  });

  it("refuses to terminate an intent that has not expired", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 1 });
    const terminated = await db.query(terminateExpiredIntentSql, [seed.intentId]);
    expect(terminated.affectedRows).toBe(0);
    const status = await db.query<{ status: string }>(
      `SELECT status::text AS status FROM upload_intents WHERE id = $1`,
      [seed.intentId],
    );
    expect(status.rows[0]?.status).toBe("uploading");
  });

  it("does not expose another device's clip", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    const stranger = await seedUploadingClip({ intentAgeHours: 30 });
    expect(await fence(seed.clipId, stranger.token)).toBeUndefined();
  });

  it("rewinds only the artifacts the dead attempt left uploading", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await db.query(
      `UPDATE artifact_objects
          SET storage_state = 'stored', stored_at = transaction_timestamp(),
              provider_etag = 'etag-keep'
        WHERE id = $1`,
      [seed.artifactIds.manifest],
    );
    await db.query(
      `UPDATE artifact_objects SET provider_etag = 'etag-dead' WHERE id = $1`,
      [seed.artifactIds.video],
    );

    await db.query(resetArtifactsForRestartSql, [seed.clipId]);

    const states = await db.query<{ kind: string; storage_state: string; provider_etag: string | null }>(
      `SELECT kind::text AS kind, storage_state::text AS storage_state, provider_etag
         FROM artifact_objects WHERE clip_id = $1 ORDER BY kind`,
      [seed.clipId],
    );
    const byKind = new Map(states.rows.map((row) => [row.kind, row]));
    expect(byKind.get("video")?.storage_state).toBe("pending");
    expect(byKind.get("video")?.provider_etag).toBeNull();
    // `stored` is immutable: the rewind must not touch it or its provider identity.
    expect(byKind.get("manifest")?.storage_state).toBe("stored");
    expect(byKind.get("manifest")?.provider_etag).toBe("etag-keep");
  });

  it("allows only one unfinished restart per clip", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    const insert = (id: string) =>
      db.query(
        `INSERT INTO upload_restart_attempts(
           id, clip_id, from_upload_intent_id, status, reason,
           requested_by_device_id, locked_at, locked_by
         ) VALUES ($1, $2, $3, 'running', 'intent_expired', $4,
                   transaction_timestamp(), 'test-worker')`,
        [id, seed.clipId, seed.intentId, seed.deviceId],
      );
    await insert(randomUUID());
    await expect(insert(randomUUID())).rejects.toThrow();
  });

  it("allows only one successful restart per expired intent", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    const succeed = async (toIntentId: string) => {
      // Retire whatever is live first — the one-active index makes that
      // mandatory, which is the point.
      await db.query(
        `UPDATE upload_intents SET status = 'expired'
          WHERE clip_id = $1 AND status IN ('pending','uploading','completing')`,
        [seed.clipId],
      );
      await db.query(
        `INSERT INTO upload_intents(id, clip_id, clip_revision, attempt_no, status, expires_at)
         VALUES ($1, $2, 1, (SELECT max(attempt_no) + 1 FROM upload_intents WHERE clip_id = $2),
                 'uploading', transaction_timestamp() + interval '24 hours')`,
        [toIntentId, seed.clipId],
      );
      await db.query(
        `INSERT INTO upload_restart_attempts(
           id, clip_id, from_upload_intent_id, to_upload_intent_id, status, reason,
           requested_by_device_id, completed_at
         ) VALUES ($1, $2, $3, $4, 'succeeded', 'intent_expired', $5, transaction_timestamp())`,
        [randomUUID(), seed.clipId, seed.intentId, toIntentId, seed.deviceId],
      );
    };
    await succeed(randomUUID());
    await expect(succeed(randomUUID())).rejects.toThrow();
  });

  it("refuses to record a restart that points back at the intent it replaced", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    await expect(
      db.query(
        `INSERT INTO upload_restart_attempts(
           id, clip_id, from_upload_intent_id, to_upload_intent_id, status, reason,
           requested_by_device_id, completed_at
         ) VALUES ($1, $2, $3, $3, 'succeeded', 'intent_expired', $4, transaction_timestamp())`,
        [randomUUID(), seed.clipId, seed.intentId, seed.deviceId],
      ),
    ).rejects.toThrow();
  });

  it("does not offer an expired intent's clip to the maintenance purge sweeper (docs/66 裁-3)", async () => {
    // The clip is restartable, not garbage: its device was merely offline past
    // the intent lifetime while the local copy still exists. Sweeping it here
    // used to delete the already-uploaded parts and tombstone the clip, after
    // which a retry is refused forever (docs/65 J-31).
    const seed = await seedUploadingClip({ intentAgeHours: 30 });
    const candidates = await db.query<{ id: string; reason: string }>(
      maintenancePurgeCandidatesSql,
      [1000],
    );
    expect(candidates.rows.map((row) => row.id)).not.toContain(seed.clipId);
  });

  it("still sweeps a clip once its own retention deadline passes", async () => {
    const seed = await seedUploadingClip({ intentAgeHours: 1 });
    await db.query(
      `UPDATE clips
          SET created_at = transaction_timestamp() - interval '31 days',
              retention_deadline = transaction_timestamp() - interval '1 hour'
        WHERE id = $1`,
      [seed.clipId],
    );
    const candidates = await db.query<{ id: string; reason: string }>(
      maintenancePurgeCandidatesSql,
      [1000],
    );
    const mine = candidates.rows.find((row) => row.id === seed.clipId);
    expect(mine?.reason).toBe("retention_expired");
  });
});
