import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";

import type { RequestContext } from "./auth.js";
import { loadConfig, type ConsoleConfig } from "./config.js";
import type { AuthenticatedSession, OperatorRole } from "./domain.js";
import { PostgresGovernanceRepository } from "./governance-repository.js";
import { GovernanceService } from "./governance.js";
import {
  CloudflareD1CredentialSink,
  drainMirrorCredentialOutbox,
  UnconfiguredCredentialSink,
  type MirrorCredentialEvent,
  type MirrorCredentialSink,
} from "./mirror-publisher.js";
import { PostgresConsoleRepository } from "./repository.js";
import type { SqlClient } from "./sql.js";

/**
 * 全部跑在真 PostgreSQL 引擎（PGlite）上，与 governance.test.ts 同一套 harness。
 * 这里要证的不变量几乎全在 SQL 里——可写 CTE 的原子性、outbox 与业务行同生同死、
 * annotator 停用的扇出——从 SQL 文本上论证不出来，只能执行。
 */

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let pool: SqlClient;
let service: GovernanceService;

const config: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "g".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
});

const context: RequestContext = {
  requestId: "req_mirror",
  ipHash: "f".repeat(64),
  userAgent: "vitest",
};

const deviceProfileId = randomUUID();
const deviceProfileCatalogId = "dm-device-1";

function actor(role: OperatorRole, sudoActive: boolean): AuthenticatedSession {
  const id = randomUUID();
  return {
    operator: {
      id,
      email: `${role}@diogenesmodel.com`,
      passwordHash: null,
      role,
      status: "active",
      failedLoginCount: 0,
      lockedUntil: null,
      locked: false,
      passwordUpdatedAt: null,
      lastLoginAt: null,
      createdAt: new Date(0).toISOString(),
      updatedAt: new Date(0).toISOString(),
    },
    session: {
      id: randomUUID(),
      operatorId: id,
      sudoUntil: sudoActive ? new Date(Date.now() + 600_000).toISOString() : null,
      sudoActive,
      expiresAt: new Date(Date.now() + 3_600_000).toISOString(),
      lastSeenAt: new Date().toISOString(),
      createdAt: new Date().toISOString(),
    },
  };
}

const key = () => `key-${randomUUID()}`;

async function seedAnnotator(): Promise<string> {
  const created = await service.createAnnotator(
    actor("operator", false),
    key(),
    { alias: `alpha-${randomUUID().slice(0, 6)}` },
    context,
  );
  return created.id;
}

async function seedDevice(annotatorId?: string) {
  return service.createDevice(
    actor("operator", true),
    key(),
    {
      clientDeviceId: `dev-${randomUUID().slice(0, 8)}`,
      ...(annotatorId ? { annotatorId } : {}),
      certifiedProfileId: deviceProfileCatalogId,
      windowsBuild: "10.0.26100",
      gpuPciId: "PCI\\VEN_10DE",
      gpuDriver: "566.36",
      clientVersion: "0.1.0",
      displaySnapshot: { width: 1920, height: 1080 },
      inputDevicesSnapshot: [{ kind: "keyboard" }],
    },
    context,
  );
}

type OutboxRow = {
  seq: string;
  device_id: string;
  credential_fingerprint: string;
  device_status: string;
  annotator_status: string;
  retire_others: boolean;
  published_at: Date | null;
  attempt_count: number;
};

async function outbox(): Promise<OutboxRow[]> {
  const result = await pool.query<OutboxRow>(
    "SELECT * FROM mirror_credential_outbox ORDER BY seq",
  );
  return result.rows;
}

class RecordingSink implements MirrorCredentialSink {
  readonly batches: MirrorCredentialEvent[][] = [];
  failNext = false;

  async publish(events: MirrorCredentialEvent[]): Promise<void> {
    if (this.failNext) {
      this.failNext = false;
      throw new Error("d1 unreachable");
    }
    this.batches.push(events);
  }

  get events(): MirrorCredentialEvent[] {
    return this.batches.flat();
  }
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  pool = { query: (text, values) => db.query(text, values as never[]) } as SqlClient;
  service = new GovernanceService(
    new PostgresGovernanceRepository(pool),
    new PostgresConsoleRepository(pool),
    config,
  );

  await pool.query(
    `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash, snapshot)
     VALUES ($1, 'device_profile', 'dm-device-1', '1.0.0', NULL, $2, '{}'::jsonb)`,
    [deviceProfileId, `sha256:${"a".repeat(64)}`],
  );
}, 120_000);

afterAll(async () => {
  await db.close();
});

beforeEach(async () => {
  await pool.query("DELETE FROM mirror_credential_outbox");
});

