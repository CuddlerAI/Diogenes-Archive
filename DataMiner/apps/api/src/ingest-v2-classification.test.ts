/**
 * 来源分类的派生（issue #855 §B / §C），跑在真 PostgreSQL 引擎（PGlite）上。
 *
 * 为什么这组测试必须存在：`sessions.schema_version` 在 `createSession` 那一刻是
 * **客户端声明的**。改完之后"声明自己是 v2"就成了拿到 `training_candidate` 的第一
 * 步——所以"只是声明"必须换不到 candidate。这里逐条钉死：缺任何一项采集时就已经
 * 存在、事后伪造不了的事实，都落 `non_trainable`。
 *
 * 这些用例**真的打到派生逻辑**，不是靠 `requireGame()` / `catalogVersion()` 的 404
 * 提前返回：那两条 404 只挡"根本不存在"，而这里给的每一行都存在。
 */
import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { beforeAll, describe, expect, it } from "vitest";

import { environmentProfile, storagePolicyForSessionSchema } from "./environment.js";
import { createSessionSql, deriveSourceClassification } from "./repository.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

const production = environmentProfile("production");
const neutralFamily = storagePolicyForSessionSchema(production, "dm-session-ingest-2");
const legacyFamily = storagePolicyForSessionSchema(production, "dm-local-dev-session-2");

const digest = (fill: string) => `sha256:${fill.repeat(64)}`;

let db: PGlite;
type DerivationClient = Parameters<typeof deriveSourceClassification>[0];
let client: DerivationClient;

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  client = {
    query: (text: string, values?: unknown[]) => db.query(text, values),
  } as unknown as DerivationClient;
}, 120_000);

/**
 * 一整套"来源链完整"的治理事实：已登记并批准登记的四本册子、允许采集的游戏、
 * 活跃标注员与覆盖采集时刻的同意书。每个用例只破坏其中一项。
 */
async function completeProvenance(options: { allowCapture?: boolean } = {}) {
  const slug = randomUUID().slice(0, 8);
  const gameId = `derivation-${slug}`;
  await db.query(
    `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                       allowed_for_capture, approved_for_trainable)
     VALUES ($1, $2, 'game', 'first_person', 'rendered', $3, false)`,
    [gameId, `Derivation Fixture ${slug}`, options.allowCapture ?? true],
  );
  const versions: Record<string, string> = {};
  for (const kind of ["device_profile", "game_profile", "recording_profile", "game_mapping"]) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_by, approved_at
       ) VALUES ($1, $2::catalog_kind, $3, $4, $5, $6, '{}'::jsonb,
                 'approved', 'test-governance', transaction_timestamp())`,
      [
        id,
        kind,
        `${gameId}-${kind}`,
        `1-${slug}`,
        kind === "device_profile" ? null : gameId,
        digest("1"),
      ],
    );
    versions[kind] = id;
  }
  const annotatorId = randomUUID();
  await db.query(`INSERT INTO annotators(id, alias, status) VALUES ($1, $2, 'active')`, [
    annotatorId,
    `annotator-${slug}`,
  ]);
  const consentId = randomUUID();
  await db.query(
    `INSERT INTO consents(id, annotator_id, policy_version, purpose, terms_hash, status,
                          allows_trainable_use, accepted_at, scopes)
     VALUES ($1, $2, '2026-07-25-teaser', 'public first-party release', $3, 'active',
             true, '2026-01-01T00:00:00Z', $4::text[])`,
    [consentId, annotatorId, digest("c").slice("sha256:".length), ["input", "video"]],
  );
  const deviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, annotator_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver, display_snapshot,
       input_devices_snapshot, client_version
     ) VALUES ($1, $2, $3, $4, $5, 'active', '10.0.26100', '10DE:2684', '566.36',
               '{}'::jsonb, '[]'::jsonb, '0.7.1')`,
    [
      deviceId,
      `client-${slug}`,
      annotatorId,
      `${gameId}-device_profile`,
      randomUUID().replace(/-/g, "") + randomUUID().replace(/-/g, "").slice(0, 32),
    ],
  );
  return {
    gameId,
    deviceId,
    annotatorId,
    consentId,
    versions,
    catalogVersionIds: [
      versions.device_profile!,
      versions.game_profile!,
      versions.recording_profile!,
      versions.game_mapping!,
    ],
  };
}

type Provenance = Awaited<ReturnType<typeof completeProvenance>>;

