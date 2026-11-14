import { mkdtemp, writeFile } from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readdir, readFile } from "node:fs/promises";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { readCatalogBundle, registerCatalogBundle, type QueryFn } from "./register-catalog.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;
const query: QueryFn = async (text, params) => {
  const result = await db.query(text, params as unknown[]);
  return { rows: result.rows as Array<Record<string, unknown>> };
};

function hashOf(bytes: string): string {
  return `sha256:${createHash("sha256").update(bytes, "utf8").digest("hex")}`;
}

/** Writes a minimal, internally consistent bundle to a temp dir. */
async function writeBundle(
  overrides: {
    gameId?: string;
    captureWidth?: number;
    viewScale?: unknown;
    /** manifest 级 non_trainable；默认 false（可训练候选）。中性一代忽略此项。 */
    nonTrainable?: boolean;
    /** game-profile 的 display_name，即 games.title 的来源。 */
    displayName?: string;
    /** 哪一代 catalog schema（issue #855 PR 1）。默认 legacy。 */
    generation?: "legacy" | "neutral";
    /** 只给负例用：在中性一代的某份载荷上强行复活 `non_trainable`。 */
    resurrectRetiredFieldIn?: "manifest" | "device_profile" | "recording_profile" | "game_profile";
    /** 只给负例用：让 manifest 与 profile 分属两代。 */
    manifestGenerationOverride?: "legacy" | "neutral";
  } = {},
) {
  const gameId = overrides.gameId ?? "test-first-party-game";
  const nonTrainable = overrides.nonTrainable ?? false;
  const generation = overrides.generation ?? "legacy";
  const neutral = generation === "neutral";
  const retired = (kind: "manifest" | "device_profile" | "recording_profile" | "game_profile", value: boolean) =>
    !neutral || overrides.resurrectRetiredFieldIn === kind ? { non_trainable: value } : {};
  const directory = await mkdtemp(path.join(tmpdir(), "dm-catalog-"));
  const files = {
    "device-profile.json": JSON.stringify({
      schema_version: neutral ? "dm-device-profile-3" : "dm-device-profile-2",
      profile_id: `${gameId}-device-v1`,
      ...retired("device_profile", false),
    }),
    // capture + settings 合成一本（docs/57 §3.1）：引擎段在前，游戏侧在后。
    "recording-profile.json": JSON.stringify({
      schema_version: neutral ? "dm-recording-profile-2" : "dm-recording-profile-1",
      profile_id: `${gameId}-recording-v1`,
      capture_backend: "native_wgc_window_v1",
      width: overrides.captureWidth ?? 1920,
      height: 1080,
      fps_num: 30,
      fps_den: 1,
      ...retired("recording_profile", false),
      // 缺省给一个"已定版"的 view_scale 块，使既有 approveTrainable 用例
      // 保持通过；负例用 viewScale 覆盖（null=整块删除）。
      view_scale:
        overrides.viewScale === undefined
          ? {
              applicable: true,
              counts_per_360: 18000,
              vertical_scale_ratio: 1,
              tolerance_pct: 0.5,
              fast_slow_tolerance_pct: 1,
              min_polling_hz: 500,
            }
          : overrides.viewScale,
    }),
    "mapping.json": JSON.stringify({
      schema_version: "dm-mapping-2",
      mapping_id: `game.${gameId}.pc`,
      version: "2026-07-26.1",
      non_trainable: false,
    }),
    "game-profile.json": JSON.stringify({
      schema_version: neutral ? "dm-game-profile-3" : "dm-game-profile-2",
      game_id: gameId,
      version: "1",
      display_name: overrides.displayName ?? "Test First Party Game",
      ...retired("game_profile", nonTrainable),
    }),
  };
  for (const [name, body] of Object.entries(files)) {
    await writeFile(path.join(directory, name), body, "utf8");
  }
  const manifestGeneration = overrides.manifestGenerationOverride ?? generation;
  const manifest = {
    schema_version:
      manifestGeneration === "neutral" ? "dm-catalog-bundle-3" : "dm-catalog-bundle-2",
    catalog_id: "test-first-party-catalog",
    catalog_version: "2026-07-26.1",
    ...(manifestGeneration === "neutral"
      ? overrides.resurrectRetiredFieldIn === "manifest"
        ? { non_trainable: nonTrainable }
        : {}
      : { non_trainable: nonTrainable }),
    device_profiles: [
      { id: `${gameId}-device-v1`, version: "1", content_hash: hashOf(files["device-profile.json"]) },
    ],
    recording_profiles: [
      {
        id: `${gameId}-recording-v1`,
        version: "1",
        content_hash: hashOf(files["recording-profile.json"]),
      },
    ],
    game_profiles: [{ id: gameId, version: "1", content_hash: hashOf(files["game-profile.json"]) }],
    mappings: [
      {
        id: `game.${gameId}.pc`,
        version: "2026-07-26.1",
        content_hash: hashOf(files["mapping.json"]),
      },
    ],
  };
  await writeFile(path.join(directory, "catalog.json"), JSON.stringify(manifest), "utf8");
  return directory;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("catalog bundle reading", () => {
  it("recomputes every hash from the exact bytes and refuses a drifted bundle", async () => {
    const directory = await writeBundle();
    const bundle = await readCatalogBundle(directory);
    expect(bundle.entries).toHaveLength(4);
    expect(bundle.game.id).toBe("test-first-party-game");
    expect(bundle.entries.map((entry) => entry.kind)).toContain("game_profile");

    // Corrupt one profile without updating the manifest: refused.
    await writeFile(path.join(directory, "mapping.json"), JSON.stringify({ tampered: true }), "utf8");
    await expect(readCatalogBundle(directory)).rejects.toThrow(/mapping.json bytes do not match/);
  });
});

describe("catalog registration", () => {
  it("registers fail-closed by default, escalates explicitly, and stays idempotent", async () => {
    const directory = await writeBundle();
    const bundle = await readCatalogBundle(directory);

    // 1. Default: draft rows, nothing trainable, game not capturable.
    const first = await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });
    expect(first.game.action).toBe("inserted");
    expect(first.entries.every((entry) => entry.action === "inserted" && entry.status === "draft")).toBe(true);
    const game = await query(`SELECT allowed_for_capture, approved_for_trainable FROM games WHERE id = $1`, [
      bundle.game.id,
    ]);
    expect(game.rows[0]).toEqual({ allowed_for_capture: false, approved_for_trainable: false });
    const exactPayloads = await query(
      `SELECT trusted.kind, payload.byte_size, payload.sha256,
              octet_length(payload.exact_bytes) AS stored_size
         FROM catalog_payloads payload
         JOIN trusted_catalog_versions trusted ON trusted.id = payload.trusted_catalog_version_id
        WHERE trusted.catalog_id LIKE 'test-first-party-game%'
           OR trusted.catalog_id = 'game.test-first-party-game.pc'
        ORDER BY trusted.kind`,
    );
    expect(exactPayloads.rows).toHaveLength(4);
    for (const row of exactPayloads.rows) {
      expect(Number(row.byte_size)).toBe(Number(row.stored_size));
      expect(String(row.sha256)).toMatch(/^sha256:[0-9a-f]{64}$/);
    }

    // 2. Idempotent re-run: unchanged everywhere.
    const second = await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });
    expect(second.game.action).toBe("unchanged");
    expect(second.entries.every((entry) => entry.action === "unchanged")).toBe(true);

    // 3. Explicit escalation with a named approver.
    const third = await registerCatalogBundle(query, bundle, {
      approve: true,
      approver: "operator@example.com",
      approveTrainable: true,
      allowCapture: true,
      gameTrainable: true,
    });
    expect(third.game.action).toBe("updated");
    expect(third.entries.every((entry) => entry.action === "escalated")).toBe(true);
    const rows = await query(
      `SELECT status, approved_for_trainable, approved_by FROM trusted_catalog_versions
        WHERE catalog_id LIKE 'test-first-party-game%' OR catalog_id = 'game.test-first-party-game.pc'`,
    );
    expect(rows.rows).toHaveLength(4);
    for (const row of rows.rows) {
      expect(row.status).toBe("approved");
      expect(row.approved_for_trainable).toBe(true);
      expect(row.approved_by).toBe("operator@example.com");
    }
    const escalatedGame = await query(
      `SELECT allowed_for_capture, approved_for_trainable FROM games WHERE id = $1`,
      [bundle.game.id],
    );
    expect(escalatedGame.rows[0]).toEqual({ allowed_for_capture: true, approved_for_trainable: true });

    // 4. Escalate-only: a re-run WITHOUT the flags must not demote anything.
    const fourth = await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });
    expect(fourth.entries.every((entry) => entry.action === "unchanged")).toBe(true);
    const stillApproved = await query(
      `SELECT bool_and(status = 'approved') AS approved FROM trusted_catalog_versions
        WHERE catalog_id LIKE 'test-first-party-game%' OR catalog_id = 'game.test-first-party-game.pc'`,
    );
    expect(stillApproved.rows[0]!.approved).toBe(true);
  });

  it("refuses trainable approval when the view_scale block is missing", async () => {
    const directory = await writeBundle({ gameId: "no-view-scale-game", viewScale: null });
    const bundle = await readCatalogBundle(directory);
    await expect(
      registerCatalogBundle(query, bundle, {
        approve: true,
        approver: "operator@example.com",
        approveTrainable: true,
        allowCapture: true,
        gameTrainable: true,
      }),
    ).rejects.toThrow(/view_scale block/);
    // 非可训练注册不受影响（dev 目录照常登记）。
    const report = await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });
    expect(report.entries).toHaveLength(4);
  });

  it("refuses trainable approval when view_scale constants are not finalized", async () => {
    const directory = await writeBundle({
      gameId: "null-constant-game",
      viewScale: {
        applicable: true,
        counts_per_360: null,
        tolerance_pct: 0.5,
        fast_slow_tolerance_pct: 1,
        min_polling_hz: 500,
      },
    });
    const bundle = await readCatalogBundle(directory);
    await expect(
      registerCatalogBundle(query, bundle, {
        approve: true,
        approver: "operator@example.com",
        approveTrainable: true,
        allowCapture: true,
        gameTrainable: true,
      }),
    ).rejects.toThrow(/counts_per_360/);
  });

  it("accepts trainable approval for an explicit not-applicable view_scale", async () => {
    const directory = await writeBundle({
      gameId: "pointer-driven-game",
      viewScale: { applicable: false },
    });
    const bundle = await readCatalogBundle(directory);
    const report = await registerCatalogBundle(query, bundle, {
      approve: true,
      approver: "operator@example.com",
      approveTrainable: true,
      allowCapture: true,
      gameTrainable: true,
    });
    expect(report.entries.every((entry) => entry.action === "inserted")).toBe(true);
  });

  // 2026-07-26 生产事故的回归：release bundle 借用了 dev 游戏身份，
  // 可训练审批因此落到一行标题写着「禁止训练」的 games 记录上（迁移 0028）。
  it("refuses trainable approval for a self-declared non-trainable bundle", async () => {
    const directory = await writeBundle({ gameId: "dev-non-trainable-game", nonTrainable: true });
    const bundle = await readCatalogBundle(directory);
    expect(bundle.declaredNonTrainable).toBe(true);

    for (const escalation of [{ approveTrainable: true }, { gameTrainable: true }]) {
      await expect(
        registerCatalogBundle(query, bundle, {
          approve: true,
          approver: "operator@example.com",
          approveTrainable: false,
          allowCapture: true,
          gameTrainable: false,
          ...escalation,
        }),
      ).rejects.toThrow(/declares non_trainable: true/);
    }

    // dev 目录本身照常登记——被拒的只有可训练升级这一步。
    const report = await registerCatalogBundle(query, bundle, {
      approve: true,
      approver: "operator@example.com",
      approveTrainable: false,
      allowCapture: true,
      gameTrainable: false,
    });
    expect(report.entries).toHaveLength(4);
    const game = await query(`SELECT approved_for_trainable FROM games WHERE id = $1`, [bundle.game.id]);
    expect(game.rows[0]!.approved_for_trainable).toBe(false);
  });

  it("refuses --game-trainable when the game identity is titled non-trainable", async () => {
    // 正是那本 release bundle 的形状：manifest 说自己可训练，却沿用了 dev 的
    // display_name。
    const directory = await writeBundle({
      gameId: "borrowed-identity-game",
      nonTrainable: false,
      displayName: "Call of Duty® Steam（release 测试，禁止训练）",
    });
    const bundle = await readCatalogBundle(directory);
    expect(bundle.declaredNonTrainable).toBe(false);

    await expect(
      registerCatalogBundle(query, bundle, {
        approve: true,
        approver: "operator@example.com",
        approveTrainable: true,
        allowCapture: true,
        gameTrainable: true,
      }),
    ).rejects.toThrow(/declares it non-trainable/);

    const rows = await query(`SELECT id FROM games WHERE id = $1`, [bundle.game.id]);
    expect(rows.rows).toHaveLength(0);
  });

  it("keeps the DB CHECK as a backstop for writers that bypass the tool", async () => {
    await query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ('check-guard-game', 'Guard Test Game（release 测试，禁止训练）',
               'game', 'first_person', 'rendered', true, false)`,
    );
    await expect(
      query(`UPDATE games SET approved_for_trainable = true WHERE id = 'check-guard-game'`),
    ).rejects.toThrow(/games_non_trainable_title_guard/);

    // 英文写法同样拦住，且不误伤普通标题。
    await expect(
      query(
        `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                           allowed_for_capture, approved_for_trainable)
         VALUES ('check-guard-game-en', 'Wolfenstein II (Steam, dev non-trainable)',
                 'game', 'first_person', 'rendered', true, true)`,
      ),
    ).rejects.toThrow(/games_non_trainable_title_guard/);
    await query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                         allowed_for_capture, approved_for_trainable)
       VALUES ('check-guard-game-ok', 'Call of Duty® Steam',
               'game', 'first_person', 'rendered', true, true)`,
    );
  });

  it("refuses changed bytes under an unchanged version", async () => {
    const directory = await writeBundle({ gameId: "conflict-game" });
    const bundle = await readCatalogBundle(directory);
    await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });

    const drifted = await writeBundle({ gameId: "conflict-game", captureWidth: 2560 });
    const driftedBundle = await readCatalogBundle(drifted);
    await expect(
      registerCatalogBundle(query, driftedBundle, {
        approve: false,
        approveTrainable: false,
        allowCapture: false,
        gameTrainable: false,
      }),
    ).rejects.toThrow(/identity is immutable; bump the version/);
  });

  it("never guesses exact bytes for a legacy JSONB-only trusted row", async () => {
    const directory = await writeBundle({ gameId: "legacy-game" });
    const bundle = await readCatalogBundle(directory);
    const gameProfile = bundle.entries.find((entry) => entry.kind === "game_profile")!;
    await query(
      `INSERT INTO games(id, title, content_kind, view_kind, material_kind)
       VALUES ($1, $2, 'game', 'third_person', 'rendered')`,
      [bundle.game.id, bundle.game.title],
    );
    await query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot
       ) VALUES (gen_random_uuid(), $1::catalog_kind, $2, $3, $4, $5, $6::jsonb)`,
      [
        gameProfile.kind,
        gameProfile.catalogId,
        gameProfile.version,
        gameProfile.gameId,
        gameProfile.contentHash,
        JSON.stringify(gameProfile.snapshot),
      ],
    );

    await expect(
      registerCatalogBundle(query, bundle, {
        approve: false,
        approveTrainable: false,
        allowCapture: false,
        gameTrainable: false,
      }),
    ).rejects.toThrow(/legacy JSONB-only row and is not publishable/);

    const payloads = await query(
      `SELECT count(*) AS count
         FROM catalog_payloads payload
         JOIN trusted_catalog_versions trusted ON trusted.id = payload.trusted_catalog_version_id
        WHERE trusted.catalog_id = $1`,
      [gameProfile.catalogId],
    );
    expect(Number(payloads.rows[0]!.count)).toBe(0);
  });

  it("rejects payload mutation and a sha256 that differs from the trusted row", async () => {
    const directory = await writeBundle({ gameId: "payload-guard-game" });
    const bundle = await readCatalogBundle(directory);
    await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });
    const row = await query(
      `SELECT trusted.id
         FROM trusted_catalog_versions trusted
        WHERE trusted.kind = 'game_profile'
          AND trusted.catalog_id = $1`,
      [bundle.game.id],
    );
    await expect(
      query(
        `UPDATE catalog_payloads
            SET exact_bytes = $2::bytea
          WHERE trusted_catalog_version_id = $1`,
        [row.rows[0]!.id, Buffer.from("{}")],
      ),
    ).rejects.toThrow(/exact bytes are immutable/);

    const orphanId = randomUUID();
    await query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot
       ) VALUES ($1, 'game_profile', 'payload-hash-mismatch', '1', $2, $3, '{}'::jsonb)`,
      [orphanId, bundle.game.id, `sha256:${"b".repeat(64)}`],
    );
    await expect(
      query(
        `INSERT INTO catalog_payloads(
           trusted_catalog_version_id, exact_bytes, byte_size, sha256
         ) VALUES ($1, $2::bytea, 2, $3)`,
        [orphanId, Buffer.from("{}"), `sha256:${"c".repeat(64)}`],
      ),
    ).rejects.toThrow(/must equal the trusted catalog content_hash/);
    await expect(
      query(
        `INSERT INTO catalog_payloads(
           trusted_catalog_version_id, exact_bytes, byte_size, sha256
         ) VALUES ($1, $2::bytea, 2, $3)`,
        [orphanId, Buffer.from("{}"), `sha256:${"b".repeat(64)}`],
      ),
    ).rejects.toThrow(/exact bytes do not match the declared sha256/);
  });

  it("refuses trainable approval without an approvable status or approver", async () => {
    const directory = await writeBundle({ gameId: "flags-game" });
    const bundle = await readCatalogBundle(directory);
    await expect(
      registerCatalogBundle(query, bundle, {
        approve: false,
        approveTrainable: true,
        allowCapture: false,
        gameTrainable: false,
      }),
    ).rejects.toThrow(/requires --approve/);
    await expect(
      registerCatalogBundle(query, bundle, {
        approve: true,
        approveTrainable: false,
        allowCapture: false,
        gameTrainable: false,
      }),
    ).rejects.toThrow(/requires --approver/);
  });

  it("registers exactly the identities createSession resolves", async () => {
    // The end-to-end contract: what the tool writes is what catalogVersion()
    // finds — kind + catalog_id + content_hash (+version, +game_id).
    const directory = await writeBundle({ gameId: "resolution-game" });
    const bundle = await readCatalogBundle(directory);
    await registerCatalogBundle(query, bundle, {
      approve: true,
      approver: "operator@example.com",
      approveTrainable: false,
      allowCapture: true,
      gameTrainable: false,
    });

    for (const entry of bundle.entries) {
      const resolved = await query(
        `SELECT id FROM trusted_catalog_versions
          WHERE kind = $1::catalog_kind AND catalog_id = $2 AND content_hash = $3 AND status <> 'revoked'
            AND ($4::text IS NULL OR version = $4)
            AND (($5::text IS NULL AND game_id IS NULL) OR game_id = $5)`,
        [entry.kind, entry.catalogId, entry.contentHash, entry.version, entry.gameId],
      );
      expect(resolved.rows).toHaveLength(1);
    }
  });
});
