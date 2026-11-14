/**
 * 新旧 catalog schema 并存期的三条边界（issue #855 PR 1 最后一条 checklist）。
 *
 *   ① v2 仍可读                       —— 今天的 current、回滚路径与三份 superseded 快照
 *   ② 新 schema 无字段可读/可注册/可签名 —— 断代之后 31 份活跃 bundle 走的那条路（PR 2）
 *   ③ 新 schema 带旧字段被拒绝          —— 防止旧语义借尸还魂
 *
 * 三条必须**同时**成立才叫兼容桥：只有①是没断代，只有②是硬切，缺③则新 schema 变成
 * 「字段可选」，于是 authoring 侧会继续用它表达训练资格，而读者早已不再消费——那正是
 * 本 issue 要消灭的那种"两套相反口径同时 active"。
 */
import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  CATALOG_SCHEMA_VERSIONS,
  RETIRED_TRAINING_FIELD,
  catalogSchemaGeneration,
} from "./catalog-schema.js";
import {
  DEFAULT_PUBLISHED_MANIFEST_SCHEMA,
  publishCatalogRelease,
} from "./catalog-publisher.js";
import { readCatalogBundle, registerCatalogBundle, type QueryFn } from "./register-catalog.js";
import {
  FilesystemCatalogReleaseSource,
  assembleSignedBundle,
  type CatalogManifest,
} from "./signed-catalog.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const migrationsDirectory = path.resolve(here, "../migrations");

let db: PGlite;
const query: QueryFn = async (text, params) => {
  const result = await db.query(text, params as unknown[]);
  return { rows: result.rows as Array<Record<string, unknown>> };
};

type Generation = "legacy" | "neutral";
type PayloadKind = "manifest" | "device_profile" | "recording_profile" | "game_profile";

function digestOf(bytes: string): string {
  return createHash("sha256").update(bytes, "utf8").digest("hex");
}

/**
 * 一份内部自洽的最小 bundle，两种目录布局各写一份：
 * - 同目录布局（`device-profile.json` 等），`readCatalogBundle` 用；
 * - content-addressed 布局（`payloads/sha256/<digest>.json`），签名发布用。
 *
 * 两份来自**同一批字节**，所以"注册收得下"和"签名收得下"说的是同一件事。
 */
