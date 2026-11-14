/**
 * 中性摄取车道的**路由与契约**（issue #855 §B / PR 3）。
 *
 * 这个文件回答三个问题，每一个都只有一个正确答案：
 *
 * 1. 一次请求写到哪个 prefix / object class / key 形状——只由持久化的
 *    `sessions.schema_version` 决定，v1 与 v2 并存时互不串台；
 * 2. 部署配置变了以后，**已有的** session 还回不回得到原来的地方——回得到，或者
 *    响亮地停住，绝不换一条前缀继续写；
 * 3. 中性契约到底长什么样——Catalog 身份一个字段没少，训练声明一个字段没有。
 *
 * 分类派生（"声明了 v2 就能拿到 candidate 吗"）在 `ingest-v2-classification.test.ts`，
 * 那边打的是真数据库。
 */
import { createHash } from "node:crypto";

import { describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import { loadConfig, type AppConfig, type AdmittedFamily } from "./config.js";
import type {
  ClipRecord,
  IngestionRepository,
  SessionArtifactRecord,
  SessionRecord,
  UploadIntentRecord,
} from "./domain.js";
import { environmentProfile, storagePolicyForSessionSchema } from "./environment.js";
import type { ObjectStorage } from "./storage.js";

const token = "device-credential-with-at-least-32-bytes";
const fingerprint = createHash("sha256").update(token).digest("hex");
const auth = { authorization: `Bearer ${token}` };
const deviceId = "11111111-1111-4111-8111-111111111111";
const annotatorId = "22222222-2222-4222-8222-222222222222";
const sessionId = "33333333-3333-4333-8333-333333333333";
const clipId = "44444444-4444-4444-8444-444444444444";
const digest = `sha256:${"a".repeat(64)}`;

const production = environmentProfile("production");
const legacyFamily = storagePolicyForSessionSchema(production, "dm-local-dev-session-2");
const neutralFamily = storagePolicyForSessionSchema(production, "dm-session-ingest-2");

const LEGACY_PREFIX = "raw/v1/non-trainable/dataminer-api-production";
const V2_PREFIX = "raw/v2/dataminer-api-production";

const legacyLane: AdmittedFamily = { family: legacyFamily, prefix: LEGACY_PREFIX };
const neutralLane: AdmittedFamily = { family: neutralFamily, prefix: V2_PREFIX };

function configWith(admittedFamilies: AdmittedFamily[]): AppConfig {
  return {
    environment: "production",
    profile: production,
    admittedFamilies,
    databaseUrl: "postgresql://example:example@localhost/dataminer",
    proxyToken: null,
    host: "127.0.0.1",
    port: 8080,
    r2: {
      accountId: "account",
      bucket: "bucket",
      accessKeyId: "access",
      secretAccessKey: "secret",
      prefix: LEGACY_PREFIX,
      presignTtlSeconds: 900,
      partSizeBytes: 64 * 1024 * 1024,
      requestTimeoutSeconds: 120,
    },
    worker: {
      batchSize: 4,
      leaseSeconds: 900,
      purgeBatchSize: 4,
      purgeLeaseSeconds: 900,
      multipartCompletionLeaseSeconds: 300,
      orphanMultipartGraceSeconds: 1_200,
    },
    retentionDays: 30,
    logsExport: { enabled: false, bucket: null, batchSize: 200, dryRun: false },
    unconsentedTelemetryTtlDays: 30,
  };
}

function harness(
  overrides: Partial<IngestionRepository>,
  admittedFamilies: AdmittedFamily[] = [legacyLane, neutralLane],
  storageOverrides: Partial<ObjectStorage> = {},
) {
  const repository = {
    ping: vi.fn(async () => undefined),
    authenticateDevice: vi.fn(async (candidate: string) =>
      candidate === fingerprint
        ? { id: deviceId, annotatorId, credentialFingerprint: candidate }
        : null,
    ),
    getIdempotency: vi.fn(async () => null),
    reserveIdempotency: vi.fn(async () => "reserved" as const),
    finishIdempotency: vi.fn(async () => undefined),
    releaseIdempotency: vi.fn(async () => undefined),
    ...overrides,
  } as unknown as IngestionRepository;
  const storage = {
    ping: vi.fn(async () => undefined),
    ...storageOverrides,
  } as unknown as ObjectStorage;
  return {
    app: buildApp({ config: configWith(admittedFamilies), repository, storage }),
    repository,
    storage,
  };
}

function sessionRecord(schemaVersion: string): SessionRecord {
  return {
    id: sessionId,
    deviceId,
    localSessionId: "dev-1784791023860-0000",
    state: "sealed",
    sessionSchemaVersion: schemaVersion,
    createdAt: "2026-08-24T00:00:00.000Z",
    updatedAt: "2026-08-24T00:01:00.000Z",
  };
}

function sessionArtifactRecord(schemaVersion: string, objectKey: string): SessionArtifactRecord {
  return {
    id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    sessionId,
    sessionSchemaVersion: schemaVersion,
    kind: "session",
    objectKey,
    contentType: "application/json",
    expectedBytes: 10,
    expectedSha256: `sha256:${"0".repeat(64)}`,
    ingestState: "pending",
    verificationState: "unverified",
    revision: 1,
  };
}

function clipRecord(): ClipRecord {
  return {
    id: clipId,
    sessionId,
    runId: "55555555-5555-4555-8555-555555555555",
    segmentIndex: 0,
    revision: 1,
    state: "declared",
    storageState: "pending",
    verificationState: "unverified",
    retentionState: "retained",
    retentionDeadline: "2026-09-24T00:00:00.000Z",
    artifacts: [
      {
        id: "66666666-6666-4666-8666-666666666666",
        clipId,
        kind: "video",
        objectKey: `${V2_PREFIX}/sessions/${sessionId}/runs/r/clips/${clipId}/video-x.mp4`,
        expectedBytes: 1024,
        expectedSha256: digest,
        contentType: "video/mp4",
        state: "declared",
      },
    ],
    isTrainable: false,
    trainabilityBlockers: ["game_unapproved"],
    createdAt: "2026-08-24T00:00:00.000Z",
    updatedAt: "2026-08-24T00:00:00.000Z",
  };
}

function uploadIntent(schemaVersion: string): UploadIntentRecord {
  return {
    id: "77777777-7777-4777-8777-777777777777",
    clipId,
    state: "initiated",
    sessionSchemaVersion: schemaVersion,
    uploads: [
      {
        id: "88888888-8888-4888-8888-888888888888",
        uploadIntentId: "77777777-7777-4777-8777-777777777777",
        artifactId: "66666666-6666-4666-8666-666666666666",
        clipId,
        kind: "video",
        objectKey: `${V2_PREFIX}/sessions/${sessionId}/runs/r/clips/${clipId}/video-x.mp4`,
        contentType: "video/mp4",
        state: "initiated",
        attemptNo: 1,
        partSizeBytes: 8 * 1024 * 1024,
        expectedBytes: 1024,
        createdAt: "2026-08-24T00:00:00.000Z",
      },
    ],
    createdAt: "2026-08-24T00:00:00.000Z",
  };
}

const requiredSnapshots = {
  artifacts: {
    session: { byteSize: 10, sha256: `sha256:${"0".repeat(64)}` },
    catalog: { byteSize: 10, sha256: `sha256:${"1".repeat(64)}` },
    device_profile: { byteSize: 10, sha256: `sha256:${"2".repeat(64)}` },
    recording_profile: { byteSize: 10, sha256: `sha256:${"3".repeat(64)}` },
    game_profile: { byteSize: 10, sha256: `sha256:${"4".repeat(64)}` },
    mapping: { byteSize: 10, sha256: `sha256:${"6".repeat(64)}` },
  },
};

// ── ① 声明式路由：唯一的键是持久化的 session 契约版本 ─────────────────────────
describe("① storage routing reads the persisted sessions.schema_version", () => {
  it.each([
    ["dm-local-dev-session-2", LEGACY_PREFIX, "raw-non-trainable", "legacy_flat"],
    ["dm-local-dev-session-3", LEGACY_PREFIX, "raw-non-trainable", "legacy_flat"],
    ["dm-session-ingest-2", V2_PREFIX, "dataminer-raw", "session_scoped"],
  ] as const)(
    "sends a %s session to %s",
    async (schemaVersion, prefix, _objectClass, keyStyle) => {
      const declareSessionArtifacts = vi.fn(async () => [
        sessionArtifactRecord(schemaVersion, `${prefix}/x`),
      ]);
      const { app } = harness({
        getSession: vi.fn(async () => sessionRecord(schemaVersion)),
        declareSessionArtifacts,
      });

      const res = await app.inject({
        method: "PUT",
        url: `/v1/sessions/${sessionId}/artifacts`,
        headers: auth,
        payload: requiredSnapshots,
      });

      expect(res.statusCode).toBe(200);
      expect(declareSessionArtifacts).toHaveBeenCalledWith(
        fingerprint,
        sessionId,
        requiredSnapshots,
        prefix,
        keyStyle,
      );
      await app.close();
    },
  );

  it("stamps each lane's own object class on the presigned session snapshot PUT", async () => {
    const signPutObject = vi.fn(async () => ({
      url: "https://example/put",
      headers: {},
      expiresInSeconds: 900,
    }));
    const { app } = harness(
      {
        getSessionArtifact: vi.fn(async () =>
          sessionArtifactRecord("dm-session-ingest-2", `${V2_PREFIX}/sessions/${sessionId}/session/session.json`),
        ),
      },
      [legacyLane, neutralLane],
      { signPutObject },
    );

    const res = await app.inject({
      method: "POST",
      url: `/v1/sessions/${sessionId}/artifacts/session/sign`,
      headers: auth,
      payload: { contentMd5: "A".repeat(22) + "==" },
    });

    expect(res.statusCode).toBe(200);
    expect(signPutObject).toHaveBeenCalledWith(
      expect.objectContaining({ objectClass: "dataminer-raw" }),
    );
    await app.close();
  });

  it("stamps the legacy object class on a legacy session even where v2 is admitted", async () => {
    const createMultipart = vi.fn(async () => "provider-upload-1");
    const { app } = harness(
      {
        getClip: vi.fn(async () => clipRecord()),
        findUploadIntentForClip: vi.fn(async () => uploadIntent("dm-local-dev-session-2")),
        getUploadIntent: vi.fn(async () => ({
          ...uploadIntent("dm-local-dev-session-2"),
          uploads: uploadIntent("dm-local-dev-session-2").uploads.map((upload) => ({
            ...upload,
            providerUploadId: "provider-upload-1",
          })),
        })),
        bindMultipartProviderUpload: vi.fn(async () => undefined),
      },
      [legacyLane, neutralLane],
      { createMultipart },
    );

    const res = await app.inject({
      method: "POST",
      url: `/v1/clips/${clipId}/upload-intents`,
      headers: { ...auth, "idempotency-key": "legacy-intent-key" },
      payload: { clipRevision: 1, transferMode: "server_owned_r2_multipart" },
    });

    expect(res.statusCode).toBe(201);
    expect(createMultipart).toHaveBeenCalledWith(
      expect.any(String),
      "video/mp4",
      "raw-non-trainable",
    );
    await app.close();
  });

  it("refuses an unknown session contract version instead of guessing a lane", async () => {
    const declareSessionArtifacts = vi.fn();
    const { app } = harness({
      getSession: vi.fn(async () => sessionRecord("dm-some-future-session-9")),
      declareSessionArtifacts,
    });

    const res = await app.inject({
      method: "PUT",
      url: `/v1/sessions/${sessionId}/artifacts`,
      headers: auth,
      payload: requiredSnapshots,
    });

    expect(res.statusCode).toBe(409);
    expect(res.json()).toMatchObject({ code: "lane_not_admitted" });
    expect(declareSessionArtifacts).not.toHaveBeenCalled();
    await app.close();
  });
});

// ── ② 部署配置变了，已有 session 仍回到原来的地方 ─────────────────────────────
//
// 这是本 PR 最容易出错也最贵的一条：一次前缀回退不会报错，只会安静地把新数据写进
// 另一条车道的键空间，而后面没有任何检查会发现。所以它的正确行为是**停住**。
describe("② an existing session survives a deployment configuration change", () => {
  it("stalls loudly instead of falling back when its lane is no longer admitted", async () => {
    const declareSessionArtifacts = vi.fn();
    const { app } = harness(
      {
        getSession: vi.fn(async () => sessionRecord("dm-session-ingest-2")),
        declareSessionArtifacts,
      },
      // 部署把 R2_PREFIX_INGEST_V2 摘掉了，v2 车道不再准入。
      [legacyLane],
    );

    const res = await app.inject({
      method: "PUT",
      url: `/v1/sessions/${sessionId}/artifacts`,
      headers: auth,
      payload: requiredSnapshots,
    });

    expect(res.statusCode).toBe(409);
    expect(res.json()).toMatchObject({ code: "lane_not_admitted" });
    // 关键断言不是那个 409，而是这一条：没有任何对象被安排到 legacy 前缀下。
    expect(declareSessionArtifacts).not.toHaveBeenCalled();
    await app.close();
  });

  it("refuses to complete a v2 upload under the legacy object class", async () => {
    const { app } = harness(
      {
        getUploadIntent: vi.fn(async () => uploadIntent("dm-session-ingest-2")),
      },
      [legacyLane],
    );

    const res = await app.inject({
      method: "POST",
      url: `/v1/upload-intents/77777777-7777-4777-8777-777777777777/complete`,
      headers: { ...auth, "idempotency-key": "complete-after-lane-removal" },
      payload: {
        artifacts: {
          video: { parts: [{ partNumber: 1, etag: "e1" }] },
          frames: { parts: [{ partNumber: 1, etag: "e2" }] },
          events: { parts: [{ partNumber: 1, etag: "e3" }] },
          meta: { parts: [{ partNumber: 1, etag: "e4" }] },
          manifest: { parts: [{ partNumber: 1, etag: "e5" }] },
        },
      },
    });

    expect(res.statusCode).toBe(409);
    expect(res.json()).toMatchObject({ code: "lane_not_admitted" });
    await app.close();
  });

  it("keeps a legacy session on raw/v1 after the v2 lane is added", async () => {
    const declareSessionArtifacts = vi.fn(async () => [
      sessionArtifactRecord("dm-local-dev-session-2", `${LEGACY_PREFIX}/x`),
    ]);
    const { app } = harness(
      {
        getSession: vi.fn(async () => sessionRecord("dm-local-dev-session-2")),
        declareSessionArtifacts,
      },
      [legacyLane, neutralLane],
    );

    const res = await app.inject({
      method: "PUT",
      url: `/v1/sessions/${sessionId}/artifacts`,
      headers: auth,
      payload: requiredSnapshots,
    });

    expect(res.statusCode).toBe(200);
    expect(declareSessionArtifacts).toHaveBeenCalledWith(
      fingerprint,
      sessionId,
      requiredSnapshots,
      LEGACY_PREFIX,
      "legacy_flat",
    );
    await app.close();
  });
});

// ── ③ 中性请求契约 ───────────────────────────────────────────────────────────
function neutralSessionBody() {
  const profile = (id: string) => ({ id, version: "1", contentHash: digest });
  return {
    localSessionId: "dev-1784791023860-0000",
    sessionSchemaVersion: "dm-session-ingest-2",
    startedAt: "2026-08-24T00:00:00.000Z",
    localState: "completed" as const,
    classification: {
      sourceFamily: "dm-session-ingest",
      contractVersion: "dm-1",
      artifactSchemaVersion: "dm-raw-recording-1",
      environment: "production",
      storageNamespace: "raw",
    },
    clientVersion: "0.7.1",
    // Catalog 身份一个字段没少，少的只有那个把车道写进身份里的 `mode`。
    catalog: { id: "dataminer-dev-synthetic", version: "1", contentHash: digest },
    game: { id: "synthetic-window", version: "1", contentHash: digest, executableSha256: digest },
    deviceProfile: profile("synthetic-device"),
    recordingProfile: {
      ...profile("synthetic-1080p60"),
      width: 1920,
      height: 1080,
      fpsNum: 60,
      fpsDen: 1,
      captureEngine: "native_wgc_window_v1" as const,
    },
    mapping: profile("synthetic-mapping"),
  };
}

describe("③ the neutral session contract carries provenance and no training claim", () => {
  it("admits a neutral declaration and echoes a pending, not passing, eligibility", async () => {
    const createSession = vi.fn(async () => sessionRecord("dm-session-ingest-2"));
    const { app } = harness({ createSession });

    const res = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { ...auth, "idempotency-key": "neutral-session-key" },
      payload: neutralSessionBody(),
    });

    expect(res.statusCode).toBe(201);
    expect(res.json()).toMatchObject({
      classification: {
        sourceFamily: "dm-session-ingest",
        artifactSchemaVersion: "dm-raw-recording-1",
        storageNamespace: "raw",
      },
      // candidate 是"有资格等待裁决"，不是通过（issue #855 §C）。
      eligibilityState: "pending",
      eligibilityReason: "trainability_derived_per_clip",
    });
    expect(res.json().classification).not.toHaveProperty("nonTrainable");
    expect(res.json().classification).not.toHaveProperty("isTrainable");
    expect(res.json().classification).not.toHaveProperty("productionCertified");
    await app.close();
  });

  it.each(["nonTrainable", "isTrainable", "productionCertified", "catalogMode"] as const)(
    "refuses a neutral declaration that resurrects %s",
    async (retiredKey) => {
      const createSession = vi.fn();
      const { app } = harness({ createSession });
      const body = neutralSessionBody();

      const res = await app.inject({
        method: "POST",
        url: "/v1/sessions",
        headers: { ...auth, "idempotency-key": `retired-${retiredKey}` },
        payload: {
          ...body,
          classification: { ...body.classification, [retiredKey]: false },
        },
      });

      // 422 policy_blocked 而不是 400：这条车道不收这个词是策略，不是请求形状写错了
      // （与 `enforceDeclaredClassification` 对矛盾取值的既有答法一致）。
      expect(res.statusCode).toBe(422);
      expect(res.json()).toMatchObject({ code: "policy_blocked" });
      expect(createSession).not.toHaveBeenCalled();
      await app.close();
    },
  );

  it("refuses a neutral declaration that puts a lane word back into the catalog identity", async () => {
    const createSession = vi.fn();
    const { app } = harness({ createSession });
    const body = neutralSessionBody();

    const res = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { ...auth, "idempotency-key": "catalog-mode-in-identity" },
      payload: { ...body, catalog: { ...body.catalog, mode: "dev_non_trainable" } },
    });

    expect(res.statusCode).toBe(400);
    expect(createSession).not.toHaveBeenCalled();
    await app.close();
  });

  it("still requires the legacy envelope on the legacy lane", async () => {
    const createSession = vi.fn();
    const { app } = harness({ createSession });
    const body = neutralSessionBody();

    const res = await app.inject({
      method: "POST",
      url: "/v1/sessions",
      headers: { ...auth, "idempotency-key": "legacy-without-booleans" },
      payload: {
        ...body,
        sessionSchemaVersion: "dm-local-dev-session-2",
        classification: {
          sourceFamily: "dm-local-dev",
          contractVersion: "dm-1",
          artifactSchemaVersion: "dm-local-dev-recording-1",
          environment: "production",
          storageNamespace: "raw",
        },
        catalog: { ...body.catalog, mode: "dev_non_trainable" },
      },
    });

    expect(res.statusCode).toBe(400);
    expect(createSession).not.toHaveBeenCalled();
    await app.close();
  });
});

