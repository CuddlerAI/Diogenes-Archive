import { randomUUID } from "node:crypto";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readdir, readFile } from "node:fs/promises";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  approveExecutable,
  listExecutables,
  revokeExecutable,
  type QueryFn,
} from "./executable-approval.js";
import { registerCatalogBundle } from "./register-catalog.js";
import { createSessionSql } from "./repository.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;
const query: QueryFn = async (text, params) => {
  const result = await db.query(text, params as unknown[]);
  return { rows: result.rows as Array<Record<string, unknown>> };
};

const GAME_ID = "approval-test-game";
const digest = (fill: string) => `sha256:${fill.repeat(64)}`;

/** Minimal governance rows so a session row is insertable. */
async function seedIdentity() {
  const versions: Record<string, string> = {};
  for (const kind of ["device_profile", "recording_profile", "game_mapping"]) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot, status,
         approved_for_trainable, approved_by, approved_at
       ) VALUES ($1, $2::catalog_kind, $3, $4, $5, $6, '{}'::jsonb, 'approved',
                 true, 'test-governance', transaction_timestamp())`,
      [
        id,
        kind,
        `${GAME_ID}-${kind}`,
        `1-${id.slice(0, 8)}`,
        kind === "device_profile" ? null : GAME_ID,
        digest("1"),
      ],
    );
    versions[kind] = id;
  }
  const deviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver, display_snapshot,
       input_devices_snapshot, client_version
     ) VALUES ($1, $2, $3, $4, 'active', '19045', 'pci-1', 'driver-1',
               '{}'::jsonb, '[]'::jsonb, '0.6.1')`,
    [
      deviceId,
      `device-${deviceId.slice(0, 8)}`,
      `${GAME_ID}-device_profile`,
      randomUUID().replace(/-/g, "") + randomUUID().replace(/-/g, "").slice(0, 32),
    ],
  );
  return { deviceId, versions };
}