function writeBundle(options: {
  generation: Generation;
  gameId?: string;
  /** 只给③用：在中性一代的某份载荷上强行复活 `non_trainable`。 */
  resurrectIn?: PayloadKind;
  /** 只给③用：让 manifest 与 profile 分属两代。 */
  manifestGeneration?: Generation;
  /** 只给负例用：从 legacy manifest 上抹掉必填的 `non_trainable`。 */
  dropLegacyField?: boolean;
}): { flat: string; contentAddressed: string; manifestBytes: Buffer } {
  const gameId = options.gameId ?? `bridge-game-${randomUUID().slice(0, 8)}`;
  const neutral = options.generation === "neutral";
  const manifestGeneration = options.manifestGeneration ?? options.generation;
  const retired = (kind: PayloadKind) =>
    !neutral || options.resurrectIn === kind ? { [RETIRED_TRAINING_FIELD]: true } : {};

  const payloads: Record<string, string> = {
    "device-profile.json": JSON.stringify({
      schema_version: CATALOG_SCHEMA_VERSIONS.device_profile[options.generation],
      profile_id: `${gameId}-device`,
      version: "1",
      ...retired("device_profile"),
    }),
    "recording-profile.json": JSON.stringify({
      schema_version: CATALOG_SCHEMA_VERSIONS.recording_profile[options.generation],
      profile_id: `${gameId}-recording`,
      version: "1",
      ...retired("recording_profile"),
    }),
    "game-profile.json": JSON.stringify({
      schema_version: CATALOG_SCHEMA_VERSIONS.game_profile[options.generation],
      game_id: gameId,
      version: "1",
      display_name: `Bridge Fixture ${gameId}`,
      ...retired("game_profile"),
    }),
    "mapping.json": JSON.stringify({
      // mapping 从来不带训练词，两代共用一个 schema。
      schema_version: "dm-mapping-2",
      mapping_id: `game.${gameId}.pc`,
      version: "1",
    }),
  };

  const manifestField =
    manifestGeneration === "legacy"
      ? options.dropLegacyField
        ? {}
        : { [RETIRED_TRAINING_FIELD]: true }
      : options.resurrectIn === "manifest"
        ? { [RETIRED_TRAINING_FIELD]: true }
        : {};
  const reference = (id: string, file: string) => ({
    id,
    version: "1",
    content_hash: `sha256:${digestOf(payloads[file]!)}`,
  });
  const manifest = {
    schema_version: CATALOG_SCHEMA_VERSIONS.manifest[manifestGeneration],
    catalog_id: `dataminer-${gameId}`,
    catalog_version: "2026-08-25.0",
    ...manifestField,
    device_profiles: [reference(`${gameId}-device`, "device-profile.json")],
    recording_profiles: [reference(`${gameId}-recording`, "recording-profile.json")],
    game_profiles: [reference(gameId, "game-profile.json")],
    mappings: [reference(`game.${gameId}.pc`, "mapping.json")],
  };
  const manifestBytes = Buffer.from(JSON.stringify(manifest), "utf8");

  const flat = mkdtempSync(path.join(tmpdir(), "dm-bridge-flat-"));
  for (const [name, body] of Object.entries(payloads)) {
    writeFileSync(path.join(flat, name), body, "utf8");
  }
  writeFileSync(path.join(flat, "catalog.json"), manifestBytes);

  const contentAddressed = mkdtempSync(path.join(tmpdir(), "dm-bridge-cas-"));
  const payloadDirectory = path.join(contentAddressed, "payloads", "sha256");
  mkdirSync(payloadDirectory, { recursive: true });
  for (const body of Object.values(payloads)) {
    writeFileSync(path.join(payloadDirectory, `${digestOf(body)}.json`), body, "utf8");
  }
  writeFileSync(path.join(contentAddressed, "catalog.json"), manifestBytes);

  return { flat, contentAddressed, manifestBytes };
}

/** 签名信封里那份 manifest 的解码结果。 */
function signedManifest(base64Manifest: string): Record<string, unknown> {
  return JSON.parse(Buffer.from(base64Manifest, "base64").toString("utf8")) as Record<
    string,
    unknown
  >;
}

beforeAll(async () => {
  db = new PGlite();
  for (const file of (await readdir(migrationsDirectory))
    .filter((name) => /^\d+_[a-z0-9_]+\.sql$/.test(name))
    .sort()) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
}, 120_000);

afterAll(async () => {
  await db?.close();
});

// ── ① v2 仍可读 ───────────────────────────────────────────────────────────────
describe("① the current v2 catalog schema stays readable", () => {
  it("reads a legacy bundle and keeps its authored classification", async () => {
    const { flat } = writeBundle({ generation: "legacy" });
    const bundle = await readCatalogBundle(flat);
    expect(bundle.schemaGeneration).toBe("legacy");
    expect(bundle.declaredNonTrainable).toBe(true);
  });

  it("still requires the legacy field on the legacy schema", async () => {
    // 断代不是"这个字段以后可以不写"。一份自称 dm-catalog-bundle-2 却不带它的
    // manifest 是损坏，不是中性。
    const { flat } = writeBundle({ generation: "legacy", dropLegacyField: true });
    await expect(readCatalogBundle(flat)).rejects.toThrow(
      /must declare an explicit boolean non_trainable/,
    );
  });

  it("signs a legacy bundle exactly as before", async () => {
    const { contentAddressed } = writeBundle({ generation: "legacy" });
    const signed = await assembleSignedBundle(
      new FilesystemCatalogReleaseSource(contentAddressed),
    );
    expect(signedManifest(signed.manifest)["schema_version"]).toBe(
      CATALOG_SCHEMA_VERSIONS.manifest.legacy,
    );
    expect(signedManifest(signed.manifest)[RETIRED_TRAINING_FIELD]).toBe(true);
  });

  it("refuses a legacy manifest that claims it is trainable", async () => {
    // dev 是唯一渠道且恒 non_trainable：一份自称可训练的 legacy bundle 没有渠道收得下。
    const { contentAddressed, manifestBytes } = writeBundle({ generation: "legacy" });
    const tampered = JSON.parse(manifestBytes.toString("utf8")) as Record<string, unknown>;
    tampered[RETIRED_TRAINING_FIELD] = false;
    writeFileSync(path.join(contentAddressed, "catalog.json"), JSON.stringify(tampered));
    await expect(
      assembleSignedBundle(new FilesystemCatalogReleaseSource(contentAddressed)),
    ).rejects.toThrow(/cannot be published on the dev channel/);
  });
});

