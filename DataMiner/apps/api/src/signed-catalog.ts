import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";
import { lstat, readFile, realpath, stat } from "node:fs/promises";
import path from "node:path";

import {
  catalogSchemaGeneration,
  readDeclaredNonTrainable,
  type CatalogSchemaGeneration,
} from "./catalog-schema.js";

// MUST byte-match the client's CATALOG_SIGNATURE_DOMAIN (catalog.rs).
export const CATALOG_SIGNATURE_DOMAIN = Buffer.from("dm-signed-catalog-1\0", "latin1");
export const DEPTH_CERTIFICATION_SIGNATURE_DOMAIN = Buffer.from(
  "dm-signed-depth-certifications-1\0",
  "latin1",
);
const DEV_DELIVERY_SEED = Buffer.from("dm-dev-catalog-delivery-seed-v1\0", "latin1");
export const DEV_SIGNING_KEY_ID = "dm-dev-catalog-delivery-v1";
const ED25519_PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");

function devSigningKey() {
  const der = Buffer.concat([ED25519_PKCS8_PREFIX, DEV_DELIVERY_SEED]);
  return createPrivateKey({ key: der, format: "der", type: "pkcs8" });
}

export function devSigningPublicKeyHex(): string {
  const spki = createPublicKey(devSigningKey()).export({ format: "der", type: "spki" });
  return Buffer.from(spki.subarray(spki.length - 32)).toString("hex");
}

export function signCatalogManifest(manifest: Buffer): Buffer {
  return sign(null, Buffer.concat([CATALOG_SIGNATURE_DOMAIN, manifest]), devSigningKey());
}

/** Reuse the delivery key with a separate domain; the payload cannot be replayed as a catalog. */
export function signDepthCertificationManifest(manifest: Buffer): Buffer {
  return sign(
    null,
    Buffer.concat([DEPTH_CERTIFICATION_SIGNATURE_DOMAIN, manifest]),
    devSigningKey(),
  );
}

export interface ManifestSigner {
  readonly keyId: string;
  sign(manifest: Buffer): Promise<Buffer>;
}

export const devSeedSigner: ManifestSigner = {
  keyId: DEV_SIGNING_KEY_ID,
  sign(manifest: Buffer): Promise<Buffer> {
    return Promise.resolve(signCatalogManifest(manifest));
  },
};

export type SignedCatalogBundle = {
  schemaVersion: "dm-signed-catalog-bundle-1";
  signingKeyId: string;
  signature: string;
  manifest: string;
  deviceProfiles: string[];
  recordingProfiles: string[];
  gameProfiles: string[];
  mappings: string[];
  /** Optional so existing four-category envelopes remain byte-for-byte stable. */
  depthProfiles?: string[];
  /** Optional so existing four-category envelopes remain byte-for-byte stable. */
  cameraBindings?: string[];
};

export type CatalogEntryReference = { id: string; version: string; content_hash: string };

export type CatalogManifest = {
  schema_version: string;
  catalog_id: string;
  catalog_version: string;
  /**
   * legacy `dm-catalog-bundle-2` 必填；中性 `dm-catalog-bundle-3` **必须缺席**
   * （issue #855 PR 1）。可选类型只是为了承载两代，不代表"这个字段可以省略"——
   * `parseManifest` 按代际做的是双向断言，不是宽容。
   */
  non_trainable?: boolean;
  device_profiles: CatalogEntryReference[];
  recording_profiles: CatalogEntryReference[];
  game_profiles: CatalogEntryReference[];
  mappings: CatalogEntryReference[];
  depth_profiles: CatalogEntryReference[];
  camera_bindings: CatalogEntryReference[];
};

export type CatalogManifestCategory =
  | "device_profiles"
  | "recording_profiles"
  | "game_profiles"
  | "mappings"
  | "depth_profiles"
  | "camera_bindings";

const CATEGORIES = [
  "device_profiles",
  "recording_profiles",
  "game_profiles",
  "mappings",
  "depth_profiles",
  "camera_bindings",
] as const satisfies readonly CatalogManifestCategory[];

const CATEGORY_FILES: Record<CatalogManifestCategory, string> = {
  device_profiles: "device-profile.json",
  recording_profiles: "recording-profile.json",
  game_profiles: "game-profile.json",
  mappings: "mapping.json",
  depth_profiles: "depth-profile.json",
  camera_bindings: "camera-binding.json",
};

const DATABASE_KIND: Record<CatalogManifestCategory, string> = {
  device_profiles: "device_profile",
  recording_profiles: "recording_profile",
  game_profiles: "game_profile",
  mappings: "game_mapping",
  depth_profiles: "depth_profile",
  camera_bindings: "camera_binding",
};

