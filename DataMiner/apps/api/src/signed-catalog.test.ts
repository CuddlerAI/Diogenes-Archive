import { createHash, createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";

import { describe, expect, it, vi } from "vitest";

import { buildApp } from "./app.js";
import { ClientSourceCatalogValidator } from "./catalog-validator.js";
import type { AppConfig } from "./config.js";
import { environmentProfile, primaryFamily } from "./environment.js";
import type { CurrentCatalogRelease, IngestionRepository } from "./domain.js";
import type { ObjectStorage } from "./storage.js";
import { catalogSetEtag, serializeCatalogSet } from "./catalog-delivery.js";
import {
  CATALOG_SIGNATURE_DOMAIN,
  DEV_SIGNING_KEY_ID,
  FilesystemCatalogReleaseSource,
  type CatalogPayloads,
  assembleSignedBundle,
  devSigningPublicKeyHex,
  serializeSignedBundle,
  signedBundleEtag,
} from "./signed-catalog.js";

// Must equal the client's pinned DEV_CATALOG_ANCHOR (catalog.rs). If this drifts,
// the client rejects every bundle this server signs — so the test pins it here.
const DEV_CATALOG_ANCHOR_HEX =
  "585316e5a8a6c41c03d280bfe1409e41a3d5f3c9083337bb78f1e62696c39f82";

const COD_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../config/catalogs/dev/cod-steam-release-test",
);
const FALLOUT4_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../config/catalogs/dev/fallout4-steam-release-test",
);

function writeDepthContentAddressedFixture(): {
  directory: string;
  manifest: Buffer;
  depthProfile: Buffer;
  cameraBinding: Buffer;
  depthDigest: string;
} {
  const directory = mkdtempSync(path.join(tmpdir(), "dm-depth-catalog-"));
  const payloadDirectory = path.join(directory, "payloads", "sha256");
  mkdirSync(payloadDirectory, { recursive: true });

  const depthProfile = readFileSync(path.join(FALLOUT4_DIR, "depth-profile.json"));
  const cameraBinding = readFileSync(path.join(FALLOUT4_DIR, "camera-binding.json"));
  const depth = JSON.parse(depthProfile.toString("utf8")) as {
    game_id: string;
  };
  const binding = JSON.parse(cameraBinding.toString("utf8")) as {
    binding_id: string;
    binding_version: string;
  };
  const depthDigest = createHash("sha256").update(depthProfile).digest("hex");
  const bindingDigest = createHash("sha256").update(cameraBinding).digest("hex");
  writeFileSync(path.join(payloadDirectory, `${depthDigest}.json`), depthProfile);
  writeFileSync(path.join(payloadDirectory, `${bindingDigest}.json`), cameraBinding);

  const manifest = Buffer.from(
    JSON.stringify({
      schema_version: "dm-catalog-bundle-2",
      catalog_id: "dataminer-dev-depth-fixture",
      catalog_version: "2026-08-13.0",
      non_trainable: true,
      device_profiles: [],
      recording_profiles: [],
      game_profiles: [],
      mappings: [],
      depth_profiles: [
        {
          id: depth.game_id,
          version: "1",
          content_hash: `sha256:${depthDigest}`,
        },
      ],
      camera_bindings: [
        {
          id: binding.binding_id,
          version: binding.binding_version,
          content_hash: `sha256:${bindingDigest}`,
        },
      ],
    }),
  );
  writeFileSync(path.join(directory, "catalog.json"), manifest);
  return { directory, manifest, depthProfile, cameraBinding, depthDigest };
}