function factsOf(provenance: Provenance) {
  return {
    gameId: provenance.gameId,
    annotatorId: provenance.annotatorId,
    consentId: provenance.consentId,
    catalogVersionIds: provenance.catalogVersionIds,
  };
}

// ── ① 完整来源链 → candidate；它仍然不是通过 ──────────────────────────────────
describe("① a complete provenance chain reaches training_candidate", () => {
  it("derives training_candidate when every capture-time fact is present", async () => {
    const provenance = await completeProvenance();

    const derived = await deriveSourceClassification(client, neutralFamily, factsOf(provenance));

    expect(derived).toEqual({ classification: "training_candidate", withheldBecause: [] });
  });

  it("keeps the legacy lane permanently non_trainable no matter what is present", async () => {
    const provenance = await completeProvenance();

    const derived = await deriveSourceClassification(client, legacyFamily, factsOf(provenance));

    // 上限就是 non_trainable 的车道连查都不用查：数据在来源处就永久不可训练
    // （issue #855 §D）。这一条是 legacy 行为一字未变的证据。
    expect(derived).toEqual({
      classification: "non_trainable",
      withheldBecause: ["lane_non_trainable"],
    });
  });
});

// ── ② 声明 v2 ≠ 拿到 candidate ───────────────────────────────────────────────
//
// 每个用例都给出一整套合法的其余事实，只抽掉一项。所以失败原因不可能是"夹具没搭
// 起来"，只能是被抽掉的那一项。
describe("② declaring the neutral contract is not enough on its own", () => {
  it("withholds candidate when the device has no annotator bound", async () => {
    const provenance = await completeProvenance();

    const derived = await deriveSourceClassification(client, neutralFamily, {
      ...factsOf(provenance),
      annotatorId: null,
      consentId: null,
    });

    expect(derived.classification).toBe("non_trainable");
    expect(derived.withheldBecause).toEqual(["missing_annotator", "missing_consent"]);
  });

  it("withholds candidate when no consent covers the moment capture started", async () => {
    const provenance = await completeProvenance();

    const derived = await deriveSourceClassification(client, neutralFamily, {
      ...factsOf(provenance),
      consentId: null,
    });

    expect(derived.classification).toBe("non_trainable");
    expect(derived.withheldBecause).toEqual(["missing_consent"]);
  });

  it("withholds candidate when the game is registered but capture is not admitted", async () => {
    // requireGame() 放行——这一行确实存在。挡住它的必须是派生本身。
    const provenance = await completeProvenance({ allowCapture: false });

    const derived = await deriveSourceClassification(client, neutralFamily, factsOf(provenance));

    expect(derived.classification).toBe("non_trainable");
    expect(derived.withheldBecause).toEqual(["capture_not_admitted"]);
  });

  it.each(["device_profile", "game_profile", "recording_profile", "game_mapping"] as const)(
    "withholds candidate when the %s version is registered but still draft",
    async (kind) => {
      const provenance = await completeProvenance();
      // draft 是**存在但尚未登记完成**：`catalogVersion()` 只拒 revoked，所以这一行
      // 照样解析得到，404 根本不会出现。挡它的只能是派生。
      await db.query(
        `UPDATE trusted_catalog_versions
            SET status = 'draft', approved_by = NULL, approved_at = NULL
          WHERE id = $1`,
        [provenance.versions[kind]],
      );

      const derived = await deriveSourceClassification(client, neutralFamily, factsOf(provenance));

      expect(derived.classification).toBe("non_trainable");
      expect(derived.withheldBecause).toEqual(["catalog_identity_not_registered"]);
    },
  );

  it("accepts a retired catalog version, which is history rather than a defect", async () => {
    const provenance = await completeProvenance();
    await db.query(
      `UPDATE trusted_catalog_versions
          SET status = 'retired', retired_at = transaction_timestamp()
        WHERE id = $1`,
      [provenance.versions.recording_profile],
    );

    const derived = await deriveSourceClassification(client, neutralFamily, factsOf(provenance));

    // `retired` = 这一版不再下发，不是"这一版从来不算数"——与 clip_trainability 对
    // 四本册子的取法逐字一致。
    expect(derived.classification).toBe("training_candidate");
  });

  it("reports every missing fact at once rather than the first one", async () => {
    const provenance = await completeProvenance({ allowCapture: false });
    await db.query(`UPDATE trusted_catalog_versions SET status = 'draft', approved_by = NULL, approved_at = NULL WHERE id = $1`, [
      provenance.versions.game_profile,
    ]);

    const derived = await deriveSourceClassification(client, neutralFamily, {
      ...factsOf(provenance),
      annotatorId: null,
      consentId: null,
    });

    expect(derived.withheldBecause).toEqual([
      "capture_not_admitted",
      "catalog_identity_not_registered",
      "missing_annotator",
      "missing_consent",
    ]);
  });
});

