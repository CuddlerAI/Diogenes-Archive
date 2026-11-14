import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";

import { ClientSourceCatalogValidator } from "./catalog-validator.js";
import { publishCatalogRelease, rosterPathFromArgv } from "./catalog-publisher.js";
import { readCatalogBundle, registerCatalogBundle } from "./register-catalog.js";
import type { CatalogManifest, CatalogQuery, SignedCatalogBundle } from "./signed-catalog.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const migrationsDirectory = path.resolve(here, "../migrations");
const fixtureDirectory = path.resolve(
  here,
  "../../../config/catalogs/dev/bioshock-infinite-steam-release-test",
);

let db: PGlite;
let query: CatalogQuery;
let roster: string[];

beforeAll(async () => {
  db = new PGlite();
  for (const file of (await readdir(migrationsDirectory))
    .filter((name) => /^\d+_[a-z0-9_]+\.sql$/.test(name))
    .sort()) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  query = async (text, params) => {
    const result = await db.query(text, params);
    return { rows: result.rows as Array<Record<string, unknown>> };
  };
  const bundle = await readCatalogBundle(fixtureDirectory);
  const registration = await registerCatalogBundle(query, bundle, {
    approve: false,
    approveTrainable: false,
    allowCapture: false,
    gameTrainable: false,
  });
  roster = registration.entries.map((entry) => {
    if (!entry.id) throw new Error("fixture registration did not return a trusted row id");
    return entry.id;
  });
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("catalog release publisher", () => {
  it("validates exact PostgreSQL bytes before atomically publishing the envelope", async () => {
    const validator = new ClientSourceCatalogValidator();
    const validate = vi.spyOn(validator, "validate");
    const result = await publishCatalogRelease(
      query,
      {
        channel: "dev",
        catalogId: "dataminer-dev-publisher-test",
        catalogVersion: "2026-07-28.publisher-test",
        actor: "publisher-test@example.com",
        trustedCatalogVersionIds: roster,
      },
      validator,
    );
    expect(validate).toHaveBeenCalledTimes(1);
    expect(result.catalogId).toBe("dataminer-dev-publisher-test");
    expect(result.etag).toMatch(/^"sha256-[0-9a-f]{64}"$/);

    const release = await db.query<{
      status: string;
      envelope_bytes: Uint8Array;
      manifest_bytes: Uint8Array;
    }>(
      `SELECT status, envelope_bytes, manifest_bytes
         FROM catalog_releases
        WHERE id = $1`,
      [result.releaseId],
    );
    expect(release.rows[0]!.status).toBe("published");
    const envelope = JSON.parse(
      Buffer.from(release.rows[0]!.envelope_bytes).toString("utf8"),
    ) as { manifest: string; gameProfiles: string[] };
    expect(Buffer.from(envelope.manifest, "base64")).toEqual(
      Buffer.from(release.rows[0]!.manifest_bytes),
    );
    expect(Buffer.from(envelope.gameProfiles[0]!, "base64")).toEqual(
      await readFile(path.join(fixtureDirectory, "game-profile.json")),
    );
  }, 120_000);

  it("registers and publishes the authored KCD2 D3D12 pair through the real client loader", async () => {
    // PGlite owns every write. This deliberately exercises the publisher's real Rust
    // validator: registering the profile hashes alone cannot catch an old reader that
    // rejects dm-depth-profile-3 or camera-binding/kcd2-combined-v1.
    const directory = path.resolve(
      here,
      "../../../config/catalogs/dev/kingdom-come-deliverance-2-steam",
    );
    const bundle = await readCatalogBundle(directory);
    expect(bundle.schemaGeneration).toBe("neutral");
    expect(bundle.declaredNonTrainable).toBeNull();
    const registration = await registerCatalogBundle(query, bundle, {
      approve: false,
      approveTrainable: false,
      allowCapture: false,
      gameTrainable: false,
    });
    expect(registration.entries.map((entry) => entry.kind).sort()).toEqual([
      "camera_binding",
      "depth_profile",
      "device_profile",
      "game_mapping",
      "game_profile",
      "recording_profile",
    ]);
    const trustedIds = registration.entries.map((entry) => {
      expect(entry.status).toBe("draft");
      expect(entry.approvedForTrainable).toBe(false);
      if (!entry.id) throw new Error("KCD2 registration did not return a trusted row id");
      return entry.id;
    });
    const validator = new ClientSourceCatalogValidator();
    const validate = vi.spyOn(validator, "validate");
    const result = await publishCatalogRelease(query, {
      channel: "dev",
      manifestSchemaVersion: "dm-catalog-bundle-3",
      catalogId: bundle.catalogId,
      catalogVersion: bundle.catalogVersion,
      actor: "kcd2-publisher-test@example.invalid",
      trustedCatalogVersionIds: trustedIds,
    }, validator);
    expect(validate).toHaveBeenCalledTimes(1);
    const release = await db.query<{
      status: string;
      envelope_bytes: Uint8Array;
      manifest_bytes: Uint8Array;
    }>(
      "SELECT status, envelope_bytes, manifest_bytes FROM catalog_releases WHERE id = $1",
      [result.releaseId],
    );
    expect(release.rows).toHaveLength(1);
    expect(release.rows[0]!.status).toBe("published");
    const envelope = JSON.parse(
      Buffer.from(release.rows[0]!.envelope_bytes).toString("utf8"),
    ) as SignedCatalogBundle;
    const manifestBytes = Buffer.from(envelope.manifest, "base64");
    expect(manifestBytes).toEqual(Buffer.from(release.rows[0]!.manifest_bytes));
    const manifest = JSON.parse(manifestBytes.toString("utf8")) as CatalogManifest;
    expect(manifest.schema_version).toBe("dm-catalog-bundle-3");
    expect(manifest.catalog_id).toBe(bundle.catalogId);
    expect(manifest.catalog_version).toBe(bundle.catalogVersion);
    expect(manifest).not.toHaveProperty("non_trainable");
    // All six exact payloads must survive registry -> immutable envelope, including
    // the optional D3D12 pair that older publishers could omit or reject.
    for (const [payloads, filename] of [
      [envelope.deviceProfiles, "device-profile.json"],
      [envelope.recordingProfiles, "recording-profile.json"],
      [envelope.gameProfiles, "game-profile.json"],
      [envelope.mappings, "mapping.json"],
      [envelope.depthProfiles, "depth-profile.json"],
      [envelope.cameraBindings, "camera-binding.json"],
    ] as const) {
      expect(payloads).toHaveLength(1);
      expect(Buffer.from(payloads![0]!, "base64")).toEqual(
        await readFile(path.join(directory, filename)),
      );
    }
    expect(JSON.parse(Buffer.from(envelope.depthProfiles![0]!, "base64").toString("utf8")))
      .toMatchObject({ schema_version: "dm-depth-profile-3", renderer_backend: "d3d12" });
    expect(JSON.parse(Buffer.from(envelope.cameraBindings![0]!, "base64").toString("utf8")))
      .toMatchObject({
        schema: "camera-binding/kcd2-combined-v2",
        binding_version: "2",
        live_pose_plan: { schema: "kcd2-d3d12-live-pose-plan/v3" },
      });
    const publishedGame = JSON.parse(Buffer.from(envelope.gameProfiles[0]!, "base64").toString("utf8"));
    expect(publishedGame.input_contract).toMatchObject({
      control_vks: [119, 121],
      mouse_buttons: [1, 2, 4],
      capture_mouse_move: true,
      capture_wheel: true,
    });
    expect(publishedGame.input_contract.keyboard_vks).toEqual(expect.arrayContaining([
      16, 17, 18, 20, 32, 37, 38, 39, 40, 65, 67, 68, 83, 87,
    ]));
    const publishedMapping = JSON.parse(Buffer.from(envelope.mappings[0]!, "base64").toString("utf8"));
    expect(publishedMapping.bindings).toEqual(expect.arrayContaining([
      { device: "keyboard", scan_code: 31, extended: false, semantic: "move.backward" },
      { device: "keyboard", scan_code: 72, extended: true, semantic: "move.forward" },
      { device: "keyboard", scan_code: 46, extended: false, semantic: "move.toggle_crouch" },
      { device: "mouse", control: "delta_x", semantic: "camera.yaw" },
    ]));
    expect(publishedMapping.bindings.some((binding: { button?: number; control?: string }) =>
      binding.button !== undefined || binding.control === "wheel")).toBe(false);
    const game = await db.query<{ allowed_for_capture: boolean; approved_for_trainable: boolean }>(
      "SELECT allowed_for_capture, approved_for_trainable FROM games WHERE id = $1",
      [bundle.game.id],
    );
    expect(game.rows).toEqual([{ allowed_for_capture: false, approved_for_trainable: false }]);
  }, 120_000);

  it("rejects a legacy JSONB-only trusted row before signing or release insertion", async () => {
    const legacyId = "00000000-0000-4000-8000-000000000099";
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot
       ) VALUES (
         $1, 'game_profile', 'legacy-jsonb-only', '1',
         'bioshock-infinite-steam-release-test',
         'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
         '{"legacy":true}'::jsonb
       )`,
      [legacyId],
    );
    const validator = { validate: vi.fn(async () => undefined) };
    await expect(
      publishCatalogRelease(
        query,
        {
          channel: "dev",
          catalogId: "dataminer-dev-publisher-test",
          catalogVersion: "2026-07-28.legacy-refused",
          actor: "publisher-test@example.com",
          trustedCatalogVersionIds: [...roster.filter((id) => id !== roster[2]), legacyId],
        },
        validator,
      ),
    ).rejects.toThrow(/exact payload/);
    expect(validator.validate).not.toHaveBeenCalled();
  });

  // issue #829 的验收：certified 发布侧退役后，一个**显式**发 certified 的
  // roster 必须被点名拒绝，而不是静默当成 dev 发出去。静默有真实代价——那个
  // 调用方 pin 的是已退役的 KMS 锚，收到 dev 字节只会死在一句 signature
  // verification failed 上，排障顺着签名去查，真相却是渠道整条没有了。
  //
  // 走 `as unknown as` 是有意的：`CatalogChannel` 已经是单值，类型层面写不出
  // 这个调用；而这条路真实存在——`publish-catalog.ts` 把 roster 文件
  // `JSON.parse(...) as CatalogPublishRequest`，类型在那里挡不住任何东西。
  it("refuses an explicit certified roster by name instead of publishing it as dev", async () => {
    const validator = { validate: vi.fn(async () => undefined) };
    const before = await db.query<{ count: string }>(
      "SELECT count(*)::text AS count FROM catalog_releases",
    );

    await expect(
      publishCatalogRelease(
        query,
        {
          channel: "certified",
          catalogId: "dataminer-certified-cod-steam",
          catalogVersion: "2026-08-20.retired-channel",
          actor: "publisher-test@example.com",
          trustedCatalogVersionIds: roster,
        } as unknown as Parameters<typeof publishCatalogRelease>[1],
        validator,
      ),
    ).rejects.toThrow(/certified catalog channel was retired/);

    // 拒绝发生在任何副作用之前：没验证、没签名、没写行。
    expect(validator.validate).not.toHaveBeenCalled();
    const after = await db.query<{ count: string }>(
      "SELECT count(*)::text AS count FROM catalog_releases",
    );
    expect(after.rows[0]!.count).toBe(before.rows[0]!.count);
  });

  it("refuses an unknown channel too, without naming it a retirement", async () => {
    await expect(
      publishCatalogRelease(query, {
        channel: "prod",
        catalogId: "dataminer-dev-publisher-test",
        catalogVersion: "2026-08-20.unknown-channel",
        actor: "publisher-test@example.com",
        trustedCatalogVersionIds: roster,
      } as unknown as Parameters<typeof publishCatalogRelease>[1]),
    ).rejects.toThrow(/unsupported catalog channel: prod/);
  });
});

// docs/36 记的发布命令曾经 100% 失败：pnpm 把 `--` 分隔符原样透传，脚本收到
// `[node, script, '--', roster]`，而旧代码取 argv[2]（拿到 `--`）并要求长度恰好
// 为 3。没有任何测试执行过那层 CLI，下发链路也从未在生产上跑过，所以一直没人
// 发现。这组用例把**真实的命令行形状**钉死。
describe("publish:catalog argv parsing", () => {
  const argv = (...rest: string[]) => ["/usr/bin/node", "/repo/scripts/publish-catalog.ts", ...rest];

  it("accepts the documented pnpm form, where `--` reaches the script", () => {
    expect(rosterPathFromArgv(argv("--", "./release-roster.json"))).toBe("./release-roster.json");
  });

  it("accepts the bare form, for pnpm versions that swallow the separator", () => {
    expect(rosterPathFromArgv(argv("./release-roster.json"))).toBe("./release-roster.json");
  });

  it("still refuses anything but exactly one roster", () => {
    // 发布不可逆：多一个杂参该拒绝，不该猜是哪个。
    expect(() => rosterPathFromArgv(argv())).toThrow(/usage/);
    expect(() => rosterPathFromArgv(argv("--"))).toThrow(/usage/);
    expect(() => rosterPathFromArgv(argv("a.json", "b.json"))).toThrow(/usage/);
  });
});
