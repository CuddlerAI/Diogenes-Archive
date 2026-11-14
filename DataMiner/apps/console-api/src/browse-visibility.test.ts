import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { clipListQuerySchema, defaultTimeBasis } from "./browse-contracts.js";
import { PostgresBrowseRepository } from "./browse-repository.js";
import type { SqlClient } from "./sql.js";

/**
 * docs/67 的可见性修复：录制时间口径、采集时长、逐段健康、run 结果、session
 * 根快照与客户端遥测。自带一套数据库而不是挂进 browse-repository.test.ts —— 那
 * 边的聚合断言钉的是精确计数，任何新行都会把它们打挂。
 *
 * 这里的关键 fixture 是「录制在前、登记在后」：run 起于 3 天前，clip 行今天才
 * 落库。两个口径因此必然落在不同的两天，图上按哪个分桶就成了可判定的事实。
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
  clipClean: randomUUID(),
  clipUnhealthy: randomUUID(),
  clipDupExcess: randomUUID(),
};

const digest = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;
const RECORDED_DAYS_AGO = 3;

async function insertClip(
  id: string,
  segmentSeq: number,
  health: Record<string, number>,
): Promise<void> {
  await db.query(
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, health_json, meta_json,
                       manifest_json, manifest_sha256, ingest_state, verification_state,
                       retention_deadline, stored_at, verified_at, created_at)
     VALUES ($1, $2, $3, 0, $4, 0, 300, 1000, 2000, 42, $5::jsonb, '{"schema":"dm-1"}'::jsonb,
             '{"files":5}'::jsonb, $6, 'stored', 'verified',
             transaction_timestamp() + interval '30 days', transaction_timestamp(),
             transaction_timestamp(), transaction_timestamp())`,
    [id, ids.session, ids.run, segmentSeq, JSON.stringify(health), digest("7")],
  );
  await db.query(
    `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                  expected_size_bytes, storage_state, verification_state, stored_at,
                                  verified_at)
     VALUES ($1, $2, 'video', 'diogenes-model-training', $3, $4, 1000, 'stored', 'verified',
             transaction_timestamp(), transaction_timestamp())`,
    [randomUUID(), id, `raw/v1/certified/dataminer-api-production/${id}/video.mp4`, digest("8")],
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
    `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                         credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                         display_snapshot, input_devices_snapshot, client_version, last_seen_at)
     VALUES ($1, 'dev-a', $2, 'dm-device-1', $3, 'active', '10.0.26100', 'PCI', '566.36',
             '{}'::jsonb, '[]'::jsonb, '0.7.0', transaction_timestamp())`,
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
             '{}'::jsonb, $8, 'completed', 'native_wgc_window_v1', 1920, 1080, 60, 1, '0.7.0',
             transaction_timestamp() - make_interval(days => $9::int),
             transaction_timestamp() - make_interval(days => $9::int) + interval '20 minutes',
             'dm-local-dev', '1.0.0', 'dev_non_trainable', $10, $11,
             transaction_timestamp())`,
    [
      ids.session,
      ids.device,
      ids.annotator,
      digest("c"),
      ids.deviceProfile,
      ids.recordingProfile,
      ids.mapping,
      digest("d"),
      RECORDED_DAYS_AGO,
      digest("e"),
      digest("f"),
    ],
  );
  // 这个 run 带着结果事实：帧数、停止原因、错误码。0.7.0 之前这三列永远是 NULL。
  await db.query(
    `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, status, capture_engine,
                              encoder_backend, width, height, fps_num, fps_den, session_frame_start,
                              frame_count, stop_reason, error_code, started_at, ended_at,
                              preflight_schema_version, preflight_id, preflight_result,
                              preflight_evidence_sha256)
     VALUES ($1, $2, 'r0000', 0, 'faulted', 'native_wgc_window_v1', 'mf_nvidia_hardware_v1',
             1920, 1080, 60, 1, 0, 600, 'capture_fault', 'CAPTURE_QUEUE_DROPPED',
             transaction_timestamp() - make_interval(days => $3::int),
             transaction_timestamp() - make_interval(days => $3::int) + interval '20 minutes',
             'dm-preflight-evidence-1', $4, 'dev_non_trainable_pass', $5)`,
    [ids.run, ids.session, RECORDED_DAYS_AGO, randomUUID(), digest("9")],
  );
  // clean 段取生产形态而不是全零（docs/146）：300 帧的段 CFR 选帧下
  // `sourceSuperseded` ≈ 297、偶发 1 帧补帧（0.33% < 1%）都是健康常态。
  // 旧判定式会把它误判成异常——unhealthyOnly 的断言靠这组数值钉回归。
  await insertClip(ids.clipClean, 0, {
    duplicateFrames: 1,
    sourceSuperseded: 297,
    queueDrops: 0,
    inputOverflow: 0,
  });
  // 硬故障位：queueDrops 非零（且补帧 12/300 = 4% 也越线）。
  await insertClip(ids.clipUnhealthy, 1, {
    duplicateFrames: 12,
    sourceSuperseded: 1,
    queueDrops: 3,
    inputOverflow: 0,
  });
  // 只靠补帧占比越线（4/300 ≈ 1.33% > DUP_MAX_FRAC 的 1%）判异常：
  // 没有任何硬故障位，superseded 再大也不该影响判定。
  await insertClip(ids.clipDupExcess, 2, {
    duplicateFrames: 4,
    sourceSuperseded: 290,
    queueDrops: 0,
    inputOverflow: 0,
  });
  await db.query(
    `INSERT INTO session_artifacts(id, session_id, kind, object_key, content_type, expected_sha256,
                                   expected_size_bytes, ingest_state, verification_state, stored_at,
                                   verified_at)
     VALUES ($1, $2, 'mapping', $3, 'application/json', $4, 2048, 'stored', 'verified',
             transaction_timestamp(), transaction_timestamp())`,
    [randomUUID(), ids.session, `raw/v1/certified/x/${ids.session}/mapping.json`, digest("b")],
  );
  await db.query(
    `INSERT INTO client_events(event_id, device_id, annotator_id, session_id, occurred_at, kind,
                               severity, client_version, details)
     VALUES ($1, $2, $3, $4, transaction_timestamp() - interval '1 hour',
             'preflight.failed', 'error', '0.7.0', '{"detailsId":"pf-1"}'::jsonb),
            ($5, $2, $3, NULL, transaction_timestamp() - interval '2 hours',
             'app.started', 'info', '0.7.0', '{}'::jsonb)`,
    [randomUUID(), ids.device, ids.annotator, ids.session, randomUUID()],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("每日贡献的时间口径（docs/67 §3.3）", () => {
  it("默认按录制墙钟分桶，而不是上传登记那天", async () => {
    const contribution = await browse.contributionForAnnotator(ids.annotator, 7);

    expect(contribution.timeBasis).toBe("recorded");
    const recordedDay = contribution.daily.at(-1 - RECORDED_DAYS_AGO);
    const today = contribution.daily.at(-1);
    expect(recordedDay).toMatchObject({ clipCount: 3, sessionCount: 1 });
    expect(today).toMatchObject({ clipCount: 0 });
  });

  it("显式要登记口径时才回到入库那天", async () => {
    const contribution = await browse.contributionForAnnotator(ids.annotator, 7, "registered");

    expect(contribution.timeBasis).toBe("registered");
    expect(contribution.daily.at(-1)).toMatchObject({ clipCount: 3 });
    expect(contribution.daily.at(-1 - RECORDED_DAYS_AGO)).toMatchObject({ clipCount: 0 });
  });

  it("给出采集时长与异常片段数，而不是只有计数和字节", async () => {
    const contribution = await browse.contributionForAnnotator(ids.annotator, 7);

    // 3 段 × 300 帧 ÷ 60fps = 15 秒。配额是按小时给的，计数答不了这个问题。
    // 异常数是 2 不是 3：生产形态的 clean 段（superseded 297、补帧 1）不算异常
    // ——这正是 docs/146 修的那个 100% 误报。
    expect(contribution.totals.capturedSeconds).toBe(15);
    expect(contribution.totals.unhealthyClipCount).toBe(2);
    expect(contribution.daily.at(-1 - RECORDED_DAYS_AGO)).toMatchObject({
      capturedSeconds: 15,
      unhealthyClipCount: 2,
    });
  });
});

describe("逐段健康（docs/67 §3.4）", () => {
  it("片段行带上四个健康计数与录制时间", async () => {
    const page = await browse.listClips({ sessionId: ids.session, pageSize: 50 });

    const unhealthy = page.items.find((clip) => clip.id === ids.clipUnhealthy);
    expect(unhealthy?.health).toEqual({
      duplicateFrames: 12,
      sourceSuperseded: 1,
      queueDrops: 3,
      inputOverflow: 0,
    });
    // superseded 不参与异常判定，但作为诊断列必须原样可见（docs/146 的裁决）。
    const clean = page.items.find((clip) => clip.id === ids.clipClean);
    expect(clean?.health).toEqual({
      duplicateFrames: 1,
      sourceSuperseded: 297,
      queueDrops: 0,
      inputOverflow: 0,
    });
    expect(unhealthy?.recordedAt).not.toBeNull();
    expect(Date.parse(unhealthy!.recordedAt!)).toBeLessThan(Date.parse(unhealthy!.createdAt));
  });

  it("可以只列出健康异常的片段——CFR 正常计数的段不在其中", async () => {
    const page = await browse.listClips({ sessionId: ids.session, unhealthyOnly: true, pageSize: 50 });

    // 硬故障位（clipUnhealthy 的 queueDrops）与补帧占比越线（clipDupExcess 的
    // 4/300）都进筛选；生产形态的 clean 段（superseded 297、补帧 1/300）不进——
    // superseded 与 1% 以内的补帧不再是异常位（docs/146）。
    expect(page.items.map((clip) => clip.id).sort()).toEqual(
      [ids.clipUnhealthy, ids.clipDupExcess].sort(),
    );
  });

  it("按录制时间筛选片段，不是按登记时间", async () => {
    const recordedWindow = new Date(
      Date.now() - (RECORDED_DAYS_AGO + 1) * 86_400_000,
    ).toISOString();
    const afterRecording = new Date(Date.now() - 86_400_000).toISOString();

    const recorded = await browse.listClips({ from: recordedWindow, to: afterRecording, pageSize: 50 });
    const registered = await browse.listClips({
      from: recordedWindow,
      to: afterRecording,
      timeBasis: "registered",
      pageSize: 50,
    });

    expect(recorded.items).toHaveLength(3);
    expect(registered.items).toHaveLength(0);
  });
});

describe("run 结果与派生健康（docs/67 §3.2）", () => {
  it("会话详情读得到停止原因、错误码与帧数", async () => {
    const session = await browse.getSession(ids.session);

    expect(session?.runs[0]).toMatchObject({
      stopReason: "capture_fault",
      errorCode: "CAPTURE_QUEUE_DROPPED",
      frameCount: 600,
      status: "faulted",
    });
  });

  it("run 的健康是逐段合计，ready 段数按片段行现算", async () => {
    const session = await browse.getSession(ids.session);

    // capture_runs.health_json / ready_clip_count 没有写入者，所以这两个数只能
    // 从片段侧来；合计必须把三段都算上，而不是只看第一段。
    expect(session?.runs[0]?.health).toEqual({
      duplicateFrames: 17,
      sourceSuperseded: 588,
      queueDrops: 3,
      inputOverflow: 0,
    });
    expect(session?.runs[0]?.readyClipCount).toBe(3);
  });
});

describe("session 根快照与客户端遥测（docs/67 §3.6）", () => {
  it("列出 session_artifacts —— 摄取面一直在写，console 此前从不读", async () => {
    const artifacts = await browse.listSessionArtifacts(ids.session);

    expect(artifacts).toHaveLength(1);
    expect(artifacts[0]).toMatchObject({
      kind: "mapping",
      ingestState: "stored",
      verificationState: "verified",
      expectedSizeBytes: 2048,
    });
  });

  it("按设备与严重级筛选客户端遥测，按发生时间倒序", async () => {
    const all = await browse.listClientEvents({ deviceId: ids.device, pageSize: 50 });
    const errors = await browse.listClientEvents({ severity: "error", pageSize: 50 });

    expect(all.items.map((event) => event.kind)).toEqual(["preflight.failed", "app.started"]);
    expect(errors.items).toHaveLength(1);
    expect(errors.items[0]).toMatchObject({
      kind: "preflight.failed",
      sessionId: ids.session,
      details: { detailsId: "pf-1" },
    });
  });
});

describe("筛选参数的边界", () => {
  it("unhealthyOnly=false 表示不筛，而不是把语义反过来", () => {
    expect(clipListQuerySchema.parse({ unhealthyOnly: "false" }).unhealthyOnly).toBe(false);
    expect(clipListQuerySchema.parse({ unhealthyOnly: "true" }).unhealthyOnly).toBe(true);
    expect(clipListQuerySchema.parse({}).unhealthyOnly).toBeUndefined();
    // 认不出来的值不该被悄悄当成真——那正是 z.coerce.boolean() 的坑。
    expect(clipListQuerySchema.safeParse({ unhealthyOnly: "yes" }).success).toBe(false);
  });

  it("时间口径缺省为录制口径", () => {
    expect(clipListQuerySchema.parse({}).timeBasis).toBeUndefined();
    expect(defaultTimeBasis).toBe("recorded");
  });
});
