import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  DEVICE_HEARTBEAT_THROTTLE_SECONDS,
  touchDeviceLastSeenSql,
} from "./repository.js";

/**
 * 设备心跳（docs/109 G4）：`devices.last_seen_at` 的写入方。
 *
 * # 这条测试存在的理由
 *
 * 这一列从 0001 就有定义，还带着 `devices(annotator_id, status, last_seen_at DESC)`
 * 索引，`pipelineOverview` 也一直拿它算「24h / 7d 活跃设备」——但**全仓从来没有
 * 任何写入方**，所以生产库里 7 台设备的这一列全是 NULL，总览页那两个磁贴恒为 0。
 * 运营因此分不清「这个人今天没干活」与「这个人的客户端每次都被拒」。
 *
 * 所以这里打的是**真实 schema 上的真实语句**（同 migration-*.test.ts 的做法），
 * 而不是 mock：一条 UPDATE 写没写进去、节流谓词挡不挡得住，只有数据库能回答。
 */
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;
const deviceId = randomUUID();
const annotatorId = randomUUID();
const profileVersionId = randomUUID();

const touch = () => db.query(touchDeviceLastSeenSql, [deviceId, DEVICE_HEARTBEAT_THROTTLE_SECONDS]);

const lastSeen = async (): Promise<Date | null> => {
  const { rows } = await db.query<{ last_seen_at: Date | null }>(
    `SELECT last_seen_at FROM devices WHERE id = $1`,
    [deviceId],
  );
  return rows[0]?.last_seen_at ?? null;
};

/** 直接把 last_seen_at 推到过去，模拟「上次心跳是很久以前」。 */
const backdateLastSeen = (seconds: number) =>
  db.query(
    `UPDATE devices SET last_seen_at = transaction_timestamp() - make_interval(secs => $2::int)
      WHERE id = $1`,
    [deviceId, seconds],
  );

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  await db.query(
    `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash,
                                          snapshot, status)
     VALUES ($1, 'device_profile', 'dm-device-1', '1.0.0', NULL, $2, '{}'::jsonb, 'draft')`,
    [profileVersionId, `sha256:${"a".repeat(64)}`],
  );
  await db.query(`INSERT INTO annotators(id, alias, status) VALUES ($1, 'alpha', 'active')`, [
    annotatorId,
  ]);
  await db.query(
    `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                         credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                         display_snapshot, input_devices_snapshot, client_version)
     VALUES ($1, 'dev-a', $2, 'dm-device-1', $3, 'active', '10.0.26100', 'PCI', '566.36',
             '{}'::jsonb, '[]'::jsonb, '0.7.2')`,
    [deviceId, annotatorId, "c".repeat(64)],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("设备心跳（docs/109 G4）", () => {
  it("入库时 last_seen_at 是 NULL——这正是修复前的全库状态", async () => {
    expect(await lastSeen()).toBeNull();
  });

  it("第一次心跳把 NULL 变成时间戳", async () => {
    await touch();
    const seen = await lastSeen();
    expect(seen).not.toBeNull();
    expect(Date.now() - (seen as Date).getTime()).toBeLessThan(60_000);
  });

  it("节流窗口内重复调用不再写——读路径不该变成写路径", async () => {
    await touch();
    const first = await lastSeen();
    const { affectedRows } = await db.query(touchDeviceLastSeenSql, [
      deviceId,
      DEVICE_HEARTBEAT_THROTTLE_SECONDS,
    ]);
    // 匹配 0 行才是对的：节流谓词写在 WHERE 里，由数据库判定，
    // 所以两个实例并发时也只会有一个写进去。
    expect(affectedRows).toBe(0);
    expect((await lastSeen())?.getTime()).toBe(first?.getTime());
  });

  it("超过节流窗口后再次写入", async () => {
    await backdateLastSeen(DEVICE_HEARTBEAT_THROTTLE_SECONDS + 60);
    const stale = await lastSeen();
    const { affectedRows } = await db.query(touchDeviceLastSeenSql, [
      deviceId,
      DEVICE_HEARTBEAT_THROTTLE_SECONDS,
    ]);
    expect(affectedRows).toBe(1);
    const fresh = await lastSeen();
    expect((fresh as Date).getTime()).toBeGreaterThan((stale as Date).getTime());
  });

  it("只动被点名的那台设备", async () => {
    const otherId = randomUUID();
    await db.query(
      `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                           credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                           display_snapshot, input_devices_snapshot, client_version)
       VALUES ($1, 'dev-b', $2, 'dm-device-1', $3, 'active', '10.0.26100', 'PCI', '566.36',
               '{}'::jsonb, '[]'::jsonb, '0.7.2')`,
      [otherId, annotatorId, "d".repeat(64)],
    );
    await backdateLastSeen(DEVICE_HEARTBEAT_THROTTLE_SECONDS + 60);
    await touch();

    const { rows } = await db.query<{ last_seen_at: Date | null }>(
      `SELECT last_seen_at FROM devices WHERE id = $1`,
      [otherId],
    );
    expect(rows[0]?.last_seen_at ?? null).toBeNull();
  });

  it("节流窗口是 5 分钟——改这个数会同时改写路径成本与「谁在线」的分辨率", () => {
    expect(DEVICE_HEARTBEAT_THROTTLE_SECONDS).toBe(300);
  });
});
