/**
 * 迁移 0036 的词表边界（issue #855 PR 1）。
 *
 * 三件事必须同时为真：
 * - `production_candidate` 改名 `training_candidate`，且 `clip_trainability` 的
 *   `source_non_trainable` 判据**跟着改了**——RENAME VALUE 按枚举 OID 传播，迁移里
 *   做的是断言传播已发生，不是重贴视图；
 * - 存量分类**一行都没动**；
 * - `dm-session-ingest-2` 可存，且**不得**携带退役的 `catalog_mode`。
 */
import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { describe, expect, it } from "vitest";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

async function migrationFiles(): Promise<string[]> {
  return (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
}

async function migratedThrough(highest: number): Promise<PGlite> {
  const db = new PGlite();
  for (const file of (await migrationFiles()).filter((f) => Number(f.slice(0, 4)) <= highest)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  return db;
}

async function applyMigration(db: PGlite, version: number): Promise<void> {
  const file = (await migrationFiles()).find((f) => Number(f.slice(0, 4)) === version);
  if (!file) throw new Error(`migration ${version} not found`);
  await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
}

/** 一台设备 + 一个游戏 + 四本已批准 catalog 行，够插一条 session。 */
async function seedSessionPrerequisites(db: PGlite): Promise<{
  deviceId: string;
  gameId: string;
  deviceCatalogId: string;
  recordingCatalogId: string;
  mappingCatalogId: string;
}> {
  const deviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver,
       display_snapshot, input_devices_snapshot, client_version
     ) VALUES ($1, $2, 'some-profile', $3, 'active',
               '10.0.26100', '10DE:2684', '566.36', '{}'::jsonb, '[]'::jsonb, '0.7.1')`,
    [deviceId, `mig36-${deviceId.slice(0, 8)}`, randomUUID().replace(/-/g, "").padEnd(64, "0")],
  );
  const gameId = "mig36-game";
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
     VALUES ($1, 'Migration 36 Game', 'game', 'first_person', 'rendered', true)`,
    [gameId],
  );
  async function catalogVersion(kind: string, fill: string, scopedGameId: string | null) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, content_hash, snapshot, game_id,
         status, approved_at, approved_by, approved_for_trainable
       ) VALUES ($1, $2::catalog_kind, $3, '1', $4, '{}'::jsonb, $5,
                 'approved', transaction_timestamp(), 'test-operator', false)`,
      [id, kind, `mig36-${kind}`, `sha256:${fill.repeat(64)}`, scopedGameId],
    );
    return id;
  }
  return {
    deviceId,
    gameId,
    deviceCatalogId: await catalogVersion("device_profile", "a", null),
    recordingCatalogId: await catalogVersion("recording_profile", "b", gameId),
    mappingCatalogId: await catalogVersion("game_mapping", "c", gameId),
  };
}

const SESSION_COLUMNS = `
  id, client_session_id, device_id, game_id, game_version,
  catalog_id, catalog_version, catalog_mode, catalog_manifest_sha256, game_profile_sha256,
  device_catalog_version_id, recording_catalog_version_id, mapping_catalog_version_id,
  schema_version, manifest_json, manifest_sha256,
  capture_engine, native_width, native_height, fps_num, fps_den,
  client_version, started_at, game_executable_sha256_missing_reason`;

async function insertSession(
  db: PGlite,
  seed: Awaited<ReturnType<typeof seedSessionPrerequisites>>,
  schemaVersion: string,
  catalogMode: string | null,
): Promise<string> {
  const id = randomUUID();
  await db.query(
    `INSERT INTO sessions(${SESSION_COLUMNS})
     VALUES ($1, $2, $3, $4, '1',
             'mig36-catalog', '1', $5, $6, $6,
             $7, $8, $9,
             $10, '{}'::jsonb, $6,
             'native_wgc_window_v1', 1920, 1080, 30, 1,
             '0.7.1', transaction_timestamp(), 'fixture has no pinned executable')`,
    [
      id,
      `mig36-${id.slice(0, 8)}`,
      seed.deviceId,
      seed.gameId,
      catalogMode,
      `sha256:${"d".repeat(64)}`,
      seed.deviceCatalogId,
      seed.recordingCatalogId,
      seed.mappingCatalogId,
      schemaVersion,
    ],
  );
  return id;
}

// 重放迁移的测试自带一个 vitest 默认 5s 关不住的成本：`migratedThrough()` 每次
// 起一个全新的 PGlite 实例、逐个重放三十多份迁移。本机实测冷跑那一条 3.9–4.0 s，
// 离 5 s 只剩一秒出头的余量；`pnpm test:workspace` 让 mirror / api / console-api
// 三个包同时跑时余量被吃光，实测约三次里红一次，且失败形态是超时——症状看起来像
// 「迁移逻辑错了」，指不到真正的原因。
//
// 这里按**文件**放宽而不是全包放宽：慢的是「重放迁移」这一类测试，其余三十来个
// 测试文件仍该守着 5 s，否则真卡住的测试就没人拦了。30 s 是实测冷跑的约 7 倍、
// 并行下最坏观测值（14 s）的两倍，够宽而仍能拦住真的挂起。
describe("migration 0036 — training_candidate + dm-session-ingest-2 vocabulary", { timeout: 30_000 }, () => {
  it("renames the enum value and carries clip_trainability with it", async () => {
    const db = await migratedThrough(35);
    try {
      const before = await db.query<{ d: string }>(
        `SELECT pg_get_viewdef('clip_trainability'::regclass) AS d`,
      );
      expect(before.rows[0]!.d).toContain("production_candidate");

      await applyMigration(db, 36);

      const labels = await db.query<{ enumlabel: string }>(
        `SELECT enumlabel FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
          WHERE t.typname = 'data_classification' ORDER BY enumsortorder`,
      );
      expect(labels.rows.map((row) => row.enumlabel)).toEqual([
        "non_trainable",
        "training_candidate",
      ]);

      // 视图**没有**被重贴，但它的 source_non_trainable 判据已经指向新 label：
      // RENAME VALUE 按枚举 OID 传播。迁移里的 DO 块断言的正是这一点。
      const after = await db.query<{ d: string }>(
        `SELECT pg_get_viewdef('clip_trainability'::regclass) AS d`,
      );
      expect(after.rows[0]!.d).not.toContain("production_candidate");
      expect(after.rows[0]!.d).toContain("training_candidate");
    } finally {
      await db.close();
    }
  });

  it("does not reclassify a single existing session", async () => {
    const db = await migratedThrough(35);
    try {
      const seed = await seedSessionPrerequisites(db);
      const sessionId = await insertSession(
        db,
        seed,
        "dm-local-dev-session-2",
        "dev_non_trainable",
      );
      await applyMigration(db, 36);
      const row = await db.query<{ c: string; schema_version: string; catalog_mode: string }>(
        `SELECT server_data_classification::text AS c, schema_version, catalog_mode
           FROM sessions WHERE id = $1`,
        [sessionId],
      );
      expect(row.rows[0]).toEqual({
        c: "non_trainable",
        schema_version: "dm-local-dev-session-2",
        catalog_mode: "dev_non_trainable",
      });
    } finally {
      await db.close();
    }
  });

  it("stores a dm-session-ingest-2 session only without the retired catalog mode", async () => {
    const db = await migratedThrough(36);
    try {
      const seed = await seedSessionPrerequisites(db);
      const sessionId = await insertSession(db, seed, "dm-session-ingest-2", null);
      const stored = await db.query<{ c: string }>(
        `SELECT server_data_classification::text AS c FROM sessions WHERE id = $1`,
        [sessionId],
      );
      // 默认仍是 non_trainable：本迁移只开词表，不发放分类。
      expect(stored.rows[0]!.c).toBe("non_trainable");

      // 退役词不得在新车道上复活。
      await expect(
        insertSession(db, seed, "dm-session-ingest-2", "dev_non_trainable"),
      ).rejects.toThrow(/sessions_local_catalog_evidence_check/);
      // 反过来，legacy 会话仍然必须带它。
      await expect(insertSession(db, seed, "dm-local-dev-session-2", null)).rejects.toThrow(
        /sessions_local_catalog_evidence_check/,
      );
      // 未知契约照旧被拒。
      await expect(insertSession(db, seed, "dm-certified-session-3", null)).rejects.toThrow(
        /sessions_local_catalog_evidence_check/,
      );
    } finally {
      await db.close();
    }
  });

  it("pairs each session generation with its own preflight vocabulary", async () => {
    const db = await migratedThrough(36);
    try {
      const seed = await seedSessionPrerequisites(db);
      const legacySession = await insertSession(
        db,
        seed,
        "dm-local-dev-session-2",
        "dev_non_trainable",
      );
      const neutralSession = await insertSession(db, seed, "dm-session-ingest-2", null);

      const insertRun = (sessionId: string, result: string) =>
        db.query(
          `INSERT INTO capture_runs(
             id, session_id, client_run_id, run_seq,
             capture_engine, encoder_backend, width, height, fps_num, fps_den,
             qpc_frequency, session_frame_start,
             preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
           ) VALUES ($1, $2, $3, 0,
                     'native_wgc_window_v1', 'mf_nvidia_hardware_v1', 1920, 1080, 30, 1,
                     10000000, 0,
                     'dm-preflight-evidence-1', $4, $5, $6)`,
          [
            randomUUID(),
            sessionId,
            `run-${randomUUID().slice(0, 8)}`,
            `pf-${randomUUID().slice(0, 8)}`,
            result,
            `sha256:${"e".repeat(64)}`,
          ],
        );

      await expect(insertRun(legacySession, "dev_non_trainable_pass")).resolves.toBeDefined();
      await expect(insertRun(neutralSession, "preflight_pass")).resolves.toBeDefined();
      // 交叉配对必须被拒：一条车道的 run 不得带另一条车道的结果串。
      await expect(insertRun(legacySession, "preflight_pass")).rejects.toThrow(
        /preflight evidence of its own lane/,
      );
      await expect(insertRun(neutralSession, "dev_non_trainable_pass")).rejects.toThrow(
        /preflight evidence of its own lane/,
      );
    } finally {
      await db.close();
    }
  });
});
