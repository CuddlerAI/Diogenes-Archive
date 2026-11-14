/**
 * Catalog registration (docs/49 E3').
 *
 * The ingestion API only READS `games` / `trusted_catalog_versions` — they are
 * the provisioning boundary operators seed out of band. Until now the only
 * seeder was the e2e script against the isolated database, so a real client's
 * first `POST /v1/sessions` against production answered
 * `catalog_version_not_found`. This module is the real registration path: it
 * reads a self-contained catalog bundle directory (the same six files the
 * client compiles in or loads from disk), recomputes every content hash from
 * the exact bytes, cross-checks them against the bundle manifest, and registers
 * the game plus all five catalog kinds. `game_profile` is governed as its own
 * trusted row, while the existing session identity remains game_id/version/exe.
 *
 * Fail-closed defaults: rows land as `draft` with `approved_for_trainable =
 * false`, and the game lands with both approval booleans false. Every
 * escalation — approved status, trainable approval, capture allowance — is an
 * explicit flag with a named approver. The tool escalates only; it never
 * demotes, because demotion is a governance act (revocation) with its own
 * audit path, not a side effect of re-running a seeding script.
 *
 * Idempotent: a re-run with identical bytes is a no-op; a changed profile under
 * an unchanged (kind, catalog_id, version) is an identity conflict and is
 * refused — bump the version instead. This mirrors the immutability trigger on
 * `trusted_catalog_versions` rather than fighting it.
 */
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";

import {
  assertConsistentGeneration,
  catalogSchemaGeneration,
  readDeclaredNonTrainable,
  type CatalogSchemaGeneration,
  type VersionedCatalogPayloadKind,
} from "./catalog-schema.js";

type JsonRecord = Record<string, unknown>;

/** 携带过 `non_trainable` 的 bundle 文件 → 代际表里的 kind。mapping/depth/camera 从未携带。 */
const VERSIONED_KIND: Partial<Record<CatalogEntryKind, VersionedCatalogPayloadKind>> = {
  device_profile: "device_profile",
  recording_profile: "recording_profile",
  game_profile: "game_profile",
};

/** Minimal query surface satisfied by both pg.Client/Pool and PGlite. */
export type QueryFn = (
  text: string,
  params?: unknown[],
) => Promise<{ rows: Array<Record<string, unknown>> }>;

const PROFILE_FILES = {
  device_profile: { file: "device-profile.json", manifestKey: "device_profiles", optional: false },
  recording_profile: {
    file: "recording-profile.json",
    manifestKey: "recording_profiles",
    optional: false,
  },
  game_profile: { file: "game-profile.json", manifestKey: "game_profiles", optional: false },
  game_mapping: { file: "mapping.json", manifestKey: "mappings", optional: false },
  depth_profile: { file: "depth-profile.json", manifestKey: "depth_profiles", optional: true },
  camera_binding: { file: "camera-binding.json", manifestKey: "camera_bindings", optional: true },
} as const;

export type CatalogEntryKind = keyof typeof PROFILE_FILES;

export type BundleEntry = {
  kind: CatalogEntryKind;
  catalogId: string;
  version: string;
  /** NULL for device_profile; the bundle's game id otherwise. */
  gameId: string | null;
  contentHash: string;
  snapshot: JsonRecord;
  /** Registration-time bytes; never reconstructed from snapshot/jsonb. */
  exactBytes: Buffer;
};

export type CatalogBundle = {
  directory: string;
  catalogId: string;
  catalogVersion: string;
  /** 这份 bundle 属于哪一代 catalog schema（`catalog-schema.ts`）。 */
  schemaGeneration: CatalogSchemaGeneration;
  /**
   * legacy 一代 manifest 级的渠道派生分类（docs/57 §3.4 三个正交布尔中的第一个）。
   *
   * 中性一代为 `null`：新 schema **不再从 bundle 推导训练分类**（issue #855 §A.4）。
   * `null` 的意思是"这份 bundle 不声明"，不是"声明为可训练"——两者的区别正是
   * 下面 `assertTrainableApprovalNotSelfContradictory` 要保住的。
   */
  declaredNonTrainable: boolean | null;
  game: { id: string; title: string; contentHash: string; snapshot: JsonRecord };
  entries: BundleEntry[];
};

