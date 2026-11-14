import { z } from "zod";

import {
  environmentProfile,
  environments,
  primaryFamily,
  type AdmissionFamily,
  type EnvironmentProfile,
} from "./environment.js";

const FIVE_MIB = 5 * 1024 * 1024;
const FIVE_GIB = 5 * 1024 * 1024 * 1024;

const environmentSchema = z.object({
  DATAMINER_ENV: z.enum(environments),
  // DATAMINER_STAGING_TOKEN is gone (11 §10). `/v1/*` callers now authenticate
  // with a per-device credential issued by the console, so there is no shared
  // ingest secret left to configure.
  // Not z.url(): a Cloud SQL unix-socket URL
  // (postgres://user:pass@/db?host=/cloudsql/INSTANCE) has an empty host, which
  // the WHATWG URL parser rejects — that would refuse the DATABASE_URL the
  // production runbook itself documents. Accept any postgres connection string.
  DATABASE_URL: z
    .string()
    .min(1)
    .refine((value) => /^postgres(ql)?:\/\//.test(value), {
      message: "must be a postgres:// or postgresql:// connection string",
    }),
  // The edge gate, not a caller credential. Cloud Run has to admit `allUsers`
  // for the Cloudflare Worker in front of api.diogenesmodel.com to reach it at
  // all — a desktop client holds a per-device bearer token, not a Google
  // identity, so `--no-allow-unauthenticated` would lock out the very fleet
  // this API exists to serve. That leaves the bare `*.run.app` URL as a way
  // around the edge, so `/v1/*` additionally requires the shared header the
  // Worker injects. Same shape as the console's CONSOLE_PROXY_TOKEN
  // (docs/11 §4.3); required for every public ingest deployment, optional only
  // in staging.
  DATAMINER_PROXY_TOKEN: z
    .string()
    .min(32, "DATAMINER_PROXY_TOKEN must be at least 32 bytes")
    .refine((value) => !value.startsWith("replace-me"), "DATAMINER_PROXY_TOKEN is a placeholder")
    .optional(),
  R2_ACCOUNT_ID: z.string().min(1),
  R2_BUCKET: z.string().min(1),
  R2_ACCESS_KEY_ID: z.string().min(1),
  R2_SECRET_ACCESS_KEY: z.string().min(1),
  // Validated against the environment profile below: the pattern and whether a
  // default exists differ between staging and production.
  R2_PREFIX: z.string().min(1).optional(),
  // Admits the consented first-party lane (docs/49 E7). Unset means the
  // deployment does not admit dm-first-party-release sessions at all — the
  // trainable-capable lane never appears by default. Only meaningful on a
  // profile that defines the family (production); setting it elsewhere is a
  // startup error so a copy-pasted env file cannot silently imply the lane.
  R2_PREFIX_FIRST_PARTY: z.string().min(1).optional(),
  // 中性摄取车道的前缀（issue #855 §B，`raw/v2/<deployment>`）。不设=这个部署完全
  // 不准入 `dm-session-ingest-2`，于是没有任何 session 会被写成 candidate。
  //
  // 与 `R2_PREFIX` 一样没有默认值，理由也一样：继承一个默认前缀会让配错的部署把
  // 字节写进另一个部署的键空间，而后面没有任何检查会发现。它同时是本 issue 的
  // 准入开关——PR 3 交付能力与路由，"生产从哪一刻起开始产出 training_candidate"
  // 是一次显式的部署动作，不是合并这条 PR 的副作用。
  R2_PREFIX_INGEST_V2: z.string().min(1).optional(),
  HOST: z.string().default("127.0.0.1"),
  PORT: z.coerce.number().int().min(1).max(65_535).default(8080),
  R2_PRESIGN_TTL_SECONDS: z.coerce.number().int().min(60).max(900).default(900),
  // 8 MiB per docs/30 §5.4: R2's non-final-part floor is 5 MiB, and at 1 Mbit/s
  // an 8 MiB part completes in about 67 seconds — a disconnect loses one small
  // part, not a 64 MiB one. A 1 GiB video is ~128 parts, far below the 10,000
  // part ceiling. The client must trust the plan the intent returns, so the
  // server default is the fleet-wide knob.
  R2_PART_SIZE_BYTES: z.coerce
    .number()
    .int()
    .min(FIVE_MIB)
    .max(FIVE_GIB)
    .default(8 * 1024 * 1024),
  R2_REQUEST_TIMEOUT_SECONDS: z.coerce.number().int().min(30).max(300).default(120),
  VERIFY_BATCH_SIZE: z.coerce.number().int().min(1).max(32).default(4),
  VERIFY_LEASE_SECONDS: z.coerce.number().int().min(60).max(3600).default(900),
  PURGE_BATCH_SIZE: z.coerce.number().int().min(1).max(32).default(4),
  PURGE_LEASE_SECONDS: z.coerce.number().int().min(60).max(3600).default(900),
  MULTIPART_COMPLETION_LEASE_SECONDS: z.coerce.number().int().min(60).max(3600).default(300),
  ORPHAN_MULTIPART_GRACE_SECONDS: z.coerce.number().int().min(300).max(86_400).default(1_200),
  // 365 is the consent retention ceiling from docs/00. docs/06 provisions a
  // 400 day R2 lifecycle rule, which is a backstop that must outlive the
  // application deadline, not a licence to keep data for 400 days.
  RETENTION_DAYS: z.coerce.number().int().min(1).max(365).optional(),
  // Accepted only in staging, where it was the original name.
  STAGING_RETENTION_DAYS: z.coerce.number().int().min(1).max(365).optional(),
  // Client behaviour logs export (docs/23 §6). Off by default: a worker without
  // the exporter configured must not silently mark outbox rows published.
  LOGS_EXPORT_ENABLED: z.enum(["true", "false"]).default("false"),
  LOGS_GCS_BUCKET: z.string().min(1).optional(),
  LOGS_EXPORT_BATCH_SIZE: z.coerce.number().int().min(1).max(1_000).default(200),
  LOGS_EXPORT_DRY_RUN: z.enum(["true", "false"]).default("false"),
  // Telemetry recorded before any consent existed — signup and enrolment —
  // cannot be reached by a consent purge, because it carries no consent_id
  // (11 §6.4 records this gap and leaves the value to be set here). Without a
  // deadline of its own it would simply be kept forever, which is the one
  // outcome the separate client_events table exists to avoid. Shorter than the
  // clip retention by default: these rows are diagnostics about getting an
  // account set up, not contributed data.
  UNCONSENTED_TELEMETRY_TTL_DAYS: z.coerce.number().int().min(1).max(365).default(30),
});

