import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { PostgresBrowseRepository } from "./browse-repository.js";
import type { SqlClient } from "./sql.js";

/**
 * depth 作为 clip 第七件产物在 console 上的可见性（docs/105、docs/109 G12）。
 *
 * # 为什么自带一套数据库
 *
 * `dailyIntake` 与 `pipelineOverview` 都是**全库聚合**，钉的是精确计数。
 * 往 `browse-daily-intake.test.ts` 的共享 fixture 上挂一件 depth 产物，
 * 会立刻打挂那边 `storedBytes: 4000` 一类的断言——实测确认过。那不是那些断言
 * 写得脆，正是它们该有的样子（该文件头注：「全局聚合对共享 fixture 的新行零容忍」）。
 * 所以这里独立建库，与它同一纪律。
 *
 * # 这组测试要钉住什么
 *
 * 一条不变式：**depth 的读数是 `storedBytes` 的子集，不是它之外的另一份**。
 * 这不是洁癖——把两个数相加会重复计算，而 depth 实测约为 video 的两倍
 * （docs/109 §4.2），加错一次就能让存储读数虚高三倍。
 */
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let browse: PostgresBrowseRepository;

const ids = {
  annotator: randomUUID(),
  deviceProfile: randomUUID(),
  recordingProfile: randomUUID(),
  mapping: randomUUID(),
  device: randomUUID(),
  session: randomUUID(),
  run: randomUUID(),
};

const digest = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;

/** 全部落在「今天」的东八区日历里——本组不测天界，那是 docs/97 那一组的事。 */
const recordedAt = new Date(Date.now() - 3_600_000).toISOString();

const VIDEO_BYTES = 1000;
const DEPTH_BYTES = 2000;

/**
 * 建一个片段；`depthBytes` 给了才挂 depth 产物。
 *
 * **默认不挂**是刻意的：depth 是可选产物，绝大多数片段没有它。若基线是「都有
 * depth」，`depthClipCount` 就恒等于 `clipCount`，这两列永远分不出对错。
 */