function sha256Prefixed(bytes: Buffer): string {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

function manifestEntry(manifest: JsonRecord, key: string): JsonRecord {
  const list = manifest[key];
  if (!Array.isArray(list) || list.length !== 1 || typeof list[0] !== "object" || list[0] === null) {
    throw new Error(`bundle manifest must contain exactly one entry under ${key}`);
  }
  return list[0] as JsonRecord;
}

function requiredString(value: unknown, description: string): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new Error(`${description} must be a non-empty string`);
  }
  return value;
}

/**
 * Reads a self-contained bundle and verifies its internal consistency: every
 * profile's exact-byte hash must equal what the manifest pins. A bundle whose
 * files drifted from its manifest would register hashes no client ever
 * declares, which fails at first upload in the most confusing way possible —
 * so it is refused here instead.
 */
export async function readCatalogBundle(directory: string): Promise<CatalogBundle> {
  const manifestBytes = await readFile(path.join(directory, "catalog.json"));
  const manifest = JSON.parse(manifestBytes.toString("utf8")) as JsonRecord;
  // 双读（issue #855 PR 1 第 3 条）：旧 `dm-catalog-bundle-2` 与中性
  // `dm-catalog-bundle-3` 都收。manifest 不再自称 catalog_mode（docs/57 §3.3）；
  // legacy 一代仍带渠道派生的 `non_trainable`，registry 如实收下，中性一代要求它缺席。
  const schemaGeneration = catalogSchemaGeneration("manifest", manifest.schema_version);
  const declaredNonTrainable = readDeclaredNonTrainable(
    "manifest",
    schemaGeneration,
    manifest,
    `bundle ${directory}`,
  );

  const gameBytes = await readFile(path.join(directory, "game-profile.json"));
  const gameSnapshot = JSON.parse(gameBytes.toString("utf8")) as JsonRecord;
  const gameManifest = manifestEntry(manifest, "game_profiles");
  const gameHash = sha256Prefixed(gameBytes);
  if (gameManifest.content_hash !== gameHash) {
    throw new Error("game-profile.json bytes do not match the manifest content_hash");
  }
  const gameId = requiredString(gameSnapshot.game_id, "game-profile game_id");
  if (gameManifest.id !== gameId) {
    throw new Error("game-profile game_id does not match the manifest entry id");
  }

  const entries: BundleEntry[] = [];
  const payloadGenerations: Array<{
    kind: VersionedCatalogPayloadKind;
    generation: CatalogSchemaGeneration;
  }> = [];
  for (const [kind, { file, manifestKey, optional }] of Object.entries(PROFILE_FILES)) {
    const authored = manifest[manifestKey];
    if (optional && authored === undefined) continue;
    if (optional && Array.isArray(authored) && authored.length === 0) continue;
    const bytes = kind === "game_profile" ? gameBytes : await readFile(path.join(directory, file));
    const snapshot = kind === "game_profile" ? gameSnapshot : (JSON.parse(bytes.toString("utf8")) as JsonRecord);
    const pinned = manifestEntry(manifest, manifestKey);
    const contentHash = sha256Prefixed(bytes);
    if (pinned.content_hash !== contentHash) {
      throw new Error(`${file} bytes do not match the manifest content_hash`);
    }
    // 逐本 profile 判代并施加同一条训练词表闸。旧实现只看 manifest，于是一份
    // manifest 已断代、profile 还带旧字段的 bundle 能在服务端注册通过，却在 Rust
    // 客户端解析时才炸——那时字节已经入库、发布名单已经排好。
    const versionedKind = VERSIONED_KIND[kind as CatalogEntryKind];
    if (versionedKind) {
      const generation = catalogSchemaGeneration(versionedKind, snapshot.schema_version);
      readDeclaredNonTrainable(versionedKind, generation, snapshot, `${directory}/${file}`);
      payloadGenerations.push({ kind: versionedKind, generation });
    }
    entries.push({
      kind: kind as CatalogEntryKind,
      catalogId: requiredString(pinned.id, `${manifestKey} entry id`),
      version: requiredString(pinned.version, `${manifestKey} entry version`),
      gameId: kind === "device_profile" ? null : gameId,
      contentHash,
      snapshot,
      exactBytes: bytes,
    });
  }

  assertConsistentGeneration(schemaGeneration, payloadGenerations, `bundle ${directory}`);

  return {
    directory,
    catalogId: requiredString(manifest.catalog_id, "catalog_id"),
    catalogVersion: requiredString(manifest.catalog_version, "catalog_version"),
    schemaGeneration,
    declaredNonTrainable,
    game: {
      id: gameId,
      title:
        typeof gameSnapshot.display_name === "string" && gameSnapshot.display_name.trim() !== ""
          ? gameSnapshot.display_name
          : gameId,
      contentHash: gameHash,
      snapshot: gameSnapshot,
    },
    entries,
  };
}

