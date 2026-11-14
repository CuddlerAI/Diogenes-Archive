import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { defaultStatsTimeZone, statsOutputQuerySchema } from "./browse-contracts.js";
import type { OutputReport, OutputRow } from "./browse-domain.js";
import { PostgresBrowseRepository } from "./browse-repository.js";
import type { SqlClient } from "./sql.js";

/**
 * docs/145：产出报表。与 `browse-daily-intake.test.ts` 同一理由自带一套数据库——
 * 这里的断言钉的是精确计数，任何共享 fixture 的新行都会打挂它们。
 *
 * 时间同样锚在东八区日历上、锚在两天前而不是今天：「北京的今天」在 UTC 窗口里
 * 可能尚未开始，锚到今天的断言会随运行时刻漂移。
 */
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let browse: PostgresBrowseRepository;

const shanghaiDay = (daysAgo: number): string =>
  new Intl.DateTimeFormat("en-CA", { timeZone: "Asia/Shanghai" }).format(
    new Date(Date.now() - daysAgo * 86_400_000),
  );

const BASE = shanghaiDay(2);
const BASE_NEXT = shanghaiDay(1);
/** 北京 01:00 = UTC 前一日 17:00——两个时区分桶必然不同的那个时刻。 */
const crossInstant = `${BASE}T01:00:00+08:00`;
const crossUtcDay = new Date(Date.parse(crossInstant)).toISOString().slice(0, 10);
const noonInstant = `${BASE}T12:00:00+08:00`;
const registeredAt = `${BASE}T13:00:00+08:00`;
/** 晚一天登记：recorded 与 registered 两口径的分叉日。 */
const lateRegisteredAt = `${BASE_NEXT}T12:00:00+08:00`;

const ids = {
  annotatorAlpha: randomUUID(),
  annotatorBravo: randomUUID(),
  /** 在册、active、区间内一条数据都没有——「补齐名单」这条主张的判定用例。 */
  annotatorIdle: randomUUID(),
  /** 停用的人：不进补零名单，但他交过的数据仍然要出现。 */
  annotatorSuspended: randomUUID(),
  deviceProfile: randomUUID(),
  recordingProfile: randomUUID(),
  mapping: randomUUID(),
  // 目录版本按游戏挂：sessions 的目录外键带 game_id，一份 cod 的目录挂不到 ets2 上。
  recordingProfileEts: randomUUID(),
  mappingEts: randomUUID(),
  deviceA: randomUUID(),
  deviceB: randomUUID(),
  deviceC: randomUUID(),
  sessionAlphaCod: randomUUID(),
  sessionAlphaEts: randomUUID(),
  sessionBravo: randomUUID(),
  sessionAnonymous: randomUUID(),
  sessionSuspended: randomUUID(),
  runCross: randomUUID(),
  runNoon: randomUUID(),
  runEts: randomUUID(),
  runBravo: randomUUID(),
  runAnonymous: randomUUID(),
  runSuspended: randomUUID(),
};

const digest = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;

/** 契约先跑一遍：区间上限、分组解析、可重复参数的行为都归它管。 */
const parse = (query: Record<string, unknown>) =>
  statsOutputQuerySchema.parse({ from: BASE, to: BASE, ...query });