/** A family the deployment actually admits, with its configured prefix. */
export type AdmittedFamily = {
  family: AdmissionFamily;
  prefix: string;
};

export type AppConfig = {
  environment: (typeof environments)[number];
  profile: EnvironmentProfile;
  /**
   * The families this deployment serves: the primary family always, plus any
   * additional family whose prefix was explicitly configured. Everything that
   * routes by sourceFamily or catalog_mode reads from this list.
   */
  admittedFamilies: AdmittedFamily[];
  databaseUrl: string;
  /** Shared secret the edge Worker presents; null disables the gate. */
  proxyToken: string | null;
  host: string;
  port: number;
  r2: {
    accountId: string;
    bucket: string;
    accessKeyId: string;
    secretAccessKey: string;
    /** The primary family's prefix. Family-aware call sites use admittedFamilies. */
    prefix: string;
    presignTtlSeconds: number;
    partSizeBytes: number;
    requestTimeoutSeconds: number;
  };
  worker: {
    batchSize: number;
    leaseSeconds: number;
    purgeBatchSize: number;
    purgeLeaseSeconds: number;
    multipartCompletionLeaseSeconds: number;
    orphanMultipartGraceSeconds: number;
  };
  retentionDays: number;
  logsExport: {
    enabled: boolean;
    /** GCS bucket for the client-events landing zone; null means unconfigured. */
    bucket: string | null;
    batchSize: number;
    dryRun: boolean;
  };
  /** Deadline for telemetry that predates any consent (11 §6.4). */
  unconsentedTelemetryTtlDays: number;
};