export type RegistrationOptions = {
  /** Set the five rows to `approved`; requires `approver`. */
  approve: boolean;
  /** Recorded as approved_by. */
  approver?: string;
  /** Also set approved_for_trainable on the five rows; requires approve. */
  approveTrainable: boolean;
  /** games.allowed_for_capture := true. */
  allowCapture: boolean;
  /** games.approved_for_trainable := true. */
  gameTrainable: boolean;
  /** games taxonomy columns; sensible defaults for a first-person PC game. */
  contentKind?: string;
  viewKind?: string;
  materialKind?: string;
  /** Report the plan without writing. */
  dryRun?: boolean;
};

export type EntryOutcome = {
  kind: CatalogEntryKind;
  catalogId: string;
  version: string;
  action: "inserted" | "unchanged" | "escalated" | "would_insert";
  id: string | null;
  status: string;
  approvedForTrainable: boolean;
};

export type ExecutableOutcome = {
  /** `absent` = no authored pin; the allowlist entry will come from a real capture instead. */
  action: "absent" | "inserted" | "escalated" | "unchanged" | "would_insert" | "would_escalate";
  sha256: string | null;
  status: "pending" | "approved" | "revoked" | null;
};

export type RegistrationReport = {
  game: { id: string; action: "inserted" | "updated" | "unchanged" | "would_insert" };
  entries: EntryOutcome[];
  /** Approved-build allowlist outcome for the authored executable pin (migration 0018). */
  executable: ExecutableOutcome;
};

/**
 * 「这本目录/这个游戏自称禁止训练」的文字口径。与迁移 0028 的
 * `games_non_trainable_title_guard` CHECK 是同一条正则——两层必须同口径，否则
 * 数据库放行的写法在工具这层拦不住，或者反过来。
 */
export const NON_TRAINABLE_TITLE_PATTERN = /禁止训练|non[-_ ]?trainable/i;

export function titleDeclaresNonTrainable(title: string): boolean {
  return NON_TRAINABLE_TITLE_PATTERN.test(title);
}

/**
 * 可训练审批与「自称禁止训练」的一致性门。
 *
 * 2026-07-26 生产库出过一次：`config/catalogs/release/cod-steam/` 那本 first-party
 * release bundle 是照 dev 的 cod-steam-release-test 改的，`non_trainable` 翻成了
 * false，却把 `game_id` 和 `display_name` 原样留着——于是
 * `--approve-trainable --game-trainable` 把可训练审批写到了一行标题写着
 * 「禁止训练」的 games 记录上。`games` 一个 id 只有一行，而本工具只升不降，
 * 这行 true 从此再没有任何注册路径能清掉（详见迁移 0028）。
 *
 * 两条独立的口子，各挡一种走法：
 * - bundle 自己 `non_trainable: true` 却要可训练审批 —— 拿 dev 目录当可训练注册；
 * - bundle 说自己可训练，但它借用的游戏身份自称禁止训练 —— 正是上面那次事故。
 *
 * 中性一代（`declaredNonTrainable === null`）没有第一条：新 schema 不再声明训练分类
 * （issue #855 §A.4），"这份 bundle 自称禁止训练"这个前提根本不存在了。第二条**照旧**
 * 生效，并且它才是那次事故真正的挡板——迁移 0028 的 `games_non_trainable_title_guard`
 * 在数据库层钉着同一条正则，两层同口径。
 */
function assertTrainableApprovalNotSelfContradictory(
  bundle: CatalogBundle,
  options: RegistrationOptions,
): void {
  if (bundle.declaredNonTrainable === true && (options.approveTrainable || options.gameTrainable)) {
    throw new Error(
      `bundle ${bundle.catalogId} declares non_trainable: true; ` +
        "--approve-trainable / --game-trainable are refused for a self-declared non-trainable catalog",
    );
  }
  if (options.gameTrainable && titleDeclaresNonTrainable(bundle.game.title)) {
    throw new Error(
      `game ${bundle.game.id} is titled "${bundle.game.title}", which declares it non-trainable; ` +
        "--game-trainable is refused. A trainable catalog must not reuse a non-trainable game identity — " +
        "give it its own game_id and display_name (migration 0028)",
    );
  }
}