function writeFiveGameContentAddressedFixture(): string {
  const directory = mkdtempSync(path.join(tmpdir(), "dm-five-game-catalog-"));
  const payloadDirectory = path.join(directory, "payloads", "sha256");
  mkdirSync(payloadDirectory, { recursive: true });
  const manifest = {
    schema_version: "dm-catalog-bundle-2",
    catalog_id: "dataminer-dev-five-game-fixture",
    catalog_version: "2026-07-28.d2-fixture",
    non_trainable: true,
    device_profiles: [] as Array<{ id: string; version: string; content_hash: string }>,
    recording_profiles: [] as Array<{ id: string; version: string; content_hash: string }>,
    game_profiles: [] as Array<{ id: string; version: string; content_hash: string }>,
    mappings: [] as Array<{ id: string; version: string; content_hash: string }>,
    depth_profiles: [] as Array<{ id: string; version: string; content_hash: string }>,
    camera_bindings: [] as Array<{ id: string; version: string; content_hash: string }>,
  };
  const baseFiles = {
    device_profiles: "device-profile.json",
    recording_profiles: "recording-profile.json",
    game_profiles: "game-profile.json",
    mappings: "mapping.json",
  } as const;
  const bases = Object.fromEntries(
    Object.entries(baseFiles).map(([category, file]) => [
      category,
      JSON.parse(readFileSync(path.join(COD_DIR, "..", "bioshock-infinite-steam-release-test", file), "utf8")),
    ]),
  ) as Record<keyof typeof baseFiles, Record<string, unknown>>;

  for (let number = 1; number <= 5; number += 1) {
    const gameId = `d2-five-game-fixture-${number}`;
    const deviceId = `${gameId}-device`;
    const recordingId = `${gameId}-recording`;
    const mappingId = `game.${gameId}.test.pc`;
    const payloadObjects = structuredClone(bases);
    payloadObjects.device_profiles.profile_id = deviceId;
    payloadObjects.device_profiles.evidence_class = "d2_multi_game_builder_fixture";
    payloadObjects.recording_profiles.profile_id = recordingId;
    payloadObjects.game_profiles.game_id = gameId;
    payloadObjects.game_profiles.display_name = `D2 fixture game ${number}`;
    payloadObjects.game_profiles.device_profile_id = deviceId;
    payloadObjects.game_profiles.recording_profile_id = recordingId;
    payloadObjects.game_profiles.mapping_id = mappingId;
    payloadObjects.recording_profiles.game_id = gameId;
    payloadObjects.mappings.mapping_id = mappingId;
    payloadObjects.mappings.recording_profile_id = recordingId;
    (payloadObjects.mappings.game as Record<string, unknown>).id = gameId;
    (payloadObjects.mappings.game as Record<string, unknown>).name = `D2 fixture game ${number}`;

    for (const category of Object.keys(baseFiles) as Array<keyof typeof baseFiles>) {
      const bytes = Buffer.from(JSON.stringify(payloadObjects[category]));
      const digest = createHash("sha256").update(bytes).digest("hex");
      writeFileSync(path.join(payloadDirectory, `${digest}.json`), bytes);
      const id =
        category === "device_profiles"
          ? deviceId
          : category === "recording_profiles"
            ? recordingId
            : category === "game_profiles"
              ? gameId
              : mappingId;
      // 有 version 字段就取 payload 自己写的那个。曾经对 device/capture/settings
      // 一律硬编码 "1"，于是上游 bundle 一 bump 版本，manifest 与 payload 就对不上，
      // 报出来的却是含糊的 "device profile identity mismatch"。
      //
      // capture-profile.json 是唯一没有 version 字段的 payload（客户端也不拿它比
      // 版本），manifest 侧沿用既有的 "1"——直接 String(undefined) 会写进
      // 字面量 "undefined"，而且因为没人比对它，还不会有任何测试报错。
      const declared = payloadObjects[category].version;
      const version = declared === undefined ? "1" : String(declared);
      manifest[category].push({ id, version, content_hash: `sha256:${digest}` });
    }
  }
  for (const category of Object.keys(baseFiles) as Array<keyof typeof baseFiles>) {
    manifest[category].sort(
      (left, right) => left.id.localeCompare(right.id) || left.version.localeCompare(right.version),
    );
  }
  writeFileSync(path.join(directory, "catalog.json"), Buffer.from(JSON.stringify(manifest)));
  return directory;
}

function required<T>(value: T | undefined): T {
  if (value === undefined) {
    throw new Error("expected a defined value");
  }
  return value;
}

function ed25519PublicKeyFromRawHex(hex: string) {
  // SPKI DER prefix for an Ed25519 public key (RFC 8410), then the 32 raw bytes.
  const prefix = Buffer.from("302a300506032b6570032100", "hex");
  const der = Buffer.concat([prefix, Buffer.from(hex, "hex")]);
  return createPublicKey({ key: der, format: "der", type: "spki" });
}

