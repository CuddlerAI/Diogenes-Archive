import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { PostgresBrowseRepository } from "./browse-repository.js";
import type { SqlClient } from "./sql.js";

/**
 * docs/153：游戏分布普查。与 `browse-output-report.test.ts` 同一理由自带一套数据库——
 * 断言钉的是精确计数，共享 fixture 的任何新行都会打挂它们。普查没有时间轴，
 * 所以时间戳用固定字面量即可，不需要锚到「几天前」。
 */
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let browse: PostgresBrowseRepository;

const recordedAt = "2026-08-01T12:00:00+08:00";

const ids = {
  annotator: randomUUID(),
  deviceProfile: randomUUID(),
  recordingProfileCod: randomUUID(),
  mappingCod: randomUUID(),
  recordingProfileEts: randomUUID(),
  mappingEts: randomUUID(),
  device: randomUUID(),
  sessionCodA: randomUUID(),
  sessionCodB: randomUUID(),
  /** 唯一一段片段已 purged 的会话：段和会话都必须从普查里消失。 */
  sessionCodPurged: randomUUID(),
  sessionEts: randomUUID(),
  runCodA: randomUUID(),
  runCodB: randomUUID(),
  runCodPurged: randomUUID(),
  runEts: randomUUID(),
};

const digest = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;

async function insertSession(
  id: string,
  gameId: string,
  clientSessionId: string,
): Promise<void> {
  await db.query(
    `INSERT INTO sessions(id, client_session_id, device_id, annotator_id, game_id, game_version,
                          game_executable_sha256, device_catalog_version_id,
                          recording_catalog_version_id, mapping_catalog_version_id, schema_version,
                          manifest_json, manifest_sha256, status, capture_engine, native_width,
                          native_height, fps_num, fps_den, client_version, started_at, ended_at,
                          catalog_id, catalog_version, catalog_mode, catalog_manifest_sha256,
                          game_profile_sha256, created_at)
     VALUES ($1, $2, $3, $4, $5, '1.0', $6, $7, $8, $9, 'dm-local-dev-session-2',
             '{}'::jsonb, $10, 'completed', 'native_wgc_window_v1', 1920, 1080, 60, 1, '0.7.0',
             $11, $11::timestamptz + interval '20 minutes',
             'dm-local-dev', '1.0.0', 'dev_non_trainable', $12, $13, $11)`,
    [
      id,
      clientSessionId,
      ids.device,
      ids.annotator,
      gameId,
      digest("c"),
      ids.deviceProfile,
      gameId === "cod" ? ids.recordingProfileCod : ids.recordingProfileEts,
      gameId === "cod" ? ids.mappingCod : ids.mappingEts,
      digest("d"),
      recordedAt,
      digest("e"),
      digest("f"),
    ],
  );
}

async function insertRun(id: string, sessionId: string): Promise<void> {
  await db.query(
    `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, status, capture_engine,
                              encoder_backend, width, height, fps_num, fps_den, session_frame_start,
                              started_at, ended_at, preflight_schema_version, preflight_id,
                              preflight_result, preflight_evidence_sha256)
     VALUES ($1, $2, 'r0', 0, 'completed', 'native_wgc_window_v1', 'mf_nvidia_hardware_v1',
             1920, 1080, 60, 1, 0, $3, $3::timestamptz + interval '10 minutes',
             'dm-preflight-evidence-1', $4, 'dev_non_trainable_pass', $5)`,
    [id, sessionId, recordedAt, randomUUID(), digest("9")],
  );
}

