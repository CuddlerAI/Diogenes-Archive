import { describe, expect, it } from "vitest";

import { authenticateDevice, credentialFingerprint } from "./auth.js";
import { RETIRE_OTHER_DEVICE_CREDENTIALS, UPSERT_DEVICE_CREDENTIAL } from "./credentials.js";
import { SqliteD1 } from "./testing/sqlite-d1.js";

/**
 * 这些测试站在 console 发布方的位置上验证写入契约：本服务只读这张表，
 * 但守卫写坏了会直接变成「已停用的设备还能写」，所以不变量在这一侧钉住。
 */

const DEVICE = "7f3c1a20-9b4e-4d21-8c05-6a1e2f3b4c5d";
const OTHER_DEVICE = "c81d4e2a-0f77-4b93-9a16-3d5e7c8b1042";

async function publish(
  db: SqliteD1,
  input: {
    credential: string;
    deviceId?: string;
    sourceSeq: number;
    deviceStatus?: string;
    annotatorStatus?: string;
  },
): Promise<void> {
  await db
    .prepare(UPSERT_DEVICE_CREDENTIAL)
    .bind(
      await credentialFingerprint(input.credential),
      input.deviceId ?? DEVICE,
      input.sourceSeq,
      input.deviceStatus ?? "active",
      input.annotatorStatus ?? "unbound",
      input.sourceSeq,
    )
    .run();
}

function statusOf(db: SqliteD1, fingerprint: string) {
  return db.query<{ device_status: string; source_seq: number }>(
    "SELECT device_status, source_seq FROM mirror_device_credentials WHERE credential_fingerprint = ?",
    fingerprint,
  )[0];
}

describe("credential mirror write contract", () => {
  it("makes a freshly published credential usable", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-a", sourceSeq: 1, deviceStatus: "pending" });

    await expect(authenticateDevice("Bearer cred-a", db)).resolves.toBe(DEVICE);
  });

  it("suspending a device takes effect on the next request", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-a", sourceSeq: 1 });
    await publish(db, { credential: "cred-a", sourceSeq: 2, deviceStatus: "suspended" });

    await expect(authenticateDevice("Bearer cred-a", db)).rejects.toThrow();
  });

  it("refuses to let a replayed older row resurrect a suspended device", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-a", sourceSeq: 1, deviceStatus: "active" });
    await publish(db, { credential: "cred-a", sourceSeq: 7, deviceStatus: "suspended" });
    // 发布方重试把 seq 3 又投了一次：它比当前 7 旧，必须被守卫挡住。
    await publish(db, { credential: "cred-a", sourceSeq: 3, deviceStatus: "active" });

    expect(statusOf(db, await credentialFingerprint("cred-a"))).toEqual({
      device_status: "suspended",
      source_seq: 7,
    });
    await expect(authenticateDevice("Bearer cred-a", db)).rejects.toThrow();
  });

  it("is idempotent — publishing the same row twice changes nothing", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-a", sourceSeq: 5 });
    await publish(db, { credential: "cred-a", sourceSeq: 5, deviceStatus: "suspended" });

    // 相同 seq 不满足 `>`，因此第二次是 no-op：发布方重投不会产生副作用。
    expect(statusOf(db, await credentialFingerprint("cred-a"))).toEqual({
      device_status: "active",
      source_seq: 5,
    });
  });

  it("retires the previous credential when one is rotated", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-old", sourceSeq: 1 });
    await expect(authenticateDevice("Bearer cred-old", db)).resolves.toBe(DEVICE);

    // 轮换：先 upsert 新凭据，再把该设备其余凭据 retire。
    await publish(db, { credential: "cred-new", sourceSeq: 2 });
    await db
      .prepare(RETIRE_OTHER_DEVICE_CREDENTIALS)
      .bind(DEVICE, await credentialFingerprint("cred-new"), 2, 2)
      .run();

    await expect(authenticateDevice("Bearer cred-new", db)).resolves.toBe(DEVICE);
    await expect(authenticateDevice("Bearer cred-old", db)).rejects.toThrow();
  });

  it("never retires another device's credentials", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-other", deviceId: OTHER_DEVICE, sourceSeq: 1 });
    await publish(db, { credential: "cred-new", sourceSeq: 2 });
    await db
      .prepare(RETIRE_OTHER_DEVICE_CREDENTIALS)
      .bind(DEVICE, await credentialFingerprint("cred-new"), 2, 2)
      .run();

    await expect(authenticateDevice("Bearer cred-other", db)).resolves.toBe(OTHER_DEVICE);
  });

  it("suspending an annotator disables every credential it published for", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, { credential: "cred-a", sourceSeq: 1, annotatorStatus: "active" });
    await publish(db, { credential: "cred-b", deviceId: OTHER_DEVICE, sourceSeq: 2, annotatorStatus: "active" });

    await publish(db, { credential: "cred-a", sourceSeq: 3, annotatorStatus: "suspended" });
    await publish(db, {
      credential: "cred-b",
      deviceId: OTHER_DEVICE,
      sourceSeq: 4,
      annotatorStatus: "suspended",
    });

    await expect(authenticateDevice("Bearer cred-a", db)).rejects.toThrow();
    await expect(authenticateDevice("Bearer cred-b", db)).rejects.toThrow();
  });

  it("never authenticates a tombstoned annotator credential", async () => {
    const db = SqliteD1.createWithSchema();
    await publish(db, {
      credential: "cred-tombstoned",
      sourceSeq: 1,
      annotatorStatus: "tombstoned",
    });

    await expect(authenticateDevice("Bearer cred-tombstoned", db)).rejects.toThrow();
  });
});