async function insertSession(
  identity: Awaited<ReturnType<typeof seedIdentity>>,
  executableSha256: string | null,
  executableMissingReason: string | null = null,
) {
  const id = randomUUID();
  await db.query(createSessionSql, [
    id,
    `local-${id.slice(0, 8)}`,
    identity.deviceId,
    GAME_ID,
    "1.0.0",
    executableSha256,
    "test-catalog",
    "1",
    "dev_non_trainable",
    digest("e"),
    digest("f"),
    identity.versions.device_profile,
    identity.versions.recording_profile,
    identity.versions.game_mapping,
    "dm-local-dev-session-2",
    {},
    digest("2"),
    "native_wgc_window_v1",
    1920,
    1080,
    30,
    1,
    "0.6.1",
    "2026-07-28T00:00:00Z",
    null,
    null,
    null,
    "non_trainable",
    null,
    null,
    null,
    null,
    executableMissingReason,
  ]);
  return id;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                       allowed_for_capture, approved_for_trainable)
     VALUES ($1, 'Approval Test Game', 'game', 'first_person', 'rendered', true, true)`,
    [GAME_ID],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("observed build registration (migration 0018)", () => {
  it("registers the executable a session actually ran, as pending", async () => {
    const identity = await seedIdentity();
    const observed = digest("a");
    await insertSession(identity, observed);

    const rows = await listExecutables(query, { gameId: GAME_ID });
    const row = rows.find((candidate) => candidate.sha256 === observed);
    expect(row).toBeDefined();
    expect(row!.status).toBe("pending");
    expect(row!.source).toBe("observed");
  });

  it("does not resurrect a revoked build when it is captured again", async () => {
    const identity = await seedIdentity();
    const observed = digest("b");
    await insertSession(identity, observed);
    await approveExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      approver: "governance@example.com",
    });
    await revokeExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      actor: "governance@example.com",
    });

    // A fresh capture of the same build must not quietly re-open it.
    await insertSession(identity, observed);

    const rows = await listExecutables(query, { gameId: GAME_ID, status: "revoked" });
    expect(rows.map((row) => row.sha256)).toContain(observed);
  });
});

// docs/55：客户端身份采集失败改为降级留痕，指纹因此可空。0018 的触发器写的是
// approved_game_executables.executable_sha256（sha256_digest NOT NULL），所以
// 0019 必须给它加 WHEN——否则第一条无指纹会话会让触发器抛错，**连带 sessions
// 插入本身失败**，摄取核心路径被一个治理表的登记动作打断。
describe("sessions without an observed build (migration 0019)", () => {
  it("accepts the session and registers nothing", async () => {
    const identity = await seedIdentity();
    const before = await listExecutables(query, { gameId: GAME_ID });

    const sessionId = await insertSession(identity, null, "open_process_denied:5");

    const stored = await db.query<{ sha: string | null; reason: string | null }>(
      `SELECT game_executable_sha256 AS sha, game_executable_sha256_missing_reason AS reason
         FROM sessions WHERE id = $1`,
      [sessionId],
    );
    expect(stored.rows[0]).toEqual({ sha: null, reason: "open_process_denied:5" });
    // 没有指纹就没有可登记的 build——名单不该凭空长出一行。
    expect(await listExecutables(query, { gameId: GAME_ID })).toHaveLength(before.length);
  });

  it("enforces sha256 and reason as mutually exclusive", async () => {
    const identity = await seedIdentity();
    // 两边都缺：漏报，且无人知情。
    await expect(insertSession(identity, null, null)).rejects.toThrow();
    // 两边都在：自相矛盾。
    await expect(insertSession(identity, digest("c"), "open_process_denied:5")).rejects.toThrow();
  });

  it("stores the full error chain, and bounds only its length", async () => {
    const identity = await seedIdentity();
    // 完整路径与系统错误原文是有意保留的排查线索（负责人裁决 2026-07-28）。
    const reason =
      "executable_read_failed: failed to open C:\\Users\\Example\\Games\\cod.exe (os error 32)";
    const sessionId = await insertSession(identity, null, reason);
    const stored = await db.query<{ reason: string }>(
      `SELECT game_executable_sha256_missing_reason AS reason FROM sessions WHERE id = $1`,
      [sessionId],
    );
    expect(stored.rows[0]?.reason).toBe(reason);

    // 上限在 DB 层也拦一道，不只靠 zod——脚本与回填同样写不进超长值。
    await expect(insertSession(identity, null, "x".repeat(2001))).rejects.toThrow();
    await expect(insertSession(identity, null, "   ")).rejects.toThrow();
  });

  it("is never trainable, because the approval join cannot match a null digest", async () => {
    const identity = await seedIdentity();
    const sessionId = await insertSession(identity, null, "open_process_denied:5");
    const runId = randomUUID();
    await db.query(
      `INSERT INTO capture_runs(
         id, session_id, client_run_id, run_seq, status, capture_engine,
         encoder_backend, width, height, fps_num, fps_den, session_frame_start,
         preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
       ) VALUES ($1, $2, $3, 0, 'completed', 'native_wgc_window_v1',
                 'mf_nvidia_hardware_v1', 1920, 1080, 30, 1, 0,
                 'dm-preflight-evidence-1', $4, 'dev_non_trainable_pass', $5)`,
      [runId, sessionId, `r0000-${runId.slice(0, 8)}`, `pf-${runId.slice(0, 8)}`, digest("8")],
    );
    const clipId = randomUUID();
    await db.query(
      `INSERT INTO clips(
         id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
         n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
         manifest_sha256, retention_deadline
       ) VALUES ($1, $2, $3, 0, 0, 0, 300, 0, 100000, 42, '{}'::jsonb, '{}'::jsonb, $4,
                 transaction_timestamp() + interval '30 days')`,
      [clipId, sessionId, runId, digest("9")],
    );

    // 这正是不另设 executable_unobserved blocker 的理由：LEFT JOIN 在 NULL 上
    // 天然不匹配，既有的 blocker 一行不改就命中。
    const { rows } = await db.query<{ blockers: string[] }>(
      `SELECT blockers FROM clip_trainability WHERE session_id = $1`,
      [sessionId],
    );
    expect(rows[0]?.blockers).toContain("executable_unapproved");
  });
});

describe("trainability gate", () => {
  it("blocks an unapproved build, clears on approval, and re-blocks on revocation", async () => {
    const identity = await seedIdentity();
    const observed = digest("d");
    const sessionId = await insertSession(identity, observed);
    // 判定只关心指纹这一项，其余治理事实此处不齐；因此断言"包含/不包含"
    // 而不是整表相等。
    const blockers = async () =>
      (
        await db.query<{ blockers: string[] }>(
          `SELECT blockers FROM clip_trainability WHERE session_id = $1`,
          [sessionId],
        )
      ).rows[0]?.blockers ?? [];
    const runId = randomUUID();
    await db.query(
      `INSERT INTO capture_runs(
         id, session_id, client_run_id, run_seq, status, capture_engine,
         encoder_backend, width, height, fps_num, fps_den, session_frame_start,
         preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
       ) VALUES ($1, $2, $3, 0, 'completed', 'native_wgc_window_v1',
                 'mf_nvidia_hardware_v1', 1920, 1080, 30, 1, 0,
                 'dm-preflight-evidence-1', $4, 'dev_non_trainable_pass', $5)`,
      [runId, sessionId, `r0000-${runId.slice(0, 8)}`, `pf-${runId.slice(0, 8)}`, digest("8")],
    );
    const clipId = randomUUID();
    await db.query(
      `INSERT INTO clips(
         id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
         n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
         manifest_sha256, retention_deadline
       ) VALUES ($1, $2, $3, 0, 0, 0, 300, 0, 100000, 42, '{}'::jsonb, '{}'::jsonb, $4,
                 transaction_timestamp() + interval '30 days')`,
      [clipId, sessionId, runId, digest("9")],
    );

    expect(await blockers()).toContain("executable_unapproved");

    await approveExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      approver: "governance@example.com",
    });
    expect(await blockers()).not.toContain("executable_unapproved");

    // 撤销立即生效：判定是读时派生的视图，无需回填任何行。
    await revokeExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      actor: "governance@example.com",
    });
    expect(await blockers()).toContain("executable_unapproved");
  });
});

describe("runtime role privileges", () => {
  // 回归：触发器默认以调用者权限执行，而写 sessions 的是运行时角色
  // `dataminer_api`——它对这张治理表零权限（grant-runtime-role.sql 不授权）。
  // 若函数不是 SECURITY DEFINER，每一次 POST /v1/sessions 都会 permission
  // denied。超级用户下的其余用例察觉不到，所以这条必须用受限角色跑。
  it("lets the ingestion role insert a session without any privilege on the allowlist", async () => {
    await db.exec(`
      CREATE ROLE ingestion_probe NOLOGIN;
      GRANT USAGE ON SCHEMA public TO ingestion_probe;
      GRANT SELECT, INSERT ON sessions TO ingestion_probe;
      GRANT SELECT ON devices, games, trusted_catalog_versions TO ingestion_probe;
    `);
    const identity = await seedIdentity();
    const observed = digest("6");

    await db.exec(`SET ROLE ingestion_probe;`);
    try {
      await insertSession(identity, observed);
    } finally {
      await db.exec(`RESET ROLE;`);
    }

    const rows = await listExecutables(query, { gameId: GAME_ID });
    expect(rows.find((row) => row.sha256 === observed)?.status).toBe("pending");

    // 同时确认最小权限没有被这次修复顺手放宽。
    const canWrite = await db.query<{ ok: boolean }>(
      `SELECT has_table_privilege('ingestion_probe', 'approved_game_executables', 'INSERT') AS ok`,
    );
    expect(canWrite.rows[0]!.ok).toBe(false);
  });
});

describe("authored pin registration", () => {
  const authoredBundle = (pin: string | null) => ({
    directory: "/nonexistent",
    catalogId: "test-catalog",
    catalogVersion: "1",
    catalogMode: "first_party_release",
    game: {
      id: GAME_ID,
      title: "Approval Test Game",
      contentHash: digest("1"),
      snapshot: pin === null ? {} : { executable_sha256: pin },
    },
    entries: [],
  });
  const baseOptions = { approve: false, approveTrainable: false, allowCapture: false, gameTrainable: false };

  it("registers an authored pin as approved only when the catalog is approved", async () => {
    const pending = digest("1");
    const registered = await registerCatalogBundle(query, authoredBundle(pending) as never, {
      ...baseOptions,
    });
    expect(registered.executable).toMatchObject({ action: "inserted", status: "pending" });
    // 注册不等于审批：pending 行不得带审批人（表的 CHECK 不管 pending，靠代码守）。
    const [row] = await listExecutables(query, { gameId: GAME_ID, status: "pending" }).then((rows) =>
      rows.filter((candidate) => candidate.sha256 === pending),
    );
    expect(row!.source).toBe("authored");
    expect(row!.approvedBy).toBeNull();
    expect(row!.approvedAt).toBeNull();

    const approved = await registerCatalogBundle(query, authoredBundle(pending) as never, {
      ...baseOptions,
      approve: true,
      approver: "governance@example.com",
    });
    expect(approved.executable).toMatchObject({ action: "escalated", status: "approved" });
    const after = (await listExecutables(query, { gameId: GAME_ID })).find(
      (candidate) => candidate.sha256 === pending,
    );
    expect(after!.status).toBe("approved");
    expect(after!.approvedBy).toBe("governance@example.com");
    expect(after!.approvedAt).not.toBeNull();
  });

  it("does not re-open a revoked build by re-registering its catalog", async () => {
    const revoked = digest("2");
    await registerCatalogBundle(query, authoredBundle(revoked) as never, {
      ...baseOptions,
      approve: true,
      approver: "governance@example.com",
    });
    await revokeExecutable(query, {
      gameId: GAME_ID,
      sha256: revoked,
      actor: "governance@example.com",
    });

    const rerun = await registerCatalogBundle(query, authoredBundle(revoked) as never, {
      ...baseOptions,
      approve: true,
      approver: "governance@example.com",
    });
    expect(rerun.executable).toMatchObject({ action: "unchanged", status: "revoked" });
  });

  it("writes no allowlist row when the profile pins nothing", async () => {
    const report = await registerCatalogBundle(query, authoredBundle(null) as never, {
      ...baseOptions,
    });
    expect(report.executable).toEqual({ action: "absent", sha256: null, status: null });
  });
});

describe("governance approval", () => {
  it("approves a pending build attributably and refuses to revive a revoked one", async () => {
    const identity = await seedIdentity();
    const observed = digest("c");
    await insertSession(identity, observed);

    const approved = await approveExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      approver: "governance@example.com",
      note: "Steam build 1234567 verified against the store manifest",
    });
    expect(approved.action).toBe("approved");
    expect(approved.row!.approvedBy).toBe("governance@example.com");
    expect(approved.row!.approvedAt).not.toBeNull();

    expect((await approveExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      approver: "governance@example.com",
    })).action).toBe("already_approved");

    await revokeExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      actor: "governance@example.com",
    });
    const retry = await approveExecutable(query, {
      gameId: GAME_ID,
      sha256: observed,
      approver: "governance@example.com",
    });
    expect(retry.action).toBe("refused_revoked");
  });

  it("refuses to approve a build nobody has ever run", async () => {
    const result = await approveExecutable(query, {
      gameId: GAME_ID,
      sha256: digest("7"),
      approver: "governance@example.com",
    });
    expect(result.action).toBe("not_found");
  });

  it("requires an attributable approver and a well-formed digest", async () => {
    await expect(
      approveExecutable(query, { gameId: GAME_ID, sha256: digest("a"), approver: "  " }),
    ).rejects.toThrow(/approver is required/);
    await expect(
      approveExecutable(query, {
        gameId: GAME_ID,
        sha256: "name-only:cod.exe",
        approver: "governance@example.com",
      }),
    ).rejects.toThrow(/sha256:/);
  });
});
