import { createHash, randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import type { RequestContext } from "./auth.js";
import { loadConfig, type ConsoleConfig } from "./config.js";
import type { AuthenticatedSession, OperatorRole } from "./domain.js";
import { PostgresGovernanceRepository } from "./governance-repository.js";
import { GovernanceService } from "./governance.js";
import { PostgresConsoleRepository } from "./repository.js";
import { hashSessionToken } from "./secrets.js";
import type { SqlClient } from "./sql.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let service: GovernanceService;
let repository: PostgresGovernanceRepository;
let consoleRepository: PostgresConsoleRepository;

const config: ConsoleConfig = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "g".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
});

const context: RequestContext = {
  requestId: "req_governance",
  ipHash: "f".repeat(64),
  userAgent: "vitest",
};

const deviceProfileId = randomUUID();
// 设备钉的是谱系（catalog_id 字符串），不是上面那个版本行 UUID（57 §2.1）。
const deviceProfileCatalogId = "dm-device-1";
const recordingProfileId = randomUUID();

function actor(role: OperatorRole, sudoActive: boolean): AuthenticatedSession {
  const id = randomUUID();
  return {
    operator: {
      id,
      email: `${role}@diogenesmodel.com`,
      passwordHash: null,
      role,
      status: "active",
      failedLoginCount: 0,
      lockedUntil: null,
      locked: false,
      passwordUpdatedAt: null,
      lastLoginAt: null,
      createdAt: new Date(0).toISOString(),
      updatedAt: new Date(0).toISOString(),
    },
    session: {
      id: randomUUID(),
      operatorId: id,
      sudoUntil: sudoActive ? new Date(Date.now() + 600_000).toISOString() : null,
      sudoActive,
      expiresAt: new Date(Date.now() + 3_600_000).toISOString(),
      lastSeenAt: new Date().toISOString(),
      createdAt: new Date().toISOString(),
    },
  };
}

function key(): string {
  return `key-${randomUUID()}`;
}

async function seedAnnotator(): Promise<string> {
  const created = await service.createAnnotator(
    actor("operator", false),
    key(),
    { alias: `alpha-${randomUUID().slice(0, 6)}` },
    context,
  );
  return created.id;
}

async function seedConsent(annotatorId: string): Promise<string> {
  const consent = await service.createConsent(
    actor("operator", false),
    key(),
    {
      annotatorId,
      policyVersion: "v1",
      purpose: "training data capture",
      termsHash: "a".repeat(64),
      acceptedAt: new Date().toISOString(),
      allowsTrainableUse: false,
    },
    context,
  );
  return consent.id;
}

