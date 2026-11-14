import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { defaultStatsTimeZone, statsDailyQuerySchema } from "./browse-contracts.js";
import { PostgresBrowseRepository } from "./browse-repository.js";
import type { SqlClient } from "./sql.js";

/**
 * docs/97：全局每日进线。自带一套数据库——`dailyIntake` 是全库聚合，任何共享
 * fixture 的新行都会打挂钉精确计数的断言（browse-visibility.test.ts 头注的同一
 * 理由，对全局聚合成立得更彻底）。
 *
 * 时间全部以东八区日历锚定。关键 fixture 是「北京 01:00 录制」：该时刻在
 * Asia/Shanghai 与 UTC 属于两个不同的自然天，分桶时区因此成为可判定事实
 * （docs/97 §6）。锚定在两天前而不是今天——「北京的今天」在 UTC 窗口里可能
 * 尚未开始，锚到今天的断言会随测试运行时刻漂移。
 */
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let browse: PostgresBrowseRepository;

/** 北京日历的日期标签（YYYY-MM-DD）。en-CA 的日期格式恰好是 ISO 形状。 */
const shanghaiDay = (daysAgo: number): string =>
  new Intl.DateTimeFormat("en-CA", { timeZone: "Asia/Shanghai" }).format(
    new Date(Date.now() - daysAgo * 86_400_000),
  );

const BASE = shanghaiDay(2);
const BASE_NEXT = shanghaiDay(1);
/** 北京 01:00 = UTC 前一日 17:00——两个时区分桶必然不同的那个时刻。 */
const crossInstant = `${BASE}T01:00:00+08:00`;
const crossUtcDay = new Date(Date.parse(crossInstant)).toISOString().slice(0, 10);
/** 北京正午 = UTC 04:00，两个时区同一天标签。 */
const noonInstant = `${BASE}T12:00:00+08:00`;
const registeredAt = `${BASE}T13:00:00+08:00`;
/** 晚一天登记：recorded 与 registered 两口径的分叉日。 */
const lateRegisteredAt = `${BASE_NEXT}T12:00:00+08:00`;

const ids = {
  annotatorAlpha: randomUUID(),
  annotatorBravo: randomUUID(),
  deviceProfile: randomUUID(),
  recordingProfile: randomUUID(),
  mapping: randomUUID(),
  deviceA: randomUUID(),
  deviceB: randomUUID(),
  sessionAlpha: randomUUID(),
  sessionBravo: randomUUID(),
  sessionAnonymous: randomUUID(),
  runCross: randomUUID(),
  runNoon: randomUUID(),
  runBravo: randomUUID(),
  runAnonymous: randomUUID(),
};

const digest = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;

