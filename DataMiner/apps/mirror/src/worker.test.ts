import { describe, expect, it } from "vitest";

import { credentialFingerprint } from "./auth.js";
import { MAX_EVENTS_PER_BATCH } from "./contracts.js";
import { SqliteD1 } from "./testing/sqlite-d1.js";
import worker, { __testing, handleRequest, type Env } from "./worker.js";

const DEVICE = "7f3c1a20-9b4e-4d21-8c05-6a1e2f3b4c5d";
const OTHER_DEVICE = "c81d4e2a-0f77-4b93-9a16-3d5e7c8b1042";
const CREDENTIAL = "dmc_live_9f2c4a7b1e6d3805";
const OTHER_CREDENTIAL = "dmc_live_0000000000000000";
const T0 = 1_800_000_000_000;

type TestEnv = Env & { db: SqliteD1 };

/** 默认播种一份活凭据；不播种就是「console 还没签发」的 fail-closed 状态。 */
async function newEnv(options: { seed?: boolean } = {}): Promise<TestEnv> {
  const db = SqliteD1.createWithSchema();
  if (options.seed !== false) {
    db.seedCredential({ fingerprint: await credentialFingerprint(CREDENTIAL), deviceId: DEVICE });
  }
  return { MIRROR_DB: db, db };
}

function heartbeat(seq: number) {
  return {
    seq,
    entity: "heartbeat" as const,
    entityId: "hb",
    observedAt: T0 + seq,
    payload: { clientVersion: "0.1.0", recordingState: "recording" as const },
  };
}

async function post(env: Env, body: unknown, credential = CREDENTIAL): Promise<Response> {
  return handleRequest(
    new Request("https://mirror.example/v1/mirror/events", {
      method: "POST",
      headers: { authorization: `Bearer ${credential}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
    env,
    T0,
  );
}

describe("auth", () => {
  /** 四种失败必须与「没带凭据」在响应上完全无法区分。 */
  async function expectIndistinguishableRejection(response: Response) {
    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({
      code: "invalid_device_credential",
      message: "a valid device credential is required",
    });
  }

  it("rejects a missing credential", async () => {
    const response = await handleRequest(
      new Request("https://mirror.example/v1/mirror/events", { method: "POST", body: "{}" }),
      await newEnv(),
      T0,
    );
    await expectIndistinguishableRejection(response);
  });

  it("fails closed when the console has not mirrored any credential yet", async () => {
    const response = await post(await newEnv({ seed: false }), {
      deviceId: DEVICE,
      events: [heartbeat(1)],
    });
    await expectIndistinguishableRejection(response);
  });

  it("rejects an unknown credential", async () => {
    const response = await post(await newEnv(), { deviceId: DEVICE, events: [heartbeat(1)] }, "dmc_live_wrong");
    await expectIndistinguishableRejection(response);
  });

  for (const deviceStatus of ["suspended", "retired"]) {
    it(`rejects a ${deviceStatus} device, indistinguishably`, async () => {
      const env = await newEnv({ seed: false });
      env.db.seedCredential({
        fingerprint: await credentialFingerprint(CREDENTIAL),
        deviceId: DEVICE,
        deviceStatus,
      });
      await expectIndistinguishableRejection(
        await post(env, { deviceId: DEVICE, events: [heartbeat(1)] }),
      );
    });
  }

  for (const annotatorStatus of ["suspended", "tombstoned"]) {
    it(`rejects a credential for a ${annotatorStatus} annotator, indistinguishably`, async () => {
      const env = await newEnv({ seed: false });
      env.db.seedCredential({
        fingerprint: await credentialFingerprint(CREDENTIAL),
        deviceId: DEVICE,
        deviceStatus: "active",
        annotatorStatus,
      });
      await expectIndistinguishableRejection(await post(env, { deviceId: DEVICE, events: [heartbeat(1)] }));
    });
  }

  it("accepts a pending device — a freshly issued credential works immediately", async () => {
    const env = await newEnv({ seed: false });
    env.db.seedCredential({
      fingerprint: await credentialFingerprint(CREDENTIAL),
      deviceId: DEVICE,
      deviceStatus: "pending",
    });
    const response = await post(env, { deviceId: DEVICE, events: [heartbeat(1)] });
    expect(response.status).toBe(200);
  });

  it("rejects a credential whose device does not match the body", async () => {
    const response = await post(await newEnv(), { deviceId: OTHER_DEVICE, events: [heartbeat(1)] });
    expect(response.status).toBe(403);
    expect(await response.json()).toMatchObject({ code: "device_scope_mismatch" });
  });

  it("resolves the device from the credential row, not from the token text", async () => {
    // 另一份凭据指向另一台设备：同一段请求体因此必须被拒。
    const env = await newEnv();
    env.db.seedCredential({
      fingerprint: await credentialFingerprint(OTHER_CREDENTIAL),
      deviceId: OTHER_DEVICE,
    });
    expect((await post(env, { deviceId: DEVICE, events: [heartbeat(1)] }, OTHER_CREDENTIAL)).status).toBe(403);
    expect((await post(env, { deviceId: OTHER_DEVICE, events: [heartbeat(1)] }, OTHER_CREDENTIAL)).status).toBe(200);
  });

  it("refuses to read another device's cursor", async () => {
    const response = await handleRequest(
      new Request(`https://mirror.example/v1/mirror/devices/${OTHER_DEVICE}/cursor`, {
        headers: { authorization: `Bearer ${CREDENTIAL}` },
      }),
      await newEnv(),
      T0,
    );
    expect(response.status).toBe(403);
  });
});