/**
 * Every `server_data_classification` value this deployment's admitted lanes can
 * produce. Feeds the repository's upload/verification gates: bytes may move for
 * any classification an admitted lane assigns, and for nothing else.
 */
export function admittedClassifications(config: AppConfig): string[] {
  return [
    ...new Set<string>([
      "non_trainable",
      ...config.admittedFamilies.map((entry) => entry.family.bestClassification),
    ]),
  ];
}

function validatedPrefix(name: string, raw: string, family: AdmissionFamily): string {
  if (!family.prefixPattern.test(raw)) {
    throw new Error(`${name} must be an exclusive deployment prefix below ${family.prefixHint}`);
  }
  if (raw.replace(/\/+$/, "").includes("//")) {
    throw new Error(`${name} must not contain empty path segments`);
  }
  return raw.replace(/\/+$/, "");
}

export function loadConfig(environment: NodeJS.ProcessEnv = process.env): AppConfig {
  const parsed = environmentSchema.parse(environment);
  const profile = environmentProfile(parsed.DATAMINER_ENV);
  const primary = primaryFamily(profile);

  // Production names its own prefix. Inheriting a default would let a
  // misconfigured deployment write production bytes under another deployment's
  // key space, which no later check would notice.
  const rawPrefix = parsed.R2_PREFIX ?? primary.defaultPrefix;
  if (!rawPrefix) {
    throw new Error(`R2_PREFIX is required in ${profile.environment}: expected ${primary.prefixHint}`);
  }
  const prefix = validatedPrefix("R2_PREFIX", rawPrefix, primary);

  // consented first-party 车道随 docs/57 §3.3 退役，`dm-first-party-release`
  // 准入家族已经从所有 profile 上摘掉。这里保留对 R2_PREFIX_FIRST_PARTY 的
  // **拒绝**而不是静默忽略：某个部署的环境变量里可能还留着这一项，忽略它等于
  // 让运维以为那条车道还开着，而实际上写进去的数据没有任何家族会收。
  if (parsed.R2_PREFIX_FIRST_PARTY) {
    throw new Error(
      "R2_PREFIX_FIRST_PARTY is no longer supported: the consented first-party lane retired with docs/57 §3.3. Remove the variable from this deployment.",
    );
  }
  const admittedFamilies: AdmittedFamily[] = [{ family: primary, prefix }];

  // 中性摄取车道（issue #855 §B）：只有部署显式命名它自己的 `raw/v2/<deployment>`
  // 前缀时才准入。profile 里没有这条家族却设了变量，是**启动错误**而不是忽略——
  // 忽略等于让运维以为车道开着，而实际上写进去的数据没有任何家族会收（同
  // `R2_PREFIX_FIRST_PARTY` 的先例）。
  if (parsed.R2_PREFIX_INGEST_V2 !== undefined) {
    const ingestV2 = profile.families.find(
      (family) => family.sessionSchemaVersion === "dm-session-ingest-2",
    );
    if (!ingestV2) {
      throw new Error(
        `R2_PREFIX_INGEST_V2 is set but ${profile.environment} defines no neutral ingest family; the lane exists only where environment.ts declares it`,
      );
    }
    admittedFamilies.push({
      family: ingestV2,
      prefix: validatedPrefix("R2_PREFIX_INGEST_V2", parsed.R2_PREFIX_INGEST_V2, ingestV2),
    });
  }

  // Production is reached over the public internet through a Cloudflare
  // Worker, and its Cloud Run origin is open to `allUsers` by necessity.
  // Without this gate `/v1/*` is reachable straight off `*.run.app`, so refuse
  // to start rather than serve bypassable.
  if (profile.environment !== "staging" && !parsed.DATAMINER_PROXY_TOKEN) {
    throw new Error(
      `DATAMINER_PROXY_TOKEN must be set in ${profile.environment} so /v1/* cannot be reached by bypassing the edge Worker`,
    );
  }

  if (parsed.STAGING_RETENTION_DAYS !== undefined && profile.environment !== "staging") {
    throw new Error("STAGING_RETENTION_DAYS is a staging-only name; use RETENTION_DAYS");
  }
  const retentionDays =
    parsed.RETENTION_DAYS ??
    parsed.STAGING_RETENTION_DAYS ??
    (profile.environment === "staging" ? 30 : undefined);
  if (retentionDays === undefined) {
    throw new Error(
      `RETENTION_DAYS is required in ${profile.environment}: retention is a consent term, not a default`,
    );
  }
  if (parsed.MULTIPART_COMPLETION_LEASE_SECONDS < parsed.R2_REQUEST_TIMEOUT_SECONDS * 2 + 60) {
    throw new Error(
      "MULTIPART_COMPLETION_LEASE_SECONDS must cover two R2 requests plus a 60 second safety margin",
    );
  }
  if (
    parsed.ORPHAN_MULTIPART_GRACE_SECONDS <
    parsed.R2_PRESIGN_TTL_SECONDS + parsed.R2_REQUEST_TIMEOUT_SECONDS + 60
  ) {
    throw new Error(
      "ORPHAN_MULTIPART_GRACE_SECONDS must cover the presign TTL, R2 request timeout, and a 60 second safety margin",
    );
  }
  if (parsed.PURGE_LEASE_SECONDS < parsed.R2_REQUEST_TIMEOUT_SECONDS + 60) {
    throw new Error(
      "PURGE_LEASE_SECONDS must cover one R2 request plus a 60 second renewal margin",
    );
  }
  // Enabled-without-a-bucket fails at startup, not at first drain: the drainer
  // would only surface it as per-batch retry noise (docs/23 §6).
  if (parsed.LOGS_EXPORT_ENABLED === "true" && !parsed.LOGS_GCS_BUCKET) {
    throw new Error("LOGS_GCS_BUCKET is required when LOGS_EXPORT_ENABLED=true");
  }
  return {
    environment: profile.environment,
    profile,
    admittedFamilies,
    databaseUrl: parsed.DATABASE_URL,
    proxyToken: parsed.DATAMINER_PROXY_TOKEN ?? null,
    host: parsed.HOST,
    port: parsed.PORT,
    r2: {
      accountId: parsed.R2_ACCOUNT_ID,
      bucket: parsed.R2_BUCKET,
      accessKeyId: parsed.R2_ACCESS_KEY_ID,
      secretAccessKey: parsed.R2_SECRET_ACCESS_KEY,
      prefix,
      presignTtlSeconds: parsed.R2_PRESIGN_TTL_SECONDS,
      partSizeBytes: parsed.R2_PART_SIZE_BYTES,
      requestTimeoutSeconds: parsed.R2_REQUEST_TIMEOUT_SECONDS,
    },
    worker: {
      batchSize: parsed.VERIFY_BATCH_SIZE,
      leaseSeconds: parsed.VERIFY_LEASE_SECONDS,
      purgeBatchSize: parsed.PURGE_BATCH_SIZE,
      purgeLeaseSeconds: parsed.PURGE_LEASE_SECONDS,
      multipartCompletionLeaseSeconds: parsed.MULTIPART_COMPLETION_LEASE_SECONDS,
      orphanMultipartGraceSeconds: parsed.ORPHAN_MULTIPART_GRACE_SECONDS,
    },
    retentionDays,
    logsExport: {
      enabled: parsed.LOGS_EXPORT_ENABLED === "true",
      bucket: parsed.LOGS_GCS_BUCKET ?? null,
      batchSize: parsed.LOGS_EXPORT_BATCH_SIZE,
      dryRun: parsed.LOGS_EXPORT_DRY_RUN === "true",
    },
    unconsentedTelemetryTtlDays: parsed.UNCONSENTED_TELEMETRY_TTL_DAYS,
  };
}