async function insertSession(
  id: string,
  deviceId: string,
  annotatorId: string | null,
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
     VALUES ($1, $2, $3, $4, 'cod', '1.0', $5, $6, $7, $8, 'dm-local-dev-session-2',
             '{}'::jsonb, $9, 'completed', 'native_wgc_window_v1', 1920, 1080, 60, 1, '0.7.0',
             $10, $10::timestamptz + interval '20 minutes',
             'dm-local-dev', '1.0.0', 'dev_non_trainable', $11, $12, $10)`,
    [
      id,
      clientSessionId,
      deviceId,
      annotatorId,
      digest("c"),
      ids.deviceProfile,
      ids.recordingProfile,
      ids.mapping,
      digest("d"),
      noonInstant,
      digest("e"),
      digest("f"),
    ],
  );
}

async function insertRun(
  id: string,
  sessionId: string,
  runSeq: number,
  startedAt: string,
): Promise<void> {
  await db.query(
    `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, status, capture_engine,
                              encoder_backend, width, height, fps_num, fps_den, session_frame_start,
                              started_at, ended_at, preflight_schema_version, preflight_id,
                              preflight_result, preflight_evidence_sha256)
     VALUES ($1, $2, $3, $4, 'completed', 'native_wgc_window_v1', 'mf_nvidia_hardware_v1',
             1920, 1080, 60, 1, 0, $5, $5::timestamptz + interval '10 minutes',
             'dm-preflight-evidence-1', $6, 'dev_non_trainable_pass', $7)`,
    [id, sessionId, `r${runSeq}`, runSeq, startedAt, randomUUID(), digest("9")],
  );
}

async function insertClip(
  sessionId: string,
  runId: string,
  runSeq: number,
  createdAt: string,
): Promise<void> {
  const clipId = randomUUID();
  // 健康计数取生产形态而不是全零（docs/146）：CFR 选帧下 300 帧的段
  // `sourceSuperseded` ≈ 297、偶发 1 帧补帧都是健康常态。全零 fixture 兜不住
  // 「把正常计数当异常位」的判定式回归——下面各处 `unhealthyClipCount: 0`
  // 的断言正是靠这组数值钉住的（旧判定式会把 4 段全部误判成异常）。
  await db.query(
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, health_json, meta_json,
                       manifest_json, manifest_sha256, ingest_state, verification_state,
                       retention_deadline, stored_at, verified_at, created_at)
     VALUES ($1, $2, $3, $4, 0, 0, 300, 1000, 2000, 42,
             '{"duplicateFrames":1,"sourceSuperseded":297,"queueDrops":0,"inputOverflow":0}'::jsonb,
             '{"schema":"dm-1"}'::jsonb, '{"files":5}'::jsonb, $5, 'stored', 'verified',
             $6::timestamptz + interval '30 days', $6, $6, $6)`,
    [clipId, sessionId, runId, runSeq, digest("7"), createdAt],
  );
  await db.query(
    `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                  expected_size_bytes, storage_state, verification_state, stored_at,
                                  verified_at)
     VALUES ($1, $2, 'video', 'diogenes-model-training', $3, $4, 1000, 'stored', 'verified',
             transaction_timestamp(), transaction_timestamp())`,
    [randomUUID(), clipId, `raw/v1/certified/dataminer-api-production/${clipId}/video.mp4`, digest("8")],
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
  await db.query(
    `INSERT INTO annotators(id, alias, status) VALUES ($1, 'alpha', 'active'), ($2, 'bravo', 'active')`,
    [ids.annotatorAlpha, ids.annotatorBravo],
  );
  for (const [id, clientDeviceId, annotatorId, fingerprint] of [
    [ids.deviceA, "dev-a", ids.annotatorAlpha, "c".repeat(64)],
    [ids.deviceB, "dev-b", ids.annotatorBravo, "d".repeat(64)],
  ] as [string, string, string, string][]) {
    await db.query(
      `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                           credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                           display_snapshot, input_devices_snapshot, client_version, last_seen_at)
       VALUES ($1, $2, $3, 'dm-device-1', $4, 'active', '10.0.26100', 'PCI', '566.36',
               '{}'::jsonb, '[]'::jsonb, '0.7.0', transaction_timestamp())`,
      [id, clientDeviceId, annotatorId, fingerprint],
    );
  }

  // 三个会话：两个具名（各自的设备与采集员）、一个匿名（同 deviceA，无采集员）。
  await insertSession(ids.sessionAlpha, ids.deviceA, ids.annotatorAlpha, "client-1");
  await insertSession(ids.sessionBravo, ids.deviceB, ids.annotatorBravo, "client-1");
  await insertSession(ids.sessionAnonymous, ids.deviceA, null, "client-2");

  await insertRun(ids.runCross, ids.sessionAlpha, 0, crossInstant);
  await insertRun(ids.runNoon, ids.sessionAlpha, 1, noonInstant);
  await insertRun(ids.runBravo, ids.sessionBravo, 0, noonInstant);
  await insertRun(ids.runAnonymous, ids.sessionAnonymous, 0, noonInstant);

  // 每段 300 帧 ÷ 60fps = 5 秒、1000 字节。跨天界段晚半天登记（同一天），
  // 正午段晚一天登记——recorded / registered 的分叉靠它成立。
  await insertClip(ids.sessionAlpha, ids.runCross, 0, registeredAt);
  await insertClip(ids.sessionAlpha, ids.runNoon, 1, lateRegisteredAt);
  await insertClip(ids.sessionBravo, ids.runBravo, 0, registeredAt);
  await insertClip(ids.sessionAnonymous, ids.runAnonymous, 0, registeredAt);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("全局每日进线（docs/97）", () => {
  it("默认按录制墙钟 + 北京天界：北京 01:00 的录制属于北京的那一天", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone);

    expect(intake.timeBasis).toBe("recorded");
    expect(intake.timeZone).toBe("Asia/Shanghai");
    const base = intake.daily.find((day) => day.day === BASE);
    expect(base).toMatchObject({
      clipCount: 4,
      sessionCount: 3,
      storedClipCount: 4,
      verifiedClipCount: 4,
      unhealthyClipCount: 0,
      capturedSeconds: 20,
      storedBytes: 4000,
      activeDeviceCount: 2,
      activeAnnotatorCount: 2,
    });
  });

  it("同一批数据切 UTC 天界：跨界段挪到前一天——分桶时区是可判定事实", async () => {
    const intake = await browse.dailyIntake(7, "recorded", "UTC");

    expect(intake.timeZone).toBe("UTC");
    expect(intake.daily.find((day) => day.day === crossUtcDay)).toMatchObject({ clipCount: 1 });
    expect(intake.daily.find((day) => day.day === BASE)).toMatchObject({ clipCount: 3 });
  });

  it("登记口径把晚传的段挪到入库那天", async () => {
    const intake = await browse.dailyIntake(7, "registered", defaultStatsTimeZone);

    expect(intake.timeBasis).toBe("registered");
    expect(intake.daily.find((day) => day.day === BASE)).toMatchObject({ clipCount: 3 });
    expect(intake.daily.find((day) => day.day === BASE_NEXT)).toMatchObject({ clipCount: 1 });
  });

  it("匿名会话不虚增活跃采集员，同一设备两会话只算一台", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone);
    const base = intake.daily.find((day) => day.day === BASE);

    // sessionAnonymous 与 sessionAlpha 共用 deviceA：设备数是 2 不是 3；
    // 匿名会话的 annotator 是 NULL，count(DISTINCT) 天然不计。
    expect(base).toMatchObject({ sessionCount: 3, activeDeviceCount: 2, activeAnnotatorCount: 2 });
  });

  it("合计按窗口整体去重，不是逐日行相加", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone);

    expect(intake.totals).toEqual({
      sessionCount: 3,
      clipCount: 4,
      storedClipCount: 4,
      verifiedClipCount: 4,
      unhealthyClipCount: 0,
      capturedSeconds: 20,
      storedBytes: 4000,
      activeDeviceCount: 2,
      activeAnnotatorCount: 2,
      trainableClipCount: 0,
      // 本组 fixture 一件 depth 都没有——这两个 0 正是「不带 depth」的基线读数。
      // depth 自身的口径在 browse-depth-visibility.test.ts 里独立建库钉，
      // 不往这套共享 fixture 上加行（该文件头注的同一理由）。
      depthClipCount: 0,
      depthBytes: 0,
    });
  });

  it("窗口天数补零、升序、以今天收尾", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone);

    expect(intake.daily).toHaveLength(7);
    expect(intake.daily.at(-1)?.day).toBe(shanghaiDay(0));
    expect([...intake.daily.map((day) => day.day)].sort()).toEqual(
      intake.daily.map((day) => day.day),
    );
    const empty = intake.daily.filter((day) => day.day !== BASE);
    for (const day of empty) {
      expect(day.clipCount).toBe(0);
      expect(day.activeDeviceCount).toBe(0);
    }
  });
});

