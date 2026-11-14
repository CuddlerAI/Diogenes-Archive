import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, describe, expect, it } from "vitest";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

const RETIREMENT = "0029_retire_certified_lane.sql";

async function migratedThrough28(): Promise<PGlite> {
  const db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files.filter((file) => Number(file.slice(0, 4)) <= 28)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  return db;
}

function retirementSql(): Promise<string> {
  return readFile(path.join(migrationsDirectory, RETIREMENT), "utf8");
}

/** 一行 0024 形状的 certified 会话，只带 CHECK 与停机检查看得见的字段。 */
async function forgeCertifiedSession(db: PGlite): Promise<string> {
  const deviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver,
       display_snapshot, input_devices_snapshot, client_version
     ) VALUES ($1, $2, 'some-profile', $3, 'active',
               '10.0.26100', '10DE:2684', '566.36', '{}'::jsonb, '[]'::jsonb, '0.7.1')`,
    [deviceId, `stale-${deviceId.slice(0, 8)}`, randomUUID().replace(/-/g, "").padEnd(64, "0")],
  );
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
     VALUES ('stale-game', 'Stale Game', 'game', 'first_person', 'rendered', true)`,
  );
  // status='approved' 要求 approved_at/approved_by 同在（0001）；game_id 只有
  // device_profile 那一 kind 允许为 NULL。这里逐条配齐，免得夹具因为别的约束
  // 挂掉，把"停机检查有没有生效"这件事糊掉。
  async function catalogVersion(kind: string, fill: string, gameId: string | null) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, content_hash, snapshot, game_id,
         status, approved_at, approved_by, approved_for_trainable
       ) VALUES ($1, $2::catalog_kind, $3, '1', $4, '{}'::jsonb, $5,
                 'approved', transaction_timestamp(), 'test-operator', false)`,
      [id, kind, `stale-${kind}`, `sha256:${fill.repeat(64)}`, gameId],
    );
    return id;
  }
  const catalogVersionId = await catalogVersion("device_profile", "a", null);
  const recordingVersionId = await catalogVersion("recording_profile", "b", "stale-game");
  const mappingVersionId = await catalogVersion("game_mapping", "c", "stale-game");

  const sessionId = randomUUID();
  await db.query(
    `INSERT INTO sessions(
       id, client_session_id, device_id, game_id, game_version, game_executable_sha256,
       catalog_id, catalog_version, catalog_mode, catalog_manifest_sha256, game_profile_sha256,
       device_catalog_version_id, recording_catalog_version_id, mapping_catalog_version_id,
       schema_version, manifest_json, manifest_sha256, server_data_classification, status,
       capture_engine, native_width, native_height, fps_num, fps_den, client_version, started_at
     ) VALUES (
       $1, $2, $3, 'stale-game', '1.0.0', $4,
       'stale-catalog', '1', 'certified', $5, $6,
       $7, $8, $9,
       'dm-certified-session-2', '{}'::jsonb, $10, 'non_trainable', 'registered',
       'native_wgc_window_v1', 1920, 1080, 30, 1, '0.7.1', '2026-06-01T00:00:00Z'
     )`,
    [
      sessionId,
      `stale-${sessionId.slice(0, 8)}`,
      deviceId,
      `sha256:${"d".repeat(64)}`,
      `sha256:${"e".repeat(64)}`,
      `sha256:${"f".repeat(64)}`,
      catalogVersionId,
      recordingVersionId,
      mappingVersionId,
      `sha256:${"1".repeat(64)}`,
    ],
  );
  return sessionId;
}

let open: PGlite[] = [];

afterAll(async () => {
  await Promise.all(open.map((db) => db.close()));
  open = [];
});

// 重放迁移的测试自带一个 vitest 默认 5s 关不住的成本：`migratedThrough()` 每次
// 起一个全新的 PGlite 实例、逐个重放三十多份迁移。本机实测冷跑那一条 3.9–4.0 s，
// 离 5 s 只剩一秒出头的余量；`pnpm test:workspace` 让 mirror / api / console-api
// 三个包同时跑时余量被吃光，实测约三次里红一次，且失败形态是超时——症状看起来像
// 「迁移逻辑错了」，指不到真正的原因。
//
// 这里按**文件**放宽而不是全包放宽：慢的是「重放迁移」这一类测试，其余三十来个
// 测试文件仍该守着 5 s，否则真卡住的测试就没人拦了。30 s 是实测冷跑的约 7 倍、
// 并行下最坏观测值（14 s）的两倍，够宽而仍能拦住真的挂起。
describe("certified lane retirement migration 0029", { timeout: 30_000 }, () => {
  it("records the migration and retires the certified vocabulary on a clean database", async () => {
    const db = await migratedThrough28();
    open.push(db);
    await db.exec(await retirementSql());

    const ledger = await db.query<{ name: string }>(
      `SELECT name FROM schema_migrations WHERE version = 29`,
    );
    expect(ledger.rows).toEqual([{ name: "retire_certified_lane" }]);

    // 词表确实收回了。逐条读 CHECK 的定义而不是靠一次插入试出来——插入会被
    // 别的约束（FK、NOT NULL）挡下，读出来的却只有一句 "constraint failed"。
    const sessionCheck = await db.query<{ definition: string }>(
      `SELECT pg_get_constraintdef(oid) AS definition
         FROM pg_constraint WHERE conname = 'sessions_local_catalog_evidence_check'`,
    );
    expect(sessionCheck.rows[0]!.definition).not.toContain("certified");
    expect(sessionCheck.rows[0]!.definition).toContain("dev_non_trainable");

    const runCheck = await db.query<{ definition: string }>(
      `SELECT pg_get_constraintdef(oid) AS definition
         FROM pg_constraint WHERE conname = 'capture_runs_preflight_reference_check'`,
    );
    expect(runCheck.rows[0]!.definition).not.toContain("certified_pass");
    expect(runCheck.rows[0]!.definition).toContain("dev_non_trainable_pass");

    // §4.3 的边界：视图与表都零触碰，收敛归下一刀。
    const view = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM pg_views WHERE viewname = 'clip_trainability'`,
    );
    expect(view.rows[0]!.count).toBe("1");
    const grants = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM pg_tables WHERE tablename = 'capture_grants'`,
    );
    expect(grants.rows[0]!.count).toBe("1");
  }, 120_000);

  // §0.2 的人工实测（生产 certified 零数据）不豁免这一步：只信一次人工确认，
  // 等于把"这条迁移在哪些库上安全"写进了一次对话而不是代码。存量在场时必须
  // 明确停住，并且**原样留着**——那是数据决定，不该由一条迁移替谁做主。
  it("refuses to retire the vocabulary while certified rows still exist", async () => {
    const db = await migratedThrough28();
    open.push(db);
    const sessionId = await forgeCertifiedSession(db);

    await expect(db.exec(await retirementSql())).rejects.toThrow(/refuses to retire/);

    // 迁移的 BEGIN 还开着，RAISE 之后事务处于 aborted 态。生产上 migrate.ts 会
    // 让整个进程带着这个错误退出；这里显式回滚，才能读到库的真实状态。
    await db.exec("ROLLBACK");

    // 迁移未记账、那一行分毫未动：人还能在原样的库上做决定。
    const ledger = await db.query(`SELECT 1 FROM schema_migrations WHERE version = 29`);
    expect(ledger.rows).toHaveLength(0);
    const survivor = await db.query<{ catalog_mode: string; schema_version: string }>(
      `SELECT catalog_mode, schema_version FROM sessions WHERE id = $1`,
      [sessionId],
    );
    expect(survivor.rows[0]).toEqual({
      catalog_mode: "certified",
      schema_version: "dm-certified-session-2",
    });
  }, 120_000);
});