function sha256Hex(bytes: Buffer): string {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

export type ParsedCatalogManifest = {
  manifest: CatalogManifest;
  generation: CatalogSchemaGeneration;
};

function parseManifest(bytes: Buffer): ParsedCatalogManifest {
  const parsed = JSON.parse(bytes.toString("utf8")) as Partial<CatalogManifest>;
  if (
    typeof parsed.schema_version !== "string" ||
    typeof parsed.catalog_id !== "string" ||
    typeof parsed.catalog_version !== "string"
  ) {
    throw new Error("catalog manifest identity is incomplete");
  }
  // 双读（issue #855 PR 1 第 3 条）：旧 `dm-catalog-bundle-2` 仍然可读——它是今天的
  // current、回滚路径与三份 superseded 研究快照的格式；中性
  // `dm-catalog-bundle-3` 新增为可读。带了旧字段的中性 manifest 直接拒。
  const generation = catalogSchemaGeneration("manifest", parsed.schema_version);
  readDeclaredNonTrainable(
    "manifest",
    generation,
    parsed as Record<string, unknown>,
    `catalog manifest ${parsed.catalog_id}@${parsed.catalog_version}`,
  );
  for (const category of ["depth_profiles", "camera_bindings"] as const) {
    const entries = parsed[category];
    if (entries !== undefined && !Array.isArray(entries)) {
      throw new Error(`catalog manifest has a non-array ${category}`);
    }
  }
  parsed.depth_profiles ??= [];
  parsed.camera_bindings ??= [];
  for (const category of CATEGORIES) {
    const entries = parsed[category];
    if (!Array.isArray(entries)) {
      throw new Error(`catalog manifest is missing ${category}`);
    }
    for (const entry of entries) {
      if (
        typeof entry !== "object" ||
        entry === null ||
        typeof entry.id !== "string" ||
        typeof entry.version !== "string" ||
        typeof entry.content_hash !== "string"
      ) {
        throw new Error(`catalog manifest has an invalid ${category} reference`);
      }
    }
  }
  return { manifest: parsed as CatalogManifest, generation };
}

export interface CatalogReleaseSource {
  readManifest(): Promise<Buffer>;
  readPayload(
    category: CatalogManifestCategory,
    reference: CatalogEntryReference,
  ): Promise<Buffer>;
}

function assertSha256Reference(reference: CatalogEntryReference): string {
  const match = /^sha256:([0-9a-f]{64})$/.exec(reference.content_hash);
  if (!match?.[1]) {
    throw new Error(
      `${reference.id}@${reference.version} has a non-canonical sha256 content hash`,
    );
  }
  return match[1];
}

async function readRegularFileBeneath(rootDirectory: string, candidate: string): Promise<Buffer> {
  const root = await realpath(rootDirectory);
  if ((await lstat(candidate)).isSymbolicLink()) {
    throw new Error(`catalog source file must not be a symlink: ${candidate}`);
  }
  const resolved = await realpath(candidate);
  const relative = path.relative(root, resolved);
  if (relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`catalog source file escapes its root: ${candidate}`);
  }
  if (!(await stat(resolved)).isFile()) {
    throw new Error(`catalog source path is not a regular file: ${candidate}`);
  }
  return readFile(resolved);
}

/** Strict `catalog.json` + `payloads/sha256/<digest>.json` release source. */
export class FilesystemCatalogReleaseSource implements CatalogReleaseSource {
  constructor(readonly directory: string) {}

  readManifest(): Promise<Buffer> {
    return readRegularFileBeneath(this.directory, path.join(this.directory, "catalog.json"));
  }

  readPayload(
    _category: CatalogManifestCategory,
    reference: CatalogEntryReference,
  ): Promise<Buffer> {
    const digest = assertSha256Reference(reference);
    return readRegularFileBeneath(
      this.directory,
      path.join(this.directory, "payloads", "sha256", `${digest}.json`),
    );
  }
}

/** Transitional compatibility for existing single-game release-test bundles. */
class LegacySingleGameDirectorySource implements CatalogReleaseSource {
  constructor(readonly directory: string) {}

  readManifest(): Promise<Buffer> {
    return readFile(path.join(this.directory, "catalog.json"));
  }

  readPayload(
    category: CatalogManifestCategory,
    _reference: CatalogEntryReference,
  ): Promise<Buffer> {
    return readFile(path.join(this.directory, CATEGORY_FILES[category]));
  }
}

export type CatalogQuery = (
  text: string,
  params?: unknown[],
) => Promise<{ rows: Array<Record<string, unknown>> }>;

/**
 * Exact-byte PostgreSQL source. The query intentionally cannot select
 * `snapshot`; JSONB must never enter the signing path.
 */
export class PostgresCatalogReleaseSource implements CatalogReleaseSource {
  constructor(
    private readonly query: CatalogQuery,
    private readonly manifestBytes: Buffer,
  ) {}

  readManifest(): Promise<Buffer> {
    return Promise.resolve(Buffer.from(this.manifestBytes));
  }