describe("维度拆分与回看列（docs/97 P2）", () => {
  it("按采集员拆分：匿名会话归入 none 组，组序按片段数降序", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone, "annotator");

    expect(intake.groupBy).toBe("annotator");
    expect(intake.groups?.map((group) => [group.key, group.totals.clipCount])).toEqual([
      [ids.annotatorAlpha, 2],
      [ids.annotatorBravo, 1],
      ["none", 1],
    ]);
    const alpha = intake.groups?.find((group) => group.key === ids.annotatorAlpha);
    expect(alpha?.label).toBe("alpha");
    // 拆分序列是稀疏的：alpha 只在锚定日有数据，就只有一行。
    expect(alpha?.daily).toHaveLength(1);
    expect(alpha?.daily[0]).toMatchObject({ day: BASE, clipCount: 2, capturedSeconds: 10 });
  });

  it("按游戏拆分：label 取 games.title", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone, "game");

    expect(intake.groups).toHaveLength(1);
    expect(intake.groups?.[0]).toMatchObject({ key: "cod", label: "Call of Duty" });
    expect(intake.groups?.[0]?.totals.clipCount).toBe(4);
  });

  it("不带 groupBy 时不出现 groups 字段", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone);
    expect(intake.groups).toBeUndefined();
    expect(intake.groupBy).toBeUndefined();
  });

  it("可训练列是现在时回看：dev 车道的片段回看恒为 0", async () => {
    const intake = await browse.dailyIntake(7, undefined, defaultStatsTimeZone);

    // fixture 全部是 dev_non_trainable 会话（无 consent/grant/审批链），
    // clip_trainability 现值必然 false——这正是「回看≠当日」语义的最小可判定面。
    expect(intake.totals.trainableClipCount).toBe(0);
    expect(intake.daily.find((day) => day.day === BASE)?.trainableClipCount).toBe(0);
  });
});