// ── ② 新 schema 无字段可读 / 可注册 / 可签名 ──────────────────────────────────
describe("② the neutral catalog schema is readable, registrable and signable without the field", () => {
  it("reads a neutral bundle and reports no authored classification", async () => {
    const { flat } = writeBundle({ generation: "neutral" });
    const bundle = await readCatalogBundle(flat);
    expect(bundle.schemaGeneration).toBe("neutral");
    // null = "这份 bundle 不声明"，不是 "声明为可训练"。
    expect(bundle.declaredNonTrainable).toBeNull();
  });

  it("registers a neutral bundle fail-closed, with both approval bits false", async () => {
    const { flat } = writeBundle({ generation: "neutral", gameId: "bridge-neutral-register" });
    const bundle = await readCatalogBundle(flat);
    const report = await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: true,
      gameTrainable: false,
    });
    expect(report.game.action).toBe("inserted");
    expect(report.entries.map((entry) => entry.kind).sort()).toEqual([
      "device_profile",
      "game_mapping",
      "game_profile",
      "recording_profile",
    ]);
    expect(report.entries.every((entry) => entry.status === "draft")).toBe(true);
    expect(report.entries.every((entry) => entry.approvedForTrainable === false)).toBe(true);
    const game = await db.query<{ allowed_for_capture: boolean; approved_for_trainable: boolean }>(
      `SELECT allowed_for_capture, approved_for_trainable FROM games WHERE id = $1`,
      ["bridge-neutral-register"],
    );
    // 允许采集 ≠ 允许训练：这正是 issue 的"先上传、后裁决"（§C）。
    expect(game.rows[0]).toEqual({ allowed_for_capture: true, approved_for_trainable: false });
  });

  it("signs a neutral bundle, and the signed manifest carries no retired field", async () => {
    const { contentAddressed } = writeBundle({ generation: "neutral" });
    const signed = await assembleSignedBundle(
      new FilesystemCatalogReleaseSource(contentAddressed),
    );
    const manifest = signedManifest(signed.manifest);
    expect(manifest["schema_version"]).toBe(CATALOG_SCHEMA_VERSIONS.manifest.neutral);
    expect(manifest).not.toHaveProperty(RETIRED_TRAINING_FIELD);
    expect(signed.signature.length).toBeGreaterThan(0);
  });

  it("publishes legacy by default and neutral only when explicitly asked", async () => {
    const { flat } = writeBundle({ generation: "neutral", gameId: "bridge-neutral-publish" });
    const bundle = await readCatalogBundle(flat);
    const registration = await registerCatalogBundle(query, bundle, {
      approve: true,
      approver: "bridge-test@example.com",
      approveTrainable: false,
      allowCapture: true,
      gameTrainable: false,
    });
    const ids = registration.entries.map((entry) => entry.id!).filter(Boolean);

    // 默认必须仍是 v2：部署 PR 1 不得静默改变旧客户端拉到的字节。
    expect(DEFAULT_PUBLISHED_MANIFEST_SCHEMA).toBe(CATALOG_SCHEMA_VERSIONS.manifest.legacy);

    const neutralRelease = await publishCatalogRelease(
      query,
      {
        channel: "dev",
        manifestSchemaVersion: CATALOG_SCHEMA_VERSIONS.manifest.neutral,
        catalogId: "dataminer-bridge-neutral-publish",
        catalogVersion: "2026-08-25.0",
        actor: "bridge-test@example.com",
        trustedCatalogVersionIds: ids,
      },
      // 客户端源校验器要在 Windows 上 cargo run 真正的 catalog.rs；这里只验发布面。
      { validate: async () => undefined },
    );
    const stored = await db.query<{ manifest_bytes: Uint8Array }>(
      `SELECT manifest_bytes FROM catalog_releases WHERE id = $1`,
      [neutralRelease.releaseId],
    );
    const published = JSON.parse(
      Buffer.from(stored.rows[0]!.manifest_bytes).toString("utf8"),
    ) as CatalogManifest & Record<string, unknown>;
    expect(published.schema_version).toBe(CATALOG_SCHEMA_VERSIONS.manifest.neutral);
    expect(published).not.toHaveProperty(RETIRED_TRAINING_FIELD);
  });

  it("refuses a manifest schema version nobody publishes", async () => {
    await expect(
      publishCatalogRelease(
        query,
        {
          channel: "dev",
          manifestSchemaVersion: "dm-catalog-bundle-9" as never,
          catalogId: "dataminer-bridge-bad-schema",
          catalogVersion: "1",
          actor: "bridge-test@example.com",
          trustedCatalogVersionIds: [randomUUID(), randomUUID(), randomUUID(), randomUUID()],
        },
        { validate: async () => undefined },
      ),
    ).rejects.toThrow(/unsupported manifest schema version/);
  });
});