/**
 * 可训练审批的视角标定完备性门（docs/46 §5.1）。
 * - `view_scale` 块必须存在：指针驱动型游戏也要显式 `applicable:false`，
 *   缺失与"不适用"不可区分即拒绝；
 * - `applicable:true` 时目录常数与阈值必须已定版（正有限数）——常数来自
 *   标定实验（H-1），不允许用未定版目录做可训练审批。
 */
function assertViewScaleApprovable(bundle: CatalogBundle): void {
  const recording = bundle.entries.find((entry) => entry.kind === "recording_profile");
  const viewScale = recording?.snapshot.view_scale;
  if (viewScale === undefined || viewScale === null || typeof viewScale !== "object") {
    throw new Error(
      "--approve-trainable requires a view_scale block in recording-profile.json; " +
        "declare applicable:false explicitly for pointer-driven games (docs/46 §5.1)",
    );
  }
  const block = viewScale as JsonRecord;
  if (block.applicable === false) {
    return;
  }
  if (block.applicable !== true) {
    throw new Error("view_scale.applicable must be an explicit boolean (docs/46 §2.1)");
  }
  // `vertical_scale_ratio` 是 2026-07-30 补进来的（docs/60 Q3）。它此前不在清单
  // 里，而 converter 的 `view_scale_constants` 要求它与 `counts_per_360` **同时**
  // 存在才产出角度列——缺它则 look_yaw/look_pitch 两列全空、view_scale_valid
  // 为 false。也就是说「审批通过、产出的数据集角度列却是空的」这个状态从前是
  // 可达的。审批门的必填集与下游消费者的必需集必须是同一份。
  for (const field of [
    "counts_per_360",
    "vertical_scale_ratio",
    "tolerance_pct",
    "fast_slow_tolerance_pct",
    "min_polling_hz",
  ]) {
    const value = block[field];
    if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
      throw new Error(
        `--approve-trainable requires a finalized positive view_scale.${field}; ` +
          "pin the calibration constants from the H-1 experiment before trainable approval (docs/46 §5.1)",
      );
    }
  }
}