describe("跨面对账（docs/97 §8 验收）", () => {
  it("每日进线（registered）与 Pipeline 存储增长同参数下逐日一致", async () => {
    const [intake, health] = await Promise.all([
      browse.dailyIntake(7, "registered", defaultStatsTimeZone),
      browse.pipelineHealth(7, defaultStatsTimeZone),
    ]);

    expect(health.timeZone).toBe(defaultStatsTimeZone);
    const intakeByDay = new Map(intake.daily.map((day) => [day.day, day.clipCount]));
    for (const day of health.storageByDay) {
      expect({ day: day.day, clipCount: day.clipCount }).toEqual({
        day: day.day,
        clipCount: intakeByDay.get(day.day) ?? -1,
      });
    }
  });
});

describe("贡献面与总览面的天界一致（docs/97 P2）", () => {
  it("采集员贡献按同一缺省时区分桶并回显", async () => {
    const contribution = await browse.contributionForAnnotator(ids.annotatorAlpha, 7);

    expect(contribution.timeZone).toBe(defaultStatsTimeZone);
    // alpha 的两段都录在锚定日（北京 01:00 与正午）——北京天界下同一天，
    // UTC 天界会把 01:00 那段挪走。与每日进线的 BASE 行同一天、同一计数。
    const active = contribution.daily.filter((day) => day.clipCount > 0);
    expect(active).toHaveLength(1);
    expect(active[0]).toMatchObject({ day: BASE, clipCount: 2 });
  });
});

describe("查询参数（docs/97 §4.1）", () => {
  it("缺省：30 天、Asia/Shanghai、口径留给仓储层缺省", () => {
    const parsed = statsDailyQuerySchema.parse({});
    expect(parsed).toMatchObject({ days: 30, tz: "Asia/Shanghai" });
    expect(parsed.timeBasis).toBeUndefined();
    expect(defaultStatsTimeZone).toBe("Asia/Shanghai");
  });

  it("非法时区名被拒——错误在契约层就位，不留给 SQL", () => {
    expect(statsDailyQuerySchema.safeParse({ tz: "Not/AZone" }).success).toBe(false);
    expect(statsDailyQuerySchema.safeParse({ tz: "Asia/Tokyo" }).success).toBe(true);
  });
});