describe("dev catalog signer", () => {
  it("derives the public key the client pins as DEV_CATALOG_ANCHOR", () => {
    expect(devSigningPublicKeyHex()).toBe(DEV_CATALOG_ANCHOR_HEX);
  });

  it("carries optional depth profile and camera binding bytes in the signed envelope", async () => {
    const bundle = await assembleSignedBundle(FALLOUT4_DIR);
    expect(bundle.depthProfiles).toHaveLength(1);
    expect(bundle.cameraBindings).toHaveLength(1);
    expect(Buffer.from(required(bundle.depthProfiles)[0]!, "base64")).toEqual(
      readFileSync(path.join(FALLOUT4_DIR, "depth-profile.json")),
    );
    expect(Buffer.from(required(bundle.cameraBindings)[0]!, "base64")).toEqual(
      readFileSync(path.join(FALLOUT4_DIR, "camera-binding.json")),
    );
  });

  it("assembles a bundle whose manifest signature verifies under the pinned anchor", async () => {
    const bundle = await assembleSignedBundle(COD_DIR);
    expect(bundle.schemaVersion).toBe("dm-signed-catalog-bundle-1");

    const manifest = Buffer.from(bundle.manifest, "base64");
    // manifest 不再自称 mode（docs/57 §3.3）：信任锚由验签的那把公钥决定。
    // COD 这份 authored bundle 随 issue #855 PR 2 断代到中性一代，因此连渠道派生的
    // `non_trainable` 也不再携带——「这份 bundle 从哪条渠道来」现在完全由下面那次
    // 验签回答，而签名比声明强：声明可以撒谎，签名不能。
    const parsed = JSON.parse(manifest.toString("utf8")) as Record<string, unknown>;
    expect(parsed.catalog_mode).toBeUndefined();
    expect(parsed.schema_version).toBe("dm-catalog-bundle-3");
    expect(parsed).not.toHaveProperty("non_trainable");

    const input = Buffer.concat([CATALOG_SIGNATURE_DOMAIN, manifest]);
    const signature = Buffer.from(bundle.signature, "base64");
    expect(signature.length).toBe(64);
    const ok = verify(null, input, ed25519PublicKeyFromRawHex(DEV_CATALOG_ANCHOR_HEX), signature);
    expect(ok).toBe(true);
  });

  it("keeps legacy envelopes byte-stable by omitting empty optional depth categories", async () => {
    const bundle = await assembleSignedBundle(COD_DIR);
    expect(bundle).not.toHaveProperty("depthProfiles");
    expect(bundle).not.toHaveProperty("cameraBindings");
    const serialized = JSON.parse(serializeSignedBundle(bundle).toString("utf8")) as Record<
      string,
      unknown
    >;
    expect(serialized).not.toHaveProperty("depthProfiles");
    expect(serialized).not.toHaveProperty("cameraBindings");
  });

  it.each(["depth_profiles", "camera_bindings"] as const)(
    "rejects present-but-non-array %s instead of treating it as absent",
    async (category) => {
      const manifest = JSON.parse(
        readFileSync(path.join(COD_DIR, "catalog.json"), "utf8"),
      ) as Record<string, unknown>;
      manifest[category] = null;
      const source = {
        readManifest: async () => Buffer.from(JSON.stringify(manifest)),
        readPayload: vi.fn(async () => Buffer.alloc(0)),
      };

      await expect(assembleSignedBundle(source)).rejects.toThrow(
        new RegExp(`non-array ${category}`),
      );
      expect(source.readPayload).not.toHaveBeenCalled();
    },
  );

  it("signs the original manifest bytes and delivers exact depth payload bytes", async () => {
    const fixture = writeDepthContentAddressedFixture();
    const validate = vi.fn(async (_manifest: Buffer, _payloads: CatalogPayloads) => undefined);
    const validator = { validate };
    const bundle = await assembleSignedBundle(
      new FilesystemCatalogReleaseSource(fixture.directory),
      validator,
    );

    expect(Buffer.from(bundle.manifest, "base64")).toEqual(fixture.manifest);
    expect(Buffer.from(required(bundle.depthProfiles?.[0]), "base64")).toEqual(
      fixture.depthProfile,
    );
    expect(Buffer.from(required(bundle.cameraBindings?.[0]), "base64")).toEqual(
      fixture.cameraBinding,
    );
    const payloads = required(validate.mock.calls[0]?.[1]);
    expect(payloads.depth_profiles[0]).toEqual(fixture.depthProfile);
    expect(payloads.camera_bindings[0]).toEqual(fixture.cameraBinding);
    expect(
      verify(
        null,
        Buffer.concat([CATALOG_SIGNATURE_DOMAIN, fixture.manifest]),
        ed25519PublicKeyFromRawHex(DEV_CATALOG_ANCHOR_HEX),
        Buffer.from(bundle.signature, "base64"),
      ),
    ).toBe(true);
  });

  it("validates and signs the authored Fallout 4 depth pair through the client loader", async () => {
    const bundle = await assembleSignedBundle(FALLOUT4_DIR, new ClientSourceCatalogValidator());
    expect(Buffer.from(required(bundle.depthProfiles?.[0]), "base64")).toEqual(
      readFileSync(path.join(FALLOUT4_DIR, "depth-profile.json")),
    );
    expect(Buffer.from(required(bundle.cameraBindings?.[0]), "base64")).toEqual(
      readFileSync(path.join(FALLOUT4_DIR, "camera-binding.json")),
    );
  }, 120_000);

  it("refuses a depth payload whose exact filesystem bytes no longer match the manifest", async () => {
    const fixture = writeDepthContentAddressedFixture();
    writeFileSync(
      path.join(fixture.directory, "payloads", "sha256", `${fixture.depthDigest}.json`),
      Buffer.from("{\"tampered\":true}"),
    );
    await expect(
      assembleSignedBundle(new FilesystemCatalogReleaseSource(fixture.directory)),
    ).rejects.toThrow(/depth_profiles .* content hash .* does not match manifest/);
  });

  it("refuses a depth payload path that is a symlink", async () => {
    const fixture = writeDepthContentAddressedFixture();
    const payloadPath = path.join(
      fixture.directory,
      "payloads",
      "sha256",
      `${fixture.depthDigest}.json`,
    );
    rmSync(payloadPath);
    const targetDirectory = path.join(fixture.directory, "symlink-target");
    mkdirSync(targetDirectory);
    symlinkSync(targetDirectory, payloadPath, "junction");

    await expect(
      assembleSignedBundle(new FilesystemCatalogReleaseSource(fixture.directory)),
    ).rejects.toThrow(/must not be a symlink/i);
  });

  it("emits profile bytes that match the manifest content hashes", async () => {
    const bundle = await assembleSignedBundle(COD_DIR);
    const parsed = JSON.parse(Buffer.from(bundle.manifest, "base64").toString("utf8")) as {
      device_profiles: { content_hash: string }[];
      game_profiles: { content_hash: string }[];
    };
    const deviceBytes = Buffer.from(required(bundle.deviceProfiles[0]), "base64");
    expect(`sha256:${createHash("sha256").update(deviceBytes).digest("hex")}`).toBe(
      required(parsed.device_profiles[0]).content_hash,
    );
    const gameBytes = Buffer.from(required(bundle.gameProfiles[0]), "base64");
    expect(`sha256:${createHash("sha256").update(gameBytes).digest("hex")}`).toBe(
      required(parsed.game_profiles[0]).content_hash,
    );
  });

  it("builds the same five-game envelope and ETag from the strict source twice", async () => {
    const directory = writeFiveGameContentAddressedFixture();
    const source = new FilesystemCatalogReleaseSource(directory);
    const validator = new ClientSourceCatalogValidator();
    const first = serializeSignedBundle(await assembleSignedBundle(source, validator));
    const second = serializeSignedBundle(await assembleSignedBundle(source));
    expect(second).toEqual(first);
    expect(signedBundleEtag(second)).toBe(signedBundleEtag(first));
    // 金值随 fixture 的源字节走：五本册子收成四本（docs/57 §3.1）、manifest
    // 去掉 catalog_mode 之后它必然再变一次。
    expect(signedBundleEtag(first)).toBe(
      '"sha256-41948c34ff304baf0b588a4b85c33f54f9dff2128cd83610697479973e6b4159"',
    );
    expect(JSON.parse(first.toString("utf8")).gameProfiles).toHaveLength(5);
  }, 120_000);
});