describe("contract", () => {
  it("rejects unknown fields instead of silently dropping them", async () => {
    const response = await post(await newEnv(), {
      deviceId: DEVICE,
      events: [{ ...heartbeat(1), payload: { ...heartbeat(1).payload, windowTitle: "Counter-Strike 2" } }],
    });
    expect(response.status).toBe(400);
    const body = (await response.json()) as { code: string; details: { message: string }[] };
    expect(body.code).toBe("invalid_request");
    expect(JSON.stringify(body.details)).toContain("windowTitle");
  });

  it("rejects an unknown top-level field", async () => {
    const response = await post(await newEnv(), { deviceId: DEVICE, events: [heartbeat(1)], resumeToken: "x" });
    expect(response.status).toBe(400);
  });

  it("rejects events that are not strictly increasing by seq", async () => {
    const response = await post(await newEnv(), { deviceId: DEVICE, events: [heartbeat(5), heartbeat(3)] });
    expect(response.status).toBe(400);
    expect(JSON.stringify(await response.json())).toContain("strictly increasing");
  });

  it("rejects duplicate seq inside one batch", async () => {
    const response = await post(await newEnv(), { deviceId: DEVICE, events: [heartbeat(2), heartbeat(2)] });
    expect(response.status).toBe(400);
  });

  it("rejects a batch larger than the documented cap", async () => {
    const events = Array.from({ length: MAX_EVENTS_PER_BATCH + 1 }, (_, index) => heartbeat(index + 1));
    const response = await post(await newEnv(), { deviceId: DEVICE, events });
    expect(response.status).toBe(400);
  });

  it("accepts a full-size batch", async () => {
    const env = await newEnv();
    const events = Array.from({ length: MAX_EVENTS_PER_BATCH }, (_, index) => heartbeat(index + 1));
    const response = await post(env, { deviceId: DEVICE, events });
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ ackedSeq: MAX_EVENTS_PER_BATCH });
  });

  it("rejects a device id that is not a console-issued uuid", async () => {
    // 即使凭据本身有效，机器名形状的 device id 也过不了契约——
    // 挡住把 hostname 之类的可识别标识混进镜像。
    const env = await newEnv({ seed: false });
    env.db.seedCredential({
      fingerprint: await credentialFingerprint(CREDENTIAL),
      deviceId: "DESKTOP-7F3K2",
    });
    const response = await post(env, { deviceId: "DESKTOP-7F3K2", events: [heartbeat(1)] });
    expect(response.status).toBe(400);
  });

  it("rejects malformed JSON", async () => {
    const response = await handleRequest(
      new Request("https://mirror.example/v1/mirror/events", {
        method: "POST",
        headers: { authorization: `Bearer ${CREDENTIAL}` },
        body: "{not json",
      }),
      await newEnv(),
      T0,
    );
    expect(response.status).toBe(400);
  });

  it("rejects a body over the size cap", async () => {
    const response = await handleRequest(
      new Request("https://mirror.example/v1/mirror/events", {
        method: "POST",
        headers: { authorization: `Bearer ${CREDENTIAL}` },
        body: "x".repeat(__testing.MAX_BODY_BYTES + 1),
      }),
      await newEnv(),
      T0,
    );
    expect(response.status).toBe(413);
  });
});