// ── ③ 库级兜底：consent 那半有 CHECK，candidate 写不进去 ──────────────────────
async function insertSession(
  provenance: Provenance,
  overrides: {
    schemaVersion?: string;
    catalogMode?: string | null;
    classification?: string;
    annotatorId?: string | null;
    consentId?: string | null;
  } = {},
) {
  const id = randomUUID();
  await db.query(createSessionSql, [
    id,
    `local-${id.slice(0, 8)}`,
    provenance.deviceId,
    provenance.gameId,
    "1.0.0",
    digest("d"),
    "derivation-catalog",
    "1",
    overrides.catalogMode === undefined ? null : overrides.catalogMode,
    digest("e"),
    digest("1"),
    provenance.versions.device_profile,
    provenance.versions.recording_profile,
    provenance.versions.game_mapping,
    overrides.schemaVersion ?? "dm-session-ingest-2",
    {},
    digest("f"),
    "native_wgc_window_v1",
    1920,
    1080,
    60,
    1,
    "0.7.1",
    "2026-08-24T00:00:00Z",
    overrides.annotatorId === undefined ? provenance.annotatorId : overrides.annotatorId,
    overrides.consentId === undefined ? provenance.consentId : overrides.consentId,
    null,
    overrides.classification ?? "training_candidate",
    null,
    null,
    null,
    null,
    null,
  ]);
  return id;
}

describe("③ the database refuses a candidate without its identity evidence", () => {
  it("refuses a training_candidate session with no annotator or consent (migration 0032)", async () => {
    const provenance = await completeProvenance();

    await expect(
      insertSession(provenance, { annotatorId: null, consentId: null }),
    ).rejects.toThrow();
  });

  it("refuses a neutral session that resurrects the retired catalog mode (migration 0036)", async () => {
    const provenance = await completeProvenance();

    await expect(
      insertSession(provenance, { catalogMode: "dev_non_trainable" }),
    ).rejects.toThrow();
  });

  it("still requires the retired word on a legacy session", async () => {
    const provenance = await completeProvenance();

    await expect(
      insertSession(provenance, {
        schemaVersion: "dm-local-dev-session-2",
        catalogMode: null,
        classification: "non_trainable",
      }),
    ).rejects.toThrow();
  });
});

// ── ④ candidate 在审批前仍被 clip_trainability 挡住 ──────────────────────────
async function insertClip(
  sessionId: string,
  options: { preflightResult: string; audioState?: "ready" | "absent"; audioObject?: boolean },
) {
  const runId = randomUUID();
  await db.query(
    `INSERT INTO capture_runs(
       id, session_id, client_run_id, run_seq, status, capture_engine,
       encoder_backend, width, height, fps_num, fps_den, session_frame_start,
       preflight_schema_version, preflight_id, preflight_result, preflight_evidence_sha256
     ) VALUES ($1, $2, $3, 0, 'completed', 'native_wgc_window_v1',
               'mf_nvidia_hardware_v1', 1920, 1080, 60, 1, 0,
               'dm-preflight-evidence-1', $4, $5, $6)`,
    [runId, sessionId, `r0000-${runId.slice(0, 8)}`, `pf-${runId.slice(0, 8)}`, options.preflightResult, digest("8")],
  );
  const clipId = randomUUID();
  const meta =
    options.audioState === undefined
      ? {}
      : { audio: { state: options.audioState, source: "process_loopback" } };
  await db.query(
    `INSERT INTO clips(
       id, session_id, capture_run_id, run_seq, segment_seq, session_frame_start,
       n_frames, qpc_start, qpc_end, event_count, meta_json, manifest_json,
       manifest_sha256, retention_deadline
     ) VALUES ($1, $2, $3, 0, 0, 0, 300, 0, 100000, 42, $4::jsonb, '{}'::jsonb, $5,
               transaction_timestamp() + interval '30 days')`,
    [clipId, sessionId, runId, JSON.stringify(meta), digest("9")],
  );
  const kinds = ["video", "frames", "events", "meta", "manifest"];
  if (options.audioObject) kinds.push("audio");
  for (const kind of kinds) {
    await db.query(
      `INSERT INTO artifact_objects(
         id, clip_id, kind, bucket_name, object_key, expected_sha256, expected_size_bytes,
         storage_state, stored_at, verification_state, verified_at
       ) VALUES ($1, $2, $3::artifact_kind, 'bucket', $4, $5, 1024,
                 'stored', transaction_timestamp(), 'verified', transaction_timestamp())`,
      [randomUUID(), clipId, kind, `raw/v2/dep/sessions/${sessionId}/${kind}`, digest("a")],
    );
  }
  return clipId;
}

