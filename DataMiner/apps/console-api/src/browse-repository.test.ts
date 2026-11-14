import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { PostgresBrowseRepository } from "./browse-repository.js";
import type { SqlClient } from "./sql.js";
import { defaultPageSize } from "./pagination.js";

// Every assertion here runs the production SQL against a real PostgreSQL engine
// with seeded rows, so the filters, rollups, and keyset paging are executed
// rather than asserted from a fake.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let browse: PostgresBrowseRepository;

const ids = {
  annotatorA: randomUUID(),
  annotatorB: randomUUID(),
  consentA: randomUUID(),
  deviceProfile: randomUUID(),
  recordingProfile: randomUUID(),
  mapping: randomUUID(),
  recordingProfileSynthetic: randomUUID(),
  mappingSynthetic: randomUUID(),
  deviceA: randomUUID(),
  deviceB: randomUUID(),
  deviceUnattached: randomUUID(),
  grantA: randomUUID(),
  sessionA: randomUUID(),
  sessionB: randomUUID(),
  sessionUnattached: randomUUID(),
  runA: randomUUID(),
  runB: randomUUID(),
  runUnattached: randomUUID(),
  clipA1: randomUUID(),
  clipA2: randomUUID(),
  clipB1: randomUUID(),
  clipUnattached: randomUUID(),
};

const digest = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;