export async function registerCatalogBundle(
  query: QueryFn,
  bundle: CatalogBundle,
  options: RegistrationOptions,
): Promise<RegistrationReport> {
  if (options.approveTrainable && !options.approve) {
    throw new Error("--approve-trainable requires --approve (the table CHECK enforces the same)");
  }
  if (options.approve && (!options.approver || options.approver.trim() === "")) {
    throw new Error("--approve requires --approver <name> so the approval is attributable");
  }
  assertTrainableApprovalNotSelfContradictory(bundle, options);
  if (options.approveTrainable) {
    // 视角标定审批门（C5，docs/46 §5.1）：可训练审批要求 settings snapshot
    // 的 view_scale 块完整——缺失标定是回溯不可修复的准入缺口（docs/45 §3），
    // 在审批时刻拦住，而不是等数据采完。
    assertViewScaleApprovable(bundle);
  }

  const contentKind = options.contentKind ?? "game";
  const viewKind = options.viewKind ?? "first_person";
  const materialKind = options.materialKind ?? "rendered";

  // Game row: insert fail-closed, escalate-only on re-run. Demotion is a
  // governance act, not a seeding side effect.
  const existingGame = await query(`SELECT id, allowed_for_capture, approved_for_trainable FROM games WHERE id = $1`, [
    bundle.game.id,
  ]);
  let gameAction: RegistrationReport["game"]["action"];
  if (existingGame.rows.length === 0) {
    gameAction = options.dryRun ? "would_insert" : "inserted";
    if (!options.dryRun) {
      await query(
        `INSERT INTO games(id, title, content_kind, view_kind, material_kind,
                           allowed_for_capture, approved_for_trainable)
         VALUES ($1, $2, $3, $4, $5, $6, $7)`,
        [
          bundle.game.id,
          bundle.game.title,
          contentKind,
          viewKind,
          materialKind,
          options.allowCapture,
          options.gameTrainable,
        ],
      );
    }
  } else {
    const row = existingGame.rows[0]!;
    const escalate =
      (options.allowCapture && row.allowed_for_capture !== true) ||
      (options.gameTrainable && row.approved_for_trainable !== true);
    gameAction = escalate ? "updated" : "unchanged";
    if (escalate && !options.dryRun) {
      await query(
        `UPDATE games
            SET allowed_for_capture = allowed_for_capture OR $2,
                approved_for_trainable = approved_for_trainable OR $3
          WHERE id = $1`,
        [bundle.game.id, options.allowCapture, options.gameTrainable],
      );
    }
  }

  const outcomes: EntryOutcome[] = [];
  for (const entry of bundle.entries) {
    const existing = await query(
      `SELECT id, game_id, content_hash, status, approved_for_trainable
         FROM trusted_catalog_versions
        WHERE kind = $1::catalog_kind AND catalog_id = $2 AND version = $3`,
      [entry.kind, entry.catalogId, entry.version],
    );
    if (existing.rows.length === 0) {
      const status = options.approve ? "approved" : "draft";
      if (options.dryRun) {
        outcomes.push({
          kind: entry.kind,
          catalogId: entry.catalogId,
          version: entry.version,
          action: "would_insert",
          id: null,
          status,
          approvedForTrainable: options.approveTrainable,
        });
        continue;
      }
      const inserted = await query(
        `WITH trusted AS (
           INSERT INTO trusted_catalog_versions(
             id, kind, catalog_id, version, game_id, content_hash, snapshot,
             status, approved_for_trainable, approved_at, approved_by
           ) VALUES (
             gen_random_uuid(), $1::catalog_kind, $2, $3, $4, $5, $6::jsonb,
             CASE WHEN $7::boolean THEN 'approved' ELSE 'draft' END::catalog_status,
             $8::boolean,
             CASE WHEN $7::boolean THEN transaction_timestamp() END,
             CASE WHEN $7::boolean THEN $9::text END
           )
           RETURNING id
         ), payload AS (
           INSERT INTO catalog_payloads(
             trusted_catalog_version_id, exact_bytes, byte_size, sha256
           )
           SELECT id, $10::bytea, $11::bigint, $5
             FROM trusted
         )
         SELECT id FROM trusted`,
        [
          entry.kind,
          entry.catalogId,
          entry.version,
          entry.gameId,
          entry.contentHash,
          JSON.stringify(entry.snapshot),
          options.approve,
          options.approveTrainable,
          options.approver ?? null,
          entry.exactBytes,
          entry.exactBytes.byteLength,
        ],
      );
      outcomes.push({
        kind: entry.kind,
        catalogId: entry.catalogId,
        version: entry.version,
        action: "inserted",
        id: String(inserted.rows[0]!.id),
        status,
        approvedForTrainable: options.approveTrainable,
      });
      continue;
    }

    const row = existing.rows[0]!;
    if (row.content_hash !== entry.contentHash || (row.game_id ?? null) !== entry.gameId) {
      throw new Error(
        `${entry.kind} ${entry.catalogId}@${entry.version} already exists with different bytes or game scope — ` +
          `catalog identity is immutable; bump the version instead`,
      );
    }
    const payload = await query(
      `SELECT exact_bytes, byte_size, sha256
         FROM catalog_payloads
        WHERE trusted_catalog_version_id = $1`,
      [row.id],
    );
    if (payload.rows.length === 0) {
      throw new Error(
        `${entry.kind} ${entry.catalogId}@${entry.version} is a legacy JSONB-only row and is not publishable — ` +
          `register the exact bytes under a new catalog version`,
      );
    }
    const registeredBytes = Buffer.from(payload.rows[0]!.exact_bytes as Uint8Array);
    if (
      payload.rows[0]!.sha256 !== entry.contentHash ||
      Number(payload.rows[0]!.byte_size) !== entry.exactBytes.byteLength ||
      !registeredBytes.equals(entry.exactBytes)
    ) {
      throw new Error(
        `${entry.kind} ${entry.catalogId}@${entry.version} exact payload is immutable; bump the version instead`,
      );
    }
    const wantsApprove = options.approve && row.status === "draft";
    const wantsTrainable = options.approveTrainable && row.approved_for_trainable !== true;
    if (!wantsApprove && !wantsTrainable) {
      outcomes.push({
        kind: entry.kind,
        catalogId: entry.catalogId,
        version: entry.version,
        action: "unchanged",
        id: String(row.id),
        status: String(row.status),
        approvedForTrainable: row.approved_for_trainable === true,
      });
      continue;
    }
    if (!options.dryRun) {
      await query(
        `UPDATE trusted_catalog_versions
            SET status = CASE WHEN status = 'draft' AND $2::boolean THEN 'approved'::catalog_status ELSE status END,
                approved_for_trainable = approved_for_trainable OR $3::boolean,
                approved_at = COALESCE(approved_at, CASE WHEN $2::boolean OR $3::boolean THEN transaction_timestamp() END),
                approved_by = COALESCE(approved_by, CASE WHEN $2::boolean OR $3::boolean THEN $4::text END)
          WHERE id = $1`,
        [row.id, options.approve, options.approveTrainable, options.approver ?? null],
      );
    }
    outcomes.push({
      kind: entry.kind,
      catalogId: entry.catalogId,
      version: entry.version,
      action: "escalated",
      id: String(row.id),
      status: wantsApprove ? "approved" : String(row.status),
      approvedForTrainable: row.approved_for_trainable === true || options.approveTrainable,
    });
  }

  const executable = await registerAuthoredExecutable(query, bundle, options);

  return { game: { id: bundle.game.id, action: gameAction }, entries: outcomes, executable };
}