describe("credential outbox is written with the business row", () => {
  it("enqueues on device registration", async () => {
    const created = await seedDevice();

    const rows = await outbox();
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      device_id: created.device.id,
      credential_fingerprint: created.device.credentialFingerprint,
      device_status: "pending",
      annotator_status: "unbound",
      retire_others: false,
      published_at: null,
    });
  });

  it("carries the annotator status when the device is bound", async () => {
    const annotatorId = await seedAnnotator();
    await seedDevice(annotatorId);

    expect((await outbox())[0]).toMatchObject({ annotator_status: "active" });
  });

  it("enqueues on suspension so the mirror stops accepting the device", async () => {
    const created = await seedDevice();
    await pool.query("DELETE FROM mirror_credential_outbox");

    await service.updateDevice(actor("operator", false), key(), created.device.id, { status: "suspended" }, context);

    expect(await outbox()).toMatchObject([{ device_status: "suspended", retire_others: false }]);
  });

  it("marks retire_others on credential rotation", async () => {
    const created = await seedDevice();
    await pool.query("DELETE FROM mirror_credential_outbox");

    const rotated = await service.rotateDeviceCredential(
      actor("operator", true),
      key(),
      created.device.id,
      context,
    );

    const rows = await outbox();
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      credential_fingerprint: rotated.device.credentialFingerprint,
      retire_others: true,
    });
    expect(rows[0]!.credential_fingerprint).not.toBe(created.device.credentialFingerprint);
  });

  it("fans a suspended annotator out to its whole device queue", async () => {
    const annotatorId = await seedAnnotator();
    const first = await seedDevice(annotatorId);
    const second = await seedDevice(annotatorId);
    const unrelated = await seedDevice();
    await pool.query("DELETE FROM mirror_credential_outbox");

    await service.updateAnnotator(actor("operator", false), key(), annotatorId, { status: "suspended" }, context);

    const rows = await outbox();
    expect(rows.map((row) => row.device_id).sort()).toEqual(
      [first.device.id, second.device.id].sort(),
    );
    expect(rows.every((row) => row.annotator_status === "suspended")).toBe(true);
    expect(rows.some((row) => row.device_id === unrelated.device.id)).toBe(false);
  });

  it("writes nothing when the business row does not change", async () => {
    // 不存在的设备：CTE 里 written 为空，outbox 也必须为空——
    // 这正是把它拼进同一条语句而不是跟发一条 INSERT 的意义。
    await expect(
      service.updateDevice(actor("operator", false), key(), randomUUID(), { status: "suspended" }, context),
    ).rejects.toThrow();

    expect(await outbox()).toHaveLength(0);
  });
});

describe("drainMirrorCredentialOutbox", () => {
  it("publishes pending rows in seq order and marks them", async () => {
    const first = await seedDevice();
    const second = await seedDevice();
    const sink = new RecordingSink();

    const result = await drainMirrorCredentialOutbox(pool, sink);

    expect(result).toEqual({ published: 2, failed: 0 });
    expect(sink.events.map((event) => event.deviceId)).toEqual([first.device.id, second.device.id]);
    expect(sink.events.map((event) => event.seq)).toEqual([...sink.events.map((e) => e.seq)].sort((a, b) => a - b));
    expect((await outbox()).every((row) => row.published_at !== null)).toBe(true);
  });

  it("is a no-op when nothing is pending", async () => {
    expect(await drainMirrorCredentialOutbox(pool, new RecordingSink())).toEqual({
      published: 0,
      failed: 0,
    });
  });

  it("does not republish rows that already went out", async () => {
    await seedDevice();
    const sink = new RecordingSink();

    await drainMirrorCredentialOutbox(pool, sink);
    const second = await drainMirrorCredentialOutbox(pool, sink);

    expect(second).toEqual({ published: 0, failed: 0 });
    expect(sink.events).toHaveLength(1);
  });

  it("keeps a failed row pending and backs it off instead of dropping it", async () => {
    await seedDevice();
    const sink = new RecordingSink();
    sink.failNext = true;

    const failure = await drainMirrorCredentialOutbox(pool, sink);

    expect(failure).toEqual({ published: 0, failed: 1 });
    const [row] = await outbox();
    expect(row).toMatchObject({ published_at: null, attempt_count: 1 });
    expect(row!.attempt_count).toBe(1);

    // 退避未到期前不再取，避免失败行把重试打成忙轮询。
    expect(await drainMirrorCredentialOutbox(pool, sink)).toEqual({ published: 0, failed: 0 });

    // 到期后仍然可以成功发出去——失败是「还没发」，不是「丢了」。
    await pool.query("UPDATE mirror_credential_outbox SET available_at = transaction_timestamp()");
    expect(await drainMirrorCredentialOutbox(pool, sink)).toEqual({ published: 1, failed: 0 });
  });

  it("clears last_error once the row finally publishes", async () => {
    await seedDevice();
    const sink = new RecordingSink();
    sink.failNext = true;
    await drainMirrorCredentialOutbox(pool, sink);
    await pool.query("UPDATE mirror_credential_outbox SET available_at = transaction_timestamp()");

    await drainMirrorCredentialOutbox(pool, sink);

    const [row] = await pool.query<{ last_error: string | null }>(
      "SELECT last_error FROM mirror_credential_outbox",
    ).then((result) => result.rows);
    expect(row?.last_error).toBeNull();
  });

  it("respects the batch size", async () => {
    await seedDevice();
    await seedDevice();
    await seedDevice();

    expect(await drainMirrorCredentialOutbox(pool, new RecordingSink(), 2)).toEqual({
      published: 2,
      failed: 0,
    });
  });
});