  async readPayload(
    category: CatalogManifestCategory,
    reference: CatalogEntryReference,
  ): Promise<Buffer> {
    const result = await this.query(
      `SELECT payload.exact_bytes, payload.sha256
         FROM trusted_catalog_versions trusted
         JOIN catalog_payloads payload
           ON payload.trusted_catalog_version_id = trusted.id
        WHERE trusted.kind = $1::catalog_kind
          AND trusted.catalog_id = $2
          AND trusted.version = $3
          AND trusted.content_hash = $4
          AND trusted.status <> 'revoked'`,
      [
        DATABASE_KIND[category],
        reference.id,
        reference.version,
        reference.content_hash,
      ],
    );
    if (result.rows.length !== 1) {
      throw new Error(
        `no unique publishable exact payload for ${category} ${reference.id}@${reference.version}`,
      );
    }
    const row = result.rows[0]!;
    if (row.sha256 !== reference.content_hash) {
      throw new Error(`registered payload hash mismatch for ${reference.id}@${reference.version}`);
    }
    if (!(row.exact_bytes instanceof Uint8Array)) {
      throw new Error(`registered exact payload is not bytea for ${reference.id}@${reference.version}`);
    }
    return Buffer.from(row.exact_bytes);
  }
}

export type CatalogPayloads = Record<CatalogManifestCategory, Buffer[]>;

export interface CatalogValidator {
  validate(manifest: Buffer, payloads: CatalogPayloads): Promise<void>;
}

const BUNDLE_FIELD: Record<
  CatalogManifestCategory,
  keyof Pick<
    SignedCatalogBundle,
    | "deviceProfiles"
    | "recordingProfiles"
    | "gameProfiles"
    | "mappings"
    | "depthProfiles"
    | "cameraBindings"
  >
> = {
  device_profiles: "deviceProfiles",
  recording_profiles: "recordingProfiles",
  game_profiles: "gameProfiles",
  mappings: "mappings",
  depth_profiles: "depthProfiles",
  camera_bindings: "cameraBindings",
};

/**
 * Load every exact payload in manifest order, verify its hash, run the
 * client-source validator, then sign the manifest exactly once.
 */
export async function assembleSignedBundle(
  sourceOrLegacyDirectory: CatalogReleaseSource | string,
  validator?: CatalogValidator,
): Promise<SignedCatalogBundle> {
  const source =
    typeof sourceOrLegacyDirectory === "string"
      ? new LegacySingleGameDirectorySource(sourceOrLegacyDirectory)
      : sourceOrLegacyDirectory;
  const manifest = await source.readManifest();
  const { manifest: parsed, generation } = parseManifest(manifest);
  // 签名钥匙不再由渠道选：certified 发布侧随 issue #829 收口（docs/100 §11）之后
  // dev 是唯一渠道，唯一的钥匙就是编译内嵌的那把种子钥匙。
  const signer = devSeedSigner;
  if (generation === "legacy") {
    // legacy manifest 自己声明的 non_trainable 必须与渠道对上——这里做的是对账不是取值。
    // dev 恒 non_trainable，所以一份声称可训练的 legacy bundle 没有任何渠道收得下。
    if (!parsed.non_trainable) {
      throw new Error(
        "a manifest that declares non_trainable=false cannot be published on the dev channel",
      );
    }
  }
  // 中性一代没有这一步，而且**不是把门拆了**：新 schema 根本不声明训练分类
  // （issue #855 §A.4），没有可对账的自称。"这份 bundle 从哪条渠道来" 改由客户端
  // 验签的那把钉死的公钥回答（catalog.rs 的 pinned anchor）——那本来就是更强的
  // 那一份证据：声明可以撞谎，签名不能。
  const payloads = {} as CatalogPayloads;

  for (const category of CATEGORIES) {
    const values: Buffer[] = [];
    for (const reference of parsed[category]) {
      assertSha256Reference(reference);
      const bytes = await source.readPayload(category, reference);
      const actual = sha256Hex(bytes);
      if (actual !== reference.content_hash) {
        throw new Error(
          `${category} ${reference.id}@${reference.version} content hash ${actual} does not match manifest ${reference.content_hash}`,
        );
      }
      values.push(bytes);
    }
    payloads[category] = values;
  }

  await validator?.validate(manifest, payloads);
  const signature = await signer.sign(manifest);
  const bundle: SignedCatalogBundle = {
    schemaVersion: "dm-signed-catalog-bundle-1",
    signingKeyId: signer.keyId,
    signature: signature.toString("base64"),
    manifest: manifest.toString("base64"),
    deviceProfiles: [],
    recordingProfiles: [],
    gameProfiles: [],
    mappings: [],
  };
  for (const category of CATEGORIES) {
    const encoded = payloads[category].map((bytes) => bytes.toString("base64"));
    if (category === "depth_profiles" || category === "camera_bindings") {
      if (encoded.length > 0) bundle[BUNDLE_FIELD[category]] = encoded;
    } else {
      bundle[BUNDLE_FIELD[category]] = encoded;
    }
  }
  return bundle;
}

export function serializeSignedBundle(bundle: SignedCatalogBundle): Buffer {
  return Buffer.from(JSON.stringify(bundle), "utf8");
}

export function signedBundleEtag(envelope: Buffer): string {
  return `"sha256-${createHash("sha256").update(envelope).digest("hex")}"`;
}