/**
 * authored 指纹入册（migration 0018）。
 *
 * #278 之后 `executable_sha256` 是选填的：钉了就是治理证据，没钉就靠采集时
 * 实测的 observed 指纹走 pending 审批。这里只处理"钉了"的那一支——它随
 * `--approve` 一起入册为 approved，因为审批这份 catalog 的人正是在审批这个
 * build。没钉指纹时不写任何行：名单留给触发器记录的 observed 指纹。
 *
 * 升级永不降级：已 revoked 的行不会被重新注册悄悄复活（撤销是治理行为，
 * 有自己的审计路径），与 trusted_catalog_versions 的纪律一致。
 */
async function registerAuthoredExecutable(
  query: QueryFn,
  bundle: CatalogBundle,
  options: RegistrationOptions,
): Promise<ExecutableOutcome> {
  const pinned = bundle.game.snapshot.executable_sha256;
  if (typeof pinned !== "string" || !/^sha256:[0-9a-f]{64}$/.test(pinned)) {
    return { action: "absent", sha256: null, status: null };
  }
  const existing = await query(
    `SELECT status FROM approved_game_executables WHERE game_id = $1 AND executable_sha256 = $2`,
    [bundle.game.id, pinned],
  );
  const current = existing.rows[0] ? String(existing.rows[0]!.status) : null;
  if (current === "approved" || current === "revoked") {
    return { action: "unchanged", sha256: pinned, status: current };
  }
  // 未 --approve 时只登记为 pending：注册不等于审批。
  const status = options.approve ? "approved" : "pending";
  if (options.dryRun) {
    return { action: current === null ? "would_insert" : "would_escalate", sha256: pinned, status };
  }
  // approved_by/approved_at 只在真正批准时写：一条 pending 行不应带着审批人
  // 却没有审批时刻（表上的 CHECK 只约束 approved 行，卫生要靠这里）。
  const approver = status === "approved" ? (options.approver ?? null) : null;
  await query(
    `INSERT INTO approved_game_executables(
       game_id, executable_sha256, status, source, approved_by, approved_at, note
     ) VALUES ($1, $2, $3, 'authored', $4,
               CASE WHEN $4::text IS NULL THEN NULL ELSE transaction_timestamp() END, $5)
     ON CONFLICT (game_id, executable_sha256) DO UPDATE
       SET status = CASE WHEN approved_game_executables.status = 'pending' AND $3 = 'approved'
                         THEN 'approved' ELSE approved_game_executables.status END,
           -- 只标记"这个 build 现在也由 catalog 钉住了"，不改既有 pending/revoked 状态。
           source = 'authored',
           approved_by = COALESCE(approved_game_executables.approved_by, $4),
           approved_at = COALESCE(approved_game_executables.approved_at,
                                  CASE WHEN $4::text IS NULL THEN NULL ELSE transaction_timestamp() END)`,
    [
      bundle.game.id,
      pinned,
      status,
      approver,
      `authored in game profile ${bundle.game.contentHash}`,
    ],
  );
  return { action: current === null ? "inserted" : "escalated", sha256: pinned, status };
}
