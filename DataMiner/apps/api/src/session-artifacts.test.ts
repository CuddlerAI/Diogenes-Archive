import { createHash } from "node:crypto";
import { Readable } from "node:stream";

import { describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import type { AppConfig } from "./config.js";
import type { IngestionRepository, SessionArtifactRecord, SessionRecord } from "./domain.js";
import { environmentProfile, primaryFamily } from "./environment.js";
import { requiredSessionArtifactKinds } from "./contracts.js";
import { sessionArtifactSetDecision } from "./service.js";
import type { ObjectStorage } from "./storage.js";

const token = "device-credential-with-at-least-32-bytes";
const fingerprint = createHash("sha256").update(token).digest("hex");
const deviceId = "11111111-1111-4111-8111-111111111111";
const sessionId = "33333333-3333-4333-8333-333333333333";
const family = primaryFamily(environmentProfile("staging"));
// legacy 车道恒有 mode；中性摄取家族的 catalogMode 为 null（issue #855 §A.4），
// 这里的夹具专测 legacy，取值时把类型收回 string。
const familySessionSchema = family.sessionSchemaVersion;
const objectClass = family.objectClass;
const auth = { authorization: `Bearer ${token}` };

const config: AppConfig = {
  environment: "staging",
  profile: environmentProfile("staging"),
  admittedFamilies: [
    {
      family,
      prefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
    },
  ],
  databaseUrl: "postgresql://example:example@localhost/dataminer",
  proxyToken: null,
  host: "127.0.0.1",
  port: 8080,
  r2: {
    accountId: "account",
    bucket: "bucket",
    accessKeyId: "access",
    secretAccessKey: "secret",
    prefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
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

function artifact(overrides: Partial<SessionArtifactRecord> = {}): SessionArtifactRecord {
  return {
    id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    sessionId,
    sessionSchemaVersion: familySessionSchema,
    kind: "session",
    objectKey: `${config.r2.prefix}/${sessionId}/session/session.json`,
    contentType: "application/json",
    expectedBytes: 10,
    expectedSha256: `sha256:${"0".repeat(64)}`,
    ingestState: "pending",
    verificationState: "unverified",
    revision: 1,
    ...overrides,
  };
}

const session: SessionRecord = {
  id: sessionId,
  deviceId,
  localSessionId: "dev-1784791023860-0000",
  state: "sealed",
  sessionSchemaVersion: familySessionSchema,
  createdAt: "2026-07-26T00:00:00.000Z",
  updatedAt: "2026-07-26T00:01:00.000Z",
};

function harness(overrides: Partial<IngestionRepository> = {}, storageOverrides: Partial<ObjectStorage> = {}) {
  const repository = {
    ping: vi.fn(async () => undefined),
    authenticateDevice: vi.fn(async (f: string) =>
      f === fingerprint ? { id: deviceId, annotatorId: null, credentialFingerprint: f } : null,
    ),
    getSession: vi.fn(async () => session),
    ...overrides,
  } as unknown as IngestionRepository;
  const storage = { ping: vi.fn(async () => undefined), ...storageOverrides } as unknown as ObjectStorage;
  return { app: buildApp({ config, repository, storage }), repository, storage };
}

const validDeclare = {
  artifacts: {
    session: { byteSize: 10, sha256: `sha256:${"0".repeat(64)}` },
    catalog: { byteSize: 10, sha256: `sha256:${"1".repeat(64)}` },
    device_profile: { byteSize: 10, sha256: `sha256:${"2".repeat(64)}` },
    recording_profile: { byteSize: 10, sha256: `sha256:${"3".repeat(64)}` },
    game_profile: { byteSize: 10, sha256: `sha256:${"4".repeat(64)}` },
    mapping: { byteSize: 10, sha256: `sha256:${"6".repeat(64)}` },
  },
};

describe("session-artifact upload (docs/47)", () => {
  it("requires a device credential", async () => {
    const { app } = harness();
    const res = await app.inject({ method: "PUT", url: `/v1/sessions/${sessionId}/artifacts`, payload: validDeclare });
    expect(res.statusCode).toBe(401);
    await app.close();
  });

  it("rejects a declaration that omits a required snapshot", async () => {
    const { app } = harness({ declareSessionArtifacts: vi.fn() });
    const res = await app.inject({
      method: "PUT",
      url: `/v1/sessions/${sessionId}/artifacts`,
      headers: auth,
      payload: { artifacts: { session: { byteSize: 10, sha256: `sha256:${"0".repeat(64)}` } } },
    });
    expect(res.statusCode).toBe(409);
    await app.close();
  });

  it("declares the artifact set and returns per-kind status", async () => {
    const declareSessionArtifacts = vi.fn(async () => [artifact(), artifact({ kind: "mapping" })]);
    const { app } = harness({ declareSessionArtifacts });
    const res = await app.inject({
      method: "PUT",
      url: `/v1/sessions/${sessionId}/artifacts`,
      headers: auth,
      payload: validDeclare,
    });
    expect(res.statusCode).toBe(200);
    expect(res.json()).toMatchObject({ sessionId, artifacts: [{ kind: "session" }, { kind: "mapping" }] });
    expect(declareSessionArtifacts).toHaveBeenCalledWith(
      fingerprint,
      sessionId,
      validDeclare,
      config.r2.prefix,
      "legacy_flat",
    );
    await app.close();
  });

  it("returns a presigned PUT for a declared artifact", async () => {
    const getSessionArtifact = vi.fn(async () => artifact());
    const signPutObject = vi.fn(async () => ({ url: "https://r2/put", headers: { "content-md5": "x" }, expiresInSeconds: 900 }));
    const { app } = harness({ getSessionArtifact }, { signPutObject });
    const res = await app.inject({
      method: "POST",
      url: `/v1/sessions/${sessionId}/artifacts/session/sign`,
      headers: auth,
      payload: { contentMd5: "1B2M2Y8AsgTpgAmY7PhCfg==" },
    });
    expect(res.statusCode).toBe(200);
    expect(res.json()).toMatchObject({ kind: "session", url: "https://r2/put" });
    expect(signPutObject).toHaveBeenCalledWith(
      expect.objectContaining({ objectClass, contentMd5: "1B2M2Y8AsgTpgAmY7PhCfg==" }),
    );
    await app.close();
  });

  // 原来这里还有一条「consented 车道上的 session-root 上传一律 409」的用例。
  // 它的前提是生产 profile 上存在一条 catalogMode = first_party_release 的已准入
  // 家族——那条家族随 docs/57 §3.3 退役，前提不复存在，用例跟着删。它守的那道门
  // （只有非可训练车道能申报 session-root 快照）仍由本文件其余用例覆盖。

  it("does not issue another write URL after verification became terminal", async () => {
    const signPutObject = vi.fn();
    const { app } = harness(
      { getSessionArtifact: vi.fn(async () => artifact({ verificationState: "rejected" })) },
      { signPutObject },
    );
    const res = await app.inject({
      method: "POST",
      url: `/v1/sessions/${sessionId}/artifacts/session/sign`,
      headers: auth,
      payload: { contentMd5: "1B2M2Y8AsgTpgAmY7PhCfg==" },
    });
    expect(res.statusCode).toBe(409);
    expect(res.json()).toMatchObject({ code: "session_artifact_immutable" });
    expect(signPutObject).not.toHaveBeenCalled();
    await app.close();
  });

  it("verifies SHA-256 from R2 on complete and marks the artifact verified", async () => {
    const body = Buffer.from("hello-meta\n");
    const sha = `sha256:${createHash("sha256").update(body).digest("hex")}`;
    const declared = artifact({ expectedBytes: body.length, expectedSha256: sha });
    const finalizeSessionArtifact = vi.fn(async () => ({ ...declared, ingestState: "stored" as const, verificationState: "verified" as const }));
    const { app } = harness(
      { getSessionArtifact: vi.fn(async () => declared), finalizeSessionArtifact },
      {
        headObject: vi.fn(async () => ({ bytes: body.length, metadata: { dataminer_class: objectClass } })),
        getObject: vi.fn(async () => Readable.from([body])),
      },
    );
    const res = await app.inject({ method: "POST", url: `/v1/sessions/${sessionId}/artifacts/session/complete`, headers: auth });
    expect(res.statusCode).toBe(200);
    expect(res.json()).toMatchObject({ verificationState: "verified" });
    expect(finalizeSessionArtifact).toHaveBeenCalledWith(fingerprint, sessionId, "session", {
      verified: true,
      actualBytes: body.length,
      actualSha256: sha,
    });
    await app.close();
  });

  it("rejects on complete when the stored bytes do not match the declared SHA-256", async () => {
    const declared = artifact({ expectedBytes: 5, expectedSha256: `sha256:${"a".repeat(64)}` });
    const finalizeSessionArtifact = vi.fn(async () => ({ ...declared, verificationState: "rejected" as const }));
    const { app } = harness(
      { getSessionArtifact: vi.fn(async () => declared), finalizeSessionArtifact },
      {
        headObject: vi.fn(async () => ({ bytes: 5, metadata: { dataminer_class: objectClass } })),
        getObject: vi.fn(async () => Readable.from([Buffer.from("wrong")])),
      },
    );
    const res = await app.inject({ method: "POST", url: `/v1/sessions/${sessionId}/artifacts/session/complete`, headers: auth });
    expect(res.statusCode).toBe(200);
    expect(finalizeSessionArtifact).toHaveBeenCalledWith(fingerprint, sessionId, "session", {
      verified: false,
      rejectionCode: "sha256_mismatch",
    });
    await app.close();
  });
});

describe("session artifact set decision (docs/46 §5.2)", () => {
  it("keeps the six-snapshot contract, with markers and calibration optional", () => {
    expect(sessionArtifactSetDecision([...requiredSessionArtifactKinds])).toBe("ok");
    expect(
      sessionArtifactSetDecision([...requiredSessionArtifactKinds, "markers", "input_calibration"]),
    ).toBe("ok");
    expect(sessionArtifactSetDecision(["session", "catalog"])).toBe("incomplete");
  });

  // 断代前还有第二条分支：可训练车道只收 input_calibration 一件。那条车道
  // （consented first-party）随 docs/57 §3.3 退役，而分支的判定条件读的正是
  // `admission.nonTrainable`——issue #855 §B 要删的训练声明字段之一被当成了车道
  // 路由键。中性摄取车道与 legacy 一样自带六件快照，所以规则收敛成一条。
  it("asks for the same six snapshots on the neutral ingest lane", () => {
    expect(sessionArtifactSetDecision(["input_calibration"])).toBe("incomplete");
    expect(sessionArtifactSetDecision(["markers"])).toBe("incomplete");
  });
});