// Minimal auth harness mirroring app.test.ts: exactly one token authenticates.
const token = "device-credential-with-at-least-32-bytes";
const tokenFingerprint = createHash("sha256").update(token).digest("hex");

const config: AppConfig = {
  environment: "staging",
  profile: environmentProfile("staging"),
  admittedFamilies: [
    {
      family: primaryFamily(environmentProfile("staging")),
      prefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
    },
  ],
  databaseUrl: "postgresql://example:example@localhost/dataminer",
  proxyToken: null,
  host: "127.0.0.1",
  port: 8080,
  r2: {
    accountId: "account",
    bucket: "bucket",
    accessKeyId: "access",
    secretAccessKey: "secret",
    prefix: "raw-staging/v1/non-trainable/dataminer-api-staging",
    presignTtlSeconds: 900,
    partSizeBytes: 64 * 1024 * 1024,
    requestTimeoutSeconds: 120,
  },
  worker: {
    batchSize: 4,
    leaseSeconds: 900,
    purgeBatchSize: 4,
    purgeLeaseSeconds: 900,
    multipartCompletionLeaseSeconds: 300,
    orphanMultipartGraceSeconds: 1_200,
  },
  retentionDays: 30,
  logsExport: { enabled: false, bucket: null, batchSize: 200, dryRun: false },
  unconsentedTelemetryTtlDays: 30,
};