// ── ③ 新 schema 带旧字段被拒绝 ────────────────────────────────────────────────
describe("③ the neutral catalog schema refuses the retired field outright", () => {
  for (const kind of [
    "manifest",
    "device_profile",
    "recording_profile",
    "game_profile",
  ] as const) {
    it(`refuses a neutral ${kind} that resurrects ${RETIRED_TRAINING_FIELD}`, async () => {
      const { flat } = writeBundle({ generation: "neutral", resurrectIn: kind });
      await expect(readCatalogBundle(flat)).rejects.toThrow(
        /must not carry non_trainable; the neutral catalog schema does not express training classification/,
      );
    });
  }

  it("refuses it in the signing path too, not only at registration", async () => {
    const { contentAddressed } = writeBundle({ generation: "neutral", resurrectIn: "manifest" });
    await expect(
      assembleSignedBundle(new FilesystemCatalogReleaseSource(contentAddressed)),
    ).rejects.toThrow(/must not carry non_trainable/);
  });

  it("refuses a half-migrated bundle whose manifest and profiles disagree", async () => {
    // 半迁移是最坏状态：部分读者放行、部分读者拒绝，而错误信息指不到真正的原因。
    const mixedManifest = writeBundle({ generation: "legacy", manifestGeneration: "neutral" });
    await expect(readCatalogBundle(mixedManifest.flat)).rejects.toThrow(
      /mixes catalog schema generations/,
    );
    const mixedProfiles = writeBundle({ generation: "neutral", manifestGeneration: "legacy" });
    await expect(readCatalogBundle(mixedProfiles.flat)).rejects.toThrow(
      /mixes catalog schema generations/,
    );
  });

  it("refuses an unknown schema version instead of guessing a generation", () => {
    expect(() => catalogSchemaGeneration("manifest", "dm-catalog-bundle-1")).toThrow(
      /unsupported catalog manifest schema/,
    );
    expect(() => catalogSchemaGeneration("device_profile", "dm-device-profile-1")).toThrow(
      /unsupported device profile schema/,
    );
    expect(() => catalogSchemaGeneration("manifest", undefined)).toThrow(
      /unsupported catalog manifest schema/,
    );
  });
});