async function insertSession(
  id: string,
  deviceId: string,
  annotatorId: string | null,
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
      deviceId,
      annotatorId,
      gameId,
      digest("c"),
      ids.deviceProfile,
      gameId === "cod" ? ids.recordingProfile : ids.recordingProfileEts,
      gameId === "cod" ? ids.mapping : ids.mappingEts,
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

/** 每段 300 帧 ÷ 60fps = 5 秒、1000 字节。计数与秒数都因此可以精确断言。 */
async function insertClip(
  sessionId: string,
  runId: string,
  runSeq: number,
  createdAt: string,
): Promise<void> {
  const clipId = randomUUID();
  await db.query(
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, health_json, meta_json,
                       manifest_json, manifest_sha256, ingest_state, verification_state,
                       retention_deadline, stored_at, verified_at, created_at)
     VALUES ($1, $2, $3, $4, 0, 0, 300, 1000, 2000, 42,
             '{"duplicateFrames":0,"sourceSuperseded":0,"queueDrops":0,"inputOverflow":0}'::jsonb,
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
     VALUES ('cod', 'Call of Duty', 'game', 'first_person', 'rendered', true),
            ('ets2', 'Euro Truck Simulator 2', 'game', 'first_person', 'rendered', true)`,
  );
  for (const [id, kind, catalogId, gameId] of [
    [ids.deviceProfile, "device_profile", "dm-device-1", null],
    [ids.recordingProfile, "recording_profile", "dm-recording-1", "cod"],
    [ids.mapping, "game_mapping", "dm-mapping-1", "cod"],
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
  await db.query(
    `INSERT INTO annotators(id, alias, status)
     VALUES ($1, 'alpha', 'active'), ($2, 'bravo', 'active'), ($3, 'idle', 'active'),
            ($4, 'sleepy', 'suspended')`,
    [ids.annotatorAlpha, ids.annotatorBravo, ids.annotatorIdle, ids.annotatorSuspended],
  );
  for (const [id, clientDeviceId, annotatorId, fingerprint] of [
    [ids.deviceA, "dev-a", ids.annotatorAlpha, "c".repeat(64)],
    [ids.deviceB, "dev-b", ids.annotatorBravo, "d".repeat(64)],
    [ids.deviceC, "dev-c", ids.annotatorSuspended, "e".repeat(64)],
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

  await insertSession(ids.sessionAlphaCod, ids.deviceA, ids.annotatorAlpha, "cod", "client-1");
  await insertSession(ids.sessionAlphaEts, ids.deviceA, ids.annotatorAlpha, "ets2", "client-2");
  await insertSession(ids.sessionBravo, ids.deviceB, ids.annotatorBravo, "cod", "client-1");
  await insertSession(ids.sessionAnonymous, ids.deviceA, null, "cod", "client-3");
  await insertSession(ids.sessionSuspended, ids.deviceC, ids.annotatorSuspended, "ets2", "client-1");

  await insertRun(ids.runCross, ids.sessionAlphaCod, 0, crossInstant);
  await insertRun(ids.runNoon, ids.sessionAlphaCod, 1, noonInstant);
  await insertRun(ids.runEts, ids.sessionAlphaEts, 0, noonInstant);
  await insertRun(ids.runBravo, ids.sessionBravo, 0, noonInstant);
  await insertRun(ids.runAnonymous, ids.sessionAnonymous, 0, noonInstant);
  await insertRun(ids.runSuspended, ids.sessionSuspended, 0, noonInstant);

  // alpha：cod 两段（其中一段跨北京天界、且晚一天登记）+ ets2 一段。
  await insertClip(ids.sessionAlphaCod, ids.runCross, 0, registeredAt);
  await insertClip(ids.sessionAlphaCod, ids.runNoon, 1, lateRegisteredAt);
  await insertClip(ids.sessionAlphaEts, ids.runEts, 0, registeredAt);
  await insertClip(ids.sessionBravo, ids.runBravo, 0, registeredAt);
  await insertClip(ids.sessionAnonymous, ids.runAnonymous, 0, registeredAt);
  await insertClip(ids.sessionSuspended, ids.runSuspended, 0, registeredAt);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

const rowFor = (report: OutputReport, ...keys: string[]): OutputRow | undefined =>
  report.rows.find((row) => keys.every((key, index) => row.keys[index] === key));

describe("产出报表 · 分组（docs/145）", () => {
  it("按采集员分组：匿名会话归 none，停用者交过的数据照样出现", async () => {
    const report = await browse.outputReport(parse({ from: BASE, to: BASE }));

    expect(report.groupBy).toEqual(["annotator"]);
    expect(rowFor(report, ids.annotatorAlpha)?.totals).toMatchObject({
      clipCount: 3,
      capturedSeconds: 15,
      storedBytes: 3000,
    });
    expect(rowFor(report, ids.annotatorBravo)?.totals.clipCount).toBe(1);
    expect(rowFor(report, "none")?.totals.clipCount).toBe(1);
    expect(rowFor(report, "none")?.labels[0]).toBe("none");
    // 停用者不进补零名单，但他那段数据是既成事实，不能因为账号停用就从报表里消失。
    expect(rowFor(report, ids.annotatorSuspended)?.totals.clipCount).toBe(1);
  });

  it("按游戏分组：label 取 games.title", async () => {
    const report = await browse.outputReport(parse({ groupBy: "game" }));

    expect(rowFor(report, "cod")).toMatchObject({ labels: ["Call of Duty"] });
    expect(rowFor(report, "cod")?.totals.clipCount).toBe(4);
    expect(rowFor(report, "ets2")?.totals.clipCount).toBe(2);
    // 不按采集员分组时，「N / M 人有产出」这个问题没有意义，所以是 null 不是 0。
    expect(report.enrolledAnnotatorCount).toBeNull();
  });

  it("两层分组：顺序即列顺序，同一个人的两个游戏各占一行", async () => {
    const report = await browse.outputReport(parse({ groupBy: "annotator,game" }));

    expect(report.groupBy).toEqual(["annotator", "game"]);
    expect(rowFor(report, ids.annotatorAlpha, "cod")?.totals.clipCount).toBe(2);
    expect(rowFor(report, ids.annotatorAlpha, "ets2")?.totals.clipCount).toBe(1);
    expect(rowFor(report, ids.annotatorAlpha, "cod")?.labels).toEqual(["alpha", "Call of Duty"]);

    const reversed = await browse.outputReport(parse({ groupBy: "game,annotator" }));
    expect(rowFor(reversed, "cod", ids.annotatorAlpha)?.totals.clipCount).toBe(2);
  });

  it("按合计时长降序：读表的人第一眼看到的是量最大的那一行", async () => {
    const report = await browse.outputReport(parse({ groupBy: "game" }));
    const seconds = report.rows.map((row) => row.totals.capturedSeconds);
    expect([...seconds].sort((left, right) => right - left)).toEqual(seconds);
  });
});

describe("产出报表 · 补齐在册名单（docs/145）", () => {
  it("零产出的在册采集员照样占一行，整行为零而不是缺席", async () => {
    const report = await browse.outputReport(parse({ from: BASE, to: BASE }));

    const idle = rowFor(report, ids.annotatorIdle);
    expect(idle).toBeDefined();
    expect(idle?.labels).toEqual(["idle"]);
    expect(idle?.totals.clipCount).toBe(0);
    expect(idle?.cells.every((cell) => cell.clipCount === 0)).toBe(true);
    // active 三人（alpha / bravo / idle）；停用的 sleepy 不在册数里。
    expect(report.enrolledAnnotatorCount).toBe(3);
  });

  it("有产出的人不会被补出第二行", async () => {
    const report = await browse.outputReport(parse({ from: BASE, to: BASE }));
    const alphaRows = report.rows.filter((row) => row.keys[0] === ids.annotatorAlpha);
    expect(alphaRows).toHaveLength(1);
  });

  it("两层分组下，零产出行的后续维度留空——他没交，也就没有游戏可归", async () => {
    const report = await browse.outputReport(parse({ groupBy: "annotator,game" }));

    const idle = rowFor(report, ids.annotatorIdle, "");
    expect(idle).toBeDefined();
    expect(idle?.labels).toEqual(["idle", ""]);
    expect(idle?.totals.clipCount).toBe(0);
  });

  it("按游戏分组时不补零：对人零是信号，对游戏零是常态", async () => {
    const report = await browse.outputReport(
      parse({ groupBy: "game", from: shanghaiDay(9), to: shanghaiDay(8) }),
    );
    expect(report.rows).toHaveLength(0);
  });
});

describe("产出报表 · 筛选（docs/145）", () => {
  it("筛采集员：可重复参数，多选取并集", async () => {
    const one = await browse.outputReport(parse({ annotatorId: ids.annotatorAlpha }));
    expect(one.rows.map((row) => row.keys[0])).toEqual([ids.annotatorAlpha]);
    expect(one.totals.clipCount).toBe(3);
    // 名单也跟着收窄，否则「1 / 3 人有产出」会在筛掉两个人之后仍然显示 3。
    expect(one.enrolledAnnotatorCount).toBe(1);

    const two = await browse.outputReport(
      parse({ annotatorId: [ids.annotatorAlpha, ids.annotatorBravo] }),
    );
    expect(two.totals.clipCount).toBe(4);
  });

  it("`none` 是一等筛选值：匿名会话问得出来", async () => {
    const report = await browse.outputReport(parse({ annotatorId: "none" }));

    expect(report.rows.map((row) => row.keys[0])).toEqual(["none"]);
    expect(report.totals.clipCount).toBe(1);
    // 只问匿名时名单是空的——没有哪个在册的人该被补进这张表。
    expect(report.enrolledAnnotatorCount).toBe(0);
  });

  it("筛游戏：与分组维度无关，筛的是数据不是列", async () => {
    const report = await browse.outputReport(parse({ gameId: "ets2" }));

    expect(report.totals.clipCount).toBe(2);
    expect(rowFor(report, ids.annotatorAlpha)?.totals.clipCount).toBe(1);
    expect(rowFor(report, ids.annotatorSuspended)?.totals.clipCount).toBe(1);
    expect(rowFor(report, ids.annotatorBravo)?.totals.clipCount).toBe(0);
  });

  it("两个维度的筛选叠加取交集", async () => {
    const report = await browse.outputReport(
      parse({ annotatorId: ids.annotatorAlpha, gameId: "cod", groupBy: "annotator,game" }),
    );
    expect(report.totals.clipCount).toBe(2);
  });
});

describe("产出报表 · 时间口径与列轴（docs/145）", () => {
  it("列轴按 from/to 生成：没产出的那天必须占一列", async () => {
    const report = await browse.outputReport(parse({ from: shanghaiDay(4), to: shanghaiDay(0) }));

    expect(report.buckets).toHaveLength(5);
    expect(report.buckets.at(0)).toBe(shanghaiDay(4));
    expect(report.buckets.at(-1)).toBe(shanghaiDay(0));
    expect([...report.buckets].sort()).toEqual(report.buckets);
    for (const row of report.rows) expect(row.cells).toHaveLength(report.buckets.length);
  });

  it("默认按录制墙钟 + 北京天界：北京 01:00 的录制属于北京的那一天", async () => {
    const report = await browse.outputReport(parse({ from: BASE, to: BASE_NEXT }));

    expect(report.timeBasis).toBe("recorded");
    expect(report.timeZone).toBe(defaultStatsTimeZone);
    const alpha = rowFor(report, ids.annotatorAlpha);
    expect(alpha?.cells[0]?.clipCount).toBe(3);
    expect(alpha?.cells[1]?.clipCount).toBe(0);
  });

  it("切 UTC 天界：跨界那段挪到前一天——分桶时区是可判定事实", async () => {
    const report = await browse.outputReport(
      parse({ from: crossUtcDay, to: BASE, tz: "UTC", annotatorId: ids.annotatorAlpha }),
    );

    expect(report.timeZone).toBe("UTC");
    const alpha = rowFor(report, ids.annotatorAlpha);
    expect(alpha?.cells[0]?.clipCount).toBe(1);
  });

  it("登记口径把晚传的段挪到入库那天", async () => {
    const report = await browse.outputReport(
      parse({ from: BASE, to: BASE_NEXT, timeBasis: "registered" }),
    );

    expect(report.timeBasis).toBe("registered");
    const alpha = rowFor(report, ids.annotatorAlpha);
    expect(alpha?.cells[0]?.clipCount).toBe(2);
    expect(alpha?.cells[1]?.clipCount).toBe(1);
  });

  it("按月粒度：整月一列，标签是 YYYY-MM", async () => {
    const report = await browse.outputReport(parse({ from: BASE, to: BASE, bucket: "month" }));

    expect(report.bucket).toBe("month");
    expect(report.buckets).toEqual([BASE.slice(0, 7)]);
    expect(rowFor(report, ids.annotatorAlpha)?.cells[0]?.clipCount).toBe(3);
  });
});

describe("产出报表 · 合计（docs/145）", () => {
  it("行合计 = 逐格相加，列合计 = 逐行相加，总计 = 两个方向都对得上", async () => {
    const report = await browse.outputReport(parse({ from: shanghaiDay(4), to: shanghaiDay(0) }));

    for (const row of report.rows) {
      const summed = row.cells.reduce((total, cell) => total + cell.clipCount, 0);
      expect(summed).toBe(row.totals.clipCount);
    }
    const byColumn = report.columnTotals.reduce((total, cell) => total + cell.clipCount, 0);
    const byRow = report.rows.reduce((total, row) => total + row.totals.clipCount, 0);
    expect(byColumn).toBe(report.totals.clipCount);
    expect(byRow).toBe(report.totals.clipCount);
  });

  it("与总览「每日进线」逐桶对账：同一区间、同一口径，两页必须是同一个数", async () => {
    // 这条是验收硬线（docs/145）。报表会被拿去发日报，它和总览各说一个数的话，
    // 先被怀疑的一定是新的那页——所以对账写成测试，不靠人工核。
    const days = 7;
    const report = await browse.outputReport(
      parse({ from: shanghaiDay(days - 1), to: shanghaiDay(0) }),
    );
    const intake = await browse.dailyIntake(days, undefined, defaultStatsTimeZone);

    expect(report.buckets).toEqual(intake.daily.map((day) => day.day));
    expect(report.columnTotals.map((cell) => cell.clipCount)).toEqual(
      intake.daily.map((day) => day.clipCount),
    );
    expect(report.columnTotals.map((cell) => cell.capturedSeconds)).toEqual(
      intake.daily.map((day) => day.capturedSeconds),
    );
    expect(report.columnTotals.map((cell) => cell.storedBytes)).toEqual(
      intake.daily.map((day) => day.storedBytes),
    );
    expect(report.totals.clipCount).toBe(intake.totals.clipCount);
    expect(report.totals.capturedSeconds).toBe(intake.totals.capturedSeconds);
    expect(report.totals.storedBytes).toBe(intake.totals.storedBytes);
    expect(report.totals.trainableClipCount).toBe(intake.totals.trainableClipCount);
  });
});

describe("产出报表 · 契约（docs/145）", () => {
  it("分组是有序列表，同一属性不能出现两次", () => {
    expect(parse({ groupBy: "game,annotator" }).groupBy).toEqual(["game", "annotator"]);
    expect(() => parse({ groupBy: "annotator,annotator" })).toThrow();
    expect(() => parse({ groupBy: "device" })).toThrow();
    expect(() => parse({ groupBy: "" })).toThrow();
  });

  it("可重复参数：单个是字符串，多个是数组，都归一成数组", () => {
    expect(parse({}).annotatorId).toEqual([]);
    expect(parse({ gameId: "cod" }).gameId).toEqual(["cod"]);
    expect(parse({ gameId: ["cod", "ets2"] }).gameId).toEqual(["cod", "ets2"]);
  });

  it("区间上限按粒度分开：超了就 400，不静默截断也不悄悄换粒度", () => {
    const from = "2026-01-01";
    expect(() => statsOutputQuerySchema.parse({ from, to: "2026-04-02" })).not.toThrow();
    expect(() => statsOutputQuerySchema.parse({ from, to: "2026-04-03" })).toThrow();
    expect(() =>
      statsOutputQuerySchema.parse({ from, to: "2026-12-31", bucket: "month" }),
    ).not.toThrow();
    expect(() => statsOutputQuerySchema.parse({ from: "2026-01-02", to: "2026-01-01" })).toThrow();
    expect(() => statsOutputQuerySchema.parse({ from: "2026-02-30", to: "2026-03-01" })).toThrow();
    expect(() => statsOutputQuerySchema.parse({ from: "2026-1-1", to: "2026-03-01" })).toThrow();
  });

  it("缺省：按日、按采集员分组、录制口径、北京天界", () => {
    const query = parse({});
    expect(query.bucket).toBe("day");
    expect(query.groupBy).toEqual(["annotator"]);
    expect(query.timeBasis).toBeUndefined();
    expect(query.tz).toBe(defaultStatsTimeZone);
  });
});