function harness(
  currentReleases: CurrentCatalogRelease[],
  appConfig: AppConfig = config,
) {
  const currentCatalogRelease = vi.fn(async () => currentReleases);
  const repository = {
    ping: vi.fn(async () => undefined),
    authenticateDevice: vi.fn(async (credentialFingerprint: string) =>
      credentialFingerprint === tokenFingerprint
        ? {
            id: "11111111-1111-4111-8111-111111111111",
            annotatorId: "22222222-2222-4222-8222-222222222222",
            credentialFingerprint,
          }
        : null,
    ),
    currentCatalogRelease,
  } as unknown as IngestionRepository;
  const storage = { ping: vi.fn(async () => undefined) } as unknown as ObjectStorage;
  return { app: buildApp({ config: appConfig, repository, storage }), currentCatalogRelease };
}

async function publishedDevRelease(): Promise<CurrentCatalogRelease> {
  const envelopeBytes = serializeSignedBundle(await assembleSignedBundle(COD_DIR));
  return {
    catalogId: "dataminer-dev-cod-steam",
    catalogVersion: "2026-07-28.3",
    envelopeBytes,
    envelopeSha256: `sha256:${createHash("sha256").update(envelopeBytes).digest("hex")}`,
  };
}

describe("GET /v1/catalog", () => {
  const compatibleHeaders = {
    authorization: `Bearer ${token}`,
    "x-dataminer-catalog-schema-max": "2",
    "x-dataminer-client-version": "0.5.1",
  };

  it("filters KCD2 for old devices before ETag while preserving the signed bundle bytes", async () => {
    const previous = await publishedDevRelease();
    const kcd2Directory = path.join(COD_DIR, "..", "kingdom-come-deliverance-2-steam");
    const envelopeBytes = serializeSignedBundle(await assembleSignedBundle(kcd2Directory));
    const kcd2: CurrentCatalogRelease = {
      catalogId: "dataminer-kingdom-come-deliverance-2-steam",
      catalogVersion: (JSON.parse(readFileSync(path.join(kcd2Directory, "catalog.json"), "utf8")) as { catalog_version: string }).catalog_version,
      envelopeBytes, envelopeSha256: `sha256:${createHash("sha256").update(envelopeBytes).digest("hex")}`,
    };
    const { app } = harness([previous, kcd2]);
    const oldHeaders = { ...compatibleHeaders, "x-dataminer-client-version": "0.7.20" };
    const oldBody = serializeCatalogSet([previous]);
    const old = await app.inject({ method: "GET", url: "/v1/catalog", headers: oldHeaders });
    expect(old.statusCode).toBe(200);
    expect(old.rawPayload).toEqual(oldBody);
    expect(old.headers.etag).toBe(catalogSetEtag(oldBody));
    const cached = await app.inject({ method: "GET", url: "/v1/catalog", headers: { ...oldHeaders, "if-none-match": old.headers.etag as string } });
    expect(cached.statusCode).toBe(304);
    for (const version of ["0.7.21", "0.7.22", "0.7.23"]) {
      const unsupported = await app.inject({ method: "GET", url: "/v1/catalog", headers: { ...oldHeaders, "x-dataminer-client-version": version, "if-none-match": old.headers.etag as string } });
      expect(unsupported.statusCode).toBe(304);
    }
    const current = await app.inject({ method: "GET", url: "/v1/catalog", headers: { ...oldHeaders, "x-dataminer-client-version": "0.7.24", "if-none-match": old.headers.etag as string } });
    expect(current.statusCode).toBe(200);
    expect(current.json().bundles).toHaveLength(2);
    expect(Buffer.from(current.json().bundles.find((entry: { catalogId: string }) => entry.catalogId === kcd2.catalogId).envelope, "base64")).toEqual(envelopeBytes);
    for (const invalid of [{ "x-dataminer-catalog-schema-max": "2" }, { "x-dataminer-catalog-schema-max": "2", "x-dataminer-client-version": "bad-version" }]) {
      const response = await app.inject({ method: "GET", url: "/v1/catalog", headers: { authorization: `Bearer ${token}`, ...invalid, "if-none-match": "*" } });
      expect(response.statusCode).toBe(409);
    }
    await app.close();
  });

  it("serves the exact immutable envelope and transport metadata", async () => {
    const release = await publishedDevRelease();
    const { app } = harness([release]);
    const response = await app.inject({
      method: "GET",
      url: "/v1/catalog",
      headers: compatibleHeaders,
    });
    expect(response.statusCode).toBe(200);
    expect(response.headers["cache-control"]).toBe("private, no-cache");
    expect(response.headers["x-dataminer-catalog-count"]).toBe("1");
    // 渠道头随 certified 车道退役（docs/100 §4.3），Vary 里不该再留着它——
    // 一个还在按它分片的缓存层等于在为一条不存在的维度保留键空间。
    expect(response.headers.vary).not.toContain("X-DataMiner-Catalog-Channel");
    const set = response.json();
    expect(set.schemaVersion).toBe("dm-signed-catalog-set-1");
    expect(set.bundles).toHaveLength(1);
    expect(set.bundles[0].catalogId).toBe("dataminer-dev-cod-steam");
    expect(set.bundles[0].catalogVersion).toBe("2026-07-28.3");
    // 原样字节：集合里嵌的必须与库里那份逐字节相同，不能经过一次重新序列化。
    expect(Buffer.from(set.bundles[0].envelope, "base64")).toEqual(release.envelopeBytes);
    const bundle = JSON.parse(
      Buffer.from(set.bundles[0].envelope, "base64").toString("utf8"),
    ) as { schemaVersion: string; manifest: string; signature: string };
    expect(bundle.schemaVersion).toBe("dm-signed-catalog-bundle-1");
    const input = Buffer.concat([
      CATALOG_SIGNATURE_DOMAIN,
      Buffer.from(bundle.manifest, "base64"),
    ]);
    const ok = verify(
      null,
      input,
      ed25519PublicKeyFromRawHex(DEV_CATALOG_ANCHOR_HEX),
      Buffer.from(bundle.signature, "base64"),
    );
    expect(ok).toBe(true);
  });

  it("returns 304 without a body when If-None-Match includes the current ETag", async () => {
    const release = await publishedDevRelease();
    // ETag 现在覆盖的是**整个集合**的字节，不再是单份 envelope 的 sha256
    // （docs/57 §3.3：每渠道返回全部已发布 bundle）。
    const etag = catalogSetEtag(serializeCatalogSet([release]));
    const response = await harness([release]).app.inject({
      method: "GET",
      url: "/v1/catalog",
      headers: { ...compatibleHeaders, "if-none-match": `"older", W/${etag}` },
    });
    expect(response.statusCode).toBe(304);
    expect(response.rawPayload).toHaveLength(0);
    expect(response.headers.etag).toBe(etag);
  });

  it.each([
    { headers: {}, label: "missing compatibility headers" },
    {
      headers: {
        "x-dataminer-catalog-schema-max": "0",
        "x-dataminer-client-version": "0.5.1",
      },
      label: "unsupported schema",
    },
    {
      headers: {
        "x-dataminer-catalog-schema-max": "2",
        "x-dataminer-client-version": "0.4.9",
      },
      label: "old client",
    },
  ])("returns 409 for $label", async ({ headers }) => {
    const release = await publishedDevRelease();
    const response = await harness([release]).app.inject({
      method: "GET",
      url: "/v1/catalog",
      headers: { authorization: `Bearer ${token}`, ...headers },
    });
    expect(response.statusCode).toBe(409);
    expect(response.json().code).toBe("catalog_incompatible");
    expect(response.headers.etag).toBeDefined();
  });

  // 漏带一个头和"客户端太老"在契约上是同一个 409，但排障动作完全不同：前者改
  // 调用方，后者升级客户端。本地排练下发链路时，笼统的"目录与客户端不兼容"
  // 让一次漏带请求头被读成了服务端发错了东西——message 必须指名道姓。
  it.each([
    {
      label: "missing schema-max header",
      headers: { "x-dataminer-client-version": "0.7.0" },
      expected: /X-DataMiner-Catalog-Schema-Max is required/,
    },
    {
      label: "missing client-version header",
      headers: { "x-dataminer-catalog-schema-max": "2" },
      expected: /X-DataMiner-Client-Version is required/,
    },
    {
      label: "client older than the minimum",
      headers: { "x-dataminer-catalog-schema-max": "2", "x-dataminer-client-version": "0.4.9" },
      expected: /older than the minimum/,
    },
  ])("names what is wrong for $label", async ({ headers, expected }) => {
    const release = await publishedDevRelease();
    const response = await harness([release]).app.inject({
      method: "GET",
      url: "/v1/catalog",
      headers: { authorization: `Bearer ${token}`, ...headers },
    });
    expect(response.statusCode).toBe(409);
    expect(response.json().message).toMatch(expected);
  });

  it("requires a device credential", async () => {
    const { app } = harness([await publishedDevRelease()]);
    const response = await app.inject({ method: "GET", url: "/v1/catalog" });
    expect(response.statusCode).toBe(401);
  });

  // #323 在 console 面加了一条标注员会话鉴权的绑定前通道。这条通道的存在**不得**
  // 让摄取面这条路松动一分：这里的凭据依然只认设备指纹，任何别的持有物——包括一个
  // 有效的标注员会话——在这里仍然什么都不是。响应体也不能因此泄露出更多东西。
  it("still refuses anything that is not a device credential, bootstrap notwithstanding", async () => {
    const { app, currentCatalogRelease } = harness([await publishedDevRelease()]);
    for (const bearer of ["an-annotator-session-token", "", "Bearer-looking-but-wrong"]) {
      const response = await app.inject({
        method: "GET",
        url: "/v1/catalog",
        headers: { ...compatibleHeaders, authorization: `Bearer ${bearer}` },
      });
      expect(response.statusCode).toBe(401);
      expect(response.json().code).toBe("invalid_device_credential");
    }
    // 鉴权在目录查找之前：一个未授权的调用方连"有没有发布"都不该问出来。
    expect(currentCatalogRelease).not.toHaveBeenCalled();
  });

  it("is fail-closed with 503 when the channel has no published release", async () => {
    const { app } = harness([]);
    const response = await app.inject({
      method: "GET",
      url: "/v1/catalog",
      headers: compatibleHeaders,
    });
    expect(response.statusCode).toBe(503);
    expect(response.json().code).toBe("catalog_not_published");
  });

  // 渠道按部署推导（docs/100 §4.3）：只剩一条渠道时，「客户端说自己要哪条」没有
  // 第二个答案。不带头与带对了头都照常服务。
  it("serves the deployment channel with or without the header", async () => {
    for (const header of [undefined, "dev"]) {
      const { app, currentCatalogRelease } = harness([]);
      await app.inject({
        method: "GET",
        url: "/v1/catalog",
        headers: header
          ? { ...compatibleHeaders, "x-dataminer-catalog-channel": header }
          : compatibleHeaders,
      });
      expect(currentCatalogRelease).toHaveBeenCalledWith("dev");
    }
  });

  // docs/100 §7 的负面用例：一个仍然声明 certified 的旧客户端必须拿到**明确的
  // 4xx**，既不是 500，也不能被静默当成 dev。它 pin 的是那把已退役的 KMS 锚，
  // 静默发它 dev 字节只会让它死在一句 "signature verification failed" 上——排障
  // 会顺着签名去查，而真相是那条渠道整条没有了。
  it("refuses a retired or unknown channel instead of silently serving dev", async () => {
    for (const channel of ["certified", "prod"]) {
      const { app, currentCatalogRelease } = harness([await publishedDevRelease()]);
      const response = await app.inject({
        method: "GET",
        url: "/v1/catalog",
        headers: { ...compatibleHeaders, "x-dataminer-catalog-channel": channel },
      });
      expect(response.statusCode).toBe(400);
      expect(response.json().code).toBe("invalid_catalog_channel");
      expect(response.json().message).toMatch(/retired/);
      expect(currentCatalogRelease).not.toHaveBeenCalled();
    }
  });
});