async function blockersOf(sessionId: string): Promise<string[]> {
  const { rows } = await db.query<{ blockers: string[] }>(
    `SELECT blockers FROM clip_trainability WHERE session_id = $1`,
    [sessionId],
  );
  return rows[0]?.blockers ?? [];
}

describe("④ training_candidate is still blocked until every approval clears", () => {
  it("keeps a candidate clip untrainable on the unapproved game and catalog", async () => {
    const provenance = await completeProvenance();
    const sessionId = await insertSession(provenance);
    await insertClip(sessionId, { preflightResult: "preflight_pass" });

    const blockers = await blockersOf(sessionId);

    // candidate 只表示"有资格等待裁决"（issue #855 §C）：source_non_trainable 不再
    // 出现，其余 blocker 一条不少。
    expect(blockers).not.toContain("source_non_trainable");
    expect(blockers).toContain("game_unapproved");
    expect(blockers).toContain("recording_profile_unapproved");
    expect(blockers).toContain("executable_unapproved");
    expect(blockers).toContain("quality_gate_not_passed");
    const { rows } = await db.query<{ is_trainable: boolean }>(
      `SELECT is_trainable FROM clip_trainability WHERE session_id = $1`,
      [sessionId],
    );
    expect(rows[0]?.is_trainable).toBe(false);
  });

  it("keeps source_non_trainable on a non_trainable session in the same shape", async () => {
    const provenance = await completeProvenance();
    const sessionId = await insertSession(provenance, { classification: "non_trainable" });
    await insertClip(sessionId, { preflightResult: "preflight_pass" });

    expect(await blockersOf(sessionId)).toContain("source_non_trainable");
  });
});

// ── ⑤ 迁移 0037：v2 的音频是 clip 事实，不是 schema 世代 ──────────────────────
describe("⑤ clip_trainability counts a neutral-lane audio clip correctly", () => {
  it("does not call a six-object audio clip incomplete", async () => {
    const provenance = await completeProvenance();
    const sessionId = await insertSession(provenance);
    await insertClip(sessionId, {
      preflightResult: "preflight_pass",
      audioState: "ready",
      audioObject: true,
    });

    // 不改 0037 的话，这一条会永远挂在 artifact_set_incomplete 上——一个只朝"更不可
    // 训练"方向错、却指不到真正原因的 bug。
    expect(await blockersOf(sessionId)).not.toContain("artifact_set_incomplete");
  });

  it("still expects five objects when the neutral clip carries no audio", async () => {
    const provenance = await completeProvenance();
    const sessionId = await insertSession(provenance);
    await insertClip(sessionId, { preflightResult: "preflight_pass", audioState: "absent" });

    expect(await blockersOf(sessionId)).not.toContain("artifact_set_incomplete");
  });

  it("notices a neutral clip that declares audio but never stored the object", async () => {
    const provenance = await completeProvenance();
    const sessionId = await insertSession(provenance);
    await insertClip(sessionId, { preflightResult: "preflight_pass", audioState: "ready" });

    expect(await blockersOf(sessionId)).toContain("artifact_set_incomplete");
  });

  it("keeps the legacy audio generation working exactly as before", async () => {
    const provenance = await completeProvenance();
    const sessionId = await insertSession(provenance, {
      schemaVersion: "dm-local-dev-session-3",
      catalogMode: "dev_non_trainable",
      classification: "non_trainable",
    });
    await insertClip(sessionId, {
      preflightResult: "dev_non_trainable_pass",
      audioState: "ready",
      audioObject: true,
    });

    expect(await blockersOf(sessionId)).not.toContain("artifact_set_incomplete");
  });
});