/** 每段 300 帧 ÷ 60fps = 5 秒。计数与秒数都因此可以精确断言。 */
async function insertClip(
  sessionId: string,
  runId: string,
  segmentSeq: number,
  retentionState = "retained",
): Promise<void> {
  await db.query(
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, health_json, meta_json,
                       manifest_json, manifest_sha256, ingest_state, verification_state,
                       retention_state, retention_deadline, stored_at, verified_at, purged_at,
                       created_at)
     VALUES ($1, $2, $3, 0, $4, 0, 300, 1000, 2000, 42,
             '{"duplicateFrames":0,"sourceSuperseded":0,"queueDrops":0,"inputOverflow":0}'::jsonb,
             '{"schema":"dm-1"}'::jsonb, '{"files":5}'::jsonb, $5, 'stored', 'verified',
             $6, $7::timestamptz + interval '30 days', $7, $7, $8, $7)`,
    [
      randomUUID(),
      sessionId,
      runId,
      segmentSeq,
      digest("7"),
      retentionState,
      recordedAt,
      retentionState === "purged" ? recordedAt : null,
    ],
  );
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  browse = new PostgresBrowseRepository(db as unknown as SqlClient);

  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
     VALUES ('cod', 'Call of Duty', 'game', 'first_person', 'rendered', true),
            ('ets2', 'Euro Truck Simulator 2', 'game', 'first_person', 'rendered', true)`,
  );
  for (const [id, kind, catalogId, gameId] of [
    [ids.deviceProfile, "device_profile", "dm-device-1", null],
    [ids.recordingProfileCod, "recording_profile", "dm-recording-1", "cod"],
    [ids.mappingCod, "game_mapping", "dm-mapping-1", "cod"],
    [ids.recordingProfileEts, "recording_profile", "dm-recording-2", "ets2"],
    [ids.mappingEts, "game_mapping", "dm-mapping-2", "ets2"],
  ] as [string, string, string, string | null][]) {
    await db.query(
      `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash,
                                            snapshot, status)
       VALUES ($1, $2, $3, '1.0.0', $4, $5, '{}'::jsonb, 'draft')`,
      [id, kind, catalogId, gameId, digest("a")],
    );
  }
  await db.query(`INSERT INTO annotators(id, alias, status) VALUES ($1, 'alpha', 'active')`, [
    ids.annotator,
  ]);
  await db.query(
    `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                         credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                         display_snapshot, input_devices_snapshot, client_version, last_seen_at)
     VALUES ($1, 'dev-a', $2, 'dm-device-1', $3, 'active', '10.0.26100', 'PCI', '566.36',
             '{}'::jsonb, '[]'::jsonb, '0.7.0', transaction_timestamp())`,
    [ids.device, ids.annotator, "c".repeat(64)],
  );

  await insertSession(ids.sessionCodA, "cod", "client-1");
  await insertSession(ids.sessionCodB, "cod", "client-2");
  await insertSession(ids.sessionCodPurged, "cod", "client-3");
  await insertSession(ids.sessionEts, "ets2", "client-4");

  await insertRun(ids.runCodA, ids.sessionCodA);
  await insertRun(ids.runCodB, ids.sessionCodB);
  await insertRun(ids.runCodPurged, ids.sessionCodPurged);
  await insertRun(ids.runEts, ids.sessionEts);

  // cod：会话 A 两段 + 会话 B 一段；ets2 一段。purged 会话的唯一一段已删。
  await insertClip(ids.sessionCodA, ids.runCodA, 0);
  await insertClip(ids.sessionCodA, ids.runCodA, 1);
  await insertClip(ids.sessionCodB, ids.runCodB, 0);
  await insertClip(ids.sessionCodPurged, ids.runCodPurged, 0, "purged");
  await insertClip(ids.sessionEts, ids.runEts, 0);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("游戏分布普查（docs/153）", () => {
  it("每游戏一行、按时长降序，label 取 games.title，计数精确", async () => {
    const census = await browse.gameDistribution();

    expect(census.rows.map((row) => row.gameId)).toEqual(["cod", "ets2"]);
    expect(census.rows[0]).toMatchObject({
      gameId: "cod",
      title: "Call of Duty",
      capturedSeconds: 15,
      sessionCount: 2,
      clipCount: 3,
    });
    expect(census.rows[1]).toMatchObject({
      gameId: "ets2",
      title: "Euro Truck Simulator 2",
      capturedSeconds: 5,
      sessionCount: 1,
      clipCount: 1,
    });
  });

  it("purged 的段连同只剩它的会话一起从存量里消失", async () => {
    const census = await browse.gameDistribution();

    // cod 库里实际有 3 场会话、4 段片段；purged 那场的段与会话都不计入存量。
    const cod = census.rows.find((row) => row.gameId === "cod");
    expect(cod?.sessionCount).toBe(2);
    expect(cod?.clipCount).toBe(3);
  });

  it("合计等于逐行相加：game 划分 session，会话数在这张表里可加", async () => {
    const census = await browse.gameDistribution();

    expect(census.totals.capturedSeconds).toBe(20);
    expect(census.totals.sessionCount).toBe(3);
    expect(census.totals.clipCount).toBe(4);
    expect(census.totals.sessionCount).toBe(
      census.rows.reduce((total, row) => total + row.sessionCount, 0),
    );
  });

  it("dev_non_trainable 语料回看可训练恒为零；asOf 是可解析的时刻", async () => {
    const census = await browse.gameDistribution();

    expect(census.totals.trainableClipCount).toBe(0);
    expect(census.totals.trainableSeconds).toBe(0);
    expect(Number.isNaN(Date.parse(census.asOf))).toBe(false);
  });
});