async function insertClip(segmentSeq: number, depthBytes?: number): Promise<string> {
  const clipId = randomUUID();
  await db.query(
    // run_seq 恒为 0：`clips_capture_run_id_session_id_run_seq_fkey` 要求它与所属 run
    // 的 run_seq 相等。段与段的区别在 segment_seq 上，不在 run_seq 上。
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, health_json, meta_json,
                       manifest_json, manifest_sha256, ingest_state, verification_state,
                       retention_deadline, stored_at, verified_at, created_at)
     VALUES ($1, $2, $3, 0, $4, 0, 300, 1000, 2000, 42,
             '{"duplicateFrames":0,"sourceSuperseded":0,"queueDrops":0,"inputOverflow":0}'::jsonb,
             '{"schema":"dm-1"}'::jsonb, '{"files":5}'::jsonb, $5, 'stored', 'verified',
             $6::timestamptz + interval '30 days', $6, $6, $6)`,
    [clipId, ids.session, ids.run, segmentSeq, digest("7"), recordedAt],
  );
  const artifact = async (kind: string, bytes: number, state: string) => {
    await db.query(
      `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                    expected_size_bytes, storage_state, verification_state,
                                    stored_at, verified_at)
       VALUES ($1, $2, $3, 'diogenes-model-training', $4, $5, $6, $7, 'verified',
               transaction_timestamp(), transaction_timestamp())`,
      [
        randomUUID(),
        clipId,
        kind,
        `raw/v1/non-trainable/dataminer-api-production/${clipId}/${kind}`,
        digest("8"),
        bytes,
        state,
      ],
    );
  };
  await artifact("video", VIDEO_BYTES, "stored");
  if (depthBytes !== undefined) await artifact("depth", depthBytes, "stored");
  return clipId;
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
     VALUES ('cod', 'Call of Duty', 'game', 'first_person', 'rendered', true)`,
  );
  for (const [id, kind, catalogId, gameId] of [
    [ids.deviceProfile, "device_profile", "dm-device-1", null],
    [ids.recordingProfile, "recording_profile", "dm-recording-1", "cod"],
    [ids.mapping, "game_mapping", "dm-mapping-1", "cod"],
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
    // certified_profile_id 是 catalog_id 文本（迁移 0021 换掉了版本外键，0027 又放开
    // NOT NULL），不是 trusted_catalog_versions.id。
    `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                         credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                         display_snapshot, input_devices_snapshot, client_version, created_at)
     VALUES ($1, 'dev-a', $2, 'dm-device-1', $3, 'active', '10.0.26100', 'PCI', '566.36',
             '{}'::jsonb, '[]'::jsonb, '0.7.2', transaction_timestamp())`,
    [ids.device, ids.annotator, "c".repeat(64)],
  );
  await db.query(
    `INSERT INTO sessions(id, client_session_id, device_id, annotator_id, game_id, game_version,
                          game_executable_sha256, device_catalog_version_id,
                          recording_catalog_version_id, mapping_catalog_version_id, schema_version,
                          manifest_json, manifest_sha256, status, capture_engine, native_width,
                          native_height, fps_num, fps_den, client_version, started_at, ended_at,
                          catalog_id, catalog_version, catalog_mode, catalog_manifest_sha256,
                          game_profile_sha256, created_at)
     VALUES ($1, 'client-1', $2, $3, 'cod', '1.0', $4, $5, $6, $7, 'dm-local-dev-session-2',
             '{}'::jsonb, $8, 'completed', 'native_wgc_window_v1', 1920, 1080, 30, 1, '0.7.2',
             $9, $9::timestamptz + interval '20 minutes',
             'dm-local-dev', '1.0.0', 'dev_non_trainable', $10, $11, $9)`,
    [
      ids.session,
      ids.device,
      ids.annotator,
      digest("c"),
      ids.deviceProfile,
      ids.recordingProfile,
      ids.mapping,
      digest("d"),
      recordedAt,
      digest("e"),
      digest("f"),
    ],
  );
  await db.query(
    `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, status, capture_engine,
                              encoder_backend, width, height, fps_num, fps_den, session_frame_start,
                              started_at, ended_at, preflight_schema_version, preflight_id,
                              preflight_result, preflight_evidence_sha256)
     VALUES ($1, $2, 'r0', 0, 'completed', 'native_wgc_window_v1', 'mf_nvidia_hardware_v1',
             1920, 1080, 30, 1, 0, $3, $3::timestamptz + interval '10 minutes',
             'dm-preflight-evidence-1', $4, 'dev_non_trainable_pass', $5)`,
    [ids.run, ids.session, recordedAt, randomUUID(), digest("9")],
  );

  // 三段：只有第一段带 depth。这个比例就是现实——生产至今只有 1 件 depth 产物。
  await insertClip(0, DEPTH_BYTES);
  await insertClip(1);
  await insertClip(2);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("每日进线分得出 depth（docs/109 G12）", () => {
  it("带 depth 的片段单独计数，不等于片段总数", async () => {
    const intake = await browse.dailyIntake(7, "recorded", "Asia/Shanghai");
    expect(intake.totals.clipCount).toBe(3);
    expect(intake.totals.depthClipCount).toBe(1);
  });

  it("depthBytes 是 storedBytes 的子集——相加即重复计算", async () => {
    const intake = await browse.dailyIntake(7, "recorded", "Asia/Shanghai");
    expect(intake.totals.storedBytes).toBe(VIDEO_BYTES * 3 + DEPTH_BYTES);
    expect(intake.totals.depthBytes).toBe(DEPTH_BYTES);
    expect(intake.totals.depthBytes).toBeLessThan(intake.totals.storedBytes);
  });

  it("没有数据的那些天报 0 而不是缺字段——补零轴必须覆盖新列", async () => {
    const intake = await browse.dailyIntake(7, "recorded", "Asia/Shanghai");
    const empty = intake.daily.filter((day) => day.clipCount === 0);
    expect(empty.length).toBeGreaterThan(0);
    for (const day of empty) {
      expect(day.depthClipCount).toBe(0);
      expect(day.depthBytes).toBe(0);
    }
  });
});

describe("总览面分得出 depth", () => {
  it("件数按 clip 去重、字节只数已落地的", async () => {
    const overview = await browse.pipelineOverview();
    // 三段各 300 帧 @30fps = 10s；只有第一段带 depth。
    expect(overview.depth).toEqual({ clips: 1, bytes: DEPTH_BYTES, seconds: 10 });
    expect(overview.depth.bytes).toBeLessThan(overview.storedBytes);
    expect(overview.depth.seconds).toBeLessThan(overview.clipSeconds.total);
  });

  it("未落地的 depth 不进读数——`pending` 的字节不该出现在存储量里", async () => {
    const clipId = randomUUID();
    await db.query(
      `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                         n_frames, qpc_start, qpc_end, event_count, health_json, meta_json,
                         manifest_json, manifest_sha256, ingest_state, verification_state,
                         retention_deadline, created_at)
       VALUES ($1, $2, $3, 0, 9, 0, 300, 1000, 2000, 0,
               '{}'::jsonb, '{}'::jsonb, '{}'::jsonb, $4, 'uploading', 'unverified',
               transaction_timestamp() + interval '30 days', transaction_timestamp())`,
      [clipId, ids.session, ids.run, digest("7")],
    );
    await db.query(
      // storage_state='pending'（还没落 R2）；verification_state 的未校验值是
      // 'unverified'，不是 'pending'——两个枚举的词表不同，写混会被 PG 直接拒绝。
      `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                    expected_size_bytes, storage_state, verification_state)
       VALUES ($1, $2, 'depth', 'diogenes-model-training', $3, $4, 999999, 'pending', 'unverified')`,
      [randomUUID(), clipId, `raw/v1/non-trainable/x/${clipId}/depth`, digest("8")],
    );

    const overview = await browse.pipelineOverview();
    // 那 999999 字节还在路上：件数、字节与时长都不该动。
    expect(overview.depth).toEqual({ clips: 1, bytes: DEPTH_BYTES, seconds: 10 });
    // 但那一段本身确实进了库，所以片段总时长要看得见它——「depth 没落地」和
    // 「这段没采到」是两回事。
    expect(overview.clips.total).toBe(4);
    expect(overview.clipSeconds.total).toBe(40);
    expect(overview.clipSeconds.uploading).toBe(10);
  });
});