describe("sinks", () => {
  it("fails loudly when D1 is not configured", async () => {
    await seedDevice();

    const result = await drainMirrorCredentialOutbox(pool, new UnconfiguredCredentialSink());

    // 静默 no-op 会把 outbox 标记 published，镜像永远为空而没人发现。
    expect(result).toEqual({ published: 0, failed: 1 });
    expect((await outbox())[0]).toMatchObject({ published_at: null });
  });

  it("posts one upsert per event and adds a retire when rotating", async () => {
    const calls: { sql: string; params: unknown[] }[] = [];
    let requests = 0;
    const before = Date.now();
    const sink = new CloudflareD1CredentialSink({
      accountId: "acct",
      databaseId: "dbid",
      apiToken: "token",
      fetchImpl: async (_url, init) => {
        requests += 1;
        const body = JSON.parse(String(init?.body)) as {
          batch: { sql: string; params: unknown[] }[];
        };
        calls.push(...body.batch);
        return Response.json({
          success: true,
          result: body.batch.map(() => ({ success: true })),
        });
      },
    });

    await sink.publish([
      {
        seq: 1,
        deviceId: randomUUID(),
        credentialFingerprint: "a".repeat(64),
        deviceStatus: "active",
        annotatorStatus: "unbound",
        retireOthers: false,
      },
      {
        seq: 2,
        deviceId: randomUUID(),
        credentialFingerprint: "b".repeat(64),
        deviceStatus: "active",
        annotatorStatus: "active",
        retireOthers: true,
      },
    ]);

    expect(requests).toBe(1);
    expect(calls).toHaveLength(3);
    expect(calls[0]!.sql).toContain("INSERT INTO mirror_device_credentials");
    expect(calls[2]!.sql).toContain("UPDATE mirror_device_credentials");
    // source_seq 必须随事件一起过去，否则 D1 侧的乱序守卫无从判断。
    expect(calls[0]!.params).toContain(1);
    expect(calls[0]!.params[5]).toEqual(expect.any(Number));
    expect(calls[0]!.params[5] as number).toBeGreaterThanOrEqual(before);
  });

  it.each([
    { success: false, result: [{ success: false }] },
    { success: true, result: [{ success: false }] },
    { success: true, result: [] },
    {},
  ])("rejects an HTTP 200 response unless every D1 statement succeeded: %j", async (payload) => {
    const sink = new CloudflareD1CredentialSink({
      accountId: "acct",
      databaseId: "dbid",
      apiToken: "token",
      fetchImpl: async () => Response.json(payload),
    });

    await expect(
      sink.publish([
        {
          seq: 1,
          deviceId: randomUUID(),
          credentialFingerprint: "d".repeat(64),
          deviceStatus: "active",
          annotatorStatus: "unbound",
          retireOthers: false,
        },
      ]),
    ).rejects.toThrow(/^d1_publish_failed status=200 result=unsuccessful$/);
  });

  it("does not put the D1 response body into the error", async () => {
    const sink = new CloudflareD1CredentialSink({
      accountId: "acct",
      databaseId: "dbid",
      apiToken: "token",
      fetchImpl: async () =>
        // D1 的错误正文会回显 SQL 与参数，参数里有 fingerprint。
        new Response(JSON.stringify({ errors: [{ message: "near a1b2c3: syntax error" }] }), {
          status: 400,
        }),
    });

    await expect(
      sink.publish([
        {
          seq: 1,
          deviceId: randomUUID(),
          credentialFingerprint: "c".repeat(64),
          deviceStatus: "active",
          annotatorStatus: "unbound",
          retireOthers: false,
        },
      ]),
    ).rejects.toThrow(/d1_publish_failed status=400$/);
  });
});