async function seedDevice(annotatorId?: string) {
  return service.createDevice(
    actor("operator", true),
    key(),
    {
      clientDeviceId: `dev-${randomUUID().slice(0, 8)}`,
      ...(annotatorId ? { annotatorId } : {}),
      certifiedProfileId: deviceProfileCatalogId,
      windowsBuild: "10.0.26100",
      gpuPciId: "PCI\\VEN_10DE",
      gpuDriver: "566.36",
      clientVersion: "0.1.0",
      displaySnapshot: { width: 1920, height: 1080 },
      inputDevicesSnapshot: [{ kind: "keyboard" }],
    },
    context,
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
  const client = db as unknown as SqlClient;
  repository = new PostgresGovernanceRepository(client);
  consoleRepository = new PostgresConsoleRepository(client);
  service = new GovernanceService(repository, consoleRepository, config);

  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind, allowed_for_capture)
     VALUES ('yuanshen', 'Genshin Impact', 'game', 'third_person', 'rendered', true)`,
  );
  await db.query(
    `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash, snapshot)
     VALUES ($1, 'device_profile', 'dm-device-1', '1.0.0', NULL, $3, '{}'::jsonb),
            ($2, 'recording_profile', 'dm-capture-1', '1.0.0', 'yuanshen', $3, '{}'::jsonb)`,
    [deviceProfileId, recordingProfileId, `sha256:${"a".repeat(64)}`],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("role and sudo guards", () => {
  it("refuses every write to a viewer", async () => {
    const viewer = actor("viewer", true);

    await expect(
      service.createAnnotator(viewer, key(), { alias: "nope" }, context),
    ).rejects.toMatchObject({ code: "insufficient_role" });
    await expect(
      service.requestClipPurge(viewer, key(), randomUUID(), "cleanup", context),
    ).rejects.toMatchObject({ code: "insufficient_role" });
  });

  it("requires a re-entered password for destructive and credential actions", async () => {
    const withoutSudo = actor("operator", false);

    await expect(
      service.requestClipPurge(withoutSudo, key(), randomUUID(), "cleanup", context),
    ).rejects.toMatchObject({ code: "sudo_required" });
    await expect(seedDevice()).resolves.toBeDefined();
  });

  it("allows non-destructive identity edits without sudo", async () => {
    const annotatorId = await seedAnnotator();

    const updated = await service.updateAnnotator(
      actor("operator", false),
      key(),
      annotatorId,
      { status: "suspended" },
      context,
    );

    expect(updated.status).toBe("suspended");
  });
});

describe("fail-closed policy", () => {
  it("refuses to register a consent that permits training", async () => {
    const annotatorId = await seedAnnotator();

    await expect(
      service.createConsent(
        actor("operator", false),
        key(),
        {
          annotatorId,
          policyVersion: "v1",
          purpose: "training",
          termsHash: "b".repeat(64),
          acceptedAt: new Date().toISOString(),
          allowsTrainableUse: true,
        },
        context,
      ),
    ).rejects.toMatchObject({ code: "policy_blocked" });
  });

  // docs/49 E7 的可训练开关：grant 侧随 capture grant 车道退役（docs/100），
  // consent 侧仍在，用的是同一个 `governance.allowTrainable` 配置。拒绝是配置，
  // 不是常量——这条钉住的是那个开关本身还活着，而不是它曾经守过哪两个入口。
  it("refuses a trainable consent by default and allows it only when configured", async () => {
    const annotatorId = await seedAnnotator();

    await expect(
      service.createConsent(
        actor("operator", false),
        key(),
        {
          annotatorId,
          policyVersion: "v2",
          purpose: "public first-party release",
          termsHash: "d".repeat(64),
          acceptedAt: new Date().toISOString(),
          allowsTrainableUse: true,
        },
        context,
      ),
    ).rejects.toMatchObject({ code: "policy_blocked" });

    const allowingService = new GovernanceService(repository, consoleRepository, {
      ...config,
      governance: { allowTrainable: true },
    });
    const consent = await allowingService.createConsent(
      actor("operator", false),
      key(),
      {
        annotatorId,
        policyVersion: "v2",
        purpose: "public first-party release",
        termsHash: "d".repeat(64),
        acceptedAt: new Date().toISOString(),
        allowsTrainableUse: true,
      },
      context,
    );
    expect(consent.allowsTrainableUse).toBe(true);
  });

});

describe("device credentials", () => {
  it("accepts the legacy profile field without persisting authorization", async () => {
    const issued = await service.createDevice(
      actor("operator", true),
      key(),
      {
        clientDeviceId: `dev-${randomUUID().slice(0, 8)}`,
        certifiedProfileId: "no-such-device-profile",
        windowsBuild: "10.0.26100",
        gpuPciId: "PCI\\VEN_10DE",
        gpuDriver: "566.36",
        clientVersion: "0.1.0",
        displaySnapshot: { width: 1920, height: 1080 },
        inputDevicesSnapshot: [{ kind: "keyboard" }],
      },
      context,
    );
    const stored = await db.query<{ certified_profile_id: string | null }>(
      `SELECT certified_profile_id FROM devices WHERE id = $1`,
      [issued.device.id],
    );
    expect(stored.rows[0]?.certified_profile_id).toBeNull();
    const bindings = await db.query(
      `SELECT profile_id FROM device_profile_bindings WHERE device_id = $1`,
      [issued.device.id],
    );
    expect(bindings.rows).toHaveLength(0);
  });

  it("registers a device without any profile field", async () => {
    const issued = await service.createDevice(
      actor("operator", true),
      key(),
      {
        clientDeviceId: `dev-${randomUUID().slice(0, 8)}`,
        windowsBuild: "10.0.26100",
        gpuPciId: "PCI\\VEN_10DE",
        gpuDriver: "566.36",
        clientVersion: "0.1.0",
        displaySnapshot: { width: 1920, height: 1080 },
        inputDevicesSnapshot: [{ kind: "keyboard" }],
      },
      context,
    );
    expect(issued.device.id).toEqual(expect.any(String));
  });

  it("returns the token once and stores only its digest", async () => {
    const issued = await seedDevice();

    expect(issued.token).toHaveLength(43);
    expect(issued.device.credentialFingerprint).toBe(hashSessionToken(issued.token));

    const stored = await db.query<{ credential_fingerprint: string }>(
      `SELECT credential_fingerprint FROM devices WHERE id = $1`,
      [issued.device.id],
    );
    expect(stored.rows[0]?.credential_fingerprint).toBe(hashSessionToken(issued.token));
    // The plaintext must not be recoverable from anything the database holds.
    const anywhere = await db.query<{ found: number }>(
      `SELECT count(*)::int AS found FROM devices WHERE credential_fingerprint = $1`,
      [issued.token],
    );
    expect(anywhere.rows[0]?.found).toBe(0);
  });

  it("rotates to a new credential and leaves the old digest unusable", async () => {
    const issued = await seedDevice();

    const rotated = await service.rotateDeviceCredential(
      actor("operator", true),
      key(),
      issued.device.id,
      context,
    );

    expect(rotated.token).not.toBe(issued.token);
    expect(rotated.device.credentialFingerprint).toBe(hashSessionToken(rotated.token));
    expect(rotated.device.credentialFingerprint).not.toBe(issued.device.credentialFingerprint);
  });

  it("never writes a device token into the audit trail", async () => {
    const issued = await seedDevice();

    const audit = await db.query<{ details: unknown }>(
      `SELECT details FROM audit_events WHERE entity_id = $1`,
      [issued.device.id],
    );

    expect(JSON.stringify(audit.rows)).not.toContain(issued.token);
    expect(audit.rows[0]?.details).toMatchObject({ credentialIssued: true });
  });
});

describe("idempotency", () => {
  it("replays the first response instead of writing twice", async () => {
    const reuse = key();
    const operator = actor("operator", false);
    const input = { alias: `alpha-${randomUUID().slice(0, 6)}` };

    const first = await service.createAnnotator(operator, reuse, input, context);
    const second = await service.createAnnotator(operator, reuse, input, context);

    expect(second.id).toBe(first.id);
    const rows = await db.query<{ count: number }>(
      `SELECT count(*)::int AS count FROM annotators WHERE alias = $1`,
      [input.alias],
    );
    expect(rows.rows[0]?.count).toBe(1);
  });

  it("refuses the same key with a different body", async () => {
    const reuse = key();
    const operator = actor("operator", false);
    await service.createAnnotator(operator, reuse, { alias: "first" }, context);

    await expect(
      service.createAnnotator(operator, reuse, { alias: "second" }, context),
    ).rejects.toMatchObject({ code: "idempotency_key_reused" });
  });

  it("does not hand the device token out again on replay", async () => {
    const reuse = key();
    const operator = actor("operator", true);
    const input = {
      clientDeviceId: `dev-${randomUUID().slice(0, 8)}`,
      certifiedProfileId: deviceProfileCatalogId,
      windowsBuild: "10.0.26100",
      gpuPciId: "PCI\\VEN_10DE",
      gpuDriver: "566.36",
      clientVersion: "0.1.0",
      displaySnapshot: { width: 1920, height: 1080 },
      inputDevicesSnapshot: [{ kind: "keyboard" }],
    };

    const first = await service.createDevice(operator, reuse, input, context);
    const replay = await service.createDevice(operator, reuse, input, context);

    expect(first.token).toHaveLength(43);
    expect(replay.device.id).toBe(first.device.id);
    expect(replay.token).toBeNull();
  });

  it("frees the key when the operation failed so the operator can retry", async () => {
    const reuse = key();
    const operator = actor("operator", false);

    await expect(
      service.updateAnnotator(operator, reuse, randomUUID(), { status: "suspended" }, context),
    ).rejects.toMatchObject({ code: "annotator_not_found" });

    const annotatorId = await seedAnnotator();
    await expect(
      service.updateAnnotator(operator, reuse, annotatorId, { status: "suspended" }, context),
    ).resolves.toMatchObject({ status: "suspended" });
  });

  it("scopes keys per operator so two operators cannot collide", async () => {
    const shared = key();
    const first = actor("operator", false);
    const second = actor("operator", false);

    const a = await service.createAnnotator(first, shared, { alias: "first-op" }, context);
    const b = await service.createAnnotator(second, shared, { alias: "second-op" }, context);

    expect(a.id).not.toBe(b.id);
  });
});

describe("clip decisions", () => {
  /**
   * 一场录制 + 若干片段。`verifiedCount` 条通过传输验证，`unverifiedCount` 条没有
   * ——后者正是两个判定入口都要跳过的那部分。
   */
  async function seedSession(options?: {
    verifiedCount?: number;
    unverifiedCount?: number;
  }): Promise<{ sessionId: string; verifiedClipIds: string[]; unverifiedClipIds: string[] }> {
    const verifiedCount = options?.verifiedCount ?? 1;
    const unverifiedCount = options?.unverifiedCount ?? 0;
    const annotatorId = await seedAnnotator();
    const device = await seedDevice(annotatorId);
    const sessionId = randomUUID();
    const runId = randomUUID();
    const clipId = randomUUID();
    const settings = randomUUID();
    const mapping = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(id, kind, catalog_id, version, game_id, content_hash, snapshot)
       VALUES ($1, 'settings_profile', $3, '1.0.0', 'yuanshen', $4, '{}'::jsonb),
              ($2, 'game_mapping', $5, '1.0.0', 'yuanshen', $4, '{}'::jsonb)`,
      [settings, mapping, `s-${settings.slice(0, 8)}`, `sha256:${"c".repeat(64)}`, `m-${mapping.slice(0, 8)}`],
    );
    await db.query(
      `INSERT INTO sessions(id, client_session_id, device_id, game_id, game_version,
                            game_executable_sha256, device_catalog_version_id, recording_catalog_version_id,
                            mapping_catalog_version_id, schema_version,
                            manifest_json, manifest_sha256, status, capture_engine, native_width,
                            native_height, fps_num, fps_den, client_version, started_at)
       VALUES ($1, $2, $3, 'yuanshen', '6.7', $4, $5, $6, $7, 'dm-local-dev-session-1',
               '{}'::jsonb, $4, 'completed', 'native_wgc_window_v1', 1920, 1080, 30, 1, '0.1.0',
               transaction_timestamp())`,
      [
        sessionId,
        `client-${sessionId.slice(0, 8)}`,
        device.device.id,
        `sha256:${"d".repeat(64)}`,
        deviceProfileId,
        recordingProfileId,
        mapping,
      ],
    );
    await db.query(
      `INSERT INTO capture_runs(id, session_id, client_run_id, run_seq, capture_engine,
                                encoder_backend, width, height, fps_num, fps_den, session_frame_start)
       VALUES ($1, $2, $3, 0, 'native_wgc_window_v1', 'nvenc_h264', 1920, 1080, 30, 1, 0)`,
      [runId, sessionId, `run-${runId.slice(0, 8)}`],
    );
    const verifiedClipIds: string[] = [];
    const unverifiedClipIds: string[] = [];
    for (let seq = 0; seq < verifiedCount + unverifiedCount; seq += 1) {
      const id = seq === 0 ? clipId : randomUUID();
      const verified = seq < verifiedCount;
      await db.query(
        `INSERT INTO clips(id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
                           n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
                           manifest_sha256, ingest_state, stored_at, verification_state, verified_at,
                           retention_deadline)
         VALUES ($1, $2, $3, 0, $5, 0, 120, 1, 2, 0, '{}'::jsonb, '{}'::jsonb, $4, 'stored',
                 transaction_timestamp(),
                 CASE WHEN $6::boolean THEN 'verified'::verification_state ELSE 'unverified'::verification_state END,
                 CASE WHEN $6::boolean THEN transaction_timestamp() ELSE NULL END,
                 transaction_timestamp() + interval '30 days')`,
        [id, sessionId, runId, `sha256:${"e".repeat(64)}`, seq, verified],
      );
      (verified ? verifiedClipIds : unverifiedClipIds).push(id);
    }
    return { sessionId, verifiedClipIds, unverifiedClipIds };
  }

  async function seedVerifiedClip(): Promise<string> {
    const seeded = await seedSession();
    return seeded.verifiedClipIds[0]!;
  }

  it("records a quality decision and touches nothing else", async () => {
    const clipId = await seedVerifiedClip();

    const outcome = await service.recordClipQuality(
      actor("operator", false),
      key(),
      clipId,
      { result: "passed" },
      context,
    );

    const row = await db.query<{
      quality_state: string;
      verification_state: string;
      ingest_state: string;
      retention_state: string;
    }>(
      `SELECT quality_state, verification_state, ingest_state, retention_state
         FROM clips WHERE id = $1`,
      [clipId],
    );
    expect(outcome.qualityState).toBe("passed");
    expect(row.rows[0]).toMatchObject({
      quality_state: "passed",
      verification_state: "verified",
      ingest_state: "stored",
      retention_state: "retained",
    });
  });

  it("refuses a quality decision before transport verification passed", async () => {
    const clipId = await seedVerifiedClip();
    await db.query(
      `UPDATE clips SET verification_state = 'unverified', verified_at = NULL WHERE id = $1`,
      [clipId],
    );

    await expect(
      service.recordClipQuality(actor("operator", false), key(), clipId, { result: "passed" }, context),
    ).rejects.toMatchObject({ code: "clip_not_verified" });
  });

  it("keeps a passed clip blocked from training", async () => {
    const clipId = await seedVerifiedClip();
    await service.recordClipQuality(
      actor("operator", false),
      key(),
      clipId,
      { result: "passed" },
      context,
    );

    const trainability = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT is_trainable, blockers FROM clip_trainability WHERE clip_id = $1`,
      [clipId],
    );

    // A quality pass is one gate among many. The view still refuses because the
    // source is non-trainable and there is no consent or grant behind it.
    expect(trainability.rows[0]?.is_trainable).toBe(false);
    expect(trainability.rows[0]?.blockers).toContain("source_non_trainable");
  });

  it("enqueues one purge job and stays idempotent on a second request", async () => {
    const clipId = await seedVerifiedClip();

    const first = await service.requestClipPurge(
      actor("operator", true),
      key(),
      clipId,
      "operator requested",
      context,
    );
    const second = await service.requestClipPurge(
      actor("operator", true),
      key(),
      clipId,
      "operator requested again",
      context,
    );

    const jobs = await db.query<{ count: number }>(
      `SELECT count(*)::int AS count FROM purge_jobs WHERE clip_id = $1`,
      [clipId],
    );
    expect(first).toMatchObject({ retentionState: "purge_requested", enqueued: true });
    expect(second.enqueued).toBe(false);
    expect(jobs.rows[0]?.count).toBe(1);
  });

  it("records the operator as the purge requester", async () => {
    const clipId = await seedVerifiedClip();
    const operator = actor("operator", true);

    await service.requestClipPurge(operator, key(), clipId, "consent withdrawn", context);

    const job = await db.query<{ requested_by_type: string; requested_by_id: string; reason: string }>(
      `SELECT requested_by_type, requested_by_id, reason FROM purge_jobs WHERE clip_id = $1`,
      [clipId],
    );
    expect(job.rows[0]).toMatchObject({
      requested_by_type: "operator",
      requested_by_id: operator.operator.id,
      reason: "consent withdrawn",
    });
  });

  // ── session 级 QA 判定（issue #855 PR 5）───────────────────────────────────
  //
  // 人审的是一场录制，不是一条 10 秒片段。按 clip 判是万级点击，按 session 判是
  // 百级——差两个数量级，而判据本身一个字没变。

  it("decides every verified clip in the session and leaves the rest pending", async () => {
    const seeded = await seedSession({ verifiedCount: 3, unverifiedCount: 2 });

    const outcome = await service.recordSessionQuality(
      actor("operator", false),
      key(),
      seeded.sessionId,
      { result: "passed" },
      context,
    );

    const states = await db.query<{ quality_state: string; count: number }>(
      `SELECT quality_state::text AS quality_state, count(*)::int AS count
         FROM clips WHERE session_id = $1 GROUP BY quality_state ORDER BY quality_state`,
      [seeded.sessionId],
    );
    expect(outcome).toMatchObject({
      sessionId: seeded.sessionId,
      qualityState: "passed",
      decidedClipCount: 3,
      skippedClipCount: 2,
    });
    expect(states.rows).toEqual([
      { quality_state: "passed", count: 3 },
      { quality_state: "pending", count: 2 },
    ]);
  });

  it("records the operator as the reviewer, never a machine author", async () => {
    const seeded = await seedSession({ verifiedCount: 2 });
    const operator = actor("operator", false);

    await service.recordSessionQuality(
      operator,
      key(),
      seeded.sessionId,
      { result: "failed" },
      context,
    );

    const reviewers = await db.query<{ quality_reviewed_by: string | null; reviewed: boolean }>(
      `SELECT quality_reviewed_by, quality_reviewed_at IS NOT NULL AS reviewed
         FROM clips WHERE session_id = $1`,
      [seeded.sessionId],
    );
    for (const row of reviewers.rows) {
      expect(row.quality_reviewed_by).toBe(operator.operator.email);
      expect(row.reviewed).toBe(true);
    }
  });

  it("touches only the quality dimension", async () => {
    const seeded = await seedSession({ verifiedCount: 2 });

    await service.recordSessionQuality(
      actor("operator", false),
      key(),
      seeded.sessionId,
      { result: "passed" },
      context,
    );

    const row = await db.query<{
      ingest_state: string;
      verification_state: string;
      retention_state: string;
    }>(
      `SELECT DISTINCT ingest_state::text AS ingest_state,
              verification_state::text AS verification_state,
              retention_state::text AS retention_state
         FROM clips WHERE session_id = $1`,
      [seeded.sessionId],
    );
    expect(row.rows).toEqual([
      { ingest_state: "stored", verification_state: "verified", retention_state: "retained" },
    ]);
  });

  it("writes one session-level audit event carrying the affected clip counts", async () => {
    const seeded = await seedSession({ verifiedCount: 3, unverifiedCount: 1 });
    const operator = actor("operator", false);

    await service.recordSessionQuality(
      operator,
      key(),
      seeded.sessionId,
      { result: "passed", note: "整场画面正常" },
      context,
    );

    const audit = await db.query<{
      action: string;
      entity_type: string;
      actor_id: string;
      details: Record<string, unknown>;
    }>(
      `SELECT action, entity_type, actor_id, details FROM audit_events
        WHERE entity_id = $1 AND action = 'session.quality_reviewed'`,
      [seeded.sessionId],
    );
    expect(audit.rows).toHaveLength(1);
    expect(audit.rows[0]).toMatchObject({
      action: "session.quality_reviewed",
      entity_type: "session",
      actor_id: operator.operator.id,
    });
    expect(audit.rows[0]?.details).toMatchObject({
      result: "passed",
      note: "整场画面正常",
      decidedClipCount: 3,
      skippedClipCount: 1,
      operator: operator.operator.email,
    });
  });

  it("refuses a session decision when no clip has passed transport verification", async () => {
    const seeded = await seedSession({ verifiedCount: 0, unverifiedCount: 2 });

    await expect(
      service.recordSessionQuality(
        actor("operator", false),
        key(),
        seeded.sessionId,
        { result: "passed" },
        context,
      ),
    ).rejects.toMatchObject({ code: "session_has_no_verified_clip" });
  });

  it("refuses a session decision on an unknown session", async () => {
    await expect(
      service.recordSessionQuality(
        actor("operator", false),
        key(),
        randomUUID(),
        { result: "passed" },
        context,
      ),
    ).rejects.toMatchObject({ code: "session_not_found" });
  });

  it("refuses a session decision from a viewer", async () => {
    const seeded = await seedSession({ verifiedCount: 1 });

    await expect(
      service.recordSessionQuality(
        actor("viewer", false),
        key(),
        seeded.sessionId,
        { result: "passed" },
        context,
      ),
    ).rejects.toMatchObject({ code: "insufficient_role" });
  });

  it("replays a repeated session decision instead of re-writing it", async () => {
    const seeded = await seedSession({ verifiedCount: 2 });
    const operator = actor("operator", false);
    const reuse = key();

    const first = await service.recordSessionQuality(
      operator,
      reuse,
      seeded.sessionId,
      { result: "passed" },
      context,
    );
    const replay = await service.recordSessionQuality(
      operator,
      reuse,
      seeded.sessionId,
      { result: "passed" },
      context,
    );

    const audit = await db.query<{ count: number }>(
      `SELECT count(*)::int AS count FROM audit_events
        WHERE entity_id = $1 AND action = 'session.quality_reviewed'`,
      [seeded.sessionId],
    );
    expect(replay).toEqual(first);
    expect(audit.rows[0]?.count).toBe(1);
  });

  it("keeps a passed session blocked from training", async () => {
    const seeded = await seedSession({ verifiedCount: 2 });

    await service.recordSessionQuality(
      actor("operator", false),
      key(),
      seeded.sessionId,
      { result: "passed" },
      context,
    );

    const trainability = await db.query<{ is_trainable: boolean; blockers: string[] }>(
      `SELECT ct.is_trainable, ct.blockers FROM clip_trainability ct
         JOIN clips c ON c.id = ct.clip_id
        WHERE c.session_id = $1`,
      [seeded.sessionId],
    );
    // QA 只是众多闸门里的一道。整场判合格之后，来源分级与同意书仍然挡着。
    expect(trainability.rows).toHaveLength(2);
    for (const row of trainability.rows) {
      expect(row.is_trainable).toBe(false);
      expect(row.blockers).toContain("source_non_trainable");
      expect(row.blockers).not.toContain("quality_gate_not_passed");
    }
  });
});

describe("audit", () => {
  it("writes an operator-attributed event for every write", async () => {
    const operator = actor("operator", false);
    const annotator = await service.createAnnotator(operator, key(), { alias: "audited" }, context);

    const audit = await db.query<{ action: string; actor_type: string; actor_id: string; details: Record<string, unknown> }>(
      `SELECT action, actor_type, actor_id, details FROM audit_events
        WHERE entity_id = $1 ORDER BY id`,
      [annotator.id],
    );

    expect(audit.rows[0]).toMatchObject({
      action: "annotator.create",
      actor_type: "operator",
      actor_id: operator.operator.id,
    });
    expect(audit.rows[0]?.details).toMatchObject({ operator: "operator@diogenesmodel.com" });
    expect(audit.rows[0]?.details).toHaveProperty("ipHash");
  });

  it("cannot be edited or deleted afterwards", async () => {
    const operator = actor("operator", false);
    const annotator = await service.createAnnotator(operator, key(), { alias: "immutable" }, context);

    await expect(
      db.query(`UPDATE audit_events SET action = 'tampered' WHERE entity_id = $1`, [annotator.id]),
    ).rejects.toThrow(/append-only/);
  });
});

/**
 * docs/11 §6.4 是把 client_events 从 audit_events 里分出来的全部理由：审计**设计上
 * 在 purge 之后仍然保留**，而行为遥测必须能被清除。schema 早就为此建好了
 * （retention_state、purged_at、专用的部分索引），但清除动作从来没有人执行——
 * 撤回 consent 只翻了一个状态，遥测原封不动地留着。
 */
describe("telemetry erasure", () => {
  async function seedTelemetry(input: {
    annotatorId: string;
    deviceId: string;
    consentId: string | null;
    receivedDaysAgo?: number;
  }) {
    const eventId = randomUUID();
    const received = input.receivedDaysAgo
      ? `transaction_timestamp() - make_interval(days => ${input.receivedDaysAgo})`
      : "transaction_timestamp()";
    await db.query(
      `INSERT INTO client_events(
         event_id, device_id, annotator_id, consent_id, occurred_at, received_at,
         kind, severity, client_version, details
       ) VALUES ($1, $2, $3, $4, transaction_timestamp(), ${received},
                 'app_started', 'info', '0.1.0', '{"secret":"telemetry payload"}'::jsonb)`,
      [eventId, input.deviceId, input.annotatorId, input.consentId],
    );
    return eventId;
  }

  async function readTelemetry(eventId: string) {
    const result = await db.query<{
      retention_state: string;
      purged_at: Date | null;
      details: unknown;
    }>(`SELECT retention_state, purged_at, details FROM client_events WHERE event_id = $1`, [
      eventId,
    ]);
    return result.rows[0]!;
  }

  it("erases the telemetry recorded under a consent when it is revoked", async () => {
    const annotator = await repository.createAnnotator({ alias: "erasure-a" });
    const device = await repository.createDevice(
      {
        clientDeviceId: `erasure-${randomUUID().slice(0, 8)}`,
        annotatorId: annotator.id,
        certifiedProfileId: deviceProfileCatalogId,
        windowsBuild: "10.0.26100",
        gpuPciId: "10DE:2684",
        gpuDriver: "566.36",
        clientVersion: "0.1.0",
        displaySnapshot: {},
        inputDevicesSnapshot: [],
      },
      "f".repeat(64),
    );
    const consent = await repository.createConsent({
      annotatorId: annotator.id,
      policyVersion: "2026-07-01",
      purpose: "staging capture",
      termsHash: "b".repeat(64),
      allowsTrainableUse: false,
      acceptedAt: new Date().toISOString(),
    });
    const eventId = await seedTelemetry({
      annotatorId: annotator.id,
      deviceId: device.id,
      consentId: consent.id,
    });

    await repository.revokeConsent(consent.id, "user withdrew consent");

    const row = await readTelemetry(eventId);
    expect(row.retention_state).toBe("purged");
    expect(row.purged_at).not.toBeNull();
    // 墓碑而不是删行，与 clip purge 一致：事件存在过这个事实留下，载荷消失。
    expect(row.details).toEqual({});
  });

  // 注册与 enroll 期间的事件早于任何 consent，consent 级清除够不到它们。
  it("leaves telemetry that predates any consent to the other two paths", async () => {
    const annotator = await repository.createAnnotator({ alias: "erasure-b" });
    const device = await repository.createDevice(
      {
        clientDeviceId: `erasure-${randomUUID().slice(0, 8)}`,
        annotatorId: annotator.id,
        certifiedProfileId: deviceProfileCatalogId,
        windowsBuild: "10.0.26100",
        gpuPciId: "10DE:2684",
        gpuDriver: "566.36",
        clientVersion: "0.1.0",
        displaySnapshot: {},
        inputDevicesSnapshot: [],
      },
      "e".repeat(64),
    );
    const consent = await repository.createConsent({
      annotatorId: annotator.id,
      policyVersion: "2026-07-01",
      purpose: "staging capture",
      termsHash: "b".repeat(64),
      allowsTrainableUse: false,
      acceptedAt: new Date().toISOString(),
    });
    const orphan = await seedTelemetry({
      annotatorId: annotator.id,
      deviceId: device.id,
      consentId: null,
    });

    await repository.revokeConsent(consent.id, "user withdrew consent");
    expect((await readTelemetry(orphan)).retention_state).toBe("retained");

    // tombstone 是够得到它的那条路。API 目前只放行 active/suspended，所以这里
    // 直调 repository —— 语句本身必须是对的，等 API 开放注销时就会用到。
    await repository.updateAnnotator(annotator.id, {
      status: "tombstoned",
    } as unknown as Parameters<typeof repository.updateAnnotator>[1]);

    const row = await readTelemetry(orphan);
    expect(row.retention_state).toBe("purged");
    expect(row.details).toEqual({});
  });

  // 停用不是注销：账号还在，数据还在它自己的保留期里。
  it("does not erase telemetry when an annotator is merely suspended", async () => {
    const annotator = await repository.createAnnotator({ alias: "erasure-c" });
    const device = await repository.createDevice(
      {
        clientDeviceId: `erasure-${randomUUID().slice(0, 8)}`,
        annotatorId: annotator.id,
        certifiedProfileId: deviceProfileCatalogId,
        windowsBuild: "10.0.26100",
        gpuPciId: "10DE:2684",
        gpuDriver: "566.36",
        clientVersion: "0.1.0",
        displaySnapshot: {},
        inputDevicesSnapshot: [],
      },
      "d".repeat(64),
    );
    const eventId = await seedTelemetry({
      annotatorId: annotator.id,
      deviceId: device.id,
      consentId: null,
    });

    await repository.updateAnnotator(annotator.id, { status: "suspended" });

    expect((await readTelemetry(eventId)).retention_state).toBe("retained");
  });
});

describe("catalog versions", () => {
  it("returns the governance timestamps the console needs, not just the state", async () => {
    const versions = await service.listCatalogVersions(actor("viewer", false));
    const device = versions.find((v) => v.kind === "device_profile");
    expect(device).toBeDefined();
    // Seeded without an approval, so the whole approval block must read empty
    // rather than be absent from the payload.
    expect({
      status: device?.status,
      exactPayloadAvailable: device?.exactPayloadAvailable,
      payloadByteSize: device?.payloadByteSize,
      publishable: device?.publishable,
      approvedForTrainable: device?.approvedForTrainable,
      approvedBy: device?.approvedBy,
      approvedAt: device?.approvedAt,
      revokedAt: device?.revokedAt,
    }).toEqual({
      status: "draft",
      exactPayloadAvailable: false,
      payloadByteSize: null,
      publishable: false,
      approvedForTrainable: false,
      approvedBy: null,
      approvedAt: null,
      revokedAt: null,
    });
    expect(typeof device?.createdAt).toBe("string");
    expect(device?.contentHash).toBe(`sha256:${"a".repeat(64)}`);
  });

  it("carries approver identity once a row is approved", async () => {
    await db.query(
      `UPDATE trusted_catalog_versions
          SET status = 'approved', approved_by = 'ops@example.com',
              approved_at = now(), approved_for_trainable = true
        WHERE id = $1`,
      [recordingProfileId],
    );

    const versions = await service.listCatalogVersions(actor("viewer", false), "recording_profile");
    const capture = versions.find((v) => v.id === recordingProfileId);
    expect(capture?.status).toBe("approved");
    expect(capture?.approvedBy).toBe("ops@example.com");
    expect(capture?.approvedAt).toBeTypeOf("string");
    // status and approvedForTrainable are orthogonal fields; the console
    // renders them as two columns and must receive them as two fields.
    expect(capture?.approvedForTrainable).toBe(true);
  });

  it("exposes exact-payload readiness and immutable release history", async () => {
    const bytes = Buffer.from('{"schema_version":"dm-device-profile-1"}', "utf8");
    const exactDeviceId = randomUUID();
    const contentHash = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot
       ) VALUES (
         $1, 'device_profile', 'dm-device-exact-1', '1.0.0', NULL, $2, $3::jsonb
       )`,
      [exactDeviceId, contentHash, bytes.toString("utf8")],
    );
    await db.query(
      `INSERT INTO catalog_payloads(
         trusted_catalog_version_id, exact_bytes, byte_size, sha256
       )
       SELECT id, $2::bytea, $3, content_hash
         FROM trusted_catalog_versions
        WHERE id = $1`,
      [exactDeviceId, bytes, bytes.byteLength],
    );
    const versions = await service.listCatalogVersions(actor("viewer", false), "device_profile");
    const device = versions.find((version) => version.id === exactDeviceId);
    expect(device).toMatchObject({
      exactPayloadAvailable: true,
      payloadByteSize: bytes.byteLength,
      publishable: true,
    });

    const releaseId = randomUUID();
    const envelope = Buffer.from('{"signature":"test"}');
    const envelopeHash = `sha256:${createHash("sha256").update(envelope).digest("hex")}`;
    await db.query(
      `INSERT INTO catalog_releases(
         id, channel, catalog_id, catalog_version,
         manifest_bytes, envelope_bytes, envelope_sha256, signing_key_id, created_by
       ) VALUES (
         $1, 'dev', 'dataminer-dev-governance-test', '2026-07-28.1',
         $2::bytea, $3::bytea, $4, 'dm-dev-test-key', 'ops@example.com'
       )`,
      [
        releaseId,
        Buffer.from("{}"),
        envelope,
        envelopeHash,
      ],
    );
    const releases = await service.listCatalogReleases(actor("viewer", false), "dev");
    expect(releases).toEqual([
      expect.objectContaining({
        id: releaseId,
        channel: "dev",
        catalogId: "dataminer-dev-governance-test",
        status: "draft",
        entryCount: 0,
        gameCount: 0,
        createdBy: "ops@example.com",
      }),
    ]);
  });
});