async function seedCatalog() {
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
     VALUES ('yuanshen', 'Genshin Impact', 'game', 'third_person', 'rendered', true),
            ('synthetic', 'Synthetic Window', 'synthetic', 'flat', 'rendered', true)`,
  );
  const catalog: [string, string, string, string | null][] = [
    [ids.deviceProfile, "device_profile", "dm-device-1", null],
    [ids.recordingProfile, "recording_profile", "dm-capture-1", "yuanshen"],
    [ids.mapping, "game_mapping", "dm-mapping-1", "yuanshen"],
    [ids.recordingProfileSynthetic, "recording_profile", "dm-capture-2", "synthetic"],
    [ids.mappingSynthetic, "game_mapping", "dm-mapping-2", "synthetic"],
  ];
  for (const [id, kind, catalogId, gameId] of catalog) {
    await db.query(
      `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash, snapshot, status)
       VALUES ($1, $2, $3, '1.0.0', $4, $5, '{}'::jsonb, 'draft')`,
      [id, kind, catalogId, gameId, digest("a")],
    );
  }
}

async function seedIdentity() {
  await db.query(
    `INSERT INTO annotators(id, alias, status) VALUES ($1, 'alpha', 'active'), ($2, 'beta', 'suspended')`,
    [ids.annotatorA, ids.annotatorB],
  );
  await db.query(
    `INSERT INTO consents(id, annotator_id, policy_version, purpose, terms_hash, status, allows_trainable_use, accepted_at)
     VALUES ($1, $2, 'v1', 'training', $3, 'active', false, transaction_timestamp())`,
    [ids.consentA, ids.annotatorA, "b".repeat(64)],
  );
  const devices: [string, string | null, string, string][] = [
    [ids.deviceA, ids.annotatorA, "dev-a", "0.1.0"],
    [ids.deviceB, ids.annotatorB, "dev-b", "0.1.0"],
    [ids.deviceUnattached, null, "dev-unattached", "0.0.99"],
  ];
  for (const [id, annotatorId, clientDeviceId, clientVersion] of devices) {
    await db.query(
      `INSERT INTO devices(id, client_device_id, annotator_id, certified_profile_id,
                           credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
                           display_snapshot, input_devices_snapshot, client_version, last_seen_at)
       VALUES ($1, $2, $3, $4, $5, 'active', '10.0.26100', 'PCI\\VEN_10DE', '566.36',
               '{"width":1920,"height":1080}'::jsonb, '[{"kind":"keyboard"}]'::jsonb, $6,
               transaction_timestamp())`,
      [id, clientDeviceId, annotatorId, "dm-device-1", id.replace(/-/g, "").padEnd(64, "0"), clientVersion],
    );
  }
  await db.query(
    `INSERT INTO capture_grants(id, annotator_id, consent_id, device_id, game_id,
                                recording_profile_version_id, status, allows_trainable,
                                valid_from, valid_until, issued_by)
     VALUES ($1, $2, $3, $4, 'yuanshen', $5, 'active', false,
             transaction_timestamp() - interval '1 day',
             transaction_timestamp() + interval '30 days', 'ops@diogenesmodel.com')`,
    [ids.grantA, ids.annotatorA, ids.consentA, ids.deviceA, ids.recordingProfile],
  );
}

async function insertSession(
  id: string,
  deviceId: string,
  annotatorId: string | null,
  gameId: string,
  offsetDays: number,
  attribution: { consentId: string; grantId: string } | null = null,
) {
  const perGame =
    gameId === "synthetic"
      ? [ids.recordingProfileSynthetic, ids.mappingSynthetic]
      : [ids.recordingProfile, ids.mapping];
  await db.query(
    `INSERT INTO sessions(id, client_session_id, device_id, annotator_id, game_id, game_version,
                          game_executable_sha256, device_catalog_version_id, recording_catalog_version_id,
                          mapping_catalog_version_id, schema_version,
                          manifest_json, manifest_sha256, status, capture_engine,
                          native_width, native_height, fps_num, fps_den, client_version, started_at,
                          catalog_id, catalog_version, catalog_mode, catalog_manifest_sha256,
                          game_profile_sha256, created_at, consent_id, capture_grant_id)
     VALUES ($1, $2, $3, $4, $5, '6.7', $6, $7, $8, $9, 'dm-local-dev-session-2',
             '{}'::jsonb, $10, 'completed', 'native_wgc_window_v1', 1920, 1080, 30, 1, '0.1.0',
             transaction_timestamp() - make_interval(days => $11::int),
             'dm-local-dev', '1.0.0', 'dev_non_trainable', $12, $13,
             transaction_timestamp() - make_interval(days => $11::int), $14, $15)`,
    [
      id,
      `client-${id.slice(0, 8)}`,
      deviceId,
      annotatorId,
      gameId,
      digest("c"),
      ids.deviceProfile,
      perGame[0],
      perGame[1],
      digest("d"),
      offsetDays,
      digest("e"),
      digest("f"),
      attribution?.consentId ?? null,
      attribution?.grantId ?? null,
    ],
  );
}

async function insertRun(id: string, sessionId: string) {
  await db.query(
    `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, status, capture_engine,
                              encoder_backend, width, height, fps_num, fps_den, session_frame_start,
                              frame_count, preflight_schema_version, preflight_id, preflight_result,
                              preflight_evidence_sha256)
     VALUES ($1, $2, $3, 0, 'completed', 'native_wgc_window_v1', 'nvenc_h264', 1920, 1080, 30, 1, 0,
             240, 'dm-preflight-evidence-1', $4, 'dev_non_trainable_pass', $5)`,
    [id, sessionId, `run-${id.slice(0, 8)}`, randomUUID(), digest("9")],
  );
}

async function insertClip(
  id: string,
  sessionId: string,
  runId: string,
  segmentSeq: number,
  options: { verified: boolean; offsetDays: number },
) {
  await db.query(
    `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                       n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
                       manifest_sha256, ingest_state, verification_state, retention_deadline,
                       stored_at, verified_at, created_at)
     VALUES ($1, $2, $3, 0, $4, 0, 120, 1000, 2000, 42, '{"schema":"dm-1"}'::jsonb,
             '{"files":4}'::jsonb, $5, $6, $7,
             transaction_timestamp() + interval '30 days',
             transaction_timestamp(), $8,
             transaction_timestamp() - make_interval(days => $9::int))`,
    [
      id,
      sessionId,
      runId,
      segmentSeq,
      digest("7"),
      "stored",
      options.verified ? "verified" : "unverified",
      options.verified ? new Date() : null,
      options.offsetDays,
    ],
  );
  for (const kind of ["video", "frames", "events", "meta", "manifest"]) {
    await db.query(
      `INSERT INTO artifact_objects(id, clip_id, kind, bucket_name, object_key, expected_sha256,
                                    expected_size_bytes, storage_state, verification_state, stored_at,
                                    verified_at)
       VALUES ($1, $2, $3, 'diogenes-model-training', $4, $5, 1000, 'stored', $6,
               transaction_timestamp(), $7)`,
      [
        randomUUID(),
        id,
        kind,
        `raw-staging/v1/non-trainable/dev/${id}/${kind}`,
        digest("8"),
        options.verified ? "verified" : "unverified",
        options.verified ? new Date() : null,
      ],
    );
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
  browse = new PostgresBrowseRepository(db as unknown as SqlClient);

  await seedCatalog();
  await seedIdentity();
  await insertSession(ids.sessionA, ids.deviceA, ids.annotatorA, "yuanshen", 0, {
    consentId: ids.consentA,
    grantId: ids.grantA,
  });
  await insertSession(ids.sessionB, ids.deviceB, ids.annotatorB, "synthetic", 1);
  await insertSession(ids.sessionUnattached, ids.deviceUnattached, null, "yuanshen", 2);
  await insertRun(ids.runA, ids.sessionA);
  await insertRun(ids.runB, ids.sessionB);
  await insertRun(ids.runUnattached, ids.sessionUnattached);
  await insertClip(ids.clipA1, ids.sessionA, ids.runA, 0, { verified: true, offsetDays: 0 });
  await insertClip(ids.clipA2, ids.sessionA, ids.runA, 1, { verified: false, offsetDays: 0 });
  await insertClip(ids.clipB1, ids.sessionB, ids.runB, 0, { verified: true, offsetDays: 1 });
  await insertClip(ids.clipUnattached, ids.sessionUnattached, ids.runUnattached, 0, {
    verified: false,
    offsetDays: 2,
  });
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("annotator browsing", () => {
  it("rolls up devices, consents, grants, and contribution per annotator", async () => {
    const page = await browse.listAnnotators({ pageSize: defaultPageSize });
    const alpha = page.items.find((item) => item.id === ids.annotatorA);

    expect(alpha).toMatchObject({
      alias: "alpha",
      status: "active",
      deviceCount: 1,
      activeConsentCount: 1,
      activeGrantCount: 1,
      sessionCount: 1,
      clipCount: 2,
      verifiedClipCount: 1,
    });
    expect(alpha?.storedBytes).toBe(10_000);
    expect(alpha?.lastActivityAt).not.toBeNull();
  });

  it("filters by status and searches by alias", async () => {
    const suspended = await browse.listAnnotators({ status: "suspended", pageSize: 50 });
    const search = await browse.listAnnotators({ query: "alph", pageSize: 50 });

    expect(suspended.items.map((item) => item.alias)).toEqual(["beta"]);
    expect(search.items.map((item) => item.alias)).toEqual(["alpha"]);
  });

  it("returns consents, grants, and devices on the detail view", async () => {
    const detail = await browse.getAnnotator(ids.annotatorA);

    expect(detail?.consents).toHaveLength(1);
    expect(detail?.consents[0]).toMatchObject({ policyVersion: "v1", allowsTrainableUse: false });
    expect(detail?.grants[0]).toMatchObject({ gameId: "yuanshen", status: "active" });
    expect(detail?.devices.map((device) => device.clientDeviceId)).toEqual(["dev-a"]);
  });

  it("tells a superseded consent apart from a withdrawn one", async () => {
    // docs/108 D5. The operator surface used to collapse both into "已撤回",
    // which made "this person re-signed to widen their authorization" read as
    // "this person withdrew their consent" — and a withdrawal is what obliges
    // us to purge their data.
    const annotatorId = randomUUID();
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'supersede-vs-revoke')`, [
      annotatorId,
    ]);
    await db.query(
      `INSERT INTO consents(
         id, annotator_id, policy_version, purpose, terms_hash, status,
         accepted_at, superseded_at, scopes
       ) VALUES ($1, $2, 'v1', 'capture', $3, 'expired',
                 '2026-01-01T00:00:00Z', '2026-05-01T00:00:00Z', ARRAY['input','video']::text[])`,
      [randomUUID(), annotatorId, "c".repeat(64)],
    );

    const detail = await browse.getAnnotator(annotatorId);
    const consent = detail!.consents[0]!;
    expect(consent.supersededAt).not.toBeNull();
    expect(consent.revokedAt).toBeNull();
    expect(consent.scopes).toEqual(["input", "video"]);
  });

  it("returns null for an unknown annotator", async () => {
    expect(await browse.getAnnotator(randomUUID())).toBeNull();
  });

  // 11 §6.2: signed-up and operator-created annotators coexist. The detail view
  // has to show which entry path an annotator took.
  it("reports no identity for an operator-created annotator", async () => {
    const detail = await browse.getAnnotator(ids.annotatorA);
    expect(detail?.identity).toBeNull();
  });

  it("surfaces the invite source and credential timestamps for a signed-up annotator", async () => {
    const annotatorId = randomUUID();
    const operatorId = randomUUID();
    await db.query(
      `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
       VALUES ($1, 'inviter@diogenesmodel.com', $2, transaction_timestamp(), 'admin', 'active')`,
      [operatorId, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA"],
    );
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'signed-up')`, [annotatorId]);
    await db.query(
      `INSERT INTO annotator_credentials(annotator_id, email, password_hash, status, email_verified_at)
       VALUES ($1, 'signed@example.com', $2, 'active', transaction_timestamp())`,
      [annotatorId, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA"],
    );
    await db.query(`INSERT INTO invite_codes(code, created_by) VALUES ('DMAAAABBBBCCCC', $1)`, [
      operatorId,
    ]);
    await db.query(
      `INSERT INTO annotator_invite_uses(id, code, annotator_id) VALUES ($1, 'DMAAAABBBBCCCC', $2)`,
      [randomUUID(), annotatorId],
    );

    const detail = await browse.getAnnotator(annotatorId);
    expect(detail?.identity).toMatchObject({
      email: "signed@example.com",
      credentialStatus: "active",
      inviteCode: "DMAAAABBBBCCCC",
      invitedBy: "inviter@diogenesmodel.com",
    });
    expect(detail?.identity?.registeredAt).not.toBeNull();
    expect(detail?.identity?.emailVerifiedAt).not.toBeNull();
    expect(detail?.identity?.redeemedAt).not.toBeNull();
  });
});

describe("device browsing", () => {
  it("reports device parameters and the certified profile on the detail view", async () => {
    const detail = await browse.getDevice(ids.deviceA);

    expect(detail).toMatchObject({
      clientDeviceId: "dev-a",
      windowsBuild: "10.0.26100",
      gpuDriver: "566.36",
      annotatorAlias: "alpha",
      sessionCount: 1,
      clipCount: 2,
    });
    expect(detail?.displaySnapshot).toEqual({ width: 1920, height: 1080 });
    expect(detail?.inputDevicesSnapshot).toEqual([{ kind: "keyboard" }]);
    expect(detail?.certifiedProfileId).toBe("dm-device-1");
  });

  it("finds devices that belong to nobody", async () => {
    const page = await browse.listDevices({ annotatorId: "none", pageSize: 50 });

    expect(page.items.map((item) => item.clientDeviceId)).toEqual(["dev-unattached"]);
  });

  it("filters by client version", async () => {
    const page = await browse.listDevices({ clientVersion: "0.0.99", pageSize: 50 });

    expect(page.items).toHaveLength(1);
    expect(page.items[0]?.clientVersion).toBe("0.0.99");
  });
});

describe("session and clip browsing", () => {
  it("filters sessions by game", async () => {
    const page = await browse.listSessions({ gameId: "synthetic", pageSize: 50 });

    expect(page.items.map((item) => item.id)).toEqual([ids.sessionB]);
    expect(page.items[0]).toMatchObject({ gameTitle: "Synthetic Window", clipCount: 1 });
  });

  it("filters sessions by device and by annotator", async () => {
    const byDevice = await browse.listSessions({ deviceId: ids.deviceA, pageSize: 50 });
    const byAnnotator = await browse.listSessions({ annotatorId: ids.annotatorB, pageSize: 50 });

    expect(byDevice.items.map((item) => item.id)).toEqual([ids.sessionA]);
    expect(byAnnotator.items.map((item) => item.id)).toEqual([ids.sessionB]);
  });

  it("finds sessions that carry no annotator, which is every M2a session", async () => {
    const page = await browse.listSessions({ annotatorId: "none", pageSize: 50 });

    expect(page.items.map((item) => item.id)).toEqual([ids.sessionUnattached]);
    expect(page.items[0]?.serverDataClassification).toBe("non_trainable");
  });

  it("reports runs and preflight evidence on the session detail view", async () => {
    const detail = await browse.getSession(ids.sessionA);

    expect(detail).toMatchObject({
      clipCount: 2,
      storedClipCount: 2,
      verifiedClipCount: 1,
      runCount: 1,
      catalogMode: "dev_non_trainable",
    });
    expect(detail?.runs[0]).toMatchObject({
      status: "completed",
      encoderBackend: "nvenc_h264",
      preflightResult: "dev_non_trainable_pass",
    });
    // 设备行只钉谱系（57 §2.1），所以「这次跑的哪一版」只有会话行答得了。抬一次
    // 驱动地板之后，同一台机器前后两次会话在这里就是两个不同的版本行。
    expect(detail?.deviceProfile).toMatchObject({
      versionId: ids.deviceProfile,
      version: "1.0.0",
    });
  });

  // issue #855 PR 5：candidate 与最终 is_trainable 之间差的是哪几道闸，本来就是
  // session 尺度的问题；此前界面上只能一条条点开片段去数。
  it("rolls the clip_trainability blockers up to the session", async () => {
    const detail = await browse.getSession(ids.sessionA);

    expect(detail?.trainability).toMatchObject({
      trainableClipCount: 0,
      blockedClipCount: 2,
      // 通过传输验证、仍是 pending 的那一条——session 级判定会落到的正是它。
      awaitingQualityReviewClipCount: 1,
    });
    const blockers = new Map(
      (detail?.trainability.blockers ?? []).map((entry) => [entry.blocker, entry.clipCount]),
    );
    // 两条都还没做 QA 判定；只有未验证的那条额外挂 clip_not_verified。
    expect(blockers.get("quality_gate_not_passed")).toBe(2);
    expect(blockers.get("clip_not_verified")).toBe(1);
    expect(blockers.get("source_non_trainable")).toBe(2);
    // 降序排列：命中最多的排在最前，界面直接照原序渲染。
    const counts = (detail?.trainability.blockers ?? []).map((entry) => entry.clipCount);
    expect([...counts].sort((a, b) => b - a)).toEqual(counts);
  });

  it("filters clips across the three identity dimensions", async () => {
    const byGame = await browse.listClips({ gameId: "synthetic", pageSize: 50 });
    const byDevice = await browse.listClips({ deviceId: ids.deviceA, pageSize: 50 });
    const byAnnotator = await browse.listClips({ annotatorId: ids.annotatorA, pageSize: 50 });
    const unattached = await browse.listClips({ annotatorId: "none", pageSize: 50 });

    expect(byGame.items.map((item) => item.id)).toEqual([ids.clipB1]);
    expect(byDevice.items).toHaveLength(2);
    expect(byAnnotator.items).toHaveLength(2);
    expect(unattached.items.map((item) => item.id)).toEqual([ids.clipUnattached]);
  });

  it("filters clips by verification state", async () => {
    const verified = await browse.listClips({ verificationState: "verified", pageSize: 50 });

    expect(verified.items.map((item) => item.id).sort()).toEqual([ids.clipA1, ids.clipB1].sort());
  });

  it("computes clip duration from the run frame rate and sums stored bytes", async () => {
    const page = await browse.listClips({ sessionId: ids.sessionA, pageSize: 50 });

    expect(page.items[0]).toMatchObject({ nFrames: 120, durationSeconds: 4, sizeBytes: 5_000 });
  });

  it("reports a fully attributed clip as still blocked from training", async () => {
    const detail = await browse.getClip(ids.clipA1);

    // This clip has an annotator and an active consent, and it is still
    // blocked: staging data is non-trainable at the source and the consent does
    // not permit training.
    expect(detail?.isTrainable).toBe(false);
    expect(detail?.blockers).toContain("source_non_trainable");
    expect(detail?.blockers).toContain("consent_disallows_trainable");
    expect(detail?.blockers).not.toContain("missing_annotator");
    // The per-game grant lane is no longer consulted (docs/108 D1, migration
    // 0032), so its blockers must not reappear in a verdict.
    expect(detail?.blockers.filter((blocker) => blocker.includes("capture_grant"))).toEqual([]);
    expect(detail?.blockerCount).toBe(detail?.blockers.length);
  });

  it("names the missing identity on a clip that belongs to nobody", async () => {
    const detail = await browse.getClip(ids.clipUnattached);

    expect(detail?.blockers).toContain("missing_annotator");
    expect(detail?.blockers).toContain("missing_consent");
    expect(detail?.blockers).not.toContain("missing_capture_grant");
    expect(detail?.isTrainable).toBe(false);
  });

  it("returns the five artifacts with keys and digests on the clip detail view", async () => {
    const detail = await browse.getClip(ids.clipA1);

    expect(detail?.artifacts.map((artifact) => artifact.kind).sort()).toEqual([
      "events",
      "frames",
      "manifest",
      "meta",
      "video",
    ]);
    expect(detail?.artifacts[0]).toMatchObject({
      bucketName: "diogenes-model-training",
      storageState: "stored",
      expectedSizeBytes: 1_000,
    });
    expect(detail?.meta).toEqual({ schema: "dm-1" });
  });
});

describe("paging", () => {
  it("walks clips with a keyset cursor without repeating or skipping a row", async () => {
    const first = await browse.listClips({ pageSize: 2 });
    const second = await browse.listClips({ pageSize: 2, cursor: first.nextCursor ?? undefined });

    expect(first.items).toHaveLength(2);
    expect(first.nextCursor).not.toBeNull();
    expect(second.items).toHaveLength(2);
    expect(second.nextCursor).toBeNull();
    const seen = [...first.items, ...second.items].map((item) => item.id);
    expect(new Set(seen).size).toBe(4);
  });

  it("rejects a malformed cursor instead of silently returning page one", async () => {
    await expect(browse.listClips({ pageSize: 2, cursor: "not-a-cursor" })).rejects.toMatchObject({
      code: "invalid_cursor",
    });
  });
});

describe("aggregates", () => {
  it("summarises the pipeline without claiming anything is trainable", async () => {
    const overview = await browse.pipelineOverview();

    expect(overview.clips.total).toBe(4);
    expect(overview.clips.stored).toBe(4);
    expect(overview.clips.verified).toBe(2);
    expect(overview.storedBytes).toBe(20_000);
    expect(overview.trainableClips).toBe(0);
    expect(overview.devices.seenLast24h).toBe(3);
  });

  /**
   * 时长与件数必须是同一批段切出来的。这条把每一格的两个数都钉死：4 段各 120 帧
   * @30fps = 4s，其中 2 段 verified。件数对而时长错（或反过来）是本页最难发现的
   * 一类错——两个数都看着合理，只有并排比才露馅。
   */
  it("每一格都给出与件数同 population 的时长", async () => {
    const overview = await browse.pipelineOverview();

    expect(overview.clipSeconds.total).toBe(16);
    expect(overview.clipSeconds.stored).toBe(16);
    expect(overview.clipSeconds.verified).toBe(8);
    expect(overview.clipSeconds.pending).toBe(0);
    expect(overview.clipSeconds.uploading).toBe(0);
    expect(overview.clipSeconds.retryWait).toBe(0);
    expect(overview.clipSeconds.failed).toBe(0);
    expect(overview.clipSeconds.rejected).toBe(0);
    expect(overview.clipSeconds.purgeActive).toBe(0);
    // 一段都不可训练，时长也必须是 0——不能出现「0 段 / 若干小时」。
    expect(overview.trainableSeconds).toBe(0);
    expect(overview.depth).toEqual({ clips: 0, bytes: 0, seconds: 0 });
  });

  it("builds a per-day contribution series that covers the whole window", async () => {
    const contribution = await browse.contributionForAnnotator(ids.annotatorA, 7);

    expect(contribution.daily).toHaveLength(7);
    expect(contribution.totals).toMatchObject({
      sessionCount: 1,
      clipCount: 2,
      verifiedClipCount: 1,
      storedBytes: 10_000,
    });
    expect(contribution.daily.at(-1)).toMatchObject({ clipCount: 2, verifiedClipCount: 1 });
    expect(contribution.daily[0]).toMatchObject({ clipCount: 0, storedBytes: 0 });
  });

  it("scopes contribution to one device", async () => {
    const contribution = await browse.contributionForDevice(ids.deviceB, 7);

    expect(contribution.totals).toMatchObject({ sessionCount: 1, clipCount: 1, storedBytes: 5_000 });
  });

  // issue #855 PR 5：QA 判定是人的裁决点，所以不预测吞吐，直接把真实积压摆出来。
  it("reports the QA backlog, its oldest wait and the 24-hour overdue count", async () => {
    const idle = await browse.pipelineHealth(7);

    expect(idle.qualityBacklog).toMatchObject({
      pendingClipCount: 2,
      pendingSessionCount: 2,
      overdueThresholdHours: 24,
      overdueClipCount: 0,
      overdueSessionCount: 0,
    });
    expect(idle.qualityBacklog.oldestVerifiedAt).not.toBeNull();
    expect(idle.qualityBacklog.oldestWaitHours ?? -1).toBeGreaterThanOrEqual(0);

    // 超期按**库时钟**数，不按进程时钟：把一条的验证时刻推回 30 小时前。
    await db.query(
      `UPDATE clips SET verified_at = transaction_timestamp() - interval '30 hours' WHERE id = $1`,
      [ids.clipB1],
    );
    try {
      const aged = await browse.pipelineHealth(7);
      expect(aged.qualityBacklog).toMatchObject({
        pendingClipCount: 2,
        overdueClipCount: 1,
        overdueSessionCount: 1,
      });
      expect(aged.qualityBacklog.oldestWaitHours ?? 0).toBeGreaterThanOrEqual(29);
    } finally {
      await db.query(`UPDATE clips SET verified_at = transaction_timestamp() WHERE id = $1`, [
        ids.clipB1,
      ]);
    }
  });

  it("drops a clip out of the QA queue once someone has judged it", async () => {
    await db.query(
      `UPDATE clips SET quality_state = 'passed', quality_reviewed_at = transaction_timestamp(),
              quality_reviewed_by = 'operator@diogenesmodel.com'
        WHERE id = $1`,
      [ids.clipB1],
    );
    try {
      const health = await browse.pipelineHealth(7);
      expect(health.qualityBacklog).toMatchObject({
        pendingClipCount: 1,
        pendingSessionCount: 1,
      });
    } finally {
      await db.query(
        `UPDATE clips SET quality_state = 'pending', quality_reviewed_at = NULL,
                quality_reviewed_by = NULL
          WHERE id = $1`,
        [ids.clipB1],
      );
    }
  });

  it("counts sessions and clips per game", async () => {
    const games = await browse.listGames();

    expect(games.find((game) => game.id === "yuanshen")).toMatchObject({
      sessionCount: 2,
      clipCount: 3,
      approvedForTrainable: false,
    });
  });

});

describe("audit browsing", () => {
  it("filters the shared audit trail by actor and entity", async () => {
    const deviceActor = randomUUID();
    await db.query(
      `INSERT INTO audit_events(event_id, actor_type, actor_id, action, entity_type, entity_id, details)
       VALUES ($1, 'device', $2, 'clip.declared', 'clip', $3, '{"segmentIndex":0}'::jsonb),
              ($4, 'operator', $5, 'operator.session_created', 'operator', $5, '{}'::jsonb)`,
      [randomUUID(), deviceActor, ids.clipA1, randomUUID(), randomUUID()],
    );

    const byActor = await browse.listAuditEvents({ actorType: "device", pageSize: 50 });
    const byEntity = await browse.listAuditEvents({
      entityType: "clip",
      entityId: ids.clipA1,
      pageSize: 50,
    });

    expect(byActor.items[0]).toMatchObject({ action: "clip.declared", actorId: deviceActor });
    expect(byEntity.items[0]?.details).toEqual({ segmentIndex: 0 });
  });
});