describe("routes", () => {
  it("reports itself as non-authoritative on every data response", async () => {
    const env = await newEnv();
    const health = await handleRequest(new Request("https://mirror.example/healthz"), env, T0);
    expect(await health.json()).toMatchObject({ authoritative: false });

    const ingest = await post(env, { deviceId: DEVICE, events: [heartbeat(1)] });
    expect(await ingest.json()).toMatchObject({ authoritative: false });
  });

  it("returns a zero cursor for an unknown device, then the stored one", async () => {
    const env = await newEnv();
    const cursorRequest = () =>
      handleRequest(
        new Request(`https://mirror.example/v1/mirror/devices/${DEVICE}/cursor`, {
          headers: { authorization: `Bearer ${CREDENTIAL}` },
        }),
        env,
        T0,
      );

    expect(await (await cursorRequest()).json()).toMatchObject({ ackedSeq: 0 });
    await post(env, { deviceId: DEVICE, events: [heartbeat(42)] });
    expect(await (await cursorRequest()).json()).toMatchObject({ ackedSeq: 42 });
  });

  it("lets a device forget itself", async () => {
    const env = await newEnv();
    await post(env, { deviceId: DEVICE, events: [heartbeat(1)] });

    const response = await handleRequest(
      new Request(`https://mirror.example/v1/mirror/devices/${DEVICE}`, {
        method: "DELETE",
        headers: { authorization: `Bearer ${CREDENTIAL}` },
      }),
      env,
      T0,
    );
    expect(response.status).toBe(200);
    expect(env.db.query("SELECT COUNT(*) AS n FROM mirror_events")[0]).toEqual({ n: 0 });
  });

  it("has no route that writes back to the client or mutates a projection directly", async () => {
    const env = await newEnv();
    const forbidden = [
      ["PATCH", "/v1/mirror/events"],
      ["PUT", `/v1/mirror/devices/${DEVICE}/cursor`],
      ["POST", "/v1/mirror/sessions"],
      ["POST", "/v1/mirror/segments"],
    ] as const;

    for (const [method, path] of forbidden) {
      const response = await handleRequest(
        new Request(`https://mirror.example${path}`, {
          method,
          headers: { authorization: `Bearer ${CREDENTIAL}` },
          ...(method === "POST" || method === "PUT" || method === "PATCH" ? { body: "{}" } : {}),
        }),
        env,
        T0,
      );
      expect([404, 405]).toContain(response.status);
    }
  });
});

describe("scheduled sweep", () => {
  it("uses the configured retention window", () => {
    const now = T0;
    expect(__testing.retentionCutoff({ MIRROR_RETENTION_DAYS: "7" } as Env, now)).toBe(now - 7 * 86_400_000);
    // 无效值不会退化成 0（那等于立刻清空整个库）。
    expect(__testing.retentionCutoff({ MIRROR_RETENTION_DAYS: "0" } as Env, now)).toBe(
      now - __testing.DEFAULT_RETENTION_DAYS * 86_400_000,
    );
    expect(__testing.retentionCutoff({} as Env, now)).toBe(now - __testing.DEFAULT_RETENTION_DAYS * 86_400_000);
  });

  it("runs without throwing on an empty database", async () => {
    await expect(worker.scheduled(null, await newEnv())).resolves.toBeUndefined();
  });
});