// ── ④ 准入开关本身 ───────────────────────────────────────────────────────────
describe("④ the neutral lane appears only when the deployment names its prefix", () => {
  const baseEnv = {
    DATAMINER_ENV: "production",
    DATABASE_URL: "postgresql://example:example@localhost/dataminer",
    DATAMINER_PROXY_TOKEN: "x".repeat(40),
    R2_ACCOUNT_ID: "account",
    R2_BUCKET: "bucket",
    R2_ACCESS_KEY_ID: "access",
    R2_SECRET_ACCESS_KEY: "secret",
    R2_PREFIX: LEGACY_PREFIX,
    RETENTION_DAYS: "30",
  } as NodeJS.ProcessEnv;

  it("admits only the primary family by default", () => {
    const config = loadConfig({ ...baseEnv });
    expect(config.admittedFamilies).toHaveLength(1);
    expect(config.admittedFamilies[0]?.family.sessionSchemaVersion).toBe("dm-local-dev-session-2");
  });

  it("admits the neutral lane when R2_PREFIX_INGEST_V2 names a raw/v2 prefix", () => {
    const config = loadConfig({ ...baseEnv, R2_PREFIX_INGEST_V2: V2_PREFIX });
    expect(config.admittedFamilies.map((entry) => entry.prefix)).toEqual([
      LEGACY_PREFIX,
      V2_PREFIX,
    ]);
    expect(config.admittedFamilies[1]?.family.objectClass).toBe("dataminer-raw");
  });

  it("refuses a neutral prefix that is not below raw/v2", () => {
    expect(() => loadConfig({ ...baseEnv, R2_PREFIX_INGEST_V2: LEGACY_PREFIX })).toThrow(
      /R2_PREFIX_INGEST_V2 must be an exclusive deployment prefix below raw\/v2/,
    );
  });

  it("refuses the variable in an environment that defines no neutral family", () => {
    expect(() =>
      loadConfig({
        ...baseEnv,
        DATAMINER_ENV: "staging",
        R2_PREFIX: "raw-staging/v1/non-trainable/dataminer-api-staging",
        R2_PREFIX_INGEST_V2: V2_PREFIX,
      }),
    ).toThrow(/defines no neutral ingest family/);
  });
});
